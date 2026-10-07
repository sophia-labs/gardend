//! Files rudiments (round 2026-10-05): any file stored as an artifact with no
//! parsing, a per-file cap the cell enforces, rename and move by an in-place
//! merge, and a trash that keeps the bytes until an explicit purge.
//!
//! Contract: the Files contract v1 (CONTRACT-files, kept outside this
//! repository). The HTTP handlers live in `loopback_files_routes.rs`; this module holds
//! the parts the older artifact and navigation routes share with them.
//!
//! Storage. Bytes live where every artifact's bytes live,
//! `{graph}/artifacts/{id}/original/{manifest.json, <bytes>}`, so the durable
//! flush, the backups, graph duplication and the existing download route all
//! carry them unchanged. A trashed file keeps that directory and gains a sibling
//! `{graph}/artifacts/{id}/trash.json` holding its last workspace entry; purge
//! removes the whole directory. The trash record is written BEFORE the
//! workspace entry is removed, and removed AFTER a restore has put the entry
//! back, so a crash between the two steps leaves a live file with a stale trash
//! record, which the trash listing ignores and the next restore clears.

use crate::app_runtime::AppHandle;
use crate::{
    crdt_queue::{enqueue_crdt_operation_outcome, EnqueueCrdtOperationInput},
    hosted_navigation_projection::{hosted_artifact_value, hosted_folder_value},
    ids::validate_local_id,
    original_file_manifest_store::{original_manifest_file_path, read_original_manifest},
    original_file_types::OriginalFileManifest,
    paths::{artifact_original_dir, artifacts_dir, existing_graph_dir},
    rdf_service::snapshot_array,
    runtime_config::FILES_MAX_UPLOAD_BYTES,
    storage::{create_dir_all, read_json, remove_file_if_exists, write_json},
    workspace_entity_projection::{workspace_entity_id, workspace_folders},
};
use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};

pub(crate) const FILE_TOO_LARGE: &str = "file_too_large";
pub(crate) const EMPTY_FILE: &str = "empty_file";
pub(crate) const MISSING_FILE: &str = "missing_file";
pub(crate) const INVALID_UPLOAD: &str = "invalid_upload";
pub(crate) const INVALID_ARTIFACT_ID: &str = "invalid_artifact_id";
pub(crate) const ARTIFACT_EXISTS: &str = "artifact_exists";
pub(crate) const ARTIFACT_NOT_FOUND: &str = "artifact_not_found";
pub(crate) const FOLDER_NOT_FOUND: &str = "folder_not_found";
pub(crate) const INVALID_PATCH: &str = "invalid_patch";
pub(crate) const NOT_IN_TRASH: &str = "not_in_trash";
pub(crate) const ARTIFACT_LIVE: &str = "artifact_live";
pub(crate) const TRASH_BYTES_MISSING: &str = "trash_bytes_missing";

/// Display names are data; this only bounds them.
pub(crate) const MAX_LABEL_CHARS: usize = 512;

const TRASH_FILE: &str = "trash.json";
const TRASH_SCHEMA_VERSION: u32 = 1;

/// A refusal with an HTTP status and, usually, a machine code the shell may
/// branch on. Body: `{"ok": false, "error", "code"?, "maxBytes"?}`.
#[derive(Debug)]
pub(crate) struct FilesError {
    status: StatusCode,
    code: Option<&'static str>,
    message: String,
    max_bytes: Option<usize>,
}

impl FilesError {
    pub(crate) fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code: Some(code),
            message: message.into(),
            max_bytes: None,
        }
    }

    pub(crate) fn uncoded(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            code: None,
            message: message.into(),
            max_bytes: None,
        }
    }

    pub(crate) fn internal(message: impl Into<String>) -> Self {
        Self::uncoded(StatusCode::INTERNAL_SERVER_ERROR, message)
    }

    pub(crate) fn too_large(max_bytes: usize) -> Self {
        Self {
            status: StatusCode::PAYLOAD_TOO_LARGE,
            code: Some(FILE_TOO_LARGE),
            message: format!(
                "File too large. Maximum size: {}MB",
                max_bytes / (1024 * 1024)
            ),
            max_bytes: Some(max_bytes),
        }
    }
}

impl IntoResponse for FilesError {
    fn into_response(self) -> Response {
        let mut body = json!({ "ok": false, "error": self.message });
        if let Some(code) = self.code {
            body["code"] = json!(code);
        }
        if let Some(max_bytes) = self.max_bytes {
            body["maxBytes"] = json!(max_bytes);
        }
        (self.status, Json(body)).into_response()
    }
}

