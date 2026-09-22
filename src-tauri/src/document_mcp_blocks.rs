use crate::app_runtime::AppHandle;
use crate::{
    active_documents::try_live_projection,
    document_block_projection::{block_json, block_matches_query},
    document_rendering::{document_blocks_for_read, render_block_content},
    document_service::{read_document, BlockSnapshot},
    mcp_utils::{
        mcp_arg_bool, mcp_arg_isize, mcp_arg_string, mcp_arg_usize, mcp_required_document_id,
        mcp_required_graph_id,
    },
};

async fn live_or_stored_blocks(
    app: &AppHandle,
    graph_id: &str,
    document_id: &str,
) -> Result<Vec<BlockSnapshot>, String> {
    if let Some(live) = try_live_projection(app, graph_id, document_id).await {
        return Ok(live.blocks);
    }
    let document = read_document(app.clone(), graph_id.to_string(), document_id.to_string())?;
    Ok(document_blocks_for_read(&document))
}

pub(super) async fn mcp_local_read_blocks(
    app: AppHandle,
    arguments: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let graph_id = mcp_required_graph_id(arguments)?;
    let document_id = mcp_required_document_id(arguments)?;
    let blocks = live_or_stored_blocks(&app, &graph_id, &document_id).await?;
    let total_blocks = blocks.len();
    let limit = mcp_arg_usize(arguments, &["limit"], 50).clamp(1, 200);
    let include_ids = mcp_arg_bool(arguments, &["includeIds", "include_ids"], true);
    let format = mcp_arg_string(arguments, &["format"]).unwrap_or_else(|| "markdown".to_string());

    let mut start = if let Some(block_id) = mcp_arg_string(arguments, &["blockId", "block_id"]) {
        blocks
            .iter()
            .position(|block| block.id == block_id)
            .unwrap_or(0)
    } else {
        let offset = mcp_arg_isize(arguments, &["offset"], 0);
        if offset < 0 {
            total_blocks.saturating_sub(offset.unsigned_abs())
        } else {
            offset as usize
        }
    };
    start = start.min(total_blocks);
    let end = (start + limit).min(total_blocks);

    let rendered = blocks[start..end]
        .iter()
        .enumerate()
        .map(|(relative_index, block)| {
            let index = start + relative_index;
            let content = match format.as_str() {
                "text" | "markdown" => block.content.clone(),
                "xml" => format!(
                    "<{} data-block-id=\"{}\">{}</{}>",
                    block.block_type, block.id, block.content, block.block_type
                ),
                _ => block.content.clone(),
            };
            let mut value = serde_json::json!({
                "index": index,
                "content": content,
            });
            if include_ids {
                value["block_id"] = serde_json::json!(block.id);
                value["blockId"] = serde_json::json!(block.id);
            }
            if block.block_type != "paragraph" {
                value["block_type"] = serde_json::json!(block.block_type);
                value["blockType"] = serde_json::json!(block.block_type);
            }
            value
        })
        .collect::<Vec<_>>();

    Ok(serde_json::json!({
        "graph_id": graph_id,
        "document_id": document_id,
        "format": format,
        "offset": start,
        "limit": limit,
        "blocks": rendered,
        "total_blocks": total_blocks,
        "has_more": end < total_blocks,
        "next_offset": if end < total_blocks { Some(end) } else { None },
    }))
}

pub(super) async fn mcp_local_get_block(
    app: AppHandle,
    arguments: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let graph_id = mcp_required_graph_id(arguments)?;
    let document_id = mcp_required_document_id(arguments)?;
    let block_id = mcp_arg_string(arguments, &["blockId", "block_id"])
        .ok_or_else(|| "blockId is required".to_string())?;
    let format = mcp_arg_string(arguments, &["format"]).unwrap_or_else(|| "xml".to_string());
    if !matches!(format.as_str(), "xml" | "markdown" | "text") {
        return Err(format!("unsupported get_block format: {format}"));
    }

    let blocks = live_or_stored_blocks(&app, &graph_id, &document_id).await?;
    let Some((index, block)) = blocks
        .iter()
        .enumerate()
        .find(|(_, block)| block.id == block_id)
    else {
        return Err(format!(
            "block {block_id} not found in document {document_id}"
        ));
    };

    let mut result = block_json(block, index, &blocks);
    result["format"] = serde_json::json!(format);
    match format.as_str() {
        "xml" => {
            result["xml"] = serde_json::json!(render_block_content(block, "xml"));
        }
        "markdown" => {
            result["markdown"] = serde_json::json!(render_block_content(block, "markdown"));
        }
        "text" => {
            result["text"] = serde_json::json!(render_block_content(block, "text"));
        }
        _ => {}
    }

    Ok(serde_json::json!({
        "graph_id": graph_id,
        "document_id": document_id,
        "block": result,
    }))
}

pub(super) async fn mcp_local_query_blocks(
    app: AppHandle,
    arguments: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let graph_id = mcp_required_graph_id(arguments)?;
    let document_id = mcp_required_document_id(arguments)?;
    let blocks = live_or_stored_blocks(&app, &graph_id, &document_id).await?;
    let limit = mcp_arg_usize(arguments, &["limit"], 50).clamp(1, 200);

    let queries = if let Some(values) = arguments
        .get("queries")
        .and_then(serde_json::Value::as_array)
    {
        values
            .iter()
            .filter_map(serde_json::Value::as_object)
            .cloned()
            .collect::<Vec<_>>()
    } else {
        arguments
            .as_object()
            .cloned()
            .map(|object| vec![object])
            .unwrap_or_default()
    };

    let mut grouped = Vec::new();
    let mut total_matches = 0usize;
    for (query_index, query) in queries.iter().enumerate() {
        let mut matches = Vec::new();
        for (index, block) in blocks.iter().enumerate() {
            if !block_matches_query(block, query)? {
                continue;
            }
            matches.push(block_json(block, index, &blocks));
            total_matches += 1;
            if matches.len() >= limit {
                break;
            }
        }
        grouped.push(serde_json::json!({
            "query_index": query_index,
            "queryIndex": query_index,
            "count": matches.len(),
            "blocks": matches,
        }));
    }

    Ok(serde_json::json!({
        "graph_id": graph_id,
        "document_id": document_id,
        "mode": if arguments.get("queries").is_some() { "batch" } else { "single" },
        "results": grouped,
        "total_matches": total_matches,
        "totalMatches": total_matches,
    }))
}
