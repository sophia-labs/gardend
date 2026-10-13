//! Document trash (One tree, 2026-10-08): a document can leave the place
//! without being destroyed, the way a file already can (`files_service.rs`).
//!
//! Trash flags the document's workspace entry (`trashed`, via
//! `workspace.updateDocument`) and writes `documents/{id}/trash.json` beside
//! the content. The entry stays in the workspace Y.Doc, so the title, place,
//! content and history all survive; the snapshot materializer hides a flagged
//! entry from the tree and every navigation read, and the cold document walk
//! skips a document with a trash record, which takes it out of
//! `list_documents`, `search_documents`, `search_blocks` and the semantic index
//! refresh. Restore clears both. Delete forever is the existing
//! `workspace.deleteDocument` path (tombstone, history gone).
//!
//! Crash order: the record is written BEFORE the entry is flagged and removed
//! AFTER it is cleared, so a crash between the steps leaves a live document with
//! a stale record; the listing ignores a record whose document is live, and the
//! next restore clears it.

use crate::app_runtime::AppHandle;
use crate::files_service::{
    artifacts_folder_exists, enqueue_workspace_operation, flush_projection, workspace_snapshot,
    FilesError,
};
use crate::{
    ids::validate_local_id,
    paths::{documents_dir, existing_graph_dir},
    storage::{create_dir_all, read_json, remove_file_if_exists, write_json},
    workspace_entity_projection::{workspace_document_entity, workspace_entity_title},
};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

pub(crate) const DOCUMENT_TRASH_SCHEMA_VERSION: u32 = 1;
const TRASH_FILE: &str = "trash.json";

pub(crate) const DOCUMENT_NOT_FOUND: &str = "document_not_found";
pub(crate) const DOCUMENT_LIVE: &str = "document_live";
pub(crate) const NOT_IN_TRASH: &str = "not_in_trash";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DocumentTrashRecord {
    schema_version: u32,
    document_id: String,
    trashed_at: String,
    title: String,
    parent_id: Option<String>,
}

fn record_path(graph_dir: &Path, document_id: &str) -> PathBuf {
    documents_dir(graph_dir).join(document_id).join(TRASH_FILE)
}

fn bad_id(message: String) -> FilesError {
    FilesError::uncoded(StatusCode::BAD_REQUEST, message)
}

/// Whether a trash record sits beside the document. Cheap and infallible so
/// the cold document walk can call it for every document.
pub(crate) fn document_is_trashed(graph_dir: &Path, document_id: &str) -> bool {
    validate_local_id(document_id, "document_id").is_ok()
        && record_path(graph_dir, document_id).is_file()
}

fn read_record(graph_dir: &Path, document_id: &str) -> Result<Option<DocumentTrashRecord>, FilesError> {
    validate_local_id(document_id, "document_id").map_err(bad_id)?;
    let path = record_path(graph_dir, document_id);
    if !path.is_file() {
        return Ok(None);
    }
    let record: DocumentTrashRecord =
        read_json(&path).map_err(|error| FilesError::internal(error.message()))?;
    if record.schema_version != DOCUMENT_TRASH_SCHEMA_VERSION || record.document_id != document_id {
        return Err(FilesError::internal(format!(
            "trash record for {document_id} is not a schema {DOCUMENT_TRASH_SCHEMA_VERSION} record for that id"
        )));
    }
    Ok(Some(record))
}

fn with_write_lease<T>(
    app: &AppHandle,
    graph_id: &str,
    body: impl FnOnce() -> Result<T, FilesError>,
) -> Result<T, FilesError> {
    let _lease =
        crate::crdt_engine::persistence_coordinator::acquire_hot_write_blocking_if_managed(
            app, graph_id,
        )
        .map_err(FilesError::internal)?;
    let _flush_guard = crate::cell_durability::write_guard();
    body()
}

fn write_record(
    app: &AppHandle,
    graph_id: &str,
    graph_dir: &Path,
    record: &DocumentTrashRecord,
) -> Result<(), FilesError> {
    with_write_lease(app, graph_id, || {
        let path = record_path(graph_dir, &record.document_id);
        if let Some(parent) = path.parent() {
            create_dir_all(parent).map_err(|error| FilesError::internal(error.message()))?;
        }
        write_json(&path, record).map_err(|error| FilesError::internal(error.message()))
    })
}

