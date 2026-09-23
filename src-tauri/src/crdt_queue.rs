use crate::app_runtime::AppHandle;
use crate::{
    app_error::AppErrorKind,
    clock::{duration_ms, timestamp},
    crdt_operation_audit::audit_crdt_operation,
    crdt_operation_journal::{
        record_crdt_operation_caller_timeout, record_crdt_operation_completion,
        record_crdt_operation_queued, recover_pending_crdt_operations,
    },
    restore_guard::require_no_active_restore,
};
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
#[cfg(feature = "desktop")]
use tauri::{Emitter, Manager};
use uuid::Uuid;

pub(crate) use crate::crdt_operation_queue::CrdtOperationQueue;
pub(crate) use crate::crdt_operation_types::{
    CompleteCrdtOperationInput, CrdtOperation, EnqueueCrdtOperationInput, EnqueuedCrdtOutcome,
};

const LOCAL_CRDT_OPERATION_TIMEOUT_SECS: u64 = 600;
const LOCAL_CRDT_OPERATION_TIMEOUT_ENV: &str = "GARDEN_LOCAL_CRDT_OPERATION_TIMEOUT_SECS";
pub(crate) const GRAPH_INCARNATION_PAYLOAD_KEY: &str = "graphIncarnation";
/// Returned when the CALLER's wait expires while the operation stays in the
/// durable queue. The operation is not dead — its background retries still
/// own their inputs (e.g. staged pending-upload archives), so callers must
/// NOT treat this error as license to clean those inputs up.
pub(crate) const CRDT_OPERATION_STILL_QUEUED_ERROR: &str = "CRDT operation timed out waiting for the desktop runtime; operation remains durably queued for local retry";
/// In-memory-only provenance added while reconstructing selected recovery-aware
/// operations. It is stripped from fresh enqueue input and never appended as a
/// new queued journal event. Flushes use it to hydrate an empty registry;
/// destructive workspace operations use it to finish an already-applied hot
/// mutation without weakening fresh not-found behavior.
pub(crate) const RECOVERED_OPERATION_PAYLOAD_KEY: &str = "__gardenRecoveredOperation";

#[cfg(not(feature = "frontend-crdt"))]
fn operation_kind_uses_recovery_provenance(kind: &str) -> bool {
    matches!(
        kind,
        "crdt.flush"
            | "workspace.deleteFolder"
            | "workspace.deleteArtifact"
            | "workspace.deleteWire"
    )
}

fn local_crdt_operation_timeout_secs() -> u64 {
    std::env::var(LOCAL_CRDT_OPERATION_TIMEOUT_ENV)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|seconds| *seconds >= LOCAL_CRDT_OPERATION_TIMEOUT_SECS)
        .unwrap_or(LOCAL_CRDT_OPERATION_TIMEOUT_SECS)
}

#[cfg_attr(feature = "desktop", tauri::command)]
pub(crate) fn poll_crdt_operations(
    queue: crate::app_runtime::State<'_, CrdtOperationQueue>,
    limit: Option<usize>,
) -> Result<Vec<CrdtOperation>, String> {
    queue.poll(limit)
}

#[cfg_attr(feature = "desktop", tauri::command)]
pub(crate) fn complete_crdt_operation(
    app: AppHandle,
    queue: crate::app_runtime::State<'_, CrdtOperationQueue>,
    input: CompleteCrdtOperationInput,
) -> Result<(), String> {
    #[cfg(not(feature = "desktop"))]
    let operation_id = input.operation_id.clone();
    if let Err(error) = record_crdt_operation_completion(&app, &input) {
        log::error!(
            "failed to append CRDT operation completion for {}: {}",
            input.operation_id,
            error
        );
    }
    let result = queue.complete(input);
    #[cfg(not(feature = "desktop"))]
    if result.is_ok() {
        app.state::<Arc<crate::cell_lifecycle::CellLifecycle>>()
            .job_finished(&operation_id);
    }
    result
}

pub(crate) fn recover_crdt_operations(app: AppHandle) -> Result<usize, String> {
    let operations = recover_pending_crdt_operations(&app)?;
    let recovered_count = operations.len();
    if recovered_count == 0 {
        return Ok(0);
    }
    if let Some(boundary) = app.try_state::<Arc<crate::cell_graph_boundary::CellGraphBoundary>>() {
        // Validate the entire durable prefix before enqueueing any of it. A
        // poisoned second operation must not leave an owned first operation
        // partially admitted when startup fails.
        for operation in &operations {
            boundary
                .authorize_crdt_operation(&operation.kind, &operation.graph_id, &operation.payload)
                .map_err(|error| {
                    format!(
                        "recovered operation {} violates the cell graph boundary: {}",
                        operation.operation_id,
                        error.mcp_message()
                    )
                })?;
        }
    }
    let queue = app.state::<CrdtOperationQueue>();
    for mut operation in operations {
        #[cfg(not(feature = "frontend-crdt"))]
        {
            if operation_kind_uses_recovery_provenance(&operation.kind) {
                if let Some(payload) = operation.payload.as_object_mut() {
                    payload.insert(
                        RECOVERED_OPERATION_PAYLOAD_KEY.to_string(),
                        serde_json::Value::Bool(true),
                    );
                }
            }
        }
        #[cfg(not(feature = "desktop"))]
        let lifecycle = app
            .state::<Arc<crate::cell_lifecycle::CellLifecycle>>()
            .inner()
            .clone();
        #[cfg(not(feature = "desktop"))]
        lifecycle
            .job_started(&operation.operation_id)
            .map_err(|error| error.to_string())?;
        if let Err(error) = queue.enqueue_detached(operation.clone()) {
            #[cfg(not(feature = "desktop"))]
            lifecycle.job_finished(&operation.operation_id);
            return Err(error);
        }
        if let Err(error) = app.emit("native-crdt-operation", &operation) {
            log::debug!("failed to emit recovered native-crdt-operation event: {error}");
        }
    }
    Ok(recovered_count)
}

