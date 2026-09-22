//! In-process CRDT operation executor (gardend and Shrubbery desktop cells).
//!
//! The browser is a Yjs sync client, never the persistence authority. This
//! executor drains the shared operation queue in-process: poll →
//! apply via the Rust engine → complete through the identical journal +
//! responder path. API callers awaiting `enqueue_crdt_operation_outcome`
//! resolve exactly as if the frontend had applied the op.
//!
//! Unported kinds fail fast with a clear error instead of letting callers
//! hang into the 600s queue timeout.

use crate::app_runtime::AppHandle;
use crate::crdt_operation_journal::record_crdt_operation_completion;
use crate::crdt_operation_queue::RetrySchedule;
use crate::crdt_queue::{CompleteCrdtOperationInput, CrdtOperation, CrdtOperationQueue};
use serde_json::Value;
#[cfg(feature = "desktop")]
use tauri::Manager;

/// Structured operation failure contract used by the production drainer.
///
/// A handler may return `RetryableAfterHotCommit` only after its authoritative
/// room sidecar (and, where applicable, operation ledger) is durable. The
/// original journal entry must then remain non-terminal so recovery can repair
/// the remaining cold/RDF/history tail under the same server-generated ID.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ApplyOperationError {
    Terminal(String),
    RetryableAfterHotCommit(String),
}

pub(crate) type ApplyOperationResult<T> = Result<T, ApplyOperationError>;

impl ApplyOperationError {
    pub(crate) fn terminal(error: impl Into<String>) -> Self {
        Self::Terminal(error.into())
    }

    pub(crate) fn retryable_after_hot_commit(error: impl Into<String>) -> Self {
        Self::RetryableAfterHotCommit(error.into())
    }

    pub(crate) fn message(&self) -> &str {
        match self {
            Self::Terminal(error) | Self::RetryableAfterHotCommit(error) => error,
        }
    }

    pub(crate) fn into_message(self) -> String {
        match self {
            Self::Terminal(error) | Self::RetryableAfterHotCommit(error) => error,
        }
    }
}

impl From<String> for ApplyOperationError {
    fn from(error: String) -> Self {
        Self::Terminal(error)
    }
}

impl std::fmt::Display for ApplyOperationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.message())
    }
}

impl std::error::Error for ApplyOperationError {}

/// No operation kinds are deferred in cells anymore: every desktop kind has
/// an in-process handler (heavy parsing delegates to the pure-function
/// pools — embeddings + parser). Kept as a mechanism for future kinds.
const DEFERRED_KINDS: &[&str] = &[];

/// Operation kinds whose handlers run a long *synchronous* body with no
/// `.await` yield points — bulk RDF load of hundreds of thousands of quads,
/// hundreds of document saves, archive unpacking. Run inline on a tokio worker,
/// such an op pins that worker for its whole duration (~90s for a 605K-quad
/// graph); on a small-core node that starves the runtime serving `/health`, the
/// readiness probe fails, and the gateway stops routing to the cell (503)
/// mid-import. We dispatch these via `spawn_blocking` so the async worker pool
/// stays schedulable throughout (and the op's own blocking DB joins leave the
/// worker pool too).
const HEAVY_KINDS: &[&str] = &[
    "graph.importArchive",
    "graph.restoreArchive",
    "import.vault",
];

fn is_heavy_kind(kind: &str) -> bool {
    HEAVY_KINDS.contains(&kind)
}

/// Drain everything currently in the queue. Called after each enqueue and
/// once at startup (recovered operations).
pub(crate) async fn drain_queue(app: AppHandle) {
    let queue = app.state::<CrdtOperationQueue>();
    // Hold through the empty poll. A waiter spawned by a later enqueue either
    // observes that this drainer consumed its ready operation or takes over
    // after it; per-graph retry barriers are enforced atomically by `poll`.
    let _drain_guard = queue.lock_drain().await;
    loop {
        let batch = {
            match queue.poll(Some(1)) {
                Ok(batch) => batch,
                Err(error) => {
                    log::error!("executor poll failed: {error}");
                    return;
                }
            }
        };
        if batch.is_empty() {
            return;
        }
        for operation in batch {
            let outcome = if is_heavy_kind(operation.kind.as_str()) {
                // Move the long synchronous body off the async worker pool.
                let app = app.clone();
                let op = operation.clone();
                match crate::app_runtime::async_runtime::spawn_blocking(move || {
                    crate::app_runtime::async_runtime::block_on(apply_operation_classified(
                        &app, &op,
                    ))
                })
                .await
                {
                    Ok(result) => result,
                    Err(join_error) => Err(ApplyOperationError::terminal(format!(
                        "heavy operation '{}' task failed: {join_error}",
                        operation.kind
                    ))),
                }
            } else {
                apply_operation_classified(&app, &operation).await
            };
            match outcome {
                Ok(value) => complete_terminal(&app, &operation, Ok(value)),
                Err(ApplyOperationError::Terminal(error)) => {
                    complete_terminal(&app, &operation, Err(error));
                }
                Err(ApplyOperationError::RetryableAfterHotCommit(error)) => {
                    let retry = match queue.requeue_retryable_after_hot_commit(operation.clone()) {
                        Ok(retry) => retry,
                        Err(queue_error) => {
                            log::error!(
                                "executor: retain retryable operation {} failed after hot commit: {queue_error}",
                                operation.operation_id
                            );
                            return;
                        }
                    };
                    log::warn!(
                        "executor: operation {} ({}) retained for tail retry after hot commit (attempt {}, delay {}ms, live caller {}): {}",
                        operation.operation_id,
                        operation.kind,
                        retry.attempt,
                        retry.delay.as_millis(),
                        retry.has_live_responder,
                        error,
                    );
                    // The wait token is already an atomic FIFO barrier. Arm
                    // its sole timer and return so the drain mutex is released
                    // during backoff. Enqueue drainers before the deadline
                    // cannot reapply it or bypass it within the same graph,
                    // while unrelated ready graphs remain available.
                    let _retry_timer = spawn_scheduled_retry(app.clone(), retry);
                    // This operation may have blocked only its own graph. A
                    // fresh drainer waits for our guard to drop, then applies
                    // already-queued work from other ready graphs immediately
                    // instead of waiting for this retry's timer or a new API
                    // enqueue to provide the next wakeup.
                    let _ready_continuation = spawn_ready_queue_continuation(app.clone());
                    return;
                }
            }
        }
    }
}

fn spawn_ready_queue_continuation(app: AppHandle) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        drain_queue(app).await;
    })
}

fn spawn_scheduled_retry(app: AppHandle, schedule: RetrySchedule) -> tokio::task::JoinHandle<()> {
    // `drain_queue` always runs inside Garden's Tokio runtime. Using the
    // current runtime keeps the timer bound to the cell lifecycle and lets the
    // paused-clock current-thread harness exercise the production task.
    tokio::spawn(async move {
        let should_drain = {
            let queue = app.state::<CrdtOperationQueue>();
            queue.wait_and_activate_scheduled_retry(schedule).await
        };
        match should_drain {
            Ok(true) => drain_queue(app).await,
            Ok(false) => {}
            Err(error) => log::error!("executor retry timer activation failed: {error}"),
        }
    })
}

