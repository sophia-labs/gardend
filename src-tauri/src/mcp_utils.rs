use crate::app_runtime::AppHandle;
use crate::graph_service::list_graphs;

pub(super) use crate::mcp_arg_utils::{
    mcp_arg_bool, mcp_arg_bool_with_fallback, mcp_arg_isize, mcp_arg_string, mcp_arg_string_vec,
    mcp_arg_string_with_fallback, mcp_arg_u64, mcp_arg_u64_vec, mcp_arg_usize, mcp_block_target,
    mcp_query_terms, mcp_required_document_id, mcp_required_graph_id, mcp_required_job_id,
};
pub(super) use crate::mcp_block_payloads::{
    mcp_block_delete_payload, mcp_block_edit_text_payload, mcp_block_insert_payload,
    mcp_block_update_payload,
};

pub(super) fn mcp_graph_id_or_default(
    app: &AppHandle,
    arguments: &serde_json::Value,
) -> Result<String, String> {
    if let Some(graph_id) = mcp_arg_string(arguments, &["graph_id", "graphId"]) {
        return Ok(graph_id);
    }

    list_graphs(app.clone())?
        .first()
        .map(|graph| graph.graph_id.clone())
        .ok_or_else(|| "graph_id is required because no local graphs exist".to_string())
}
