use crate::app_runtime::AppHandle;
use crate::{
    document_service::read_graph_documents_cold,
    mcp_utils::{mcp_arg_string, mcp_arg_usize, mcp_graph_id_or_default},
    paths::existing_graph_dir,
    salience_score_projection::{find_projected_block, local_block_value_scores_for},
};
use std::collections::BTreeMap;

pub(super) fn mcp_local_get_important_blocks(
    app: AppHandle,
    arguments: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let graph_id = mcp_graph_id_or_default(&app, arguments)?;
    let graph_dir = existing_graph_dir(&app, &graph_id)?;
    let observer =
        mcp_arg_string(arguments, &["observer_agent_id", "observerAgentId"]).unwrap_or_default();
    let document_filter = mcp_arg_string(arguments, &["document_id", "documentId"]);
    let limit = mcp_arg_usize(arguments, &["limit"], 5).clamp(1, 20);
    let valence_filter = mcp_arg_string(arguments, &["valence"]);
    let mut scored = local_block_value_scores_for(
        &app,
        &graph_id,
        &observer,
        document_filter.as_deref(),
        None,
        None,
        limit,
        None,
        valence_filter.as_deref(),
    )?;
    let documents = read_graph_documents_cold(&graph_dir)?;
    let mut document_map = BTreeMap::new();
    for document in documents {
        document_map.insert(document.document_id.clone(), document);
    }

    let mut blocks = Vec::new();
    for score in scored.drain(..) {
        let document_id = score
            .get("document_id")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string();
        let block_id = score
            .get("block_id")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string();
        let Some(document) = document_map.get(&document_id) else {
            continue;
        };
        let content = find_projected_block(document, &block_id)
            .map(|block| block.content)
            .unwrap_or_else(|| "(deleted)".to_string());
        blocks.push(serde_json::json!({
            "content": content.clone(),
            "text": content,
            "document": document.title,
            "score": score.get("composite_score").cloned().unwrap_or_else(|| serde_json::json!(0.0)),
            "importance": score.get("cumulative_importance").cloned().unwrap_or_else(|| serde_json::json!(0.0)),
            "valence": score.get("cumulative_valence").cloned().unwrap_or_else(|| serde_json::json!(0.0)),
            "block_id": block_id,
            "blockId": block_id,
            "doc_id": document_id,
            "docId": document_id,
            "document_id": document_id,
            "documentId": document_id,
            "block_wires": score.get("block_wire_count").cloned().unwrap_or_else(|| serde_json::json!(0)),
            "blockWires": score.get("block_wire_count").cloned().unwrap_or_else(|| serde_json::json!(0)),
            "doc_wires": score.get("doc_wire_count").cloned().unwrap_or_else(|| serde_json::json!(0)),
            "docWires": score.get("doc_wire_count").cloned().unwrap_or_else(|| serde_json::json!(0)),
            "tags": score.get("tags").cloned().unwrap_or_else(|| serde_json::json!([])),
        }));
    }
    blocks.sort_by(|left, right| {
        let left_score = left
            .get("score")
            .and_then(serde_json::Value::as_f64)
            .unwrap_or(0.0);
        let right_score = right
            .get("score")
            .and_then(serde_json::Value::as_f64)
            .unwrap_or(0.0);
        right_score
            .partial_cmp(&left_score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    blocks.truncate(limit);
    Ok(serde_json::json!({
        "blocks": blocks,
        "count": blocks.len(),
        "source": "local-value-store",
        "value_store_available": true,
        "valueStoreAvailable": true,
    }))
}