fn complete_terminal(app: &AppHandle, operation: &CrdtOperation, outcome: Result<Value, String>) {
    let input = match &outcome {
        Ok(value) => CompleteCrdtOperationInput {
            operation_id: operation.operation_id.clone(),
            ok: true,
            value: Some(value.clone()),
            error: None,
            timing: None,
        },
        Err(error) => CompleteCrdtOperationInput {
            operation_id: operation.operation_id.clone(),
            ok: false,
            value: None,
            error: Some(error.clone()),
            timing: None,
        },
    };
    if let Err(error) = record_crdt_operation_completion(app, &input) {
        log::error!(
            "executor: journal completion failed for {}: {error}",
            operation.operation_id
        );
    }
    let queue = app.state::<CrdtOperationQueue>();
    if let Err(error) = queue.complete(input) {
        log::error!(
            "executor: queue completion failed for {}: {error}",
            operation.operation_id
        );
    }
    #[cfg(not(feature = "desktop"))]
    app.state::<std::sync::Arc<crate::cell_lifecycle::CellLifecycle>>()
        .job_finished(&operation.operation_id);
}

pub(crate) async fn apply_operation(
    app: &AppHandle,
    operation: &CrdtOperation,
) -> Result<Value, String> {
    apply_operation_classified(app, operation)
        .await
        .map_err(ApplyOperationError::into_message)
}

pub(super) async fn apply_operation_classified(
    app: &AppHandle,
    operation: &CrdtOperation,
) -> ApplyOperationResult<Value> {
    if let Some(coordinator) =
        app.try_state::<super::persistence_coordinator::GraphPersistenceCoordinator>()
    {
        let graph_guard = coordinator.acquire_hot_write(&operation.graph_id).await?;
        graph_guard.set_crdt_kind(&operation.kind);
        require_current_graph_incarnation(app, operation)?;
        return apply_operation_inner(app, operation).await;
    }
    // Standalone harnesses compile handlers without the production managed
    // state. They remain useful pure-operation tests, while setup_core always
    // installs the graph gate for desktop/headless applications.
    if crate::graph_incarnation_admission::expected_incarnation(&operation.payload)
        .map_err(crate::app_error::AppError::message)?.is_some()
    {
        return Err(ApplyOperationError::terminal(
            "expected-incarnation execution requires a managed graph lifecycle coordinator",
        ));
    }
    apply_operation_inner(app, operation).await
}

fn require_current_graph_incarnation(
    app: &AppHandle,
    operation: &CrdtOperation,
) -> Result<(), String> {
    let expected = operation
        .payload
        .get(crate::crdt_queue::GRAPH_INCARNATION_PAYLOAD_KEY)
        .and_then(Value::as_str);
    if expected.is_none() && operation.kind == "graph.importArchive" {
        // A legacy archive import may create its target and retains the
        // import handler's partial-replay/collision guard.
        return Ok(());
    }

    let (_graph_dir, graph) =
        crate::graph_record_store::read_graph_record_no_heal(app, &operation.graph_id)
            .map_err(crate::app_error::AppError::message)?;

    if let Some(expected) = expected {
        return if graph.incarnation_id.as_deref() == Some(expected) {
            Ok(())
        } else {
            Err(format!(
                "stale graph incarnation for {}: expected {}, actual {}",
                operation.graph_id,
                expected,
                graph.incarnation_id.as_deref().unwrap_or("missing"),
            ))
        };
    }

    // Legacy tokenless ordinary operations must never invoke a healing handler
    // against a hard-removed graph. The no-heal read above proves an active
    // canonical graph exists. When both durable timestamps parse, it also
    // fences a known replacement created after the operation was enqueued.
    // Creation and enqueue in the same millisecond remain an unavoidable
    // legacy ambiguity; only the UUID-bearing format closes that interval.
    if let (Some(created_at), Some(enqueued_at)) = (
        crate::clock::parse_timestamp(&graph.created_at),
        crate::clock::parse_timestamp(&operation.enqueue_timestamp),
    ) {
        if created_at > enqueued_at {
            return Err(format!(
                "stale legacy graph operation for {}: graph created at {} after enqueue {}",
                operation.graph_id, graph.created_at, operation.enqueue_timestamp,
            ));
        }
    }
    Ok(())
}

async fn apply_operation_inner(
    app: &AppHandle,
    operation: &CrdtOperation,
) -> ApplyOperationResult<Value> {
    let kind = operation.kind.as_str();
    if let Some(ids) = operation.payload.get("documentIds").and_then(Value::as_array) {
        let graph_dir = crate::paths::existing_graph_dir(app, &operation.graph_id)
            .map_err(ApplyOperationError::terminal)?;
        for id in ids.iter().filter_map(Value::as_str) {
            crate::document_body_availability::require_available(&graph_dir, id)
                .map_err(ApplyOperationError::terminal)?;
        }
    }
    // Imported missing bodies may not be recreated by a queued/stale edit.
    // This complements the lower-level room and persistence guards.
    if let Some(id) = operation.document_id.as_deref()
        .or_else(|| operation.payload.get("documentId").and_then(Value::as_str)) {
        let graph_dir = crate::paths::existing_graph_dir(app, &operation.graph_id)
            .map_err(ApplyOperationError::terminal)?;
        crate::document_body_availability::require_available(&graph_dir, id)
            .map_err(ApplyOperationError::terminal)?;
    }
    if DEFERRED_KINDS.contains(&kind) {
        return Err(ApplyOperationError::terminal(format!(
            "operation kind {kind} is deferred in headless cells (M1); use the desktop app or wait for the ingest milestone"
        )));
    }
    match kind {
        "crdt.flush" => super::flush_ops::apply_classified(app, operation)
            .await
            .map_err(ApplyOperationError::from),
        "document.write" => super::document_ops::document_write_classified(app, operation)
            .await
            .map_err(ApplyOperationError::from),
        "document.createOnce" => super::create_once::apply(app, operation).await,
        "artifact.mutateText" => crate::artifact_text_service::apply(app, operation).await,
        "document.editComment" => super::document_ops::edit_comment_classified(app, operation)
            .await
            .map_err(ApplyOperationError::from),
        "document.liveProjection" => super::document_ops::live_projection(app, operation)
            .await
            .map_err(ApplyOperationError::from),
        "document.ingestMarkdownOriginal" => {
            super::document_ops::ingest_markdown_original_classified(app, operation)
                .await
                .map_err(ApplyOperationError::from)
        }
        "document.uploadIngest" => super::upload_ingest_ops::apply_classified(app, operation)
            .await
            .map_err(ApplyOperationError::from),
        "document.batchPrepare" => super::workspace_ops::batch_prepare(app, operation)
            .await
            .map_err(ApplyOperationError::from),
        "document.batchRegister" => super::workspace_ops::batch_register(app, operation)
            .await
            .map_err(ApplyOperationError::from),
        "flow.seed" => super::flow_ops::apply_classified(app, operation)
            .await
            .map_err(ApplyOperationError::from),
        "import.vault" => super::import_vault_ops::apply_classified(app, operation)
            .await
            .map_err(ApplyOperationError::from),
        "graph.importArchive" => super::import_archive_ops::apply_classified(app, operation)
            .await
            .map_err(ApplyOperationError::from),
        "graph.restoreArchive" => super::import_archive_ops::apply_classified(app, operation)
            .await
            .map_err(ApplyOperationError::from),
        "import.webClip" => super::web_clip_ops::apply_classified(app, operation)
            .await
            .map_err(ApplyOperationError::from),
        "block.insert" | "block.update" | "block.editText" | "block.delete" => {
            super::block_ops::apply_classified(app, operation)
                .await
                .map_err(ApplyOperationError::from)
        }
        "workspace.setCustomCss"
        | "workspace.createDocument"
        | "workspace.updateDocument"
        | "workspace.deleteDocument"
        | "workspace.createFolder"
        | "workspace.updateFolder"
        | "workspace.deleteFolder"
        | "workspace.moveFolder"
        | "workspace.moveDocuments"
        | "workspace.putArtifact"
        | "workspace.deleteArtifact"
        | "workspace.createWire"
        | "workspace.refreshWire"
        | "workspace.deleteWire" => super::workspace_ops::apply_classified(app, operation)
            .await
            .map_err(ApplyOperationError::from),
        other => Err(ApplyOperationError::terminal(format!(
            "unsupported local CRDT operation: {other}"
        ))),
    }
}

