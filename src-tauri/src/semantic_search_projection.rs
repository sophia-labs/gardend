use crate::{
    document_service::DocumentRecord,
    rdf::document_subject,
    semantic_index::{
        dot_product, normalize_semantic_text, semantic_content_hash, semantic_text_is_indexable,
        SemanticBlockEmbedding, SemanticBlockSource,
    },
};
use serde::Serialize;

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct SemanticSearchHit {
    pub(super) document_id: String,
    pub(super) document_title: String,
    pub(super) block_id: String,
    pub(super) block_type: String,
    pub(super) content: String,
    pub(super) score: f32,
    pub(super) order: f64,
}

pub(super) fn ranked_semantic_hits(
    blocks: &[SemanticBlockEmbedding],
    query_vector: &[f32],
    limit: usize,
) -> Vec<SemanticSearchHit> {
    let mut hits = blocks
        .iter()
        .filter(|block| block.vector.len() == query_vector.len())
        .map(|block| (dot_product(query_vector, &block.vector), block))
        .collect::<Vec<_>>();
    hits.sort_by(|left, right| right.0.total_cmp(&left.0));
    hits.into_iter()
        .take(limit)
        .map(|(score, block)| SemanticSearchHit {
            document_id: block.document_id.clone(),
            document_title: block.document_title.clone(),
            block_id: block.block_id.clone(),
            block_type: block.block_type.clone(),
            content: block.content.clone(),
            score,
            order: block.order,
        })
        .collect()
}

pub(super) fn semantic_block_sources(documents: &[DocumentRecord]) -> Vec<SemanticBlockSource> {
    let mut sources = Vec::new();
    for document in documents {
        if document.blocks.is_empty() {
            let content = normalize_semantic_text(&document.body);
            if semantic_text_is_indexable(&content) {
                let iri = if document.rdf_subject.trim().is_empty() {
                    document_subject(&document.document_id)
                } else {
                    document.rdf_subject.clone()
                };
                sources.push(SemanticBlockSource {
                    iri,
                    kind: "document".to_string(),
                    graph_id: document.graph_id.clone(),
                    document_id: document.document_id.clone(),
                    document_title: document.title.clone(),
                    block_id: "body".to_string(),
                    block_type: "document".to_string(),
                    content_hash: semantic_content_hash(&content),
                    content,
                    order: 0.0,
                });
            }
            continue;
        }

        for block in &document.blocks {
            let content = normalize_semantic_text(&block.content);
            if !semantic_text_is_indexable(&content) {
                continue;
            }
            let block_id = if block.id.is_empty() {
                format!("block-{}", block.order)
            } else {
                block.id.clone()
            };
            let document_iri = if document.rdf_subject.trim().is_empty() {
                document_subject(&document.document_id)
            } else {
                document.rdf_subject.clone()
            };
            sources.push(SemanticBlockSource {
                iri: format!("{document_iri}#block-{block_id}"),
                kind: "block".to_string(),
                graph_id: document.graph_id.clone(),
                document_id: document.document_id.clone(),
                document_title: document.title.clone(),
                block_id,
                block_type: block.block_type.clone(),
                content_hash: semantic_content_hash(&content),
                content,
                order: block.order,
            });
        }
    }
    sources
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        document_service::{BlockSnapshot, DocumentRecord},
        rdf::document_subject,
        runtime_config::{DOCUMENT_SCHEMA_VERSION, LOCAL_GRAPH_ORIGIN, LOCAL_PROVIDER_ID},
    };

    fn document_with_body(document_id: &str, body: &str) -> DocumentRecord {
        DocumentRecord {
            document_id: document_id.to_string(),
            graph_id: "graph-a".to_string(),
            title: format!("Document {document_id}"),
            revision: 0,
            body: body.to_string(),
            origin: LOCAL_GRAPH_ORIGIN.to_string(),
            provider_id: LOCAL_PROVIDER_ID.to_string(),
            local_path: String::new(),
            rdf_subject: document_subject(document_id),
            created_at: "1".to_string(),
            updated_at: "2".to_string(),
            capabilities: Vec::new(),
            schema_version: DOCUMENT_SCHEMA_VERSION,
            tiptap_xml: String::new(),
            tiptap_json: None,
            ydoc_update_base64: String::new(),
            ydoc_state_path: String::new(),
            tree: None,
            blocks: Vec::new(),
            rdf_triple_count: 0,
            document_kind: None,
        }
    }

    fn semantic_block(
        document_id: &str,
        block_id: &str,
        vector: Vec<f32>,
    ) -> SemanticBlockEmbedding {
        SemanticBlockEmbedding {
            iri: format!("urn:mnemosyne:local:document:{document_id}#block-{block_id}"),
            kind: "block".to_string(),
            graph_id: "graph-a".to_string(),
            document_id: document_id.to_string(),
            document_title: format!("Document {document_id}"),
            block_id: block_id.to_string(),
            block_type: "paragraph".to_string(),
            content: format!("content for {block_id}"),
            content_hash: semantic_content_hash(block_id),
            order: 1.0,
            vector,
            vector_ref: None,
        }
    }

    #[test]
    fn semantic_block_sources_use_body_fallback_and_skip_short_text() {
        let long = document_with_body("doc-long", "alpha beta gamma");
        let short = document_with_body("doc-short", "one two");

        let sources = semantic_block_sources(&[long, short]);

        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].document_id, "doc-long");
        assert_eq!(sources[0].iri, document_subject("doc-long"));
        assert_eq!(sources[0].kind, "document");
        assert_eq!(sources[0].block_id, "body");
        assert_eq!(sources[0].block_type, "document");
        assert_eq!(sources[0].content, "alpha beta gamma");
        assert_eq!(
            sources[0].content_hash,
            semantic_content_hash("alpha beta gamma")
        );
    }

    #[test]
    fn semantic_block_sources_prefer_materialized_blocks() {
        let mut document = document_with_body("doc-blocks", "body should be ignored");
        document.blocks = vec![
            BlockSnapshot {
                id: String::new(),
                block_type: "paragraph".to_string(),
                content: "alpha beta gamma".to_string(),
                parent_id: None,
                order: 7.0,
                level: None,
                checked: None,
                language: None,
                marks: Vec::new(),
            },
            BlockSnapshot {
                id: "block-short".to_string(),
                block_type: "paragraph".to_string(),
                content: "too short".to_string(),
                parent_id: None,
                order: 8.0,
                level: None,
                checked: None,
                language: None,
                marks: Vec::new(),
            },
        ];

        let sources = semantic_block_sources(&[document]);

        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].block_id, "block-7");
        assert_eq!(
            sources[0].iri,
            format!("{}#block-block-7", document_subject("doc-blocks"))
        );
        assert_eq!(sources[0].kind, "block");
        assert_eq!(sources[0].order, 7.0);
        assert_eq!(sources[0].content, "alpha beta gamma");
    }

    #[test]
    fn ranked_semantic_hits_sort_and_ignore_dimension_mismatches() {
        let blocks = vec![
            semantic_block("doc-low", "low", vec![0.0, 1.0]),
            semantic_block("doc-high", "high", vec![1.0, 0.0]),
            semantic_block("doc-wrong", "wrong", vec![1.0, 0.0, 0.0]),
        ];

        let hits = ranked_semantic_hits(&blocks, &[1.0, 0.0], 10);

        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].block_id, "high");
        assert_eq!(hits[0].score, 1.0);
        assert_eq!(hits[1].block_id, "low");
        assert_eq!(hits[1].score, 0.0);
    }
}
