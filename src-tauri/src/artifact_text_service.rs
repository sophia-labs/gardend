//! Bounded text artifacts. Original bytes remain authoritative; workspace
//! content identity is a rebuildable invalidation hint. The queue owns replay,
//! while graph-scoped receipts bind caller IDs to exact mutation requests.
use crate::{
    app_error::{AppError, AppResult},
    app_runtime::AppHandle,
    crdt_engine::{
        executor::{ApplyOperationError, ApplyOperationResult},
        persistence_coordinator::GraphPersistenceCoordinator,
        workspace_ops,
    },
    crdt_queue::{CrdtOperation, EnqueueCrdtOperationInput},
    original_file_types::OriginalFileManifest,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    fs,
    path::{Path, PathBuf},
};
#[cfg(feature = "desktop")]
use tauri::Manager;
use yrs::{Any, Map, Out, ReadTxn, Transact, WriteTxn};

pub(crate) const MAX_TEXT_BYTES: usize = 2 * 1024 * 1024;
const OWNER_FILE: &str = "text-owner.json";

pub(crate) fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn write_record<T: Serialize>(path: &Path, value: &T) -> AppResult<()> {
    let _guard = crate::cell_durability::write_guard();
    crate::storage::write_json(path, value)
}
fn conflict(message: impl Into<String>) -> AppError {
    AppError::conflict(message).with_code("artifact_content_conflict")
}
fn present(path: &Path) -> AppResult<bool> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => {
            Err(AppError::storage("artifact authority contains a symlink"))
        }
        Ok(_) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(AppError::storage(e.to_string())),
    }
}
fn artifact_root(dir: &Path, id: &str) -> AppResult<PathBuf> {
    crate::ids::validate_local_id(id, "artifact_id").map_err(AppError::validation)?;
    let base = crate::paths::artifacts_dir(dir);
    present(&base)?;
    let root = base.join(id);
    present(&root)?;
    Ok(root)
}
pub(crate) fn refuse_legacy_writer(dir: &Path, id: &str) -> AppResult<()> {
    if present(&artifact_root(dir, id)?.join(OWNER_FILE))? {
        return Err(conflict(
            "text-owned artifact requires the guarded text mutation API",
        ));
    }
    Ok(())
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct TextMutation {
    pub graph_incarnation: String,
    pub artifact_id: String,
    #[serde(rename = "textOperationId", alias = "operationId")]
    pub operation_id: String,
    pub mode: String,
    pub expected_content_sha256: Option<String>,
    pub text: Option<String>,
    pub filename: Option<String>,
    pub mime_type: Option<String>,
    pub parent_id: Option<String>,
    pub revision_id: Option<String>,
}
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Owner {
    schema_version: u32,
    graph_incarnation: String,
    pending: Option<String>,
    pending_digest: Option<String>,
}
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Receipt {
    schema_version: u32,
    request_digest: String,
    artifact_id: String,
    operation_id: String,
    graph_incarnation: String,
    filename: String,
    mime_type: String,
    before_hash: Option<String>,
    content_hash_sha256: String,
    revision_id: String,
    before_revision_id: Option<String>,
    created_at: u128,
    stage: String,
    create: bool,
    parent_id: Option<String>,
    noop: bool,
}
fn mime_ok(mime: &str) -> bool {
    matches!(mime, "text/html" | "text/plain")
}
fn strict_text(bytes: Vec<u8>, mime: &str) -> AppResult<String> {
    if !mime_ok(mime) {
        return Err(AppError::validation(
            "strict text supports text/html or text/plain UTF-8 only",
        ));
    }
    if bytes.len() > MAX_TEXT_BYTES {
        return Err(AppError::capacity("artifact text exceeds 2 MiB"));
    }
    String::from_utf8(bytes).map_err(|_| AppError::validation("artifact text is not valid UTF-8"))
}
fn bounded_original(dir: &Path) -> AppResult<(OriginalFileManifest, Vec<u8>)> {
    present(dir)?;
    let manifest = crate::original_file_manifest_store::read_original_manifest(dir)
        .map_err(AppError::storage)?;
    let path =
        crate::original_file_manifest_store::original_manifest_file_path(dir, &manifest.filename)
            .map_err(AppError::storage)?;
    present(&path)?;
    if fs::metadata(&path)
        .map_err(|e| AppError::storage(e.to_string()))?
        .len()
        > MAX_TEXT_BYTES as u64
    {
        return Err(AppError::capacity("artifact text exceeds 2 MiB"));
    }
    let bytes = crate::storage::read_bytes(&path)?;
    strict_text(bytes.clone(), &manifest.mime_type)?;
    Ok((manifest, bytes))
}
fn identity(app: &AppHandle, graph: &str, expected: Option<&str>) -> AppResult<(PathBuf, String)> {
    let (dir, record) = crate::graph_record_store::read_graph_record_no_heal(app, graph)?;
    let incarnation = record
        .incarnation_id
        .ok_or_else(|| conflict("graph incarnation is missing"))?;
    if expected.is_some_and(|v| v != incarnation) {
        return Err(conflict("stale graph incarnation"));
    }
    Ok((dir, incarnation))
}
fn read_owner(root: &Path, incarnation: &str) -> AppResult<Option<Owner>> {
    let path = root.join(OWNER_FILE);
    if !present(&path)? {
        return Ok(None);
    }
    let owner: Owner = crate::storage::read_json(&path)?;
    if owner.schema_version != 1 || owner.graph_incarnation != incarnation {
        return Err(conflict("unsupported or stale text owner"));
    }
    Ok(Some(owner))
}
pub(crate) async fn read_text(app: &AppHandle, graph: &str, id: &str) -> AppResult<Value> {
    let coordinator = app.state::<GraphPersistenceCoordinator>();
    let lease = coordinator
        .acquire_hot_write(graph)
        .await
        .map_err(AppError::storage)?;
    lease.declare_rdf_read_only();
    let (dir, incarnation) = identity(app, graph, None)?;
    let root = artifact_root(&dir, id)?;
    if !present(&root)? {
        return Err(AppError::not_found("artifact not found"));
    }
    let owner = read_owner(&root, &incarnation)?;
    if owner.as_ref().is_some_and(|o| o.pending.is_some()) {
        return Err(conflict(
            "artifact text recovery/tail is pending; retry this read",
        ));
    }
    if owner.is_some() {
        let room = workspace_ops::workspace_room(app, graph, &dir)
            .await
            .map_err(AppError::storage)?;
        let exists = room
            .with_doc(|doc| {
                let txn = doc.transact();
                txn.get_map("artifacts")
                    .is_some_and(|map| map.contains_key(&txn, id))
            })
            .await;
        if !exists {
            return Err(AppError::not_found(
                "text artifact was deleted from the workspace",
            ));
        }
    }
    let (m, bytes) = bounded_original(&root.join("original"))?;
    let content_hash = hash(&bytes);
    let size = bytes.len();
    let text = strict_text(bytes, &m.mime_type)?;
    Ok(
        json!({"graphId":graph,"graphIncarnation":incarnation,"artifactId":id,
        "filename":m.filename,"mimeType":m.mime_type,"sizeBytes":size,
        "contentHashSha256":content_hash,"text":text}),
    )
}
fn validate(input: &TextMutation) -> AppResult<()> {
    for (id, label) in [
        (&input.artifact_id, "artifact_id"),
        (&input.operation_id, "operation_id"),
    ] {
        crate::ids::validate_local_id(id, label).map_err(AppError::validation)?;
    }
    if input.graph_incarnation.is_empty() {
        return Err(AppError::validation("graph_incarnation is required"));
    }
    if let Some(text) = &input.text {
        if text.len() > MAX_TEXT_BYTES {
            return Err(AppError::capacity("artifact text exceeds 2 MiB"));
        }
    }
    match input.mode.as_str() {
        "create" => {
            let name = input
                .filename
                .as_deref()
                .ok_or_else(|| AppError::validation("filename is required"))?;
            if name.is_empty() || name == "manifest.json" || crate::ids::safe_filename(name) != name
            {
                return Err(AppError::validation(
                    "filename must be a safe single component",
                ));
            }
            if !mime_ok(input.mime_type.as_deref().unwrap_or(""))
                || input.text.is_none()
                || input.expected_content_sha256.is_some()
                || input.revision_id.is_some()
            {
                return Err(AppError::validation("invalid text creation fields"));
            }
            if let Some(parent) = &input.parent_id {
                crate::ids::validate_local_id(parent, "parent_id").map_err(AppError::validation)?;
            }
        }
        "write" | "restore" => {
            if !input.expected_content_sha256.as_deref().is_some_and(|s| {
                s.len() == 64
                    && s.bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            }) {
                return Err(AppError::validation(
                    "expected_content_sha256 must be a lowercase SHA-256",
                ));
            }
            if input.filename.is_some() || input.mime_type.is_some() || input.parent_id.is_some() {
                return Err(AppError::validation(
                    "text edits preserve MIME, filename and placement",
                ));
            }
            if input.mode == "write" && (input.text.is_none() || input.revision_id.is_some()) {
                return Err(AppError::validation("write requires text only"));
            }
            if input.mode == "restore" {
                if input.text.is_some() {
                    return Err(AppError::validation("restore must not contain text"));
                }
                crate::ids::validate_local_id(
                    input.revision_id.as_deref().unwrap_or(""),
                    "revision_id",
                )
                .map_err(AppError::validation)?;
            }
        }
        _ => return Err(AppError::validation("unknown text mutation mode")),
    }
    Ok(())
}
pub(crate) async fn submit(app: AppHandle, graph: String, input: TextMutation) -> AppResult<Value> {
    validate(&input)?;
    crate::crdt_queue::enqueue_crdt_operation(
        app,
        EnqueueCrdtOperationInput {
            kind: "artifact.mutateText".into(),
            graph_id: graph,
            document_id: Some(input.artifact_id.clone()),
            payload: serde_json::to_value(input)
                .map_err(|e| AppError::serialization(e.to_string()))?,
        },
    )
    .await
    .map_err(|e| {
        if e.contains("artifact_content_conflict") || e.contains("stale graph incarnation") {
            conflict(e)
        } else if e.contains("timed out") {
            AppError::deadline(e).with_code("artifact_text_tail_pending")
        } else {
            AppError::storage(e).with_code("artifact_text_recovery_required")
        }
    })
}
pub(crate) async fn mcp_mutate(app: AppHandle, args: &Value, mode: &str) -> AppResult<Value> {
    let object = args
        .as_object()
        .ok_or_else(|| AppError::validation("arguments must be an object"))?;
    let allowed = if mode == "create" {
        &[
            "graph_id",
            "graph_incarnation",
            "artifact_id",
            "operation_id",
            "filename",
            "mime_type",
            "text",
            "parent_id",
        ][..]
    } else {
        &[
            "graph_id",
            "graph_incarnation",
            "artifact_id",
            "operation_id",
            "expected_content_sha256",
            "text",
        ][..]
    };
    if let Some(key) = object.keys().find(|key| !allowed.contains(&key.as_str())) {
        return Err(AppError::validation(format!(
            "unsupported text mutation field {key}"
        )));
    }
    if object.values().any(|v| !v.is_string()) {
        return Err(AppError::validation(
            "text mutation arguments must be strings",
        ));
    }
    let required = |key: &str| {
        args.get(key)
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| AppError::validation(format!("{key} is required")))
    };
    let optional = |key: &str| args.get(key).and_then(Value::as_str).map(str::to_string);
    let graph = required("graph_id")?;
    let input = TextMutation {
        graph_incarnation: required("graph_incarnation")?,
        artifact_id: required("artifact_id")?,
        operation_id: required("operation_id")?,
        mode: mode.into(),
        expected_content_sha256: optional("expected_content_sha256"),
        text: optional("text"),
        filename: optional("filename"),
        mime_type: optional("mime_type"),
        parent_id: optional("parent_id"),
        revision_id: optional("revision_id"),
    };
    submit(app, graph, input).await
}
fn receipt_result(r: &Receipt, replayed: bool) -> Value {
    json!({"status":"committed","artifactId":r.artifact_id,"graphIncarnation":r.graph_incarnation,
        "operationId":r.operation_id,"contentHashSha256":r.content_hash_sha256,"revisionId":if r.noop{None}else{Some(&r.revision_id)},
        "noop":r.noop,"replayed":replayed,"refreshPending":false})
}
fn prepare_snapshot(root: &Path, id: &str, name: &str, mime: &str, bytes: &[u8]) -> AppResult<()> {
    let _guard = crate::cell_durability::write_guard();
    let dir = root.join("revisions").join(id);
    present(&root.join("revisions"))?;
    present(&dir)?;
    if present(&dir.join("manifest.json"))? {
        let (m, b) = bounded_original(&dir)?;
        if b != bytes || m.filename != name || m.mime_type != mime {
            return Err(conflict("prepared snapshot disagrees with operation"));
        }
        return Ok(());
    }
    crate::original_file_storage::save_original_bytes_to_dir(&dir, name, mime, bytes)
        .map_err(AppError::storage)?;
    Ok(())
}
pub(crate) async fn apply(
    app: &AppHandle,
    operation: &CrdtOperation,
) -> ApplyOperationResult<Value> {
    let mut committed = false;
    let result = apply_inner(app, operation, &mut committed).await;
    result.map_err(|e| {
        let message=format!("{}: {}",e.code().unwrap_or("artifact_text_mutation_failed"),e.message_ref());
        if committed { ApplyOperationError::retryable_after_hot_commit(message) }
        else { ApplyOperationError::terminal(format!("{message}; any retained prepared operation must be retried with the same operation_id")) }
    })
}
async fn apply_inner(
    app: &AppHandle,
    operation: &CrdtOperation,
    committed: &mut bool,
) -> AppResult<Value> {
    let input: TextMutation = serde_json::from_value(operation.payload.clone())
        .map_err(|e| AppError::validation(e.to_string()))?;
    validate(&input)?;
    if operation.document_id.as_deref() != Some(input.artifact_id.as_str()) {
        return Err(conflict("artifact ID carriers disagree"));
    }
    let (dir, incarnation) = identity(app, &operation.graph_id, Some(&input.graph_incarnation))?;
    let root = artifact_root(&dir, &input.artifact_id)?;
    let operations = dir.join("artifact-text-operations");
    present(&operations)?;
    let key = hash(input.operation_id.as_bytes());
    let receipt_path = operations.join(format!("{key}.json"));
    present(&receipt_path)?;
    let digest =
        hash(&serde_json::to_vec(&input).map_err(|e| AppError::serialization(e.to_string()))?);
    let room = workspace_ops::workspace_room(app, &operation.graph_id, &dir)
        .await
        .map_err(AppError::storage)?;
    let exists = room
        .with_doc(|doc| {
            let txn = doc.transact();
            txn.get_map("artifacts")
                .is_some_and(|m| m.contains_key(&txn, &input.artifact_id))
        })
        .await;
    let mut owner = read_owner(&root, &incarnation)?;
    let mut receipt: Receipt = if receipt_path.is_file() {
        let r: Receipt = crate::storage::read_json(&receipt_path)?;
        if r.schema_version != 1
            || r.request_digest != digest
            || r.graph_incarnation != incarnation
            || r.artifact_id != input.artifact_id
        {
            return Err(conflict("operation_id is already bound to another request"));
        }
        if r.stage == "completed" {
            if let Some(o) = owner.as_mut() {
                if o.pending.as_deref() == Some(&input.operation_id) {
                    o.pending = None;
                    o.pending_digest = None;
                    write_record(&root.join(OWNER_FILE), o)?;
                }
            }
            return Ok(receipt_result(&r, true));
        }
        r
    } else {
        if owner
            .as_ref()
            .and_then(|o| o.pending.as_deref())
            .is_some_and(|p| p != input.operation_id)
        {
            return Err(conflict("another text operation requires recovery"));
        }
        if owner
            .as_ref()
            .and_then(|o| o.pending_digest.as_deref())
            .is_some_and(|d| d != digest)
        {
            return Err(conflict(
                "operation_id is already bound to a prepared request",
            ));
        }
        let create = input.mode == "create";
        if create && (exists || (present(&root)? && owner.is_none())) {
            return Err(conflict("artifact or orphan bytes already exist"));
        }
        if !create && (!exists || owner.is_none()) {
            return Err(conflict("edit requires a live text-owned artifact"));
        }
        let current = if create {
            None
        } else {
            Some(bounded_original(&root.join("original"))?)
        };
        let before_hash = current.as_ref().map(|(_, b)| hash(b));
        if !create && before_hash != input.expected_content_sha256 {
            return Err(conflict("expected current content SHA-256 does not match"));
        }
        let (filename, mime) = if let Some((m, _)) = &current {
            (m.filename.clone(), m.mime_type.clone())
        } else {
            (
                input.filename.clone().unwrap(),
                input.mime_type.clone().unwrap(),
            )
        };
        let bytes = if input.mode == "restore" {
            let (m, b) = bounded_original(
                &root
                    .join("revisions")
                    .join(input.revision_id.as_ref().unwrap()),
            )?;
            if m.filename != filename || m.mime_type != mime {
                return Err(conflict("restore changes immutable text MIME/filename"));
            }
            let history = crate::artifact_revisions::list_artifact_revisions(
                app,
                &operation.graph_id,
                &input.artifact_id,
            )?;
            let entry = history
                .iter()
                .find(|entry| Some(&entry.revision_id) == input.revision_id.as_ref())
                .ok_or_else(|| conflict("restore revision is not published history"))?;
            if entry.content_hash_sha256 != hash(&b)
                || entry.size_bytes != b.len()
                || entry.filename != m.filename
                || entry.mime_type != m.mime_type
            {
                return Err(conflict(
                    "restore revision bytes disagree with retained history",
                ));
            }
            b
        } else {
            input.text.as_ref().unwrap().as_bytes().to_vec()
        };
        strict_text(bytes.clone(), &mime)?;
        let after = hash(&bytes);
        let noop = before_hash.as_ref() == Some(&after);
        let r = Receipt {
            schema_version: 1,
            request_digest: digest,
            artifact_id: input.artifact_id.clone(),
            operation_id: input.operation_id.clone(),
            graph_incarnation: incarnation.clone(),
            filename: filename.clone(),
            mime_type: mime.clone(),
            before_hash,
            content_hash_sha256: after,
            revision_id: format!("text-{key}"),
            before_revision_id: current.as_ref().map(|_| format!("text-{key}-before")),
            created_at: operation.enqueue_timestamp.parse().unwrap_or(0),
            stage: if noop { "completed" } else { "prepared" }.into(),
            create,
            parent_id: input.parent_id.clone(),
            noop,
        };
        if noop {
            write_record(&receipt_path, &r)?;
            return Ok(receipt_result(&r, false));
        }
        // Reserve before any snapshot. Existing legacy writers test this marker
        // while holding this same graph gate, so create cannot race an upsert.
        owner = Some(Owner {
            schema_version: 1,
            graph_incarnation: incarnation.clone(),
            pending: Some(input.operation_id.clone()),
            pending_digest: Some(r.request_digest.clone()),
        });
        write_record(&root.join(OWNER_FILE), owner.as_ref().unwrap())?;
        if let Some((_, before)) = &current {
            prepare_snapshot(
                &root,
                r.before_revision_id.as_ref().unwrap(),
                &filename,
                &mime,
                before,
            )?;
        }
        prepare_snapshot(&root, &r.revision_id, &filename, &mime, &bytes)?;
        write_record(&receipt_path, &r)?;
        r
    };
    let mut owner = owner.ok_or_else(|| conflict("text receipt has no ownership marker"))?;
    if owner.pending.as_deref() != Some(&input.operation_id) {
        return Err(conflict("text pending owner differs from receipt"));
    }
    if !receipt.create && !exists {
        return Err(conflict(
            "artifact was deleted while a text operation was pending",
        ));
    }
    let (_, new_bytes) = bounded_original(&root.join("revisions").join(&receipt.revision_id))?;
    if hash(&new_bytes) != receipt.content_hash_sha256 {
        return Err(conflict("prepared revision hash changed"));
    }
    if receipt.stage == "prepared" {
        let current = if present(&root.join("original").join("manifest.json"))? {
            Some(bounded_original(&root.join("original"))?)
        } else {
            None
        };
        let observed = current.as_ref().map(|(_, b)| hash(b));
        if observed != receipt.before_hash
            && observed.as_ref() != Some(&receipt.content_hash_sha256)
        {
            return Err(conflict(
                "pending operation cannot overwrite unrelated current content",
            ));
        }
        // Whole-file replacement is the byte authority boundary; subsequent
        // metadata/history/room failures retain a typed retryable queue tail.
        let saved = {
            let _guard = crate::cell_durability::write_guard();
            crate::original_file_storage::save_original_bytes_to_dir(
                &root.join("original"),
                &receipt.filename,
                &receipt.mime_type,
                &new_bytes,
            )
        };
        if let Err(error) = saved {
            *committed = bounded_original(&root.join("original"))
                .is_ok_and(|(_, b)| hash(&b) == receipt.content_hash_sha256);
            return Err(AppError::storage(format!(
                "artifact commit durability uncertain: {error}"
            )));
        }
        *committed = true;
        receipt.stage = "bytesCommitted".into();
        write_record(&receipt_path, &receipt)?;
    } else if receipt.stage == "bytesCommitted" || receipt.stage == "publishingWorkspace" {
        *committed = true;
    } else {
        return Err(conflict("unsupported text receipt stage"));
    }
    let (_, actual) = bounded_original(&root.join("original"))?;
    if hash(&actual) != receipt.content_hash_sha256 {
        return Err(conflict("committed tail cannot overwrite a later edit"));
    }
    crate::artifact_revisions::publish_text_snapshot(
        &root,
        &receipt.revision_id,
        receipt.created_at,
        false,
    )?;
    if let Some(before) = &receipt.before_revision_id {
        crate::artifact_revisions::publish_text_snapshot(&root, before, receipt.created_at, true)?;
    }
    #[cfg(test)]
    if fail_tail_for_test(&input.operation_id) {
        return Err(AppError::storage("injected committed HTML tail failure"));
    }
    let first_publication = receipt.stage == "bytesCommitted";
    if receipt.create && !exists && !first_publication {
        return Err(conflict(
            "workspace publication is uncertain or was deleted; refusing resurrection",
        ));
    }
    receipt.stage = "publishingWorkspace".into();
    write_record(&receipt_path, &receipt)?;
    workspace_ops::update_workspace_doc_checked(&room,|_,txn| {
        let artifacts=txn.get_or_insert_map("artifacts");
        if !artifacts.contains_key(&*txn,&input.artifact_id) {
            if !receipt.create { return Err("text artifact is missing".into()); }
            let data=json!({"label":receipt.filename,"originalFilename":receipt.filename,"mimeType":receipt.mime_type,"sizeBytes":actual.len(),"parentId":receipt.parent_id,"order":receipt.created_at as f64,"updatedAt":receipt.created_at as f64,"status":"ready","storageKey":format!("local://artifacts/{}/original/{}",input.artifact_id,receipt.filename)});
            workspace_ops::put_workspace_artifact(txn,&operation.graph_id,&input.artifact_id,data.as_object().unwrap())?;
        }
        let Some(Out::YMap(map))=artifacts.get(&*txn,&input.artifact_id) else {return Err("artifact entry is not a map".into())};
        map.insert(txn,"contentHashSha256",receipt.content_hash_sha256.as_str());
        map.insert(txn,"contentOperationId",receipt.operation_id.as_str());
        map.insert(txn,"size",Any::Number(actual.len() as f64));
        map.insert(txn,"sf_sizeBytes",Any::Number(actual.len() as f64));
        map.insert(txn,"updatedAt",workspace_ops::epoch_ms_to_iso(receipt.created_at as f64).as_str());
        Ok(())
    }).await.map_err(AppError::storage)?;
    workspace_ops::ensure_workspace_source_effect_persisted(
        app,
        &operation.graph_id,
        &dir,
        &room,
        &operation.operation_id,
    )
    .await
    .map_err(AppError::storage)?;
    receipt.stage = "completed".into();
    write_record(&receipt_path, &receipt)?;
    owner.pending = None;
    owner.pending_digest = None;
    write_record(&root.join(OWNER_FILE), &owner)?;
    Ok(receipt_result(&receipt, false))
}

#[cfg(test)]
static FAIL_TAIL: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);
#[cfg(test)]
fn fail_tail_for_test(id: &str) -> bool {
    let mut slot = FAIL_TAIL.lock().unwrap();
    if slot.as_deref() == Some(id) {
        slot.take();
        true
    } else {
        false
    }
}
#[cfg(test)]
#[path = "artifact_text_tests.rs"]
mod tests;
