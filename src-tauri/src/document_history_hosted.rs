use crate::app_runtime::AppHandle;
use crate::{
    document_history_persistence::{history_sort_key, history_tier_label},
    document_history_projection::{snapshot_blocks_html_fragment, snapshot_blocks_markdown},
    document_history_service::{document_history_for_read, document_snapshot_for_read},
    document_history_store::LocalDocumentSnapshotMeta,
    paths::existing_graph_dir,
};
use axum::{
    body::Body,
    http::{header, StatusCode},
    response::Response,
};
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub(super) struct HostedDocumentSnapshotEntry {
    snapshot_id: String,
    graph_id: String,
    doc_id: String,
    is_manual: bool,
    document_revision: Option<u64>,
    tier: String,
    tier_label: String,
    snapshot_count: u64,
    chars_added: i64,
    chars_removed: i64,
    blocks_added: i64,
    blocks_removed: i64,
    blocks_modified: i64,
    created_at: String,
    label: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct HostedDocumentSnapshotListBody {
    snapshots: Vec<HostedDocumentSnapshotEntry>,
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct HostedDocumentSnapshotCountBody {
    count: u64,
}

pub(super) fn document_snapshot_hosted_entry(
    snapshot: &LocalDocumentSnapshotMeta,
) -> HostedDocumentSnapshotEntry {
    HostedDocumentSnapshotEntry {
        snapshot_id: snapshot.snapshot_id.clone(),
        graph_id: snapshot.graph_id.clone(),
        doc_id: snapshot.document_id.clone(),
        is_manual: snapshot.is_manual,
        document_revision: snapshot.document_revision,
        tier: snapshot.tier.clone(),
        tier_label: history_tier_label(&snapshot.tier).to_string(),
        snapshot_count: snapshot.snapshot_count,
        chars_added: snapshot.chars_added,
        chars_removed: snapshot.chars_removed,
        blocks_added: snapshot.blocks_added,
        blocks_removed: snapshot.blocks_removed,
        blocks_modified: snapshot.blocks_modified,
        created_at: snapshot.created_at.clone(),
        label: snapshot.label.clone(),
    }
}

pub(super) fn document_snapshot_hosted_json(
    snapshot: &LocalDocumentSnapshotMeta,
) -> HostedDocumentSnapshotEntry {
    document_snapshot_hosted_entry(snapshot)
}

fn text_response(content: String, content_type: &'static str) -> Result<Response, String> {
    let bytes = content.into_bytes();
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, content_type)
        .header(header::CONTENT_LENGTH, bytes.len().to_string())
        .body(Body::from(bytes))
        .map_err(|error| format!("build text response: {error}"))
}

pub(super) fn hosted_document_snapshot_list_response(
    app: &AppHandle,
    graph_id: &str,
    doc_id: &str,
    limit: usize,
) -> Result<HostedDocumentSnapshotListBody, String> {
    let graph_dir = existing_graph_dir(app, graph_id)?;
    let mut snapshots = document_history_for_read(&graph_dir, graph_id, doc_id)?.snapshots;
    snapshots.sort_by_key(|snapshot| std::cmp::Reverse(history_sort_key(snapshot)));
    snapshots.truncate(limit.clamp(1, 200));
    let rows = snapshots
        .iter()
        .map(document_snapshot_hosted_entry)
        .collect::<Vec<_>>();
    Ok(HostedDocumentSnapshotListBody { snapshots: rows })
}

pub(super) fn hosted_document_snapshot_count_response(
    app: &AppHandle,
    graph_id: &str,
    doc_id: &str,
) -> Result<HostedDocumentSnapshotCountBody, String> {
    let graph_dir = existing_graph_dir(app, graph_id)?;
    let store = document_history_for_read(&graph_dir, graph_id, doc_id)?;
    Ok(HostedDocumentSnapshotCountBody {
        count: store.total_count,
    })
}

pub(super) fn hosted_document_snapshot_text_response(
    app: &AppHandle,
    graph_id: &str,
    doc_id: &str,
    snapshot_id: &str,
) -> Result<Response, String> {
    let graph_dir = existing_graph_dir(app, graph_id)?;
    let payload = document_snapshot_for_read(&graph_dir, graph_id, doc_id, snapshot_id)?;
    text_response(
        snapshot_blocks_markdown(&payload.blocks),
        "text/plain; charset=utf-8",
    )
}

pub(super) fn hosted_document_snapshot_html_response(
    app: &AppHandle,
    graph_id: &str,
    doc_id: &str,
    snapshot_id: &str,
) -> Result<Response, String> {
    let graph_dir = existing_graph_dir(app, graph_id)?;
    let payload = document_snapshot_for_read(&graph_dir, graph_id, doc_id, snapshot_id)?;
    text_response(
        snapshot_blocks_html_fragment(&payload.blocks),
        "text/html; charset=utf-8",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hosted_snapshot_json_uses_hosted_field_names() {
        let snapshot = LocalDocumentSnapshotMeta {
            snapshot_id: "snapshot-a".to_string(),
            graph_id: "graph-a".to_string(),
            document_id: "doc-a".to_string(),
            is_manual: true,
            document_revision: Some(7),
            tier: "daily".to_string(),
            snapshot_count: 3,
            chars_added: 5,
            chars_removed: 1,
            blocks_added: 2,
            blocks_removed: 0,
            blocks_modified: 1,
            created_at: "1000".to_string(),
            label: Some("keeper".to_string()),
        };

        let entry = document_snapshot_hosted_json(&snapshot);
        let value = serde_json::to_value(&entry).expect("entry serializes");
        assert_eq!(
            value.get("snapshot_id"),
            Some(&serde_json::json!("snapshot-a"))
        );
        assert_eq!(value.get("doc_id"), Some(&serde_json::json!("doc-a")));
        assert_eq!(value.get("tier_label"), Some(&serde_json::json!("daily")));
        assert_eq!(value.get("is_manual"), Some(&serde_json::json!(true)));
        assert_eq!(value.get("document_revision"), Some(&serde_json::json!(7)));
        assert_eq!(value.get("snapshot_count"), Some(&serde_json::json!(3)));
    }
}
