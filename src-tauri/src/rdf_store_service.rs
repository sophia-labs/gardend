use crate::storage::display_path;
use oxigraph::store::Store;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock, RwLock, RwLockReadGuard},
};

static GRAPH_STORES: OnceLock<Mutex<BTreeMap<PathBuf, Arc<Store>>>> = OnceLock::new();

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
        stores.remove(&store_path);
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
    stores
        .lock()
        .map_err(|_| "Oxigraph store cache lock poisoned".to_string())?
        .remove(&graph_dir.join("store.oxigraph"));
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
