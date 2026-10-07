//! HTTP surface of the Files rudiments (CONTRACT-files v1, kept outside this
//! repository): a streamed,
//! capped upload that stores any file without parsing; rename and move by
//! merge; the trash; and a read of folder-view preferences that does not need
//! the offline source mirror. Logic shared with the older artifact and
//! navigation routes lives in `files_service.rs`.

use crate::{
    files_service::{
        self, AdoptOutcome, FilesError, ARTIFACT_EXISTS, ARTIFACT_NOT_FOUND, EMPTY_FILE,
        FOLDER_NOT_FOUND, INVALID_PATCH, INVALID_UPLOAD, MAX_LABEL_CHARS, MISSING_FILE,
    },
    ids::validate_local_id,
    loopback_http::{loopback_app_error, require_loopback_scopes},
    loopback_state::LoopbackState,
    paths::{
        cleanup_pending_upload_file, create_pending_upload_file_writer, existing_graph_dir,
        PendingUploadWriteError,
    },
    runtime_config::FILES_MAX_UPLOAD_BYTES,
};
use axum::{
    body::Bytes,
    extract::{multipart::Field, Multipart, Path as AxumPath, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{delete, get, post},
    Json, Router,
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, path::PathBuf, sync::Arc};
use uuid::Uuid;

/// Multipart framing and the small text parts around one file. A declared
/// body longer than the cap plus this is refused before a byte is read.
const FORM_OVERHEAD_ALLOWANCE: usize = 64 * 1024;
/// Each text part (`label`, `parentId`, `artifactId`) is read with this bound,
/// never into an unbounded buffer.
const TEXT_PART_MAX_BYTES: usize = 4 * 1024;
const PATCH_BODY_MAX_BYTES: usize = 64 * 1024;

pub(super) fn loopback_files_router() -> Router<Arc<LoopbackState>> {
    Router::new()
        .route("/artifacts/{graph_id}/files", post(loopback_files_upload))
        .route(
            "/navigation/{graph_id}/trash",
            get(loopback_files_trash_list),
        )
        .route(
            "/navigation/{graph_id}/trash/{artifact_id}",
            delete(loopback_files_purge),
        )
        .route(
            "/navigation/{graph_id}/trash/{artifact_id}/restore",
            post(loopback_files_restore),
        )
        .route(
            "/navigation/{graph_id}/file-views",
            get(loopback_files_views),
        )
}

/// A finished staged upload. Dropping it removes the staged file, whichever
/// way the request ends; adoption copies the bytes out first.
struct StagedUpload {
    path: PathBuf,
    size_bytes: usize,
    sha256: String,
    filename: String,
    mime_type: String,
}

impl Drop for StagedUpload {
    fn drop(&mut self) {
        cleanup_pending_upload_file(&self.path);
    }
}

enum StreamRefusal {
    TooLarge,
    Read(String),
    Write(String),
}

impl StreamRefusal {
    fn into_error(self) -> FilesError {
        match self {
            Self::TooLarge => FilesError::too_large(FILES_MAX_UPLOAD_BYTES),
            Self::Read(message) => {
                FilesError::new(StatusCode::BAD_REQUEST, INVALID_UPLOAD, message)
            }
            Self::Write(message) => FilesError::internal(message),
        }
    }
}

/// Stream one multipart file part to a staged file under the graph, hashing
/// as it goes. Refuses the moment the part passes `max_bytes`; the unfinished
/// writer removes its file on drop.
async fn stream_file_part(
    graph_dir: &std::path::Path,
    mut field: Field<'_>,
    max_bytes: usize,
) -> Result<(PathBuf, usize, String), StreamRefusal> {
    let mut writer = create_pending_upload_file_writer(graph_dir).map_err(StreamRefusal::Write)?;
    let mut hasher = Sha256::new();
    loop {
        let chunk = match field.chunk().await {
            Ok(Some(chunk)) => chunk,
            Ok(None) => break,
            Err(error) if error.status() == StatusCode::PAYLOAD_TOO_LARGE => {
                return Err(StreamRefusal::TooLarge)
            }
            Err(error) => return Err(StreamRefusal::Read(format!("read uploaded file: {error}"))),
        };
        writer
            .write_chunk(&chunk, max_bytes)
            .map_err(|error| match error {
                PendingUploadWriteError::TooLarge { .. } => StreamRefusal::TooLarge,
                PendingUploadWriteError::Write(message) => StreamRefusal::Write(message),
            })?;
        hasher.update(&chunk);
    }
    let (path, size_bytes) = writer.finish().map_err(StreamRefusal::Write)?;
    Ok((path, size_bytes, format!("{:x}", hasher.finalize())))
}

async fn read_text_part(mut field: Field<'_>) -> Result<String, FilesError> {
    let mut bytes = Vec::new();
    loop {
        let chunk = match field.chunk().await {
            Ok(Some(chunk)) => chunk,
            Ok(None) => break,
            Err(error) => {
                return Err(FilesError::new(
                    StatusCode::BAD_REQUEST,
                    INVALID_UPLOAD,
                    format!("read multipart field: {error}"),
                ))
            }
        };
        if bytes.len() + chunk.len() > TEXT_PART_MAX_BYTES {
            return Err(FilesError::new(
                StatusCode::BAD_REQUEST,
                INVALID_UPLOAD,
                format!("a multipart text field is longer than {TEXT_PART_MAX_BYTES} bytes"),
            ));
        }
        bytes.extend_from_slice(&chunk);
    }
    String::from_utf8(bytes)
        .map(|text| text.trim().to_string())
        .map_err(|_| {
            FilesError::new(
                StatusCode::BAD_REQUEST,
                INVALID_UPLOAD,
                "a multipart text field is not UTF-8",
            )
        })
}

fn declared_length(headers: &HeaderMap) -> Option<usize> {
    headers
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<usize>().ok())
}

