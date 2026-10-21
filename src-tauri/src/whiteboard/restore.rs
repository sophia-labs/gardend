//! Forward restore of a whiteboard (format sophia.whiteboard/1, whiteboard/FORMAT.md).

use yrs::{Doc, TransactionMut};

use crate::crdt_engine::forward_restore::RestoreContext;
use crate::crdt_engine::minimal_edit::OpCounts;

/// Per element id: equal to the target modulo `version`/`versionNonce`/`updated`/`customData.sophia` ->
/// untouched; otherwise write the target with `version = max(current, target) + 1`, a fresh `versionNonce`,
/// `updated = ctx.now_ms`, `customData.sophia = {by: "restore", at}`; present now but absent in the target ->
/// tombstone (`isDeleted: true`, version bumped). Excalidraw's reconcile (higher version wins) then accepts
/// the restore in every open browser. `meta` and `files` through `ytree::restore_map_roots`.
pub(crate) fn restore_elements(
    txn: &mut TransactionMut<'_>,
    target: &Doc,
    ctx: &RestoreContext,
) -> Result<OpCounts, String> {
    unimplemented!("U12")
}

/// Live elements equal modulo version, versionNonce, updated and customData.sophia; tombstones ignored.
pub(crate) fn scenes_equivalent(a: &Doc, b: &Doc) -> bool {
    unimplemented!("U12")
}
