use crate::storage::display_path;
use oxigraph::store::Store;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex, OnceLock, RwLock, RwLockReadGuard,
    },
};

static GRAPH_STORES: OnceLock<Mutex<BTreeMap<PathBuf, Arc<Store>>>> = OnceLock::new();

/// Quad counts of the cached graph stores, keyed by the store's address.
/// `Store::len` is a full scan (14 s on a 26-million-quad graph, before every
/// SPARQL query, uncancellable), so a cached store's count is kept until the
/// store is written (`cell_durability::mark_rdf_store_written` forgets it) or
/// leaves the cache. A store outside the cache is always counted afresh (its
/// address may be reused once it is dropped).
static QUAD_COUNTS: OnceLock<Mutex<BTreeMap<usize, usize>>> = OnceLock::new();

fn quad_counts() -> std::sync::MutexGuard<'static, BTreeMap<usize, usize>> {
    QUAD_COUNTS
        .get_or_init(|| Mutex::new(BTreeMap::new()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn store_key(store: &Store) -> usize {
    store as *const Store as usize
}

fn is_cached_graph_store(store: &Store) -> bool {
    GRAPH_STORES.get().is_some_and(|stores| {
        stores
            .lock()
            .map(|stores| {
                stores
                    .values()
                    .any(|cached| std::ptr::eq(cached.as_ref(), store))
            })
            .unwrap_or(false)
    })
}

/// How many quads `store` holds: remembered for a cached graph store until
/// its next write, counted for any other.
pub(crate) fn graph_store_quad_count(store: &Store) -> Result<usize, String> {
    let cached = is_cached_graph_store(store);
    if cached {
        if let Some(count) = quad_counts().get(&store_key(store)) {
            return Ok(*count);
        }
    }
    let epoch = crate::cell_durability::current_write_epoch();
    let count = store
        .len()
        .map_err(|error| format!("count quads: {error}"))?;
    // A write that landed while counting may or may not be in the count:
    // keep it only when nothing was written meanwhile.
    if cached && crate::cell_durability::current_write_epoch() == epoch {
        quad_counts().insert(store_key(store), count);
    }
    Ok(count)
}

/// The store was written (or left the cache): its remembered count is gone,
/// and so is its revision ([`graph_store_revision`]).
pub(crate) fn forget_quad_count(store: &Store) {
    if let Some(counts) = QUAD_COUNTS.get() {
        counts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&store_key(store));
    }
    forget_store_generation(store_key(store));
}

/// Store revisions for cursor paging (`sparql_cursor`). A cached graph
/// store's generation is drawn from a process-wide counter the first time it
/// is asked for and forgotten at every write to that store
/// (`cell_durability::mark_rdf_store_written`, and graph-scoped marks such as
/// a persistence lease's), so the next ask draws a new one. Broad Garden
/// persistence transactions (`mark_registered_stores_written`) move every
/// store's revision at once. In memory only: a restarted cell starts over,
/// and its cursors were signed under another key anyway.
static STORE_GENERATIONS: OnceLock<Mutex<BTreeMap<usize, u64>>> = OnceLock::new();
static NEXT_STORE_GENERATION: AtomicU64 = AtomicU64::new(1);
static BROAD_WRITE_GENERATION: AtomicU64 = AtomicU64::new(0);

fn store_generations() -> std::sync::MutexGuard<'static, BTreeMap<usize, u64>> {
    STORE_GENERATIONS
        .get_or_init(|| Mutex::new(BTreeMap::new()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn forget_store_generation(key: usize) {
    if STORE_GENERATIONS.get().is_some() {
        store_generations().remove(&key);
    }
}

/// The revision of a cached graph store: it changes at every write to the
/// store. `None` for a store outside the cache (no cursor is minted for it).
pub(crate) fn graph_store_revision(store: &Store) -> Option<String> {
    if !is_cached_graph_store(store) {
        return None;
    }
    let broad = BROAD_WRITE_GENERATION.load(Ordering::SeqCst);
    let generation = *store_generations()
        .entry(store_key(store))
        .or_insert_with(|| NEXT_STORE_GENERATION.fetch_add(1, Ordering::SeqCst));
    Some(format!("{broad}.{generation}"))
}

/// Every graph store's revision moves (a broad persistence transaction).
pub(crate) fn note_all_stores_written() {
    BROAD_WRITE_GENERATION.fetch_add(1, Ordering::SeqCst);
}

/// The cached stores of `graph_id` were written (a graph-scoped mark).
pub(crate) fn note_graph_stores_written(graph_id: &str) {
    let Some(stores) = GRAPH_STORES.get() else {
        return;
    };
    let keys: Vec<usize> = match stores.lock() {
        Ok(stores) => stores
            .iter()
            .filter(|(path, _)| {
                path.parent()
                    .and_then(Path::file_name)
                    .is_some_and(|name| name == graph_id)
            })
            .map(|(_, store)| store_key(store))
            .collect(),
        // Unknown which store was written: move them all.
        Err(_) => {
            note_all_stores_written();
            return;
        }
    };
    for key in keys {
        forget_store_generation(key);
    }
}

/// Process-wide lifecycle barrier for every Oxigraph store path.
///
/// Store checkpoints are safe while an already-open store is being written,
/// but the durable flusher must not take its registry snapshot while another
/// thread is opening, evicting, or replacing a path. Without this barrier, a
/// first open after enumeration was mistaken for a closed store and its live
/// RocksDB directory was plain-copied. A same-ID publication could likewise
/// replace a path after the flusher captured the old incarnation's `Arc`,
/// producing a snapshot with the replacement manifest and the old RDF store.
///
/// Physical opens/cache misses and evictions take a shared guard. A cache hit
/// may return without the guard: it cannot add a path incarnation that was not
/// already present in the flusher's registry snapshot, and evictions remain
/// fenced. The durable flusher takes the exclusive guard from registry
/// enumeration through checkpoints and the plain-file walk. The enumeration
/// helpers themselves deliberately do not acquire this lock, because the
/// flusher already owns it exclusively.
pub(crate) fn rdf_store_lifecycle_gate() -> &'static RwLock<()> {
    static GATE: OnceLock<RwLock<()>> = OnceLock::new();
    GATE.get_or_init(|| RwLock::new(()))
}

pub(crate) fn rdf_store_lifecycle_read_guard() -> Result<RwLockReadGuard<'static, ()>, String> {
    rdf_store_lifecycle_gate()
        .read()
        .map_err(|_| "Oxigraph store lifecycle gate poisoned".to_string())
}

pub(super) fn open_graph_store(graph_dir: &Path) -> Result<Arc<Store>, String> {
    let store_path = graph_dir.join("store.oxigraph");

    // Existing handles are already in any registry snapshot a concurrent
    // durable flush could have taken. Keep this hot path independent of the
    // lifecycle gate so normal RDF requests do not wait behind an EFS walk.
    if let Some(stores) = GRAPH_STORES.get() {
        let stores = stores
            .lock()
            .map_err(|_| "Oxigraph store cache lock poisoned".to_string())?;
        if store_path.is_dir() {
            if let Some(store) = stores.get(&store_path) {
                return Ok(Arc::clone(store));
            }
        }
    }

    // A cache miss can introduce a new physical path incarnation, so double
    // check only after joining the lifecycle barrier.
    let _lifecycle_guard = rdf_store_lifecycle_read_guard()?;
    let mut stores = GRAPH_STORES
        .get_or_init(|| Mutex::new(BTreeMap::new()))
        .lock()
        .map_err(|_| "Oxigraph store cache lock poisoned".to_string())?;

    if store_path.is_dir() {
        if let Some(store) = stores.get(&store_path) {
            return Ok(Arc::clone(store));
        }
    } else {
        // Failed graph-creation cleanup can remove a graph directory while
        // this process still has its old path-keyed handle cached. Never hand
        // that detached RocksDB generation to a later same-id create.
        if let Some(store) = stores.remove(&store_path) {
            forget_quad_count(&store);
        }
    }

    let store =
        Arc::new(Store::open(&store_path).map_err(|error| {
            format!("open Oxigraph store {}: {error}", display_path(&store_path))
        })?);
    stores.insert(store_path, Arc::clone(&store));
    Ok(store)
}

/// Drop the process cache's ownership of one graph store at its lifecycle
/// boundary. Active callers may retain their own `Arc`, but a future open will
/// never resolve through the deleted graph incarnation's cached handle.
pub(crate) fn evict_graph_store(graph_dir: &Path) -> Result<(), String> {
    let _lifecycle_guard = rdf_store_lifecycle_read_guard()?;
    let Some(stores) = GRAPH_STORES.get() else {
        return Ok(());
    };
    let evicted = stores
        .lock()
        .map_err(|_| "Oxigraph store cache lock poisoned".to_string())?
        .remove(&graph_dir.join("store.oxigraph"));
    if let Some(store) = evicted {
        forget_quad_count(&store);
    }
    Ok(())
}

/// Snapshot the currently-open per-graph stores as `(store_path, store)` pairs.
/// Used by the durable-plane flusher to back up live RocksDB dirs via a
/// consistent checkpoint rather than copying their files underneath writers.
#[cfg_attr(feature = "desktop", allow(dead_code))]
pub(crate) fn open_graph_stores() -> Vec<(PathBuf, Arc<Store>)> {
    let Some(stores) = GRAPH_STORES.get() else {
        return Vec::new();
    };
    match stores.lock() {
        Ok(stores) => stores
            .iter()
            .map(|(path, store)| (path.clone(), Arc::clone(store)))
            .collect(),
        Err(_) => Vec::new(),
    }
}

// Named so the cells lane's gating filter (`cell_durability`) runs it.
#[cfg(test)]
mod cell_durability_quad_count_tests {
    use super::*;
    use oxigraph::model::{GraphNameRef, NamedNodeRef, QuadRef};

    /// A cached graph store's count is remembered until a marked write, and
    /// is right again after it; a store outside the cache is always counted.
    #[test]
    fn a_cached_store_count_is_kept_until_its_next_write() {
        let graph_dir = std::env::temp_dir().join(format!("quad-count-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&graph_dir).unwrap();
        let store = open_graph_store(&graph_dir).unwrap();
        let s = NamedNodeRef::new("urn:s").unwrap();
        let g = GraphNameRef::NamedNode(NamedNodeRef::new("urn:g").unwrap());
        assert_eq!(graph_store_quad_count(&store).unwrap(), 0);
        store.insert(QuadRef::new(s, s, s, g)).unwrap();
        crate::cell_durability::mark_rdf_store_written(&store);
        assert_eq!(
            graph_store_quad_count(&store).unwrap(),
            1,
            "the write was marked"
        );
        // A write by any other test bumps the process-wide epoch, and a count
        // that overlapped it is (rightly) not remembered: count again until one
        // ran clear (the cells check failed on this race once, 2026-10-15).
        let mut remembered = quad_counts().get(&store_key(&store)).copied();
        for _ in 0..200 {
            if remembered.is_some() {
                break;
            }
            assert_eq!(graph_store_quad_count(&store).unwrap(), 1);
            remembered = quad_counts().get(&store_key(&store)).copied();
        }
        assert_eq!(remembered, Some(1), "remembered");
        let o = NamedNodeRef::new("urn:o").unwrap();
        store.insert(QuadRef::new(s, s, o, g)).unwrap();
        crate::cell_durability::mark_rdf_store_written(&store);
        assert_eq!(graph_store_quad_count(&store).unwrap(), 2);

        let loose = Store::new().unwrap();
        loose.insert(QuadRef::new(s, s, s, g)).unwrap();
        assert_eq!(graph_store_quad_count(&loose).unwrap(), 1);
        assert!(
            quad_counts().get(&store_key(&loose)).is_none(),
            "never remembered"
        );

        drop(store);
        evict_graph_store(&graph_dir).unwrap();
        let _ = std::fs::remove_dir_all(&graph_dir);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxigraph::model::{GraphNameRef, NamedNodeRef, QuadRef};
    use uuid::Uuid;

    fn insert_named_quad(store: &Store, subject: &str, object: &str) {
        store
            .insert(QuadRef::new(
                NamedNodeRef::new(subject).expect("subject IRI"),
                NamedNodeRef::new("http://example.com/p").expect("predicate IRI"),
                NamedNodeRef::new(object).expect("object IRI"),
                GraphNameRef::DefaultGraph,
            ))
            .expect("insert test quad");
    }

    #[test]
    fn missing_store_directory_never_reuses_a_stale_cached_handle() {
        let graph_dir =
            std::env::temp_dir().join(format!("garden-rdf-store-incarnation-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&graph_dir).expect("create first graph incarnation");
        let first = open_graph_store(&graph_dir).expect("open first store incarnation");
        let first_weak = Arc::downgrade(&first);
        drop(first);

        std::fs::remove_dir_all(&graph_dir).expect("remove first store incarnation");
        std::fs::create_dir_all(&graph_dir).expect("create replacement graph incarnation");
        let second = open_graph_store(&graph_dir).expect("open replacement store incarnation");
        assert!(
            first_weak.upgrade().is_none(),
            "replacement open retained the detached cached store"
        );

        evict_graph_store(&graph_dir).expect("evict replacement store");
        drop(second);
        let _ = std::fs::remove_dir_all(&graph_dir);
    }

    #[test]
    fn explicit_eviction_opens_a_prepopulated_replacement_store_at_the_same_path() {
        let graph_dir = std::env::temp_dir().join(format!(
            "garden-rdf-store-populated-replacement-{}",
            Uuid::new_v4()
        ));
        std::fs::create_dir_all(&graph_dir).expect("create first graph incarnation");
        let first = open_graph_store(&graph_dir).expect("open first store incarnation");
        insert_named_quad(
            &first,
            "http://example.com/old",
            "http://example.com/old-value",
        );
        assert_eq!(first.len().expect("first store length"), 1);
        let first_weak = Arc::downgrade(&first);

        // This is the lifecycle ordering used by failed duplicate cleanup:
        // evict the process cache before deleting the graph directory.
        evict_graph_store(&graph_dir).expect("evict first store incarnation");
        drop(first);
        assert!(
            first_weak.upgrade().is_none(),
            "explicit eviction retained the detached store"
        );
        std::fs::remove_dir_all(&graph_dir).expect("remove first graph incarnation");

        // Model a replacement producer that populates its RocksDB directory
        // before the shared cache sees the path again.
        std::fs::create_dir_all(&graph_dir).expect("create replacement graph incarnation");
        let replacement = Store::open(graph_dir.join("store.oxigraph"))
            .expect("create populated replacement store");
        insert_named_quad(
            &replacement,
            "http://example.com/new-one",
            "http://example.com/new-value-one",
        );
        insert_named_quad(
            &replacement,
            "http://example.com/new-two",
            "http://example.com/new-value-two",
        );
        drop(replacement);

        let reopened = open_graph_store(&graph_dir).expect("open replacement through cache");
        assert_eq!(
            reopened.len().expect("replacement store length"),
            2,
            "same-path reopen returned the stale one-quad store"
        );

        evict_graph_store(&graph_dir).expect("evict replacement store");
        drop(reopened);
        let _ = std::fs::remove_dir_all(&graph_dir);
    }
}