pub(crate) async fn enqueue_crdt_operation(
    app: AppHandle,
    input: EnqueueCrdtOperationInput,
) -> Result<serde_json::Value, String> {
    enqueue_crdt_operation_outcome(app, input)
        .await
        .map(|outcome| outcome.value)
}

/// Parse an epoch-ms string and return it as a JSON number, or `null` on failure.
fn enqueue_timestamp_as_order(enqueue_timestamp: &str) -> Option<u64> {
    enqueue_timestamp.parse::<u64>().ok()
}

/// Write `order` into a payload object when the caller did not supply one.
/// The value is derived from the operation's journal-recorded enqueue timestamp
/// (epoch milliseconds), so replay produces the same `order` rather than a
/// fresh `Date.now()`.  A2 item 10.
fn inject_order_if_absent(
    obj: &mut serde_json::Map<String, serde_json::Value>,
    enqueue_timestamp: &str,
) {
    let missing = obj.get("order").map(|v| v.is_null()).unwrap_or(true);
    if missing {
        if let Some(ts) = enqueue_timestamp_as_order(enqueue_timestamp) {
            obj.insert("order".to_string(), serde_json::json!(ts));
        }
    }
}

/// Write `updatedAt` into a payload object when the caller did not supply one.
/// Derives from the journal-recorded enqueue timestamp (epoch milliseconds) so
/// replay produces the same `updatedAt` rather than a fresh `Date.now()`. The
/// value lands as an epoch-ms number; TS handlers that need ISO-8601 should
/// `new Date(updatedAt).toISOString()` at the use site.  A2 item 11.
fn inject_updated_at_if_absent(
    obj: &mut serde_json::Map<String, serde_json::Value>,
    enqueue_timestamp: &str,
) {
    let missing = obj.get("updatedAt").map(|v| v.is_null()).unwrap_or(true);
    if missing {
        if let Some(ts) = enqueue_timestamp_as_order(enqueue_timestamp) {
            obj.insert("updatedAt".to_string(), serde_json::json!(ts));
        }
    }
}

/// Inject stable IDs into operation payloads that previously relied on
/// lazy `crypto.randomUUID()` fallbacks inside the TS handler.  Generating
/// here means the journaled record is the single source of truth: recovery
/// replays with the same IDs.
///
/// Also injects a deterministic `order` field (derived from `enqueue_timestamp`)
/// for operation kinds whose handlers previously fell back to `Date.now()`.
/// The journaled enqueue timestamp is stable across replays, so this makes sort
/// order reproducible.  Item 10 of the A2 replay-mitigations plan.
fn normalize_payload_ids(kind: &str, payload: &mut serde_json::Value, enqueue_timestamp: &str) {
    let obj = match payload.as_object_mut() {
        Some(obj) => obj,
        None => return,
    };
    match kind {
        "workspace.createDocument" | "import.webClip" => {
            let missing = obj.get("documentId").map(|v| v.is_null()).unwrap_or(true);
            if missing {
                obj.insert(
                    "documentId".to_string(),
                    serde_json::json!(format!("doc-{}", Uuid::new_v4().simple())),
                );
            }
            inject_order_if_absent(obj, enqueue_timestamp);
            inject_updated_at_if_absent(obj, enqueue_timestamp);
        }
        "workspace.updateDocument" => {
            inject_updated_at_if_absent(obj, enqueue_timestamp);
        }
        "workspace.createFolder" => {
            let missing = obj.get("folderId").map(|v| v.is_null()).unwrap_or(true);
            if missing {
                obj.insert(
                    "folderId".to_string(),
                    serde_json::json!(format!("folder-{}", Uuid::new_v4().simple())),
                );
            }
            inject_order_if_absent(obj, enqueue_timestamp);
            inject_updated_at_if_absent(obj, enqueue_timestamp);
        }
        "workspace.updateFolder" | "workspace.moveFolder" => {
            inject_updated_at_if_absent(obj, enqueue_timestamp);
        }
        "workspace.moveDocuments" => {
            inject_order_if_absent(obj, enqueue_timestamp);
            inject_updated_at_if_absent(obj, enqueue_timestamp);
        }
        "workspace.putArtifact" => {
            inject_order_if_absent(obj, enqueue_timestamp);
            inject_updated_at_if_absent(obj, enqueue_timestamp);
        }
        "workspace.refreshWire" => {
            inject_updated_at_if_absent(obj, enqueue_timestamp);
        }
        "document.write" => {
            inject_order_if_absent(obj, enqueue_timestamp);
            inject_updated_at_if_absent(obj, enqueue_timestamp);
        }
        "document.editComment" => {
            inject_updated_at_if_absent(obj, enqueue_timestamp);
        }
        "workspace.createWire" => {
            let missing = obj.get("wireId").map(|v| v.is_null()).unwrap_or(true);
            if missing {
                obj.insert(
                    "wireId".to_string(),
                    serde_json::json!(format!("w-{}", &Uuid::new_v4().simple().to_string()[..8])),
                );
            }
            inject_updated_at_if_absent(obj, enqueue_timestamp);
        }
        "document.batchPrepare" => {
            let missing_batch = obj.get("batchId").map(|v| v.is_null()).unwrap_or(true);
            if missing_batch {
                obj.insert(
                    "batchId".to_string(),
                    serde_json::json!(format!("batch-{}", Uuid::new_v4().simple())),
                );
            }
            // Pre-populate folderIdMap keyed by each folder path AND every
            // intermediate path component, so the handler resolves stable IDs
            // for ancestors as well as leaves on first apply and on replay.
            let leaf_paths: Vec<String> = obj
                .get("folders")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            if !leaf_paths.is_empty() {
                let existing_map = obj
                    .get("folderIdMap")
                    .and_then(|v| v.as_object())
                    .cloned()
                    .unwrap_or_default();
                // Expand each leaf path "a/b/c" into ["a", "a/b", "a/b/c"].
                let mut all_paths: Vec<String> = Vec::new();
                for leaf in &leaf_paths {
                    let parts: Vec<&str> = leaf.split('/').filter(|p| !p.is_empty()).collect();
                    for end in 1..=parts.len() {
                        let prefix = parts[..end].join("/");
                        if !all_paths.contains(&prefix) {
                            all_paths.push(prefix);
                        }
                    }
                }
                let mut folder_id_map = serde_json::Map::new();
                for path in &all_paths {
                    if let Some(existing) = existing_map.get(path) {
                        folder_id_map.insert(path.clone(), existing.clone());
                    } else {
                        folder_id_map.insert(
                            path.clone(),
                            serde_json::json!(format!("folder-{}", Uuid::new_v4().simple())),
                        );
                    }
                }
                obj.insert(
                    "folderIdMap".to_string(),
                    serde_json::Value::Object(folder_id_map),
                );
            }
            inject_updated_at_if_absent(obj, enqueue_timestamp);
        }
        "document.batchRegister" => {
            inject_updated_at_if_absent(obj, enqueue_timestamp);
        }
        "block.insert" => {
            // Ensure every block in the tiptapJson content array carries a
            // data-block-id so the handler can require it without generating.
            inject_block_ids_in_tiptap_content(obj);
        }
        "document.uploadIngest" | "document.ingestMarkdownOriginal" => {
            let missing = obj.get("documentId").map(|v| v.is_null()).unwrap_or(true);
            if missing {
                obj.insert(
                    "documentId".to_string(),
                    serde_json::json!(format!("doc-{}", Uuid::new_v4().simple())),
                );
            }
        }
        _ => {}
    }
}

