use crate::app_runtime::AppHandle;
use crate::{
    active_documents::try_live_projection,
    document_digest_connections::document_digest_wire_summary,
    document_projection_service::document_blocks_for_read,
    document_service::{read_document, read_workspace_record},
    json_utils::{json_bool, json_string},
    mcp_utils::{mcp_arg_usize, mcp_required_document_id, mcp_required_graph_id},
    paths::existing_graph_dir,
    salience_service::local_block_value_scores,
    workspace_entity_projection::{
        folder_path, workspace_documents, workspace_entity_id, workspace_folders, workspace_wires,
    },
};
use std::collections::BTreeMap;

pub(super) async fn mcp_local_document_digest(
    app: AppHandle,
    arguments: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let graph_id = mcp_required_graph_id(arguments)?;
    let document_id = mcp_required_document_id(arguments)?;
    let top_valued = mcp_arg_usize(arguments, &["topValued", "top_valued"], 3);
    let graph_dir = existing_graph_dir(&app, &graph_id)?;
    let workspace = read_workspace_record(&graph_dir, &graph_id)?;
    let document = read_document(app.clone(), graph_id.clone(), document_id.clone())?;
    let blocks = match try_live_projection(&app, &graph_id, &document_id).await {
        Some(live) => live.blocks,
        None => document_blocks_for_read(&document),
    };
    let snapshot = workspace.snapshot.as_ref();
    let documents = workspace_documents(snapshot);
    let folders = workspace_folders(snapshot);
    let wires = workspace_wires(snapshot);
    let workspace_document = documents
        .iter()
        .find(|candidate| workspace_entity_id(candidate).as_deref() == Some(document_id.as_str()));
    let parent_id = workspace_document
        .and_then(|entity| json_string(entity.get("parentId").or_else(|| entity.get("parent_id"))));
    let read_only = workspace_document
        .and_then(|entity| json_bool(entity.get("readOnly")))
        .unwrap_or(false);
    let text = blocks
        .iter()
        .map(|block| block.content.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    let headings = blocks
        .iter()
        .filter(|block| block.block_type == "heading")
        .map(|block| {
            serde_json::json!({
                "block_id": block.id,
                "blockId": block.id,
                "level": block.level.unwrap_or(1),
                "text": block.content,
            })
        })
        .collect::<Vec<_>>();

    let wire_summary = document_digest_wire_summary(&wires, &documents, &document_id, 5);

    let block_content_by_id = blocks
        .iter()
        .map(|block| (block.id.clone(), block.content.clone()))
        .collect::<BTreeMap<_, _>>();
    let top_valued_blocks = if top_valued == 0 {
        Vec::new()
    } else {
        local_block_value_scores(
            &app,
            &graph_id,
            Some(&document_id),
            None,
            None,
            top_valued.clamp(1, 20),
            None,
            None,
        )?
        .into_iter()
        .map(|score| {
            let block_id = score
                .get("block_id")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default();
            serde_json::json!({
                "block_id": block_id,
                "blockId": block_id,
                "content": block_content_by_id.get(block_id).cloned().unwrap_or_else(|| "(deleted)".to_string()),
                "score": score.get("composite_score").cloned().unwrap_or_else(|| serde_json::json!(0.0)),
                "importance": score.get("cumulative_importance").cloned().unwrap_or_else(|| serde_json::json!(0.0)),
                "valence": score.get("cumulative_valence").cloned().unwrap_or_else(|| serde_json::json!(0.0)),
                "block_wires": score.get("block_wire_count").cloned().unwrap_or_else(|| serde_json::json!(0)),
                "blockWires": score.get("block_wire_count").cloned().unwrap_or_else(|| serde_json::json!(0)),
                "doc_wires": score.get("doc_wire_count").cloned().unwrap_or_else(|| serde_json::json!(0)),
                "docWires": score.get("doc_wire_count").cloned().unwrap_or_else(|| serde_json::json!(0)),
            })
        })
        .collect::<Vec<_>>()
    };

    Ok(serde_json::json!({
        "metadata": {
            "title": document.title,
            "document_id": document_id,
            "documentId": document.document_id,
            "folder_path": folder_path(&folders, parent_id),
            "folderPath": folder_path(&folders, workspace_document.and_then(|entity| json_string(entity.get("parentId").or_else(|| entity.get("parent_id"))))),
            "readOnly": read_only,
            "localPath": document.local_path,
            "rdfSubject": document.rdf_subject,
            "schemaVersion": document.schema_version,
        },
        "size": {
            "block_count": blocks.len(),
            "blockCount": blocks.len(),
            "character_count": text.chars().count(),
            "characterCount": text.chars().count(),
            "word_count": text.split_whitespace().count(),
            "wordCount": text.split_whitespace().count(),
        },
        "freshness": {
            "created_at": document.created_at,
            "createdAt": document.created_at,
            "updated_at": document.updated_at,
            "updatedAt": document.updated_at,
            "snapshot_count": 0,
            "snapshotCount": 0,
        },
        "headings": headings,
        "wire_summary": {
            "incoming": wire_summary.incoming_count,
            "outgoing": wire_summary.outgoing_count,
            "total": wire_summary.total_count,
            "predicate_distribution": wire_summary.predicate_counts,
            "top_connected_documents": wire_summary.top_connected_documents,
        },
        "valuation_summary": {
            "top_valued": top_valued_blocks,
            "requested": top_valued,
            "status": "ok",
            "source": "local-value-store",
        },
    }))
}
