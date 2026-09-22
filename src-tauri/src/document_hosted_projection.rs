use crate::app_runtime::AppHandle;
use crate::{
    document_block_projection::{
        hosted_block_context_item, hosted_block_summary_value, hosted_block_value,
    },
    document_rendering::{document_blocks_for_read, document_export_response},
    document_service::{
        read_document, read_graph_documents_cold, read_workspace_record, DocumentRecord,
    },
    json_utils::json_bool,
    paths::existing_graph_dir,
    text_utils::{compact_text, truncate_chars},
    workspace_entity_projection::{
        hosted_entity_parent_id, hosted_entity_timestamp, workspace_document_entity,
        workspace_documents, workspace_entity_id,
    },
};
use axum::response::Response;
use serde::Serialize;

pub(super) use crate::document_hosted_write_payloads::hosted_document_write_payload;

// `entityType` is always `"document"` for these projections; serde uses the
// constant via a trivial helper rather than letting callers vary it.
fn entity_type_document() -> &'static str {
    "document"
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct HostedDocumentSummary {
    #[serde(rename = "entityType")]
    entity_type: &'static str,
    id: String,
    graph_id: String,
    title: String,
    revision: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    body_availability: Option<serde_json::Value>,
    snippet: Option<String>,
    updated_at: Option<String>,
    last_accessed_at: Option<String>,
    parent_id: Option<String>,
    read_only: bool,
}