fn requested_operation_id(payload: &serde_json::Value) -> Result<Option<String>, String> {
    let Some(raw) = payload
        .get("operationId")
        .or_else(|| payload.get("operation_id"))
    else {
        return Ok(None);
    };
    let value = raw
        .as_str()
        .ok_or_else(|| "operationId must be a string".to_string())?
        .trim();
    if value.is_empty() {
        return Err("operationId must not be empty".to_string());
    }
    if value.len() > 160
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':'))
    {
        return Err("operationId must be <=160 characters of [A-Za-z0-9._:-]".to_string());
    }
    Ok(Some(value.to_string()))
}

/// Stamp the active graph incarnation into the journaled payload. The token is
/// deliberately payload-local for backward-compatible journal recovery; old
/// pending operations without it retain legacy behavior, while every newly
/// enqueued operation against an existing graph is fenced across process
/// restart and delete/recreate.
fn inject_graph_incarnation(
    app: &AppHandle,
    kind: &str,
    graph_id: &str,
    payload: &mut serde_json::Value,
) -> Result<(), String> {
    let expected = crate::graph_incarnation_admission::expected_incarnation(payload)
        .map_err(crate::app_error::AppError::message)?;
    if let Some(expected) = &expected {
        crate::graph_incarnation_admission::require_current(app, graph_id, expected)
            .map_err(crate::app_error::AppError::message)?;
        // Preserve the supplied identity; never backfill or heal on its behalf.
        payload[GRAPH_INCARNATION_PAYLOAD_KEY] = serde_json::json!(expected);
        return Ok(());
    }
    if kind == "document.createOnce" || kind == "artifact.mutateText" || kind == "workspace.setCustomCss" {
        // First creation must not heal a missing graph at admission either.
        crate::graph_record_store::read_graph_record_no_heal(app, graph_id)
            .map_err(crate::app_error::AppError::message)?;
    } else if kind == "graph.importArchive" {
        match crate::graph_record_store::read_graph_record_no_heal(app, graph_id) {
            Ok(_) => {}
            Err(error) if error.kind() == AppErrorKind::NotFound => {
                // This operation may create its target. Do not invoke F4c
                // self-heal here or import would collide with a fabricated
                // graph before its own partial-replay guard can run.
                return Ok(());
            }
            Err(error) => return Err(error.message()),
        }
    } else {
        // Fresh authorized existing-graph writes intentionally retain F4c
        // self-heal. The lifecycle lease spans this lookup through insertion.
        crate::graph_record_store::read_graph_record(app, graph_id)
            .map_err(crate::app_error::AppError::message)?;
    }
    let incarnation_id = crate::graph_record_store::ensure_graph_incarnation(app, graph_id)
        .map_err(crate::app_error::AppError::message)?;
    let object = payload
        .as_object_mut()
        .ok_or_else(|| "CRDT operation payload must be a JSON object".to_string())?;
    if let Some(expected) = object
        .get(GRAPH_INCARNATION_PAYLOAD_KEY)
        .and_then(serde_json::Value::as_str)
    {
        if expected != incarnation_id {
            return Err(format!(
                "stale graph incarnation for {graph_id}: expected {expected}, actual {incarnation_id}"
            ));
        }
    }
    object.insert(
        GRAPH_INCARNATION_PAYLOAD_KEY.to_string(),
        serde_json::json!(incarnation_id),
    );
    Ok(())
}

