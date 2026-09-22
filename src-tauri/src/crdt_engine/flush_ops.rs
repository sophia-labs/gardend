//! Cold-projection flush for hosted Y.Doc rooms.

use crate::app_runtime::AppHandle;
use crate::crdt_engine::executor::{ApplyOperationError, ApplyOperationResult};
use crate::crdt_engine::rooms::{Room, RoomRegistry};
use crate::crdt_queue::CrdtOperation;
use serde_json::{json, Value};
use std::path::Path;
use std::sync::Arc;
#[cfg(feature = "desktop")]
use tauri::Manager;

pub(crate) const HYDRATE_PERSISTED_SIDECARS_PAYLOAD_KEY: &str = "hydratePersistedSidecars";

#[cfg(test)]
#[derive(Default)]
struct FlushFailureInjection {
    graph_id: String,
    remaining: usize,
    attempts: usize,
}

#[cfg(test)]
static FLUSH_FAILURE_INJECTION: std::sync::OnceLock<std::sync::Mutex<FlushFailureInjection>> =
    std::sync::OnceLock::new();

#[cfg(test)]
pub(crate) fn fail_next_flush_attempts_for_test(graph_id: &str, attempts: usize) {
    *FLUSH_FAILURE_INJECTION
        .get_or_init(|| std::sync::Mutex::new(FlushFailureInjection::default()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = FlushFailureInjection {
        graph_id: graph_id.to_string(),
        remaining: attempts,
        attempts: 0,
    };
}

#[cfg(test)]
pub(crate) fn flush_attempts_for_test(graph_id: &str) -> usize {
    let state = FLUSH_FAILURE_INJECTION
        .get_or_init(|| std::sync::Mutex::new(FlushFailureInjection::default()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    (state.graph_id == graph_id)
        .then_some(state.attempts)
        .unwrap_or(0)
}

#[cfg(test)]
fn maybe_fail_flush_for_test(graph_id: &str) -> Result<(), String> {
    let mut state = FLUSH_FAILURE_INJECTION
        .get_or_init(|| std::sync::Mutex::new(FlushFailureInjection::default()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if state.graph_id != graph_id {
        return Ok(());
    }
    state.attempts = state.attempts.saturating_add(1);
    if state.remaining > 0 {
        state.remaining -= 1;
        return Err(format!("injected cold-tail storage failure for {graph_id}"));
    }
    Ok(())
}

pub(crate) async fn apply(app: &AppHandle, operation: &CrdtOperation) -> Result<Value, String> {
    apply_classified(app, operation)
        .await
        .map_err(ApplyOperationError::into_message)
}

fn classify_flush_error(error: String) -> ApplyOperationError {
    let terminal = [
        "graph not found:",
        "stale graph generation",
        "stale graph incarnation",
        "room was evicted",
        "revision conflict",
        "precondition",
        "folder parent cycle",
        "document tombstoned:",
        "invalid document",
        "invalid graph",
        "outside the",
        "decode ",
    ]
    .iter()
    .any(|marker| error.contains(marker));
    if terminal {
        ApplyOperationError::terminal(error)
    } else {
        // Filesystem, RDF, history, and other cold-tail failures retain the
        // exact journal operation until the scheduled retry repairs them.
        ApplyOperationError::retryable_after_hot_commit(error)
    }
}

pub(crate) async fn apply_classified(
    app: &AppHandle,
    operation: &CrdtOperation,
) -> ApplyOperationResult<Value> {
    let graph_id = &operation.graph_id;
    if let Some(expected_generation) = operation
        .payload
        .get("graphGeneration")
        .and_then(Value::as_u64)
    {
        if let Some(coordinator) =
            app.try_state::<super::persistence_coordinator::GraphPersistenceCoordinator>()
        {
            coordinator
                .require_generation(graph_id, expected_generation)
                .map_err(ApplyOperationError::terminal)?;
        }
    }
    let graph_dir =
        crate::graph_paths::existing_graph_dir(app, graph_id).map_err(classify_flush_error)?;
    let registry = app.state::<RoomRegistry>();
    let recovered = operation
        .payload
        .get(crate::crdt_queue::RECOVERED_OPERATION_PAYLOAD_KEY)
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let explicit_sidecar_hydration = operation
        .payload
        .get(HYDRATE_PERSISTED_SIDECARS_PAYLOAD_KEY)
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let hydrate_rooms = recovered || explicit_sidecar_hydration;
    // Whether the recovered/explicit hydration covers the WHOLE graph. The
    // graph-wide sweep is handled below one room at a time — the old shape
    // (pre-hydrate EVERY persisted sidecar into the registry, then walk the
    // registry) decoded every document's full rewrite history at ~10x its
    // durable size and RETAINED all of it (the registry never evicts on its
    // own), which OOM'd 8Gi cells the moment startup recovery replayed one
    // graph-wide flush left pending by a previous crash — and each such OOM
    // left the same journal op pending for the NEXT pod: a death loop.
    let enumerate_graph_documents = hydrate_rooms
        && operation
            .document_id
            .as_deref()
            .filter(|id| !id.is_empty())
            .is_none()
        && (explicit_sidecar_hydration
            || operation
                .payload
                .get("graphGeneration")
                .and_then(Value::as_u64)
                .is_none());
    let workspace_key = format!("workspace:{graph_id}");
    let workspace_was_live = registry.peek(&workspace_key).await.is_some();
    let named_document_key = operation
        .document_id
        .as_deref()
        .filter(|id| !id.is_empty())
        .map(|id| format!("doc:{graph_id}:{id}"));
    let named_document_was_live = match &named_document_key {
        Some(key) => registry.peek(key).await.is_some(),
        None => false,
    };
    if hydrate_rooms {
        hydrate_workspace_and_named_document_room(
            &registry,
            graph_id,
            &graph_dir,
            operation.document_id.as_deref(),
        )
        .await
        .map_err(classify_flush_error)?;
    }
    // The workspace room hydrated above deliberately STAYS live: one
    // workspace room is O(1) residency (not the O(all-docs) disease), and
    // leaving it live after a recovered flush is the pre-existing contract
    // (pinned by persistence_tests::
    // recovered_automatic_workspace_flush_does_not_hydrate_unrelated_documents).
    let _ = workspace_was_live;

    // DOCUMENT rooms this op hydrates must not outlive it — on ANY exit
    // path. The fallible body runs as an inner future so a workspace-persist
    // or doc-flush error cannot return before the named-room eviction at the
    // bottom: a decoded room leaked through an early return would read as
    // "pre-live" to every retry of the same journal op and never be evicted
    // again, rebuilding the recovered-flush leak one error at a time. (The
    // sweep's own rooms are already evicted inline on success AND error.)
    let named_document_hydrated = hydrate_rooms && !named_document_was_live;
    let outcome: ApplyOperationResult<Value> = async {
        #[cfg(test)]
        maybe_fail_flush_for_test(graph_id).map_err(classify_flush_error)?;

        // Desktop flushes the filesystem/workspace channel before document
        // channels. Preserve that ordering so a just-created browser document
        // has a durable workspace entry (and title) before its document
        // record lands.
        let mut workspace_flushed = false;
        if let Some(room) = registry.peek(&workspace_key).await {
            workspace_flushed = super::workspace_ops::persist_workspace(
                app,
                graph_id,
                &graph_dir,
                &room,
                &operation.operation_id,
            )
            .await
            .map_err(|error| {
                classify_flush_error(format!("crdt.flush workspace {graph_id}: {error}"))
            })?;
        }

        let prefix = format!("doc:{graph_id}:");
        let rooms = document_rooms(&registry, &prefix, operation.document_id.as_deref()).await;
        let mut documents_flushed = Vec::new();
        let mut processed_live_ids = std::collections::BTreeSet::new();
        for (document_id, room) in rooms {
            processed_live_ids.insert(document_id.clone());
            if crate::document_tombstone_store::document_is_tombstoned(&graph_dir, &document_id)
                .map_err(classify_flush_error)?
            {
                registry.evict_room(&format!("doc:{graph_id}:{document_id}"));
                continue;
            }
            if !room.needs_projection_flush() {
                continue;
            }
            let title = live_workspace_title(&registry, graph_id, &document_id)
                .await
                .or_else(|| stored_document_title(&graph_dir, &document_id))
                .unwrap_or_else(|| "Untitled".to_string());
            let persisted = super::document_ops::flush_room_document_if_dirty(
                app,
                graph_id,
                &document_id,
                &title,
                &room,
                &operation.operation_id,
            )
            .await
            .map_err(|error| {
                classify_flush_error(format!(
                    "crdt.flush document {graph_id}/{document_id}: {error}"
                ))
            })?;
            if persisted.is_some() {
                documents_flushed.push(document_id);
            }
        }

        // Recovered/explicit GRAPH-WIDE sweep over the cold persisted
        // sidecars, ONE room at a time: hydrate → flush → evict, so peak
        // registry residency is O(1 room) instead of O(all rooms). Rooms
        // that were already live took the ordinary loop above and are never
        // evicted here; a room this sweep creates is evicted whether its
        // flush succeeded or failed (an error path that retained the freshly
        // decoded room would rebuild the leak one retry at a time).
        if enumerate_graph_documents {
            for (document_id, state_path) in
                persisted_document_sidecars(graph_id, &graph_dir).map_err(classify_flush_error)?
            {
                if processed_live_ids.contains(&document_id) {
                    continue;
                }
                let room_key = format!("doc:{graph_id}:{document_id}");
                let room = registry
                    .get_or_create(&room_key, state_path)
                    .await
                    .map_err(classify_flush_error)?;
                let title = live_workspace_title(&registry, graph_id, &document_id)
                    .await
                    .or_else(|| stored_document_title(&graph_dir, &document_id))
                    .unwrap_or_else(|| "Untitled".to_string());
                let flush_result = super::document_ops::flush_room_document_if_dirty(
                    app,
                    graph_id,
                    &document_id,
                    &title,
                    &room,
                    &operation.operation_id,
                )
                .await;
                drop(room);
                registry.evict_room(&room_key);
                let persisted = flush_result.map_err(|error| {
                    classify_flush_error(format!(
                        "crdt.flush document {graph_id}/{document_id}: {error}"
                    ))
                })?;
                if persisted.is_some() {
                    documents_flushed.push(document_id);
                }
            }
        }

        Ok(json!({
            "flushed": true,
            "workspaceFlushed": workspace_flushed,
            "documentsFlushed": documents_flushed,
        }))
    }
    .await;

    // The named document room a doc-scoped recovery hydrated is evicted on
    // success AND error alike; rooms live before the op are untouched.
    if named_document_hydrated {
        if let Some(key) = &named_document_key {
            registry.evict_room(key);
        }
    }

    outcome
}

/// Startup recovery drains before the loopback server is exposed, so its room
/// registry is intentionally empty. Rehydrate the workspace room (one Y.Doc)
/// and, for a document-scoped recovered flush, that ONE document's hot
/// authority sidecar. Graph-wide document coverage deliberately does NOT
/// hydrate here anymore: `apply_classified` sweeps the cold sidecars from
/// [`persisted_document_sidecars`] one room at a time (hydrate → flush →
/// evict), because pre-hydrating every sidecar retained every document's
/// decoded rewrite history in the registry at once — the recovered-flush OOM.
async fn hydrate_workspace_and_named_document_room(
    registry: &RoomRegistry,
    graph_id: &str,
    graph_dir: &Path,
    document_id: Option<&str>,
) -> Result<(), String> {
    let workspace_path = crate::ydoc_paths::workspace_ydoc_state_path(graph_dir);
    if workspace_path.is_file() {
        registry
            .get_or_create(&format!("workspace:{graph_id}"), workspace_path)
            .await?;
    }

    if let Some(document_id) = document_id.filter(|id| !id.is_empty()) {
        // An explicit recovery/hydration request must fail closed. A stale
        // sidecar can survive an interrupted cleanup, but the durable delete
        // marker is authoritative and may not be replayed into a room.
        crate::document_tombstone_store::require_document_not_tombstoned(graph_dir, document_id)?;
        let state_path =
            crate::ydoc_paths::checked_document_ydoc_state_path(graph_dir, document_id)?;
        if state_path.is_file() {
            registry
                .get_or_create(&format!("doc:{graph_id}:{document_id}"), state_path)
                .await?;
        }
    }
    Ok(())
}

/// Enumerate the graph's persisted document sidecars for the graph-wide
/// recovered/explicit sweep: sorted for determinism, invalid/non-UTF8 ids
/// logged and skipped, tombstoned documents skipped (the durable delete
/// marker is authoritative and may not be replayed into a room). Pure
/// filesystem listing — decoding happens one room at a time in the caller.
fn persisted_document_sidecars(
    graph_id: &str,
    graph_dir: &Path,
) -> Result<Vec<(String, std::path::PathBuf)>, String> {
    let documents_root = graph_dir.join("ydocs/documents");
    if !documents_root.is_dir() {
        return Ok(Vec::new());
    }
    let entries = std::fs::read_dir(&documents_root)
        .map_err(|error| format!("read {}: {error}", documents_root.display()))?;
    let mut document_ids = Vec::new();
    for entry in entries {
        let entry =
            entry.map_err(|error| format!("read {} entry: {error}", documents_root.display()))?;
        if !entry
            .file_type()
            .map_err(|error| format!("read {} type: {error}", entry.path().display()))?
            .is_dir()
        {
            continue;
        }
        let Some(document_id) = entry.file_name().to_str().map(str::to_string) else {
            log::warn!(
                "ignoring non-UTF8 document sidecar directory under {}",
                documents_root.display()
            );
            continue;
        };
        if let Err(error) = crate::ids::validate_local_id(&document_id, "document_id") {
            log::warn!(
                "ignoring invalid document sidecar directory {}: {error}",
                entry.path().display()
            );
            continue;
        }
        if crate::document_tombstone_store::document_is_tombstoned(graph_dir, &document_id)? {
            log::warn!(
                "skipping tombstoned document sidecar during graph hydration: {graph_id}/{document_id}"
            );
            continue;
        }
        let state_path =
            crate::ydoc_paths::checked_document_ydoc_state_path(graph_dir, &document_id)?;
        if state_path.is_file() {
            document_ids.push((document_id, state_path));
        }
    }
    document_ids.sort_by(|(left, _), (right, _)| left.cmp(right));
    Ok(document_ids)
}

async fn document_rooms(
    registry: &RoomRegistry,
    prefix: &str,
    document_id: Option<&str>,
) -> Vec<(String, Arc<Room>)> {
    if let Some(document_id) = document_id.filter(|id| !id.is_empty()) {
        return registry
            .peek(&format!("{prefix}{document_id}"))
            .await
            .map(|room| vec![(document_id.to_string(), room)])
            .unwrap_or_default();
    }
    registry
        .rooms_with_prefix(prefix)
        .await
        .into_iter()
        .filter_map(|(key, room)| {
            key.strip_prefix(prefix)
                .filter(|document_id| !document_id.is_empty())
                .map(|document_id| (document_id.to_string(), room))
        })
        .collect()
}

/// Rebuild every workspace/document cold face from the persisted Y.Doc
/// authorities, even when no CRDT update is currently dirty.
///
/// Normal flushes quite correctly skip a room whose projection epoch was
/// already persisted. A source-sync projection wipe is different: it removes
/// the disposable face while leaving the Y.Doc untouched. Advancing only the
/// projection epoch makes the ordinary, heavily-tested persistence path do
/// the rebuild without fabricating a Yjs mutation.
pub(crate) async fn rebuild_all_ydoc_projections(
    app: &AppHandle,
    graph_id: &str,
    graph_dir: &Path,
    operation_id: &str,
) -> Result<Value, String> {
    let registry = app.state::<RoomRegistry>();
    // Hydrate the workspace room only. Graph-wide document hydration happens
    // ONE ROOM AT A TIME below — the same recovered-flush OOM fix
    // `apply_classified` applies (see `hydrate_workspace_and_named_document_room`'s
    // doc comment): pre-hydrating every persisted sidecar into the registry
    // at once retained every document's decoded rewrite history
    // simultaneously.
    hydrate_workspace_and_named_document_room(&registry, graph_id, graph_dir, None).await?;

    let mut workspace_rebuilt = false;
    if let Some(room) = registry.peek(&format!("workspace:{graph_id}")).await {
        room.force_projection_rebuild();
        workspace_rebuilt =
            super::workspace_ops::rebuild_workspace_projection(graph_id, graph_dir, &room).await?;
    }

    let prefix = format!("doc:{graph_id}:");
    let mut documents_rebuilt = Vec::new();
    let mut processed_live_ids = std::collections::BTreeSet::new();

    // Rooms already live in the registry before this call: rebuild in place,
    // never evicted here (mirrors `apply_classified`'s ordinary-loop /
    // graph-wide-sweep split below).
    for (document_id, room) in document_rooms(&registry, &prefix, None).await {
        processed_live_ids.insert(document_id.clone());
        if crate::document_tombstone_store::document_is_tombstoned(graph_dir, &document_id)? {
            registry.evict_room(&format!("{prefix}{document_id}"));
            continue;
        }
        room.force_projection_rebuild();
        let title = live_workspace_title(&registry, graph_id, &document_id)
            .await
            .or_else(|| stored_document_title(graph_dir, &document_id))
            .unwrap_or_else(|| "Untitled".to_string());
        if super::document_ops::rebuild_room_document_projection(
            app,
            graph_id,
            &document_id,
            &title,
            &room,
            operation_id,
        )
        .await?
        {
            documents_rebuilt.push(document_id);
        }
    }

    // GRAPH-WIDE sweep over the cold persisted sidecars this call didn't
    // already find live, ONE room at a time: hydrate -> rebuild -> evict, so
    // peak registry residency stays O(1 room) — the same discipline
    // `apply_classified`'s recovered/explicit sweep uses.
    // `persisted_document_sidecars` already excludes tombstoned documents.
    for (document_id, state_path) in persisted_document_sidecars(graph_id, graph_dir)? {
        if processed_live_ids.contains(&document_id) {
            continue;
        }
        let room_key = format!("{prefix}{document_id}");
        let room = registry.get_or_create(&room_key, state_path).await?;
        room.force_projection_rebuild();
        let title = live_workspace_title(&registry, graph_id, &document_id)
            .await
            .or_else(|| stored_document_title(graph_dir, &document_id))
            .unwrap_or_else(|| "Untitled".to_string());
        let rebuild_result = super::document_ops::rebuild_room_document_projection(
            app,
            graph_id,
            &document_id,
            &title,
            &room,
            operation_id,
        )
        .await;
        drop(room);
        registry.evict_room(&room_key);
        if rebuild_result? {
            documents_rebuilt.push(document_id);
        }
    }

    Ok(json!({
        "workspaceRebuilt": workspace_rebuilt,
        "documentsRebuilt": documents_rebuilt,
    }))
}

async fn live_workspace_title(
    registry: &RoomRegistry,
    graph_id: &str,
    document_id: &str,
) -> Option<String> {
    let room = registry.peek(&format!("workspace:{graph_id}")).await?;
    let document_id = document_id.to_string();
    room.with_doc(move |doc| super::workspace_ops::document_title_in_workspace(doc, &document_id))
        .await
}

fn stored_document_title(graph_dir: &Path, document_id: &str) -> Option<String> {
    super::workspace_ops::document_record_json(graph_dir, document_id)
        .and_then(|record| {
            record
                .get("title")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .filter(|title| !title.is_empty())
}

#[cfg(all(test, feature = "headless", not(feature = "desktop")))]
mod tests {
    use super::{
        apply, fail_next_flush_attempts_for_test, flush_attempts_for_test,
        hydrate_workspace_and_named_document_room, persisted_document_sidecars,
    };
    use crate::crdt_engine::rooms::RoomRegistry;
    use crate::crdt_queue::{CrdtOperation, CrdtOperationQueue};
    use crate::graph_service::{create_graph_service, CreateGraphInput};
    use oxigraph::sparql::{QueryResults, SparqlEvaluator};
    use serde_json::{json, Value};
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};
    #[cfg(feature = "desktop")]
    use tauri::Manager;
    use yrs::updates::decoder::Decode;
    use yrs::{Any, Doc, Map, MapPrelim, Out, ReadTxn, StateVector, Transact, Update, WriteTxn};

    fn temp_profile() -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        std::env::temp_dir().join(format!("garden-fid004-flush-{nanos}"))
    }

    fn full_update(doc: &Doc) -> Vec<u8> {
        doc.transact()
            .encode_state_as_update_v1(&StateVector::default())
    }

    fn workspace_update(document_id: &str, title: &str) -> Vec<u8> {
        let doc = Doc::new();
        {
            let mut txn = doc.transact_mut();
            let documents = txn.get_or_insert_map("documents");
            let entry = documents.insert(&mut txn, document_id, MapPrelim::default());
            entry.insert(&mut txn, "title", title);
            entry.insert(&mut txn, "parentId", Any::Null);
            entry.insert(&mut txn, "section", "documents");
            entry.insert(&mut txn, "order", 1.0);
            entry.insert(&mut txn, "createdAt", 1.0);
            entry.insert(&mut txn, "updatedAt", 1.0);
            entry.insert(&mut txn, "readOnly", false);
        }
        full_update(&doc)
    }

    fn document_update(block_id: &str, text: &str) -> Vec<u8> {
        let doc = crate::crdt_engine::builder::ydoc_from_tiptap_json(&json!({
            "type": "doc",
            "content": [{
                "type": "paragraph",
                "attrs": { "data-block-id": block_id },
                "content": [{ "type": "text", "text": text }],
            }],
        }));
        full_update(&doc)
    }

    #[test]
    fn automatic_flush_repairs_after_more_than_eight_failures_without_new_mutation() {
        let _serial = crate::tauri_runtime::profile_env_serial()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let profile = temp_profile();
        std::env::set_var("GARDEN_PROFILE_DIR", &profile);

        let result = std::panic::catch_unwind(|| {
            crate::app_runtime::async_runtime::block_on(async {
                let app = crate::tauri_runtime::build_mock_app_for_tests(true);
                let graph_id = "flush-beyond-eight";
                create_graph_service(
                    &app,
                    CreateGraphInput {
                        title: "Flush Beyond Eight".to_string(),
                        graph_id: Some(graph_id.to_string()),
                        description: None,
                        operation_id: None,
                    },
                )
                .expect("create graph");
                let graph_dir = crate::graph_paths::existing_graph_dir(&app, graph_id)
                    .expect("graph directory");
                let room = app
                    .state::<RoomRegistry>()
                    .get_or_create(
                        &format!("workspace:{graph_id}"),
                        crate::ydoc_paths::workspace_ydoc_state_path(&graph_dir),
                    )
                    .await
                    .expect("workspace room");
                room.configure_projection_flush(graph_id.to_string(), None)
                    .expect("configure automatic flush");
                room.update_doc(|_doc, txn| {
                    let folders = txn.get_or_insert_map("folders");
                    let folder = folders.insert(txn, "folder-retry", MapPrelim::default());
                    folder.insert(txn, "name", "Eventually Durable");
                    folder.insert(txn, "section", "documents");
                    folder.insert(txn, "parentId", Any::Null);
                    folder.insert(txn, "order", 1.0);
                    Ok(())
                })
                .await
                .expect("one hot mutation");
                assert!(room.needs_projection_flush());

                fail_next_flush_attempts_for_test(graph_id, 9);
                room.schedule_projection_flush(app.clone());
                tokio::time::timeout(std::time::Duration::from_secs(20), async {
                    loop {
                        if flush_attempts_for_test(graph_id) >= 10 && !room.needs_projection_flush()
                        {
                            break;
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
                    }
                })
                .await
                .expect("durable scheduled retry eventually repairs projection");

                assert_eq!(flush_attempts_for_test(graph_id), 10);
                let snapshot: Value = crate::storage::read_json(
                    &crate::ydoc_paths::workspace_snapshot_path(&graph_dir),
                )
                .expect("workspace projection");
                assert_eq!(snapshot["counts"]["folders"], 1);
                assert!(
                    !room.needs_projection_flush(),
                    "scheduled repair clears the original dirty epoch without another mutation"
                );
            });
        });

        std::env::remove_var("GARDEN_PROFILE_DIR");
        let _ = std::fs::remove_dir_all(&profile);
        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
    }

    #[test]
    fn explicit_hydration_rejects_tombstoned_sidecar_and_graph_scan_skips_it() {
        let _serial = crate::tauri_runtime::profile_env_serial()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let profile = temp_profile();
        std::env::set_var("GARDEN_PROFILE_DIR", &profile);

        let result = std::panic::catch_unwind(|| {
            crate::app_runtime::async_runtime::block_on(async {
                let app = crate::tauri_runtime::build_mock_app_for_tests(true);
                let graph_id = "flush-tombstoned-sidecar";
                let document_id = "deleted-sidecar";
                create_graph_service(
                    &app,
                    CreateGraphInput {
                        title: "Flush Tombstone".to_string(),
                        graph_id: Some(graph_id.to_string()),
                        description: None,
                        operation_id: None,
                    },
                )
                .expect("create graph");
                let graph_dir = crate::graph_paths::existing_graph_dir(&app, graph_id)
                    .expect("graph directory");
                let state_path =
                    crate::ydoc_paths::document_ydoc_state_path(&graph_dir, document_id);
                let stale_registry = RoomRegistry::default();
                let stale_room = stale_registry
                    .get_or_create(&format!("doc:{graph_id}:{document_id}"), state_path.clone())
                    .await
                    .expect("create stale sidecar room");
                stale_room
                    .update_doc(|_doc, txn| {
                        txn.get_or_insert_map("metadata")
                            .insert(txn, "deleted-sentinel", true);
                        Ok(())
                    })
                    .await
                    .expect("persist stale sidecar");
                stale_registry.evict_room(&format!("doc:{graph_id}:{document_id}"));
                crate::document_tombstone_store::write_document_tombstone_for_operation(
                    &graph_dir,
                    document_id,
                    Some("delete-before-explicit-hydration"),
                )
                .expect("write tombstone");

                let recovered_registry = RoomRegistry::default();
                let error = hydrate_workspace_and_named_document_room(
                    &recovered_registry,
                    graph_id,
                    &graph_dir,
                    Some(document_id),
                )
                .await
                .expect_err("explicit tombstoned sidecar hydration must fail closed");
                assert!(error.contains("document tombstoned"), "{error}");
                assert!(recovered_registry
                    .peek(&format!("doc:{graph_id}:{document_id}"))
                    .await
                    .is_none());

                // The graph-wide sweep listing (pure filesystem, no rooms)
                // must skip the tombstoned sidecar so the one-at-a-time
                // recovery sweep can never replay it into a room.
                let sidecars = persisted_document_sidecars(graph_id, &graph_dir)
                    .expect("graph-wide recovery listing skips tombstoned sidecar");
                assert!(
                    !sidecars.iter().any(|(id, _)| id == document_id),
                    "tombstoned sidecar must not be listed for the recovery sweep"
                );
                assert!(recovered_registry
                    .peek(&format!("doc:{graph_id}:{document_id}"))
                    .await
                    .is_none());
            });
        });

        std::env::remove_var("GARDEN_PROFILE_DIR");
        let _ = std::fs::remove_dir_all(&profile);
        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
    }

    #[test]
    fn flush_materializes_live_sync_rooms_for_stored_reads_search_and_rdf() {
        let _serial = crate::tauri_runtime::profile_env_serial()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let profile = temp_profile();
        std::env::set_var("GARDEN_PROFILE_DIR", &profile);

        let result = std::panic::catch_unwind(|| {
            crate::app_runtime::async_runtime::block_on(async {
                let app = crate::tauri_runtime::build_mock_app_for_tests(true);
                let graph_id = "fid004-live-projection";
                let document_id = "daily-note-2026-07-11";
                let title = "Daily Note — 2026-07-11";
                let sentinel = "A live websocket sentence for projection convergence.";
                create_graph_service(
                    &app,
                    CreateGraphInput {
                        title: "FID-004 Projection".to_string(),
                        graph_id: Some(graph_id.to_string()),
                        description: None,
                        operation_id: None,
                    },
                )
                .expect("create graph");
                let graph_dir = crate::graph_paths::existing_graph_dir(&app, graph_id)
                    .expect("existing graph dir");
                let registry = app.state::<RoomRegistry>();

                // Model the browser's workspace Y.Doc update arriving over
                // y-websocket, not an API mutation that cold-persists eagerly.
                let remote_workspace = Doc::new();
                {
                    let mut txn = remote_workspace.transact_mut();
                    let documents = txn.get_or_insert_map("documents");
                    let entry = documents.insert(&mut txn, document_id, MapPrelim::default());
                    entry.insert(&mut txn, "title", title);
                    entry.insert(&mut txn, "parentId", Any::Null);
                    entry.insert(&mut txn, "section", "documents");
                    entry.insert(&mut txn, "order", 1.0);
                    entry.insert(&mut txn, "createdAt", 1.0);
                    entry.insert(&mut txn, "updatedAt", 1.0);
                    entry.insert(&mut txn, "readOnly", false);
                }
                let workspace_room = registry
                    .get_or_create(
                        &format!("workspace:{graph_id}"),
                        crate::ydoc_paths::workspace_ydoc_state_path(&graph_dir),
                    )
                    .await
                    .expect("workspace room");
                assert!(workspace_room
                    .apply_client_update(&full_update(&remote_workspace))
                    .await
                    .expect("apply workspace sync update"));

                let remote_document = crate::crdt_engine::builder::ydoc_from_tiptap_json(&json!({
                    "type": "doc",
                    "content": [{
                        "type": "paragraph",
                        "attrs": { "data-block-id": "block-live-sync" },
                        "content": [{ "type": "text", "text": sentinel }],
                    }],
                }));
                let document_room = registry
                    .get_or_create(
                        &format!("doc:{graph_id}:{document_id}"),
                        crate::ydoc_paths::document_ydoc_state_path(&graph_dir, document_id),
                    )
                    .await
                    .expect("document room");
                assert!(document_room
                    .apply_client_update(&full_update(&remote_document))
                    .await
                    .expect("apply document sync update"));

                // The live room has the edit, while cold readers reproduce
                // FID-004 before crdt.flush.
                assert!(crate::document_service::read_document(
                    app.clone(),
                    graph_id.to_string(),
                    document_id.to_string(),
                )
                .is_err());
                let before_search = crate::mcp_search_service::mcp_local_search_documents(
                    app.clone(),
                    &json!({ "graphId": graph_id, "query": "Daily Note" }),
                )
                .expect("search before flush");
                assert_eq!(before_search["total"], 0);

                let outcome = apply(
                    &app,
                    &CrdtOperation {
                        operation_id: "fid004-flush-1".to_string(),
                        kind: "crdt.flush".to_string(),
                        graph_id: graph_id.to_string(),
                        document_id: None,
                        payload: json!({ "includeMaterialization": true }),
                        enqueue_timestamp: "1".to_string(),
                    },
                )
                .await
                .expect("flush live rooms");
                assert_eq!(outcome["flushed"], true);
                assert_eq!(outcome["workspaceFlushed"], true);
                assert_eq!(outcome["documentsFlushed"], json!([document_id]));

                let record = crate::document_service::read_document(
                    app.clone(),
                    graph_id.to_string(),
                    document_id.to_string(),
                )
                .expect("stored document after flush");
                assert_eq!(record.title, title);
                assert!(record.body.contains(sentinel), "body: {:?}", record.body);
                assert_eq!(record.blocks.len(), 1);
                assert_eq!(record.blocks[0].id, "block-live-sync");
                assert!(record.rdf_triple_count > 0);
                let projection_iri =
                    crate::rdf_authority::document_projection_graph_iri(graph_id, document_id);
                let workspace_projection_iri =
                    crate::rdf_authority::workspace_projection_graph_iri(graph_id);
                let rdf_store = crate::rdf_service::open_graph_store(&graph_dir)
                    .expect("open graph RDF after flush");
                let projected = SparqlEvaluator::new()
                    .parse_query(&format!(
                        "ASK WHERE {{ GRAPH <{projection_iri}> {{ ?s ?p ?o }} \
                         GRAPH <{workspace_projection_iri}> {{ ?ws ?wp ?wo }} }}"
                    ))
                    .expect("parse projection ASK")
                    .on_store(&rdf_store)
                    .execute()
                    .expect("query document projection");
                assert!(matches!(projected, QueryResults::Boolean(true)));
                drop(rdf_store);

                let document_search = crate::mcp_search_service::mcp_local_search_documents(
                    app.clone(),
                    &json!({ "graphId": graph_id, "query": "Daily Note" }),
                )
                .expect("document search after flush");
                assert_eq!(document_search["total"], 1);
                let block_search = crate::mcp_search_service::mcp_local_search_blocks(
                    app.clone(),
                    &json!({
                        "graphId": graph_id,
                        "query": "projection convergence",
                        "mode": "lexical",
                        "caseSensitive": false,
                    }),
                )
                .expect("block search after flush");
                assert_eq!(block_search["total"], 1);

                let workspace: Value = crate::storage::read_json(
                    &crate::ydoc_paths::workspace_snapshot_path(&graph_dir),
                )
                .expect("workspace snapshot after flush");
                assert_eq!(workspace["counts"]["documents"], 1);
                assert_eq!(workspace["documents"][0]["id"], document_id);

                // Flush is dirty-aware: an unchanged second call performs no
                // cold save and therefore cannot manufacture a revision.
                let revision = record.revision;
                let second = apply(
                    &app,
                    &CrdtOperation {
                        operation_id: "fid004-flush-2".to_string(),
                        kind: "crdt.flush".to_string(),
                        graph_id: graph_id.to_string(),
                        document_id: None,
                        payload: json!({}),
                        enqueue_timestamp: "2".to_string(),
                    },
                )
                .await
                .expect("idempotent second flush");
                assert_eq!(second["workspaceFlushed"], false);
                assert_eq!(second["documentsFlushed"], json!([]));
                let unchanged = crate::document_service::read_document(
                    app.clone(),
                    graph_id.to_string(),
                    document_id.to_string(),
                )
                .expect("stored document after second flush");
                assert_eq!(unchanged.revision, revision);

                // A document-scoped flush must not drain sibling rooms. This
                // mirrors ManagedCrdtChannel.flush(documentId) on desktop.
                for (id, text) in [
                    ("scoped-document", "Only this room should flush now."),
                    ("sibling-document", "This room should remain live-only."),
                ] {
                    let remote = crate::crdt_engine::builder::ydoc_from_tiptap_json(&json!({
                        "type": "doc",
                        "content": [{
                            "type": "paragraph",
                            "attrs": { "data-block-id": format!("block-{id}") },
                            "content": [{ "type": "text", "text": text }],
                        }],
                    }));
                    let room = registry
                        .get_or_create(
                            &format!("doc:{graph_id}:{id}"),
                            crate::ydoc_paths::document_ydoc_state_path(&graph_dir, id),
                        )
                        .await
                        .expect("scoped test room");
                    assert!(room
                        .apply_client_update(&full_update(&remote))
                        .await
                        .expect("apply scoped test update"));
                }
                let scoped = apply(
                    &app,
                    &CrdtOperation {
                        operation_id: "fid004-flush-scoped".to_string(),
                        kind: "crdt.flush".to_string(),
                        graph_id: graph_id.to_string(),
                        document_id: Some("scoped-document".to_string()),
                        payload: json!({}),
                        enqueue_timestamp: "3".to_string(),
                    },
                )
                .await
                .expect("document-scoped flush");
                assert_eq!(scoped["documentsFlushed"], json!(["scoped-document"]));
                assert!(crate::document_service::read_document(
                    app.clone(),
                    graph_id.to_string(),
                    "scoped-document".to_string(),
                )
                .is_ok());
                assert!(crate::document_service::read_document(
                    app.clone(),
                    graph_id.to_string(),
                    "sibling-document".to_string(),
                )
                .is_err());

                let remainder = apply(
                    &app,
                    &CrdtOperation {
                        operation_id: "fid004-flush-remainder".to_string(),
                        kind: "crdt.flush".to_string(),
                        graph_id: graph_id.to_string(),
                        document_id: None,
                        payload: json!({}),
                        enqueue_timestamp: "4".to_string(),
                    },
                )
                .await
                .expect("graph flush drains remaining room");
                assert_eq!(remainder["documentsFlushed"], json!(["sibling-document"]));
            });
        });

        std::env::remove_var("GARDEN_PROFILE_DIR");
        let _ = std::fs::remove_dir_all(&profile);
        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
    }

    #[test]
    fn automatic_flush_coalesces_client_burst_and_explicit_flush_stays_idempotent() {
        let _serial = crate::tauri_runtime::profile_env_serial()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let profile = temp_profile();
        std::env::set_var("GARDEN_PROFILE_DIR", &profile);

        let result = std::panic::catch_unwind(|| {
            crate::app_runtime::async_runtime::block_on(async {
                let app = crate::tauri_runtime::build_mock_app_for_tests(true);
                let graph_id = "fid004-auto-debounce";
                let document_id = "daily-note-auto";
                let title = "Automatically Flushed Daily Note";
                create_graph_service(
                    &app,
                    CreateGraphInput {
                        title: "FID-004 Auto Debounce".to_string(),
                        graph_id: Some(graph_id.to_string()),
                        description: None,
                        operation_id: None,
                    },
                )
                .expect("create graph");
                let graph_dir = crate::graph_paths::existing_graph_dir(&app, graph_id)
                    .expect("existing graph dir");
                let registry = app.state::<RoomRegistry>();

                let workspace_room = registry
                    .get_or_create(
                        &format!("workspace:{graph_id}"),
                        crate::ydoc_paths::workspace_ydoc_state_path(&graph_dir),
                    )
                    .await
                    .expect("workspace room");
                workspace_room
                    .configure_projection_flush(graph_id.to_string(), None)
                    .expect("configure workspace auto flush");
                assert!(workspace_room
                    .apply_client_update(&workspace_update(document_id, title))
                    .await
                    .expect("workspace websocket update"));
                workspace_room.schedule_projection_flush(app.clone());

                let document_room = registry
                    .get_or_create(
                        &format!("doc:{graph_id}:{document_id}"),
                        crate::ydoc_paths::document_ydoc_state_path(&graph_dir, document_id),
                    )
                    .await
                    .expect("document room");
                document_room
                    .configure_projection_flush(graph_id.to_string(), Some(document_id.to_string()))
                    .expect("configure document auto flush");

                // Three independent sync clients land inside one 600ms window.
                // Their Yjs structs merge, but only the last debounce generation
                // may enqueue the document cold flush.
                for index in 1..=3 {
                    assert!(document_room
                        .apply_client_update(&document_update(
                            &format!("auto-block-{index}"),
                            &format!("automatic websocket burst sentinel {index}"),
                        ))
                        .await
                        .expect("document websocket burst update"));
                    document_room.schedule_projection_flush(app.clone());
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                }
                assert!(
                    crate::document_service::read_document(
                        app.clone(),
                        graph_id.to_string(),
                        document_id.to_string(),
                    )
                    .is_err(),
                    "document cold projection ran before the 600ms debounce"
                );

                let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(4);
                let record = loop {
                    if let Ok(record) = crate::document_service::read_document(
                        app.clone(),
                        graph_id.to_string(),
                        document_id.to_string(),
                    ) {
                        break record;
                    }
                    assert!(
                        tokio::time::Instant::now() < deadline,
                        "automatic document projection flush timed out"
                    );
                    tokio::time::sleep(std::time::Duration::from_millis(25)).await;
                };
                assert_eq!(record.revision, 1, "burst must make one cold save");
                assert_eq!(record.title, title);
                assert_eq!(record.blocks.len(), 3);
                assert!(record.rdf_triple_count > 0);

                let workspace: Value = crate::storage::read_json(
                    &crate::ydoc_paths::workspace_snapshot_path(&graph_dir),
                )
                .expect("workspace auto projection");
                assert_eq!(workspace["documents"][0]["id"], document_id);
                let search = crate::mcp_search_service::mcp_local_search_blocks(
                    app.clone(),
                    &json!({
                        "graphId": graph_id,
                        "query": "websocket burst sentinel",
                        "mode": "lexical",
                        "caseSensitive": false,
                    }),
                )
                .expect("search automatic projection");
                assert_eq!(search["total"], 3);

                let queue = app.state::<CrdtOperationQueue>();
                let trace_deadline =
                    tokio::time::Instant::now() + std::time::Duration::from_secs(2);
                loop {
                    let traces = queue
                        .recent_traces(Some(20), false, Some("crdt.flush"), Some(document_id), None)
                        .expect("document flush traces");
                    if traces.len() == 1 {
                        break;
                    }
                    assert!(
                        traces.is_empty() && tokio::time::Instant::now() < trace_deadline,
                        "burst must enqueue exactly one document flush; traces={traces:?}"
                    );
                    tokio::time::sleep(std::time::Duration::from_millis(25)).await;
                }

                // Stay past a second full debounce window: no trailing task
                // may enqueue another document flush or bump the cold revision.
                tokio::time::sleep(std::time::Duration::from_millis(750)).await;
                let traces = queue
                    .recent_traces(Some(20), false, Some("crdt.flush"), Some(document_id), None)
                    .expect("settled document flush traces");
                assert_eq!(traces.len(), 1, "burst left a redundant flush task");
                let settled = crate::document_service::read_document(
                    app.clone(),
                    graph_id.to_string(),
                    document_id.to_string(),
                )
                .expect("settled automatic projection");
                assert_eq!(settled.revision, 1, "no trailing redundant save");

                crate::crdt_projection_flush::flush_document_projection(
                    app.clone(),
                    graph_id,
                    document_id,
                )
                .await
                .expect("explicit flush after automatic flush");
                let explicit = crate::document_service::read_document(
                    app.clone(),
                    graph_id.to_string(),
                    document_id.to_string(),
                )
                .expect("projection after explicit flush");
                assert_eq!(explicit.revision, 1, "explicit clean flush is idempotent");
            });
        });

        std::env::remove_var("GARDEN_PROFILE_DIR");
        let _ = std::fs::remove_dir_all(&profile);
        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
    }

    #[test]
    fn automatic_flush_stops_on_not_found_and_new_update_can_reschedule() {
        let _serial = crate::tauri_runtime::profile_env_serial()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let profile = temp_profile();
        std::env::set_var("GARDEN_PROFILE_DIR", &profile);

        let result = std::panic::catch_unwind(|| {
            crate::app_runtime::async_runtime::block_on(async {
                let app = crate::tauri_runtime::build_mock_app_for_tests(true);
                let graph_id = "fid004-auto-retry";
                let document_id = "retry-document";
                let graph_dir = profile.join("graphs").join(graph_id);
                let registry = app.state::<RoomRegistry>();
                let room = registry
                    .get_or_create(
                        &format!("workspace:{graph_id}"),
                        crate::ydoc_paths::workspace_ydoc_state_path(&graph_dir),
                    )
                    .await
                    .expect("pre-graph workspace room");
                room.configure_projection_flush(graph_id.to_string(), None)
                    .expect("configure retrying flush");
                assert!(room
                    .apply_client_update(&workspace_update(document_id, "Retry Document"))
                    .await
                    .expect("pre-graph websocket update"));
                room.schedule_projection_flush(app.clone());

                let queue = app.state::<CrdtOperationQueue>();
                tokio::time::sleep(std::time::Duration::from_millis(600)).await;
                let stopped = queue
                    .recent_traces(Some(20), false, Some("crdt.flush"), None, None)
                    .expect("stopped not-found traces")
                    .into_iter()
                    .filter(|trace| trace.get("graphId").and_then(Value::as_str) == Some(graph_id))
                    .collect::<Vec<_>>();
                assert!(
                    stopped.is_empty(),
                    "pre-enqueue not-found must not create a queue trace"
                );
                assert_eq!(
                    queue.counts_for_test().expect("not-found queue counts"),
                    (0, 0)
                );
                assert!(
                    crate::crdt_operation_journal::recover_pending_crdt_operations(&app)
                        .expect("not-found journal")
                        .is_empty(),
                    "pre-enqueue not-found must not journal an operation"
                );
                assert!(room.needs_projection_flush());

                create_graph_service(
                    &app,
                    CreateGraphInput {
                        title: "FID-004 Retry".to_string(),
                        graph_id: Some(graph_id.to_string()),
                        description: None,
                        operation_id: None,
                    },
                )
                .expect("create graph after terminal automatic flush failure");
                assert!(room
                    .apply_client_update(&workspace_update("retry-document-2", "Retry Document 2",))
                    .await
                    .expect("post-create websocket update"));
                room.schedule_projection_flush(app.clone());

                let snapshot_path = crate::ydoc_paths::workspace_snapshot_path(&graph_dir);
                let success_deadline =
                    tokio::time::Instant::now() + std::time::Duration::from_secs(4);
                while !snapshot_path.is_file() {
                    assert!(
                        tokio::time::Instant::now() < success_deadline,
                        "new post-create update never scheduled a fresh flush"
                    );
                    tokio::time::sleep(std::time::Duration::from_millis(25)).await;
                }
                let snapshot: Value =
                    crate::storage::read_json(&snapshot_path).expect("retried workspace snapshot");
                assert!(snapshot["documents"]
                    .as_array()
                    .expect("workspace documents")
                    .iter()
                    .any(|document| document["id"] == "retry-document-2"));
                let trace_deadline =
                    tokio::time::Instant::now() + std::time::Duration::from_secs(2);
                loop {
                    let succeeded = queue
                        .recent_traces(Some(20), false, Some("crdt.flush"), None, None)
                        .expect("completed retry traces")
                        .iter()
                        .any(|trace| {
                            trace.get("graphId").and_then(Value::as_str) == Some(graph_id)
                                && trace.get("ok").and_then(Value::as_bool) == Some(true)
                        });
                    if succeeded {
                        break;
                    }
                    assert!(
                        tokio::time::Instant::now() < trace_deadline,
                        "retried projection landed without a successful completion trace"
                    );
                    tokio::time::sleep(std::time::Duration::from_millis(25)).await;
                }
            });
        });

        std::env::remove_var("GARDEN_PROFILE_DIR");
        let _ = std::fs::remove_dir_all(&profile);
        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
    }

    #[test]
    fn graph_deletion_evicts_room_and_cancels_pending_automatic_flush() {
        let _serial = crate::tauri_runtime::profile_env_serial()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let profile = temp_profile();
        std::env::set_var("GARDEN_PROFILE_DIR", &profile);

        let result = std::panic::catch_unwind(|| {
            crate::app_runtime::async_runtime::block_on(async {
                let app = crate::tauri_runtime::build_mock_app_for_tests(true);
                let graph_id = "fid004-auto-teardown";
                create_graph_service(
                    &app,
                    CreateGraphInput {
                        title: "FID-004 Teardown".to_string(),
                        graph_id: Some(graph_id.to_string()),
                        description: None,
                        operation_id: None,
                    },
                )
                .expect("create graph");
                let graph_dir = crate::graph_paths::existing_graph_dir(&app, graph_id)
                    .expect("existing graph dir");

                // Exercise the production topology: AppHandle owns this
                // registry, and eviction (not dropping a standalone registry)
                // must cancel the pending generation without an AppHandle cycle.
                let registry = app.state::<RoomRegistry>();
                let room = registry
                    .get_or_create(
                        &format!("workspace:{graph_id}"),
                        crate::ydoc_paths::workspace_ydoc_state_path(&graph_dir),
                    )
                    .await
                    .expect("temporary room");
                room.configure_projection_flush(graph_id.to_string(), None)
                    .expect("configure temporary room");
                assert!(room
                    .apply_client_update(&workspace_update("torn-down", "Torn Down"))
                    .await
                    .expect("temporary websocket update"));
                room.schedule_projection_flush(app.clone());
                crate::graph_service::soft_delete_graph_service(&app, graph_id.to_string(), false)
                    .expect("delete graph");
                assert!(registry
                    .peek(&format!("workspace:{graph_id}"))
                    .await
                    .is_none());
                assert_eq!(
                    room.apply_client_update(&workspace_update("late", "Late update"))
                        .await
                        .expect_err("evicted websocket room rejects updates"),
                    "room was evicted"
                );
                drop(room);

                tokio::time::sleep(std::time::Duration::from_millis(400)).await;
                let queue = app.state::<CrdtOperationQueue>();
                let traces = queue
                    .recent_traces(Some(20), false, Some("crdt.flush"), None, None)
                    .expect("teardown traces");
                assert!(
                    traces.iter().all(|trace| {
                        trace.get("graphId").and_then(Value::as_str) != Some(graph_id)
                    }),
                    "a torn-down room left a zombie automatic flush"
                );
            });
        });

        std::env::remove_var("GARDEN_PROFILE_DIR");
        let _ = std::fs::remove_dir_all(&profile);
        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
    }

    #[test]
    fn stale_automatic_flush_generation_is_rejected_after_graph_id_recreation() {
        let _serial = crate::tauri_runtime::profile_env_serial()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let profile = temp_profile();
        std::env::set_var("GARDEN_PROFILE_DIR", &profile);

        let result = std::panic::catch_unwind(|| {
            crate::app_runtime::async_runtime::block_on(async {
                let app = crate::tauri_runtime::build_mock_app_for_tests(true);
                let graph_id = "fid004-generation-recreate";
                create_graph_service(
                    &app,
                    CreateGraphInput {
                        title: "Generation One".to_string(),
                        graph_id: Some(graph_id.to_string()),
                        description: None,
                        operation_id: None,
                    },
                )
                .expect("create first graph generation");
                let coordinator = app.state::<
                    crate::crdt_engine::persistence_coordinator::GraphPersistenceCoordinator,
                >();
                let old_generation = coordinator.generation(graph_id).expect("old generation");
                crate::graph_service::soft_delete_graph_service(&app, graph_id.to_string(), false)
                    .expect("delete first graph generation");
                let old_dir = profile.join("graphs").join(graph_id);
                std::fs::remove_dir_all(&old_dir).expect("remove deleted graph generation");
                create_graph_service(
                    &app,
                    CreateGraphInput {
                        title: "Generation Two".to_string(),
                        graph_id: Some(graph_id.to_string()),
                        description: None,
                        operation_id: None,
                    },
                )
                .expect("recreate graph id");

                let error = apply(
                    &app,
                    &CrdtOperation {
                        operation_id: "stale-generation-flush".to_string(),
                        kind: "crdt.flush".to_string(),
                        graph_id: graph_id.to_string(),
                        document_id: None,
                        payload: json!({
                            "includeMaterialization": true,
                            "graphGeneration": old_generation,
                        }),
                        enqueue_timestamp: "1".to_string(),
                    },
                )
                .await
                .expect_err("stale generation must be rejected");
                assert!(error.contains("stale graph generation"), "{error}");
                assert_eq!(
                    crate::graph_service::read_graph_record(&app, graph_id)
                        .expect("recreated graph record")
                        .1
                        .title,
                    "Generation Two"
                );
            });
        });

        std::env::remove_var("GARDEN_PROFILE_DIR");
        let _ = std::fs::remove_dir_all(&profile);
        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
    }

    /// Regression for the fresh-cell OOM: a hydrated workspace room is seeded
    /// conservatively dirty, so the first client attach triggers a workspace
    /// projection flush whose title sweep used to `get_or_create` (fully
    /// decode and retain) EVERY document whose cold-manifest title trailed the
    /// workspace snapshot. The sweep must reconcile cold manifests without
    /// hydrating a single document room; only already-live rooms may take the
    /// room projection path.
    #[test]
    fn first_flush_title_sweep_reconciles_cold_manifests_without_hydrating_rooms() {
        let _serial = crate::tauri_runtime::profile_env_serial()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let profile = temp_profile();
        std::env::set_var("GARDEN_PROFILE_DIR", &profile);

        let result = std::panic::catch_unwind(|| {
            crate::app_runtime::async_runtime::block_on(async {
                let app = crate::tauri_runtime::build_mock_app_for_tests(true);
                let graph_id = "title-sweep-oom";
                const DOC_COUNT: usize = 40;
                let doc_id = |index: usize| format!("sweep-doc-{index:03}");
                let first_title = |index: usize| format!("First Title {index:03}");
                let renamed_title = |index: usize| format!("Renamed Title {index:03}");
                let doc_prefix = format!("doc:{graph_id}:");
                create_graph_service(
                    &app,
                    CreateGraphInput {
                        title: "Title Sweep OOM".to_string(),
                        graph_id: Some(graph_id.to_string()),
                        description: None,
                        operation_id: None,
                    },
                )
                .expect("create graph");
                let graph_dir = crate::graph_paths::existing_graph_dir(&app, graph_id)
                    .expect("existing graph dir");
                let registry = app.state::<RoomRegistry>();

                // A browser-shaped workspace update declaring every document.
                let remote_workspace = Doc::new();
                {
                    let mut txn = remote_workspace.transact_mut();
                    let documents = txn.get_or_insert_map("documents");
                    for index in 0..DOC_COUNT {
                        let entry = documents.insert(&mut txn, doc_id(index), MapPrelim::default());
                        entry.insert(&mut txn, "title", first_title(index));
                        entry.insert(&mut txn, "parentId", Any::Null);
                        entry.insert(&mut txn, "section", "documents");
                        entry.insert(&mut txn, "order", (index + 1) as f64);
                        entry.insert(&mut txn, "createdAt", 1.0);
                        entry.insert(&mut txn, "updatedAt", 1.0);
                        entry.insert(&mut txn, "readOnly", false);
                    }
                }
                let workspace_room = registry
                    .get_or_create(
                        &format!("workspace:{graph_id}"),
                        crate::ydoc_paths::workspace_ydoc_state_path(&graph_dir),
                    )
                    .await
                    .expect("workspace room");
                assert!(workspace_room
                    .apply_client_update(&full_update(&remote_workspace))
                    .await
                    .expect("apply workspace update"));

                // Give every document a non-trivial rewrite history through
                // the real y-websocket write path: three full-state client
                // updates whose merged tombstones a hydration must decode.
                for index in 0..DOC_COUNT {
                    let id = doc_id(index);
                    let room = registry
                        .get_or_create(
                            &format!("doc:{graph_id}:{id}"),
                            crate::ydoc_paths::document_ydoc_state_path(&graph_dir, &id),
                        )
                        .await
                        .expect("document room");
                    for rewrite in 0..3 {
                        assert!(room
                            .apply_client_update(&document_update(
                                &format!("block-{id}-{rewrite}"),
                                &format!(
                                    "Rewrite {rewrite} of {id}: a full sentence so the \
                                     merged update history carries real weight."
                                ),
                            ))
                            .await
                            .expect("apply document rewrite"));
                    }
                }

                // First flush persists cold records/projections for everything
                // under the first titles.
                let outcome = apply(
                    &app,
                    &CrdtOperation {
                        operation_id: "sweep-flush-initial".to_string(),
                        kind: "crdt.flush".to_string(),
                        graph_id: graph_id.to_string(),
                        document_id: None,
                        payload: json!({ "includeMaterialization": true }),
                        enqueue_timestamp: "1".to_string(),
                    },
                )
                .await
                .expect("initial graph flush");
                assert_eq!(outcome["workspaceFlushed"], true);
                assert_eq!(
                    outcome["documentsFlushed"]
                        .as_array()
                        .expect("documents flushed")
                        .len(),
                    DOC_COUNT
                );
                for index in 0..DOC_COUNT {
                    assert_eq!(
                        super::stored_document_title(&graph_dir, &doc_id(index)).as_deref(),
                        Some(first_title(index).as_str())
                    );
                }

                // Rename every document through the workspace channel. The hot
                // authority sidecar persists synchronously, but no projection
                // flush runs: cold snapshot + records keep the first titles.
                let rename_remote = Doc::new();
                {
                    let state = workspace_room.encode_state_for_test().await;
                    let mut txn = rename_remote.transact_mut();
                    txn.apply_update(Update::decode_v1(&state).expect("decode workspace state"))
                        .expect("sync rename replica");
                    let documents = txn.get_or_insert_map("documents");
                    for index in 0..DOC_COUNT {
                        let Some(Out::YMap(entry)) = documents.get(&txn, &doc_id(index)) else {
                            panic!("workspace entry missing for {}", doc_id(index));
                        };
                        entry.insert(&mut txn, "title", renamed_title(index));
                    }
                }
                assert!(workspace_room
                    .apply_client_update(&full_update(&rename_remote))
                    .await
                    .expect("apply rename update"));

                // Cell restart: every room evicted, durable sidecars remain.
                registry.evict_graph(graph_id);
                assert!(registry.rooms_with_prefix(&doc_prefix).await.is_empty());

                // Fresh attach re-hydrates the workspace room conservatively
                // dirty (projection_epoch seeded from `hydrated`) — the exact
                // precondition of the OOM.
                let workspace_room = registry
                    .get_or_create(
                        &format!("workspace:{graph_id}"),
                        crate::ydoc_paths::workspace_ydoc_state_path(&graph_dir),
                    )
                    .await
                    .expect("rehydrated workspace room");
                assert!(
                    workspace_room.needs_projection_flush(),
                    "hydrated workspace room must be conservatively dirty"
                );

                // Exactly one document room is genuinely live across the
                // flush; every other room stays cold.
                let live_id = doc_id(0);
                let live_room = registry
                    .get_or_create(
                        &format!("doc:{graph_id}:{live_id}"),
                        crate::ydoc_paths::document_ydoc_state_path(&graph_dir, &live_id),
                    )
                    .await
                    .expect("live document room");
                assert_eq!(registry.rooms_with_prefix(&doc_prefix).await.len(), 1);

                let cold_probe_id = doc_id(25);
                let cold_sidecar_path =
                    crate::ydoc_paths::document_ydoc_state_path(&graph_dir, &cold_probe_id);
                let cold_sidecar_before =
                    std::fs::read(&cold_sidecar_path).expect("cold sidecar before sweep");

                // The first post-attach workspace projection flush runs the
                // title sweep over all 40 stale manifests.
                let outcome = apply(
                    &app,
                    &CrdtOperation {
                        operation_id: "sweep-flush-hydrated".to_string(),
                        kind: "crdt.flush".to_string(),
                        graph_id: graph_id.to_string(),
                        document_id: None,
                        payload: json!({ "includeMaterialization": true }),
                        enqueue_timestamp: "2".to_string(),
                    },
                )
                .await
                .expect("hydrated-dirty graph flush");
                assert_eq!(outcome["workspaceFlushed"], true);
                // The sweep syncs the live room under its own projection gate,
                // so the subsequent document loop finds it clean.
                assert_eq!(outcome["documentsFlushed"], json!([]));

                // (b) The sweep must not mass-hydrate: registry holds exactly
                // the rooms that were live before the flush.
                let live_after: Vec<String> = registry
                    .rooms_with_prefix(&doc_prefix)
                    .await
                    .into_iter()
                    .map(|(key, _)| key)
                    .collect();
                assert_eq!(
                    live_after,
                    vec![format!("doc:{graph_id}:{live_id}")],
                    "title sweep must not hydrate cold document rooms"
                );

                // (a) Titles reconciled: workspace snapshot and every cold
                // manifest agree on the renamed titles; the live room's record
                // reconciled through the room projection path as before.
                let snapshot: Value = crate::storage::read_json(
                    &crate::ydoc_paths::workspace_snapshot_path(&graph_dir),
                )
                .expect("workspace snapshot after sweep");
                let snapshot_documents = snapshot["documents"]
                    .as_array()
                    .expect("snapshot documents");
                let snapshot_titles: std::collections::BTreeMap<String, String> =
                    snapshot_documents
                        .iter()
                        .map(|document| {
                            (
                                document["id"].as_str().expect("snapshot id").to_string(),
                                document["title"]
                                    .as_str()
                                    .expect("snapshot title")
                                    .to_string(),
                            )
                        })
                        .collect();
                for index in 0..DOC_COUNT {
                    let id = doc_id(index);
                    assert_eq!(
                        snapshot_titles.get(&id).map(String::as_str),
                        Some(renamed_title(index).as_str()),
                        "snapshot title for {id}"
                    );
                    assert_eq!(
                        super::stored_document_title(&graph_dir, &id).as_deref(),
                        Some(renamed_title(index).as_str()),
                        "cold manifest title for {id}"
                    );
                }
                assert!(
                    !live_room.needs_projection_flush(),
                    "live room reconciles through its projection gate as before"
                );
                let live_record = crate::document_service::read_document(
                    app.clone(),
                    graph_id.to_string(),
                    live_id.clone(),
                )
                .expect("live document record after sweep");
                assert_eq!(live_record.title, renamed_title(0));
                assert!(
                    live_record.body.contains("Rewrite 2"),
                    "{}",
                    live_record.body
                );

                // The cold path must round-trip the authoritative sidecar
                // byte-identically — reconciling a title never rewrites CRDT
                // history.
                let cold_sidecar_after =
                    std::fs::read(&cold_sidecar_path).expect("cold sidecar after sweep");
                assert_eq!(
                    cold_sidecar_before, cold_sidecar_after,
                    "cold title sync must not alter the document sidecar"
                );

                // Lazy convergence: a previously-cold room opened after the
                // sweep flushes under the reconciled title (the document Y.Doc
                // holds no title; its flush pulls the live workspace title).
                let lazy_id = doc_id(17);
                let lazy_room = registry
                    .get_or_create(
                        &format!("doc:{graph_id}:{lazy_id}"),
                        crate::ydoc_paths::document_ydoc_state_path(&graph_dir, &lazy_id),
                    )
                    .await
                    .expect("lazily opened document room");
                assert!(lazy_room
                    .apply_client_update(&document_update(
                        "block-lazy-edit",
                        "Post-sweep lazy edit sentinel.",
                    ))
                    .await
                    .expect("lazy room edit"));
                let outcome = apply(
                    &app,
                    &CrdtOperation {
                        operation_id: "sweep-flush-lazy".to_string(),
                        kind: "crdt.flush".to_string(),
                        graph_id: graph_id.to_string(),
                        document_id: Some(lazy_id.clone()),
                        payload: json!({ "includeMaterialization": true }),
                        enqueue_timestamp: "3".to_string(),
                    },
                )
                .await
                .expect("lazy document flush");
                assert_eq!(outcome["documentsFlushed"], json!([lazy_id.clone()]));
                let lazy_record = crate::document_service::read_document(
                    app.clone(),
                    graph_id.to_string(),
                    lazy_id.clone(),
                )
                .expect("lazy document record");
                assert_eq!(lazy_record.title, renamed_title(17));
                assert!(
                    lazy_record.body.contains("Post-sweep lazy edit sentinel."),
                    "{}",
                    lazy_record.body
                );
            });
        });

        std::env::remove_var("GARDEN_PROFILE_DIR");
        let _ = std::fs::remove_dir_all(&profile);
        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
    }

    /// Regression for the recovered-flush OOM (the fresh-pod death loop): a
    /// graph-wide `crdt.flush` replayed by startup recovery used to
    /// pre-hydrate EVERY persisted document sidecar into the registry — fully
    /// decoding every document's rewrite history at once and retaining all of
    /// it forever. The sweep must now (a) still converge every cold
    /// projection, including a sidecar whose hot state ran ahead of its cold
    /// record before the "crash", while (b) retaining ONLY the DOCUMENT rooms
    /// that were live before the flush — freshly hydrated document rooms are
    /// evicted when their flush completes. The ONE workspace room the op
    /// hydrates deliberately STAYS live (O(1) residency, the pre-existing
    /// recovered-flush contract also pinned by persistence_tests::
    /// recovered_automatic_workspace_flush_does_not_hydrate_unrelated_documents).
    /// A document-scoped recovered flush follows the same discipline for its
    /// named room. Real engine, real rooms, real rewrite histories, real
    /// registry residency via `rooms_with_prefix` — no mocks.
    #[test]
    fn recovered_graph_flush_sweeps_cold_rooms_one_at_a_time_and_retains_only_prior_live() {
        let _serial = crate::tauri_runtime::profile_env_serial()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let profile = temp_profile();
        std::env::set_var("GARDEN_PROFILE_DIR", &profile);

        let result = std::panic::catch_unwind(|| {
            crate::app_runtime::async_runtime::block_on(async {
                let app = crate::tauri_runtime::build_mock_app_for_tests(true);
                let graph_id = "recovered-sweep-oom";
                const DOC_COUNT: usize = 12;
                let doc_id = |index: usize| format!("sweep-doc-{index:03}");
                let doc_title = |index: usize| format!("Recovered Title {index:03}");
                let doc_prefix = format!("doc:{graph_id}:");
                let workspace_key = format!("workspace:{graph_id}");
                create_graph_service(
                    &app,
                    CreateGraphInput {
                        title: "Recovered Sweep OOM".to_string(),
                        graph_id: Some(graph_id.to_string()),
                        description: None,
                        operation_id: None,
                    },
                )
                .expect("create graph");
                let graph_dir = crate::graph_paths::existing_graph_dir(&app, graph_id)
                    .expect("existing graph dir");
                let registry = app.state::<RoomRegistry>();

                // Workspace entries + per-document rooms with real rewrite
                // histories through the real y-websocket write path.
                let remote_workspace = Doc::new();
                {
                    let mut txn = remote_workspace.transact_mut();
                    let documents = txn.get_or_insert_map("documents");
                    for index in 0..DOC_COUNT {
                        let entry = documents.insert(&mut txn, doc_id(index), MapPrelim::default());
                        entry.insert(&mut txn, "title", doc_title(index));
                        entry.insert(&mut txn, "parentId", Any::Null);
                        entry.insert(&mut txn, "section", "documents");
                        entry.insert(&mut txn, "order", (index + 1) as f64);
                        entry.insert(&mut txn, "createdAt", 1.0);
                        entry.insert(&mut txn, "updatedAt", 1.0);
                        entry.insert(&mut txn, "readOnly", false);
                    }
                }
                let workspace_room = registry
                    .get_or_create(
                        &workspace_key,
                        crate::ydoc_paths::workspace_ydoc_state_path(&graph_dir),
                    )
                    .await
                    .expect("workspace room");
                assert!(workspace_room
                    .apply_client_update(&full_update(&remote_workspace))
                    .await
                    .expect("apply workspace update"));
                for index in 0..DOC_COUNT {
                    let id = doc_id(index);
                    let room = registry
                        .get_or_create(
                            &format!("doc:{graph_id}:{id}"),
                            crate::ydoc_paths::document_ydoc_state_path(&graph_dir, &id),
                        )
                        .await
                        .expect("document room");
                    for rewrite in 0..3 {
                        assert!(room
                            .apply_client_update(&document_update(
                                &format!("block-{id}-{rewrite}"),
                                &format!("Rewrite {rewrite} of {id} with real history weight."),
                            ))
                            .await
                            .expect("apply document rewrite"));
                    }
                }

                // First flush cold-persists everything.
                let outcome = apply(
                    &app,
                    &CrdtOperation {
                        operation_id: "recovered-sweep-initial".to_string(),
                        kind: "crdt.flush".to_string(),
                        graph_id: graph_id.to_string(),
                        document_id: None,
                        payload: json!({ "includeMaterialization": true }),
                        enqueue_timestamp: "1".to_string(),
                    },
                )
                .await
                .expect("initial graph flush");
                assert_eq!(
                    outcome["documentsFlushed"]
                        .as_array()
                        .expect("documents flushed")
                        .len(),
                    DOC_COUNT
                );

                // One document's HOT sidecar runs ahead of its cold record
                // (the room persists client updates synchronously), then the
                // "crash": every room evicted, sidecars durable.
                let ahead_id = doc_id(7);
                let ahead_room = registry
                    .get_or_create(
                        &format!("doc:{graph_id}:{ahead_id}"),
                        crate::ydoc_paths::document_ydoc_state_path(&graph_dir, &ahead_id),
                    )
                    .await
                    .expect("hot-ahead room");
                assert!(ahead_room
                    .apply_client_update(&document_update(
                        "block-hot-ahead",
                        "Pre-crash hot-ahead sentinel.",
                    ))
                    .await
                    .expect("apply hot-ahead edit"));
                registry.evict_graph(graph_id);
                assert!(registry.rooms_with_prefix(&doc_prefix).await.is_empty());
                assert!(registry.peek(&workspace_key).await.is_none());

                // Exactly one document room is genuinely live (and dirty)
                // across the recovered flush; the workspace room is NOT.
                let live_id = doc_id(0);
                let live_key = format!("doc:{graph_id}:{live_id}");
                let live_room = registry
                    .get_or_create(
                        &live_key,
                        crate::ydoc_paths::document_ydoc_state_path(&graph_dir, &live_id),
                    )
                    .await
                    .expect("live document room");

                // Startup recovery replays the graph-wide flush: recovered
                // provenance, no graphGeneration → the graph-wide sweep.
                let outcome = apply(
                    &app,
                    &CrdtOperation {
                        operation_id: "recovered-sweep-replay".to_string(),
                        kind: "crdt.flush".to_string(),
                        graph_id: graph_id.to_string(),
                        document_id: None,
                        payload: json!({
                            "includeMaterialization": true,
                            (crate::crdt_queue::RECOVERED_OPERATION_PAYLOAD_KEY): true,
                        }),
                        enqueue_timestamp: "2".to_string(),
                    },
                )
                .await
                .expect("recovered graph-wide flush");
                assert_eq!(outcome["flushed"], true);
                assert_eq!(
                    outcome["workspaceFlushed"], true,
                    "the hydrated workspace room is conservatively dirty and flushes"
                );
                let flushed: std::collections::BTreeSet<String> = outcome["documentsFlushed"]
                    .as_array()
                    .expect("documents flushed")
                    .iter()
                    .map(|value| value.as_str().expect("document id").to_string())
                    .collect();
                let expected: std::collections::BTreeSet<String> =
                    (0..DOC_COUNT).map(doc_id).collect();
                assert_eq!(
                    flushed, expected,
                    "the sweep converges every persisted document (live + cold)"
                );

                // (b) THE RESIDENCY INVARIANT: only the pre-live room remains.
                let live_after: Vec<String> = registry
                    .rooms_with_prefix(&doc_prefix)
                    .await
                    .into_iter()
                    .map(|(key, _)| key)
                    .collect();
                assert_eq!(
                    live_after,
                    vec![live_key.clone()],
                    "the recovered sweep must retain ONLY rooms that were live before it"
                );
                assert!(
                    registry.peek(&workspace_key).await.is_some(),
                    "the ONE workspace room stays live after a recovered flush — \
                     O(1) residency, the pre-existing contract"
                );
                live_room
                    .update_doc(|_doc, _txn| Ok(()))
                    .await
                    .expect("the pre-live room is untouched by the sweep's evictions");

                // (a) The sweep genuinely flushed the hot-ahead sidecar into
                // its cold record — recovery converged real content, not just
                // bookkeeping.
                let ahead_record = crate::document_service::read_document(
                    app.clone(),
                    graph_id.to_string(),
                    ahead_id.clone(),
                )
                .expect("hot-ahead document record after sweep");
                assert!(
                    ahead_record.body.contains("Pre-crash hot-ahead sentinel."),
                    "{}",
                    ahead_record.body
                );

                // A document-scoped recovered flush follows the same
                // discipline: its named room is hydrated, flushed, and NOT
                // left behind.
                let scoped_id = doc_id(3);
                let scoped_key = format!("doc:{graph_id}:{scoped_id}");
                let outcome = apply(
                    &app,
                    &CrdtOperation {
                        operation_id: "recovered-doc-scoped".to_string(),
                        kind: "crdt.flush".to_string(),
                        graph_id: graph_id.to_string(),
                        document_id: Some(scoped_id.clone()),
                        payload: json!({
                            "includeMaterialization": true,
                            (crate::crdt_queue::RECOVERED_OPERATION_PAYLOAD_KEY): true,
                        }),
                        enqueue_timestamp: "3".to_string(),
                    },
                )
                .await
                .expect("recovered document-scoped flush");
                assert_eq!(outcome["documentsFlushed"], json!([scoped_id.clone()]));
                assert!(
                    registry.peek(&scoped_key).await.is_none(),
                    "a document-scoped recovery must not retain the room it hydrated"
                );
            });
        });

        std::env::remove_var("GARDEN_PROFILE_DIR");
        let _ = std::fs::remove_dir_all(&profile);
        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
    }
}