/// Hosted document envelope. `blocks` is the projected block tree which is
/// authored by the block-projection layer and varies per block type — kept as
/// `serde_json::Value` so this surface doesn't fork the block schema.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct HostedDocumentEnvelope {
    #[serde(rename = "entityType")]
    entity_type: &'static str,
    id: String,
    graph_id: String,
    title: String,
    revision: u64,
    /// Block tree authored by `hosted_block_value`; opaque per-block schema.
    blocks: Vec<serde_json::Value>,
    created_at: Option<String>,
    updated_at: Option<String>,
    last_accessed_at: Option<String>,
    created_by: Option<String>,
    snippet: Option<String>,
    parent_id: Option<String>,
    read_only: bool,
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct HostedDocumentBlocksResponseBody {
    /// Per-block summary values authored by `hosted_block_summary_value`.
    blocks: Vec<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct HostedBlockContextResponseBody {
    doc_id: String,
    block_id: Option<String>,
    mode: &'static str,
    title: String,
    /// Block context items authored by `hosted_block_context_item`.
    blocks: Vec<serde_json::Value>,
}

pub(super) fn document_snippet(document: &DocumentRecord) -> Option<String> {
    let blocks = document_blocks_for_read(document);
    let text = compact_text(
        &blocks
            .iter()
            .map(|block| block.content.as_str())
            .collect::<Vec<_>>()
            .join(" "),
    );
    if text.is_empty() {
        None
    } else {
        Some(truncate_chars(&text, 160))
    }
}

pub(super) fn hosted_document_summary_value(
    document: &DocumentRecord,
    workspace_entity: Option<&serde_json::Value>,
    graph_id: &str,
) -> HostedDocumentSummary {
    let read_only = workspace_entity
        .and_then(|entity| json_bool(entity.get("readOnly")))
        .unwrap_or(false);
    HostedDocumentSummary {
        entity_type: entity_type_document(),
        id: document.document_id.clone(),
        graph_id: graph_id.to_string(),
        title: document.title.clone(),
        revision: Some(document.revision),
        body_availability: None,
        snippet: document_snippet(document),
        updated_at: hosted_entity_timestamp(
            workspace_entity,
            "updatedAt",
            Some(&document.updated_at),
        ),
        last_accessed_at: None,
        parent_id: hosted_entity_parent_id(workspace_entity),
        read_only,
    }
}

pub(super) fn hosted_document_summaries(
    app: &AppHandle,
    graph_id: &str,
) -> Result<Vec<HostedDocumentSummary>, String> {
    let graph_dir = existing_graph_dir(app, graph_id)?;
    let workspace = read_workspace_record(&graph_dir, graph_id)?;
    let workspace_documents = workspace_documents(workspace.snapshot.as_ref());
    let documents = read_graph_documents_cold(&graph_dir)?;
    let mut summaries: Vec<_> = documents
        .iter()
        .map(|document| {
            let workspace_entity = workspace_documents.iter().find(|entity| {
                workspace_entity_id(entity).as_deref() == Some(document.document_id.as_str())
            });
            hosted_document_summary_value(document, workspace_entity, graph_id)
        })
        .collect();
    for row in crate::document_body_availability::unavailable_documents(&graph_dir)? {
        let id = row["documentId"].as_str().ok_or("unavailable document ID missing")?;
        if crate::document_tombstone_store::document_is_tombstoned(&graph_dir, id)? {
            return Err("unavailable body conflicts with destination tombstone".into());
        }
        let metadata = &row["metadata"];
        summaries.push(HostedDocumentSummary {
            entity_type: entity_type_document(), id: id.to_string(), graph_id: graph_id.to_string(),
            title: metadata["title"].as_str().ok_or("unavailable source title missing")?.to_string(),
            revision: None, snippet: None, last_accessed_at: None,
            updated_at: hosted_entity_timestamp(Some(metadata), "updatedAt", None),
            parent_id: hosted_entity_parent_id(Some(metadata)), read_only: true,
            body_availability: Some(serde_json::json!({"state":"source-body-unavailable",
                "reason":row["reason"],"sourceUserId":row["sourceUserId"],
                "sourceGraphId":row["sourceGraphId"],"inventorySha256":row["inventorySha256"]})),
        });
    }
    Ok(summaries)
}

pub(super) fn hosted_document_response(
    app: &AppHandle,
    graph_id: &str,
    document_id: &str,
) -> Result<HostedDocumentEnvelope, String> {
    let graph_dir = existing_graph_dir(app, graph_id)?;
    let workspace = read_workspace_record(&graph_dir, graph_id)?;
    let document = read_document(app.clone(), graph_id.to_string(), document_id.to_string())?;
    let workspace_entity = workspace_document_entity(workspace.snapshot.as_ref(), document_id);
    let read_only = workspace_entity
        .as_ref()
        .and_then(|entity| json_bool(entity.get("readOnly")))
        .unwrap_or(false);
    Ok(HostedDocumentEnvelope {
        entity_type: entity_type_document(),
        id: document.document_id.clone(),
        graph_id: graph_id.to_string(),
        title: document.title.clone(),
        revision: document.revision,
        blocks: document_blocks_for_read(&document)
            .iter()
            .map(hosted_block_value)
            .collect(),
        created_at: hosted_entity_timestamp(
            workspace_entity.as_ref(),
            "createdAt",
            Some(&document.created_at),
        ),
        updated_at: hosted_entity_timestamp(
            workspace_entity.as_ref(),
            "updatedAt",
            Some(&document.updated_at),
        ),
        last_accessed_at: None,
        created_by: None,
        snippet: document_snippet(&document),
        parent_id: hosted_entity_parent_id(workspace_entity.as_ref()),
        read_only,
    })
}

pub(super) fn hosted_document_blocks_response(
    app: &AppHandle,
    graph_id: &str,
    document_id: &str,
) -> Result<HostedDocumentBlocksResponseBody, String> {
    let document = read_document(app.clone(), graph_id.to_string(), document_id.to_string())?;
    Ok(HostedDocumentBlocksResponseBody {
        blocks: document_blocks_for_read(&document)
            .iter()
            .map(hosted_block_summary_value)
            .collect(),
    })
}

pub(super) fn hosted_block_context_response(
    app: &AppHandle,
    graph_id: &str,
    document_id: &str,
    block_id: Option<&str>,
) -> Result<HostedBlockContextResponseBody, String> {
    let document = read_document(app.clone(), graph_id.to_string(), document_id.to_string())?;
    let blocks = document_blocks_for_read(&document);

    let body = if let Some(block_id) = block_id {
        let Some(index) = blocks.iter().position(|block| block.id == block_id) else {
            return Err(format!(
                "block {block_id} not found in document {document_id}"
            ));
        };
        let mut context_blocks = Vec::new();
        if let Some(previous) = index.checked_sub(1).and_then(|index| blocks.get(index)) {
            context_blocks.push(hosted_block_context_item(previous, false));
        }
        context_blocks.push(hosted_block_context_item(&blocks[index], true));
        if let Some(next) = blocks.get(index + 1) {
            context_blocks.push(hosted_block_context_item(next, false));
        }
        HostedBlockContextResponseBody {
            doc_id: document_id.to_string(),
            block_id: Some(block_id.to_string()),
            mode: "block",
            title: document.title.clone(),
            blocks: context_blocks,
        }
    } else {
        let headings = blocks
            .iter()
            .filter(|block| block.block_type == "heading")
            .map(|block| hosted_block_context_item(block, false))
            .collect::<Vec<_>>();
        if !headings.is_empty() {
            HostedBlockContextResponseBody {
                doc_id: document_id.to_string(),
                block_id: None,
                mode: "toc",
                title: document.title.clone(),
                blocks: headings,
            }
        } else {
            HostedBlockContextResponseBody {
                doc_id: document_id.to_string(),
                block_id: None,
                mode: "document",
                title: document.title.clone(),
                blocks: blocks
                    .iter()
                    .take(3)
                    .map(|block| hosted_block_context_item(block, false))
                    .collect(),
            }
        }
    };
    Ok(body)
}

pub(super) fn hosted_document_export_response(
    app: &AppHandle,
    graph_id: &str,
    document_id: &str,
    format: Option<&str>,
    theme: Option<&str>,
) -> Result<Response, String> {
    let export_format = format
        .map(|value| value.trim().to_ascii_lowercase())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "markdown".to_string());
    let document = read_document(app.clone(), graph_id.to_string(), document_id.to_string())?;
    document_export_response(&document, &export_format, theme)
}