/// Walk the `tiptapJson.content` array (if present) and inject a
/// `data-block-id` attribute on any top-level block that lacks one.
fn inject_block_ids_in_tiptap_content(obj: &mut serde_json::Map<String, serde_json::Value>) {
    let tiptap_key = if obj.contains_key("tiptapJson") {
        "tiptapJson"
    } else if obj.contains_key("tiptap_json") {
        "tiptap_json"
    } else {
        return;
    };
    if let Some(tiptap_val) = obj.get_mut(tiptap_key) {
        if let Some(content) = tiptap_val
            .as_object_mut()
            .and_then(|o| o.get_mut("content"))
            .and_then(|c| c.as_array_mut())
        {
            for block in content.iter_mut() {
                if let Some(block_obj) = block.as_object_mut() {
                    let attrs = block_obj
                        .entry("attrs".to_string())
                        .or_insert_with(|| serde_json::json!({}));
                    if let Some(attrs_obj) = attrs.as_object_mut() {
                        let missing = attrs_obj
                            .get("data-block-id")
                            .map(|v| v.is_null())
                            .unwrap_or(true);
                        if missing {
                            attrs_obj.insert(
                                "data-block-id".to_string(),
                                serde_json::json!(format!(
                                    "block-{}",
                                    &Uuid::new_v4().simple().to_string()[..8]
                                )),
                            );
                        }
                    }
                }
            }
        }
    }
}

pub(crate) async fn enqueue_crdt_operation_outcome(
    app: AppHandle,
    input: EnqueueCrdtOperationInput,
) -> Result<EnqueuedCrdtOutcome, String> {
    enqueue_crdt_operation_outcome_inner(app, input, None).await
}

pub(crate) async fn enqueue_crdt_operation_outcome_marked(
    app: AppHandle,
    input: EnqueueCrdtOperationInput,
    queued: Arc<AtomicBool>,
) -> Result<EnqueuedCrdtOutcome, String> {
    enqueue_crdt_operation_outcome_inner(app, input, Some(queued)).await
}

async fn enqueue_crdt_operation_outcome_inner(
    app: AppHandle,
    mut input: EnqueueCrdtOperationInput,
    queued: Option<Arc<AtomicBool>>,
) -> Result<EnqueuedCrdtOutcome, String> {
    if input.kind.trim().is_empty() {
        return Err("CRDT operation kind is required".to_string());
    }
    if input.graph_id.trim().is_empty() {
        return Err("CRDT operation graphId is required".to_string());
    }
    let expected = crate::graph_incarnation_admission::expected_incarnation(&input.payload)
        .map_err(crate::app_error::AppError::message)?;
    if expected.is_some() && cfg!(feature = "frontend-crdt") {
        return Err("expected-incarnation mutation requires the native CRDT executor".into());
    }
    if expected.is_some() && app.try_state::<crate::crdt_engine::persistence_coordinator::GraphPersistenceCoordinator>().is_none() {
        return Err("expected-incarnation mutation requires a managed graph lifecycle coordinator".into());
    }
    if let Some(boundary) = app.try_state::<Arc<crate::cell_graph_boundary::CellGraphBoundary>>() {
        boundary
            .authorize_crdt_operation(&input.kind, &input.graph_id, &input.payload)
            .map_err(|error| error.mcp_message())?;
    }
    if let Some(payload) = input.payload.as_object_mut() {
        payload.remove(RECOVERED_OPERATION_PAYLOAD_KEY);
    }
    let enqueue_lease =
        match app
            .try_state::<crate::crdt_engine::persistence_coordinator::GraphPersistenceCoordinator>()
        {
            Some(coordinator) => Some(coordinator.acquire_hot_write(&input.graph_id).await?),
            None => None,
        };
    if let Some(lease) = &enqueue_lease {
        lease.set_crdt_kind(&input.kind);
    }
    require_no_active_restore(&app, &input.graph_id)?;

    // Capture the enqueue timestamp before normalizing so that `order`
    // derivation and the journaled record share the exact same value.
    let enqueue_timestamp = timestamp();
    normalize_payload_ids(&input.kind, &mut input.payload, &enqueue_timestamp);
    inject_graph_incarnation(&app, &input.kind, &input.graph_id, &mut input.payload)?;

    let operation = CrdtOperation {
        operation_id: requested_operation_id(&input.payload)?
            .unwrap_or_else(|| format!("op-{}", Uuid::new_v4().simple())),
        kind: input.kind,
        graph_id: input.graph_id,
        document_id: input.document_id,
        payload: input.payload,
        enqueue_timestamp,
    };

    record_crdt_operation_queued(&app, &operation)?;
    #[cfg(not(feature = "desktop"))]
    let lifecycle = app
        .state::<Arc<crate::cell_lifecycle::CellLifecycle>>()
        .inner()
        .clone();
    #[cfg(not(feature = "desktop"))]
    lifecycle
        .job_started(&operation.operation_id)
        .map_err(|error| error.to_string())?;
    let queue = app.state::<CrdtOperationQueue>();
    let receiver = match queue.enqueue(operation.clone()) {
        Ok(receiver) => receiver,
        Err(error) => {
            #[cfg(not(feature = "desktop"))]
            lifecycle.job_finished(&operation.operation_id);
            return Err(error);
        }
    };
    if let Some(queued) = queued.as_deref() {
        // There is intentionally no await between queue insertion and this
        // release-store. Cancellation can only occur at a poll boundary, so a
        // timeout observer never sees false for an already-queued operation.
        queued.store(true, Ordering::Release);
    }
    // Deletion may proceed once the incarnation-bearing operation is durable
    // and visible to the queue. The executor reacquires the graph lease and
    // validates the token before dispatch; never hold this root lease while
    // waiting for that executor or a desktop frontend response.
    drop(enqueue_lease);
    let emit_started = Instant::now();
    if let Err(error) = app.emit("native-crdt-operation", &operation) {
        log::debug!("failed to emit native-crdt-operation event: {error}");
    }
    queue.mark_emitted(&operation.operation_id, duration_ms(emit_started.elapsed()));

    // The Rust cell is the CRDT authority in both gardend and the Shrubbery
    // desktop shell. Only an explicitly requested legacy frontend build polls
    // and completes this queue from TypeScript.
    #[cfg(not(feature = "frontend-crdt"))]
    {
        let executor_app = app.clone();
        crate::app_runtime::async_runtime::spawn(async move {
            crate::crdt_engine::executor::drain_queue(executor_app).await;
        });
    }

    let outcome = match tokio::time::timeout(
        Duration::from_secs(local_crdt_operation_timeout_secs()),
        receiver,
    )
    .await
    {
        Ok(Ok(result)) if result.ok => Ok(EnqueuedCrdtOutcome {
            operation_id: operation.operation_id.clone(),
            value: result
                .value
                .unwrap_or_else(|| serde_json::json!({ "ok": true })),
        }),
        Ok(Ok(result)) => Err(result
            .error
            .unwrap_or_else(|| "CRDT operation failed".to_string())),
        Ok(Err(_)) => Err("CRDT operation response channel closed".to_string()),
        Err(_) => {
            if let Err(error) = record_crdt_operation_caller_timeout(&app, &operation.operation_id)
            {
                log::error!(
                    "failed to append CRDT operation caller-timeout for {}: {}",
                    operation.operation_id,
                    error
                );
            }
            queue.detach_responder(&operation.operation_id);
            Err(CRDT_OPERATION_STILL_QUEUED_ERROR.to_string())
        }
    };

    audit_crdt_operation(&app, &operation, &outcome);
    outcome
}