fn remove_record(
    app: &AppHandle,
    graph_id: &str,
    graph_dir: &Path,
    document_id: &str,
) -> Result<(), FilesError> {
    with_write_lease(app, graph_id, || {
        remove_file_if_exists(&record_path(graph_dir, document_id))
            .map(|_| ())
            .map_err(|error| FilesError::internal(error.message()))
    })
}

fn live_entity(
    app: &AppHandle,
    graph_id: &str,
    document_id: &str,
) -> Result<Option<Value>, FilesError> {
    let snapshot = workspace_snapshot(app, graph_id)?;
    Ok(workspace_document_entity(snapshot.as_ref(), document_id))
}

/// Whether a visible (not trashed) document of this id is in the workspace.
pub(crate) async fn document_is_live(
    app: &AppHandle,
    graph_id: &str,
    document_id: &str,
) -> Result<bool, FilesError> {
    validate_local_id(document_id, "document_id").map_err(bad_id)?;
    flush_projection(app, graph_id).await?;
    Ok(live_entity(app, graph_id, document_id)?.is_some())
}

/// Whether a document of this id is in the trash.
pub(crate) fn document_in_trash(app: &AppHandle, graph_id: &str, document_id: &str) -> bool {
    existing_graph_dir(app, graph_id)
        .map(|dir| document_is_trashed(&dir, document_id))
        .unwrap_or(false)
}

fn trashed_body(graph_id: &str, record: &DocumentTrashRecord, already: bool) -> Value {
    json!({
        "kind": "document",
        "id": record.document_id,
        "documentId": record.document_id,
        "graphId": graph_id,
        "status": "trashed",
        "trashed": true,
        "trashedAt": record.trashed_at,
        "alreadyTrashed": already,
    })
}

/// Take a document out of the place and keep it whole.
pub(crate) async fn trash_document(
    app: &AppHandle,
    graph_id: &str,
    document_id: &str,
) -> Result<Value, FilesError> {
    validate_local_id(document_id, "document_id").map_err(bad_id)?;
    let graph_dir = existing_graph_dir(app, graph_id)
        .map_err(|message| FilesError::uncoded(StatusCode::NOT_FOUND, message))?;
    flush_projection(app, graph_id).await?;
    let Some(entry) = live_entity(app, graph_id, document_id)? else {
        return match read_record(&graph_dir, document_id)? {
            Some(record) => Ok(trashed_body(graph_id, &record, true)),
            None => Err(FilesError::new(
                StatusCode::NOT_FOUND,
                DOCUMENT_NOT_FOUND,
                format!("document not found: {document_id}"),
            )),
        };
    };
    let record = DocumentTrashRecord {
        schema_version: DOCUMENT_TRASH_SCHEMA_VERSION,
        document_id: document_id.to_string(),
        trashed_at: chrono::Utc::now().to_rfc3339(),
        title: workspace_entity_title(&entry).unwrap_or_else(|| document_id.to_string()),
        parent_id: entry
            .get("parentId")
            .and_then(Value::as_str)
            .map(str::to_string),
    };
    write_record(app, graph_id, &graph_dir, &record)?;
    if let Err(error) = enqueue_workspace_operation(
        app,
        "workspace.updateDocument",
        graph_id,
        document_id,
        json!({ "documentId": document_id, "trashed": true }),
    )
    .await
    {
        let _ = remove_record(app, graph_id, &graph_dir, document_id);
        return Err(FilesError::uncoded(StatusCode::BAD_REQUEST, error));
    }
    // Out of the semantic index too (the cold walk no longer lists it, so a
    // refresh will not bring it back).
    let removed = with_write_lease(app, graph_id, || {
        crate::semantic_index_status::remove_document_from_semantic_index(&graph_dir, document_id)
            .map_err(FilesError::internal)
    });
    if let Err(error) = removed {
        log::warn!("[trash] semantic index cleanup for {document_id}: {}", error.message());
    }
    flush_projection(app, graph_id).await?;
    Ok(trashed_body(graph_id, &record, false))
}

