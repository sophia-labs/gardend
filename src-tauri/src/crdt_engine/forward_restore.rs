//! Restore as a forward transaction (DESIGN §2.12): for each document, plan current -> snapshot with the
//! minimal-edit engine and apply it through the room, so live rooms, reconnecting sockets and browser caches
//! all converge on the restored content.

use serde::Serialize;
use yrs::{Doc, TransactionMut};

use crate::app_runtime::AppHandle;
use crate::crdt_engine::minimal_edit::OpCounts;
use crate::time_travel_types::RestorePointManifest;

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
    unimplemented!("U9")
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
    unimplemented!("U9")
}

/// The restore engine. The caller holds the RestoreGuard, the graph hot-write lease, and runs this inside
/// `restore_guard::as_restore_owner(ctx.operation_id, …)`. Workspace first, then documents sorted by id.
pub(crate) async fn forward_restore_manifest(
    app: &AppHandle,
    graph_id: &str,
    manifest: &RestorePointManifest,
    ctx: &RestoreContext,
) -> Result<ForwardRestoreReport, String> {
    unimplemented!("U9")
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
    unimplemented!("U9")
}
