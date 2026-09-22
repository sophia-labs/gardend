use crate::app_runtime::AppHandle;
use crate::{
    app_error::{AppError, AppResult},
    graph_catalog_store::{profile_dir_from_graph_dir, upsert_cached_profile_graph_record},
    ids::validate_local_id,
    paths::{graphs_dir, profile_dir},
    runtime_config::{
        default_validation_policy, is_default_validation_policy, ValidationPolicy,
        GRAPH_STATUS_ACTIVE, GRAPH_STATUS_DELETED,
    },
    storage::{read_json, write_json},
};
use fs4::fs_std::FileExt;
use serde::{Deserialize, Serialize};
#[cfg(test)]
use std::sync::{mpsc::Sender, Mutex, OnceLock};
use std::{
    fs::{File, OpenOptions},
    path::{Path, PathBuf},
};
use uuid::Uuid;

pub(crate) const GRAPH_INCARNATION_HEADER: &str = "x-graph-incarnation";
pub(crate) const GRAPH_INCARNATION_QUERY: &str = "graph_incarnation";

struct GraphRecordMutationGuard {
    _file: File,
}

#[cfg(test)]
struct GraphLockAttemptHook {
    lock_path: PathBuf,
    sender: Sender<PathBuf>,
}

#[cfg(test)]
static GRAPH_LOCK_ATTEMPT_HOOK: OnceLock<Mutex<Option<GraphLockAttemptHook>>> = OnceLock::new();

#[cfg(test)]
fn install_graph_lock_attempt_hook(lock_path: PathBuf, sender: Sender<PathBuf>) {
    *GRAPH_LOCK_ATTEMPT_HOOK
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) =
        Some(GraphLockAttemptHook { lock_path, sender });
}

#[cfg(test)]
fn clear_graph_lock_attempt_hook() {
    *GRAPH_LOCK_ATTEMPT_HOOK
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
}

#[cfg(test)]
fn notify_graph_lock_attempt(lock_path: &Path) {
    let sender = GRAPH_LOCK_ATTEMPT_HOOK
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .as_ref()
        .filter(|hook| hook.lock_path == lock_path)
        .map(|hook| hook.sender.clone());
    if let Some(sender) = sender {
        let _ = sender.send(lock_path.to_path_buf());
    }
}

fn graph_record_lock_path(graph_dir: &Path) -> PathBuf {
    graph_dir.join(".graph-record-lock").join(".lock")
}

fn acquire_graph_record_mutation_lock(graph_dir: &Path) -> AppResult<GraphRecordMutationGuard> {
    let lock_path = graph_record_lock_path(graph_dir);
    let parent = lock_path.parent().ok_or_else(|| {
        AppError::storage(format!(
            "graph record lock has no parent: {}",
            lock_path.display()
        ))
    })?;
    crate::storage::create_dir_all(parent).map_err(AppError::storage)?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .map_err(|error| {
            AppError::storage(format!(
                "open graph record lock {}: {error}",
                lock_path.display()
            ))
        })?;
    #[cfg(test)]
    notify_graph_lock_attempt(&lock_path);
    file.lock_exclusive().map_err(|error| {
        AppError::storage(format!(
            "lock graph record {}: {error}",
            lock_path.display()
        ))
    })?;
    Ok(GraphRecordMutationGuard { _file: file })
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GraphRecord {
    pub(crate) graph_id: String,
    pub(crate) title: String,
    #[serde(default)]
    pub(crate) description: Option<String>,
    #[serde(default = "default_graph_status")]
    pub(crate) status: String,
    pub(crate) origin: String,
    pub(crate) provider_id: String,
    pub(crate) local_path: String,
    pub(crate) created_at: String,
    /// Immutable graph-incarnation fence for durable queued operations. New
    /// graphs receive a random UUID; legacy records are backfilled under the
    /// graph lifecycle lease before their next enqueue.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) incarnation_id: Option<String>,
    pub(crate) updated_at: String,
    pub(crate) capabilities: Vec<String>,
    /// Set when the graph was created as part of a CRDT operation (e.g. graph.importArchive).
    /// Used by replay guards to detect partial-replay vs. conflict on a graph_id collision.
    /// Optional: graphs created via the direct `create_graph` Tauri command leave this absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) created_by_operation_id: Option<String>,
    /// The per-graph memory-validation policy (EA-6). Defaults to
    /// [`ValidationPolicy::Halt`] when absent (legacy graphs), and is skipped on
    /// serialization when it equals the default so a legacy `graph.json`
    /// round-trips byte-for-byte. The memory apply fork reads this to decide
    /// whether a SHACL violation halts the write, flags-and-accepts it (recording
    /// to the violation ledger), or is skipped entirely.
    #[serde(
        default = "default_validation_policy",
        skip_serializing_if = "is_default_validation_policy"
    )]
    pub(crate) validation_policy: ValidationPolicy,
    /// The DOCUMENT-side content revision — bumped ONLY by writes that change a
    /// file-backed record the Oxigraph seed materializes (document save/create/
    /// delete + workspace snapshot save + graph-metadata edit), via
    /// [`touch_graph_content_revision`]. The seed marker keys on THIS, not on
    /// `updated_at`: an RDF-only write (memory / salience / song / semantic index /
    /// user `sparql_update` / emporium projection) still bumps `updated_at` for
    /// display freshness but leaves `content_revision` untouched, so it no longer
    /// invalidates the document seed and triggers the whole-graph reseed storm
    /// (appraisal §4.4 / P1-6). `None` on a legacy graph (or one that has never had
    /// a document save) — the seed then falls back to the immutable `created_at`, so
    /// it seeds once on boot and never re-materializes on a pure RDF write. Skipped
    /// on serialization when absent so a legacy `graph.json` round-trips byte-for-byte.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) content_revision: Option<String>,
}

