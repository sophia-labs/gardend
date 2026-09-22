use crate::{
    salience_value_config::{value_weight, LocalValueConfig},
    salience_value_store::LocalBlockValueRecord,
};
use std::collections::BTreeSet;

const DEFAULT_UNVALUATED_IMPORTANCE: f64 = 1.584_962_500_721_156_3;
pub(super) const VALUE_UNKNOWN_AGE_DAYS: f64 = 9_999.0;

pub(super) fn block_value_key(document_id: &str, block_id: &str) -> String {
    format!("{document_id}:{block_id}")
}

pub(super) fn round4(value: f64) -> f64 {
    (value * 10_000.0).round() / 10_000.0
}

pub(super) fn cumulative_importance(raw_importance: f64) -> f64 {
    if raw_importance > 0.0 {
        raw_importance.log2_1p()
    } else {
        0.0
    }
}

pub(super) fn cumulative_valence(raw_valence: f64) -> f64 {
    if raw_valence == 0.0 {
        0.0
    } else {
        raw_valence.signum() * raw_valence.abs().log2_1p()
    }
}

trait Log2OnePlus {
    fn log2_1p(self) -> f64;
}

impl Log2OnePlus for f64 {
    fn log2_1p(self) -> f64 {
        (1.0 + self).log2()
    }
}

pub(super) fn record_has_value_score(record: &LocalBlockValueRecord) -> bool {
    record.importance_count > 0
        || record.valence_count > 0
        || record.raw_importance_sum != 0.0
        || record.raw_valence_sum != 0.0
        || record.user_importance.is_some()
        || record.user_valence.is_some()
}

pub(super) fn local_value_composite_score(
    importance: f64,
    valence: f64,
    user_importance: Option<f64>,
    doc_age_days: f64,
    block_wire_count: usize,
    doc_wire_count: usize,
    wire_age_days: f64,
    config: &LocalValueConfig,
) -> f64 {
    let effective_importance = if user_importance == Some(0.0) {
        0.0
    } else if importance > 0.0 {
        importance
    } else {
        DEFAULT_UNVALUATED_IMPORTANCE
    };
    let importance_ref = value_weight(config, "importance_ref").max(0.000_001);
    let valence_ref = value_weight(config, "valence_ref").max(0.000_001);
    let block_wires_ref = value_weight(config, "block_wires_ref").max(0.000_001);
    let doc_wires_ref = value_weight(config, "doc_wires_ref").max(0.000_001);
    let half_life_days = value_weight(config, "half_life_days").max(0.000_001);
    let score = value_weight(config, "importance_weight")
        * (effective_importance / importance_ref).tanh()
        + value_weight(config, "valence_weight") * (valence.abs() / valence_ref).tanh()
        + value_weight(config, "temporal_weight") * (-doc_age_days / half_life_days).exp()
        + value_weight(config, "block_wires_weight")
            * (block_wire_count as f64 / block_wires_ref).tanh()
        + value_weight(config, "doc_wires_weight") * (doc_wire_count as f64 / doc_wires_ref).tanh()
        + value_weight(config, "wire_freshness_weight") * (-wire_age_days / half_life_days).exp();
    round4(score)
}

pub(super) fn normalize_value_tags(tags: Vec<String>) -> Vec<String> {
    let mut seen = BTreeSet::new();
    tags.into_iter()
        .map(|tag| tag.trim().trim_start_matches('#').to_ascii_lowercase())
        .filter(|tag| !tag.is_empty())
        .filter(|tag| seen.insert(tag.clone()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn value_tags_are_normalized_and_deduplicated() {
        assert_eq!(
            normalize_value_tags(vec![
                " #Decision ".to_string(),
                "decision".to_string(),
                "#praxis".to_string(),
                "".to_string(),
                " PRAXIS ".to_string(),
            ]),
            vec!["decision".to_string(), "praxis".to_string()]
        );
    }

    #[test]
    fn cumulative_value_scores_use_log_curve() {
        assert_eq!(cumulative_importance(0.0), 0.0);
        assert_eq!(cumulative_importance(3.0), 2.0);
        assert_eq!(cumulative_valence(3.0), 2.0);
        assert_eq!(cumulative_valence(-3.0), -2.0);
    }
}