/// `POST /artifacts/{graph_id}/files`: store one file of any type, unparsed.
pub(super) async fn loopback_files_upload(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    AxumPath(graph_id): AxumPath<String>,
    multipart: Multipart,
) -> Response {
    if let Err(response) = require_loopback_scopes(
        &headers,
        &state,
        &["artifacts.write", "workspace.write.crdt"],
    ) {
        return response;
    }
    if declared_length(&headers)
        .is_some_and(|length| length > FILES_MAX_UPLOAD_BYTES + FORM_OVERHEAD_ALLOWANCE)
    {
        return files_service::file_too_large_response(FILES_MAX_UPLOAD_BYTES);
    }
    match upload(&state, &graph_id, multipart).await {
        Ok((status, body)) => (status, Json(body)).into_response(),
        Err(error) => error.into_response(),
    }
}

async fn upload(
    state: &Arc<LoopbackState>,
    graph_id: &str,
    mut multipart: Multipart,
) -> Result<(StatusCode, Value), FilesError> {
    let graph_dir = existing_graph_dir(&state.app, graph_id)
        .map_err(|message| FilesError::uncoded(StatusCode::NOT_FOUND, message))?;
    let mut staged: Option<StagedUpload> = None;
    let mut parent_id: Option<String> = None;
    let mut label: Option<String> = None;
    let mut requested_id: Option<String> = None;
    loop {
        let field = match multipart.next_field().await {
            Ok(Some(field)) => field,
            Ok(None) => break,
            Err(error) if error.status() == StatusCode::PAYLOAD_TOO_LARGE => {
                return Err(FilesError::too_large(FILES_MAX_UPLOAD_BYTES))
            }
            Err(error) => {
                return Err(FilesError::new(
                    StatusCode::BAD_REQUEST,
                    INVALID_UPLOAD,
                    format!("invalid multipart upload: {error}"),
                ))
            }
        };
        let name = field.name().unwrap_or_default().to_string();
        if name == "file" {
            if staged.is_some() {
                return Err(FilesError::new(
                    StatusCode::BAD_REQUEST,
                    INVALID_UPLOAD,
                    "send exactly one file part per upload",
                ));
            }
            let filename = field
                .file_name()
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .unwrap_or("unnamed")
                .to_string();
            let mime_type = field
                .content_type()
                .map(str::trim)
                .filter(|mime| !mime.is_empty())
                .unwrap_or("application/octet-stream")
                .to_string();
            let (path, size_bytes, sha256) =
                stream_file_part(&graph_dir, field, FILES_MAX_UPLOAD_BYTES)
                    .await
                    .map_err(StreamRefusal::into_error)?;
            staged = Some(StagedUpload {
                path,
                size_bytes,
                sha256,
                filename,
                mime_type,
            });
            continue;
        }
        let text = read_text_part(field).await?;
        if text.is_empty() {
            continue;
        }
        match name.as_str() {
            "parentId" | "parent_id" => parent_id = Some(text),
            "label" => label = Some(text),
            "artifactId" | "artifact_id" => requested_id = Some(text),
            _ => {}
        }
    }

    let Some(staged) = staged else {
        return Err(FilesError::new(
            StatusCode::BAD_REQUEST,
            MISSING_FILE,
            "missing multipart file part",
        ));
    };
    if staged.size_bytes == 0 {
        return Err(FilesError::new(
            StatusCode::BAD_REQUEST,
            EMPTY_FILE,
            "Empty file uploaded",
        ));
    }
    let artifact_id = match requested_id {
        Some(id) => {
            files_service::require_artifact_id(&id)?;
            id
        }
        None => format!("file-{}", Uuid::new_v4().simple()),
    };
    let label = label.unwrap_or_else(|| staged.filename.clone());
    if label.chars().count() > MAX_LABEL_CHARS {
        return Err(FilesError::new(
            StatusCode::BAD_REQUEST,
            INVALID_UPLOAD,
            format!("label is longer than {MAX_LABEL_CHARS} characters"),
        ));
    }
    if let Some(parent) = parent_id.as_deref() {
        if validate_local_id(parent, "parentId").is_err()
            || !files_service::artifacts_folder_exists(&state.app, graph_id, parent)?
        {
            return Err(FilesError::new(
                StatusCode::NOT_FOUND,
                FOLDER_NOT_FOUND,
                format!("no artifacts folder {parent} in this graph"),
            ));
        }
    }
    let live = files_service::find_artifact_entity(&state.app, graph_id, &artifact_id)?;
    if live.is_some() && !files_service::has_original(&graph_dir, &artifact_id)? {
        // A metadata-only artifact already owns this id; its entry is not
        // ours to overwrite.
        return Err(FilesError::new(
            StatusCode::CONFLICT,
            ARTIFACT_EXISTS,
            format!("artifact {artifact_id} already exists"),
        ));
    }

    let outcome = files_service::adopt_uploaded_file(
        &state.app,
        graph_id,
        &artifact_id,
        &staged.filename,
        &staged.mime_type,
        &staged.path,
        staged.size_bytes,
        &staged.sha256,
    )?;
    let (manifest, replayed) = match outcome {
        AdoptOutcome::Adopted(manifest) => (manifest, false),
        AdoptOutcome::Replayed(manifest) => (manifest, true),
    };
    let original_filename = staged.filename.clone();
    // The bytes are adopted (or already there): drop the staged copy now.
    drop(staged);

    // A replay whose first attempt registered the file returns that entry as
    // it is now (it may have been renamed since). A replay whose first attempt
    // died between storing and registering completes the registration.
    if !(replayed && live.is_some()) {
        let payload = json!({
            "artifactId": artifact_id,
            "id": artifact_id,
            "label": label,
            "parentId": parent_id,
            "originalFilename": original_filename,
            "mimeType": manifest.mime_type,
            "sizeBytes": manifest.size_bytes,
            "fileType": crate::crdt_engine::workspace_ops::artifact_file_type(&original_filename),
            "status": "ready",
            "storageKey": format!("local://artifacts/{artifact_id}/original/{}", manifest.filename),
        });
        files_service::enqueue_workspace_operation(
            &state.app,
            "workspace.putArtifact",
            graph_id,
            &artifact_id,
            payload,
        )
        .await
        .map_err(|error| FilesError::uncoded(StatusCode::BAD_REQUEST, error))?;
        files_service::flush_projection(&state.app, graph_id).await?;
    }
    let entity = files_service::find_artifact_entity(&state.app, graph_id, &artifact_id)?
        .ok_or_else(|| {
            FilesError::internal(format!(
                "uploaded artifact {artifact_id} is not in the workspace"
            ))
        })?;
    let mut body = files_service::artifact_value(&entity, graph_id);
    body["sha256"] = json!(manifest_sha(&state.app, graph_id, &artifact_id)?);
    body["replayed"] = json!(replayed);
    Ok((
        if replayed {
            StatusCode::OK
        } else {
            StatusCode::CREATED
        },
        body,
    ))
}

