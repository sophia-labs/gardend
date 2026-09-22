use crate::app_runtime::AppHandle;
use crate::{
    clock::{epoch_millis, parse_timestamp, timestamp},
    document_service::{read_graph_documents_cold, read_workspace_record},
    graph_service::touch_graph_updated_at,
    mcp_utils::{mcp_arg_string, mcp_graph_id_or_default},
    paths::existing_graph_dir,
    profile_service::touch_profile_updated_at,
    salience_mcp_inputs::mcp_value_inputs,
    salience_rdf_materializer::reconcile_value_store,
    salience_score_projection::local_block_value_json_with_context,
    salience_value_store::{
        block_value_key, cumulative_importance, cumulative_valence, normalize_value_tags,
        read_value_store_for, write_value_store, LocalBlockValueRecord,
    },
    workspace_entity_projection::workspace_wires,
};
use std::collections::BTreeMap;

/// One stable valuation event from the source-sync ledger.  The public MCP
/// `value` face is an incremental convenience; this event form is the
/// replayable authority used by offline clients and projection rebuilds.
#[derive(Debug, Clone)]
pub(crate) struct SourceValuationEvent {
    pub(crate) event_id: String,
    pub(crate) observer: String,
    pub(crate) document_id: String,
    pub(crate) block_id: String,
    pub(crate) importance: Option<i64>,
    pub(crate) valence: Option<i64>,
    pub(crate) tags: Vec<String>,
    pub(crate) at_ms: i64,
}

/// Deterministically fold a set of stable valuation events and replace the
/// disposable per-observer value stores/projections. Sorting by logical time
/// and event identity makes replay independent of network delivery order; a
/// duplicate event is removed by the source ledger before this function.
pub(crate) fn rebuild_value_store_from_source_events(
    app: &AppHandle,
    graph_id: &str,
    events: &[SourceValuationEvent],
    clear_existing: bool,
) -> Result<serde_json::Value, String> {
    let graph_dir = existing_graph_dir(app, graph_id)?;
    let mut by_observer: BTreeMap<String, Vec<&SourceValuationEvent>> = BTreeMap::new();
    for event in events {
        by_observer
            .entry(event.observer.clone())
            .or_default()
            .push(event);
    }

    let mut observer_reports = Vec::new();
    for (observer, mut observer_events) in by_observer {
        observer_events.sort_by(|left, right| {
            (left.at_ms, left.event_id.as_str()).cmp(&(right.at_ms, right.event_id.as_str()))
        });
        let mut store = read_value_store_for(&graph_dir, graph_id, &observer)?;
        if clear_existing {
            store.blocks.clear();
        }
        for event in observer_events {
            let key = block_value_key(&event.document_id, &event.block_id);
            let record = store
                .blocks
                .entry(key)
                .or_insert_with(|| LocalBlockValueRecord {
                    document_id: event.document_id.clone(),
                    block_id: event.block_id.clone(),
                    ..LocalBlockValueRecord::default()
                });
            if let Some(importance) = event.importance {
                let importance = importance as f64;
                if importance == 0.0 && record.raw_importance_sum > 0.0 {
                    record.raw_importance_sum *= 0.2;
                } else {
                    record.raw_importance_sum += importance;
                }
                record.importance_count += 1;
            }
            if let Some(valence) = event.valence {
                record.raw_valence_sum += valence as f64;
                record.valence_count += 1;
            }
            if !event.tags.is_empty() {
                let mut tags = record.tags.clone();
                tags.extend(event.tags.clone());
                record.tags = normalize_value_tags(tags);
            }
            record.cumulative_importance = cumulative_importance(record.raw_importance_sum);
            record.cumulative_valence = cumulative_valence(record.raw_valence_sum);
            record.valuation_count = record.importance_count + record.valence_count;
            if event.importance.is_some() || event.valence.is_some() {
                record.last_valuated_at = crate::emporium::terms::iso_from_ms(event.at_ms);
            }
        }
        write_value_store(&graph_dir, &store)?;
        let diff = reconcile_value_store(&graph_dir, &store)?;
        observer_reports.push(serde_json::json!({
            "observer": &observer,
            "events": events.iter().filter(|event| event.observer == observer).count(),
            "blocks": store.blocks.len(),
            "rdfOperations": diff.op_count(),
        }));
    }
    // This function is a source replay/rebuild, not a new authoring event.
    // Touching graph/profile clocks here made the projection depend on when a
    // repair happened and therefore prevented byte-set-identical replay.
    // Incremental public `value` writes retain their normal clock touches.
    Ok(serde_json::json!({
        "events": events.len(),
        "observers": observer_reports,
    }))
}