pub(crate) fn read_graph_record(
    app: &AppHandle,
    graph_id: &str,
) -> AppResult<(PathBuf, GraphRecord)> {
    validate_local_id(graph_id, "graph_id").map_err(AppError::validation)?;
    let graph_dir = graphs_dir(app).map_err(AppError::storage)?.join(graph_id);
    let graph_path = graph_dir.join("graph.json");
    if !graph_path.is_file() {
        #[cfg(all(feature = "headless", not(feature = "desktop")))]
        {
            if crate::runtime_config::self_heal_graphs_enabled() {
                crate::graph_paths::self_heal_missing_graph(app, graph_id)
                    .map_err(AppError::storage)?;
            } else {
                // Self-heal compiled in but disabled (env unset): preserve the
                // original not-found (404) status rather than falling through
                // to self_heal_missing_graph's own "disabled" error, which is
                // a bare String that would get mapped to AppError::storage
                // (500) below — see F4c security review finding 4.
                return Err(AppError::not_found(format!("graph not found: {graph_id}")));
            }
        }
        #[cfg(any(not(feature = "headless"), feature = "desktop"))]
        return Err(AppError::not_found(format!("graph not found: {graph_id}")));
    }
    let graph = read_json::<GraphRecord>(&graph_path).map_err(AppError::storage)?;
    if let Err(error) = profile_dir(app)
        .map_err(AppError::storage)
        .and_then(|profile_dir| upsert_cached_profile_graph_record(&profile_dir, &graph))
    {
        log::warn!("Failed to refresh graph catalog cache for {graph_id}: {error}");
    }
    if graph.status == GRAPH_STATUS_DELETED {
        return Err(AppError::not_found(format!("graph not found: {graph_id}")));
    }
    Ok((graph_dir, graph))
}

/// Read an active graph without invoking headless self-heal. Lifecycle and
/// replay validation must never create the graph it is trying to reject.
pub(crate) fn read_graph_record_no_heal(
    app: &AppHandle,
    graph_id: &str,
) -> AppResult<(PathBuf, GraphRecord)> {
    validate_local_id(graph_id, "graph_id").map_err(AppError::validation)?;
    let graph_dir = graphs_dir(app).map_err(AppError::storage)?.join(graph_id);
    let graph_path = graph_dir.join("graph.json");
    if !graph_path.is_file() {
        return Err(AppError::not_found(format!("graph not found: {graph_id}")));
    }
    let graph = read_json::<GraphRecord>(&graph_path).map_err(AppError::storage)?;
    if graph.status == GRAPH_STATUS_DELETED {
        return Err(AppError::not_found(format!("graph not found: {graph_id}")));
    }
    Ok((graph_dir, graph))
}