pub(crate) fn file_too_large_response(max_bytes: usize) -> Response {
    FilesError::too_large(max_bytes).into_response()
}

/// Exact decoded length of standard base64 text, trimmed as the decoder trims
/// it. Malformed lengths are estimated; the decoder refuses them anyway.
pub(crate) fn base64_decoded_len(data: &str) -> usize {
    let trimmed = data.trim();
    let len = trimmed.len();
    let padding = trimmed
        .bytes()
        .rev()
        .take(2)
        .take_while(|byte| *byte == b'=')
        .count();
    let full = (len / 4) * 3;
    match len % 4 {
        0 => full.saturating_sub(padding),
        2 => full + 1,
        3 => full + 2,
        _ => full + 1,
    }
}

/// The Files cap on an inline base64 payload, checked before decoding it.
pub(crate) fn refuse_oversized_base64(data: &str) -> Result<(), FilesError> {
    if base64_decoded_len(data) > FILES_MAX_UPLOAD_BYTES {
        Err(FilesError::too_large(FILES_MAX_UPLOAD_BYTES))
    } else {
        Ok(())
    }
}

/// Request-body bound for a JSON route that carries one base64 file: the
/// encoded cap plus 1 MiB for the envelope. Larger bodies are refused by the
/// extractor before they are buffered whole.
pub(crate) const fn base64_route_body_limit() -> usize {
    (FILES_MAX_UPLOAD_BYTES + 2) / 3 * 4 + 1024 * 1024
}

pub(crate) fn require_artifact_id(artifact_id: &str) -> Result<(), FilesError> {
    validate_local_id(artifact_id, "artifactId")
        .map_err(|message| FilesError::new(StatusCode::BAD_REQUEST, INVALID_ARTIFACT_ID, message))
}

// ── workspace reads ─────────────────────────────────────────────────────────

fn workspace_snapshot(app: &AppHandle, graph_id: &str) -> Result<Option<Value>, FilesError> {
    let graph_dir = existing_graph_dir(app, graph_id)
        .map_err(|message| FilesError::uncoded(StatusCode::NOT_FOUND, message))?;
    let record = crate::workspace_record_store::read_workspace_record(&graph_dir, graph_id)
        .map_err(FilesError::internal)?;
    Ok(record.snapshot)
}

/// The materialized workspace entry for one artifact, if it is live.
pub(crate) fn find_artifact_entity(
    app: &AppHandle,
    graph_id: &str,
    artifact_id: &str,
) -> Result<Option<Value>, FilesError> {
    let snapshot = workspace_snapshot(app, graph_id)?;
    Ok(snapshot.as_ref().and_then(|snapshot| {
        snapshot_array(snapshot, "artifacts")
            .iter()
            .find(|entity| workspace_entity_id(entity).as_deref() == Some(artifact_id))
            .cloned()
    }))
}

fn live_artifact_ids(app: &AppHandle, graph_id: &str) -> Result<BTreeSet<String>, FilesError> {
    let snapshot = workspace_snapshot(app, graph_id)?;
    Ok(snapshot
        .as_ref()
        .map(|snapshot| {
            snapshot_array(snapshot, "artifacts")
                .iter()
                .filter_map(workspace_entity_id)
                .collect()
        })
        .unwrap_or_default())
}

/// Files live only under folders of the artifacts section: the workspace tree
/// shows an artifact whose parent is a documents folder at the artifacts root.
pub(crate) fn artifacts_folder_exists(
    app: &AppHandle,
    graph_id: &str,
    folder_id: &str,
) -> Result<bool, FilesError> {
    let snapshot = workspace_snapshot(app, graph_id)?;
    Ok(workspace_folders(snapshot.as_ref()).iter().any(|folder| {
        let value = hosted_folder_value(folder, graph_id);
        value["id"] == folder_id && value["section"] == "artifacts"
    }))
}

pub(crate) fn artifact_value(entity: &Value, graph_id: &str) -> Value {
    let mut value = hosted_artifact_value(entity, graph_id);
    if let Some(object) = value.as_object_mut() {
        let id = object.get("id").cloned().unwrap_or(Value::Null);
        object.insert("artifactId".to_string(), id);
    }
    value
}

