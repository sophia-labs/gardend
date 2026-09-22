use crate::salience_value_store::LocalValueStore;

pub(super) fn local_value_config_json(
    graph_id: &str,
    store: &LocalValueStore,
) -> serde_json::Value {
    serde_json::json!({
        "graph_id": graph_id,
        "graphId": graph_id,
        "importance_prompt": store.config.importance_prompt.clone(),
        "importancePrompt": store.config.importance_prompt.clone(),
        "valence_prompt": store.config.valence_prompt.clone(),
        "valencePrompt": store.config.valence_prompt.clone(),
        "weights": store.config.weights.clone(),
        "temporal_half_life_days": store.config.temporal_half_life_days,
        "temporalHalfLifeDays": store.config.temporal_half_life_days,
        "history_count": store.config_history.len(),
        "historyCount": store.config_history.len(),
        "source": "local-value-store",
    })
}
