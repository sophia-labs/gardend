use crate::app_runtime::AppHandle;
use crate::{
    document_rendering::{document_blocks_for_read, document_markdown},
    document_service::{read_document, read_graph_documents_cold, DocumentRecord},
    mcp_utils::{mcp_arg_string, mcp_arg_usize, mcp_required_document_id, mcp_required_graph_id},
    paths::existing_graph_dir,
};

const DEFAULT_LIST_LIMIT: usize = 100;
const MAX_LIST_LIMIT: usize = 500;
const DEFAULT_LIST_BYTES: usize = 64 * 1024;
const MIN_LIST_BYTES: usize = 8 * 1024;
const MAX_LIST_BYTES: usize = 256 * 1024;
const DEFAULT_READ_CHARS: usize = 64 * 1024;
const MAX_READ_CHARS: usize = 256 * 1024;

pub(super) fn mcp_local_list_documents(
    app: AppHandle,
    arguments: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let graph_dir = existing_graph_dir(&app, &mcp_graph_id_argument(arguments))?;
    let mut documents: Vec<_> = read_graph_documents_cold(&graph_dir)?.iter().map(document_summary).collect();
    for row in crate::document_body_availability::unavailable_documents(&graph_dir)? {
        documents.push(serde_json::json!({"documentId":row["documentId"],
            "title":row["metadata"]["title"],"createdAt":row["metadata"]["createdAt"],
            "updatedAt":row["metadata"]["updatedAt"],"revision":null,"blockCount":null,
            "bodyAvailability":{"state":"source-body-unavailable","reason":row["reason"],
                "inventorySha256":row["inventorySha256"]}}));
    }
    documents.sort_by(|left, right| {
        left["title"].as_str().unwrap_or("")
            .to_lowercase()
            .cmp(&right["title"].as_str().unwrap_or("").to_lowercase())
            .then_with(|| left["documentId"].as_str().cmp(&right["documentId"].as_str()))
    });
    let cursor = decimal_cursor(arguments, "document")?;
    let limit = mcp_arg_usize(arguments, &["limit"], DEFAULT_LIST_LIMIT).clamp(1, MAX_LIST_LIMIT);
    let max_bytes = mcp_arg_usize(arguments, &["maxBytes", "max_bytes"], DEFAULT_LIST_BYTES)
        .clamp(MIN_LIST_BYTES, MAX_LIST_BYTES);
    let total = documents.len();
    let start = cursor.min(total);
    let mut page = Vec::new();
    for document in documents.iter().skip(start).take(limit) {
        let mut candidate = page.clone();
        candidate.push(document.clone());
        let next = start + candidate.len();
        let candidate_value = document_list_page(candidate, total, start, next, limit, max_bytes);
        let (_, candidate_bytes) = finalize_document_list_bytes(candidate_value)?;
        if candidate_bytes > max_bytes {
            break;
        }
        page.push(document.clone());
    }
    let next = start + page.len();
    let response = document_list_page(page, total, start, next, limit, max_bytes);
    let (response, _) = finalize_document_list_bytes(response)?;
    Ok(response)
}

fn finalize_document_list_bytes(
    mut value: serde_json::Value,
) -> Result<(serde_json::Value, usize), String> {
    loop {
        let bytes = serde_json::to_vec(&value)
            .map_err(|error| format!("serialize bounded document listing: {error}"))?
            .len();
        if value["page"]["serializedBytes"].as_u64() == Some(bytes as u64) {
            return Ok((value, bytes));
        }
        value["page"]["serializedBytes"] = serde_json::json!(bytes);
    }
}

fn mcp_graph_id_argument(arguments: &serde_json::Value) -> String {
    arguments
        .get("graphId")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string()
}

