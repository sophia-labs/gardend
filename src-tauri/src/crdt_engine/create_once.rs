//! Conservative first creation, deliberately separate from document.write.
//!
//! The executor's managed graph hot-write lease spans every check, admission,
//! and content attempt. A retained admission is never a permission to replay,
//! even for the same recovered operation. Partial effects require inspection.

use crate::{app_runtime::AppHandle, crdt_queue::CrdtOperation};
#[cfg(feature = "desktop")]
use tauri::Manager;
use serde_json::{json, Value};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};
use yrs::{updates::decoder::Decode, Array, Doc, Map, ReadTxn, Transact, Update};

use super::{
    executor::{ApplyOperationError, ApplyOperationResult},
    rooms::RoomRegistry,
};

pub(crate) const ADMISSION_DIR: &str = "document-create-once";

pub(crate) fn admission_path(graph_dir: &Path, document_id: &str) -> Result<PathBuf, String> {
    crate::ids::validate_local_id(document_id, "document_id")?;
    Ok(graph_dir
        .join(ADMISSION_DIR)
        .join(format!("{document_id}.json")))
}

fn refused(error: impl std::fmt::Display) -> ApplyOperationError {
    ApplyOperationError::terminal(format!("create_document_once refused: {error}"))
}

fn uncertain(error: impl std::fmt::Display) -> ApplyOperationError {
    ApplyOperationError::terminal(format!(
        "create_document_once uncertain: admission may be retained; inspect without resending: {error}"
    ))
}

/// `exists`/`is_file` would hide permission errors and dangling symlinks.
fn present(path: &Path) -> Result<bool, String> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(format!("inspect {}: {error}", path.display())),
    }
}

fn plain_directory_if_present(path: &Path) -> Result<(), String> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() && !meta.file_type().is_symlink() => Ok(()),
        Ok(_) => Err(format!(
            "authority directory is not a plain directory: {}",
            path.display()
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("inspect {}: {error}", path.display())),
    }
}

fn reject_unknown(object: &serde_json::Map<String, Value>, allowed: &[&str]) -> Result<(), String> {
    if let Some(key) = object.keys().find(|key| !allowed.contains(&key.as_str())) {
        return Err(format!("unsupported create_document_once field {key}"));
    }
    Ok(())
}

fn required_string<'a>(value: &'a Value, key: &str) -> Result<&'a str, String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty() && s.trim() == *s)
        .ok_or_else(|| format!("{key} must be a non-empty unpadded string"))
}

fn validate_content(value: &Value, depth: usize) -> Result<(), String> {
    if depth > 64 {
        return Err("TipTap nesting exceeds 64".into());
    }
    let node = value.as_object().ok_or("TipTap nodes must be objects")?;
    let kind = required_string(value, "type")?;
    if kind == "text" {
        reject_unknown(node, &["type", "text", "marks"])?;
        required_string(value, "text").or_else(|_| {
            value
                .get("text")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| "TipTap text must be a non-empty string".to_string())
        })?;
        if let Some(marks) = value.get("marks") {
            let marks = marks.as_array().ok_or("TipTap marks must be an array")?;
            for mark in marks {
                let object = mark.as_object().ok_or("TipTap marks must be objects")?;
                reject_unknown(object, &["type", "attrs"])?;
                required_string(mark, "type")?;
                if mark.get("attrs").is_some_and(|attrs| !attrs.is_object()) {
                    return Err("TipTap mark attrs must be an object".into());
                }
            }
        }
    } else {
        reject_unknown(node, &["type", "attrs", "content"])?;
        if value.get("attrs").is_some_and(|attrs| !attrs.is_object()) {
            return Err("TipTap attrs must be an object".into());
        }
        if let Some(content) = value.get("content") {
            for child in content
                .as_array()
                .ok_or("TipTap content must be an array")?
            {
                validate_content(child, depth + 1)?;
            }
        }
    }
    Ok(())
}

