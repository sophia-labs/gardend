use crate::{
    clock::parse_timestamp,
    document_history_store::{
        LocalDocumentHistoryStore, LocalDocumentSnapshotMeta, LocalDocumentSnapshotPayload,
        HISTORY_TIERS, HISTORY_TIER_MAX_SLOTS,
    },
};
use std::path::Path;

pub(super) use crate::document_history_file_store::{
    read_document_history_store, read_document_snapshot_payload, remove_snapshot_payload,
    write_document_history_store,
};

pub(super) fn history_tier_label(tier: &str) -> &'static str {
    match tier {
        "20min" => "20 min",
        "2h" => "2 hr",
        "12h" => "12 hr",
        "daily" => "daily",
        "weekly" => "weekly",
        _ => "",
    }
}

pub(super) fn history_sort_key(snapshot: &LocalDocumentSnapshotMeta) -> u128 {
    parse_timestamp(&snapshot.created_at).unwrap_or(0)
}

fn history_parent_window_key(snapshot: &LocalDocumentSnapshotMeta, from_tier_idx: usize) -> u128 {
    let millis = history_sort_key(snapshot);
    let hour = 60 * 60 * 1000_u128;
    let day = 24 * hour;
    match from_tier_idx {
        0 => millis / (2 * hour),
        1 => millis / (12 * hour),
        2 => millis / day,
        3 => millis / (7 * day),
        _ => millis,
    }
}

pub(super) fn newest_snapshot_payload(
    graph_dir: &Path,
    store: &LocalDocumentHistoryStore,
) -> Result<Option<LocalDocumentSnapshotPayload>, String> {
    // `snapshots` is the durable append order. Do not use millisecond
    // timestamps to choose the predecessor: multiple commits may share one
    // clock value.
    for snapshot in store.snapshots.iter().rev() {
        if let Ok(payload) =
            read_document_snapshot_payload(graph_dir, &store.document_id, &snapshot.snapshot_id)
        {
            return Ok(Some(payload));
        }
    }
    Ok(None)
}

fn evict_history_snapshot_upward(
    store: &mut LocalDocumentHistoryStore,
    snapshot_id: &str,
    from_tier_idx: usize,
    obsolete_payloads: &mut Vec<String>,
) -> usize {
    let next_tier_idx = from_tier_idx + 1;
    if next_tier_idx >= HISTORY_TIERS.len() {
        return 0;
    }
    let next_tier = HISTORY_TIERS[next_tier_idx];
    let Some(victim_pos) = store
        .snapshots
        .iter()
        .position(|snapshot| snapshot.snapshot_id == snapshot_id)
    else {
        return 0;
    };
    let victim = store.snapshots[victim_pos].clone();
    let victim_window = history_parent_window_key(&victim, from_tier_idx);

    if let Some(anchor_pos) = store.snapshots.iter().position(|snapshot| {
        snapshot.snapshot_id != snapshot_id
            && snapshot.tier == next_tier
            && history_parent_window_key(snapshot, from_tier_idx) == victim_window
    }) {
        let victim_count = victim.snapshot_count.max(1);
        store.snapshots[anchor_pos].snapshot_count = store.snapshots[anchor_pos]
            .snapshot_count
            .saturating_add(victim_count);
        store.snapshots.remove(victim_pos);
        obsolete_payloads.push(snapshot_id.to_string());
        return 1;
    }

    let auto_in_next = store
        .snapshots
        .iter()
        .filter(|snapshot| snapshot.tier == next_tier && !snapshot.is_manual)
        .count();
    if next_tier == "weekly" || auto_in_next < HISTORY_TIER_MAX_SLOTS {
        if let Some(snapshot) = store
            .snapshots
            .iter_mut()
            .find(|snapshot| snapshot.snapshot_id == snapshot_id)
        {
            snapshot.tier = next_tier.to_string();
        }
        return 0;
    }

    let Some(oldest_next_id) = store
        .snapshots
        .iter()
        .filter(|snapshot| snapshot.tier == next_tier && !snapshot.is_manual)
        .min_by_key(|snapshot| history_sort_key(snapshot))
        .map(|snapshot| snapshot.snapshot_id.clone())
    else {
        return 0;
    };
    let deleted =
        evict_history_snapshot_upward(store, &oldest_next_id, next_tier_idx, obsolete_payloads);
    if let Some(snapshot) = store
        .snapshots
        .iter_mut()
        .find(|snapshot| snapshot.snapshot_id == snapshot_id)
    {
        snapshot.tier = next_tier.to_string();
    }
    deleted
}

pub(super) fn collapse_document_history(store: &mut LocalDocumentHistoryStore) -> Vec<String> {
    let mut obsolete_payloads = Vec::new();
    for tier_idx in 0..HISTORY_TIERS.len().saturating_sub(1) {
        let tier = HISTORY_TIERS[tier_idx];
        for _ in 0..store.snapshots.len() {
            let auto_count = store
                .snapshots
                .iter()
                .filter(|snapshot| snapshot.tier == tier && !snapshot.is_manual)
                .count();
            if auto_count <= HISTORY_TIER_MAX_SLOTS {
                break;
            }
            let Some(oldest_id) = store
                .snapshots
                .iter()
                .filter(|snapshot| snapshot.tier == tier && !snapshot.is_manual)
                .min_by_key(|snapshot| history_sort_key(snapshot))
                .map(|snapshot| snapshot.snapshot_id.clone())
            else {
                break;
            };
            evict_history_snapshot_upward(store, &oldest_id, tier_idx, &mut obsolete_payloads);
        }
    }
    obsolete_payloads
}
