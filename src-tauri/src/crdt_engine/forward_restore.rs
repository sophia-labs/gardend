//! Restore as a forward transaction (DESIGN §2.12): for each document, plan current -> snapshot with the
//! minimal-edit engine and apply it through the room, so live rooms, reconnecting sockets and browser caches
//! all converge on the restored content.

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Arc;

use serde::Serialize;
use serde_json::{json, Value};
#[cfg(feature = "desktop")]
use tauri::Manager;
use yrs::types::ToJson;
use yrs::updates::decoder::Decode;
use yrs::{Doc, Map, ReadTxn, StateVector, Transact, TransactionMut, Update, WriteTxn};

use crate::app_runtime::AppHandle;
use crate::crdt_engine::document_ops::{persist_room_document, resolve_document_kind};
use crate::crdt_engine::minimal_edit::attrs::normal_form;
use crate::crdt_engine::minimal_edit::{emit, model, plan, ytree, OpCounts, WritePolicy};
use crate::crdt_engine::projection::tiptap_json_in;
use crate::crdt_engine::rooms::{Room, RoomRegistry};
use crate::crdt_engine::workspace_ops::{
    document_title_in_workspace, persist_workspace, workspace_room,
};
use crate::time_travel_types::{DocumentSnapshotRef, RestorePointManifest};

/// Workspace roots a restore makes equal to the restore point. Never restored: `uploadBatches` (transient
/// upload bookkeeping, workspace_ops.rs:4290) and `customCssReceipts` (replay receipts, custom_css.rs:18).
pub(crate) const WORKSPACE_RESTORE_ROOTS: &[&str] = &[
    "documents",
    "folders",
    "artifacts",
    "wires",
    "ui",
    "customCss",
];
/// Document map roots besides `content`. Never restored: `_operationLog`, the block-op replay ledger
/// (block_ops.rs:274): rewinding it would let a retried operation apply twice.
pub(crate) const DOCUMENT_RESTORE_ROOTS: &[&str] = &["comments"];
/// The Flow board's roots (flow_board.rs:164-165).
pub(crate) const FLOW_BOARD_RESTORE_ROOTS: &[&str] = &["resource", "scene"];

#[derive(Clone, Debug)]
pub(crate) struct RestoreContext {
    pub(crate) restore_point_id: String,
    pub(crate) operation_id: String,
    /// Epoch ms stamped on restored whiteboard elements (`updated`, `customData.sophia.at`).
    pub(crate) now_ms: f64,
}

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ForwardRestoreReport {
    pub(crate) workspace_ops: OpCounts,
    pub(crate) documents_changed: Vec<String>,
    pub(crate) documents_unchanged: Vec<String>,
    /// Deleted after the restore point: left deleted (recreation stays an explicit act).
    pub(crate) documents_skipped_deleted: Vec<String>,
    /// Current Y.Doc could not be opened and no room was live: bytes swapped (the old path, one document).
    pub(crate) documents_cold_restored: Vec<String>,
    pub(crate) ops: OpCounts,
}

/// Decode stored update bytes into a fresh Doc (empty or canonical-empty bytes give an empty Doc).
pub(crate) fn doc_from_update_bytes(bytes: &[u8]) -> Result<Doc, String> {
    let doc = Doc::new();
    if bytes.is_empty() {
        return Ok(doc);
    }
    let update = Update::decode_v1(bytes)
        .map_err(|error| format!("forward restore: decode Y.Doc update: {error}"))?;
    doc.transact_mut()
        .apply_update(update)
        .map_err(|error| format!("forward restore: apply Y.Doc update: {error}"))?;
    Ok(doc)
}

/// Make one document's Y.Doc equal to `target` inside the room's transaction. Kind-aware: TipTap documents
/// through the planner + `comments`; the Flow board through `ytree`; a whiteboard through
/// `crate::whiteboard::restore::restore_elements`. Never touches `_operationLog`.
pub(crate) fn apply_document_restore(
    txn: &mut TransactionMut<'_>,
    target: &Doc,
    kind: Option<&str>,
    ctx: &RestoreContext,
) -> Result<OpCounts, String> {
    if crate::whiteboard::is_whiteboard(kind) {
        return crate::whiteboard::restore::restore_elements(txn, target, ctx);
    }
    if kind == Some(crate::flow_board::FLOW_BOARD_KIND) {
        let target_txn = target.transact();
        return Ok(ytree::restore_map_roots(
            txn,
            FLOW_BOARD_RESTORE_ROOTS,
            &target_txn,
        ));
    }

    // A TipTap document: everything is stated and ids are bound, so the snapshot's content comes back
    // exactly, and no normaliser runs (a restore restores what was there, not what the contract prefers).
    let policy = WritePolicy::restore();
    let target_txn = target.transact();
    let target_json = tiptap_json_in(&target_txn);
    let nodes: Vec<Value> = target_json
        .get("content")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let desired = model::desired_from_tiptap_nodes(&nodes, &policy);
    let fragment = txn.get_or_insert_xml_fragment("content");
    let current = model::read_fragment(&*txn, &fragment, &policy);
    let fragment_plan = plan::plan_fragment(&current, &desired, &policy);
    let stats = emit::emit_fragment(txn, &fragment, &current, &fragment_plan)
        .map_err(|error| format!("forward restore: emit content: {error}"))?;
    let mut counts = stats.counts;
    counts.add(&ytree::restore_map_roots(
        txn,
        DOCUMENT_RESTORE_ROOTS,
        &target_txn,
    ));
    Ok(counts)
}