/// SHA-256 of the stored original, read back from disk (the bytes a download
/// will serve), not trusted from the request.
fn manifest_sha(
    app: &crate::app_runtime::AppHandle,
    graph_id: &str,
    artifact_id: &str,
) -> Result<String, FilesError> {
    let graph_dir = existing_graph_dir(app, graph_id)
        .map_err(|message| FilesError::uncoded(StatusCode::NOT_FOUND, message))?;
    let original_dir = crate::paths::artifact_original_dir(&graph_dir, artifact_id)
        .map_err(FilesError::internal)?;
    let manifest = crate::original_file_manifest_store::read_original_manifest(&original_dir)
        .map_err(FilesError::internal)?;
    let path = crate::original_file_manifest_store::original_manifest_file_path(
        &original_dir,
        &manifest.filename,
    )
    .map_err(FilesError::internal)?;
    files_service::hash_file(&path)
        .map(|(_, sha256)| sha256)
        .map_err(FilesError::internal)
}

/// `PATCH /navigation/{graph_id}/artifacts/{artifact_id}`: rename and/or move.
pub(super) async fn loopback_files_patch(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    AxumPath((graph_id, artifact_id)): AxumPath<(String, String)>,
    body: Bytes,
) -> Response {
    if let Err(response) = require_loopback_scopes(
        &headers,
        &state,
        &["workspace.write.crdt", "artifacts.write"],
    ) {
        return response;
    }
    match patch(&state, &graph_id, &artifact_id, &body).await {
        Ok(value) => Json(value).into_response(),
        Err(error) => error.into_response(),
    }
}

