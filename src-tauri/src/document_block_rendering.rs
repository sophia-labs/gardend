use crate::document_service::{BlockSnapshot, DocumentRecord};

pub(super) fn document_blocks_for_read(document: &DocumentRecord) -> Vec<BlockSnapshot> {
    if !document.blocks.is_empty() {
        return document.blocks.clone();
    }

    let content = if !document.body.trim().is_empty() {
        document.body.clone()
    } else {
        document.title.clone()
    };

    vec![BlockSnapshot {
        id: "body".to_string(),
        block_type: "document".to_string(),
        content,
        parent_id: None,
        order: 0.0,
        level: None,
        checked: None,
        language: None,
        marks: Vec::new(),
    }]
}

pub(super) fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

pub(super) fn html_escape(value: &str) -> String {
    xml_escape(value).replace('\'', "&#39;")
}

pub(super) fn render_block_content(block: &BlockSnapshot, format: &str) -> String {
    match format {
        "text" | "markdown" => block.content.clone(),
        "xml" => format!(
            "<{} data-block-id=\"{}\">{}</{}>",
            block.block_type,
            xml_escape(&block.id),
            xml_escape(&block.content),
            block.block_type
        ),
        _ => block.content.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        rdf::document_subject,
        runtime_config::{DOCUMENT_SCHEMA_VERSION, LOCAL_GRAPH_ORIGIN, LOCAL_PROVIDER_ID},
    };

    fn test_document(blocks: Vec<BlockSnapshot>) -> DocumentRecord {
        DocumentRecord {
            document_id: "doc-a".to_string(),
            graph_id: "graph-a".to_string(),
            title: "Document A".to_string(),
            revision: 1,
            body: "Fallback body".to_string(),
            origin: LOCAL_GRAPH_ORIGIN.to_string(),
            provider_id: LOCAL_PROVIDER_ID.to_string(),
            local_path: "/tmp/doc-a".to_string(),
            rdf_subject: document_subject("doc-a"),
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

    #[test]
    fn document_blocks_for_read_prefers_materialized_blocks_and_falls_back() {
        let materialized = test_document(vec![BlockSnapshot {
            id: "a".to_string(),
            block_type: "paragraph".to_string(),
            content: "Materialized".to_string(),
            parent_id: None,
            order: 0.0,
            level: None,
            checked: None,
            language: None,
            marks: Vec::new(),
        }]);
        assert_eq!(
            document_blocks_for_read(&materialized)[0].content,
            "Materialized"
        );

        let mut fallback = test_document(Vec::new());
        fallback.body = "Body text".to_string();
        assert_eq!(document_blocks_for_read(&fallback)[0].content, "Body text");

        fallback.body = "  ".to_string();
        assert_eq!(document_blocks_for_read(&fallback)[0].content, "Document A");
    }
}
