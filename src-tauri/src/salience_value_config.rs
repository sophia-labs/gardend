use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub(super) struct LocalValueConfig {
    #[serde(default = "default_importance_prompt")]
    pub(super) importance_prompt: String,
    #[serde(default = "default_valence_prompt")]
    pub(super) valence_prompt: String,
    #[serde(default = "default_value_weights")]
    pub(super) weights: BTreeMap<String, f64>,
    #[serde(default = "default_half_life_days")]
    pub(super) temporal_half_life_days: f64,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub(super) struct LocalValueConfigArchive {
    pub(super) archived_at: String,
    pub(super) updated: Vec<String>,
    pub(super) config: LocalValueConfig,
}

fn default_importance_prompt() -> String {
    "Importance measures durable relevance from 0 to 5. Use 0 for active forgetting, 3 for important, and 5 for landmark material.".to_string()
}

fn default_valence_prompt() -> String {
    "Valence measures affective charge from -5 to +5. Positive values mark breakthroughs; negative values mark tensions or problems.".to_string()
}

fn default_half_life_days() -> f64 {
    7.0
}

pub(super) fn default_value_weights() -> BTreeMap<String, f64> {
    let mut weights = BTreeMap::new();
    weights.insert("importance_weight".to_string(), 0.30);
    weights.insert("valence_weight".to_string(), 0.20);
    weights.insert("temporal_weight".to_string(), 0.15);
    weights.insert("block_wires_weight".to_string(), 0.15);
    weights.insert("doc_wires_weight".to_string(), 0.10);
    weights.insert("wire_freshness_weight".to_string(), 0.10);
    weights.insert("importance_ref".to_string(), 10.0);
    weights.insert("valence_ref".to_string(), 10.0);
    weights.insert("block_wires_ref".to_string(), 3.0);
    weights.insert("doc_wires_ref".to_string(), 8.0);
    weights.insert("half_life_days".to_string(), default_half_life_days());
    weights.insert("workspace_depth".to_string(), 2.0);
    weights.insert("workspace_min_score".to_string(), 0.0);
    weights
}

pub(super) fn default_value_config() -> LocalValueConfig {
    LocalValueConfig {
        importance_prompt: default_importance_prompt(),
        valence_prompt: default_valence_prompt(),
        weights: default_value_weights(),
        temporal_half_life_days: default_half_life_days(),
    }
}

pub(super) fn normalize_value_config(config: &mut LocalValueConfig) {
    if config.importance_prompt.trim().is_empty() {
        config.importance_prompt = default_importance_prompt();
    }
    if config.valence_prompt.trim().is_empty() {
        config.valence_prompt = default_valence_prompt();
    }
    let defaults = default_value_weights();
    for (key, value) in defaults {
        config.weights.entry(key).or_insert(value);
    }
    if config.temporal_half_life_days <= 0.0 {
        config.temporal_half_life_days = *config
            .weights
            .get("half_life_days")
            .unwrap_or(&default_half_life_days());
    }
    config
        .weights
        .insert("half_life_days".to_string(), config.temporal_half_life_days);
}

pub(super) fn value_weight(config: &LocalValueConfig, key: &str) -> f64 {
    config
        .weights
        .get(key)
        .copied()
        .or_else(|| default_value_weights().get(key).copied())
        .unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn value_config_normalization_restores_required_defaults() {
        let mut config = LocalValueConfig {
            importance_prompt: " ".to_string(),
            valence_prompt: String::new(),
            weights: BTreeMap::new(),
            temporal_half_life_days: 0.0,
        };

        normalize_value_config(&mut config);

        assert!(!config.importance_prompt.trim().is_empty());
        assert!(!config.valence_prompt.trim().is_empty());
        assert_eq!(config.temporal_half_life_days, 7.0);
        assert_eq!(
            config.weights.get("half_life_days").copied(),
            Some(config.temporal_half_life_days)
        );
        assert!(config.weights.contains_key("importance_weight"));
    }
}