/// Revalidate at execution, including generic CRDT and recovered callers.
fn validate_payload(payload: &Value, document_id: &str) -> Result<(), String> {
    let object = payload
        .as_object()
        .ok_or("createOnce payload must be an object")?;
    reject_unknown(
        object,
        &[
            "documentId",
            "title",
            "tiptapJson",
            "order",
            "parentId",
            "awaitDurable",
            crate::crdt_queue::GRAPH_INCARNATION_PAYLOAD_KEY,
            crate::crdt_queue::RECOVERED_OPERATION_PAYLOAD_KEY,
        ],
    )?;
    if required_string(payload, "documentId")? != document_id {
        return Err("document ID carriers disagree".into());
    }
    crate::ids::validate_local_id(document_id, "document_id")?;
    let title = required_string(payload, "title")?;
    if crate::ids::normalize_title(title)? != title {
        return Err("title must already be normalized".into());
    }
    if !payload
        .get("order")
        .and_then(Value::as_f64)
        .is_some_and(f64::is_finite)
    {
        return Err("order must be an explicit finite number".into());
    }
    if payload.get("parentId").is_some_and(|v| !v.is_null()) {
        return Err("create_document_once supports root parentId:null only".into());
    }
    if payload.get("awaitDurable").is_some_and(|v| !v.is_boolean()) {
        return Err("awaitDurable must be a boolean".into());
    }
    let tiptap = payload.get("tiptapJson").ok_or("tiptapJson is required")?;
    if tiptap.get("type").and_then(Value::as_str) != Some("doc") {
        return Err("tiptapJson.type must be doc".into());
    }
    reject_unknown(
        tiptap.as_object().ok_or("tiptapJson must be an object")?,
        &["type", "content"],
    )?;
    let blocks = tiptap
        .get("content")
        .and_then(Value::as_array)
        .ok_or("tiptapJson.content must be an array")?;
    if blocks.is_empty() {
        return Err("tiptapJson.content must not be empty".into());
    }
    let mut ids = std::collections::HashSet::new();
    for block in blocks {
        validate_content(block, 0)?;
        let id = block
            .get("attrs")
            .and_then(|attrs| attrs.get("data-block-id"))
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .ok_or("each block requires data-block-id")?;
        if !ids.insert(id) {
            return Err("duplicate data-block-id".into());
        }
    }
    Ok(())
}

pub(crate) fn mcp_input(arguments: &Value) -> Result<(String, String, Value), String> {
    reject_unknown(
        arguments.as_object().ok_or("arguments must be an object")?,
        &[
            "graph_id",
            "graphIncarnation",
            "graph_incarnation",
            "document_id",
            "title",
            "tiptapJson",
            "order",
            "parentId",
            "awaitDurable",
            "requireDurable",
        ],
    )?;
    let graph_id = required_string(arguments, "graph_id")?.to_string();
    crate::ids::validate_local_id(&graph_id, "graph_id")?;
    let document_id = required_string(arguments, "document_id")?.to_string();
    if arguments.get("requireDurable").is_some_and(|value| !value.is_boolean()) {
        return Err("requireDurable must be a boolean".into());
    }
    if arguments.get("requireDurable") == Some(&Value::Bool(true))
        && arguments.get("awaitDurable") == Some(&Value::Bool(false))
    {
        return Err("requireDurable requires awaitDurable".into());
    }
    let mut payload = json!({
        "documentId": document_id, "title": arguments.get("title"),
        "tiptapJson": arguments.get("tiptapJson"), "order": arguments.get("order"),
        "parentId": arguments.get("parentId").cloned().unwrap_or(Value::Null),
        "awaitDurable": arguments.get("awaitDurable").cloned().unwrap_or(Value::Bool(true)),
    });
    if let Some(expected) = crate::graph_incarnation_admission::expected_incarnation(arguments)
        .map_err(crate::app_error::AppError::message)?
    {
        payload[crate::crdt_queue::GRAPH_INCARNATION_PAYLOAD_KEY] = json!(expected);
    }
    validate_payload(&payload, &document_id)?;
    Ok((graph_id, document_id, payload))
}

fn workspace_has_document(doc: &Doc, document_id: &str) -> Result<bool, String> {
    let txn = doc.transact();
    if txn.has_missing_updates() {
        return Err("workspace authority has missing CRDT updates".into());
    }
    if txn.root_refs().any(|(name, value)| {
        name == "documents" && !matches!(value, yrs::Out::YMap(_) | yrs::Out::UndefinedRef(_))
    }) {
        return Err("workspace documents authority is not a map".into());
    }
    // Root type tags are not carried in a Yjs update, so freshly decoded
    // legitimate maps are UndefinedRef. Read their map/sequence components
    // without installing a root or mutating the resident workspace.
    if txn
        .get_array("documents")
        .is_some_and(|array| array.len(&txn) != 0)
    {
        return Err("workspace documents authority contains a sequence".into());
    }
    if txn.get_map("documents").is_some_and(|documents| {
        documents
            .iter(&txn)
            .any(|(_, value)| !matches!(value, yrs::Out::YMap(_)))
    }) {
        return Err("workspace document entry is not a map".into());
    }
    Ok(txn
        .get_map("documents")
        .is_some_and(|documents| documents.contains_key(&txn, document_id)))
}

