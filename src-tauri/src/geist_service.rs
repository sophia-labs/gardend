use crate::app_runtime::AppHandle;
pub(super) use crate::geist_memory_recall_service::mcp_local_recall_or_recent_documents;
pub(super) use crate::geist_song_service::mcp_local_song_summary;
use crate::{
    document_projection_service::document_blocks_for_read,
    document_service::{list_documents, DocumentRecord},
    graph_service::read_graph_record,
    mcp_utils::{mcp_arg_bool, mcp_arg_string, mcp_arg_usize, mcp_graph_id_or_default},
    orientation_service::mcp_local_orientation_location,
    profile_service::ensure_profile,
    salience_service::mcp_local_get_important_blocks,
    text_utils::text_preview,
    workspace_projection_service::mcp_local_get_workspace,
};

fn mcp_local_important_block_previews(
    documents: &[DocumentRecord],
    limit: usize,
) -> serde_json::Value {
    let mut blocks = Vec::new();
    let limit = limit.min(20);
    for document in documents {
        if blocks.len() >= limit {
            break;
        }
        for block in document_blocks_for_read(document) {
            if blocks.len() >= limit {
                break;
            }
            if block.content.trim().is_empty() {
                continue;
            }
            let preview = text_preview(&block.content, 240);
            blocks.push(serde_json::json!({
                "content": preview.clone(),
                "text": preview,
                "document": document.title.clone(),
                "document_id": document.document_id.clone(),
                "documentId": document.document_id.clone(),
                "block_id": block.id.clone(),
                "blockId": block.id,
                "block_type": block.block_type.clone(),
                "blockType": block.block_type,
                "score": null,
                "importance": null,
                "valence": null,
                "source": "local-document-preview",
            }));
        }
    }
    let count = blocks.len();
    serde_json::json!({
        "blocks": blocks,
        "count": count,
        "source": "local-document-preview",
        "value_store_available": false,
        "valueStoreAvailable": false,
    })
}

pub(super) fn mcp_local_quick_orient(
    app: AppHandle,
    arguments: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let graph_id = mcp_graph_id_or_default(&app, arguments)?;
    let profile = ensure_profile(&app)?;
    let (_, graph) = read_graph_record(&app, &graph_id)?;
    let documents = list_documents(app.clone(), graph_id)?;
    let recall_limit = mcp_arg_usize(arguments, &["recall_limit", "recallLimit"], 5).min(20);
    Ok(serde_json::json!({
        "location": mcp_local_orientation_location(&profile, &graph),
        "home_graph": graph.graph_id.clone(),
        "homeGraph": graph.graph_id.clone(),
        "song": mcp_local_song_summary(&app, &graph.graph_id),
        "recall": mcp_local_recall_or_recent_documents(app, &graph.graph_id, &documents, recall_limit),
        "workspace": {
            "graph_id": graph.graph_id.clone(),
            "graphId": graph.graph_id.clone(),
            "title": graph.title.clone(),
            "counts": {
                "documents": documents.len(),
            },
        },
        "source": "local-native-loopback",
    }))
}

pub(super) async fn mcp_local_context_bundle(
    app: AppHandle,
    arguments: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let graph_id = mcp_graph_id_or_default(&app, arguments)?;
    let profile = ensure_profile(&app)?;
    let (_, graph) = read_graph_record(&app, &graph_id)?;
    let documents = list_documents(app.clone(), graph_id.clone())?;
    let recall_limit = mcp_arg_usize(arguments, &["recall_limit", "recallLimit"], 5).min(20);
    let important_limit =
        mcp_arg_usize(arguments, &["important_limit", "importantLimit"], 5).min(20);
    let workspace_depth = mcp_arg_usize(
        arguments,
        &["workspace_depth", "workspaceDepth", "depth"],
        1,
    )
    .min(5);
    let workspace_args = serde_json::json!({
        "graphId": graph_id,
        "depth": workspace_depth,
        "limit": mcp_arg_usize(arguments, &["workspace_limit", "workspaceLimit"], 50).min(200),
        "maxBytes": mcp_arg_usize(arguments, &["workspace_max_bytes", "workspaceMaxBytes"], 65536).min(262144),
    });
    let workspace = mcp_local_get_workspace(app.clone(), &workspace_args).await?;
    let agent_name = mcp_arg_string(arguments, &["agent_name", "agentName"]);
    let agent_document = agent_name
        .as_ref()
        .map(|name| {
            serde_json::json!({
                "agent_name": name,
                "agentName": name,
                "document": null,
                "source": "local-unavailable",
                "unavailable_reason": "local agent identity documents are not implemented in the native runtime yet",
                "unavailableReason": "local agent identity documents are not implemented in the native runtime yet",
            })
        })
        .unwrap_or(serde_json::Value::Null);
    let valued_args = serde_json::json!({
        "graphId": graph_id,
        "limit": important_limit,
    });
    let important_blocks = match mcp_local_get_important_blocks(app.clone(), &valued_args) {
        Ok(value)
            if value
                .get("count")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0)
                > 0 =>
        {
            value
        }
        _ => mcp_local_important_block_previews(&documents, important_limit),
    };
    Ok(serde_json::json!({
        "location": mcp_local_orientation_location(&profile, &graph),
        "home_graph": graph.graph_id.clone(),
        "homeGraph": graph.graph_id.clone(),
        "set_home": mcp_arg_bool(arguments, &["set_home", "setHome"], true),
        "setHome": mcp_arg_bool(arguments, &["set_home", "setHome"], true),
        "song": mcp_local_song_summary(&app, &graph.graph_id),
        "recall": mcp_local_recall_or_recent_documents(app.clone(), &graph.graph_id, &documents, recall_limit),
        "agent_document": agent_document.clone(),
        "agentDocument": agent_document,
        "important_blocks": important_blocks.clone(),
        "importantBlocks": important_blocks,
        "workspace": workspace,
        "source": "local-native-loopback",
    }))
}
