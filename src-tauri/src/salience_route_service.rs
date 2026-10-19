use crate::app_runtime::AppHandle;
use crate::{
    clock::timestamp,
    graph_service::touch_graph_updated_at,
    paths::existing_graph_dir,
    profile_service::touch_profile_updated_at,
    salience_rdf_materializer::reconcile_value_store,
    salience_score_projection::local_block_value_scores,
    salience_value_store::{
        block_value_key, normalize_value_config, read_value_store, record_has_value_score,
        write_value_store, LocalBlockValueRecord,
    },
};

pub(super) use crate::salience_config_projection::local_value_config_json;
pub(super) use crate::salience_route_inputs::{
    SalienceConfigPatch, SalienceRenewRequest, SalienceUserValuationRequest,
};

pub(super) fn set_local_user_valuation(
    app: AppHandle,
    graph_id: &str,
    input: SalienceUserValuationRequest,
) -> Result<serde_json::Value, String> {
    let valid_importance = [0, 3, 5];
    let valid_valence = [-4, 0, 4];
    if let Some(importance) = input.importance {
        if !valid_importance.contains(&importance) {
            return Err("importance must be one of [0, 3, 5] or null".to_string());
        }
    }
    if let Some(valence) = input.valence {
        if !valid_valence.contains(&valence) {
            return Err("valence must be one of [-4, 0, 4] or null".to_string());
        }
    }

    let graph_dir = existing_graph_dir(&app, graph_id)?;
    let mut store = read_value_store(&graph_dir, graph_id)?;
    let key = block_value_key(&input.document_id, &input.block_id);
    let record = store
        .blocks
        .entry(key)
        .or_insert_with(|| LocalBlockValueRecord {
            document_id: input.document_id.clone(),
            block_id: input.block_id.clone(),
            ..LocalBlockValueRecord::default()
        });
    record.document_id = input.document_id.clone();
    record.block_id = input.block_id.clone();
    apply_user_valuation(record, input.importance, input.valence, &timestamp());
    write_value_store(&graph_dir, &store)?;
    // TODO(MO-delta-routing): route the structured TripleDiff instead of dropping it.
    let _diff = reconcile_value_store(&graph_dir, &store)?;
    touch_graph_updated_at(&graph_dir)?;
    touch_profile_updated_at(&app)?;

    let mut scores = local_block_value_scores(
        &app,
        graph_id,
        Some(&input.document_id),
        Some(&input.block_id),
        None,
        1,
        None,
        None,
    )?;
    if !scores.is_empty() {
        Ok(scores.remove(0))
    } else {
        Ok(serde_json::json!({
            "block_id": input.block_id,
            "blockId": input.block_id,
            "document_id": input.document_id,
            "documentId": input.document_id,
            "cumulative_importance": 0.0,
            "cumulativeImportance": 0.0,
            "cumulative_valence": 0.0,
            "cumulativeValence": 0.0,
            "raw_importance_sum": 0.0,
            "rawImportanceSum": 0.0,
            "raw_valence_sum": 0.0,
            "rawValenceSum": 0.0,
            "importance_count": 0,
            "importanceCount": 0,
            "valence_count": 0,
            "valenceCount": 0,
            "composite_score": 0.0,
            "compositeScore": 0.0,
            "block_wire_count": 0,
            "blockWireCount": 0,
            "doc_wire_count": 0,
            "docWireCount": 0,
            "last_valuated_at": serde_json::Value::Null,
            "lastValuatedAt": serde_json::Value::Null,
            "user_importance": input.importance.map(|value| value as f64),
            "userImportance": input.importance.map(|value| value as f64),
            "user_valence": input.valence.and_then(|value| if value == 0 { None } else { Some(value as f64) }),
            "userValence": input.valence.and_then(|value| if value == 0 { None } else { Some(value as f64) }),
        }))
    }
}

/// The human's rating is one replaceable slot, but setting it is still an act
/// of attention, so an affirming rating renews the block's freshness the way
/// an agent valuation does. Forgetting (importance 0) and clearing do not:
/// a block you just let go of should not come back as fresh.
fn apply_user_valuation(
    record: &mut LocalBlockValueRecord,
    importance: Option<i64>,
    valence: Option<i64>,
    now: &str,
) {
    record.user_importance = importance.map(|value| value as f64);
    record.user_valence =
        valence.and_then(|value| if value == 0 { None } else { Some(value as f64) });
    let affirms = importance.is_some_and(|value| value > 0)
        || (importance != Some(0) && record.user_valence.is_some());
    if affirms {
        record.last_valuated_at = now.to_string();
    }
}

/// Renew a block's freshness without touching any rating: the human says
/// "this still matters" without re-scoring it. Only a block that already
/// carries a value can be renewed, and a block the human rated 0 stays
/// forgotten until it is rated again.
fn renew_user_valuation(record: &mut LocalBlockValueRecord, now: &str) -> Result<(), String> {
    if !record_has_value_score(record) {
        return Err("block has no value to renew; rate it first".to_string());
    }
    if record.user_importance == Some(0.0) {
        return Err("block was rated 0 (let go); rate it again to renew it".to_string());
    }
    record.last_valuated_at = now.to_string();
    Ok(())
}

