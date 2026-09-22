use crate::{
    document_service::{read_document_record_cold, read_workspace_record},
    graph_service::GraphRecord,
    paths::documents_dir,
    rdf::{graph_subject, sparql_string_literal},
    rdf_authority::seed_marker_graph_iri,
    rdf_record_materializer::materialize_graph_record,
    rdf_store_service::open_graph_store,
    rdf_workspace_store_materializer::reconcile_workspace_snapshot,
    runtime_config::MNEMO_NS,
    storage::read_json,
};
use oxigraph::sparql::{CancellationToken, QueryResults, SparqlEvaluator};
use oxigraph::store::Store;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock},
};

static GRAPH_STORE_SEEDED_AT: OnceLock<Mutex<BTreeMap<PathBuf, String>>> = OnceLock::new();
static SOURCE_REPLAY_STORES: OnceLock<Mutex<BTreeSet<PathBuf>>> = OnceLock::new();

fn seed_projection_version() -> String {
    format!("document-faces-v1+{}", crate::pdf_source::PROJECTION_VERSION)
}

/// A Send ownership token, not a mutex guard: async source replay may hold it
/// across awaits. Ordinary seed callers refuse while its store is replaying.
/// Checkpoint replacement (or invalidate for a checkpoint-free replay) leaves
/// no durable completion marker; only finish may publish a completed seed.
pub(crate) struct SeedReplayGuard {
    graph_dir: PathBuf,
    store_path: PathBuf,
}

pub(crate) fn begin_seed_replay(graph_dir: &Path) -> Result<SeedReplayGuard, String> {
    let store_path = graph_dir.join("store.oxigraph");
    let walk_lock = seed_walk_lock(&store_path)?;
    let _walk_guard = walk_lock.lock().map_err(|_| "Oxigraph seed walk lock poisoned".to_string())?;
    let mut active = SOURCE_REPLAY_STORES.get_or_init(|| Mutex::new(BTreeSet::new()))
        .lock().map_err(|_| "Oxigraph source replay map poisoned".to_string())?;
    if !active.insert(store_path.clone()) {
        return Err("Oxigraph source projection replay already in progress".into());
    }
    Ok(SeedReplayGuard { graph_dir: graph_dir.to_path_buf(), store_path })
}

impl SeedReplayGuard {
    pub(crate) fn invalidate(&self) -> Result<(), String> {
        with_invalidated_seed_cache(&self.graph_dir, || {
            let graph = read_json::<GraphRecord>(&self.graph_dir.join("graph.json"))?;
            let store = open_graph_store(&self.graph_dir)?;
            let marker_graph = oxigraph::model::NamedNode::new(seed_marker_graph_iri(&graph.graph_id))
                .map_err(|error| error.to_string())?;
            store.clear_graph(marker_graph.as_ref()).map_err(|error| error.to_string())?;
            crate::cell_durability::mark_rdf_store_written(&store);
            Ok(())
        })
    }

    pub(crate) fn finish(self) -> Result<(), String> {
        ensure_graph_store_seeded_inner(&self.graph_dir, None, Some(&self))
    }
}

impl Drop for SeedReplayGuard {
    fn drop(&mut self) {
        // No fallible I/O in Drop. The working store marker was invalidated
        // before projection effects, so an aborted replay stays cold on restart.
        if let Ok(mut active) = SOURCE_REPLAY_STORES.get_or_init(|| Mutex::new(BTreeSet::new())).lock() {
            active.remove(&self.store_path);
        }
    }
}

fn require_seed_replay_owner(store_path: &Path, owner: Option<&SeedReplayGuard>) -> Result<(), String> {
    let active = SOURCE_REPLAY_STORES.get_or_init(|| Mutex::new(BTreeSet::new()))
        .lock().map_err(|_| "Oxigraph source replay map poisoned".to_string())?;
    if active.contains(store_path) && owner.is_none_or(|owner| owner.store_path != store_path) {
        return Err("Oxigraph source projection replay in progress; retry after completion".into());
    }
    Ok(())
}

/// Per-store single-flight gate for the materialization walk. Without it, N
/// concurrent RDF calls arriving on a process whose in-memory marker is cold
/// (fresh pod; or any key change on a busy cell) all run the FULL document
/// walk simultaneously — N × the walk's transient memory and N wholesale
/// store rewrites for one logical reseed. Competing callers now block here,
/// re-check the markers, and find the winner's completed seed.
static SEED_WALK_LOCKS: OnceLock<Mutex<BTreeMap<PathBuf, Arc<Mutex<()>>>>> = OnceLock::new();

fn seed_walk_lock(store_path: &Path) -> Result<Arc<Mutex<()>>, String> {
    Ok(SEED_WALK_LOCKS
        .get_or_init(|| Mutex::new(BTreeMap::new()))
        .lock()
        .map_err(|_| "Oxigraph seed walk lock map poisoned".to_string())?
        .entry(store_path.to_path_buf())
        .or_default()
        .clone())
}

/// Per-store count of re-materialization passes actually performed (i.e. the
/// times the early-return did NOT fire). The test observable for the "RDF-only
/// writes stop storming" invariant: a pure memory / emporium write must NOT bump
/// this, a document save MUST. Per-store (keyed by `store_path`) so parallel tests
/// over distinct temp graphs never contend. Test-only surface.
#[cfg(test)]
static SEED_RESEED_COUNTS: OnceLock<Mutex<BTreeMap<PathBuf, u64>>> = OnceLock::new();

#[cfg(test)]
fn record_reseed(store_path: &Path) {
    if let Ok(mut counts) = SEED_RESEED_COUNTS
        .get_or_init(|| Mutex::new(BTreeMap::new()))
        .lock()
    {
        *counts.entry(store_path.to_path_buf()).or_insert(0) += 1;
    }
}

/// Read how many re-materialization passes have run for `store_path`.
#[cfg(test)]
pub(super) fn reseed_count(store_path: &Path) -> u64 {
    SEED_RESEED_COUNTS
        .get()
        .and_then(|counts| {
            counts
                .lock()
                .ok()
                .map(|counts| *counts.get(store_path).unwrap_or(&0))
        })
        .unwrap_or(0)
}