fn invalid_patch(message: impl Into<String>) -> FilesError {
    FilesError::new(StatusCode::BAD_REQUEST, INVALID_PATCH, message)
}

async fn patch(
    state: &Arc<LoopbackState>,
    graph_id: &str,
    artifact_id: &str,
    body: &[u8],
) -> Result<Value, FilesError> {
    files_service::require_artifact_id(artifact_id)?;
    if body.len() > PATCH_BODY_MAX_BYTES {
        return Err(invalid_patch("patch body is too large"));
    }
    let input: Value = serde_json::from_slice(body)
        .map_err(|error| invalid_patch(format!("patch body is not JSON: {error}")))?;
    let Some(input) = input.as_object() else {
        return Err(invalid_patch("patch body must be a JSON object"));
    };
    let mut payload = serde_json::Map::new();
    payload.insert("patch".to_string(), json!(true));
    payload.insert("artifactId".to_string(), json!(artifact_id));
    if let Some(label) = input.get("label") {
        let label = label
            .as_str()
            .map(str::trim)
            .filter(|label| !label.is_empty())
            .ok_or_else(|| invalid_patch("label must be a non-empty string"))?;
        if label.chars().count() > MAX_LABEL_CHARS {
            return Err(invalid_patch(format!(
                "label is longer than {MAX_LABEL_CHARS} characters"
            )));
        }
        payload.insert("label".to_string(), json!(label));
    }
    if let Some(parent) = input.get("parentId") {
        match parent {
            Value::Null => {
                payload.insert("parentId".to_string(), Value::Null);
            }
            Value::String(parent_id) if validate_local_id(parent_id, "parentId").is_ok() => {
                payload.insert("parentId".to_string(), json!(parent_id));
            }
            _ => return Err(invalid_patch("parentId must be a folder id or null")),
        }
    }
    if let Some(order) = input.get("order") {
        if !order.as_f64().is_some_and(f64::is_finite) {
            return Err(invalid_patch("order must be a finite number"));
        }
        payload.insert("order".to_string(), order.clone());
    }
    if payload.len() == 2 {
        return Err(invalid_patch("send at least one of label, parentId, order"));
    }
    files_service::flush_projection(&state.app, graph_id).await?;
    if files_service::find_artifact_entity(&state.app, graph_id, artifact_id)?.is_none() {
        return Err(FilesError::new(
            StatusCode::NOT_FOUND,
            ARTIFACT_NOT_FOUND,
            format!("artifact not found: {artifact_id}"),
        ));
    }
    if let Some(parent_id) = payload.get("parentId").and_then(Value::as_str) {
        if !files_service::artifacts_folder_exists(&state.app, graph_id, parent_id)? {
            return Err(FilesError::new(
                StatusCode::NOT_FOUND,
                FOLDER_NOT_FOUND,
                format!("no artifacts folder {parent_id} in this graph"),
            ));
        }
    }
    if let Err(error) = files_service::enqueue_workspace_operation(
        &state.app,
        "workspace.putArtifact",
        graph_id,
        artifact_id,
        Value::Object(payload),
    )
    .await
    {
        // The room transaction is the authority; its refusals keep their codes.
        return Err(if error.contains("artifact not found") {
            FilesError::new(StatusCode::NOT_FOUND, ARTIFACT_NOT_FOUND, error)
        } else if error.contains("parent folder not found") {
            FilesError::new(StatusCode::NOT_FOUND, FOLDER_NOT_FOUND, error)
        } else if error.contains("putArtifact patch") {
            invalid_patch(error)
        } else {
            FilesError::uncoded(StatusCode::BAD_REQUEST, error)
        });
    }
    files_service::flush_projection(&state.app, graph_id).await?;
    let entity = files_service::find_artifact_entity(&state.app, graph_id, artifact_id)?
        .ok_or_else(|| {
            FilesError::new(
                StatusCode::NOT_FOUND,
                ARTIFACT_NOT_FOUND,
                format!("artifact not found: {artifact_id}"),
            )
        })?;
    Ok(files_service::artifact_value(&entity, graph_id))
}