/// What the restore did to one document.
enum DocumentOutcome {
    Changed(OpCounts),
    Unchanged,
    ColdRestored,
}

/// The restore engine. The caller holds the RestoreGuard, the graph hot-write lease, and runs this inside
/// `restore_guard::as_restore_owner(ctx.operation_id, …)`. Workspace first, then documents sorted by id.
pub(crate) async fn forward_restore_manifest(
    app: &AppHandle,
    graph_id: &str,
    manifest: &RestorePointManifest,
    ctx: &RestoreContext,
) -> Result<ForwardRestoreReport, String> {
    let graph_dir = crate::graph_paths::existing_graph_dir(app, graph_id)?;
    let registry = app.state::<RoomRegistry>();

    // The workspace room is evicted afterwards only when this restore opened it.
    let workspace_key = format!("workspace:{graph_id}");
    let workspace_was_live = registry.peek(&workspace_key).await.is_some();
    let workspace = workspace_room(app, graph_id, &graph_dir).await?;
    let result = restore_workspace_and_documents(
        app, graph_id, &graph_dir, &registry, &workspace, manifest, ctx,
    )
    .await;
    if !workspace_was_live {
        registry.evict_room_if_identity(&workspace_key, &workspace);
    }
    result
}

async fn restore_workspace_and_documents(
    app: &AppHandle,
    graph_id: &str,
    graph_dir: &Path,
    registry: &RoomRegistry,
    workspace: &Arc<Room>,
    manifest: &RestorePointManifest,
    ctx: &RestoreContext,
) -> Result<ForwardRestoreReport, String> {
    let mut report = ForwardRestoreReport::default();
    let mut documents: Vec<&DocumentSnapshotRef> = manifest.documents.iter().collect();
    documents.sort_by(|a, b| a.document_id.cmp(&b.document_id));

    // Deleted after the restore point: left deleted (Q3). Their workspace entries must not come back
    // with the rest of the workspace, so the target workspace loses them before the comparison.
    let mut deleted: BTreeSet<&str> = BTreeSet::new();
    for doc_ref in &documents {
        let id = doc_ref.document_id.as_str();
        if crate::document_tombstone_store::document_is_tombstoned(graph_dir, id)
            .map_err(|error| format!("forward restore: tombstone check for {id}: {error}"))?
        {
            deleted.insert(id);
        }
    }

    // 1. The workspace maps.
    let (workspace_bytes, _snapshot) =
        crate::time_travel_store::read_workspace_bundle(graph_dir, &manifest.restore_point_id)
            .map_err(|error| format!("forward restore: read workspace bundle: {error}"))?;
    let workspace_target = doc_from_update_bytes(&workspace_bytes)?;
    if !deleted.is_empty() {
        let mut txn = workspace_target.transact_mut();
        if let Some(entries) = txn.get_map("documents") {
            for id in &deleted {
                entries.remove(&mut txn, id);
            }
        }
    }
    report.workspace_ops = workspace
        .update_doc(|_doc, txn| {
            let target_txn = workspace_target.transact();
            Ok(ytree::restore_map_roots(
                txn,
                WORKSPACE_RESTORE_ROOTS,
                &target_txn,
            ))
        })
        .await?;
    if !report.workspace_ops.is_zero() {
        persist_workspace(app, graph_id, graph_dir, workspace, &ctx.operation_id).await?;
    }

    // 2. The documents, sorted by id.
    for doc_ref in documents {
        let id = doc_ref.document_id.as_str();
        if deleted.contains(id) {
            report.documents_skipped_deleted.push(id.to_string());
            continue;
        }
        match restore_one_document(
            app, graph_id, graph_dir, registry, workspace, manifest, doc_ref, ctx,
        )
        .await?
        {
            DocumentOutcome::Changed(ops) => {
                report.ops.add(&ops);
                report.documents_changed.push(id.to_string());
            }
            DocumentOutcome::Unchanged => report.documents_unchanged.push(id.to_string()),
            DocumentOutcome::ColdRestored => report.documents_cold_restored.push(id.to_string()),
        }
    }
    Ok(report)
}

