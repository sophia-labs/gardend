use crate::app_runtime::AppHandle;
pub(super) use crate::salience_important_blocks::mcp_local_get_important_blocks;
pub(super) use crate::salience_mcp_valuation::mcp_local_value;
pub(super) use crate::salience_score_projection::{
    local_block_value_scores, local_block_value_scores_for,
};
use crate::{
    clock::timestamp,
    graph_service::touch_graph_updated_at,
    mcp_utils::{mcp_arg_string, mcp_arg_usize, mcp_graph_id_or_default},
    paths::existing_graph_dir,
    profile_service::touch_profile_updated_at,
    salience_rdf_materializer::reconcile_value_store,
    salience_route_service::local_value_config_json,
    salience_value_store::{
        default_value_weights, normalize_value_config, read_value_store_for, write_value_store,
        LocalValueConfigArchive,
    },
};
use std::collections::{BTreeMap, BTreeSet};

pub(super) fn mcp_local_get_values(
    app: AppHandle,
    arguments: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let graph_id = mcp_graph_id_or_default(&app, arguments)?;
    let graph_dir = existing_graph_dir(&app, &graph_id)?;
    let observer =
        mcp_arg_string(arguments, &["observer_agent_id", "observerAgentId"]).unwrap_or_default();
    let store = read_value_store_for(&graph_dir, &graph_id, &observer)?;
    Ok(local_value_config_json(&graph_id, &store))
}

fn parse_value_weights_text(text: &str) -> (BTreeMap<String, f64>, BTreeSet<String>) {
    let mut weights = default_value_weights();
    let defaults = default_value_weights();
    let mut specified = BTreeSet::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (key, value) = if let Some((key, value)) = line.split_once(':') {
            (key.trim(), value.trim())
        } else if let Some((key, value)) = line.split_once('=') {
            (key.trim(), value.trim())
        } else {
            continue;
        };
        if !defaults.contains_key(key) {
            continue;
        }
        if let Ok(parsed) = value.parse::<f64>() {
            weights.insert(key.to_string(), parsed);
            specified.insert(key.to_string());
        }
    }
    (weights, specified)
}

pub(super) fn mcp_local_revaluate(
    app: AppHandle,
    arguments: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let graph_id = mcp_graph_id_or_default(&app, arguments)?;
    let graph_dir = existing_graph_dir(&app, &graph_id)?;
    let observer =
        mcp_arg_string(arguments, &["observer_agent_id", "observerAgentId"]).unwrap_or_default();
    let mut store = read_value_store_for(&graph_dir, &graph_id, &observer)?;
    let mut updated = Vec::new();
    let original = store.config.clone();

    if let Some(importance_prompt) =
        mcp_arg_string(arguments, &["importance_prompt", "importancePrompt"])
    {
        store.config.importance_prompt = importance_prompt;
        updated.push("importance_prompt".to_string());
    }
    if let Some(valence_prompt) = mcp_arg_string(arguments, &["valence_prompt", "valencePrompt"]) {
        store.config.valence_prompt = valence_prompt;
        updated.push("valence_prompt".to_string());
    }
    if let Some(weights_text) = mcp_arg_string(arguments, &["weights"]) {
        let (new_weights, specified) = parse_value_weights_text(&weights_text);
        for key in specified {
            if let Some(value) = new_weights.get(&key) {
                store.config.weights.insert(key.clone(), *value);
                if key == "half_life_days" {
                    store.config.temporal_half_life_days = *value;
                }
            }
        }
        updated.push("weights".to_string());
    }

    normalize_value_config(&mut store.config);
    if !updated.is_empty() {
        store.config_history.push(LocalValueConfigArchive {
            archived_at: timestamp(),
            updated: updated.clone(),
            config: original,
        });
        write_value_store(&graph_dir, &store)?;
        // TODO(MO-delta-routing): route the structured TripleDiff (op_count /
        // adds / removes) instead of discarding it.
        let _diff = reconcile_value_store(&graph_dir, &store)?;
        touch_graph_updated_at(&graph_dir)?;
        touch_profile_updated_at(&app)?;
    }

    Ok(serde_json::json!({
        "success": true,
        "updated": updated,
        "config": local_value_config_json(&graph_id, &store),
    }))
}

pub(super) fn mcp_local_get_block_values(
    app: AppHandle,
    arguments: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let graph_id = mcp_graph_id_or_default(&app, arguments)?;
    let observer =
        mcp_arg_string(arguments, &["observer_agent_id", "observerAgentId"]).unwrap_or_default();
    let document_id = mcp_arg_string(arguments, &["document_id", "documentId"]);
    let block_id = mcp_arg_string(arguments, &["block_id", "blockId"]);
    let limit = mcp_arg_usize(arguments, &["limit"], 20).clamp(1, 100);
    let min_score = arguments
        .get("min_score")
        .or_else(|| arguments.get("minScore"))
        .and_then(serde_json::Value::as_f64);
    let valence_filter = mcp_arg_string(arguments, &["valence"]);
    let blocks = local_block_value_scores_for(
        &app,
        &graph_id,
        &observer,
        document_id.as_deref(),
        block_id.as_deref(),
        None,
        limit,
        min_score,
        valence_filter.as_deref(),
    )?;
    Ok(serde_json::json!({
        "blocks": blocks,
        "count": blocks.len(),
        "graph_id": graph_id,
        "graphId": graph_id,
        "source": "local-value-store",
    }))
}
