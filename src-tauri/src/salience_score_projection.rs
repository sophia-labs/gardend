use crate::app_runtime::AppHandle;
use crate::{
    clock::epoch_millis,
    document_projection_service::document_blocks_for_read,
    document_service::{
        read_graph_documents_cold, read_workspace_record, BlockSnapshot, DocumentRecord,
    },
    json_utils::json_string,
    paths::existing_graph_dir,
    salience_score_values::local_unvaluated_wired_block_json,
    salience_value_store::{
        block_value_key, normalize_value_config, read_value_store_for, record_has_value_score,
    },
    salience_wire_context::{
        document_created_age_days, newer_value_timestamp, value_timestamp_age_days,
        value_wire_timestamp,
    },
    wire_projection_service::wire_is_active,
    workspace_entity_projection::workspace_wires,
};
use std::collections::{BTreeMap, BTreeSet};

pub(super) use crate::salience_score_values::local_block_value_json_with_context;

pub(super) fn find_projected_block(
    document: &DocumentRecord,
    block_id: &str,
) -> Option<BlockSnapshot> {
    document_blocks_for_read(document)
        .into_iter()
        .find(|block| block.id == block_id)
}

/// Commons read of block value scores (empty observer) — preserved for call sites
/// that have no witness. Thin wrapper over [`local_block_value_scores_for`].
pub(super) fn local_block_value_scores(
    app: &AppHandle,
    graph_id: &str,
    document_id: Option<&str>,
    block_id: Option<&str>,
    block_id_list: Option<&BTreeSet<String>>,
    limit: usize,
    min_score: Option<f64>,
    valence_filter: Option<&str>,
) -> Result<Vec<serde_json::Value>, String> {
    local_block_value_scores_for(
        app,
        graph_id,
        "",
        document_id,
        block_id,
        block_id_list,
        limit,
        min_score,
        valence_filter,
    )
}