#[cfg(all(test, feature = "headless", not(feature = "desktop")))]
mod incarnation_recovery_tests {
    use super::*;
    use crate::graph_service::{create_graph_service, CreateGraphInput};
    use std::path::PathBuf;
    #[cfg(feature = "desktop")]
    use tauri::Manager;
    use uuid::Uuid;

    fn temp_profile() -> PathBuf {
        std::env::temp_dir().join(format!("garden-incarnation-recovery-{}", Uuid::new_v4()))
    }

    #[test]
    fn detached_scheduled_retry_redrains_and_completes_without_restart() {
        let _serial = crate::tauri_runtime::profile_env_serial()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let profile = temp_profile();
        std::env::set_var("GARDEN_PROFILE_DIR", &profile);

        let result = std::panic::catch_unwind(|| {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_time()
                .build()
                .expect("current-thread Tokio runtime");
            runtime.block_on(async {
                tokio::time::pause();
                let app = crate::tauri_runtime::build_mock_app_for_tests(true);
                let graph_id = "scheduled-retry-no-restart";
                let folder_id = "folder-after-scheduled-retry";
                let graph = create_graph_service(
                    &app,
                    CreateGraphInput {
                        title: "Scheduled Retry".to_string(),
                        graph_id: Some(graph_id.to_string()),
                        description: None,
                        operation_id: None,
                    },
                )
                .expect("create graph");
                let operation = CrdtOperation {
                    operation_id: "op-scheduled-no-restart".to_string(),
                    kind: "workspace.createFolder".to_string(),
                    graph_id: graph_id.to_string(),
                    document_id: Some(folder_id.to_string()),
                    payload: serde_json::json!({
                        "folderId": folder_id,
                        "name": "Scheduled Folder",
                        "order": 1,
                        "updatedAt": 1,
                        "graphIncarnation": graph.incarnation_id.expect("graph incarnation"),
                    }),
                    enqueue_timestamp: "1".to_string(),
                };
                crate::crdt_operation_journal::record_crdt_operation_queued(&app, &operation)
                    .expect("journal detached operation");
                let queue = app.state::<CrdtOperationQueue>();
                queue
                    .enqueue_detached(operation)
                    .expect("enqueue detached operation");
                let first_attempt = queue.poll(Some(1)).unwrap().remove(0);
                let schedule = queue
                    .requeue_retryable_after_hot_commit(first_attempt)
                    .expect("schedule detached retry");
                let timer = spawn_scheduled_retry(app.clone(), schedule.clone());
                tokio::task::yield_now().await;
                assert_eq!(queue.counts_for_test().unwrap(), (1, 0));

                tokio::time::advance(schedule.delay).await;
                timer.await.expect("scheduled retry task");
                assert_eq!(queue.counts_for_test().unwrap(), (0, 0));
                assert!(
                    crate::crdt_operation_journal::recover_pending_crdt_operations(&app)
                        .expect("read operation journal")
                        .is_empty(),
                    "the scheduled retry should journal terminal success without restart"
                );
                let graph_dir = crate::graph_paths::existing_graph_dir(&app, graph_id)
                    .expect("graph directory");
                let snapshot: serde_json::Value = crate::storage::read_json(
                    &crate::ydoc_paths::workspace_snapshot_path(&graph_dir),
                )
                .expect("workspace snapshot");
                assert_eq!(snapshot["counts"]["folders"], 1);
                assert_eq!(snapshot["tree"]["documents"][0]["id"], folder_id);
            });
        });

        std::env::remove_var("GARDEN_PROFILE_DIR");
        let _ = std::fs::remove_dir_all(&profile);
        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
    }

    #[test]
    fn recovered_operation_rejects_recreated_graph_when_generation_is_zero() {
        let _serial = crate::tauri_runtime::profile_env_serial()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let profile = temp_profile();
        std::env::set_var("GARDEN_PROFILE_DIR", &profile);

        let result = std::panic::catch_unwind(|| {
            crate::app_runtime::async_runtime::block_on(async {
                let app = crate::tauri_runtime::build_mock_app_for_tests(true);
                let graph_id = "incarnation-recovery";
                let original = create_graph_service(
                    &app,
                    CreateGraphInput {
                        title: "Original incarnation".to_string(),
                        graph_id: Some(graph_id.to_string()),
                        description: None,
                        operation_id: None,
                    },
                )
                .expect("create original graph");
                let original_incarnation = original
                    .incarnation_id
                    .clone()
                    .expect("new graph incarnation");
                let (graph_dir, _) = crate::graph_record_store::read_graph_record(&app, graph_id)
                    .expect("original graph dir");
                let pending = CrdtOperation {
                    operation_id: "pending-old-incarnation".to_string(),
                    kind: "workspace.createFolder".to_string(),
                    graph_id: graph_id.to_string(),
                    document_id: None,
                    payload: serde_json::json!({
                        "folderId": "must-not-land",
                        "title": "Must not land",
                        "graphIncarnation": original_incarnation.clone(),
                    }),
                    enqueue_timestamp: "1".to_string(),
                };
                crate::crdt_operation_journal::record_crdt_operation_queued(&app, &pending)
                    .expect("journal old operation");

                // Model durable delete/recreate plus process restart: the
                // replacement exists while the in-memory generation is still
                // its zero/default value.
                std::fs::remove_dir_all(&graph_dir).expect("remove original graph files");
                let missing_error = require_current_graph_incarnation(&app, &pending)
                    .expect_err("missing stale graph is rejected without self-heal");
                assert!(missing_error.contains("graph not found"), "{missing_error}");
                assert!(
                    !graph_dir.join("graph.json").exists(),
                    "incarnation validation must not recreate a missing graph"
                );
                let replacement = create_graph_service(
                    &app,
                    CreateGraphInput {
                        title: "Replacement incarnation".to_string(),
                        graph_id: Some(graph_id.to_string()),
                        description: None,
                        operation_id: None,
                    },
                )
                .expect("create replacement graph");
                assert_ne!(replacement.incarnation_id, original.incarnation_id);
                assert_eq!(
                    app.state::<super::super::persistence_coordinator::GraphPersistenceCoordinator>()
                        .generation(graph_id)
                        .expect("generation"),
                    0,
                    "the durable token, not an in-memory generation, must fence recovery"
                );

                let recovered =
                    crate::crdt_operation_journal::recover_pending_crdt_operations(&app)
                        .expect("recover journal");
                assert_eq!(recovered.len(), 1);
                assert_eq!(
                    recovered[0]
                        .payload
                        .get(crate::crdt_queue::GRAPH_INCARNATION_PAYLOAD_KEY)
                        .and_then(Value::as_str),
                    Some(original_incarnation.as_str())
                );
                let error = apply_operation(&app, &recovered[0])
                    .await
                    .expect_err("old incarnation operation is fenced");
                assert!(error.contains("stale graph incarnation"), "{error}");
            });
        });

        std::env::remove_var("GARDEN_PROFILE_DIR");
        let _ = std::fs::remove_dir_all(&profile);
        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
    }

