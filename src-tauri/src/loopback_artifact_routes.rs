use crate::{
    app_error::AppError,
    artifact_revisions::{
        create_artifact_revision, list_artifact_revisions, read_artifact_revision,
        restore_artifact_revision,
    },
    crdt_queue::{enqueue_crdt_operation_outcome, EnqueueCrdtOperationInput},
    ids::safe_filename,
    local_crdt_jobs::{insert_and_spawn_crdt_job, LocalCrdtJobInput},
    local_jobs::{local_job_submit_response, LocalJobRecord},
    loopback_artifact_ingest_routes::{
        loopback_hosted_convert_artifact, loopback_hosted_import_artifact,
    },
    loopback_batch_routes::{loopback_hosted_batch_prepare, loopback_hosted_batch_register},
    loopback_http::{loopback_error, loopback_original_file_error, require_loopback_scope},
    loopback_image_routes::{loopback_hosted_serve_image, loopback_hosted_upload_image},
    loopback_ingestion_routes::loopback_ingestion_config_router,
    loopback_pdf_ingest_routes::loopback_hosted_ingest_pdf_accurate,
    loopback_state::LoopbackState,
    multipart_pending_upload::stream_field_to_pending_upload,
    original_file_service::{original_file_download_response, read_artifact_original_file},
    paths::{cleanup_pending_upload_file, existing_graph_dir},
    runtime_config::LOCAL_UPLOAD_MAX_BYTES,
    storage::display_path,
};
use axum::{
    extract::{Multipart, Path as AxumPath, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use std::sync::Arc;
use uuid::Uuid;

pub(super) fn loopback_artifact_router() -> Router<Arc<LoopbackState>> {
    Router::new()
        .route(
            "/artifacts/{graph_id}/{artifact_id}/text",
            get(loopback_read_artifact_text)
                .post(loopback_create_artifact_text)
                .put(loopback_write_artifact_text),
        )
        .route(
            "/artifacts/{graph_id}/{artifact_id}/text/restore",
            post(loopback_restore_artifact_text),
        )
        .merge(loopback_ingestion_config_router())
        .route(
            "/artifacts/{graph_id}/batch/prepare",
            post(loopback_hosted_batch_prepare),
        )
        .route(
            "/artifacts/{graph_id}/batch/register",
            post(loopback_hosted_batch_register),
        )
        .route(
            "/artifacts/{graph_id}/upload",
            post(loopback_hosted_upload_artifact),
        )
        .route(
            "/artifacts/{graph_id}/ingest/pdf-accurate",
            post(loopback_hosted_ingest_pdf_accurate),
        )
        .route(
            "/artifacts/{graph_id}/images/upload",
            post(loopback_hosted_upload_image),
        )
        .route(
            "/artifacts/{graph_id}/images/{image_id}",
            get(loopback_hosted_serve_image),
        )
        .route(
            "/artifacts/{graph_id}/{artifact_id}/download",
            get(loopback_hosted_download_artifact),
        )
        .route(
            "/artifacts/{graph_id}/{artifact_id}/revisions",
            get(loopback_list_artifact_revisions).post(loopback_create_artifact_revision),
        )
        .route(
            "/artifacts/{graph_id}/{artifact_id}/revisions/{revision_id}/download",
            get(loopback_download_artifact_revision),
        )
        .route(
            "/artifacts/{graph_id}/{artifact_id}/revisions/{revision_id}/restore",
            post(loopback_restore_artifact_revision),
        )
        .route(
            "/artifacts/{graph_id}/{artifact_id}/import",
            post(loopback_hosted_import_artifact),
        )
        .route(
            "/artifacts/{graph_id}/{artifact_id}/convert",
            post(loopback_hosted_convert_artifact),
        )
}

pub(super) async fn loopback_read_artifact_text(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    AxumPath((graph, id)): AxumPath<(String, String)>,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "artifacts.read") {
        return response;
    }
    let mut response = match crate::artifact_text_service::read_text(&state.app, &graph, &id).await
    {
        Ok(value) => Json(value).into_response(),
        Err(error) => crate::loopback_http::loopback_app_error(error),
    };
    response.headers_mut().insert(
        axum::http::header::CACHE_CONTROL,
        axum::http::HeaderValue::from_static("no-store"),
    );
    response
}
async fn text_mutation_route(
    state: Arc<LoopbackState>,
    headers: HeaderMap,
    graph: String,
    id: String,
    mut value: serde_json::Value,
    mode: &str,
) -> Response {
    if let Err(response) = crate::loopback_http::require_loopback_scopes(
        &headers,
        &state,
        &["artifacts.write", "workspace.write.crdt"],
    ) {
        return response;
    }
    let Some(object) = value.as_object_mut() else {
        return loopback_error(
            StatusCode::BAD_REQUEST,
            "text mutation body must be an object",
        );
    };
    if object
        .get("artifactId")
        .is_some_and(|v| v.as_str() != Some(id.as_str()))
    {
        return loopback_error(StatusCode::BAD_REQUEST, "artifact ID carriers disagree");
    }
    object.insert("artifactId".into(), serde_json::json!(id));
    object.insert("mode".into(), serde_json::json!(mode));
    let input = match serde_json::from_value::<crate::artifact_text_service::TextMutation>(value) {
        Ok(v) => v,
        Err(e) => return loopback_error(StatusCode::BAD_REQUEST, &e.to_string()),
    };
    match crate::artifact_text_service::submit(state.app.clone(), graph, input).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => crate::loopback_http::loopback_app_error(e),
    }
}
pub(super) async fn loopback_create_artifact_text(
    State(s): State<Arc<LoopbackState>>,
    h: HeaderMap,
    AxumPath((g, a)): AxumPath<(String, String)>,
    Json(v): Json<serde_json::Value>,
) -> Response {
    text_mutation_route(s, h, g, a, v, "create").await
}
pub(super) async fn loopback_write_artifact_text(
    State(s): State<Arc<LoopbackState>>,
    h: HeaderMap,
    AxumPath((g, a)): AxumPath<(String, String)>,
    Json(v): Json<serde_json::Value>,
) -> Response {
    text_mutation_route(s, h, g, a, v, "write").await
}
pub(super) async fn loopback_restore_artifact_text(
    State(s): State<Arc<LoopbackState>>,
    h: HeaderMap,
    AxumPath((g, a)): AxumPath<(String, String)>,
    Json(v): Json<serde_json::Value>,
) -> Response {
    text_mutation_route(s, h, g, a, v, "restore").await
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

fn artifact_upload_job_response(
    record: &LocalJobRecord,
    document_id: &str,
) -> Result<serde_json::Value, String> {
    let mut value = serde_json::to_value(local_job_submit_response(record))
        .map_err(|error| format!("serialize local artifact upload job response: {error}"))?;
    if let Some(object) = value.as_object_mut() {
        object.insert(
            "jobId".to_string(),
            serde_json::Value::String(record.job_id.clone()),
        );
        object.insert(
            "documentId".to_string(),
            serde_json::Value::String(document_id.to_string()),
        );
        object.insert(
            "document_id".to_string(),
            serde_json::Value::String(document_id.to_string()),
        );
    }
    Ok(value)
}

pub(super) async fn loopback_hosted_upload_artifact(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    AxumPath(graph_id): AxumPath<String>,
    mut multipart: Multipart,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "artifacts.write") {
        return response;
    }

    let mut filename: Option<String> = None;
    let mut mime_type: Option<String> = None;
    let graph_dir = match existing_graph_dir(&state.app, &graph_id) {
        Ok(graph_dir) => graph_dir,
        Err(error) => return loopback_error(StatusCode::NOT_FOUND, &error),
    };
    let mut pending_original_path: Option<std::path::PathBuf> = None;
    let mut size_bytes: usize = 0;
    let mut parent_id: Option<String> = None;
    let mut batch_id: Option<String> = None;

    loop {
        let field = match multipart.next_field().await {
            Ok(Some(field)) => field,
            Ok(None) => break,
            Err(error) => {
                return loopback_error(
                    StatusCode::BAD_REQUEST,
                    &format!("invalid multipart upload: {error}"),
                )
            }
        };

        let name = field.name().map(str::to_string).unwrap_or_default();
        if name == "file" {
            let field_filename = field
                .file_name()
                .map(str::to_string)
                .unwrap_or_else(|| "unnamed".to_string());
            let field_mime_type = field.content_type().map(ToString::to_string);
            let upload = match stream_field_to_pending_upload(
                &graph_dir,
                field,
                LOCAL_UPLOAD_MAX_BYTES,
                "failed to read uploaded file",
            )
            .await
            {
                Ok(upload) => upload,
                Err(error) => return loopback_error(error.status, &error.message),
            };
            if let Some(previous_path) = pending_original_path.replace(upload.path) {
                cleanup_pending_upload_file(&previous_path);
            }
            filename = Some(field_filename);
            mime_type = field_mime_type;
            size_bytes = upload.bytes_written;
            continue;
        }

        let text = match field.text().await {
            Ok(value) => value.trim().to_string(),
            Err(error) => {
                return loopback_error(
                    StatusCode::BAD_REQUEST,
                    &format!("failed to read multipart field {name}: {error}"),
                )
            }
        };
        if text.is_empty() {
            continue;
        }
        match name.as_str() {
            "parent_id" | "parentId" => parent_id = Some(text),
            "batch_id" | "batchId" => batch_id = Some(text),
            _ => {}
        }
    }

    let Some(pending_original_path) = pending_original_path else {
        return loopback_error(StatusCode::BAD_REQUEST, "missing multipart file field");
    };
    if size_bytes == 0 {
        cleanup_pending_upload_file(&pending_original_path);
        return loopback_error(StatusCode::BAD_REQUEST, "Empty file uploaded");
    }
    let filename = filename.unwrap_or_else(|| "unnamed".to_string());
    let mime_type = mime_type.unwrap_or_else(|| "application/octet-stream".to_string());
    let mut payload = serde_json::json!({
        "filename": filename,
        "mimeType": mime_type,
        "sizeBytes": size_bytes,
        "pendingOriginalPath": display_path(&pending_original_path),
        "parentId": parent_id,
        "batchId": batch_id,
    });

    if prefers_sync_work(&headers) {
        let outcome = enqueue_crdt_operation_outcome(
            state.app.clone(),
            EnqueueCrdtOperationInput {
                kind: "document.uploadIngest".to_string(),
                graph_id,
                document_id: None,
                payload,
            },
        )
        .await;
        return match outcome {
            Ok(outcome) => (StatusCode::CREATED, Json(outcome.value)).into_response(),
            Err(error) => {
                cleanup_pending_upload_file(&pending_original_path);
                loopback_error(StatusCode::UNPROCESSABLE_ENTITY, &error)
            }
        };
    }

    let document_id = format!("doc-{}", Uuid::new_v4().simple());
    let payload_filename = payload
        .get("filename")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let payload_mime_type = payload
        .get("mimeType")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let payload_parent_id = payload
        .get("parentId")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let payload_batch_id = payload
        .get("batchId")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let source_filename = payload_filename.as_str().unwrap_or("unnamed");
    let detail_graph_id = graph_id.clone();
    if let Some(object) = payload.as_object_mut() {
        object.insert(
            "documentId".to_string(),
            serde_json::Value::String(document_id.clone()),
        );
        object.insert(
            "sourceFile".to_string(),
            serde_json::json!({
                "storageKey": format!(
                    "local://documents/{document_id}/original/{}",
                    safe_filename(source_filename)
                ),
                "originalFilename": payload_filename.clone(),
                "mimeType": payload_mime_type.clone(),
                "sizeBytes": size_bytes,
            }),
        );
    }
    let record = match insert_and_spawn_crdt_job(
        state,
        LocalCrdtJobInput {
            job_type: "artifact_upload".to_string(),
            graph_id,
            operation_kind: "document.uploadIngest".to_string(),
            document_id: Some(document_id.clone()),
            detail: serde_json::json!({
                "graphId": detail_graph_id,
                "documentId": document_id.clone(),
                "filename": payload_filename,
                "mimeType": payload_mime_type,
                "sizeBytes": size_bytes,
                "parentId": payload_parent_id,
                "batchId": payload_batch_id,
                "asyncWork": true,
            }),
            payload,
            pending_cleanup_path: Some(pending_original_path),
            running_message: "Ingesting uploaded artifact".to_string(),
            success_message: "Artifact upload import complete".to_string(),
            result_mapper: None,
        },
    ) {
        Ok(record) => record,
        Err(error) => return loopback_error(StatusCode::INTERNAL_SERVER_ERROR, &error),
    };
    match artifact_upload_job_response(&record, &document_id) {
        Ok(value) => (StatusCode::ACCEPTED, Json(value)).into_response(),
        Err(error) => loopback_error(StatusCode::INTERNAL_SERVER_ERROR, &error),
    }
}

