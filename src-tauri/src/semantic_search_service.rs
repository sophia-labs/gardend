use crate::app_runtime::AppHandle;
use crate::{
    paths::existing_graph_dir,
    semantic_embedder::embed_texts,
    semantic_index::{normalize_vector, read_semantic_index},
    semantic_mcp_inputs::semantic_search_input_from_mcp_args,
    semantic_models::{read_semantic_model_config, semantic_model_spec_by_id},
    semantic_search_projection::{ranked_semantic_hits, SemanticSearchHit},
};
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct SemanticSearchInput {
    pub(super) graph_id: String,
    pub(super) query: String,
    pub(super) limit: Option<usize>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct SemanticSearchResult {
    pub(super) graph_id: String,
    pub(super) query: String,
    pub(super) provider_id: String,
    pub(super) model_id: String,
    pub(super) dimensions: usize,
    pub(super) indexed_at: Option<String>,
    pub(super) block_count: usize,
    pub(super) hits: Vec<SemanticSearchHit>,
}

pub(super) fn mcp_local_semantic_search(
    app: AppHandle,
    arguments: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    serde_json::to_value(semantic_search(
        app,
        semantic_search_input_from_mcp_args(arguments),
    )?)
    .map_err(|error| error.to_string())
}

#[cfg_attr(feature = "desktop", tauri::command)]
pub(super) fn semantic_search(
    app: AppHandle,
    input: SemanticSearchInput,
) -> Result<SemanticSearchResult, String> {
    let graph_dir = existing_graph_dir(&app, &input.graph_id)?;
    let query = input.query.trim();
    if query.is_empty() {
        return Err("semantic search query cannot be empty".to_string());
    }
    let index = read_semantic_index(&graph_dir)?;
    let config = read_semantic_model_config(&app)?;
    let spec = semantic_model_spec_by_id(&config.selected_model_id)?;
    if index.manifest.model_id != spec.model_id || index.manifest.dimensions != spec.dimensions {
        return Err(format!(
            "semantic index model mismatch: active model {} ({} dimensions), index has {} ({} dimensions)",
            spec.model_id, spec.dimensions, index.manifest.model_id, index.manifest.dimensions
        ));
    }
    let mut query_vectors = embed_texts(&app, &[query.to_string()], "search_query")?;
    let Some(query_vector) = query_vectors.pop() else {
        return Err("fastembed returned no query vector".to_string());
    };
    let query_vector = normalize_vector(query_vector);
    let limit = input.limit.unwrap_or(10).clamp(1, 50);

    Ok(SemanticSearchResult {
        graph_id: input.graph_id,
        query: query.to_string(),
        provider_id: index.manifest.provider_id,
        model_id: index.manifest.model_id,
        dimensions: index.manifest.dimensions,
        indexed_at: Some(index.manifest.indexed_at),
        block_count: index.blocks.len(),
        hits: ranked_semantic_hits(&index.blocks, &query_vector, limit),
    })
}