    #[test]
    fn legacy_tokenless_operation_rejects_missing_and_known_replacement_graphs() {
        let _serial = crate::tauri_runtime::profile_env_serial()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let profile = temp_profile();
        std::env::set_var("GARDEN_PROFILE_DIR", &profile);

        let result = std::panic::catch_unwind(|| {
            crate::app_runtime::async_runtime::block_on(async {
                let app = crate::tauri_runtime::build_mock_app_for_tests(true);
                let graph_id = "legacy-tokenless-recovery";
                let legacy = CrdtOperation {
                    operation_id: "legacy-tokenless".to_string(),
                    kind: "workspace.createFolder".to_string(),
                    graph_id: graph_id.to_string(),
                    document_id: None,
                    payload: serde_json::json!({
                        "folderId": "must-not-land",
                        "title": "Must Not Land",
                    }),
                    enqueue_timestamp: "1".to_string(),
                };

                let missing = apply_operation(&app, &legacy)
                    .await
                    .expect_err("tokenless operation cannot heal missing graph");
                assert!(missing.contains("graph not found"), "{missing}");
                let graph_path = crate::profile_paths::graphs_dir(&app)
                    .expect("graphs dir")
                    .join(graph_id)
                    .join("graph.json");
                assert!(
                    !graph_path.exists(),
                    "legacy validation must not recreate a missing graph"
                );

                let replacement = create_graph_service(
                    &app,
                    CreateGraphInput {
                        title: "Known Replacement".to_string(),
                        graph_id: Some(graph_id.to_string()),
                        description: None,
                        operation_id: None,
                    },
                )
                .expect("create replacement");
                assert!(
                    crate::clock::parse_timestamp(&replacement.created_at).expect("createdAt") > 1
                );
                let stale = apply_operation(&app, &legacy)
                    .await
                    .expect_err("known replacement fences tokenless operation");
                assert!(stale.contains("stale legacy graph operation"), "{stale}");
            });
        });

        std::env::remove_var("GARDEN_PROFILE_DIR");
        let _ = std::fs::remove_dir_all(&profile);
        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
    }
}

#[cfg(all(test, feature = "headless", not(feature = "desktop")))]
mod post_hot_recovery_tests {
    use super::*;
    use crate::crdt_engine::document_ops::IngestFailurePoint;
    use crate::crdt_engine::import_archive_ops::ArchiveFailurePoint;
    use crate::graph_service::{create_graph_service, CreateGraphInput};
    use base64::Engine;
    use flate2::{write::GzEncoder, Compression};
    use serde_json::json;
    use sha2::{Digest, Sha256};
    use std::io::Write;
    use std::path::{Path, PathBuf};
    #[cfg(feature = "desktop")]
    use tauri::Manager;
    use uuid::Uuid;
    use yrs::updates::decoder::Decode;
    use yrs::{Doc, Map as YMap, ReadTxn, StateVector, Transact, Update};

    const EDIT_GRAPH: &str = "retry-edit-comment";
    const INGEST_GRAPH: &str = "retry-direct-ingest";
    const UPLOAD_GRAPH: &str = "retry-upload-ingest";
    const WEB_GRAPH: &str = "retry-web-clip";
    const VAULT_GRAPH: &str = "retry-vault-import";
    const ARCHIVE_GRAPH: &str = "retry-archive-import";

    const EDIT_OPERATION: &str = "op-retry-edit-comment";
    const INGEST_OPERATION: &str = "op-retry-direct-ingest";
    const UPLOAD_OPERATION: &str = "op-retry-upload-ingest";
    const WEB_OPERATION: &str = "op-retry-web-clip";
    const VAULT_OPERATION: &str = "op-retry-vault-import";
    const ARCHIVE_OPERATION: &str = "op-retry-archive-import";

    fn temp_profile() -> PathBuf {
        std::env::temp_dir().join(format!("garden-post-hot-recovery-{}", Uuid::new_v4()))
    }