/// Per-store count of DOCUMENTS actually re-materialized across all seed
/// passes — the observable for the incremental walk: a pass provoked by a
/// workspace-only `content_revision` bump must re-materialize ZERO documents,
/// a pass after one document save exactly ONE. Test-only surface.
#[cfg(test)]
static SEED_DOC_MATERIALIZATION_COUNTS: OnceLock<Mutex<BTreeMap<PathBuf, u64>>> = OnceLock::new();

#[cfg(test)]
fn record_document_materializations(store_path: &Path, count: u64) {
    if let Ok(mut counts) = SEED_DOC_MATERIALIZATION_COUNTS
        .get_or_init(|| Mutex::new(BTreeMap::new()))
        .lock()
    {
        *counts.entry(store_path.to_path_buf()).or_insert(0) += count;
    }
}

/// Read how many documents the seed walks for `store_path` have re-materialized.
#[cfg(test)]
pub(super) fn seed_document_materialization_count(store_path: &Path) -> u64 {
    SEED_DOC_MATERIALIZATION_COUNTS
        .get()
        .and_then(|counts| {
            counts
                .lock()
                .ok()
                .map(|counts| *counts.get(store_path).unwrap_or(&0))
        })
        .unwrap_or(0)
}

/// Run a validated synchronous store replacement without retaining a seed
/// cache entry for the replaced dataset. The walk gate prevents a competing
/// cold seed from rearming the cache between invalidation and replacement.
/// The closure must not call the seed reconciler or cross an async boundary.
/// Its caller must finish source replay and reconcile seeding before reporting
/// success. Once invalidation occurs, replacement errors leave it absent.
pub(crate) fn with_invalidated_seed_cache<T>(
    graph_dir: &Path,
    replace: impl FnOnce() -> Result<T, String>,
) -> Result<T, String> {
    let store_path = graph_dir.join("store.oxigraph");
    let walk_lock = seed_walk_lock(&store_path)?;
    let _walk_guard = walk_lock
        .lock()
        .map_err(|_| "Oxigraph seed walk lock poisoned during replacement".to_string())?;
    GRAPH_STORE_SEEDED_AT
        .get_or_init(|| Mutex::new(BTreeMap::new()))
        .lock()
        .map_err(|_| "Oxigraph seed marker lock poisoned".to_string())?
        .remove(&store_path);
    replace()
}

/// Test-only restart simulation: forget the PROCESS-local marker for one
/// store, exactly as a pod restart does, while the durable in-store marker
/// (and the store itself) survive. Lets a test prove the hydrated-restore
/// fast path without constructing a second process.
#[cfg(test)]
pub(super) fn forget_process_seed_marker_for_test(store_path: &Path) {
    if let Some(markers) = GRAPH_STORE_SEEDED_AT.get() {
        if let Ok(mut markers) = markers.lock() {
            markers.remove(store_path);
        }
    }
}

/// The Oxigraph seed marker key. Keyed on the DOCUMENT-side `content_revision` (bumped
/// only by writes that change a file-backed record the seed materializes — document
/// save/create/delete, workspace-snapshot save, graph-metadata edit), falling back to
/// the immutable `created_at` when a graph has never had such a write. Deliberately NOT
/// `graph.updated_at`: an RDF-only write (memory / salience / song / semantic index /
/// user `sparql_update` / emporium projection) still bumps `updated_at`, so keying on it
/// made the NEXT rdf call wholesale re-materialize EVERY document — the reseed storm
/// (appraisal §4.4 / P1-6), billed to an unrelated request. Keying on `content_revision`
/// makes a pure RDF write a no-op for the seed while a document save still reseeds.
fn seed_key_for(graph: &GraphRecord) -> String {
    let revision = graph
        .content_revision
        .clone()
        .unwrap_or_else(|| graph.created_at.clone());
    let identity_key = match graph.incarnation_id.as_deref() {
        Some(incarnation_id) => format!("{incarnation_id}:{revision}"),
        None => revision,
    };
    format!("{}|{identity_key}", seed_projection_version())
}

pub(super) fn ensure_graph_store_seeded(graph_dir: &Path) -> Result<(), String> {
    ensure_graph_store_seeded_inner(graph_dir, None, None)
}

/// Seed/reconcile with cooperative request cancellation between projection
/// units. A single document materializer remains atomic, but a disconnected
/// or expired external query no longer launches or continues the whole graph
/// walk. Partial work deliberately leaves the durable marker stale so the next
/// admitted operation repairs it.
pub(super) fn ensure_graph_store_seeded_with_cancellation(
    graph_dir: &Path,
    cancellation: &CancellationToken,
) -> Result<(), String> {
    ensure_graph_store_seeded_inner(graph_dir, Some(cancellation), None)
}

