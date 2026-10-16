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
use std::collections::BTreeMap;

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

/// Parse the `weights` text of `revaluate`. Two spellings are accepted: one
/// `key: value` (or `key = value`) per line, and a JSON object. Unknown keys,
/// non-numeric values, and text that names no key at all are errors, so the
/// caller never sees "weights" reported as updated when nothing was applied.
fn parse_value_weights_text(text: &str) -> Result<BTreeMap<String, f64>, String> {
    let known = default_value_weights();
    let mut parsed = BTreeMap::new();
    let mut problems = Vec::new();
    let mut malformed = Vec::new();
    let mut accept = |key: &str, value: Option<f64>, raw: &str| {
        if !known.contains_key(key) {
            problems.push(format!("unknown weight \"{key}\""));
        } else if let Some(value) = value.filter(|value| value.is_finite()) {
            parsed.insert(key.to_string(), value);
        } else {
            problems.push(format!("{key}: \"{raw}\" is not a number"));
        }
    };
    let trimmed = text.trim();
    if trimmed.starts_with('{') {
        let object: serde_json::Map<String, serde_json::Value> = serde_json::from_str(trimmed)
            .map_err(|error| format!("weights looks like JSON but does not parse: {error}"))?;
        for (key, value) in &object {
            let number = value.as_f64().or_else(|| {
                value
                    .as_str()
                    .and_then(|raw| raw.trim().parse::<f64>().ok())
            });
            accept(key.trim(), number, &value.to_string());
        }
    } else {
        for line in trimmed.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((key, value)) = line.split_once(':').or_else(|| line.split_once('=')) else {
                malformed.push(format!("\"{line}\" is not a key: value line"));
                continue;
            };
            let key = key.trim().trim_matches('"');
            let raw = value.trim().trim_end_matches(',').trim().trim_matches('"');
            accept(key, raw.parse::<f64>().ok(), raw);
        }
    }
    problems.extend(malformed);
    if !problems.is_empty() {
        let mut names: Vec<&str> = known.keys().map(String::as_str).collect();
        names.sort_unstable();
        return Err(format!(
            "revaluate weights not applied: {}. Known weights: {}",
            problems.join("; "),
            names.join(", ")
        ));
    }
    if parsed.is_empty() {
        return Err("revaluate weights names no weight; nothing applied".to_string());
    }
    Ok(parsed)
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
        for (key, value) in parse_value_weights_text(&weights_text)? {
            if key == "half_life_days" {
                store.config.temporal_half_life_days = value;
            }
            store.config.weights.insert(key, value);
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

#[cfg(test)]
mod tests {
    use super::parse_value_weights_text;

    #[test]
    fn weights_parse_from_lines() {
        let parsed =
            parse_value_weights_text("importance_weight: 0.4\n# note\nhalf_life_days = 14\n")
                .unwrap();
        assert_eq!(parsed.get("importance_weight"), Some(&0.4));
        assert_eq!(parsed.get("half_life_days"), Some(&14.0));
        assert_eq!(parsed.len(), 2);
    }

    #[test]
    fn weights_parse_from_json_object() {
        let parsed = parse_value_weights_text(
            r#"{"importance_weight": 0.4, "valence_ref": "4", "temporal_weight": 0.1}"#,
        )
        .unwrap();
        assert_eq!(parsed.get("importance_weight"), Some(&0.4));
        assert_eq!(parsed.get("valence_ref"), Some(&4.0));
        assert_eq!(parsed.get("temporal_weight"), Some(&0.1));
    }

    #[test]
    fn weights_parse_json_shaped_lines() {
        // The body of a pretty-printed JSON object, without its braces.
        let parsed =
            parse_value_weights_text("\"importance_weight\": 0.4,\n\"valence_weight\": 0.2,")
                .unwrap();
        assert_eq!(parsed.get("importance_weight"), Some(&0.4));
        assert_eq!(parsed.get("valence_weight"), Some(&0.2));
    }

    #[test]
    fn weights_errors_are_loud_and_apply_nothing() {
        let unknown = parse_value_weights_text("importance_wieght: 0.4").unwrap_err();
        assert!(
            unknown.contains("unknown weight \"importance_wieght\""),
            "{unknown}"
        );
        assert!(
            unknown.contains("importance_weight"),
            "lists the known keys: {unknown}"
        );
        assert!(parse_value_weights_text(r#"{"importance_weight": "high"}"#).is_err());
        assert!(parse_value_weights_text(r#"{"importance_weight": 0.4"#).is_err());
        assert!(parse_value_weights_text("# only a comment").is_err());
        assert!(parse_value_weights_text("importance_weight 0.4").is_err());
        // One bad key rejects the whole call rather than applying the rest.
        assert!(parse_value_weights_text("importance_weight: 0.4\nbogus: 1").is_err());
    }
}