    fn current_runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("current-thread Tokio runtime")
    }

    fn create_graph(app: &AppHandle, graph_id: &str) -> String {
        create_graph_service(
            app,
            CreateGraphInput {
                title: format!("Recovery {graph_id}"),
                graph_id: Some(graph_id.to_string()),
                description: None,
                operation_id: None,
            },
        )
        .expect("create recovery graph")
        .incarnation_id
        .expect("new graph incarnation")
    }

    fn ordinary_operation(
        operation_id: &str,
        kind: &str,
        graph_id: &str,
        document_id: Option<&str>,
        incarnation: &str,
        mut payload: Value,
    ) -> CrdtOperation {
        payload
            .as_object_mut()
            .expect("operation payload object")
            .insert(
                crate::crdt_queue::GRAPH_INCARNATION_PAYLOAD_KEY.to_string(),
                json!(incarnation),
            );
        CrdtOperation {
            operation_id: operation_id.to_string(),
            kind: kind.to_string(),
            graph_id: graph_id.to_string(),
            document_id: document_id.map(str::to_string),
            payload,
            enqueue_timestamp: "1720000000000".to_string(),
        }
    }

    fn stage_bytes(root: &Path, name: &str, bytes: &[u8]) -> PathBuf {
        let source = root.join(format!("{name}-source.bin"));
        std::fs::write(&source, bytes).expect("write staged source");
        crate::paths::copy_pending_upload_file(root, &source).expect("copy pending upload")
    }

    fn minimal_graph_archive_for(source_user_id: &str, source_graph_id: &str) -> Vec<u8> {
        let document = crate::crdt_engine::builder::ydoc_from_tiptap_json(&json!({
            "type": "doc",
            "content": [{
                "type": "paragraph",
                "attrs": { "data-block-id": "archive-block" },
                "content": [{ "type": "text", "text": "Recovered archive content" }],
            }],
        }));
        let document_update = document
            .transact()
            .encode_state_as_update_v1(&StateVector::default());
        let manifest = serde_json::to_vec(&json!({
            "version": 1,
            "format": "mnemosyne-graph-export",
            "source_user_id": source_user_id,
            "source_graph_id": source_graph_id,
            "source_graph_title": "Recovered Archive",
            "includes_artifacts": false,
            "document_count": 1,
            "rdf_triple_count": 0,
            "files": {
                "rdf": "graph.nq",
                "workspace": "crdt/workspace.yjs",
                "documents_dir": "crdt/documents/"
            }
        }))
        .expect("serialize archive manifest");

        let mut tar = tar::Builder::new(Vec::new());
        for (path, data) in [
            ("manifest.json", manifest.as_slice()),
            ("crdt/documents/archive-doc.yjs", document_update.as_slice()),
        ] {
            let mut header = tar::Header::new_ustar();
            header.set_size(data.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            tar.append_data(&mut header, path, data)
                .expect("append archive entry");
        }
        let tar_bytes = tar.into_inner().expect("finish tar archive");
        let mut gzip = GzEncoder::new(Vec::new(), Compression::default());
        gzip.write_all(&tar_bytes).expect("write gzip archive");
        gzip.finish().expect("finish gzip archive")
    }

    fn minimal_graph_archive() -> Vec<u8> {
        minimal_graph_archive_for("source-user", "source-graph")
    }

    async fn journal_enqueue_and_attempt_once_before_crash(
        app: &AppHandle,
        operation: CrdtOperation,
    ) {
        crate::crdt_operation_journal::record_crdt_operation_queued(app, &operation)
            .expect("journal operation");
        let queue = app.state::<CrdtOperationQueue>();
        queue
            .enqueue_detached(operation)
            .expect("enqueue detached journal operation");
        let attempted = queue
            .poll(Some(1))
            .expect("poll first operation attempt")
            .into_iter()
            .next()
            .expect("queued operation is ready for its first attempt");
        match super::apply_operation_classified(app, &attempted).await {
            Err(ApplyOperationError::RetryableAfterHotCommit(_)) => {
                // Model the narrow but valid process-exit interval after the
                // drainer durably retains the journal entry and installs its
                // per-graph retry barrier, but before that barrier's timer
                // fires. Scheduled-retry behavior has its own production-path
                // test; starting a Tokio timer here would auto-advance under a
                // paused clock and turn this restart test into an in-process
                // retry test.
                queue
                    .requeue_retryable_after_hot_commit(attempted)
                    .expect("retain retryable operation before process exit");
            }
            Ok(value) => {
                panic!("injected first attempt unexpectedly succeeded before restart: {value}")
            }
            Err(ApplyOperationError::Terminal(error)) => {
                panic!("injected first attempt became terminal before restart: {error}")
            }
        }
    }

    fn document(
        app: &AppHandle,
        graph_id: &str,
        document_id: &str,
    ) -> crate::document_types::DocumentRecord {
        crate::document_service::read_document(
            app.clone(),
            graph_id.to_string(),
            document_id.to_string(),
        )
        .expect("read recovered document")
    }

    #[test]
    fn classified_handlers_remain_pending_then_recover_across_restart() {
        let _serial = crate::tauri_runtime::profile_env_serial()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let profile = temp_profile();
        std::env::set_var("GARDEN_PROFILE_DIR", &profile);

        let result = std::panic::catch_unwind(|| {
            let mut retained_pending_paths = Vec::new();
            {
                let runtime = current_runtime();
                runtime.block_on(async {
                    // Keep every scheduled retry behind its barrier. Dropping
                    // this runtime models the process exit before its timer.
                    tokio::time::pause();
                    let app = crate::tauri_runtime::build_mock_app_for_tests(true);
                    let edit_incarnation = create_graph(&app, EDIT_GRAPH);
                    let ingest_incarnation = create_graph(&app, INGEST_GRAPH);
                    let upload_incarnation = create_graph(&app, UPLOAD_GRAPH);
                    let web_incarnation = create_graph(&app, WEB_GRAPH);
                    let vault_incarnation = create_graph(&app, VAULT_GRAPH);

                    let seed = ordinary_operation(
                        "seed-edit-document",
                        "document.write",
                        EDIT_GRAPH,
                        Some("edit-doc"),
                        &edit_incarnation,
                        json!({
                            "documentId": "edit-doc",
                            "title": "Comment recovery",
                            "content": "Seed content",
                            "format": "markdown"
                        }),
                    );
                    super::apply_operation_classified(&app, &seed)
                        .await
                        .expect("seed comment document");

                    let edit = ordinary_operation(
                        EDIT_OPERATION,
                        "document.editComment",
                        EDIT_GRAPH,
                        Some("edit-doc"),
                        &edit_incarnation,
                        json!({
                            "action": "set",
                            "commentId": "comment-recovered",
                            "text": "Survives restart",
                            "updatedAt": 1720000000000_u64
                        }),
                    );
                    crate::crdt_engine::document_ops::fail_next_edit_comment_after_hot_for_test(
                        EDIT_OPERATION,
                    );
                    journal_enqueue_and_attempt_once_before_crash(&app, edit).await;

                    let ingest_dir = crate::graph_paths::existing_graph_dir(&app, INGEST_GRAPH)
                        .expect("direct ingest graph dir");
                    let ingest_pending = stage_bytes(
                        &ingest_dir,
                        "direct-ingest",
                        b"# Direct ingest\n\nRecovered directly.",
                    );
                    retained_pending_paths.push(ingest_pending.clone());
                    let ingest = ordinary_operation(
                        INGEST_OPERATION,
                        "document.ingestMarkdownOriginal",
                        INGEST_GRAPH,
                        Some("ingest-doc"),
                        &ingest_incarnation,
                        json!({
                            "documentId": "ingest-doc",
                            "filename": "direct.md",
                            "mimeType": "text/markdown",
                            "markdown": "# Direct ingest\n\nRecovered directly.",
                            "pendingOriginalPath": ingest_pending.to_string_lossy()
                        }),
                    );
                    crate::crdt_engine::document_ops::fail_next_ingest_step_for_test(
                        INGEST_OPERATION,
                        IngestFailurePoint::AfterTail,
                    );
                    journal_enqueue_and_attempt_once_before_crash(&app, ingest).await;

                    let upload_dir = crate::graph_paths::existing_graph_dir(&app, UPLOAD_GRAPH)
                        .expect("upload ingest graph dir");
                    let upload_pending = stage_bytes(
                        &upload_dir,
                        "upload-ingest",
                        b"# Upload ingest\n\nRecovered upload.",
                    );
                    retained_pending_paths.push(upload_pending.clone());
                    let upload = ordinary_operation(
                        UPLOAD_OPERATION,
                        "document.uploadIngest",
                        UPLOAD_GRAPH,
                        Some("upload-doc"),
                        &upload_incarnation,
                        json!({
                            "documentId": "upload-doc",
                            "filename": "upload.md",
                            "mimeType": "text/markdown",
                            "pendingOriginalPath": upload_pending.to_string_lossy()
                        }),
                    );
                    crate::crdt_engine::document_ops::fail_next_ingest_step_for_test(
                        UPLOAD_OPERATION,
                        IngestFailurePoint::AfterWorkspace,
                    );
                    journal_enqueue_and_attempt_once_before_crash(&app, upload).await;

                    let web = ordinary_operation(
                        WEB_OPERATION,
                        "import.webClip",
                        WEB_GRAPH,
                        Some("web-doc"),
                        &web_incarnation,
                        json!({
                            "documentId": "web-doc",
                            "url": "https://example.test/recovery",
                            "html": "<html><head><title>Recovered clip</title></head><body><main><p>Browser content survives.</p></main></body></html>"
                        }),
                    );
                    crate::crdt_engine::document_ops::fail_next_document_write_after_hot_for_test(
                        WEB_OPERATION,
                    );
                    journal_enqueue_and_attempt_once_before_crash(&app, web).await;

                    let vault_dir = crate::graph_paths::existing_graph_dir(&app, VAULT_GRAPH)
                        .expect("vault graph dir");
                    let vault_pending = stage_bytes(
                        &vault_dir,
                        "vault-import",
                        include_bytes!(concat!(
                            env!("CARGO_MANIFEST_DIR"),
                            "/tests/fixtures/vault_import/obsidian.zip"
                        )),
                    );
                    retained_pending_paths.push(vault_pending.clone());
                    let vault = ordinary_operation(
                        VAULT_OPERATION,
                        "import.vault",
                        VAULT_GRAPH,
                        None,
                        &vault_incarnation,
                        json!({
                            "sourceType": "obsidian",
                            "pendingArchivePath": vault_pending.to_string_lossy()
                        }),
                    );
                    crate::operation_completion_ledger::fail_next_completion_append_for_test(
                        VAULT_OPERATION,
                    );
                    journal_enqueue_and_attempt_once_before_crash(&app, vault).await;

                    let archive_pending = stage_bytes(
                        &crate::profile_paths::profile_dir(&app).expect("profile dir"),
                        "graph-archive",
                        &minimal_graph_archive(),
                    );
                    retained_pending_paths.push(archive_pending.clone());
                    let archive = CrdtOperation {
                        operation_id: ARCHIVE_OPERATION.to_string(),
                        kind: "graph.importArchive".to_string(),
                        graph_id: ARCHIVE_GRAPH.to_string(),
                        document_id: None,
                        payload: json!({
                            "newGraphId": ARCHIVE_GRAPH,
                            "pendingArchivePath": archive_pending.to_string_lossy()
                        }),
                        enqueue_timestamp: "1720000000000".to_string(),
                    };
                    crate::operation_completion_ledger::fail_next_completion_append_for_test(
                        ARCHIVE_OPERATION,
                    );
                    journal_enqueue_and_attempt_once_before_crash(&app, archive).await;

                    let pending =
                        crate::crdt_operation_journal::recover_pending_crdt_operations(&app)
                            .expect("recover first-process journal");
                    assert_eq!(pending.len(), 6, "every injected failure remains retryable");
                    for expected in [
                        EDIT_OPERATION,
                        INGEST_OPERATION,
                        UPLOAD_OPERATION,
                        WEB_OPERATION,
                        VAULT_OPERATION,
                        ARCHIVE_OPERATION,
                    ] {
                        assert!(
                            pending.iter().any(|operation| operation.operation_id == expected),
                            "journal retained {expected}"
                        );
                    }
                    assert!(retained_pending_paths.iter().all(|path| path.is_file()));
                });
            }

            // A fresh app/registry/queue recovers solely from durable profile
            // state. No new user mutation or enqueue is allowed here.
            {
                let runtime = current_runtime();
                runtime.block_on(async {
                    let app = crate::tauri_runtime::build_mock_app_for_tests(true);
                    assert_eq!(
                        crate::crdt_queue::recover_crdt_operations(app.clone())
                            .expect("recover journal into fresh queue"),
                        6
                    );
                    drain_queue(app.clone()).await;
                    assert!(
                        crate::crdt_operation_journal::recover_pending_crdt_operations(&app)
                            .expect("read completed journal")
                            .is_empty(),
                        "fresh executor drains every recovered operation"
                    );

                    let edit = document(&app, EDIT_GRAPH, "edit-doc");
                    assert_eq!(edit.revision, 2, "comment replay increments only once");
                    let bytes = base64::engine::general_purpose::STANDARD
                        .decode(&edit.ydoc_update_base64)
                        .expect("decode comment Y.Doc");
                    let update = Update::decode_v1(&bytes).expect("decode comment update");
                    let comment_doc = Doc::new();
                    {
                        let mut txn = comment_doc.transact_mut();
                        txn.apply_update(update).expect("apply comment update");
                    }
                    {
                        let txn = comment_doc.transact();
                        let comments = txn.get_map("comments").expect("comments map");
                        assert!(
                            comments.contains_key(&txn, "comment-recovered"),
                            "comment remains in the durable Y.Doc"
                        );
                    }

                    let direct = document(&app, INGEST_GRAPH, "ingest-doc");
                    assert_eq!(direct.revision, 1);
                    assert!(direct.body.contains("Recovered directly"));
                    let upload = document(&app, UPLOAD_GRAPH, "upload-doc");
                    assert_eq!(upload.revision, 1);
                    assert!(upload.body.contains("Recovered upload"));
                    let web = document(&app, WEB_GRAPH, "web-doc");
                    assert_eq!(web.revision, 1);
                    assert!(web.body.contains("Browser content survives"));

                    let vault_documents = crate::document_service::list_documents(
                        app.clone(),
                        VAULT_GRAPH.to_string(),
                    )
                    .expect("list recovered vault documents");
                    assert_eq!(vault_documents.len(), 4);
                    assert!(vault_documents
                        .iter()
                        .all(|document| document.revision == 1));
                    let archived = document(&app, ARCHIVE_GRAPH, "archive-doc");
                    assert_eq!(archived.revision, 1);
                    assert!(archived.body.contains("Recovered archive content"));

                    for (operation_id, kind) in [
                        (INGEST_OPERATION, "document.ingestMarkdownOriginal"),
                        (UPLOAD_OPERATION, "document.uploadIngest"),
                        (VAULT_OPERATION, "import.vault"),
                        (ARCHIVE_OPERATION, "graph.importArchive"),
                    ] {
                        let entry = crate::operation_completion_ledger::completion_entry_for(
                            &app,
                            operation_id,
                        )
                        .expect("read completion ledger")
                        .expect("completion ledger entry");
                        assert_eq!(entry.kind, kind);
                    }
                    assert!(
                        crate::operation_completion_ledger::completion_entry_for(
                            &app,
                            &format!("{UPLOAD_OPERATION}-ingest")
                        )
                        .expect("lookup obsolete nested upload id")
                        .is_none(),
                        "upload owns one outer completion identity"
                    );
                });
            }

            assert!(
                retained_pending_paths.iter().all(|path| !path.exists()),
                "pending sources are removed only after Tier-B completion"
            );
        });

        std::env::remove_var("GARDEN_PROFILE_DIR");
        let _ = std::fs::remove_dir_all(&profile);
        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
    }

    #[test]
    fn ingest_boundaries_and_outer_ledgers_converge_without_revision_inflation() {
        let _serial = crate::tauri_runtime::profile_env_serial()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let profile = temp_profile();
        std::env::set_var("GARDEN_PROFILE_DIR", &profile);

        let result = std::panic::catch_unwind(|| {
            current_runtime().block_on(async {
                tokio::time::pause();
                let app = crate::tauri_runtime::build_mock_app_for_tests(true);
                for (index, failure) in [
                    Some(IngestFailurePoint::AfterOriginal),
                    Some(IngestFailurePoint::AfterWorkspace),
                    Some(IngestFailurePoint::AfterHot),
                    Some(IngestFailurePoint::AfterTail),
                    None,
                ]
                .into_iter()
                .enumerate()
                {
                    let graph_id = format!("ingest-boundary-{index}");
                    let operation_id = format!("op-ingest-boundary-{index}");
                    let document_id = format!("ingest-boundary-doc-{index}");
                    let incarnation = create_graph(&app, &graph_id);
                    let graph_dir = crate::graph_paths::existing_graph_dir(&app, &graph_id)
                        .expect("ingest boundary graph");
                    let pending = stage_bytes(
                        &graph_dir,
                        &format!("ingest-boundary-{index}"),
                        b"# Boundary\n\nOne durable revision.",
                    );
                    let operation = ordinary_operation(
                        &operation_id,
                        "document.ingestMarkdownOriginal",
                        &graph_id,
                        Some(&document_id),
                        &incarnation,
                        json!({
                            "documentId": document_id,
                            "filename": "boundary.md",
                            "mimeType": "text/markdown",
                            "markdown": "# Boundary\n\nOne durable revision.",
                            "pendingOriginalPath": pending.to_string_lossy()
                        }),
                    );
                    match failure {
                        Some(point) => {
                            crate::crdt_engine::document_ops::fail_next_ingest_step_for_test(
                                &operation_id,
                                point,
                            );
                        }
                        None => {
                            crate::operation_completion_ledger::fail_next_completion_append_for_test(
                                &operation_id,
                            );
                        }
                    }
                    assert!(matches!(
                        super::apply_operation_classified(&app, &operation).await,
                        Err(ApplyOperationError::RetryableAfterHotCommit(_))
                    ));
                    assert!(pending.is_file(), "pending retained at {failure:?}");
                    assert!(
                        crate::operation_completion_ledger::completion_entry_for(
                            &app,
                            &operation_id
                        )
                        .expect("lookup incomplete ingest")
                        .is_none()
                    );
                    super::apply_operation_classified(&app, &operation)
                        .await
                        .expect("retry direct ingest");
                    assert!(!pending.exists());
                    assert_eq!(
                        document(&app, &graph_id, &document_id).revision,
                        1,
                        "replay at {failure:?} must not manufacture a revision"
                    );
                }

                for (index, fail_ledger) in [false, true].into_iter().enumerate() {
                    let graph_id = format!("upload-boundary-{index}");
                    let operation_id = format!("op-upload-boundary-{index}");
                    let document_id = format!("upload-boundary-doc-{index}");
                    let incarnation = create_graph(&app, &graph_id);
                    let graph_dir = crate::graph_paths::existing_graph_dir(&app, &graph_id)
                        .expect("upload boundary graph");
                    let pending = stage_bytes(
                        &graph_dir,
                        &format!("upload-boundary-{index}"),
                        b"# Upload boundary\n\nOne durable revision.",
                    );
                    let operation = ordinary_operation(
                        &operation_id,
                        "document.uploadIngest",
                        &graph_id,
                        Some(&document_id),
                        &incarnation,
                        json!({
                            "documentId": document_id,
                            "filename": "upload-boundary.md",
                            "mimeType": "text/markdown",
                            "pendingOriginalPath": pending.to_string_lossy()
                        }),
                    );
                    if fail_ledger {
                        crate::operation_completion_ledger::fail_next_completion_append_for_test(
                            &operation_id,
                        );
                    } else {
                        crate::crdt_engine::document_ops::fail_next_ingest_step_for_test(
                            &operation_id,
                            IngestFailurePoint::AfterHot,
                        );
                    }
                    assert!(matches!(
                        super::apply_operation_classified(&app, &operation).await,
                        Err(ApplyOperationError::RetryableAfterHotCommit(_))
                    ));
                    assert!(pending.is_file());
                    super::apply_operation_classified(&app, &operation)
                        .await
                        .expect("retry upload ingest");
                    assert!(!pending.exists());
                    assert_eq!(document(&app, &graph_id, &document_id).revision, 1);
                    assert_eq!(
                        crate::operation_completion_ledger::completion_entry_for(
                            &app,
                            &operation_id
                        )
                        .expect("lookup upload completion")
                        .expect("upload completion")
                        .kind,
                        "document.uploadIngest"
                    );
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
    fn bulk_import_document_wire_and_ledger_failures_remain_retryable() {
        let _serial = crate::tauri_runtime::profile_env_serial()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let profile = temp_profile();
        std::env::set_var("GARDEN_PROFILE_DIR", &profile);

        let result = std::panic::catch_unwind(|| {
            current_runtime().block_on(async {
                tokio::time::pause();
                let app = crate::tauri_runtime::build_mock_app_for_tests(true);
                let vault_incarnation = create_graph(&app, "vault-step-boundaries");
                let vault_dir =
                    crate::graph_paths::existing_graph_dir(&app, "vault-step-boundaries")
                        .expect("vault boundary graph");
                let vault_pending = stage_bytes(
                    &vault_dir,
                    "vault-step-boundaries",
                    include_bytes!(concat!(
                        env!("CARGO_MANIFEST_DIR"),
                        "/tests/fixtures/vault_import/obsidian.zip"
                    )),
                );
                let vault = ordinary_operation(
                    "op-vault-step-boundaries",
                    "import.vault",
                    "vault-step-boundaries",
                    None,
                    &vault_incarnation,
                    json!({
                        "sourceType": "obsidian",
                        "pendingArchivePath": vault_pending.to_string_lossy()
                    }),
                );
                for point in [
                    crate::crdt_engine::import_vault_ops::VaultFailurePoint::BeforeDocument,
                    crate::crdt_engine::import_vault_ops::VaultFailurePoint::BeforeWire,
                ] {
                    crate::crdt_engine::import_vault_ops::fail_next_vault_step_for_test(
                        &vault.operation_id,
                        point,
                    );
                    assert!(matches!(
                        super::apply_operation_classified(&app, &vault).await,
                        Err(ApplyOperationError::RetryableAfterHotCommit(_))
                    ));
                    assert!(vault_pending.is_file());
                }
                crate::operation_completion_ledger::fail_next_completion_append_for_test(
                    &vault.operation_id,
                );
                assert!(matches!(
                    super::apply_operation_classified(&app, &vault).await,
                    Err(ApplyOperationError::RetryableAfterHotCommit(_))
                ));
                assert!(vault_pending.is_file());
                super::apply_operation_classified(&app, &vault)
                    .await
                    .expect("finish vault import");
                assert!(!vault_pending.exists());
                let vault_documents = crate::document_service::list_documents(
                    app.clone(),
                    "vault-step-boundaries".to_string(),
                )
                .expect("vault boundary documents");
                assert_eq!(vault_documents.len(), 4);
                assert!(vault_documents
                    .iter()
                    .all(|document| document.revision == 1));

                let archive_pending = stage_bytes(
                    &crate::profile_paths::profile_dir(&app).expect("profile dir"),
                    "archive-step-boundaries",
                    &minimal_graph_archive(),
                );
                let archive = CrdtOperation {
                    operation_id: "op-archive-step-boundaries".to_string(),
                    kind: "graph.importArchive".to_string(),
                    graph_id: "archive-step-boundaries".to_string(),
                    document_id: None,
                    payload: json!({
                        "newGraphId": "archive-step-boundaries",
                        "pendingArchivePath": archive_pending.to_string_lossy()
                    }),
                    enqueue_timestamp: "1720000000000".to_string(),
                };
                crate::crdt_engine::import_archive_ops::fail_next_archive_step_for_test(
                    &archive.operation_id,
                    ArchiveFailurePoint::BeforeDocument,
                );
                assert!(matches!(
                    super::apply_operation_classified(&app, &archive).await,
                    Err(ApplyOperationError::RetryableAfterHotCommit(_))
                ));
                assert!(archive_pending.is_file());
                crate::operation_completion_ledger::fail_next_completion_append_for_test(
                    &archive.operation_id,
                );
                assert!(matches!(
                    super::apply_operation_classified(&app, &archive).await,
                    Err(ApplyOperationError::RetryableAfterHotCommit(_))
                ));
                assert!(archive_pending.is_file());
                super::apply_operation_classified(&app, &archive)
                    .await
                    .expect("finish graph archive import");
                assert!(!archive_pending.exists());
                assert_eq!(
                    document(&app, "archive-step-boundaries", "archive-doc").revision,
                    1
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
    fn cell_restore_is_existing_empty_graph_only_and_exactly_replayable() {
        const GRAPH_ID: &str = "cell-restore-existing";
        const OWNER: &str = "test-owner";
        const OPERATION_ID: &str = "op-cell-restore-existing";
        const GENERATION: u64 = 7;

        let _serial = crate::tauri_runtime::profile_env_serial()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let profile = temp_profile();
        std::env::set_var("GARDEN_PROFILE_DIR", &profile);

        let result = std::panic::catch_unwind(|| {
            current_runtime().block_on(async {
                let app = crate::tauri_runtime::build_mock_cell_app_for_tests(
                    GRAPH_ID,
                    &format!("user:{OWNER}"),
                    GENERATION,
                );
                let incarnation = create_graph(&app, GRAPH_ID);
                let (_, graph_before) = crate::graph_service::read_graph_record(&app, GRAPH_ID)
                    .expect("read pre-restore graph");
                let archive = minimal_graph_archive_for(OWNER, GRAPH_ID);
                let archive_sha256 = format!("{:x}", Sha256::digest(&archive));
                let pending = stage_bytes(
                    &crate::graph_paths::existing_graph_dir(&app, GRAPH_ID).expect("graph dir"),
                    "cell-restore-existing",
                    &archive,
                );
                let operation = ordinary_operation(
                    OPERATION_ID,
                    "graph.restoreArchive",
                    GRAPH_ID,
                    None,
                    &incarnation,
                    json!({
                        "newGraphId": GRAPH_ID,
                        "pendingArchivePath": pending.to_string_lossy(),
                        "archiveSha256": archive_sha256,
                        "sourceGraphId": GRAPH_ID,
                        "sourceUserId": OWNER,
                        "targetGeneration": GENERATION,
                        "planDigest": "d".repeat(64),
                        "expectedDocumentCount": 1,
                        "expectedRdfTripleCount": 0,
                        "includesArtifacts": false,
                    }),
                );

                crate::crdt_engine::import_archive_ops::fail_next_archive_step_for_test(
                    OPERATION_ID, crate::crdt_engine::import_archive_ops::ArchiveFailurePoint::BeforeDocument);
                journal_enqueue_and_attempt_once_before_crash(&app, operation.clone()).await;
                assert!(pending.exists());
                drop(app);
                let app = crate::tauri_runtime::build_mock_cell_app_for_tests(GRAPH_ID,&format!("user:{OWNER}"),GENERATION);
                assert_eq!(crate::crdt_queue::recover_crdt_operations(app.clone()).unwrap(),1);
                super::drain_queue(app.clone()).await;
                assert!(crate::crdt_operation_journal::recover_pending_crdt_operations(&app).unwrap().is_empty());
                let restored = super::apply_operation_classified(&app, &operation).await.expect("completed recovered restore");
                assert_eq!(restored["restoredExistingGraph"], json!(true));
                assert_eq!(restored["targetGeneration"], json!(GENERATION));
                assert_eq!(restored["documentCount"], json!(1));
                assert!(!pending.exists());
                assert_eq!(
                    document(&app, GRAPH_ID, "archive-doc").revision,
                    1,
                    "restore writes the archived document exactly once",
                );
                let (_, graph_after) = crate::graph_service::read_graph_record(&app, GRAPH_ID)
                    .expect("read post-restore graph");
                assert_eq!(graph_after.incarnation_id, graph_before.incarnation_id);
                assert_eq!(graph_after.created_at, graph_before.created_at);

                let replay = super::apply_operation_classified(&app, &operation)
                    .await
                    .expect("exact completion-ledger replay");
                assert_eq!(replay["replayed"], json!(true));
                assert_eq!(document(&app, GRAPH_ID, "archive-doc").revision, 1);

                let conflicting = ordinary_operation(
                    "op-cell-restore-conflict",
                    "graph.restoreArchive",
                    GRAPH_ID,
                    None,
                    &incarnation,
                    json!({
                        "newGraphId": GRAPH_ID,
                        "tarGzBase64": base64::engine::general_purpose::STANDARD.encode(&archive),
                        "archiveSha256": format!("{:x}", Sha256::digest(&archive)),
                        "sourceGraphId": GRAPH_ID,
                        "sourceUserId": OWNER,
                        "targetGeneration": GENERATION,
                        "planDigest": "e".repeat(64),
                        "expectedDocumentCount": 1,
                        "expectedRdfTripleCount": 0,
                        "includesArtifacts": false,
                    }),
                );
                assert!(matches!(
                    super::apply_operation_classified(&app, &conflicting).await,
                    Err(ApplyOperationError::Terminal(_))
                ));
                assert_eq!(document(&app, GRAPH_ID, "archive-doc").revision, 1);
            });
        });

        std::env::remove_var("GARDEN_PROFILE_DIR");
        let _ = std::fs::remove_dir_all(&profile);
        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
    }

    #[test]
    fn edit_comment_validation_is_terminal_before_hot_commit() {
        let operation = CrdtOperation {
            operation_id: "terminal-edit-comment".to_string(),
            kind: "document.editComment".to_string(),
            graph_id: "unused".to_string(),
            document_id: Some("unused".to_string()),
            payload: json!({
                "action": "not-an-action",
                "commentId": "comment",
                "updatedAt": 1,
            }),
            enqueue_timestamp: "1".to_string(),
        };
        let profile = temp_profile();
        let _serial = crate::tauri_runtime::profile_env_serial()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        std::env::set_var("GARDEN_PROFILE_DIR", &profile);
        let result = std::panic::catch_unwind(|| {
            current_runtime().block_on(async {
                let app = crate::tauri_runtime::build_mock_app_for_tests(true);
                assert!(matches!(
                    crate::crdt_engine::document_ops::edit_comment_classified(&app, &operation)
                        .await,
                    Err(ApplyOperationError::Terminal(message))
                        if message.contains("action must be set, resolve, or delete")
                ));
            });
        });
        std::env::remove_var("GARDEN_PROFILE_DIR");
        let _ = std::fs::remove_dir_all(&profile);
        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
    }
}