fn ensure_graph_store_seeded_inner(
    graph_dir: &Path,
    cancellation: Option<&CancellationToken>,
    replay_owner: Option<&SeedReplayGuard>,
) -> Result<(), String> {
    require_seed_not_cancelled(cancellation, "before seed inspection")?;
    let store_path = graph_dir.join("store.oxigraph");
    require_seed_replay_owner(&store_path, replay_owner)?;
    let graph = read_json::<GraphRecord>(&graph_dir.join("graph.json"))?;
    let seed_key = seed_key_for(&graph);

    // Fast path 1 — process-local marker (no store open, no lock contention).
    if process_seed_marker_matches(&store_path, &seed_key)? {
        return Ok(());
    }

    // Single-flight: exactly one walk per store at a time. Losers of the race
    // block here, then find the winner's markers on the re-check below.
    let walk_lock = seed_walk_lock(&store_path)?;
    let _walk_guard = walk_lock
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    require_seed_not_cancelled(cancellation, "waiting for the seed single-flight lock")?;
    require_seed_replay_owner(&store_path, replay_owner)?;
    if process_seed_marker_matches(&store_path, &seed_key)? {
        return Ok(());
    }

    let store = open_graph_store(graph_dir)?;

    // Fast path 2 — the durable marker rides INSIDE the Oxigraph store (a
    // quad in the reserved `…:projection:seed` graph), so it travels
    // atomically with the store through `Store::backup` durable snapshots.
    // A hydrated-restored cell whose in-store key matches is ALREADY seeded:
    // the previous process-local-only marker was reset by every restart,
    // which made the first SPARQL query on a fresh pod wholesale
    // re-materialize (and, before the cold-read fix, hydrate the full Y.Doc
    // history of) every document — the fresh-cell OOM.
    let previous_key = read_durable_seed_marker(&store, &graph.graph_id)?;
    if previous_key.as_deref() == Some(seed_key.as_str()) {
        log::info!(
            "rdf-seed[{}]: durable in-store marker matches ({seed_key}); restored store is \
             already seeded — no walk",
            graph.graph_id
        );
        remember_process_seed_marker(store_path, seed_key)?;
        return Ok(());
    }

    #[cfg(test)]
    record_reseed(&store_path);

    // WALK TELEMETRY — deliberately chatty at INFO. A walk is rare (marker
    // mismatch only) and potentially expensive, and it used to run in TOTAL
    // silence: the 2026-07-22 canary OOM (494Mi -> 14Gi in <30s on the first
    // post-hydration SPARQL query) emitted zero log lines between boot-ready
    // and OOMKill, costing a full diagnosis cycle. The store quad count is
    // the single most diagnostic number here: a store expected to hold ~1e5
    // quads that reports 1e7 explains a detonation by itself (the per-doc
    // teardown's transient cost scales with the triples ALREADY IN the
    // document's projection graph, not with the manifest on disk).
    let walk_started = std::time::Instant::now();
    let store_quads = store.len().unwrap_or(0);
    log::info!(
        "rdf-seed[{}]: walk starting — previous_marker={previous_key:?} seed_key={seed_key} \
         store_quads={store_quads}",
        graph.graph_id
    );

    materialize_graph_record(&store, &graph)?;
    require_seed_not_cancelled(cancellation, "materializing graph metadata")?;
    if let Some(snapshot) = read_workspace_record(graph_dir, &graph.graph_id)?.snapshot {
        reconcile_workspace_snapshot(&store, &graph.graph_id, &snapshot)?;
        require_seed_not_cancelled(cancellation, "materializing workspace metadata")?;
    }
    log::info!(
        "rdf-seed[{}]: graph + workspace projections reconciled ({:?} elapsed)",
        graph.graph_id,
        walk_started.elapsed()
    );

    // INCREMENTAL WALK. Every document-content writer (`save_document*` /
    // `ensure_document_persistence_tail`, the title sweep's record save, the
    // restore service's metadata rewrite) already reconciles ITS OWN document
    // RDF and re-stamps `updated_at` before bumping `content_revision`, so on
    // a marker mismatch only documents stamped AT/AFTER the previous marker's
    // revision can be out of date. A workspace-only `content_revision` bump
    // (`persist_materialized_workspace` touches it on every workspace flush)
    // therefore re-materializes ZERO documents instead of all of them — on an
    // active fat graph the old wholesale walk re-ran after every workspace
    // flush, billed to the next unlucky SPARQL query. Anything unparseable
    // (no previous marker, foreign/incarnation-mismatched key, non-epoch
    // stamp) falls back to materializing — the safe direction.
    let reseed_floor = incremental_reseed_floor(&graph, previous_key.as_deref());
    let documents_dir = documents_dir(graph_dir);
    let mut document_manifests = Vec::new();
    if documents_dir.is_dir() {
        for entry in
            fs::read_dir(&documents_dir).map_err(|error| format!("read documents dir: {error}"))?
        {
            require_seed_not_cancelled(cancellation, "enumerating document projections")?;
            let entry = entry.map_err(|error| format!("read document entry: {error}"))?;
            let document_path = entry.path().join("document.json");
            if document_path.is_file() {
                document_manifests.push(document_path);
            }
        }
    }
    let document_total = document_manifests.len();
    log::info!(
        "rdf-seed[{}]: document walk starting — documents={document_total} floor={reseed_floor:?}",
        graph.graph_id
    );

    let mut processed: usize = 0;
    let mut materialized_documents: u64 = 0;
    for document_path in document_manifests {
        require_seed_not_cancelled(cancellation, "walking document projections")?;
        processed += 1;
        let skip = reseed_floor
            .map(|floor_ms| manifest_is_strictly_older(&document_path, floor_ms))
            .unwrap_or(false);
        if !skip {
            // Cold read: the materializer consumes only the projected content
            // fields (title/tree/blocks); hydrating the full Y.Doc history
            // here cost O(history) memory per document for bytes the walk
            // never used.
            let document = read_document_record_cold(graph_dir, &document_path)?;
            // Match saves and source/Y.Doc replay across all four declared
            // Document faces, including the ten bare metadata predicates.
            crate::document_meaningful_object::reconcile_document_record(&store, &document)?;
            crate::pdf_source::reconcile(&store, &document)?;
            require_seed_not_cancelled(cancellation, "materializing a document projection")?;
            materialized_documents += 1;
        }
        if processed % 25 == 0 {
            log::info!(
                "rdf-seed[{}]: progress {processed}/{document_total} \
                 (materialized={materialized_documents}, {:?} elapsed)",
                graph.graph_id,
                walk_started.elapsed()
            );
        }
    }
    #[cfg(test)]
    record_document_materializations(&store_path, materialized_documents);

    require_seed_not_cancelled(cancellation, "committing the durable seed marker")?;
    write_durable_seed_marker(&store, &graph.graph_id, &seed_key)?;
    log::info!(
        "rdf-seed[{}]: walk complete — {materialized_documents}/{document_total} documents \
         re-materialized in {:?}; marker={seed_key}",
        graph.graph_id,
        walk_started.elapsed()
    );

    // DURABILITY AUDIT FINDING (2026-07-18): this function re-materializes
    // graph/workspace/document RDF projections directly against `store`, and
    // is reachable from a PURE READ (`run_sparql_query_service` calls it
    // before every legacy/internal SPARQL query). Controlled external queries
    // now hold `GraphPersistenceLease`, but older direct/internal callers do
    // not uniformly do so. Mark unconditionally: this branch only runs when
    // the seed marker proved stale, so a real re-materialization (at minimum
    // the graph record, the workspace reconcile, and the durable marker write
    // above) always just happened.
    crate::cell_durability::mark_rdf_store_written(&store);

    remember_process_seed_marker(store_path, seed_key)?;

    Ok(())
}

fn require_seed_not_cancelled(
    cancellation: Option<&CancellationToken>,
    phase: &str,
) -> Result<(), String> {
    if cancellation.is_some_and(CancellationToken::is_cancelled) {
        Err(format!("SPARQL query cancelled while {phase}"))
    } else {
        Ok(())
    }
}