async fn require_no_authority(
    app: &AppHandle,
    graph_dir: &Path,
    graph_id: &str,
    document_id: &str,
) -> Result<(), String> {
    for directory in [
        "documents",
        "ydocs",
        "ydocs/documents",
        "ydocs/workspace",
        "ydocs/document-tombstones",
        ADMISSION_DIR,
    ] {
        plain_directory_if_present(&graph_dir.join(directory))?;
    }
    for (label, path) in [
        ("prior admission", admission_path(graph_dir, document_id)?),
        (
            "document authority",
            crate::document_paths::document_dir(graph_dir, document_id)?,
        ),
        (
            "document sidecar",
            crate::ydoc_paths::document_ydoc_dir(graph_dir, document_id),
        ),
        (
            "deletion fence",
            graph_dir
                .join("ydocs/document-tombstones")
                .join(format!("{document_id}.json")),
        ),
    ] {
        if present(&path)? {
            return Err(format!("{label} exists for {document_id}"));
        }
    }
    let registry = app.state::<RoomRegistry>();
    if registry
        .existing_room(&format!("doc:{graph_id}:{document_id}"))?
        .is_some()
    {
        return Err(format!("document room exists for {document_id}"));
    }
    if let Some(room) = registry.existing_room(&format!("workspace:{graph_id}"))? {
        if room
            .with_doc(|doc| workspace_has_document(doc, document_id))
            .await?
        {
            return Err(format!("hot workspace authority exists for {document_id}"));
        }
    }
    let workspace_path = crate::ydoc_paths::workspace_ydoc_state_path(graph_dir);
    if present(&workspace_path)? {
        if !fs::symlink_metadata(&workspace_path)
            .map_err(|e| e.to_string())?
            .is_file()
        {
            return Err("workspace sidecar is not a plain file".into());
        }
        let bytes =
            fs::read(&workspace_path).map_err(|e| format!("read workspace authority: {e}"))?;
        let doc = Doc::new();
        doc.transact_mut()
            .apply_update(
                Update::decode_v1(&bytes)
                    .map_err(|e| format!("decode workspace authority: {e}"))?,
            )
            .map_err(|e| format!("apply workspace authority: {e}"))?;
        if workspace_has_document(&doc, document_id)? {
            return Err(format!(
                "workspace sidecar authority exists for {document_id}"
            ));
        }
    }
    let snapshot_path = crate::ydoc_paths::workspace_snapshot_path(graph_dir);
    if present(&snapshot_path)? {
        if !fs::symlink_metadata(&snapshot_path)
            .map_err(|e| e.to_string())?
            .is_file()
        {
            return Err("workspace snapshot is not a plain file".into());
        }
        let snapshot: Value =
            crate::storage::read_json(&snapshot_path).map_err(|e| e.to_string())?;
        if snapshot.get("schemaVersion").and_then(Value::as_u64) != Some(1)
            || snapshot.get("graphId").and_then(Value::as_str) != Some(graph_id)
        {
            return Err("unsupported or mismatched workspace snapshot authority".into());
        }
        let documents = snapshot
            .get("documents")
            .and_then(Value::as_array)
            .ok_or("malformed workspace snapshot documents")?;
        for document in documents {
            let id = document
                .get("id")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
                .ok_or("malformed workspace document ID")?;
            if id == document_id {
                return Err(format!(
                    "workspace snapshot authority exists for {document_id}"
                ));
            }
        }
    }
    Ok(())
}

fn retain_admission(
    graph_dir: &Path,
    document_id: &str,
    graph_incarnation: &str,
    operation_id: &str,
) -> Result<(), String> {
    let _durability_guard = crate::cell_durability::write_guard();
    let path = admission_path(graph_dir, document_id)?;
    let parent = path.parent().ok_or("admission has no parent")?;
    crate::storage::create_dir_all(parent).map_err(|e| e.to_string())?;
    let mut options = fs::OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    // Never remove a failed/partial fence. Even malformed bytes fail closed.
    let mut file = options
        .open(&path)
        .map_err(|e| format!("reserve admission: {e}"))?;
    let bytes = serde_json::to_vec(&json!({
        "schemaVersion": 1, "documentId": document_id, "graphIncarnation": graph_incarnation,
        "operationId": operation_id, "state": "admitted", "admittedAt": crate::clock::timestamp(),
    }))
    .map_err(|e| e.to_string())?;
    file.write_all(&bytes)
        .and_then(|_| file.sync_all())
        .map_err(|e| format!("persist admission: {e}"))?;
    crate::storage_atomic::sync_parent_dir(parent)?;
    Ok(())
}

