//! A board's shapes as the document record's blocks (SPRINT D3), and the guard that keeps block-editing tools off boards.
//!
//! A whiteboard's saved record used to carry `blocks: []`. Now it lists one block per live shape (id = the element id),
//! so each shape gets its own semantic passage, `search_blocks` hit, wire endpoint (`{board}#block-{id}`) and wire
//! snippet through paths that already exist. Arrows, bound text and deleted elements are not blocks; frames are.
//!
//! Owned by G1. The signatures below are FINAL; G1 replaces the stub bodies.

use super::scene::Scene;
use crate::app_runtime::AppHandle;
use crate::document_types::BlockSnapshot;

/// The record's blocks for a scene, in reading order (top to bottom, then left to right, the order of
/// `outline::shapes`): `id` = element id, `block_type` = `scene::shape_kind`, `content` = the shape's words plus its
/// link title, `parent_id` = the frame's element id when the shape is inside a frame, `order` = 0.0, 1.0, 2.0, ...,
/// `level`/`checked`/`language` None, `marks` empty.
pub(crate) fn blocks_of(scene: &Scene) -> Vec<BlockSnapshot> {
    // STUB (today's behaviour: no blocks; the call sites are live). G1 replaces the body.
    let _ = scene;
    Vec::new()
}

/// Refuse a block-editing tool on a scene kind. Ok for any other document. The error for a whiteboard is exactly
/// `"{tool}: a whiteboard is drawn, not written: use whiteboard_apply"`; for a Flow board
/// `"{tool}: a Flow board is not a text document: use flow_seed_board"`.
pub(crate) async fn refuse_scene_kind(
    app: &AppHandle,
    graph_id: &str,
    document_id: &str,
    tool: &str,
) -> Result<(), String> {
    // STUB (no-op: today's behaviour; the call sites are live). G1 replaces the body.
    let _ = (app, graph_id, document_id, tool);
    Ok(())
}