pub(crate) async fn flush_projection(app: &AppHandle, graph_id: &str) -> Result<(), FilesError> {
    crate::crdt_projection_flush::flush_graph_projection(app.clone(), graph_id)
        .await
        .map_err(FilesError::internal)
}

pub(crate) async fn enqueue_workspace_operation(
    app: &AppHandle,
    kind: &str,
    graph_id: &str,
    artifact_id: &str,
    payload: Value,
) -> Result<Value, String> {
    enqueue_crdt_operation_outcome(
        app.clone(),
        EnqueueCrdtOperationInput {
            kind: kind.to_string(),
            graph_id: graph_id.to_string(),
            document_id: Some(artifact_id.to_string()),
            payload,
        },
    )
    .await
    .map(|outcome| outcome.value)
}

// ── upload adoption ─────────────────────────────────────────────────────────

pub(crate) enum AdoptOutcome {
    /// The staged bytes are now the artifact's original.
    Adopted(OriginalFileManifest),
    /// The id already holds exactly these bytes (a retry); nothing written.
    Replayed(OriginalFileManifest),
}

/// SHA-256 and length of a stored file, read in 64 KiB chunks.
pub(crate) fn hash_file(path: &Path) -> Result<(u64, String), String> {
    use std::io::Read;
    let mut file =
        std::fs::File::open(path).map_err(|error| format!("open stored file: {error}"))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 64 * 1024];
    let mut total = 0u64;
    loop {
        let read = match file.read(&mut buffer) {
            Ok(read) => read,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(format!("read stored file: {error}")),
        };
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        total += read as u64;
    }
    Ok((total, format!("{:x}", hasher.finalize())))
}

/// Move a staged upload into `artifacts/{id}/original/` under the graph's
/// hot-write lease, so two uploads naming the same id cannot interleave their
/// check and their write. The caller still owns (and removes) the staged file.
#[allow(clippy::too_many_arguments)]
pub(crate) fn adopt_uploaded_file(
    app: &AppHandle,
    graph_id: &str,
    artifact_id: &str,
    filename: &str,
    mime_type: &str,
    staged_path: &Path,
    size_bytes: usize,
    sha256: &str,
) -> Result<AdoptOutcome, FilesError> {
    let _lease =
        crate::crdt_engine::persistence_coordinator::acquire_hot_write_blocking_if_managed(
            app, graph_id,
        )
        .map_err(FilesError::internal)?;
    let _flush_guard = crate::cell_durability::write_guard();
    let graph_dir = existing_graph_dir(app, graph_id)
        .map_err(|message| FilesError::uncoded(StatusCode::NOT_FOUND, message))?;
    if trash_path(&graph_dir, artifact_id)?.is_file() {
        return Err(FilesError::new(
            StatusCode::CONFLICT,
            ARTIFACT_EXISTS,
            format!("artifact {artifact_id} is in the trash; restore it or choose another id"),
        ));
    }
    let original_dir = artifact_original_dir(&graph_dir, artifact_id).map_err(|message| {
        FilesError::new(StatusCode::BAD_REQUEST, INVALID_ARTIFACT_ID, message)
    })?;
    if original_dir.join("manifest.json").is_file() {
        let manifest = read_original_manifest(&original_dir).map_err(FilesError::internal)?;
        let stored = original_manifest_file_path(&original_dir, &manifest.filename)
            .map_err(FilesError::internal)?;
        let (stored_len, stored_sha) = hash_file(&stored).map_err(FilesError::internal)?;
        if stored_len == size_bytes as u64 && stored_sha == sha256 {
            return Ok(AdoptOutcome::Replayed(manifest));
        }
        return Err(FilesError::new(
            StatusCode::CONFLICT,
            ARTIFACT_EXISTS,
            format!("artifact {artifact_id} already holds different bytes"),
        ));
    }
    crate::artifact_text_service::refuse_legacy_writer(&graph_dir, artifact_id)
        .map_err(|error| FilesError::new(StatusCode::CONFLICT, ARTIFACT_EXISTS, error.message()))?;
    let manifest = crate::original_file_storage::save_original_file_from_path_to_dir(
        &original_dir,
        filename,
        mime_type,
        staged_path,
    )
    .map_err(FilesError::internal)?;
    crate::graph_service::touch_graph_updated_at(&graph_dir)
        .map_err(|error| FilesError::internal(error.message()))?;
    Ok(AdoptOutcome::Adopted(manifest))
}

