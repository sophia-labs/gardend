use crate::app_runtime::AppHandle;
use crate::{
    graph_service::touch_graph_updated_at,
    paths::existing_graph_dir,
    profile_service::touch_profile_updated_at,
    salience_rdf_materializer::reconcile_value_store,
    salience_score_projection::local_block_value_scores,
    salience_value_store::{
        block_value_key, normalize_value_config, read_value_store, write_value_store,
        LocalBlockValueRecord,
    },
};

pub(super) use crate::salience_config_projection::local_value_config_json;
pub(super) use crate::salience_route_inputs::{SalienceConfigPatch, SalienceUserValuationRequest};

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
    record.user_importance = input.importance.map(|value| value as f64);
    record.user_valence =
        input
            .valence
            .and_then(|value| if value == 0 { None } else { Some(value as f64) });
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