/// Called while the graph lifecycle lease is held by enqueue. The write does
/// not change updated_at/content_revision because the UUID is internal storage
/// identity, not user-visible graph content.
pub(crate) fn ensure_graph_incarnation(app: &AppHandle, graph_id: &str) -> AppResult<String> {
    let (graph_dir, graph) = read_graph_record_no_heal(app, graph_id)?;
    if let Some(incarnation_id) = graph.incarnation_id {
        return Ok(incarnation_id);
    }
    mutate_graph_record(&graph_dir, |graph| {
        if let Some(incarnation_id) = graph.incarnation_id.clone() {
            return Ok(incarnation_id);
        }
        let incarnation_id = Uuid::new_v4().to_string();
        graph.incarnation_id = Some(incarnation_id.clone());
        Ok(incarnation_id)
    })
}

pub(crate) fn write_graph_record(graph_dir: &Path, graph: &GraphRecord) -> AppResult<()> {
    write_graph_record_canonical(graph_dir, graph)?;
    upsert_graph_record_from_graph_dir(graph_dir, graph)
}

/// Write only the canonical graph manifest. Lifecycle transitions sometimes
/// need to establish the tombstone boundary before a best-effort catalog cache
/// projection; callers must explicitly handle that projection afterward.
pub(crate) fn write_graph_record_canonical(graph_dir: &Path, graph: &GraphRecord) -> AppResult<()> {
    let _durability_guard = crate::cell_durability::write_guard();
    write_json(&graph_dir.join("graph.json"), graph).map_err(AppError::storage)
}

/// Serialize one read-modify-write of canonical `graph.json` by graph path.
/// The file lock is released by the OS on process death and is deliberately
/// separate from the async lifecycle coordinator: direct RDF writes and cold
/// projection tails can originate outside that coordinator.
pub(crate) fn mutate_graph_record<T>(
    graph_dir: &Path,
    mutate: impl FnOnce(&mut GraphRecord) -> AppResult<T>,
) -> AppResult<T> {
    let _durability_guard = crate::cell_durability::write_guard();
    let _guard = acquire_graph_record_mutation_lock(graph_dir)?;
    let mut graph =
        read_json::<GraphRecord>(&graph_dir.join("graph.json")).map_err(AppError::storage)?;
    let result = mutate(&mut graph)?;
    write_graph_record(graph_dir, &graph)?;
    Ok(result)
}

/// Serialize one read-modify-write of canonical `graph.json` without making
/// the profile catalog projection part of the commit result. Lifecycle
/// boundaries such as a soft-delete tombstone use this variant so a cache
/// refresh failure cannot turn an already-committed canonical mutation into
/// an apparent failure. The caller remains responsible for refreshing any
/// derived projections after this returns.
pub(crate) fn mutate_graph_record_canonical<T>(
    graph_dir: &Path,
    mutate: impl FnOnce(&mut GraphRecord) -> AppResult<T>,
) -> AppResult<T> {
    let _durability_guard = crate::cell_durability::write_guard();
    let _guard = acquire_graph_record_mutation_lock(graph_dir)?;
    let mut graph =
        read_json::<GraphRecord>(&graph_dir.join("graph.json")).map_err(AppError::storage)?;
    let result = mutate(&mut graph)?;
    write_graph_record_canonical(graph_dir, &graph)?;
    Ok(result)
}

pub(crate) fn touch_graph_updated_at(graph_dir: &Path) -> AppResult<()> {
    mutate_graph_record(graph_dir, |graph| {
        let now = crate::clock::epoch_millis();
        let updated_at = crate::clock::parse_timestamp(&graph.updated_at).unwrap_or(0);
        let content_revision = graph
            .content_revision
            .as_deref()
            .and_then(crate::clock::parse_timestamp)
            .unwrap_or(0);
        graph.updated_at = now.max(updated_at).max(content_revision).to_string();
        Ok(())
    })
}

