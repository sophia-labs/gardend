use crate::{
    clock::parse_timestamp, document_service::DocumentRecord, json_utils::json_string,
    salience_value_store::VALUE_UNKNOWN_AGE_DAYS, wire_projection_service::wire_is_active,
};
use std::collections::BTreeMap;

pub(super) fn newer_value_timestamp(
    current: Option<String>,
    candidate: Option<String>,
) -> Option<String> {
    let Some(candidate) = candidate else {
        return current;
    };
    match current {
        Some(current) => {
            let current_num = parse_timestamp(&current).unwrap_or(0);
            let candidate_num = parse_timestamp(&candidate).unwrap_or(0);
            if candidate_num > current_num {
                Some(candidate)
            } else {
                Some(current)
            }
        }
        None => Some(candidate),
    }
}

pub(super) fn value_wire_timestamp(wire: &serde_json::Value) -> Option<String> {
    json_string(
        wire.get("createdAt")
            .or_else(|| wire.get("created_at"))
            .or_else(|| wire.get("snapshotAt"))
            .or_else(|| wire.get("updatedAt")),
    )
}

pub(super) fn value_wire_stats(
    wires: &[serde_json::Value],
    document_id: &str,
    block_id: &str,
) -> (usize, usize, Option<String>) {
    let mut doc_count = 0usize;
    let mut block_count = 0usize;
    let mut newest_wire = None;
    for wire in wires {
        if !wire_is_active(wire) {
            continue;
        }
        if json_string(wire.get("id"))
            .map(|id| id.ends_with("-inv"))
            .unwrap_or(false)
        {
            continue;
        }
        let source_document_id = json_string(wire.get("sourceDocumentId"));
        let target_document_id = json_string(wire.get("targetDocumentId"));
        let source_block_id = json_string(wire.get("sourceBlockId"));
        let target_block_id = json_string(wire.get("targetBlockId"));
        let document_matches = source_document_id.as_deref() == Some(document_id)
            || target_document_id.as_deref() == Some(document_id);
        if document_matches {
            doc_count += 1;
            newest_wire = newer_value_timestamp(newest_wire, value_wire_timestamp(wire));
        }
        let block_matches = (source_document_id.as_deref() == Some(document_id)
            && source_block_id.as_deref() == Some(block_id))
            || (target_document_id.as_deref() == Some(document_id)
                && target_block_id.as_deref() == Some(block_id));
        if block_matches {
            block_count += 1;
        }
    }
    (block_count, doc_count, newest_wire)
}

pub(super) fn value_timestamp_age_days(value: Option<&str>, now_ms: u128) -> f64 {
    let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) else {
        return VALUE_UNKNOWN_AGE_DAYS;
    };
    let Some(mut timestamp) = parse_timestamp(value) else {
        return VALUE_UNKNOWN_AGE_DAYS;
    };
    if timestamp < 10_000_000_000 {
        timestamp *= 1000;
    }
    if timestamp > now_ms {
        return 0.0;
    }
    (now_ms - timestamp) as f64 / 86_400_000.0
}

pub(super) fn document_created_age_days(
    documents: &BTreeMap<String, DocumentRecord>,
    document_id: &str,
    fallback_timestamp: Option<&str>,
    now_ms: u128,
) -> f64 {
    let timestamp = fallback_timestamp
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            documents
                .get(document_id)
                .map(|document| document.created_at.as_str())
        });
    value_timestamp_age_days(timestamp, now_ms)
}