#[cfg(test)]
pub(crate) fn retain_admission_for_test(graph_dir: &Path, document_id: &str) -> Result<(), String> {
    retain_admission(
        graph_dir,
        document_id,
        "test-graph-incarnation",
        "test-operation",
    )
}

pub(crate) async fn apply(
    app: &AppHandle,
    operation: &CrdtOperation,
) -> ApplyOperationResult<Value> {
    if app
        .try_state::<super::persistence_coordinator::GraphPersistenceCoordinator>()
        .is_none()
    {
        return Err(refused("managed graph persistence authority is required"));
    }
    let document_id = operation
        .document_id
        .as_deref()
        .ok_or_else(|| refused("document_id is required"))?;
    validate_payload(&operation.payload, document_id).map_err(refused)?;
    let (graph_dir, graph) =
        crate::graph_record_store::read_graph_record_no_heal(app, &operation.graph_id)
            .map_err(refused)?;
    let incarnation = graph
        .incarnation_id
        .as_deref()
        .ok_or_else(|| refused("graph incarnation is missing"))?;
    if operation
        .payload
        .get(crate::crdt_queue::GRAPH_INCARNATION_PAYLOAD_KEY)
        .and_then(Value::as_str)
        != Some(incarnation)
    {
        return Err(refused("current graph incarnation token is required"));
    }
    require_no_authority(app, &graph_dir, &operation.graph_id, document_id)
        .await
        .map_err(refused)?;
    retain_admission(
        &graph_dir,
        document_id,
        incarnation,
        &operation.operation_id,
    )
    .map_err(uncertain)?;
    #[cfg(test)]
    after_admission_for_test(&graph_dir, document_id, &operation.operation_id).await?;
    let mut write = operation.clone();
    // Internal only: absence was decided under the execution lease. This
    // payload never comes from a public flag and no subsequent attempt can
    // reach this writer through createOnce after an admission was retained.
    write.payload["expectedRevision"] = json!(0);
    super::document_ops::document_write_classified(app, &write)
        .await
        .map_err(uncertain)
}

#[cfg(test)]
enum TestAction {
    AdmissionFailure,
    HotFailure,
    DocumentTailFailure,
    WorkspaceTailFailure,
    Pause {
        entered: tokio::sync::oneshot::Sender<()>,
        resume: std::sync::Arc<tokio::sync::Notify>,
    },
}

#[cfg(test)]
static TEST_ACTION: std::sync::Mutex<Option<(String, TestAction)>> = std::sync::Mutex::new(None);

#[cfg(test)]
async fn after_admission_for_test(
    graph_dir: &Path,
    document_id: &str,
    operation_id: &str,
) -> ApplyOperationResult<()> {
    let action = {
        let mut slot = TEST_ACTION.lock().unwrap();
        if slot.as_ref().is_some_and(|(id, _)| id == operation_id) {
            slot.take().map(|(_, action)| action)
        } else {
            None
        }
    };
    match action {
        Some(TestAction::AdmissionFailure) => {
            return Err(uncertain("injected failure after admission"))
        }
        Some(TestAction::HotFailure) => {
            super::document_ops::fail_next_document_write_after_hot_for_test(operation_id)
        }
        Some(TestAction::DocumentTailFailure) => {
            let directory =
                crate::document_paths::document_dir(graph_dir, document_id).map_err(uncertain)?;
            fs::create_dir_all(&directory).map_err(uncertain)?;
            crate::storage::write_bytes(
                &directory.join("history"),
                b"injected history obstruction",
            )
            .map_err(uncertain)?;
        }
        Some(TestAction::WorkspaceTailFailure) => {
            fs::create_dir_all(crate::ydoc_paths::workspace_snapshot_path(graph_dir))
                .map_err(uncertain)?;
        }
        Some(TestAction::Pause { entered, resume }) => {
            let _ = entered.send(());
            resume.notified().await;
        }
        None => {}
    }
    Ok(())
}

#[cfg(all(test, feature = "headless", not(feature = "desktop")))]
#[path = "create_once_tests.rs"]
mod tests;