pub(crate) fn record_crdt_phase(
    app: &AppHandle,
    operation_id: Option<&str>,
    phase: &str,
    elapsed: Duration,
) {
    if let Some(operation_id) = operation_id {
        let queue = app.state::<CrdtOperationQueue>();
        let _ = queue.add_phase(operation_id, phase, duration_ms(elapsed));
    }
}

#[cfg(test)]
mod tests {
    use super::{normalize_payload_ids, requested_operation_id, LOCAL_CRDT_OPERATION_TIMEOUT_SECS};

    const FIXED_TS: &str = "1700000000000";

    #[test]
    fn timeout_is_at_least_ten_minutes() {
        assert!(
            LOCAL_CRDT_OPERATION_TIMEOUT_SECS >= 600,
            "CRDT timeout shrank below 10 minutes ({LOCAL_CRDT_OPERATION_TIMEOUT_SECS}s); slow ops would orphan again"
        );
    }

    #[test]
    fn caller_operation_id_is_stable_and_path_safe() {
        assert_eq!(
            requested_operation_id(&serde_json::json!({
                "operationId": "offline:client-a:create-doc-1"
            }))
            .unwrap()
            .as_deref(),
            Some("offline:client-a:create-doc-1")
        );
        assert!(requested_operation_id(&serde_json::json!({
            "operationId": "../escape"
        }))
        .is_err());
        assert!(requested_operation_id(&serde_json::json!({
            "operationId": ""
        }))
        .is_err());
    }

    #[test]
    fn normalize_injects_document_id_when_absent() {
        let mut payload = serde_json::json!({ "title": "Doc" });
        normalize_payload_ids("workspace.createDocument", &mut payload, FIXED_TS);
        let id = payload["documentId"].as_str().expect("documentId injected");
        assert!(id.starts_with("doc-"), "documentId starts with doc-: {id}");
    }

    #[test]
    fn normalize_preserves_caller_supplied_document_id() {
        let mut payload = serde_json::json!({ "documentId": "doc-caller", "title": "Doc" });
        normalize_payload_ids("workspace.createDocument", &mut payload, FIXED_TS);
        assert_eq!(payload["documentId"], "doc-caller");
    }

    #[test]
    fn normalize_injects_document_id_for_web_clip() {
        let mut payload = serde_json::json!({ "url": "https://example.com", "html": "<p>hi</p>" });
        normalize_payload_ids("import.webClip", &mut payload, FIXED_TS);
        let id = payload["documentId"].as_str().expect("documentId injected");
        assert!(id.starts_with("doc-"), "documentId starts with doc-: {id}");
    }

    #[test]
    fn normalize_injects_folder_id_when_absent() {
        let mut payload = serde_json::json!({ "name": "Folder" });
        normalize_payload_ids("workspace.createFolder", &mut payload, FIXED_TS);
        let id = payload["folderId"].as_str().expect("folderId injected");
        assert!(
            id.starts_with("folder-"),
            "folderId starts with folder-: {id}"
        );
    }

