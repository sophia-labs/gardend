use crate::{
    salience_value_config::default_value_config,
    storage::{create_dir_all, read_json, write_json},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

pub(super) use crate::salience_value_config::{
    default_value_weights, normalize_value_config, LocalValueConfig, LocalValueConfigArchive,
};
pub(super) use crate::salience_value_scoring::{
    block_value_key, cumulative_importance, cumulative_valence, local_value_composite_score,
    normalize_value_tags, record_has_value_score, round4, VALUE_UNKNOWN_AGE_DAYS,
};

const VALUE_STORE_SCHEMA_VERSION: u32 = 1;
const VALUE_STORE_FILE: &str = "block-values.json";

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub(super) struct LocalValueStore {
    pub(super) schema_version: u32,
    pub(super) graph_id: String,
    /// The OBSERVER (witness) these valuations belong to — the per-observer salience
    /// fix. When non-empty, the store file (`values/{observer}/block-values.json`) AND
    /// the `:projection:salience:agent:{observer}` named graph fork together, so two
    /// witnesses' valuations of the same block never collapse into one global
    /// `LocalBlockValueRecord` sum. Empty ⇒ the shared commons (today's behavior,
    /// byte-for-byte: `values/block-values.json` + the un-segmented graph).
    #[serde(default)]
    pub(super) observer: String,
    pub(super) config: LocalValueConfig,
    #[serde(default)]
    pub(super) config_history: Vec<LocalValueConfigArchive>,
    pub(super) blocks: BTreeMap<String, LocalBlockValueRecord>,
}

#[derive(Debug, Serialize, Deserialize, Clone, Default)]
#[serde(rename_all = "camelCase")]
pub(super) struct LocalBlockValueRecord {
    pub(super) document_id: String,
    pub(super) block_id: String,
    #[serde(default)]
    pub(super) raw_importance_sum: f64,
    #[serde(default)]
    pub(super) raw_valence_sum: f64,
    pub(super) cumulative_importance: f64,
    pub(super) cumulative_valence: f64,
    #[serde(default)]
    pub(super) importance_count: usize,
    #[serde(default)]
    pub(super) valence_count: usize,
    #[serde(default)]
    pub(super) valuation_count: usize,
    #[serde(default)]
    pub(super) tags: Vec<String>,
    #[serde(default)]
    pub(super) last_valuated_at: String,
    #[serde(default)]
    pub(super) user_importance: Option<f64>,
    #[serde(default)]
    pub(super) user_valence: Option<f64>,
}

/// The values dir for a witness — `values/` for the shared commons (empty observer,
/// byte-identical path), `values/{observer}/` for a per-agent value store. The
/// observer is sanitized to ONE path segment (the same rule the graph IRI uses), so a
/// full-IRI observer cannot escape the values dir. Mirrors `song_store_dir`.
fn value_store_dir(graph_dir: &Path, observer: &str) -> PathBuf {
    let base = graph_dir.join("values");
    match crate::rdf_authority::observer_segment(observer) {
        Some(seg) => base.join(seg),
        None => base,
    }
}

fn value_store_path(graph_dir: &Path, observer: &str) -> PathBuf {
    value_store_dir(graph_dir, observer).join(VALUE_STORE_FILE)
}

fn default_value_store(graph_id: &str, observer: &str) -> LocalValueStore {
    LocalValueStore {
        schema_version: VALUE_STORE_SCHEMA_VERSION,
        graph_id: graph_id.to_string(),
        observer: observer.to_string(),
        config: default_value_config(),
        config_history: Vec::new(),
        blocks: BTreeMap::new(),
    }
}

/// Read the SHARED commons value store (empty observer) — preserved for call sites
/// with no observer. Thin wrapper over [`read_value_store_for`]; behavior is
/// byte-identical to the historical singleton.
pub(super) fn read_value_store(
    graph_dir: &Path,
    graph_id: &str,
) -> Result<LocalValueStore, String> {
    read_value_store_for(graph_dir, graph_id, "")
}

/// Read the PER-OBSERVER value store from `values/{observer}/block-values.json` (or
/// the shared `values/block-values.json` for an empty observer). The returned store
/// carries the observer so [`write_value_store`] / the reconcile path fork the file +
/// the `:projection:salience:agent:{observer}` named graph consistently.
pub(super) fn read_value_store_for(
    graph_dir: &Path,
    graph_id: &str,
    observer: &str,
) -> Result<LocalValueStore, String> {
    let path = value_store_path(graph_dir, observer);
    if !path.is_file() {
        return Ok(default_value_store(graph_id, observer));
    }
    let mut store = read_json::<LocalValueStore>(&path)?;
    store.graph_id = graph_id.to_string();
    store.observer = observer.to_string();
    store.schema_version = VALUE_STORE_SCHEMA_VERSION;
    normalize_value_config(&mut store.config);
    for record in store.blocks.values_mut() {
        if record.raw_importance_sum == 0.0 && record.cumulative_importance > 0.0 {
            record.raw_importance_sum = (2.0_f64).powf(record.cumulative_importance) - 1.0;
        }
        if record.raw_valence_sum == 0.0 && record.cumulative_valence != 0.0 {
            let sign = if record.cumulative_valence >= 0.0 {
                1.0
            } else {
                -1.0
            };
            record.raw_valence_sum = sign * ((2.0_f64).powf(record.cumulative_valence.abs()) - 1.0);
        }
        if record.importance_count == 0 && record.cumulative_importance > 0.0 {
            record.importance_count = record.valuation_count.max(1);
        }
        if record.valence_count == 0 && record.cumulative_valence != 0.0 {
            record.valence_count = record.valuation_count.max(1);
        }
    }
    Ok(store)
}

pub(super) fn write_value_store(graph_dir: &Path, store: &LocalValueStore) -> Result<(), String> {
    create_dir_all(&value_store_dir(graph_dir, &store.observer))?;
    write_json(&value_store_path(graph_dir, &store.observer), store).map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use uuid::Uuid;

    #[test]
    fn read_value_store_backfills_legacy_raw_sums_and_counts() {
        let graph_dir = std::env::temp_dir().join(format!("sophia-value-store-{}", Uuid::new_v4()));
        let values_dir = graph_dir.join("values");
        fs::create_dir_all(&values_dir).unwrap();
        fs::write(
            values_dir.join(VALUE_STORE_FILE),
            serde_json::to_vec_pretty(&serde_json::json!({
                "schemaVersion": 0,
                "graphId": "legacy-graph",
                "config": {},
                "blocks": {
                    "doc-a:block-a": {
                        "documentId": "doc-a",
                        "blockId": "block-a",
                        "cumulativeImportance": 2.0,
                        "cumulativeValence": -1.0,
                        "valuationCount": 3
                    }
                }
            }))
            .unwrap(),
        )
        .unwrap();

        let store = read_value_store(&graph_dir, "current-graph").unwrap();
        let record = store.blocks.get("doc-a:block-a").unwrap();
        assert_eq!(store.graph_id, "current-graph");
        assert_eq!(store.schema_version, VALUE_STORE_SCHEMA_VERSION);
        assert_eq!(record.raw_importance_sum, 3.0);
        assert_eq!(record.raw_valence_sum, -1.0);
        assert_eq!(record.importance_count, 3);
        assert_eq!(record.valence_count, 3);

        fs::remove_dir_all(graph_dir).unwrap();
    }
}