// ── trash ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TrashRecord {
    schema_version: u32,
    artifact_id: String,
    trashed_at: String,
    /// Whether `original/` held bytes when the file was trashed. A
    /// metadata-only artifact restores without them.
    has_original: bool,
    /// The navigation value at trash time (what the trash listing shows).
    value: Value,
    /// The raw workspace entry, kept for fields the navigation value omits.
    entry: Value,
}

fn trash_path(graph_dir: &Path, artifact_id: &str) -> Result<PathBuf, FilesError> {
    require_artifact_id(artifact_id)?;
    Ok(artifacts_dir(graph_dir).join(artifact_id).join(TRASH_FILE))
}

fn read_trash(graph_dir: &Path, artifact_id: &str) -> Result<Option<TrashRecord>, FilesError> {
    let path = trash_path(graph_dir, artifact_id)?;
    if !path.is_file() {
        return Ok(None);
    }
    let record: TrashRecord =
        read_json(&path).map_err(|error| FilesError::internal(error.message()))?;
    if record.schema_version != TRASH_SCHEMA_VERSION || record.artifact_id != artifact_id {
        return Err(FilesError::internal(format!(
            "trash record for {artifact_id} is not a schema {TRASH_SCHEMA_VERSION} record for that id"
        )));
    }
    Ok(Some(record))
}

fn write_trash(
    app: &AppHandle,
    graph_id: &str,
    graph_dir: &Path,
    record: &TrashRecord,
) -> Result<(), FilesError> {
    let _lease =
        crate::crdt_engine::persistence_coordinator::acquire_hot_write_blocking_if_managed(
            app, graph_id,
        )
        .map_err(FilesError::internal)?;
    let _flush_guard = crate::cell_durability::write_guard();
    let path = trash_path(graph_dir, &record.artifact_id)?;
    if let Some(parent) = path.parent() {
        create_dir_all(parent).map_err(|error| FilesError::internal(error.message()))?;
    }
    write_json(&path, record).map_err(|error| FilesError::internal(error.message()))
}

fn remove_trash(
    app: &AppHandle,
    graph_id: &str,
    graph_dir: &Path,
    artifact_id: &str,
) -> Result<(), FilesError> {
    let _lease =
        crate::crdt_engine::persistence_coordinator::acquire_hot_write_blocking_if_managed(
            app, graph_id,
        )
        .map_err(FilesError::internal)?;
    let _flush_guard = crate::cell_durability::write_guard();
    let path = trash_path(graph_dir, artifact_id)?;
    remove_file_if_exists(&path)
        .map(|_| ())
        .map_err(|error| FilesError::internal(error.message()))
}

pub(crate) fn has_original(graph_dir: &Path, artifact_id: &str) -> Result<bool, FilesError> {
    let original_dir = artifact_original_dir(graph_dir, artifact_id).map_err(|message| {
        FilesError::new(StatusCode::BAD_REQUEST, INVALID_ARTIFACT_ID, message)
    })?;
    Ok(original_dir.join("manifest.json").is_file())
}

fn trashed_body(graph_id: &str, record: &TrashRecord, already: bool) -> Value {
    json!({
        "id": record.artifact_id,
        "artifactId": record.artifact_id,
        "graphId": graph_id,
        "status": "trashed",
        "trashed": true,
        "trashedAt": record.trashed_at,
        "alreadyTrashed": already,
    })
}

/// Take a file out of the workspace and keep its bytes and its entry.
pub(crate) async fn trash_artifact(
    app: &AppHandle,
    graph_id: &str,
    artifact_id: &str,
) -> Result<Value, FilesError> {
    require_artifact_id(artifact_id)?;
    let graph_dir = existing_graph_dir(app, graph_id)
        .map_err(|message| FilesError::uncoded(StatusCode::NOT_FOUND, message))?;
    flush_projection(app, graph_id).await?;
    let Some(entry) = find_artifact_entity(app, graph_id, artifact_id)? else {
        return match read_trash(&graph_dir, artifact_id)? {
            Some(record) => Ok(trashed_body(graph_id, &record, true)),
            None => Err(FilesError::new(
                StatusCode::NOT_FOUND,
                ARTIFACT_NOT_FOUND,
                format!("artifact not found: {artifact_id}"),
            )),
        };
    };
    let record = TrashRecord {
        schema_version: TRASH_SCHEMA_VERSION,
        artifact_id: artifact_id.to_string(),
        trashed_at: chrono::Utc::now().to_rfc3339(),
        has_original: has_original(&graph_dir, artifact_id)?,
        value: artifact_value(&entry, graph_id),
        entry,
    };
    write_trash(app, graph_id, &graph_dir, &record)?;
    if let Err(error) = enqueue_workspace_operation(
        app,
        "workspace.deleteArtifact",
        graph_id,
        artifact_id,
        json!({ "artifactId": artifact_id }),
    )
    .await
    {
        // A concurrent delete already removed the entry: the trash record
        // stands. Anything else leaves the file live; drop the stale record.
        if !error.contains("not found") {
            let _ = remove_trash(app, graph_id, &graph_dir, artifact_id);
            return Err(FilesError::uncoded(StatusCode::BAD_REQUEST, error));
        }
    }
    flush_projection(app, graph_id).await?;
    Ok(trashed_body(graph_id, &record, false))
}

