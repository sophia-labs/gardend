use crate::{document_service::BlockSnapshot, paths::documents_dir};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub(crate) const HISTORY_STORE_SCHEMA_VERSION: u32 = 3;
pub(crate) const HISTORY_STORE_FILE: &str = "history.json";
pub(crate) const DOCUMENT_TAIL_COMMIT_FILE: &str = "tail-commit.json";
// Version 2 completion includes the derived PDF-source face.
pub(crate) const DOCUMENT_TAIL_COMMIT_SCHEMA_VERSION: u32 = 2;
pub(crate) const HISTORY_TIER_MAX_SLOTS: usize = 4;
pub(crate) const HISTORY_TIERS: [&str; 5] = ["20min", "2h", "12h", "daily", "weekly"];

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LocalDocumentHistoryStore {
    pub(crate) schema_version: u32,
    pub(crate) graph_id: String,
    pub(crate) document_id: String,
    #[serde(default)]
    pub(crate) total_count: u64,
    /// Highest document revision whose automatic snapshot was durably added to
    /// this store. This survives tier collapse and makes the history tail
    /// idempotent when document.json was written before a crash or later tail
    /// failure. Manual snapshots deliberately do not advance it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) latest_automatic_revision: Option<u64>,
    /// Snapshot referenced by `latest_automatic_revision`. Schema-v2 stores do
    /// not have this field; migration may adopt their newest automatic entry
    /// only after its payload has been decoded and compared with document.json.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) latest_automatic_snapshot_id: Option<String>,
    #[serde(default)]
    pub(crate) snapshots: Vec<LocalDocumentSnapshotMeta>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LocalDocumentSnapshotMeta {
    pub(crate) snapshot_id: String,
    pub(crate) graph_id: String,
    pub(crate) document_id: String,
    #[serde(default)]
    pub(crate) is_manual: bool,
    /// Exact durable document revision captured by this snapshot. `None` is
    /// retained only for pre-v3 history entries whose revision is unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) document_revision: Option<u64>,
    pub(crate) tier: String,
    #[serde(default = "default_snapshot_count")]
    pub(crate) snapshot_count: u64,
    #[serde(default)]
    pub(crate) chars_added: i64,
    #[serde(default)]
    pub(crate) chars_removed: i64,
    #[serde(default)]
    pub(crate) blocks_added: i64,
    #[serde(default)]
    pub(crate) blocks_removed: i64,
    #[serde(default)]
    pub(crate) blocks_modified: i64,
    pub(crate) created_at: String,
    #[serde(default)]
    pub(crate) label: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LocalDocumentSnapshotPayload {
    pub(crate) snapshot_id: String,
    pub(crate) graph_id: String,
    pub(crate) document_id: String,
    pub(crate) title: String,
    pub(crate) created_at: String,
    pub(crate) blocks: Vec<BlockSnapshot>,
    #[serde(default)]
    pub(crate) tiptap_xml: String,
}

/// Final commit record for the document persistence tail. This is written only
/// after RDF reconciliation, automatic-history payload + metadata, and the
/// graph content-revision touch have all committed. A matching marker is still
/// validated against its referenced history payload before a replay can skip.
#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LocalDocumentTailCommit {
    pub(crate) schema_version: u32,
    pub(crate) graph_id: String,
    pub(crate) document_id: String,
    pub(crate) document_revision: u64,
    pub(crate) snapshot_id: String,
    pub(crate) graph_content_revision: String,
}

fn default_snapshot_count() -> u64 {
    1
}

pub(crate) fn document_history_dir(graph_dir: &Path, document_id: &str) -> PathBuf {
    documents_dir(graph_dir).join(document_id).join("history")
}

pub(crate) fn document_history_store_path(graph_dir: &Path, document_id: &str) -> PathBuf {
    document_history_dir(graph_dir, document_id).join(HISTORY_STORE_FILE)
}

pub(crate) fn document_tail_commit_path(graph_dir: &Path, document_id: &str) -> PathBuf {
    document_history_dir(graph_dir, document_id).join(DOCUMENT_TAIL_COMMIT_FILE)
}

pub(crate) fn document_history_snapshots_dir(graph_dir: &Path, document_id: &str) -> PathBuf {
    document_history_dir(graph_dir, document_id).join("snapshots")
}

pub(crate) fn document_snapshot_payload_path(
    graph_dir: &Path,
    document_id: &str,
    snapshot_id: &str,
) -> PathBuf {
    document_history_snapshots_dir(graph_dir, document_id).join(format!("{snapshot_id}.json"))
}
