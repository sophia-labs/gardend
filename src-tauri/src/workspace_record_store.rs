use crate::{
    document_sidecar_store::read_ydoc_update_base64,
    document_types::WorkspaceRecord,
    graph_service::GraphRecord,
    paths::{workspace_snapshot_path, workspace_ydoc_state_path},
    storage::{display_path, read_json},
};
use std::path::Path;

pub(super) fn read_workspace_record(
    graph_dir: &Path,
    graph_id: &str,
) -> Result<WorkspaceRecord, String> {
    let state_path = workspace_ydoc_state_path(graph_dir);
    let snapshot_path = workspace_snapshot_path(graph_dir);
    let ydoc_update_base64 = read_ydoc_update_base64(&state_path)?.unwrap_or_default();
    let snapshot = if snapshot_path.is_file() {
        Some(read_json::<serde_json::Value>(&snapshot_path)?)
    } else {
        None
    };
    let graph = read_json::<GraphRecord>(&graph_dir.join("graph.json"))?;
    Ok(WorkspaceRecord {
        graph_id: graph_id.to_string(),
        ydoc_update_base64,
        ydoc_state_path: display_path(&state_path),
        snapshot,
        snapshot_path: display_path(&snapshot_path),
        updated_at: graph.updated_at,
    })
}