/// Trashed files of a graph, newest first. A record whose id is live again
/// (a crash between restore's two steps) is not listed.
pub(crate) fn list_trash(app: &AppHandle, graph_id: &str) -> Result<Value, FilesError> {
    let graph_dir = existing_graph_dir(app, graph_id)
        .map_err(|message| FilesError::uncoded(StatusCode::NOT_FOUND, message))?;
    let live = live_artifact_ids(app, graph_id)?;
    let root = artifacts_dir(&graph_dir);
    let mut items = Vec::new();
    let entries = match std::fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(json!({ "graphId": graph_id, "items": [], "count": 0 }));
        }
        Err(error) => return Err(FilesError::internal(format!("read artifacts dir: {error}"))),
    };
    for entry in entries {
        let entry =
            entry.map_err(|error| FilesError::internal(format!("read artifacts dir: {error}")))?;
        let id = entry.file_name().to_string_lossy().into_owned();
        if validate_local_id(&id, "artifactId").is_err() || live.contains(&id) {
            continue;
        }
        match read_trash(&graph_dir, &id) {
            Ok(Some(record)) => items.push(json!({
                "artifactId": record.artifact_id,
                "label": record.value["label"],
                "originalFilename": record.value["originalFilename"],
                "mimeType": record.value["mimeType"],
                "sizeBytes": record.value["sizeBytes"],
                "fileType": record.value["fileType"],
                "parentId": record.value["parentId"],
                "trashedAt": record.trashed_at,
            })),
            Ok(None) => {}
            Err(error) => log::warn!(
                "[files] skipping unreadable trash record {id}: {}",
                error.message
            ),
        }
    }
    items.sort_by(|left, right| {
        right["trashedAt"]
            .as_str()
            .unwrap_or_default()
            .cmp(left["trashedAt"].as_str().unwrap_or_default())
    });
    let count = items.len();
    Ok(json!({ "graphId": graph_id, "items": items, "count": count }))
}

fn restore_payload(record: &TrashRecord, parent_id: Option<&str>) -> Value {
    let value = &record.value;
    let mut payload = json!({
        "artifactId": record.artifact_id,
        "id": record.artifact_id,
        "label": value["label"],
        "parentId": parent_id,
        "order": value["order"],
        "fileType": value["fileType"],
        "status": value["status"],
        "errorMessage": value["errorMessage"],
        "storageKey": value["storageKey"],
        "originalFilename": value["originalFilename"],
        "mimeType": value["mimeType"],
        "sizeBytes": value["sizeBytes"],
        "ingestedDocId": value["ingestedDocId"],
    });
    for key in ["sceneProjection", "sceneProjectionText", "sceneProjectedAt"] {
        if let Some(field) = record.entry.get(key).filter(|field| !field.is_null()) {
            payload[key] = field.clone();
        }
    }
    payload
}