/// Bump BOTH `updated_at` (display freshness) AND `content_revision` (the Oxigraph
/// seed marker key). Called by the write paths that change a file-backed record the
/// seed re-materializes — document save/create/delete and workspace-snapshot save —
/// so the NEXT rdf call reseeds THAT graph's document projection (preserving the
/// existing catch-up behavior), while pure RDF-only writes (which call the plain
/// [`touch_graph_updated_at`]) do NOT bump `content_revision` and so no longer storm.
pub(crate) fn next_content_revision(previous: Option<&str>) -> AppResult<String> {
    next_content_revision_at(previous, crate::clock::epoch_millis())
}

fn next_content_revision_at(previous: Option<&str>, now: u128) -> AppResult<String> {
    let next = match previous {
        Some(previous) => {
            let previous = crate::clock::parse_timestamp(previous).ok_or_else(|| {
                AppError::storage(format!("invalid graph content revision: {previous}"))
            })?;
            if previous >= now {
                previous.checked_add(1).ok_or_else(|| {
                    AppError::internal(
                        "graph content revision exhausted the epoch-millisecond range",
                    )
                })?
            } else {
                now
            }
        }
        None => now,
    };
    Ok(next.to_string())
}

pub(crate) fn touch_graph_content_revision(graph_dir: &Path) -> AppResult<String> {
    mutate_graph_record(graph_dir, |graph| {
        let next = next_content_revision(graph.content_revision.as_deref())?;
        // The RDF seed marker keys on content_revision. Two real content writes
        // in one millisecond must still produce distinct keys.
        graph.updated_at = next.clone();
        graph.content_revision = Some(next.clone());
        Ok(next)
    })
}

fn default_graph_status() -> String {
    GRAPH_STATUS_ACTIVE.to_string()
}