pub(super) fn mcp_local_read_document(
    app: AppHandle,
    arguments: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let graph_id = mcp_required_graph_id(arguments)?;
    let document_id = mcp_required_document_id(arguments)?;
    let format = mcp_arg_string(arguments, &["format"]).unwrap_or_else(|| "xml".to_string());
    let document = read_document(app, graph_id.clone(), document_id.clone())?;
    let blocks = document_blocks_for_read(&document);

    if format == "ids_only" {
        let cursor = decimal_cursor(arguments, "block")?;
        let limit = mcp_arg_usize(arguments, &["limit"], 200).clamp(1, 1_000);
        let total = blocks.len();
        let start = cursor.min(total);
        let block_ids = blocks
            .iter()
            .skip(start)
            .take(limit)
            .map(|block| block.id.clone())
            .collect::<Vec<_>>();
        let next = start + block_ids.len();
        return Ok(serde_json::json!({
            "graph_id": graph_id,
            "document_id": document_id,
            "format": "ids_only",
            "block_ids": block_ids,
            "block_count": total,
            "page": {
                "cursor": start.to_string(),
                "nextCursor": if next >= total { serde_json::Value::Null } else { serde_json::json!(next.to_string()) },
                "limit": limit,
                "returned": next - start,
                "complete": next >= total,
            },
        }));
    }

    let content = match format.as_str() {
        "markdown" => document_markdown(&document),
        "json" => serde_json::to_string_pretty(&serde_json::json!({
            "tiptapJson": document.tiptap_json,
            "tree": document.tree,
            "blocks": document.blocks,
        }))
        .map_err(|error| format!("serialize document json: {error}"))?,
        "xml" => {
            if document.tiptap_xml.trim().is_empty() {
                document.body.clone()
            } else {
                document.tiptap_xml.clone()
            }
        }
        other => return Err(format!("unsupported read_document format: {other}")),
    };

    let total_chars = content.chars().count();
    let offset = mcp_arg_usize(arguments, &["offset"], 0).min(total_chars);
    let max_chars = mcp_arg_usize(arguments, &["maxChars", "max_chars"], DEFAULT_READ_CHARS)
        .clamp(1, MAX_READ_CHARS);
    let visible_content = content
        .chars()
        .skip(offset)
        .take(max_chars)
        .collect::<String>();
    let next_offset = offset + visible_content.chars().count();
    let complete = next_offset >= total_chars;

    Ok(serde_json::json!({
        "graph_id": graph_id,
        "document_id": document_id,
        "title": document.title,
        "format": format,
        "content": visible_content,
        "fragment": offset > 0 || !complete,
        "offset": offset,
        "nextOffset": if complete { serde_json::Value::Null } else { serde_json::json!(next_offset) },
        "complete": complete,
        "totalChars": total_chars,
        "maxChars": max_chars,
        "updated_at": document.updated_at,
        "created_at": document.created_at,
        "revision": document.revision,
        "block_count": blocks.len(),
        "rdfTripleCount": document.rdf_triple_count,
        "schemaVersion": document.schema_version,
    }))
}

fn document_summary(document: &DocumentRecord) -> serde_json::Value {
    serde_json::json!({
        "documentId": document.document_id,
        "title": document.title.chars().take(512).collect::<String>(),
        "createdAt": document.created_at,
        "updatedAt": document.updated_at,
        "blockCount": document.blocks.len(),
        "rdfTripleCount": document.rdf_triple_count,
        "schemaVersion": document.schema_version,
    })
}

fn document_list_page(
    documents: Vec<serde_json::Value>,
    total: usize,
    start: usize,
    next: usize,
    limit: usize,
    max_bytes: usize,
) -> serde_json::Value {
    let complete = next >= total;
    serde_json::json!({
        "documents": documents,
        "count": total,
        "page": {
            "cursor": start.to_string(),
            "nextCursor": if complete { serde_json::Value::Null } else { serde_json::json!(next.to_string()) },
            "limit": limit,
            "maxBytes": max_bytes,
            "returned": next - start,
            "complete": complete,
            "serializedBytes": 0,
        },
    })
}

fn decimal_cursor(arguments: &serde_json::Value, label: &str) -> Result<usize, String> {
    let Some(value) = arguments.get("cursor") else {
        return Ok(0);
    };
    if let Some(offset) = value.as_u64() {
        return usize::try_from(offset).map_err(|_| format!("{label} cursor is too large"));
    }
    value
        .as_str()
        .ok_or_else(|| format!("{label} cursor must be a non-negative integer string"))?
        .parse::<usize>()
        .map_err(|_| format!("{label} cursor must be a non-negative integer string"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcp_graph_id_argument_preserves_route_defaults() {
        assert_eq!(
            mcp_graph_id_argument(&serde_json::json!({ "graph_id": "ignored" })),
            ""
        );
        assert_eq!(
            mcp_graph_id_argument(&serde_json::json!({ "graphId": "graph-a" })),
            "graph-a"
        );
    }

    #[test]
    fn decimal_cursor_is_bounded_and_explicit() {
        assert_eq!(
            decimal_cursor(&serde_json::json!({"cursor": "12"}), "document").unwrap(),
            12
        );
        assert!(decimal_cursor(&serde_json::json!({"cursor": -1}), "document").is_err());
    }

    #[test]
    fn document_list_byte_metadata_is_exact() {
        let value = document_list_page(
            vec![serde_json::json!({"title": "x".repeat(10_000)})],
            1,
            0,
            1,
            100,
            DEFAULT_LIST_BYTES,
        );
        let (value, bytes) = finalize_document_list_bytes(value).unwrap();
        assert_eq!(value["page"]["serializedBytes"], bytes);
        assert_eq!(serde_json::to_vec(&value).unwrap().len(), bytes);
    }
}
