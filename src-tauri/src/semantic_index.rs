use crate::{
    paths::{semantic_index_dir, semantic_index_path},
    storage::{create_dir_all, read_json, write_json},
};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeSet, path::Path};

pub(crate) use crate::semantic_index_utils::{
    dot_product, normalize_semantic_text, normalize_vector, semantic_content_hash,
    semantic_text_is_indexable,
};

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SemanticIndexFile {
    pub(crate) manifest: SemanticIndexManifest,
    pub(crate) blocks: Vec<SemanticBlockEmbedding>,
    #[serde(default)]
    pub(crate) entities: Vec<SemanticEntity>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SemanticIndexManifest {
    pub(crate) schema_version: u32,
    pub(crate) graph_id: String,
    pub(crate) provider_id: String,
    pub(crate) model_id: String,
    pub(crate) dimensions: usize,
    pub(crate) block_count: usize,
    pub(crate) document_count: usize,
    pub(crate) indexed_at: String,
    pub(crate) index_path: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SemanticBlockEmbedding {
    #[serde(default)]
    pub(crate) iri: String,
    #[serde(default)]
    pub(crate) kind: String,
    pub(crate) graph_id: String,
    pub(crate) document_id: String,
    pub(crate) document_title: String,
    pub(crate) block_id: String,
    pub(crate) block_type: String,
    pub(crate) content: String,
    pub(crate) content_hash: String,
    pub(crate) order: f64,
    pub(crate) vector: Vec<f32>,
    #[serde(default)]
    pub(crate) vector_ref: Option<SemanticVectorRef>,
}

#[derive(Debug, Clone)]
pub(crate) struct SemanticBlockSource {
    pub(crate) iri: String,
    pub(crate) kind: String,
    pub(crate) graph_id: String,
    pub(crate) document_id: String,
    pub(crate) document_title: String,
    pub(crate) block_id: String,
    pub(crate) block_type: String,
    pub(crate) content: String,
    pub(crate) content_hash: String,
    pub(crate) order: f64,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SemanticVectorRef {
    pub(crate) file: String,
    pub(crate) index: usize,
    pub(crate) dimensions: usize,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SemanticEntity {
    pub(crate) iri: String,
    pub(crate) kind: String,
    pub(crate) vector_ref: SemanticVectorRef,
    pub(crate) content_hash: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SemanticIndexStatus {
    pub(crate) graph_id: String,
    pub(crate) provider_id: String,
    pub(crate) model_id: String,
    pub(crate) dimensions: usize,
    pub(crate) active_model_id: String,
    pub(crate) active_dimensions: usize,
    pub(crate) compatible: bool,
    pub(crate) compatibility_reason: String,
    pub(crate) exists: bool,
    pub(crate) index_path: String,
    pub(crate) block_count: usize,
    pub(crate) document_count: usize,
    pub(crate) stale_document_count: usize,
    pub(crate) indexed_at: Option<String>,
}

pub(crate) fn semantic_vector_ref(index: usize, dimensions: usize) -> SemanticVectorRef {
    SemanticVectorRef {
        file: "blocks.json".to_string(),
        index,
        dimensions,
    }
}

pub(crate) fn semantic_entities_from_blocks(
    blocks: &[SemanticBlockEmbedding],
    dimensions: usize,
) -> Vec<SemanticEntity> {
    blocks
        .iter()
        .enumerate()
        .filter_map(|(index, block)| {
            if block.iri.trim().is_empty() {
                return None;
            }
            Some(SemanticEntity {
                iri: block.iri.clone(),
                kind: block.kind.clone(),
                vector_ref: block
                    .vector_ref
                    .clone()
                    .unwrap_or_else(|| semantic_vector_ref(index, dimensions)),
                content_hash: block.content_hash.clone(),
            })
        })
        .collect()
}

pub(crate) fn read_semantic_index(graph_dir: &Path) -> Result<SemanticIndexFile, String> {
    let path = semantic_index_path(graph_dir);
    if !path.is_file() {
        return Err("semantic index not found; run refresh_semantic_index first".to_string());
    }
    read_json::<SemanticIndexFile>(&path).map_err(Into::into)
}

pub(crate) fn write_semantic_index(
    graph_dir: &Path,
    index: &SemanticIndexFile,
) -> Result<(), String> {
    let _flush_guard = crate::cell_durability::write_guard();
    create_dir_all(&semantic_index_dir(graph_dir))?;
    write_json(&semantic_index_path(graph_dir), index).map_err(Into::into)
}

pub(crate) fn semantic_index_document_count(blocks: &[SemanticBlockEmbedding]) -> usize {
    blocks
        .iter()
        .map(|block| block.document_id.as_str())
        .collect::<BTreeSet<_>>()
        .len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn semantic_index_document_count_uses_unique_documents() {
        let block = |document_id: &str| SemanticBlockEmbedding {
            iri: format!("urn:mnemosyne:local:document:{document_id}#block-block"),
            kind: "block".to_string(),
            graph_id: "graph".to_string(),
            document_id: document_id.to_string(),
            document_title: "Document".to_string(),
            block_id: "block".to_string(),
            block_type: "paragraph".to_string(),
            content: "alpha beta gamma".to_string(),
            content_hash: semantic_content_hash("alpha beta gamma"),
            order: 1.0,
            vector: vec![1.0],
            vector_ref: Some(semantic_vector_ref(0, 1)),
        };

        assert_eq!(
            semantic_index_document_count(&[block("doc-1"), block("doc-1"), block("doc-2")]),
            2
        );
    }

    #[test]
    fn semantic_entities_reference_blocks_by_vector_ref() {
        let blocks = vec![SemanticBlockEmbedding {
            iri: "urn:mnemosyne:local:document:doc#block-a".to_string(),
            kind: "block".to_string(),
            graph_id: "graph".to_string(),
            document_id: "doc".to_string(),
            document_title: "Document".to_string(),
            block_id: "a".to_string(),
            block_type: "paragraph".to_string(),
            content: "alpha beta gamma".to_string(),
            content_hash: semantic_content_hash("alpha beta gamma"),
            order: 1.0,
            vector: vec![1.0, 0.0],
            vector_ref: None,
        }];

        let entities = semantic_entities_from_blocks(&blocks, 2);

        assert_eq!(
            entities,
            vec![SemanticEntity {
                iri: "urn:mnemosyne:local:document:doc#block-a".to_string(),
                kind: "block".to_string(),
                vector_ref: semantic_vector_ref(0, 2),
                content_hash: semantic_content_hash("alpha beta gamma"),
            }]
        );
    }
}