fn process_seed_marker_matches(store_path: &Path, seed_key: &str) -> Result<bool, String> {
    let seeded = GRAPH_STORE_SEEDED_AT
        .get_or_init(|| Mutex::new(BTreeMap::new()))
        .lock()
        .map_err(|_| "Oxigraph seed marker lock poisoned".to_string())?;
    Ok(seeded.get(store_path).map(String::as_str) == Some(seed_key))
}

fn remember_process_seed_marker(store_path: PathBuf, seed_key: String) -> Result<(), String> {
    GRAPH_STORE_SEEDED_AT
        .get_or_init(|| Mutex::new(BTreeMap::new()))
        .lock()
        .map_err(|_| "Oxigraph seed marker lock poisoned".to_string())?
        .insert(store_path, seed_key);
    Ok(())
}

/// Read the durable seed marker out of the store's reserved
/// `…:projection:seed` graph. `None` = never seeded (fresh/replaced store) or
/// a pre-marker store from before this mechanism — both take the full walk.
fn read_durable_seed_marker(store: &Store, graph_id: &str) -> Result<Option<String>, String> {
    let marker_graph = seed_marker_graph_iri(graph_id);
    let subject = graph_subject(graph_id);
    let query = format!(
        r#"SELECT ?key WHERE {{ GRAPH <{marker_graph}> {{ <{subject}> <{MNEMO_NS}seedKey> ?key }} }}"#
    );
    let solutions = match SparqlEvaluator::new()
        .parse_query(&query)
        .map_err(|error| format!("parse seed marker query: {error}"))?
        .on_store(store)
        .execute()
        .map_err(|error| format!("read seed marker: {error}"))?
    {
        QueryResults::Solutions(solutions) => solutions,
        _ => return Err("seed marker query expected SELECT solutions".to_string()),
    };
    for solution in solutions {
        let solution = solution.map_err(|error| format!("seed marker row: {error}"))?;
        if let Some(oxigraph::model::Term::Literal(literal)) = solution.get("key") {
            return Ok(Some(literal.value().to_string()));
        }
    }
    Ok(None)
}

/// Replace the durable seed marker with `seed_key`. Direct-on-store into the
/// reserved `…:projection:seed` graph (the same bypass-the-authority-gate
/// posture as every projection materializer); the caller marks the store
/// written for the durable flush.
fn write_durable_seed_marker(store: &Store, graph_id: &str, seed_key: &str) -> Result<(), String> {
    let marker_graph = seed_marker_graph_iri(graph_id);
    let subject = graph_subject(graph_id);
    let update = format!(
        r#"DELETE WHERE {{ GRAPH <{marker_graph}> {{ <{subject}> <{MNEMO_NS}seedKey> ?old }} }} ;
INSERT DATA {{ GRAPH <{marker_graph}> {{ <{subject}> <{MNEMO_NS}seedKey> {key} }} }}"#,
        key = sparql_string_literal(seed_key),
    );
    SparqlEvaluator::new()
        .parse_update(&update)
        .map_err(|error| format!("parse seed marker update: {error}"))?
        .on_store(store)
        .execute()
        .map_err(|error| format!("write seed marker: {error}"))
}

/// The incremental walk's floor: the PREVIOUS completed seed's
/// `content_revision` (epoch-ms), valid only when the previous durable marker
/// belongs to the SAME graph incarnation. `None` = no usable floor → full walk.
fn incremental_reseed_floor(graph: &GraphRecord, previous_key: Option<&str>) -> Option<i64> {
    // An older projection implementation must walk every source once even if
    // its content revision is unchanged. Never bump the source just to repair.
    let prefix = format!("{}|", seed_projection_version());
    let previous_key = previous_key?.strip_prefix(&prefix)?;
    let previous_revision = match graph.incarnation_id.as_deref() {
        Some(incarnation_id) => previous_key.strip_prefix(&format!("{incarnation_id}:"))?,
        None => previous_key,
    };
    previous_revision.parse::<i64>().ok()
}