/// Put a trashed file back: into its folder if that folder still exists,
/// otherwise at the root.
pub(crate) async fn restore_artifact(
    app: &AppHandle,
    graph_id: &str,
    artifact_id: &str,
) -> Result<Value, FilesError> {
    require_artifact_id(artifact_id)?;
    let graph_dir = existing_graph_dir(app, graph_id)
        .map_err(|message| FilesError::uncoded(StatusCode::NOT_FOUND, message))?;
    let Some(record) = read_trash(&graph_dir, artifact_id)? else {
        return Err(FilesError::new(
            StatusCode::NOT_FOUND,
            NOT_IN_TRASH,
            format!("artifact {artifact_id} is not in the trash"),
        ));
    };
    flush_projection(app, graph_id).await?;
    if let Some(entry) = find_artifact_entity(app, graph_id, artifact_id)? {
        remove_trash(app, graph_id, &graph_dir, artifact_id)?;
        let mut value = artifact_value(&entry, graph_id);
        value["restoredToParentId"] = value["parentId"].clone();
        value["alreadyLive"] = json!(true);
        return Ok(value);
    }
    if record.has_original && !has_original(&graph_dir, artifact_id)? {
        return Err(FilesError::new(
            StatusCode::GONE,
            TRASH_BYTES_MISSING,
            format!("the bytes of trashed artifact {artifact_id} are gone"),
        ));
    }
    let parent_id = match record.value["parentId"].as_str() {
        Some(parent) => {
            if artifacts_folder_exists(app, graph_id, parent)? {
                Some(parent.to_string())
            } else {
                None
            }
        }
        None => None,
    };
    enqueue_workspace_operation(
        app,
        "workspace.putArtifact",
        graph_id,
        artifact_id,
        restore_payload(&record, parent_id.as_deref()),
    )
    .await
    .map_err(|error| FilesError::uncoded(StatusCode::BAD_REQUEST, error))?;
    flush_projection(app, graph_id).await?;
    remove_trash(app, graph_id, &graph_dir, artifact_id)?;
    let entry = find_artifact_entity(app, graph_id, artifact_id)?.ok_or_else(|| {
        FilesError::internal(format!(
            "restored artifact {artifact_id} is not in the workspace"
        ))
    })?;
    let mut value = artifact_value(&entry, graph_id);
    value["restoredToParentId"] = json!(parent_id);
    Ok(value)
}

/// Delete a trashed file's bytes for good.
pub(crate) async fn purge_artifact(
    app: &AppHandle,
    graph_id: &str,
    artifact_id: &str,
) -> Result<Value, FilesError> {
    require_artifact_id(artifact_id)?;
    let graph_dir = existing_graph_dir(app, graph_id)
        .map_err(|message| FilesError::uncoded(StatusCode::NOT_FOUND, message))?;
    if read_trash(&graph_dir, artifact_id)?.is_none() {
        return Err(FilesError::new(
            StatusCode::NOT_FOUND,
            NOT_IN_TRASH,
            format!("artifact {artifact_id} is not in the trash"),
        ));
    }
    flush_projection(app, graph_id).await?;
    if find_artifact_entity(app, graph_id, artifact_id)?.is_some() {
        return Err(FilesError::new(
            StatusCode::CONFLICT,
            ARTIFACT_LIVE,
            format!("artifact {artifact_id} is in the workspace; trash it first"),
        ));
    }
    crate::original_file_service::delete_artifact_original_files(app, graph_id, artifact_id)
        .map_err(|error| FilesError::internal(error.message()))?;
    Ok(json!({ "artifactId": artifact_id, "graphId": graph_id, "purged": true }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::{engine::general_purpose::STANDARD, Engine as _};

    #[test]
    fn base64_decoded_len_is_exact_for_every_padding() {
        for len in 0..64usize {
            let bytes = vec![7u8; len];
            let encoded = STANDARD.encode(&bytes);
            assert_eq!(base64_decoded_len(&encoded), len, "len {len}");
            assert_eq!(base64_decoded_len(&format!("  {encoded}\n")), len);
        }
    }

    #[test]
    fn base64_cap_refuses_one_byte_over_without_decoding() {
        let at_cap = "A".repeat((FILES_MAX_UPLOAD_BYTES / 3) * 4)
            + &"A".repeat(match FILES_MAX_UPLOAD_BYTES % 3 {
                0 => 0,
                1 => 2,
                _ => 3,
            });
        assert_eq!(base64_decoded_len(&at_cap), FILES_MAX_UPLOAD_BYTES);
        assert!(refuse_oversized_base64(&at_cap).is_ok());
        let over = at_cap + "A";
        assert!(base64_decoded_len(&over) > FILES_MAX_UPLOAD_BYTES);
        assert!(refuse_oversized_base64(&over).is_err());
        assert!(base64_route_body_limit() > (FILES_MAX_UPLOAD_BYTES / 3) * 4 + 1024);
    }

    #[test]
    fn too_large_body_names_code_and_cap() {
        let error = FilesError::too_large(FILES_MAX_UPLOAD_BYTES);
        assert_eq!(error.status, StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(error.code, Some(FILE_TOO_LARGE));
        assert_eq!(error.max_bytes, Some(52_428_800));
        assert_eq!(error.message, "File too large. Maximum size: 50MB");
    }
}