    #[test]
    fn normalize_injects_wire_id_when_absent() {
        let mut payload = serde_json::json!({ "sourceDocumentId": "s", "targetDocumentId": "t" });
        normalize_payload_ids("workspace.createWire", &mut payload, FIXED_TS);
        let id = payload["wireId"].as_str().expect("wireId injected");
        assert!(id.starts_with("w-"), "wireId starts with w-: {id}");
    }

    #[test]
    fn normalize_injects_batch_id_and_folder_id_map() {
        let mut payload = serde_json::json!({
            "clientBatchKey": "key-a",
            "folders": ["a/b", "a/b/c"],
        });
        normalize_payload_ids("document.batchPrepare", &mut payload, FIXED_TS);
        let batch_id = payload["batchId"].as_str().expect("batchId injected");
        assert!(
            batch_id.starts_with("batch-"),
            "batchId starts with batch-: {batch_id}"
        );
        let map = payload["folderIdMap"]
            .as_object()
            .expect("folderIdMap injected");
        let id_ab = map["a/b"].as_str().expect("a/b mapped");
        let id_abc = map["a/b/c"].as_str().expect("a/b/c mapped");
        assert!(id_ab.starts_with("folder-"), "{id_ab}");
        assert!(id_abc.starts_with("folder-"), "{id_abc}");
        assert_ne!(id_ab, id_abc);
    }

    #[test]
    fn normalize_preserves_caller_supplied_folder_id_map_entries() {
        let mut payload = serde_json::json!({
            "clientBatchKey": "key-b",
            "batchId": "batch-stable",
            "folders": ["x/y"],
            "folderIdMap": { "x/y": "folder-stable" },
        });
        normalize_payload_ids("document.batchPrepare", &mut payload, FIXED_TS);
        assert_eq!(payload["batchId"], "batch-stable");
        assert_eq!(payload["folderIdMap"]["x/y"], "folder-stable");
    }

    #[test]
    fn normalize_injects_block_ids_in_tiptap_content() {
        let mut payload = serde_json::json!({
            "tiptapJson": {
                "type": "doc",
                "content": [
                    { "type": "paragraph", "attrs": {} },
                    { "type": "paragraph", "attrs": { "data-block-id": "block-existing" } },
                ]
            }
        });
        normalize_payload_ids("block.insert", &mut payload, FIXED_TS);
        let content = payload["tiptapJson"]["content"].as_array().unwrap();
        let id0 = content[0]["attrs"]["data-block-id"]
            .as_str()
            .expect("id injected for block 0");
        assert!(id0.starts_with("block-"), "{id0}");
        assert_eq!(content[1]["attrs"]["data-block-id"], "block-existing");
    }

    #[test]
    fn normalize_is_noop_for_unrelated_kinds() {
        let mut payload = serde_json::json!({ "documentId": "doc-a" });
        normalize_payload_ids("workspace.deleteDocument", &mut payload, FIXED_TS);
        assert_eq!(payload["documentId"], "doc-a");
    }

    #[test]
    fn normalize_injects_document_id_for_upload_ingest() {
        let mut payload = serde_json::json!({ "filename": "report.pdf", "dataBase64": "abc" });
        normalize_payload_ids("document.uploadIngest", &mut payload, FIXED_TS);
        let id = payload["documentId"].as_str().expect("documentId injected");
        assert!(id.starts_with("doc-"), "documentId starts with doc-: {id}");
    }

    #[test]
    fn normalize_preserves_caller_document_id_for_upload_ingest() {
        let mut payload =
            serde_json::json!({ "documentId": "doc-stable", "filename": "report.pdf" });
        normalize_payload_ids("document.uploadIngest", &mut payload, FIXED_TS);
        assert_eq!(payload["documentId"], "doc-stable");
    }

    #[test]
    fn normalize_injects_document_id_for_ingest_markdown_original() {
        let mut payload = serde_json::json!({ "filename": "doc.pdf", "markdown": "# Title", "dataBase64": "abc" });
        normalize_payload_ids("document.ingestMarkdownOriginal", &mut payload, FIXED_TS);
        let id = payload["documentId"].as_str().expect("documentId injected");
        assert!(id.starts_with("doc-"), "documentId starts with doc-: {id}");
    }

    #[test]
    fn normalize_preserves_caller_document_id_for_ingest_markdown_original() {
        let mut payload = serde_json::json!({ "documentId": "doc-stable", "filename": "doc.pdf", "markdown": "# Title" });
        normalize_payload_ids("document.ingestMarkdownOriginal", &mut payload, FIXED_TS);
        assert_eq!(payload["documentId"], "doc-stable");
    }

    // ── order-derivation tests (A2 item 10) ──────────────────────────────────

    #[test]
    fn normalize_injects_order_for_create_document_when_absent() {
        let mut payload = serde_json::json!({ "title": "Doc" });
        normalize_payload_ids("workspace.createDocument", &mut payload, FIXED_TS);
        let order = payload["order"].as_u64().expect("order injected");
        assert_eq!(order, 1_700_000_000_000u64);
    }

    #[test]
    fn normalize_preserves_caller_supplied_order_for_create_document() {
        let mut payload = serde_json::json!({ "title": "Doc", "order": 42 });
        normalize_payload_ids("workspace.createDocument", &mut payload, FIXED_TS);
        assert_eq!(payload["order"], 42);
    }