/// Trashed documents of a graph as Trash listing items.
pub(crate) fn list_trashed_documents(
    app: &AppHandle,
    graph_id: &str,
) -> Result<Vec<Value>, FilesError> {
    let graph_dir = existing_graph_dir(app, graph_id)
        .map_err(|message| FilesError::uncoded(StatusCode::NOT_FOUND, message))?;
    let snapshot = workspace_snapshot(app, graph_id)?;
    let entries = match std::fs::read_dir(documents_dir(&graph_dir)) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(FilesError::internal(format!("read documents dir: {error}"))),
    };
    let mut items = Vec::new();
    for entry in entries.flatten() {
        let id = entry.file_name().to_string_lossy().into_owned();
        if validate_local_id(&id, "document_id").is_err()
            || workspace_document_entity(snapshot.as_ref(), &id).is_some()
        {
            continue;
        }
        match read_record(&graph_dir, &id) {
            Ok(Some(record)) => items.push(json!({
                "kind": "document",
                "id": record.document_id,
                "documentId": record.document_id,
                "label": record.title,
                "title": record.title,
                "parentId": record.parent_id,
                "trashedAt": record.trashed_at,
            })),
            Ok(None) => {}
            Err(error) => log::warn!(
                "[trash] skipping unreadable document trash record {id}: {}",
                error.message()
            ),
        }
    }
    Ok(items)
}

/// Put a trashed document back: into its folder if that folder still exists,
/// otherwise at the root.
pub(crate) async fn restore_document(
    app: &AppHandle,
    graph_id: &str,
    document_id: &str,
) -> Result<Value, FilesError> {
    validate_local_id(document_id, "document_id").map_err(bad_id)?;
    let graph_dir = existing_graph_dir(app, graph_id)
        .map_err(|message| FilesError::uncoded(StatusCode::NOT_FOUND, message))?;
    let Some(record) = read_record(&graph_dir, document_id)? else {
        return Err(FilesError::new(
            StatusCode::NOT_FOUND,
            NOT_IN_TRASH,
            format!("document {document_id} is not in the trash"),
        ));
    };
    flush_projection(app, graph_id).await?;
    if live_entity(app, graph_id, document_id)?.is_some() {
        remove_record(app, graph_id, &graph_dir, document_id)?;
        return Ok(json!({
            "kind": "document", "id": document_id, "documentId": document_id,
            "graphId": graph_id, "title": record.title, "alreadyLive": true,
        }));
    }
    let parent_id = match record.parent_id.as_deref() {
        Some(parent) if artifacts_folder_exists(app, graph_id, parent)? => Some(parent.to_string()),
        _ => None,
    };
    enqueue_workspace_operation(
        app,
        "workspace.updateDocument",
        graph_id,
        document_id,
        json!({ "documentId": document_id, "trashed": false, "parentId": parent_id }),
    )
    .await
    .map_err(|error| {
        if error.contains("not found") {
            FilesError::new(StatusCode::GONE, DOCUMENT_NOT_FOUND, error)
        } else {
            FilesError::uncoded(StatusCode::BAD_REQUEST, error)
        }
    })?;
    flush_projection(app, graph_id).await?;
    remove_record(app, graph_id, &graph_dir, document_id)?;
    Ok(json!({
        "kind": "document", "id": document_id, "documentId": document_id,
        "graphId": graph_id, "title": record.title,
        "parentId": parent_id, "restoredToParentId": parent_id,
    }))
}

/// Delete a trashed document for good: the existing delete path (tombstone,
/// content and history removed). The semantic index refreshes by itself.
pub(crate) async fn purge_document(
    app: &AppHandle,
    graph_id: &str,
    document_id: &str,
) -> Result<Value, FilesError> {
    validate_local_id(document_id, "document_id").map_err(bad_id)?;
    let graph_dir = existing_graph_dir(app, graph_id)
        .map_err(|message| FilesError::uncoded(StatusCode::NOT_FOUND, message))?;
    if read_record(&graph_dir, document_id)?.is_none() {
        return Err(FilesError::new(
            StatusCode::NOT_FOUND,
            NOT_IN_TRASH,
            format!("document {document_id} is not in the trash"),
        ));
    }
    flush_projection(app, graph_id).await?;
    if live_entity(app, graph_id, document_id)?.is_some() {
        return Err(FilesError::new(
            StatusCode::CONFLICT,
            DOCUMENT_LIVE,
            format!("document {document_id} is in the workspace; trash it first"),
        ));
    }
    enqueue_workspace_operation(
        app,
        "workspace.deleteDocument",
        graph_id,
        document_id,
        json!({}),
    )
    .await
    .map_err(|error| FilesError::uncoded(StatusCode::BAD_REQUEST, error))?;
    flush_projection(app, graph_id).await?;
    Ok(json!({ "kind": "document", "documentId": document_id, "graphId": graph_id, "purged": true }))
}