pub(super) fn mcp_local_value(
    app: AppHandle,
    arguments: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let graph_id = mcp_graph_id_or_default(&app, arguments)?;
    let graph_dir = existing_graph_dir(&app, &graph_id)?;
    // Per-observer salience: each witness values into its OWN store + named graph, so
    // two witnesses' valuations of the same block never sum. Empty/absent ⇒ the shared
    // commons (today's behavior, byte-identical).
    let observer =
        mcp_arg_string(arguments, &["observer_agent_id", "observerAgentId"]).unwrap_or_default();
    let (single, entries) = mcp_value_inputs(arguments)?;
    let workspace = read_workspace_record(&graph_dir, &graph_id)?;
    let wires = workspace_wires(workspace.snapshot.as_ref());
    let documents = read_graph_documents_cold(&graph_dir)?
        .into_iter()
        .map(|document| (document.document_id.clone(), document))
        .collect::<BTreeMap<_, _>>();
    let mut store = read_value_store_for(&graph_dir, &graph_id, &observer)?;
    let now = timestamp();
    let now_ms = parse_timestamp(&now).unwrap_or_else(epoch_millis);
    let mut results = Vec::new();
    let mut errors = Vec::new();
    let mut tags_applied = 0usize;

    for entry in entries {
        if entry.document_id.is_empty() || entry.block_id.is_empty() {
            errors.push(serde_json::json!({
                "index": entry.input_index,
                "error": "document_id and block_id are required",
            }));
            continue;
        }
        if entry.importance.is_none() && entry.valence.is_none() && entry.tags.is_empty() {
            errors.push(serde_json::json!({
                "index": entry.input_index,
                "error": "At least one of importance, valence, or tags required",
            }));
            continue;
        }
        if let Some(importance) = entry.importance {
            if !(0..=5).contains(&importance) {
                errors.push(serde_json::json!({
                    "index": entry.input_index,
                    "error": "importance must be between 0 and 5",
                }));
                continue;
            }
        }
        if let Some(valence) = entry.valence {
            if !(-5..=5).contains(&valence) {
                errors.push(serde_json::json!({
                    "index": entry.input_index,
                    "error": "valence must be between -5 and +5",
                }));
                continue;
            }
        }

        let key = block_value_key(&entry.document_id, &entry.block_id);
        let record = store
            .blocks
            .entry(key)
            .or_insert_with(|| LocalBlockValueRecord {
                document_id: entry.document_id.clone(),
                block_id: entry.block_id.clone(),
                ..LocalBlockValueRecord::default()
            });
        record.document_id = entry.document_id.clone();
        record.block_id = entry.block_id.clone();
        let mut updated_score = false;
        if let Some(importance) = entry.importance {
            let importance = importance as f64;
            if importance == 0.0 && record.raw_importance_sum > 0.0 {
                record.raw_importance_sum *= 0.2;
            } else {
                record.raw_importance_sum += importance;
                if importance > 0.0 {
                    updated_score = true;
                }
            }
            record.importance_count += 1;
        }
        if let Some(valence) = entry.valence {
            record.raw_valence_sum += valence as f64;
            record.valence_count += 1;
            updated_score = true;
        }
        record.cumulative_importance = cumulative_importance(record.raw_importance_sum);
        record.cumulative_valence = cumulative_valence(record.raw_valence_sum);
        record.valuation_count = record.importance_count + record.valence_count;
        if !entry.tags.is_empty() {
            let mut tags = record.tags.clone();
            tags.extend(entry.tags);
            record.tags = normalize_value_tags(tags);
            tags_applied += 1;
        }
        if updated_score {
            record.last_valuated_at = now.clone();
        }
        if entry.importance.is_some() || entry.valence.is_some() {
            let scores = local_block_value_json_with_context(
                record,
                &store.config,
                &wires,
                &documents,
                now_ms,
            );
            results.push(
                serde_json::to_value(&scores).expect("LocalBlockValueScores always serializes"),
            );
        }
    }

    write_value_store(&graph_dir, &store)?;
    // TODO(MO-delta-routing): route the structured TripleDiff instead of dropping it.
    let _diff = reconcile_value_store(&graph_dir, &store)?;
    touch_graph_updated_at(&graph_dir)?;
    touch_profile_updated_at(&app)?;

    if single {
        if let Some(error) = errors.first() {
            return Err(error
                .get("error")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("valuation failed")
                .to_string());
        }
        if !results.is_empty() {
            return Ok(results.remove(0));
        }
        if tags_applied > 0 {
            return Ok(serde_json::json!({
                "tags_applied": tags_applied,
                "tagsApplied": tags_applied,
            }));
        }
        Ok(serde_json::json!({}))
    } else {
        let mut output = serde_json::json!({
            "results": results,
            "updated_count": results.len(),
            "updatedCount": results.len(),
        });
        if !errors.is_empty() {
            output["errors"] = serde_json::Value::Array(errors.clone());
            output["error_count"] = serde_json::json!(errors.len());
            output["errorCount"] = serde_json::json!(errors.len());
        }
        if tags_applied > 0 {
            output["tags_applied"] = serde_json::json!(tags_applied);
            output["tagsApplied"] = serde_json::json!(tags_applied);
        }
        Ok(output)
    }
}