    #[test]
    fn normalize_order_is_stable_across_two_calls_with_same_timestamp() {
        // Simulates replay: same timestamp → same order.
        let mut payload1 = serde_json::json!({ "title": "Doc" });
        let mut payload2 = serde_json::json!({ "title": "Doc" });
        normalize_payload_ids("workspace.createDocument", &mut payload1, FIXED_TS);
        normalize_payload_ids("workspace.createDocument", &mut payload2, FIXED_TS);
        assert_eq!(payload1["order"], payload2["order"]);
    }

    #[test]
    fn normalize_injects_order_for_create_folder_when_absent() {
        let mut payload = serde_json::json!({ "name": "Folder" });
        normalize_payload_ids("workspace.createFolder", &mut payload, FIXED_TS);
        let order = payload["order"].as_u64().expect("order injected");
        assert_eq!(order, 1_700_000_000_000u64);
    }

    #[test]
    fn normalize_preserves_caller_supplied_order_for_create_folder() {
        let mut payload = serde_json::json!({ "name": "Folder", "order": 99 });
        normalize_payload_ids("workspace.createFolder", &mut payload, FIXED_TS);
        assert_eq!(payload["order"], 99);
    }

    #[test]
    fn normalize_injects_order_for_move_documents_when_absent() {
        let mut payload = serde_json::json!({ "documentIds": ["doc-a"] });
        normalize_payload_ids("workspace.moveDocuments", &mut payload, FIXED_TS);
        let order = payload["order"].as_u64().expect("order injected");
        assert_eq!(order, 1_700_000_000_000u64);
    }

    #[test]
    fn normalize_preserves_caller_supplied_order_for_move_documents() {
        let mut payload = serde_json::json!({ "documentIds": ["doc-a"], "order": 200 });
        normalize_payload_ids("workspace.moveDocuments", &mut payload, FIXED_TS);
        assert_eq!(payload["order"], 200);
    }

    #[test]
    fn normalize_injects_order_for_document_write_when_absent() {
        let mut payload = serde_json::json!({ "documentId": "doc-a", "content": "# Hi" });
        normalize_payload_ids("document.write", &mut payload, FIXED_TS);
        let order = payload["order"].as_u64().expect("order injected");
        assert_eq!(order, 1_700_000_000_000u64);
    }

    #[test]
    fn normalize_injects_order_for_web_clip_when_absent() {
        let mut payload = serde_json::json!({ "url": "https://example.com", "html": "<p>hi</p>" });
        normalize_payload_ids("import.webClip", &mut payload, FIXED_TS);
        let order = payload["order"].as_u64().expect("order injected");
        assert_eq!(order, 1_700_000_000_000u64);
    }

    #[test]
    fn normalize_does_not_inject_order_for_unrelated_kinds() {
        let mut payload = serde_json::json!({ "documentId": "doc-a" });
        normalize_payload_ids("workspace.deleteDocument", &mut payload, FIXED_TS);
        // order should not be present since this kind doesn't carry it
        assert!(payload.get("order").is_none());
    }

    #[cfg(all(feature = "headless", not(feature = "desktop")))]
    #[test]
    fn enqueue_cannot_journal_tokenless_operation_across_delete() {
        use crate::runtime_config::GRAPH_STATUS_DELETED;
        use std::path::PathBuf;
        #[cfg(feature = "desktop")]
        use tauri::Manager;
        use uuid::Uuid;

        let _serial = crate::tauri_runtime::profile_env_serial()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let profile: PathBuf =
            std::env::temp_dir().join(format!("garden-enqueue-delete-race-{}", Uuid::new_v4()));
        std::env::set_var("GARDEN_PROFILE_DIR", &profile);

        let result = std::panic::catch_unwind(|| {
            crate::app_runtime::async_runtime::block_on(async {
                let app = crate::tauri_runtime::build_mock_app_for_tests(true);
                let graph_id = "enqueue-delete-race";
                crate::graph_service::create_graph_service(
                    &app,
                    crate::graph_service::CreateGraphInput {
                        title: "Enqueue Delete Race".to_string(),
                        graph_id: Some(graph_id.to_string()),
                        description: None,
                        operation_id: None,
                    },
                )
                .expect("create graph");
                let (graph_dir, mut graph) =
                    crate::graph_record_store::read_graph_record(&app, graph_id)
                        .expect("active graph");
                let coordinator = app.state::<
                    crate::crdt_engine::persistence_coordinator::GraphPersistenceCoordinator,
                >();
                let lifecycle_lease = coordinator
                    .acquire_lifecycle_exclusive(graph_id)
                    .await
                    .expect("graph lease");

                let queued = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
                let blocked = super::enqueue_crdt_operation_outcome_marked(
                    app.clone(),
                    super::EnqueueCrdtOperationInput {
                        kind: "workspace.createFolder".to_string(),
                        graph_id: graph_id.to_string(),
                        document_id: None,
                        payload: serde_json::json!({ "title": "Blocked before enqueue" }),
                    },
                    queued.clone(),
                );
                assert!(
                    tokio::time::timeout(std::time::Duration::from_millis(25), blocked)
                        .await
                        .is_err(),
                    "held graph lease keeps the marked future pre-enqueue"
                );
                assert!(
                    !queued.load(std::sync::atomic::Ordering::Acquire),
                    "a timeout before queue insertion must remain safe to retry"
                );
                assert!(
                    crate::crdt_operation_journal::recover_pending_crdt_operations(&app)
                        .expect("journal after pre-enqueue timeout")
                        .is_empty()
                );
                assert!(app
                    .state::<super::CrdtOperationQueue>()
                    .poll(Some(8))
                    .expect("queue after pre-enqueue timeout")
                    .is_empty());

                let app_for_enqueue = app.clone();
                let (started_tx, started_rx) = tokio::sync::oneshot::channel();
                let enqueue = crate::app_runtime::async_runtime::spawn(async move {
                    let _ = started_tx.send(());
                    super::enqueue_crdt_operation_outcome(
                        app_for_enqueue,
                        super::EnqueueCrdtOperationInput {
                            kind: "workspace.createFolder".to_string(),
                            graph_id: graph_id.to_string(),
                            document_id: None,
                            payload: serde_json::json!({ "title": "Must not land" }),
                        },
                    )
                    .await
                });
                started_rx.await.expect("enqueue waiter started");
                tokio::task::yield_now().await;

                graph.status = GRAPH_STATUS_DELETED.to_string();
                crate::graph_record_store::write_graph_record(&graph_dir, &graph)
                    .expect("mark graph deleted under lifecycle lease");
                drop(lifecycle_lease);

                let error = match enqueue.await.expect("enqueue task") {
                    Ok(_) => {
                        panic!("ordinary operation against deleted graph must fail before journal")
                    }
                    Err(error) => error,
                };
                assert!(error.contains("graph not found"), "{error}");
                assert!(
                    crate::crdt_operation_journal::recover_pending_crdt_operations(&app)
                        .expect("recover journal")
                        .is_empty(),
                    "failed enqueue must not leave a tokenless pending journal event"
                );
                assert!(app
                    .state::<super::CrdtOperationQueue>()
                    .poll(Some(8))
                    .expect("poll queue")
                    .is_empty());
            });
        });

        std::env::remove_var("GARDEN_PROFILE_DIR");
        let _ = std::fs::remove_dir_all(&profile);
        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
    }

