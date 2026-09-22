use crate::{
    document_projection_service::document_blocks_for_read, document_service::DocumentRecord,
};

pub(super) fn push_lexical_block_hits(
    results: &mut Vec<serde_json::Value>,
    documents: &[DocumentRecord],
    queries: &[String],
    doc_filter: Option<&str>,
    limit: usize,
    case_sensitive: bool,
) {
    for document in documents {
        if let Some(doc_filter) = doc_filter {
            if document.document_id != doc_filter {
                continue;
            }
        }
        for block in document_blocks_for_read(document) {
            for query in queries {
                let matched = if case_sensitive {
                    block.content.contains(query)
                } else {
                    block.content.to_lowercase().contains(&query.to_lowercase())
                };
                if !matched {
                    continue;
                }
                results.push(serde_json::json!({
                    "document_id": document.document_id,
                    "documentId": document.document_id,
                    "document_title": document.title,
                    "documentTitle": document.title,
                    "block_id": block.id,
                    "blockId": block.id,
                    "block_type": block.block_type,
                    "blockType": block.block_type,
                    "content": block.content,
                    "order": block.order,
                    "score": 1.0,
                    "match_source": "lexical",
                    "matchSource": "lexical",
                    "query": query,
                }));
                if results.len() >= limit {
                    return;
                }
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document_service::BlockSnapshot;
    use crate::rdf::document_subject;
    use crate::runtime_config::{DOCUMENT_SCHEMA_VERSION, LOCAL_GRAPH_ORIGIN, LOCAL_PROVIDER_ID};

    fn test_document(document_id: &str, title: &str, blocks: Vec<BlockSnapshot>) -> DocumentRecord {
        DocumentRecord {
            document_id: document_id.to_string(),
            graph_id: "graph-a".to_string(),
            title: title.to_string(),
            revision: 1,
            body: blocks
                .iter()
                .map(|block| block.content.as_str())
                .collect::<Vec<_>>()
                .join("\n"),
            origin: LOCAL_GRAPH_ORIGIN.to_string(),
            provider_id: LOCAL_PROVIDER_ID.to_string(),
            local_path: format!("/tmp/{document_id}.json"),
            rdf_subject: document_subject(document_id),
            created_at: "1000".to_string(),
            updated_at: "2000".to_string(),
            capabilities: Vec::new(),
            schema_version: DOCUMENT_SCHEMA_VERSION,
            tiptap_xml: String::new(),
            tiptap_json: None,
            ydoc_update_base64: String::new(),
            ydoc_state_path: String::new(),
            tree: None,
            blocks,
            rdf_triple_count: 0,
            document_kind: None,
        }
    }

    fn test_block(id: &str, content: &str, order: f64) -> BlockSnapshot {
        BlockSnapshot {
            id: id.to_string(),
            block_type: "paragraph".to_string(),
            content: content.to_string(),
            parent_id: None,
            order,
            level: None,
            checked: None,
            language: None,
            marks: Vec::new(),
        }
    }

    #[test]
    fn lexical_block_hits_respect_doc_filter_case_and_limit() {
        let documents = vec![
            test_document(
                "doc-a",
                "Doc A",
                vec![
                    test_block("block-a", "Alpha beta", 0.0),
                    test_block("block-b", "alpha gamma", 1.0),
                ],
            ),
            test_document(
                "doc-b",
                "Doc B",
                vec![test_block("block-c", "Alpha beta", 0.0)],
            ),
        ];
        let queries = vec!["alpha".to_string()];
        let mut results = Vec::new();

        push_lexical_block_hits(&mut results, &documents, &queries, Some("doc-a"), 1, false);

        assert_eq!(results.len(), 1);
        assert_eq!(results[0]["document_id"], "doc-a");
        assert_eq!(results[0]["block_id"], "block-a");
        assert_eq!(results[0]["match_source"], "lexical");

        let mut case_sensitive_results = Vec::new();
        push_lexical_block_hits(
            &mut case_sensitive_results,
            &documents,
            &queries,
            Some("doc-a"),
            10,
            true,
        );

        assert_eq!(case_sensitive_results.len(), 1);
        assert_eq!(case_sensitive_results[0]["block_id"], "block-b");
    }
}
