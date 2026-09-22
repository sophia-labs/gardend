use crate::app_runtime::AppHandle;
use crate::{
    artifact_ingest_service::local_ingest_artifact,
    clock::timestamp,
    crdt_projection_flush::{
        flush_document_projection, prefers_synchronous_flush,
        spawn_deferred_document_projection_flush,
    },
    crdt_queue::{enqueue_crdt_operation, EnqueueCrdtOperationInput},
    document_projection_service::hosted_document_response,
    hosted_navigation_projection::hosted_navigation_parts,
    json_utils::json_string,
    local_jobs::{local_job_submit_response, LocalJobProgress, LocalJobRecord},
    loopback_http::{loopback_error, require_loopback_scope},
    loopback_state::LoopbackState,
};
use axum::{
    extract::{Path as AxumPath, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ArtifactImportResponse {
    document_id: String,
    title: String,
    read_only: bool,
}

// `artifact` and `document` are the raw values returned by the underlying
// CRDT operations — typed as `serde_json::Value` because each operation kind
// has a different result shape and this route just forwards them through.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ArtifactConvertEnvelope {
    document_id: String,
    title: String,
    artifact: serde_json::Value,
    document: serde_json::Value,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ArtifactConvertResponse {
    document_id: serde_json::Value,
    title: serde_json::Value,
}

#[derive(Debug, Serialize)]
struct ArtifactImportJobResponseBody {
    #[serde(flatten)]
    submit: crate::local_jobs::LocalJobSubmitResponse,
    #[serde(rename = "jobId")]
    job_id: String,
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub(super) struct ArtifactImportInput {
    #[serde(default)]
    title: Option<String>,
    #[serde(default, alias = "parent_id")]
    parent_id: Option<String>,
    #[serde(default, alias = "read_only")]
    read_only: bool,
    #[serde(default = "default_true", alias = "use_ydoc_path")]
    use_ydoc_path: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ArtifactConvertInput {
    #[serde(alias = "document_id")]
    document_id: String,
    #[serde(default)]
    title: Option<String>,
}

fn default_true() -> bool {
    true
}

fn artifact_import_error_response(error: &str) -> Response {
    if error.contains("not found") {
        loopback_error(StatusCode::NOT_FOUND, error)
    } else if error.contains("Empty artifact file") || error.contains("no stored file") {
        loopback_error(StatusCode::BAD_REQUEST, error)
    } else if error.contains("ingest") || error.contains("parse") || error.contains("unsupported") {
        loopback_error(StatusCode::UNPROCESSABLE_ENTITY, error)
    } else {
        loopback_error(StatusCode::BAD_REQUEST, error)
    }
}

fn artifact_import_response_value(
    value: &serde_json::Value,
    read_only: bool,
) -> Result<ArtifactImportResponse, String> {
    let document_id = json_string(value.get("documentId").or_else(|| value.get("document_id")))
        .ok_or_else(|| "artifact import did not return a documentId".to_string())?;
    let title = json_string(value.get("title"))
        .or_else(|| {
            value
                .get("document")
                .and_then(|document| json_string(document.get("title")))
        })
        .unwrap_or_else(|| document_id.clone());
    Ok(ArtifactImportResponse {
        document_id,
        title,
        read_only,
    })
}

fn prefers_sync_work(headers: &HeaderMap) -> bool {
    headers
        .get("prefer")
        .and_then(|value| value.to_str().ok())
        .map(|value| {
            value
                .split(',')
                .any(|part| part.trim().eq_ignore_ascii_case("respond-sync"))
        })
        .unwrap_or(false)
}

fn artifact_import_job_response(record: &LocalJobRecord) -> ArtifactImportJobResponseBody {
    ArtifactImportJobResponseBody {
        submit: local_job_submit_response(record),
        job_id: record.job_id.clone(),
    }
}

fn report_artifact_import_progress(
    state: &Arc<LoopbackState>,
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
    let _ = state.jobs.update_progress(
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

fn spawn_artifact_import_job(
    state: Arc<LoopbackState>,
    job_id: String,
    graph_id: String,
    artifact_id: String,
    mode: String,
    title: Option<String>,
    parent_id: Option<String>,
    read_only: bool,
) {
    tokio::spawn(async move {
        if state.jobs.is_cancelled(&job_id).unwrap_or(false) {
            return;
        }
        let _ = state.jobs.mark_running(&job_id);
        report_artifact_import_progress(
            &state,
            &job_id,
            "running",
            "Importing stored artifact",
            1,
            2,
            serde_json::json!({
                "graphId": graph_id.clone(),
                "artifactId": artifact_id.clone(),
                "mode": mode.clone(),
            }),
        );
        let result = local_ingest_artifact(
            state.app.clone(),
            graph_id,
            artifact_id,
            mode,
            title,
            parent_id,
            read_only,
            false,
        )
        .await
        .and_then(|value| artifact_import_response_value(&value, read_only))
        .and_then(|response| {
            serde_json::to_value(response)
                .map_err(|error| format!("serialize artifact import response: {error}"))
        });
        if state.jobs.is_cancelled(&job_id).unwrap_or(false) {
            return;
        }
        if result.is_ok() {
            report_artifact_import_progress(
                &state,
                &job_id,
                "complete",
                "Stored artifact import complete",
                2,
                2,
                serde_json::json!({}),
            );
        }
        let _ = state
            .jobs
            .finish_existing(&job_id, result, "application/json");
    });
}

pub(super) async fn loopback_hosted_import_artifact(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    AxumPath((graph_id, artifact_id)): AxumPath<(String, String)>,
    body: Option<Json<ArtifactImportInput>>,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "artifacts.ingest") {
        return response;
    }
    let input = body.map(|Json(input)| input).unwrap_or_default();
    let read_only = input.read_only;
    let mode = if read_only { "ingest" } else { "import" }.to_string();
    let _use_ydoc_path = input.use_ydoc_path;
    if prefers_sync_work(&headers) {
        return match local_ingest_artifact(
            state.app.clone(),
            graph_id,
            artifact_id,
            mode,
            input.title,
            input.parent_id,
            read_only,
            false,
        )
        .await
        {
            Ok(value) => match artifact_import_response_value(&value, read_only) {
                Ok(response) => (StatusCode::CREATED, Json(response)).into_response(),
                Err(error) => artifact_import_error_response(&error),
            },
            Err(error) => artifact_import_error_response(&error),
        };
    }
    let detail = serde_json::json!({
        "graphId": graph_id.clone(),
        "artifactId": artifact_id.clone(),
        "mode": mode.clone(),
        "readOnly": read_only,
        "parentId": input.parent_id.clone(),
        "asyncWork": true,
    });
    let record = match state
        .jobs
        .insert_queued("artifact_import", Some(graph_id.clone()), detail)
    {
        Ok(record) => record,
        // TODO(c2-handoff): C3 owns this file; switch to loopback_app_error(error) when reconciling.
        Err(error) => {
            return loopback_error(StatusCode::INTERNAL_SERVER_ERROR, error.message_ref())
        }
    };
    spawn_artifact_import_job(
        state,
        record.job_id.clone(),
        graph_id,
        artifact_id,
        mode,
        input.title,
        input.parent_id,
        read_only,
    );
    (
        StatusCode::ACCEPTED,
        Json(artifact_import_job_response(&record)),
    )
        .into_response()
}

async fn local_convert_artifact(
    app: AppHandle,
    graph_id: String,
    artifact_id: String,
    input: ArtifactConvertInput,
    wait_for_flush: bool,
) -> Result<serde_json::Value, String> {
    let (_, _, artifacts) = hosted_navigation_parts(&app, &graph_id)?;
    let artifact = artifacts
        .into_iter()
        .find(|artifact| json_string(artifact.get("id")).as_deref() == Some(artifact_id.as_str()))
        .ok_or_else(|| format!("artifact {artifact_id} not found"))?;
    let _document = hosted_document_response(&app, &graph_id, &input.document_id)?;

    let artifact_value = enqueue_crdt_operation(
        app.clone(),
        EnqueueCrdtOperationInput {
            kind: "workspace.putArtifact".to_string(),
            graph_id: graph_id.clone(),
            document_id: Some(artifact_id.clone()),
            payload: serde_json::json!({
                "artifactId": artifact_id.clone(),
                "label": json_string(artifact.get("label")),
                "parentId": json_string(artifact.get("parentId")),
                "order": artifact.get("order").cloned().unwrap_or(serde_json::Value::Null),
                "fileType": json_string(artifact.get("fileType")),
                "status": json_string(artifact.get("status")),
                "storageKey": json_string(artifact.get("storageKey")),
                "originalFilename": json_string(artifact.get("originalFilename")),
                "mimeType": json_string(artifact.get("mimeType")),
                "sizeBytes": artifact.get("sizeBytes").cloned().unwrap_or(serde_json::Value::Null),
                "ingestedDocId": serde_json::Value::Null,
            }),
        },
    )
    .await?;
    let update_value = enqueue_crdt_operation(
        app.clone(),
        EnqueueCrdtOperationInput {
            kind: "workspace.updateDocument".to_string(),
            graph_id: graph_id.clone(),
            document_id: Some(input.document_id.clone()),
            payload: serde_json::json!({
                "documentId": input.document_id.clone(),
                "readOnly": false,
            }),
        },
    )
    .await?;
    if wait_for_flush {
        flush_document_projection(app, &graph_id, &input.document_id).await?;
    } else {
        spawn_deferred_document_projection_flush(
            app,
            graph_id.clone(),
            input.document_id.clone(),
            "artifacts.convert",
        );
    }
    let title = input
        .title
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "Converted Document".to_string());
    let envelope = ArtifactConvertEnvelope {
        document_id: input.document_id.clone(),
        title,
        artifact: artifact_value,
        document: update_value,
    };
    serde_json::to_value(envelope)
        .map_err(|error| format!("serialize artifact convert envelope: {error}"))
}

pub(super) async fn loopback_hosted_convert_artifact(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    AxumPath((graph_id, artifact_id)): AxumPath<(String, String)>,
    Json(input): Json<ArtifactConvertInput>,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "artifacts.ingest") {
        return response;
    }
    let wait_for_flush = prefers_synchronous_flush(&headers);
    match local_convert_artifact(
        state.app.clone(),
        graph_id,
        artifact_id,
        input,
        wait_for_flush,
    )
    .await
    {
        Ok(value) => {
            let response = ArtifactConvertResponse {
                document_id: value
                    .get("documentId")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null),
                title: value
                    .get("title")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null),
            };
            Json(response).into_response()
        }
        Err(error) => artifact_import_error_response(&error),
    }
}