/// `DELETE /navigation/{graph_id}/artifacts/{artifact_id}`: to the trash.
pub(super) async fn loopback_files_trash(
    state: Arc<LoopbackState>,
    headers: HeaderMap,
    graph_id: String,
    artifact_id: String,
) -> Response {
    if let Err(response) = require_loopback_scopes(
        &headers,
        &state,
        &["workspace.delete.crdt", "artifacts.delete"],
    ) {
        return response;
    }
    match files_service::trash_artifact(&state.app, &graph_id, &artifact_id).await {
        Ok(value) => Json(value).into_response(),
        Err(error) => error.into_response(),
    }
}

/// `GET /navigation/{graph_id}/trash`.
pub(super) async fn loopback_files_trash_list(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    AxumPath(graph_id): AxumPath<String>,
) -> Response {
    if let Err(response) = require_loopback_scopes(&headers, &state, &["artifacts.read"]) {
        return response;
    }
    match files_service::list_trash(&state.app, &graph_id) {
        Ok(value) => Json(value).into_response(),
        Err(error) => error.into_response(),
    }
}

/// `POST /navigation/{graph_id}/trash/{artifact_id}/restore`.
pub(super) async fn loopback_files_restore(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    AxumPath((graph_id, artifact_id)): AxumPath<(String, String)>,
) -> Response {
    if let Err(response) = require_loopback_scopes(
        &headers,
        &state,
        &["workspace.write.crdt", "artifacts.write"],
    ) {
        return response;
    }
    match files_service::restore_artifact(&state.app, &graph_id, &artifact_id).await {
        Ok(value) => Json(value).into_response(),
        Err(error) => error.into_response(),
    }
}

/// `DELETE /navigation/{graph_id}/trash/{artifact_id}`: delete for good.
pub(super) async fn loopback_files_purge(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    AxumPath((graph_id, artifact_id)): AxumPath<(String, String)>,
) -> Response {
    if let Err(response) = require_loopback_scopes(
        &headers,
        &state,
        &["workspace.delete.crdt", "artifacts.delete"],
    ) {
        return response;
    }
    match files_service::purge_artifact(&state.app, &graph_id, &artifact_id).await {
        Ok(value) => Json(value).into_response(),
        Err(error) => error.into_response(),
    }
}

/// `GET /navigation/{graph_id}/file-views?folderKey=&viewId=`.
pub(super) async fn loopback_files_views(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    AxumPath(graph_id): AxumPath<String>,
    Query(params): Query<BTreeMap<String, String>>,
) -> Response {
    if let Err(response) = require_loopback_scopes(&headers, &state, &["workspace.read"]) {
        return response;
    }
    let folder_key = params
        .get("folderKey")
        .map(String::as_str)
        .filter(|key| !key.is_empty());
    let view_id = params
        .get("viewId")
        .map(String::as_str)
        .filter(|view| !view.is_empty());
    match crate::source_sync::read_file_views(&state.app, &graph_id, folder_key, view_id).await {
        Ok(value) => Json(value).into_response(),
        Err(error) => loopback_app_error(error),
    }
}
