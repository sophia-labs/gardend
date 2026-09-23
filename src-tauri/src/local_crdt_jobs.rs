use crate::app_runtime::AppHandle;
use crate::{
    clock::timestamp,
    crdt_projection_flush::{flush_document_projection, flush_graph_projection},
    crdt_queue::{enqueue_crdt_operation, EnqueueCrdtOperationInput},
    local_jobs::{LocalJobProgress, LocalJobRecord, LocalJobRegistry},
    loopback_state::LoopbackState,
    paths::cleanup_pending_upload_file,
};
use std::{path::PathBuf, sync::Arc};

pub(crate) type LocalCrdtJobResultMapper =
    Box<dyn FnOnce(serde_json::Value) -> Result<serde_json::Value, String> + Send + 'static>;

pub(crate) struct LocalCrdtJobInput {
    pub(crate) job_type: String,
    pub(crate) graph_id: String,
    pub(crate) operation_kind: String,
    pub(crate) document_id: Option<String>,
    pub(crate) payload: serde_json::Value,
    pub(crate) detail: serde_json::Value,
    pub(crate) pending_cleanup_path: Option<PathBuf>,
    pub(crate) running_message: String,
    pub(crate) success_message: String,
    pub(crate) result_mapper: Option<LocalCrdtJobResultMapper>,
}

pub(crate) fn insert_and_spawn_crdt_job(
    state: Arc<LoopbackState>,
    input: LocalCrdtJobInput,
) -> Result<LocalJobRecord, String> {
    insert_and_spawn_crdt_job_for(state.app.clone(), state.jobs.clone(), input)
}

pub(crate) fn insert_and_spawn_crdt_job_for(
    app: AppHandle,
    jobs: Arc<LocalJobRegistry>,
    input: LocalCrdtJobInput,
) -> Result<LocalJobRecord, String> {
    let record = match jobs.insert_queued(
        &input.job_type,
        Some(input.graph_id.clone()),
        input.detail.clone(),
    ) {
        Ok(record) => record,
        Err(error) => {
            cleanup_optional_path(&input.pending_cleanup_path);
            return Err(error.into());
        }
    };
    spawn_crdt_job(app, jobs, record.job_id.clone(), input);
    Ok(record)
}

fn spawn_crdt_job(
    app: AppHandle,
    jobs: Arc<LocalJobRegistry>,
    job_id: String,
    input: LocalCrdtJobInput,
) {
    tokio::spawn(async move {
        let LocalCrdtJobInput {
            job_type: _,
            graph_id,
            operation_kind,
            document_id,
            payload,
            detail: _,
            pending_cleanup_path,
            running_message,
            success_message,
            result_mapper,
        } = input;
        if jobs.is_cancelled(&job_id).unwrap_or(false) {
            cleanup_optional_path(&pending_cleanup_path);
            return;
        }
        let _ = jobs.mark_running(&job_id);
        report_crdt_job_progress(
            &jobs,
            &job_id,
            "running",
            &running_message,
            1,
            2,
            serde_json::json!({
                "operationKind": operation_kind.clone(),
                "documentId": document_id.clone(),
            }),
        );
        let operation_kind_for_flush = operation_kind.clone();
        let result = match enqueue_crdt_operation(
            app.clone(),
            EnqueueCrdtOperationInput {
                kind: operation_kind,
                graph_id: graph_id.clone(),
                document_id: document_id.clone(),
                payload,
            },
        )
        .await
        {
            Ok(value) => match flush_after_crdt_job(
                app.clone(),
                &operation_kind_for_flush,
                &graph_id,
                document_id.as_deref(),
            )
            .await
            {
                Ok(()) => match result_mapper {
                    Some(mapper) => mapper(value),
                    None => Ok(value),
                },
                Err(error) => Err(error),
            },
            Err(error) => Err(error),
        };
        if let Err(error) = &result {
            // A caller-timeout is not operation death: the op remains
            // durably queued and its background retries still need the
            // staged archive (deleting it here doomed every retry of the
            // maiden graph import, 2026-08-29). Cleanup for the surviving
            // op belongs to the executor's success/ledger paths.
            if error != crate::crdt_queue::CRDT_OPERATION_STILL_QUEUED_ERROR {
                cleanup_optional_path(&pending_cleanup_path);
            }
        }
        if jobs.is_cancelled(&job_id).unwrap_or(false) {
            cleanup_optional_path(&pending_cleanup_path);
            return;
        }
        if result.is_ok() {
            report_crdt_job_progress(
                &jobs,
                &job_id,
                "complete",
                &success_message,
                2,
                2,
                serde_json::json!({}),
            );
        }
        let _ = jobs.finish_existing(&job_id, result, "application/json");
    });
}

async fn flush_after_crdt_job(
    app: AppHandle,
    operation_kind: &str,
    graph_id: &str,
    document_id: Option<&str>,
) -> Result<(), String> {
    match operation_kind {
        "document.uploadIngest" | "document.write" | "import.webClip" => {
            if let Some(document_id) = document_id {
                flush_document_projection(app, graph_id, document_id).await
            } else {
                flush_graph_projection(app, graph_id).await
            }
        }
        "document.batchRegister"
        | "import.vault"
        | "graph.importArchive"
        | "graph.restoreArchive"
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
        | "workspace.deleteWire" => flush_graph_projection(app, graph_id).await,
        _ => Ok(()),
    }
}

fn report_crdt_job_progress(
    jobs: &Arc<LocalJobRegistry>,
    job_id: &str,
    phase: &str,
    message: &str,
    current: usize,
    total: usize,
    details: serde_json::Value,
) {
    let total = total.max(current);
    let percent = if total == 0 {
        0.0
    } else {
        (current as f64 / total as f64) * 100.0
    };
    let _ = jobs.update_progress(
        job_id,
        LocalJobProgress {
            phase: phase.to_string(),
            message: message.to_string(),
            current,
            total,
            percent,
            updated_at: timestamp(),
            details,
        },
    );
}

fn cleanup_optional_path(path: &Option<PathBuf>) {
    if let Some(path) = path {
        cleanup_pending_upload_file(path);
    }
}