fn upsert_graph_record_from_graph_dir(graph_dir: &Path, graph: &GraphRecord) -> AppResult<()> {
    let profile_dir = profile_dir_from_graph_dir(graph_dir)?;
    upsert_cached_profile_graph_record(&profile_dir, graph)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{sync::mpsc, time::Duration};
    use uuid::Uuid;

    #[test]
    fn graph_record_default_status_is_active() {
        let value = serde_json::json!({
            "graphId": "demo",
            "title": "Demo",
            "origin": "local",
            "providerId": "local-profile",
            "localPath": "/tmp/demo",
            "createdAt": "1",
            "updatedAt": "1",
            "capabilities": [],
        });

        let record: GraphRecord = serde_json::from_value(value).expect("deserialize graph record");

        assert_eq!(record.status, GRAPH_STATUS_ACTIVE);
        assert!(record.incarnation_id.is_none());
    }

    #[test]
    fn content_revision_is_strictly_monotonic_within_one_millisecond() {
        let profile = std::env::temp_dir().join(format!(
            "garden-content-revision-monotonic-{}",
            Uuid::new_v4()
        ));
        let graph_dir = profile.join("graphs").join("monotonic");
        std::fs::create_dir_all(&graph_dir).expect("create graph dir");
        let now = crate::clock::epoch_millis().to_string();
        write_json(
            &graph_dir.join("graph.json"),
            &serde_json::json!({
                "graphId": "monotonic",
                "title": "Monotonic",
                "status": "active",
                "origin": "local",
                "providerId": "local-profile",
                "localPath": graph_dir.display().to_string(),
                "createdAt": now,
                "updatedAt": now,
                "contentRevision": now,
                "capabilities": [],
            }),
        )
        .expect("write graph record");

        touch_graph_content_revision(&graph_dir).expect("first content touch");
        let first = read_json::<GraphRecord>(&graph_dir.join("graph.json"))
            .expect("first graph record")
            .content_revision
            .and_then(|value| crate::clock::parse_timestamp(&value))
            .expect("first numeric revision");
        touch_graph_content_revision(&graph_dir).expect("second content touch");
        let second = read_json::<GraphRecord>(&graph_dir.join("graph.json"))
            .expect("second graph record")
            .content_revision
            .and_then(|value| crate::clock::parse_timestamp(&value))
            .expect("second numeric revision");
        assert!(
            second > first,
            "content revision did not advance: {first} -> {second}"
        );

        let _ = std::fs::remove_dir_all(profile);
    }

    #[test]
    fn shared_next_content_revision_advances_at_the_same_clock_value() {
        assert_eq!(
            next_content_revision_at(Some("2000"), 2000).expect("same-ms revision"),
            "2001"
        );
        assert_eq!(
            next_content_revision_at(Some("2001"), 2000).expect("future revision"),
            "2002"
        );
        assert!(next_content_revision_at(Some("not-a-revision"), 2000).is_err());
    }

    #[test]
    fn concurrent_graph_manifest_mutations_preserve_content_revision_and_other_fields() {
        let profile = std::env::temp_dir().join(format!(
            "garden-graph-record-concurrent-rmw-{}",
            Uuid::new_v4()
        ));
        let graph_dir = profile.join("graphs").join("concurrent-rmw");
        std::fs::create_dir_all(&graph_dir).expect("create graph dir");
        let pinned = crate::clock::epoch_millis() + 60_000;
        write_json(
            &graph_dir.join("graph.json"),
            &serde_json::json!({
                "graphId": "concurrent-rmw",
                "title": "Concurrent RMW",
                "description": "original description",
                "status": "active",
                "origin": "local",
                "providerId": "local-profile",
                "localPath": graph_dir.display().to_string(),
                "createdAt": pinned.to_string(),
                "updatedAt": pinned.to_string(),
                "contentRevision": pinned.to_string(),
                "capabilities": ["original-capability"],
            }),
        )
        .expect("write graph record");

        let guard = acquire_graph_record_mutation_lock(&graph_dir).expect("hold graph record lock");
        let lock_path = graph_record_lock_path(&graph_dir);
        let (attempt_tx, attempt_rx) = mpsc::channel();
        install_graph_lock_attempt_hook(lock_path.clone(), attempt_tx);

        let mut handles = Vec::new();
        for _ in 0..3 {
            let graph_dir = graph_dir.clone();
            handles.push(std::thread::spawn(move || {
                touch_graph_content_revision(&graph_dir).map(|_| ())
            }));
        }
        let rdf_graph_dir = graph_dir.clone();
        handles.push(std::thread::spawn(move || {
            touch_graph_updated_at(&rdf_graph_dir)
        }));
        let metadata_graph_dir = graph_dir.clone();
        handles.push(std::thread::spawn(move || {
            mutate_graph_record(&metadata_graph_dir, |graph| {
                graph.description = Some("concurrent description".to_string());
                graph.capabilities.push("concurrent-capability".to_string());
                Ok(())
            })
        }));

        let mut attempt_error = None;
        for _ in 0..5 {
            match attempt_rx.recv_timeout(Duration::from_secs(5)) {
                Ok(attempted_path) => assert_eq!(attempted_path, lock_path),
                Err(error) => {
                    attempt_error = Some(format!("graph mutation did not contend: {error}"));
                    break;
                }
            }
        }
        clear_graph_lock_attempt_hook();
        drop(guard);
        for handle in handles {
            handle
                .join()
                .expect("graph mutation thread")
                .expect("graph mutation");
        }
        if let Some(error) = attempt_error {
            panic!("{error}");
        }

        let stored =
            read_json::<GraphRecord>(&graph_dir.join("graph.json")).expect("final graph record");
        let final_revision = stored
            .content_revision
            .as_deref()
            .and_then(crate::clock::parse_timestamp)
            .expect("final content revision");
        assert_eq!(final_revision, pinned + 3);
        assert!(
            crate::clock::parse_timestamp(&stored.updated_at).expect("updatedAt") >= final_revision,
            "an RDF timestamp touch must not roll updatedAt below contentRevision"
        );
        assert_eq!(
            stored.description.as_deref(),
            Some("concurrent description")
        );
        assert!(stored
            .capabilities
            .iter()
            .any(|capability| capability == "original-capability"));
        assert!(stored
            .capabilities
            .iter()
            .any(|capability| capability == "concurrent-capability"));

        clear_graph_lock_attempt_hook();
        let _ = std::fs::remove_dir_all(profile);
    }
}