pub(super) async fn loopback_hosted_download_artifact(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    AxumPath((graph_id, artifact_id)): AxumPath<(String, String)>,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "artifacts.read") {
        return response;
    }
    match read_artifact_original_file(&state.app, &graph_id, &artifact_id).and_then(
        |(manifest, bytes)| {
            original_file_download_response(&manifest, bytes, false).map_err(AppError::storage)
        },
    ) {
        Ok(response) => response,
        Err(error) => loopback_original_file_error(error.message_ref()),
    }
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct CreateArtifactRevisionInput {
    data_base64: String,
    mime_type: String,
    #[serde(default)]
    filename: Option<String>,
    #[serde(default)]
    label: Option<String>,
}

pub(super) async fn loopback_create_artifact_revision(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    AxumPath((graph_id, artifact_id)): AxumPath<(String, String)>,
    Json(input): Json<CreateArtifactRevisionInput>,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "artifacts.write") {
        return response;
    }
    // Preserve the artifact's existing filename across edits when the client
    // doesn't supply one.
    let filename = input.filename.unwrap_or_else(|| {
        read_artifact_original_file(&state.app, &graph_id, &artifact_id)
            .map(|(manifest, _)| manifest.filename)
            .unwrap_or_else(|_| format!("{artifact_id}.png"))
    });
    match create_artifact_revision(
        &state.app,
        &graph_id,
        &artifact_id,
        &filename,
        &input.mime_type,
        &input.data_base64,
        input.label,
    ) {
        Ok(entry) => (StatusCode::CREATED, Json(entry)).into_response(),
        Err(error) => loopback_original_file_error(error.message_ref()),
    }
}