/// Cheap stamp scan: parse ONLY `updatedAt` out of the manifest (serde skips
/// unknown fields without allocating them, so the fat inline Y.Doc payload is
/// never materialized as a `String` here). Strictly-older-than-floor is the
/// only skip; an equal, newer, missing, or unparseable stamp does the work —
/// the safe direction.
fn manifest_is_strictly_older(document_path: &Path, floor_ms: i64) -> bool {
    #[derive(serde::Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct ManifestStamp {
        #[serde(default)]
        updated_at: String,
    }
    read_json::<ManifestStamp>(document_path)
        .ok()
        .and_then(|stamp| stamp.updated_at.parse::<i64>().ok())
        .map(|updated_at| updated_at < floor_ms)
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph_record_store::{touch_graph_content_revision, touch_graph_updated_at};
    use crate::storage::write_json;
    use uuid::Uuid;

    const GID: &str = "seed-key-graph";

    #[test]
    fn seed_replay_guard_refuses_mid_replay_refill() {
        let dir = std::env::temp_dir().join(format!("garden-seed-replay-gap-{}", Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        write_graph_json(&dir);
        write_minimal_document(&dir, "kept");
        ensure_graph_store_seeded(&dir).unwrap();
        let path = dir.join("store.oxigraph");
        let graph = read_json::<GraphRecord>(&dir.join("graph.json")).unwrap();
        let key = seed_key_for(&graph);
        let store = open_graph_store(&dir).unwrap();
        let replay = begin_seed_replay(&dir).unwrap();
        fn require_send<T: Send>(_: &T) {}
        require_send(&replay);
        with_invalidated_seed_cache(&dir, || write_durable_seed_marker(&store, GID, "checkpoint-old")).unwrap();
        // The transaction gate is gone; the token survives the async interval.
        let worker_dir = dir.clone();
        let result = std::thread::spawn(move || ensure_graph_store_seeded(&worker_dir)).join().unwrap();
        println!("SEED_REPLAY_GAP={{\"seedAccepted\":{},\"cacheCurrent\":{}}}", result.is_ok(), process_seed_marker_matches(&path, &key).unwrap());
        assert!(result.is_err(), "an unrelated seed caller must not certify an in-progress replay");
        assert!(!process_seed_marker_matches(&path, &key).unwrap());
        replay.finish().unwrap();
        assert!(process_seed_marker_matches(&path, &key).unwrap());
        ensure_graph_store_seeded(&dir).unwrap();
    }

    #[test]
    fn seed_replay_guard_abort_and_nested_start_remain_fail_closed() {
        let dir = std::env::temp_dir().join(format!("garden-seed-replay-abort-{}", Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        write_graph_json(&dir);
        write_minimal_document(&dir, "kept");
        ensure_graph_store_seeded(&dir).unwrap();
        let path = dir.join("store.oxigraph");
        let graph = read_json::<GraphRecord>(&dir.join("graph.json")).unwrap();
        let key = seed_key_for(&graph);
        let store = open_graph_store(&dir).unwrap();
        let replay = begin_seed_replay(&dir).unwrap();
        assert!(begin_seed_replay(&dir).is_err());
        replay.invalidate().unwrap();
        assert!(ensure_graph_store_seeded(&dir).unwrap_err().contains("replay in progress"));
        drop(replay);
        assert!(!process_seed_marker_matches(&path, &key).unwrap());
        assert_eq!(read_durable_seed_marker(&store, GID).unwrap(), None);
        let before = reseed_count(&path);
        ensure_graph_store_seeded(&dir).unwrap();
        assert_eq!(reseed_count(&path), before + 1);
        assert!(process_seed_marker_matches(&path, &key).unwrap());
        println!("SEED_REPLAY_ABORT_RETAINED_PROFILE={}", dir.display());
    }

    #[test]
    fn legacy_current_seed_migrates_all_document_faces_once() {
        let dir = std::env::temp_dir().join(format!("garden-seed-version-{}", Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        write_graph_json(&dir);
        write_minimal_document(&dir, "kept");
        let manifest = documents_dir(&dir).join("kept/document.json");
        let mut value: serde_json::Value = read_json(&manifest).unwrap();
        value["body"] = serde_json::json!("Retained legacy document text.");
        value["tiptapXml"] = serde_json::json!("<paragraph>Retained legacy document text.</paragraph>");
        value["updatedAt"] = serde_json::json!("500");
        write_json(&manifest, &value).unwrap();
        let mut graph_value: serde_json::Value = read_json(&dir.join("graph.json")).unwrap();
        graph_value["contentRevision"] = serde_json::json!("5000");
        write_json(&dir.join("graph.json"), &graph_value).unwrap();
        ensure_graph_store_seeded(&dir).unwrap();
        let path = dir.join("store.oxigraph");
        let store = open_graph_store(&dir).unwrap();
        let document = read_document_record_cold(&dir, &manifest).unwrap();
        crate::rdf_record_materializer::materialize_document_record(&store, &document).unwrap();
        let graph = read_json::<GraphRecord>(&dir.join("graph.json")).unwrap();
        let revision = graph.content_revision.clone().unwrap_or_else(|| graph.created_at.clone());
        let identity = graph.incarnation_id.as_ref().map(|id| format!("{id}:{revision}")).unwrap_or(revision);
        let legacy_key = format!("{}|{identity}", crate::pdf_source::PROJECTION_VERSION);
        write_durable_seed_marker(&store, GID, &legacy_key).unwrap();
        forget_process_seed_marker_for_test(&path);
        let authority = (fs::read(&manifest).unwrap(), fs::read(dir.join("graph.json")).unwrap());
        let walks_before = reseed_count(&path);
        let documents_before = seed_document_materialization_count(&path);
        ensure_graph_store_seeded(&dir).unwrap();
        let actual = crate::rdf_query_service::dump_rdf_from_store(&store, "nquads", None, None).unwrap().data;
        let simple = |value: &str| oxigraph::model::Literal::new_simple_literal(value).to_string();
        let integer = |value: u64| format!("\"{value}\"^^<http://www.w3.org/2001/XMLSchema#integer>");
        let expected = BTreeMap::from([
            ("body", simple(&document.body)), ("documentId", simple(&document.document_id)),
            ("graphId", simple(&document.graph_id)), ("localPath", simple(&document.local_path)),
            ("origin", simple(&document.origin)), ("providerId", simple(&document.provider_id)),
            ("rdfTripleCount", integer(crate::rdf_document_tree::document_tree_triples(&document).len() as u64)),
            ("schemaVersion", integer(document.schema_version as u64)),
            ("tiptapXml", simple(&document.tiptap_xml)), ("ydocStatePath", simple(&document.ydoc_state_path)),
        ]);
        let prefix = format!("<{}> <{MNEMO_NS}", document.rdf_subject);
        let suffix = format!(" <{}> .", crate::rdf_authority::document_projection_graph_iri(GID, "kept"));
        let actual_rows = actual.lines().filter_map(|line| line.strip_prefix(&prefix))
            .filter_map(|line| line.split_once("> "))
            .filter(|(predicate, _)| expected.contains_key(predicate))
            .map(|(predicate, object)| (predicate, object.strip_suffix(&suffix).unwrap().to_string()))
            .collect::<Vec<_>>();
        assert_eq!(actual_rows.len(), 10, "exactly one value per advertised predicate");
        assert_eq!(actual_rows.into_iter().collect::<BTreeMap<_, _>>(), expected);
        assert_eq!(reseed_count(&path) - walks_before, 1);
        assert_eq!(seed_document_materialization_count(&path) - documents_before, 1);
        assert_eq!((fs::read(&manifest).unwrap(), fs::read(dir.join("graph.json")).unwrap()), authority);
        ensure_graph_store_seeded(&dir).unwrap();
        forget_process_seed_marker_for_test(&path);
        ensure_graph_store_seeded(&dir).unwrap();
        assert_eq!(reseed_count(&path) - walks_before, 1, "no subsequent warm/cold walk");
        assert_eq!(seed_document_materialization_count(&path) - documents_before, 1);
        println!("SEED_MIGRATION={{\"firstColdWalks\":1,\"documents\":1,\"laterWarmColdWalks\":0,\"authorityUnchanged\":true}}");
        println!("SEED_MIGRATION_RETAINED_PROFILE={}", dir.display());
    }

    #[test]
    fn seed_replacement_abort_keeps_store_and_invalidates_cache() {
        let dir = std::env::temp_dir().join(format!("garden-seed-abort-{}", Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        write_graph_json(&dir);
        write_minimal_document(&dir, "kept");
        ensure_graph_store_seeded(&dir).unwrap();
        let path = dir.join("store.oxigraph");
        let graph = read_json::<GraphRecord>(&dir.join("graph.json")).unwrap();
        let key = seed_key_for(&graph);
        let store = open_graph_store(&dir).unwrap();
        let before = crate::rdf_query_service::dump_rdf_from_store(&store, "nquads", None, None).unwrap().data;
        let count = reseed_count(&path);
        assert!(process_seed_marker_matches(&path, &key).unwrap());
        let result = with_invalidated_seed_cache(&dir, || {
            assert!(!process_seed_marker_matches(&path, &key)?);
            let mut transaction = store.start_transaction().map_err(|e| e.to_string())?;
            transaction.clear().map_err(|e| e.to_string())?;
            Err::<(), String>("injected transaction abort before commit".into())
        });
        assert_eq!(result.unwrap_err(), "injected transaction abort before commit");
        assert!(!process_seed_marker_matches(&path, &key).unwrap());
        assert_eq!(crate::rdf_query_service::dump_rdf_from_store(&store, "nquads", None, None).unwrap().data, before);
        ensure_graph_store_seeded(&dir).unwrap();
        assert!(process_seed_marker_matches(&path, &key).unwrap());
        assert_eq!(reseed_count(&path), count, "unchanged durable marker avoids a walk after abort");
        println!("SEED_FAILURE_RETAINED_PROFILE={}", dir.display());
    }

    #[test]
    fn seed_replacement_walk_gate_prevents_old_cache_refill() {
        let dir = std::env::temp_dir().join(format!("garden-seed-gate-{}", Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        write_graph_json(&dir);
        write_minimal_document(&dir, "kept");
        ensure_graph_store_seeded(&dir).unwrap();
        let path = dir.join("store.oxigraph");
        let graph = read_json::<GraphRecord>(&dir.join("graph.json")).unwrap();
        let key = seed_key_for(&graph);
        let store = open_graph_store(&dir).unwrap();
        let count = reseed_count(&path);
        std::thread::scope(|scope| {
            let (blocked_tx, blocked_rx) = std::sync::mpsc::sync_channel(1);
            let (done_tx, done_rx) = std::sync::mpsc::sync_channel(1);
            with_invalidated_seed_cache(&dir, || {
                let worker_dir = dir.clone();
                let worker_path = path.clone();
                scope.spawn(move || {
                    let lock = seed_walk_lock(&worker_path).unwrap();
                    assert!(matches!(lock.try_lock(), Err(std::sync::TryLockError::WouldBlock)));
                    blocked_tx.send(()).unwrap();
                    let result = ensure_graph_store_seeded(&worker_dir);
                    done_tx.send(result).unwrap();
                });
                blocked_rx.recv_timeout(std::time::Duration::from_secs(2)).unwrap();
                assert!(!process_seed_marker_matches(&path, &key)?);
                write_durable_seed_marker(&store, GID, "restored-old-marker")?;
                assert!(matches!(done_rx.try_recv(), Err(std::sync::mpsc::TryRecvError::Empty)));
                Ok(())
            }).unwrap();
            done_rx.recv_timeout(std::time::Duration::from_secs(2)).unwrap().unwrap();
        });
        assert!(process_seed_marker_matches(&path, &key).unwrap());
        assert_eq!(read_durable_seed_marker(&store, GID).unwrap().as_deref(), Some(key.as_str()));
        assert_eq!(reseed_count(&path), count + 1);
        ensure_graph_store_seeded(&dir).unwrap();
        assert_eq!(reseed_count(&path), count + 1, "next warm read adds no walk");
        println!("SEED_FAILURE_RETAINED_PROFILE={}", dir.display());
    }

    #[test]
    fn seed_replacement_failed_reconcile_never_rearms_marker() {
        let dir = std::env::temp_dir().join(format!("garden-seed-final-failure-{}", Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        write_graph_json(&dir);
        write_minimal_document(&dir, "kept");
        ensure_graph_store_seeded(&dir).unwrap();
        let path = dir.join("store.oxigraph");
        let graph = read_json::<GraphRecord>(&dir.join("graph.json")).unwrap();
        let key = seed_key_for(&graph);
        let store = open_graph_store(&dir).unwrap();
        with_invalidated_seed_cache(&dir, || write_durable_seed_marker(&store, GID, "restored-old-marker")).unwrap();
        fs::write(documents_dir(&dir).join("kept/document.json"), b"{broken-json").unwrap();
        assert!(ensure_graph_store_seeded(&dir).is_err());
        assert!(!process_seed_marker_matches(&path, &key).unwrap());
        assert_eq!(read_durable_seed_marker(&store, GID).unwrap().as_deref(), Some("restored-old-marker"));
        write_minimal_document(&dir, "kept");
        ensure_graph_store_seeded(&dir).unwrap();
        assert!(process_seed_marker_matches(&path, &key).unwrap());
        println!("SEED_FAILURE_RETAINED_PROFILE={}", dir.display());
    }

    fn write_graph_json(graph_dir: &Path) {
        write_json(
            &graph_dir.join("graph.json"),
            &serde_json::json!({
                "graphId": GID,
                "title": "Seed Key Test",
                "status": "active",
                "origin": "local",
                "providerId": "local-profile",
                "localPath": graph_dir.display().to_string(),
                // A small, stable `created_at` — the seed's fallback key when a graph
                // has never had a document save (content_revision absent). Distinct
                // from the (large epoch-ms) `content_revision` a later save mints.
                "createdAt": "1000",
                "updatedAt": "1000",
                "capabilities": [],
            }),
        )
        .expect("write graph.json");
    }

    fn write_minimal_document(graph_dir: &Path, document_id: &str) {
        write_stamped_document(graph_dir, document_id, "Doc One", "2000");
    }

    fn write_stamped_document(graph_dir: &Path, document_id: &str, title: &str, updated_at: &str) {
        let dir = documents_dir(graph_dir).join(document_id);
        fs::create_dir_all(&dir).expect("document dir");
        write_json(
            &dir.join("document.json"),
            &serde_json::json!({
                "documentId": document_id,
                "graphId": GID,
                "title": title,
                "body": "",
                "origin": "local",
                "providerId": "local-profile",
                "localPath": dir.display().to_string(),
                "rdfSubject": crate::rdf::document_subject(document_id),
                "createdAt": "1000",
                "updatedAt": updated_at,
                "capabilities": [],
                // A real content tree whose text carries the title, so a
                // materialization pass leaves an observable literal in the
                // document's projection graph (the seed materializer projects
                // tree triples, not manifest titles).
                "tree": {
                    "docId": document_id,
                    "root": {
                        "kind": "element",
                        "tagName": "doc",
                        "textContent": null,
                        "attributes": { "extra": {} },
                        "children": [{
                            "kind": "element",
                            "tagName": "paragraph",
                            "textContent": null,
                            "attributes": { "blockId": format!("block-{document_id}"), "extra": {} },
                            "children": [{
                                "kind": "text",
                                "tagName": null,
                                "textContent": title,
                                "attributes": { "extra": {} },
                                "children": [],
                            }],
                        }],
                    },
                },
            }),
        )
        .expect("write document.json");
    }

    #[test]
    fn pre_cancelled_external_seed_stops_before_filesystem_work() {
        let graph_dir = std::env::temp_dir().join(format!("sophia-seed-cancel-{}", Uuid::new_v4()));
        let cancellation = CancellationToken::new();
        cancellation.cancel();

        let error = ensure_graph_store_seeded_with_cancellation(&graph_dir, &cancellation)
            .expect_err("pre-cancelled seed must stop before graph inspection");
        assert!(error.contains("cancelled"), "{error}");
        assert!(
            !graph_dir.exists(),
            "cancellation must not create graph/store state"
        );
    }

    /// THE RESEED-STORM FIX (appraisal §4.4 / P1-6), proven with a real graph +
    /// real document record + the real seed path (no mocks): keying the seed marker
    /// on the DOCUMENT-side `content_revision` (not `graph.updated_at`) makes a pure
    /// RDF-only write a no-op for the seed, while a document save still reseeds.
    ///
    /// The observable is the per-store count of FULL re-materialization passes — the
    /// pass that runs `materialize_document_record` over EVERY document. "The pass did
    /// not run" is exactly "the documents were not re-materialized".
    #[test]
    fn rdf_only_write_does_not_reseed_documents_but_a_document_save_does() {
        // The real profile layout: `<profile>/graphs/<graph_id>` — the graph-record
        // writers (`touch_graph_*`) refuse a graph dir that is not inside a `graphs`
        // directory, so mirror it exactly (as the room-mutation test does).
        let profile_dir = std::env::temp_dir().join(format!("sophia-seed-key-{}", Uuid::new_v4()));
        let graph_dir = profile_dir.join("graphs").join(GID);
        fs::create_dir_all(&graph_dir).expect("graph dir");
        write_graph_json(&graph_dir);
        write_minimal_document(&graph_dir, "doc-1");
        let store_path = graph_dir.join("store.oxigraph");

        // (0) BOOT: the in-memory marker is absent → the first call runs one full
        //     re-materialization pass (graph record + workspace + every document).
        ensure_graph_store_seeded(&graph_dir).expect("boot seed");
        assert_eq!(
            reseed_count(&store_path),
            1,
            "boot performs exactly one document re-materialization pass"
        );

        // (b) THE FIX — an RDF-only write (memory / emporium / salience / song /
        //     semantic index / user sparql_update) bumps ONLY `updated_at`, never
        //     `content_revision`, so the seed key is unchanged and the NEXT rdf call
        //     SHORT-CIRCUITS: NO document re-materialization, no storm.
        touch_graph_updated_at(&graph_dir).expect("rdf-only write graph touch");
        ensure_graph_store_seeded(&graph_dir).expect("seed after rdf-only write");
        assert_eq!(
            reseed_count(&store_path),
            1,
            "a pure RDF-only write must NOT re-materialize the documents (the storm is dead)"
        );

        // (a) EXISTING BEHAVIOR PRESERVED — a document save bumps `content_revision`,
        //     changing the seed key, so the next call reseeds THAT graph's documents.
        touch_graph_content_revision(&graph_dir).expect("document save graph touch");
        ensure_graph_store_seeded(&graph_dir).expect("seed after document save");
        assert_eq!(
            reseed_count(&store_path),
            2,
            "a document save must reseed the document projection"
        );

        // Converged: with nothing further changed, a subsequent call short-circuits.
        ensure_graph_store_seeded(&graph_dir).expect("converged seed");
        assert_eq!(
            reseed_count(&store_path),
            2,
            "a converged seed does not re-run the pass"
        );

        let _ = fs::remove_dir_all(&profile_dir);
    }

    /// THE FRESH-POD FIX: the seed marker now ALSO lives durably inside the
    /// Oxigraph store (reserved `…:projection:seed` graph), so a restart —
    /// which wipes the process-local marker exactly as a pod replacement does
    /// — must NOT re-run the materialization pass over a store that already
    /// contains the matching materialization. Real store on disk, really
    /// reopened from its RocksDB directory after the cache handle is dropped;
    /// the observables are the same pass/document counters the reseed-storm
    /// test pins.
    #[test]
    fn restart_with_hydrated_store_does_not_reseed() {
        let profile_dir =
            std::env::temp_dir().join(format!("sophia-seed-restart-{}", Uuid::new_v4()));
        let graph_dir = profile_dir.join("graphs").join(GID);
        fs::create_dir_all(&graph_dir).expect("graph dir");
        write_graph_json(&graph_dir);
        write_minimal_document(&graph_dir, "doc-1");
        let store_path = graph_dir.join("store.oxigraph");

        // Boot: one pass, one document materialized, durable marker written.
        ensure_graph_store_seeded(&graph_dir).expect("boot seed");
        assert_eq!(reseed_count(&store_path), 1);
        assert_eq!(seed_document_materialization_count(&store_path), 1);

        // "Restart": forget the process-local marker AND drop the cached
        // store handle, so the next call must reopen the store from disk and
        // can only know it is seeded through the durable in-store marker.
        forget_process_seed_marker_for_test(&store_path);
        crate::rdf_store_service::evict_graph_store(&graph_dir).expect("drop cached store handle");

        ensure_graph_store_seeded(&graph_dir).expect("post-restart seed check");
        assert_eq!(
            reseed_count(&store_path),
            1,
            "a hydrated-restored store must NOT re-run the materialization pass"
        );
        assert_eq!(
            seed_document_materialization_count(&store_path),
            1,
            "a hydrated-restored store must NOT re-materialize any document"
        );

        // And the fast path re-armed the process marker: a further call
        // never reopens the walk either.
        ensure_graph_store_seeded(&graph_dir).expect("re-armed process marker");
        assert_eq!(reseed_count(&store_path), 1);

        crate::rdf_store_service::evict_graph_store(&graph_dir).expect("evict store");
        let _ = fs::remove_dir_all(&profile_dir);
    }

    /// THE BOUNDED-WALK FIX: a marker mismatch no longer re-materializes
    /// every document. Every content writer reconciles its own document RDF
    /// before bumping `content_revision`, so the walk only needs documents
    /// stamped at/after the PREVIOUS marker's revision: a workspace-only
    /// `content_revision` bump (what `persist_materialized_workspace` does on
    /// every workspace flush of an active cell) re-materializes ZERO
    /// documents, while a real document save re-materializes exactly the
    /// saved one — proven against the real store by reading the saved title
    /// back out of the document's projection graph.
    #[test]
    fn marker_mismatch_walk_is_incremental_over_document_stamps() {
        let profile_dir =
            std::env::temp_dir().join(format!("sophia-seed-incremental-{}", Uuid::new_v4()));
        let graph_dir = profile_dir.join("graphs").join(GID);
        fs::create_dir_all(&graph_dir).expect("graph dir");
        write_graph_json(&graph_dir);
        // Stamped BELOW the boot marker's revision fallback ("1000" =
        // createdAt), so later incremental walks can prove they skip it.
        write_stamped_document(&graph_dir, "doc-old", "Old Doc", "500");
        let store_path = graph_dir.join("store.oxigraph");

        // Boot: no previous marker → FULL walk, the old document included.
        ensure_graph_store_seeded(&graph_dir).expect("boot seed");
        assert_eq!(reseed_count(&store_path), 1);
        assert_eq!(seed_document_materialization_count(&store_path), 1);

        // A workspace-only content bump: the pass runs (marker mismatch) but
        // must re-materialize ZERO documents — doc-old's stamp (500) is
        // strictly older than the previous marker revision (1000).
        touch_graph_content_revision(&graph_dir).expect("workspace-style content bump");
        ensure_graph_store_seeded(&graph_dir).expect("seed after workspace bump");
        assert_eq!(reseed_count(&store_path), 2, "the pass itself runs");
        assert_eq!(
            seed_document_materialization_count(&store_path),
            1,
            "a workspace-only content bump must re-materialize ZERO documents"
        );

        // A real document save: fresh updatedAt stamp (>= the marker floor)
        // plus the content bump every save performs. Exactly ONE document
        // re-materializes, and the new title is really in the store.
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_millis()
            .to_string();
        write_stamped_document(&graph_dir, "doc-old", "Old Doc Renamed", &now_ms);
        touch_graph_content_revision(&graph_dir).expect("document-save content bump");
        ensure_graph_store_seeded(&graph_dir).expect("seed after document save");
        assert_eq!(reseed_count(&store_path), 3);
        assert_eq!(
            seed_document_materialization_count(&store_path),
            2,
            "a document save re-materializes exactly the saved document"
        );

        // The saved document's projection graph really carries the NEW tree
        // content (the stamped tree's text = the new title), and no longer
        // the old one — the incremental pass genuinely re-materialized it.
        let store = crate::rdf_store_service::open_graph_store(&graph_dir).expect("open store");
        let projection_graph = crate::rdf_authority::document_projection_graph_iri(GID, "doc-old");
        let renamed = crate::rdf_query_service::execute_sparql_query(
            &store,
            &format!(r#"ASK {{ GRAPH <{projection_graph}> {{ ?s ?p "Old Doc Renamed" }} }}"#),
        )
        .expect("query renamed tree text");
        assert_eq!(
            renamed.boolean,
            Some(true),
            "the incremental pass must land the saved document's new content in its projection graph"
        );
        let stale = crate::rdf_query_service::execute_sparql_query(
            &store,
            &format!(r#"ASK {{ GRAPH <{projection_graph}> {{ ?s ?p "Old Doc" }} }}"#),
        )
        .expect("query stale tree text");
        assert_eq!(
            stale.boolean,
            Some(false),
            "re-materialization replaces the document's projection, never accretes it"
        );

        crate::rdf_store_service::evict_graph_store(&graph_dir).expect("evict store");
        let _ = fs::remove_dir_all(&profile_dir);
    }

    #[test]
    fn replacement_incarnation_never_inherits_same_path_seed_marker() {
        let profile_dir =
            std::env::temp_dir().join(format!("sophia-seed-incarnation-{}", Uuid::new_v4()));
        let graph_dir = profile_dir.join("graphs").join(GID);
        fs::create_dir_all(&graph_dir).expect("graph dir");
        write_json(
            &graph_dir.join("graph.json"),
            &serde_json::json!({
                "graphId": GID,
                "title": "First incarnation",
                "status": "active",
                "origin": "local",
                "providerId": "local-profile",
                "localPath": graph_dir.display().to_string(),
                "createdAt": "1000",
                "incarnationId": "incarnation-one",
                "updatedAt": "5000",
                "contentRevision": "5000",
                "capabilities": [],
            }),
        )
        .expect("first graph record");
        write_minimal_document(&graph_dir, "doc-1");
        let store_path = graph_dir.join("store.oxigraph");
        ensure_graph_store_seeded(&graph_dir).expect("seed first incarnation");
        assert_eq!(reseed_count(&store_path), 1);

        crate::rdf_store_service::evict_graph_store(&graph_dir).expect("evict first store");
        fs::remove_dir_all(&store_path).expect("remove first store");
        write_json(
            &graph_dir.join("graph.json"),
            &serde_json::json!({
                "graphId": GID,
                "title": "Replacement incarnation",
                "status": "active",
                "origin": "local",
                "providerId": "local-profile",
                "localPath": graph_dir.display().to_string(),
                "createdAt": "1000",
                "incarnationId": "incarnation-two",
                "updatedAt": "5000",
                "contentRevision": "5000",
                "capabilities": [],
            }),
        )
        .expect("replacement graph record with deliberately reused revision");

        ensure_graph_store_seeded(&graph_dir).expect("seed replacement incarnation");
        assert_eq!(
            reseed_count(&store_path),
            2,
            "path-local seed state skipped a replacement graph incarnation"
        );

        crate::rdf_store_service::evict_graph_store(&graph_dir).expect("evict replacement store");
        let _ = fs::remove_dir_all(&profile_dir);
    }
}