#[allow(clippy::too_many_arguments)]
async fn restore_one_document(
    app: &AppHandle,
    graph_id: &str,
    graph_dir: &Path,
    registry: &RoomRegistry,
    workspace: &Arc<Room>,
    manifest: &RestorePointManifest,
    doc_ref: &DocumentSnapshotRef,
    ctx: &RestoreContext,
) -> Result<DocumentOutcome, String> {
    let id = doc_ref.document_id.as_str();
    let key = format!("doc:{graph_id}:{id}");
    let state_path = crate::ydoc_paths::checked_document_ydoc_state_path(graph_dir, id)?;
    let was_live = registry.peek(&key).await.is_some();

    // A document with no record cannot carry a new revision, and a room cannot be opened for it without
    // inventing one: the old byte path recreates it (what apply_restore always did).
    let has_record = crate::paths::existing_document_dir(graph_dir, id).is_ok();
    if !has_record && !was_live {
        return cold_restore(graph_dir, manifest, doc_ref);
    }

    let room = match registry.get_or_create(&key, state_path).await {
        Ok(room) => room,
        Err(error) => {
            // The current Y.Doc cannot be opened. With a live room that is a real fault: refuse. Without
            // one, nothing can hold newer state in memory, so swapping this one document's bytes is safe.
            if registry.existing_room(&key)?.is_some() {
                return Err(format!(
                    "forward restore: document {id}: cannot open its live room: {error}"
                ));
            }
            log::warn!("forward restore: document {id} opened cold ({error}); swapping its bytes");
            return cold_restore(graph_dir, manifest, doc_ref);
        }
    };

    let outcome = restore_document_in_room(
        app, graph_id, graph_dir, workspace, manifest, doc_ref, ctx, &room,
    )
    .await;
    // Only a room this restore opened goes away; a live one keeps serving its subscribers.
    if !was_live {
        registry.evict_room_if_identity(&key, &room);
    }
    outcome
}

fn cold_restore(
    graph_dir: &Path,
    manifest: &RestorePointManifest,
    doc_ref: &DocumentSnapshotRef,
) -> Result<DocumentOutcome, String> {
    crate::time_travel_restore_service::cold_restore_one_document(graph_dir, manifest, doc_ref)
        .map_err(|error| {
            format!(
                "forward restore: cold restore of document {}: {error}",
                doc_ref.document_id
            )
        })?;
    Ok(DocumentOutcome::ColdRestored)
}

#[allow(clippy::too_many_arguments)]
async fn restore_document_in_room(
    app: &AppHandle,
    graph_id: &str,
    graph_dir: &Path,
    workspace: &Arc<Room>,
    manifest: &RestorePointManifest,
    doc_ref: &DocumentSnapshotRef,
    ctx: &RestoreContext,
    room: &Arc<Room>,
) -> Result<DocumentOutcome, String> {
    let id = doc_ref.document_id.as_str();
    let bytes = crate::time_travel_store::read_document_bytes(
        graph_dir,
        &manifest.restore_point_id,
        id,
    )
    .map_err(|error| format!("forward restore: read bytes of document {id}: {error}"))?;
    let target = doc_from_update_bytes(&bytes)?;
    // The kind comes from the workspace just restored.
    let kind = resolve_document_kind(app, graph_id, id).await?;
    let ops = room
        .update_doc(|_doc, txn| apply_document_restore(txn, &target, kind.as_deref(), ctx))
        .await?;
    if ops.is_zero() {
        return Ok(DocumentOutcome::Unchanged);
    }

    // A new revision through the normal save path: record, RDF, history, flow lane, whiteboard body.
    let document_id = id.to_string();
    let title = workspace
        .with_doc(move |doc| document_title_in_workspace(doc, &document_id))
        .await
        .or_else(|| {
            crate::document_history_file_store::read_document_snapshot_payload(
                graph_dir,
                id,
                &doc_ref.snapshot_id,
            )
            .ok()
            .map(|payload| payload.title)
            .filter(|title| !title.is_empty())
        })
        .unwrap_or_else(|| id.to_string());
    persist_room_document(app, graph_id, id, &title, room, &ctx.operation_id).await?;
    Ok(DocumentOutcome::Changed(ops))
}