pub(super) async fn loopback_list_artifact_revisions(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    AxumPath((graph_id, artifact_id)): AxumPath<(String, String)>,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "artifacts.read") {
        return response;
    }
    match list_artifact_revisions(&state.app, &graph_id, &artifact_id) {
        Ok(revisions) => Json(serde_json::json!({ "revisions": revisions })).into_response(),
        Err(error) => loopback_original_file_error(error.message_ref()),
    }
}

pub(super) async fn loopback_download_artifact_revision(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    AxumPath((graph_id, artifact_id, revision_id)): AxumPath<(String, String, String)>,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "artifacts.read") {
        return response;
    }
    match read_artifact_revision(&state.app, &graph_id, &artifact_id, &revision_id).and_then(
        |(manifest, bytes)| {
            original_file_download_response(&manifest, bytes, false).map_err(AppError::storage)
        },
    ) {
        Ok(response) => response,
        Err(error) => loopback_original_file_error(error.message_ref()),
    }
}

pub(super) async fn loopback_restore_artifact_revision(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    AxumPath((graph_id, artifact_id, revision_id)): AxumPath<(String, String, String)>,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "artifacts.write") {
        return response;
    }
    match restore_artifact_revision(&state.app, &graph_id, &artifact_id, &revision_id) {
        Ok(checkpoint) => (StatusCode::OK, Json(checkpoint)).into_response(),
        Err(error) => loopback_original_file_error(error.message_ref()),
    }
}