pub(super) fn renew_local_user_valuation(
    app: AppHandle,
    graph_id: &str,
    input: SalienceRenewRequest,
) -> Result<serde_json::Value, String> {
    let graph_dir = existing_graph_dir(&app, graph_id)?;
    {
        let mut store = read_value_store(&graph_dir, graph_id)?;
        let key = block_value_key(&input.document_id, &input.block_id);
        let record = store
            .blocks
            .get_mut(&key)
            .ok_or_else(|| "block has no value to renew; rate it first".to_string())?;
        renew_user_valuation(record, &timestamp())?;
        write_value_store(&graph_dir, &store)?;
        // TODO(MO-delta-routing): route the structured TripleDiff instead of dropping it.
        let _diff = reconcile_value_store(&graph_dir, &store)?;
        touch_graph_updated_at(&graph_dir)?;
        touch_profile_updated_at(&app)?;
    }
    let mut scores = local_block_value_scores(
        &app,
        graph_id,
        Some(&input.document_id),
        Some(&input.block_id),
        None,
        1,
        None,
        None,
    )?;
    if scores.is_empty() {
        return Err("block has no value to renew; rate it first".to_string());
    }
    Ok(scores.remove(0))
}

pub(super) fn patch_local_salience_config(
    app: AppHandle,
    graph_id: &str,
    patch: SalienceConfigPatch,
) -> Result<serde_json::Value, String> {
    let graph_dir = existing_graph_dir(&app, graph_id)?;
    let mut store = read_value_store(&graph_dir, graph_id)?;
    if let Some(value) = patch.importance_prompt {
        store.config.importance_prompt = value;
    }
    if let Some(value) = patch.valence_prompt {
        store.config.valence_prompt = value;
    }
    for (key, value) in [
        ("importance_weight", patch.importance_weight),
        ("valence_weight", patch.valence_weight),
        ("temporal_weight", patch.temporal_weight),
        ("block_wires_weight", patch.block_wires_weight),
        ("doc_wires_weight", patch.doc_wires_weight),
        ("wire_freshness_weight", patch.wire_freshness_weight),
        ("importance_ref", patch.importance_ref),
        ("valence_ref", patch.valence_ref),
        ("block_wires_ref", patch.block_wires_ref),
        ("doc_wires_ref", patch.doc_wires_ref),
        ("half_life_days", patch.half_life_days),
    ] {
        if let Some(value) = value {
            store.config.weights.insert(key.to_string(), value);
            if key == "half_life_days" {
                store.config.temporal_half_life_days = value;
            }
        }
    }
    normalize_value_config(&mut store.config);
    write_value_store(&graph_dir, &store)?;
    touch_graph_updated_at(&graph_dir)?;
    touch_profile_updated_at(&app)?;
    Ok(local_value_config_json(graph_id, &store))
}

#[cfg(test)]
mod tests {
    use super::*;

    const EARLIER: &str = "2026-09-01T00:00:00Z";
    const NOW: &str = "2026-09-29T18:00:00Z";

    fn record_valued_at(at: &str) -> LocalBlockValueRecord {
        LocalBlockValueRecord {
            document_id: "doc-a".to_string(),
            block_id: "block-a".to_string(),
            raw_importance_sum: 4.0,
            importance_count: 1,
            last_valuated_at: at.to_string(),
            ..LocalBlockValueRecord::default()
        }
    }

    #[test]
    fn affirming_user_rating_renews_freshness() {
        for (importance, valence) in [(Some(3), None), (Some(5), Some(-4)), (None, Some(4))] {
            let mut record = record_valued_at(EARLIER);
            apply_user_valuation(&mut record, importance, valence, NOW);
            assert_eq!(record.last_valuated_at, NOW, "{importance:?}/{valence:?}");
        }
    }

    #[test]
    fn forgetting_or_clearing_does_not_renew_freshness() {
        for (importance, valence) in [
            (Some(0), None),
            (Some(0), Some(4)),
            (None, None),
            (None, Some(0)),
        ] {
            let mut record = record_valued_at(EARLIER);
            apply_user_valuation(&mut record, importance, valence, NOW);
            assert_eq!(
                record.last_valuated_at, EARLIER,
                "{importance:?}/{valence:?}"
            );
        }
    }

    #[test]
    fn user_rating_still_replaces_rather_than_accumulates() {
        let mut record = record_valued_at(EARLIER);
        apply_user_valuation(&mut record, Some(5), Some(4), NOW);
        apply_user_valuation(&mut record, Some(3), None, NOW);
        assert_eq!(record.user_importance, Some(3.0));
        assert_eq!(record.user_valence, None);
        assert_eq!(record.raw_importance_sum, 4.0);
        assert_eq!(record.importance_count, 1);
    }

    #[test]
    fn renew_moves_only_the_clock() {
        let mut record = record_valued_at(EARLIER);
        record.user_importance = Some(5.0);
        record.user_valence = Some(-4.0);
        renew_user_valuation(&mut record, NOW).unwrap();
        assert_eq!(record.last_valuated_at, NOW);
        assert_eq!(record.user_importance, Some(5.0));
        assert_eq!(record.user_valence, Some(-4.0));
        assert_eq!(record.raw_importance_sum, 4.0);
        assert_eq!(record.importance_count, 1);
    }

    #[test]
    fn renew_refuses_unvalued_and_let_go_blocks() {
        let mut unvalued = LocalBlockValueRecord::default();
        assert!(renew_user_valuation(&mut unvalued, NOW).is_err());
        assert!(unvalued.last_valuated_at.is_empty());

        let mut let_go = record_valued_at(EARLIER);
        let_go.user_importance = Some(0.0);
        assert!(renew_user_valuation(&mut let_go, NOW).is_err());
        assert_eq!(let_go.last_valuated_at, EARLIER);
    }
}
