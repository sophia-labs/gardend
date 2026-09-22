use serde::Deserialize;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct SalienceUserValuationRequest {
    #[serde(alias = "document_id")]
    pub(super) document_id: String,
    #[serde(alias = "block_id")]
    pub(super) block_id: String,
    pub(super) importance: Option<i64>,
    pub(super) valence: Option<i64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct SalienceConfigPatch {
    #[serde(default, alias = "importance_prompt")]
    pub(super) importance_prompt: Option<String>,
    #[serde(default, alias = "valence_prompt")]
    pub(super) valence_prompt: Option<String>,
    #[serde(default, alias = "importance_weight")]
    pub(super) importance_weight: Option<f64>,
    #[serde(default, alias = "valence_weight")]
    pub(super) valence_weight: Option<f64>,
    #[serde(default, alias = "temporal_weight")]
    pub(super) temporal_weight: Option<f64>,
    #[serde(default, alias = "block_wires_weight")]
    pub(super) block_wires_weight: Option<f64>,
    #[serde(default, alias = "doc_wires_weight")]
    pub(super) doc_wires_weight: Option<f64>,
    #[serde(default, alias = "wire_freshness_weight")]
    pub(super) wire_freshness_weight: Option<f64>,
    #[serde(default, alias = "importance_ref")]
    pub(super) importance_ref: Option<f64>,
    #[serde(default, alias = "valence_ref")]
    pub(super) valence_ref: Option<f64>,
    #[serde(default, alias = "block_wires_ref")]
    pub(super) block_wires_ref: Option<f64>,
    #[serde(default, alias = "doc_wires_ref")]
    pub(super) doc_wires_ref: Option<f64>,
    #[serde(default, alias = "half_life_days")]
    pub(super) half_life_days: Option<f64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn salience_route_inputs_deserialize_hosted_aliases() {
        let valuation: SalienceUserValuationRequest = serde_json::from_value(serde_json::json!({
            "document_id": "doc-a",
            "block_id": "block-a",
            "importance": 5,
            "valence": -4,
        }))
        .unwrap();
        assert_eq!(valuation.document_id, "doc-a");
        assert_eq!(valuation.block_id, "block-a");
        assert_eq!(valuation.importance, Some(5));
        assert_eq!(valuation.valence, Some(-4));

        let patch: SalienceConfigPatch = serde_json::from_value(serde_json::json!({
            "importance_prompt": "important?",
            "valencePrompt": "charged?",
            "half_life_days": 13.0,
        }))
        .unwrap();
        assert_eq!(patch.importance_prompt.as_deref(), Some("important?"));
        assert_eq!(patch.valence_prompt.as_deref(), Some("charged?"));
        assert_eq!(patch.half_life_days, Some(13.0));
    }
}