    #[cfg(all(feature = "headless", not(feature = "desktop")))]
    #[test]
    fn marked_enqueue_timeout_after_insertion_completes_exactly_once() {
        #[cfg(feature = "desktop")]
        use tauri::Manager;
        use uuid::Uuid;

        let _serial = crate::tauri_runtime::profile_env_serial()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let profile = std::env::temp_dir().join(format!(
            "garden-marked-enqueue-completion-{}",
            Uuid::new_v4()
        ));
        std::env::set_var("GARDEN_PROFILE_DIR", &profile);

        let result = std::panic::catch_unwind(|| {
            crate::app_runtime::async_runtime::block_on(async {
                let app = crate::tauri_runtime::build_mock_app_for_tests(true);
                let graph_id = "marked-enqueue-completion";
                crate::graph_service::create_graph_service(
                    &app,
                    crate::graph_service::CreateGraphInput {
                        title: "Marked Enqueue Completion".to_string(),
                        graph_id: Some(graph_id.to_string()),
                        description: None,
                        operation_id: None,
                    },
                )
                .expect("create graph");

                let queue = app.state::<super::CrdtOperationQueue>();
                let drain_guard = queue.lock_drain().await;
                let queued = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
                let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel();
                let app_for_enqueue = app.clone();
                let queued_for_enqueue = queued.clone();
                let waiter = crate::app_runtime::async_runtime::spawn(async move {
                    tokio::select! {
                        outcome = super::enqueue_crdt_operation_outcome_marked(
                            app_for_enqueue,
                            super::EnqueueCrdtOperationInput {
                                kind: "workspace.createFolder".to_string(),
                                graph_id: graph_id.to_string(),
                                document_id: None,
                                payload: serde_json::json!({
                                    "title": "Exactly Once",
                                    "name": "Exactly Once",
                                }),
                            },
                            queued_for_enqueue,
                        ) => Some(outcome),
                        _ = cancel_rx => None,
                    }
                });

                tokio::time::timeout(std::time::Duration::from_secs(1), async {
                    while !queued.load(std::sync::atomic::Ordering::Acquire) {
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .expect("marked enqueue reached queue insertion");
                assert_eq!(queue.counts_for_test().expect("queue counts"), (1, 0));
                let pending = crate::crdt_operation_journal::recover_pending_crdt_operations(&app)
                    .expect("pending marked operation");
                assert_eq!(pending.len(), 1);
                let operation_id = pending[0].operation_id.clone();

                cancel_tx.send(()).expect("drop completion waiter");
                assert!(waiter.await.expect("waiter task").is_none());
                drop(drain_guard);

                tokio::time::timeout(std::time::Duration::from_secs(5), async {
                    loop {
                        let no_pending =
                            crate::crdt_operation_journal::recover_pending_crdt_operations(&app)
                                .expect("completion journal")
                                .is_empty();
                        if no_pending
                            && queue.counts_for_test().expect("final queue counts") == (0, 0)
                        {
                            break;
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                    }
                })
                .await
                .expect("queued operation completed after waiter cancellation");
                let traces = queue
                    .recent_traces(
                        Some(20),
                        false,
                        Some("workspace.createFolder"),
                        None,
                        Some(&operation_id),
                    )
                    .expect("completion traces");
                assert_eq!(traces.len(), 1, "operation completed exactly once");
                assert_eq!(traces[0]["ok"], true);
            });
        });

        std::env::remove_var("GARDEN_PROFILE_DIR");
        let _ = std::fs::remove_dir_all(&profile);
        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
    }
}
