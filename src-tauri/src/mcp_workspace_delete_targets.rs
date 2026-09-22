use crate::app_runtime::AppHandle;
use crate::{
    document_service::read_workspace_record,
    json_utils::json_string,
    mcp_utils::{mcp_arg_string, mcp_arg_string_vec},
    paths::existing_graph_dir,
    wire_projection_service::wire_is_active,
    workspace_entity_projection::{workspace_entity_id, workspace_wires},
};

pub(super) fn wire_ids_for_delete_request(
    app: &AppHandle,
    graph_id: &str,
    arguments: &serde_json::Value,
) -> Result<Vec<String>, String> {
    let mut ids = mcp_arg_string_vec(arguments, &["wire_ids", "wireIds"]);
    if let Some(wire_id) = mcp_arg_string(arguments, &["wire_id", "wireId", "id"]) {
        ids.push(wire_id);
    }
    if ids.is_empty() {
        let document_id = mcp_arg_string(arguments, &["document_id", "documentId"])
            .ok_or_else(|| "wire_id, wire_ids, or document_id is required".to_string())?;
        let block_id = mcp_arg_string(arguments, &["block_id", "blockId"]);
        let graph_dir = existing_graph_dir(app, graph_id)?;
        let workspace = read_workspace_record(&graph_dir, graph_id)?;
        for wire in workspace_wires(workspace.snapshot.as_ref()) {
            if wire_matches_delete_target(&wire, &document_id, block_id.as_deref()) {
                if let Some(wire_id) = workspace_entity_id(&wire) {
                    ids.push(wire_id);
                }
            }
        }
    }
    ids.sort();
    ids.dedup();
    Ok(ids)
}

fn wire_matches_delete_target(
    wire: &serde_json::Value,
    document_id: &str,
    block_id: Option<&str>,
) -> bool {
    if !wire_is_active(wire) {
        return false;
    }
    let source_document_id = json_string(wire.get("sourceDocumentId"));
    let target_document_id = json_string(wire.get("targetDocumentId"));
    let source_block_id = json_string(wire.get("sourceBlockId"));
    let target_block_id = json_string(wire.get("targetBlockId"));
    let matches_document = source_document_id.as_deref() == Some(document_id)
        || target_document_id.as_deref() == Some(document_id);
    match block_id {
        Some(block_id) => {
            (source_document_id.as_deref() == Some(document_id)
                && source_block_id.as_deref() == Some(block_id))
                || (target_document_id.as_deref() == Some(document_id)
                    && target_block_id.as_deref() == Some(block_id))
        }
        None => matches_document,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_matches_delete_target_accepts_source_or_target_document() {
        let wire = serde_json::json!({
            "id": "wire-1",
            "sourceDocumentId": "doc-a",
            "targetDocumentId": "doc-b"
        });

        assert!(wire_matches_delete_target(&wire, "doc-a", None));
        assert!(wire_matches_delete_target(&wire, "doc-b", None));
        assert!(!wire_matches_delete_target(&wire, "doc-c", None));
    }

    #[test]
    fn wire_matches_delete_target_respects_optional_block_filter() {
        let wire = serde_json::json!({
            "id": "wire-1",
            "sourceDocumentId": "doc-a",
            "sourceBlockId": "block-a",
            "targetDocumentId": "doc-b",
            "targetBlockId": "block-b"
        });

        assert!(wire_matches_delete_target(&wire, "doc-a", Some("block-a")));
        assert!(wire_matches_delete_target(&wire, "doc-b", Some("block-b")));
        assert!(!wire_matches_delete_target(&wire, "doc-a", Some("block-b")));
    }

    #[test]
    fn wire_matches_delete_target_ignores_inactive_wires() {
        let wire = serde_json::json!({
            "id": "wire-1",
            "sourceDocumentId": "doc-a",
            "targetDocumentId": "doc-b",
            "deletedAt": "1000"
        });

        assert!(!wire_matches_delete_target(&wire, "doc-a", None));
    }
}