/// Per-OBSERVER read of block value scores — reads THIS witness's value store
/// (`values/{observer}/…`) so a query returns only that witness's valuations, never
/// the global sum. Empty observer ⇒ the commons (byte-identical to today).
#[allow(clippy::too_many_arguments)]
pub(super) fn local_block_value_scores_for(
    app: &AppHandle,
    graph_id: &str,
    observer: &str,
    document_id: Option<&str>,
    block_id: Option<&str>,
    block_id_list: Option<&BTreeSet<String>>,
    limit: usize,
    min_score: Option<f64>,
    valence_filter: Option<&str>,
) -> Result<Vec<serde_json::Value>, String> {
    let graph_dir = existing_graph_dir(app, graph_id)?;
    let mut store = read_value_store_for(&graph_dir, graph_id, observer)?;
    normalize_value_config(&mut store.config);
    let workspace = read_workspace_record(&graph_dir, graph_id)?;
    let wires = workspace_wires(workspace.snapshot.as_ref());
    let documents = read_graph_documents_cold(&graph_dir)?;
    let document_map = documents
        .into_iter()
        .map(|document| (document.document_id.clone(), document))
        .collect::<BTreeMap<_, _>>();
    let now_ms = epoch_millis();
    let mut valued_keys = BTreeSet::new();
    let mut blocks = Vec::new();

    for record in store.blocks.values() {
        let key = block_value_key(&record.document_id, &record.block_id);
        if !record_has_value_score(record) {
            continue;
        }
        valued_keys.insert(key);
        if document_id
            .map(|document_id| record.document_id != document_id)
            .unwrap_or(false)
        {
            continue;
        }
        if block_id
            .map(|block_id| record.block_id != block_id)
            .unwrap_or(false)
        {
            continue;
        }
        if block_id_list
            .map(|block_ids| !block_ids.contains(&record.block_id))
            .unwrap_or(false)
        {
            continue;
        }
        match valence_filter {
            Some("positive") if record.cumulative_valence <= 0.0 => continue,
            Some("negative") if record.cumulative_valence >= 0.0 => continue,
            Some("positive") | Some("negative") => {}
            Some(_) => continue,
            None => {}
        }
        let scores = local_block_value_json_with_context(
            record,
            &store.config,
            &wires,
            &document_map,
            now_ms,
        );
        if min_score
            .map(|min_score| scores.composite_score < min_score)
            .unwrap_or(false)
        {
            continue;
        }
        let value = serde_json::to_value(&scores).expect("LocalBlockValueScores always serializes");
        blocks.push(value);
    }

    if !(block_id.is_some() && document_id.is_some()) {
        let mut block_wire_counts: BTreeMap<String, usize> = BTreeMap::new();
        let mut doc_wire_counts: BTreeMap<String, usize> = BTreeMap::new();
        let mut doc_newest_wire: BTreeMap<String, String> = BTreeMap::new();
        let mut block_to_doc: BTreeMap<String, String> = BTreeMap::new();
        for wire in &wires {
            if !wire_is_active(wire) {
                continue;
            }
            if json_string(wire.get("id"))
                .map(|id| id.ends_with("-inv"))
                .unwrap_or(false)
            {
                continue;
            }
            let created = value_wire_timestamp(wire);
            for doc_key in ["sourceDocumentId", "targetDocumentId"] {
                if let Some(doc_id) = json_string(wire.get(doc_key)) {
                    *doc_wire_counts.entry(doc_id.clone()).or_insert(0) += 1;
                    let newest = doc_newest_wire.remove(&doc_id);
                    if let Some(value) = newer_value_timestamp(newest, created.clone()) {
                        doc_newest_wire.insert(doc_id, value);
                    }
                }
            }
            for (block_key, doc_key) in [
                ("sourceBlockId", "sourceDocumentId"),
                ("targetBlockId", "targetDocumentId"),
            ] {
                let Some(wire_block_id) = json_string(wire.get(block_key)) else {
                    continue;
                };
                let Some(wire_document_id) = json_string(wire.get(doc_key)) else {
                    continue;
                };
                let key = block_value_key(&wire_document_id, &wire_block_id);
                *block_wire_counts.entry(key.clone()).or_insert(0) += 1;
                block_to_doc.insert(key, wire_document_id);
            }
        }

        for (key, block_wire_count) in block_wire_counts {
            if valued_keys.contains(&key) {
                continue;
            }
            let Some(wire_document_id) = block_to_doc.get(&key) else {
                continue;
            };
            let Some((_, wire_block_id)) = key.split_once(':') else {
                continue;
            };
            if document_id
                .map(|document_id| wire_document_id != document_id)
                .unwrap_or(false)
            {
                continue;
            }
            if block_id
                .map(|block_id| wire_block_id != block_id)
                .unwrap_or(false)
            {
                continue;
            }
            if block_id_list
                .map(|block_ids| !block_ids.contains(wire_block_id))
                .unwrap_or(false)
            {
                continue;
            }
            if valence_filter.is_some() {
                continue;
            }
            let doc_wire_count = doc_wire_counts.get(wire_document_id).copied().unwrap_or(0);
            let doc_age_days =
                document_created_age_days(&document_map, wire_document_id, None, now_ms);
            let wire_age_days = value_timestamp_age_days(
                doc_newest_wire.get(wire_document_id).map(String::as_str),
                now_ms,
            );
            let scores = local_unvaluated_wired_block_json(
                wire_document_id,
                wire_block_id,
                &store.config,
                block_wire_count,
                doc_wire_count,
                doc_age_days,
                wire_age_days,
            );
            if min_score
                .map(|min_score| scores.composite_score < min_score)
                .unwrap_or(false)
            {
                continue;
            }
            let value =
                serde_json::to_value(&scores).expect("UnvaluatedWiredBlock always serializes");
            blocks.push(value);
        }
    }

    blocks.sort_by(|left, right| {
        let left_score = left
            .get("composite_score")
            .and_then(serde_json::Value::as_f64)
            .unwrap_or(0.0);
        let right_score = right
            .get("composite_score")
            .and_then(serde_json::Value::as_f64)
            .unwrap_or(0.0);
        right_score
            .partial_cmp(&left_score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    blocks.truncate(limit);
    Ok(blocks)
}