/// JSON with every number as an f64, so the same value written as an integer or a float compares equal.
fn normalize_numbers(value: Value) -> Value {
    match value {
        Value::Number(number) => number
            .as_f64()
            .and_then(serde_json::Number::from_f64)
            .map(Value::Number)
            .unwrap_or(Value::Null),
        Value::Array(items) => Value::Array(items.into_iter().map(normalize_numbers).collect()),
        Value::Object(entries) => Value::Object(
            entries
                .into_iter()
                .map(|(key, value)| (key, normalize_numbers(value)))
                .collect(),
        ),
        other => other,
    }
}

/// A root map as canonical JSON; an absent root reads as an empty map (restore clears, it never removes roots).
fn root_map_json(doc: &Doc, name: &str) -> Value {
    let txn = doc.transact();
    match txn.get_map(name) {
        Some(map) => normalize_numbers(serde_json::to_value(map.to_json(&txn)).unwrap_or(Value::Null)),
        None => json!({}),
    }
}

/// The current Y.Doc of a document or of the workspace as a scratch Doc: the live room's state when one
/// is hosted, else the sidecar on disk (rooms this restore opened persist before they are evicted).
async fn current_doc(
    registry: &RoomRegistry,
    key: &str,
    state_path: &Path,
) -> Result<Doc, String> {
    if let Some(room) = registry.peek(key).await {
        let bytes = room
            .with_doc(|doc| {
                doc.transact()
                    .encode_state_as_update_v1(&StateVector::default())
            })
            .await;
        return doc_from_update_bytes(&bytes);
    }
    if !state_path.is_file() {
        return Ok(Doc::new());
    }
    let bytes = std::fs::read(state_path).map_err(|error| {
        format!(
            "forward restore: read {}: {error}",
            state_path.to_string_lossy()
        )
    })?;
    doc_from_update_bytes(&bytes)
}

/// Where the document differs from its snapshot under the oracle normal form, or `None` when equal.
fn difference(current: &Doc, target: &Doc, kind: Option<&str>) -> Option<String> {
    if crate::whiteboard::is_whiteboard(kind) {
        return (!crate::whiteboard::restore::scenes_equivalent(current, target))
            .then(|| "whiteboard scene differs from the restore point".to_string());
    }
    if kind == Some(crate::flow_board::FLOW_BOARD_KIND) {
        let export = |doc: &Doc| crate::flow_board::board_export_json(doc, None).json;
        return (export(current) != export(target))
            .then(|| "Flow board export differs from the restore point".to_string());
    }
    let content = |doc: &Doc| normal_form(&tiptap_json_in(&doc.transact()), false);
    if content(current) != content(target) {
        return Some("content differs from the restore point".to_string());
    }
    for root in DOCUMENT_RESTORE_ROOTS {
        if root_map_json(current, root) != root_map_json(target, root) {
            return Some(format!("{root} differ from the restore point"));
        }
    }
    None
}

/// After a forward restore: every restored document's current state equals its snapshot under the oracle
/// normal form (TipTap: `attrs::normal_form` + comments; Flow: `board_export_json`; whiteboard:
/// `whiteboard::restore::scenes_equivalent`).
pub(crate) async fn verify_forward_restore(
    app: &AppHandle,
    graph_id: &str,
    manifest: &RestorePointManifest,
    report: &ForwardRestoreReport,
) -> Result<(), String> {
    let graph_dir = crate::graph_paths::existing_graph_dir(app, graph_id)?;
    let registry = app.state::<RoomRegistry>();
    // Deleted documents were left alone, and cold-restored ones are byte copies of the snapshot.
    let skipped: BTreeSet<&str> = report
        .documents_skipped_deleted
        .iter()
        .chain(report.documents_cold_restored.iter())
        .map(String::as_str)
        .collect();
    let mut documents: Vec<&DocumentSnapshotRef> = manifest.documents.iter().collect();
    documents.sort_by(|a, b| a.document_id.cmp(&b.document_id));
    for doc_ref in documents {
        let id = doc_ref.document_id.as_str();
        if skipped.contains(id) {
            continue;
        }
        let bytes = crate::time_travel_store::read_document_bytes(
            &graph_dir,
            &manifest.restore_point_id,
            id,
        )
        .map_err(|error| format!("forward restore verify: read bytes of document {id}: {error}"))?;
        let target = doc_from_update_bytes(&bytes)?;
        let current = current_doc(
            &registry,
            &format!("doc:{graph_id}:{id}"),
            &crate::ydoc_paths::checked_document_ydoc_state_path(&graph_dir, id)?,
        )
        .await?;
        let kind = resolve_document_kind(app, graph_id, id).await?;
        if let Some(problem) = difference(&current, &target, kind.as_deref()) {
            return Err(format!(
                "forward restore verification failed: document {id}: {problem}"
            ));
        }
    }
    Ok(())
}
