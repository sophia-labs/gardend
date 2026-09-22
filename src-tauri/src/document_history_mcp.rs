use crate::app_runtime::AppHandle;
use crate::{
    document_history_persistence::{history_sort_key, history_tier_label},
    document_history_projection::snapshot_blocks_markdown,
    document_history_service::{document_history_for_read, document_snapshot_for_read},
    document_history_store::LocalDocumentSnapshotMeta,
    mcp_utils::{mcp_arg_string, mcp_arg_usize, mcp_graph_id_or_default, mcp_required_document_id},
    paths::existing_graph_dir,
};

fn history_snapshot_json(snapshot: &LocalDocumentSnapshotMeta) -> serde_json::Value {
    serde_json::json!({
        "snapshot_id": snapshot.snapshot_id,
        "snapshotId": snapshot.snapshot_id,
        "created_at": snapshot.created_at,
        "createdAt": snapshot.created_at,
        "tier": snapshot.tier,
        "tier_label": history_tier_label(&snapshot.tier),
        "tierLabel": history_tier_label(&snapshot.tier),
        "snapshot_count": snapshot.snapshot_count,
        "snapshotCount": snapshot.snapshot_count,
        "chars_added": snapshot.chars_added,
        "charsAdded": snapshot.chars_added,
        "chars_removed": snapshot.chars_removed,
        "charsRemoved": snapshot.chars_removed,
        "blocks_added": snapshot.blocks_added,
        "blocksAdded": snapshot.blocks_added,
        "blocks_removed": snapshot.blocks_removed,
        "blocksRemoved": snapshot.blocks_removed,
        "blocks_modified": snapshot.blocks_modified,
        "blocksModified": snapshot.blocks_modified,
        "is_manual": snapshot.is_manual,
        "isManual": snapshot.is_manual,
        "label": snapshot.label,
    })
}

pub(super) fn mcp_local_get_document_history(
    app: AppHandle,
    arguments: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let graph_id = mcp_graph_id_or_default(&app, arguments)?;
    let document_id = mcp_required_document_id(arguments)?;
    let limit = mcp_arg_usize(arguments, &["limit"], 20).clamp(1, 200);
    let graph_dir = existing_graph_dir(&app, &graph_id)?;
    let mut snapshots = document_history_for_read(&graph_dir, &graph_id, &document_id)?.snapshots;
    snapshots.sort_by_key(|snapshot| std::cmp::Reverse(history_sort_key(snapshot)));
    snapshots.truncate(limit);
    let rows = snapshots
        .iter()
        .map(history_snapshot_json)
        .collect::<Vec<_>>();

    Ok(serde_json::json!({
        "graph_id": graph_id.clone(),
        "graphId": graph_id,
        "document_id": document_id.clone(),
        "documentId": document_id,
        "snapshot_count": rows.len(),
        "snapshotCount": rows.len(),
        "snapshots": rows,
        "source": "local-document-history",
    }))
}

pub(super) fn mcp_local_read_document_at_snapshot(
    app: AppHandle,
    arguments: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let graph_id = mcp_graph_id_or_default(&app, arguments)?;
    let document_id = mcp_required_document_id(arguments)?;
    let snapshot_id = mcp_arg_string(arguments, &["snapshot_id", "snapshotId"])
        .ok_or_else(|| "snapshot_id is required".to_string())?;
    let graph_dir = existing_graph_dir(&app, &graph_id)?;
    let payload = document_snapshot_for_read(&graph_dir, &graph_id, &document_id, &snapshot_id)?;
    let content = snapshot_blocks_markdown(&payload.blocks);

    Ok(serde_json::json!({
        "graph_id": graph_id.clone(),
        "graphId": graph_id,
        "document_id": document_id.clone(),
        "documentId": document_id,
        "snapshot_id": snapshot_id.clone(),
        "snapshotId": snapshot_id,
        "content": content,
        "source": "local-document-history",
    }))
}
