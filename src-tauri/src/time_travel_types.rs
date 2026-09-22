use serde::{Deserialize, Serialize};

/// v1 = workspace bundle + per-doc snapshot ID refs only (read-only — cannot be restored)
/// v2 = adds per-doc Y.Doc state bytes inside the bundle, enabling deterministic restore
pub(crate) const RESTORE_POINT_MANIFEST_SCHEMA_VERSION: u32 = 2;
pub(crate) const RESTORE_POINT_MIN_RESTORABLE_SCHEMA_VERSION: u32 = 2;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct RestorePointManifest {
    pub schema_version: u32,
    pub restore_point_id: String,
    pub graph_id: String,
    pub created_at: i64,
    pub trigger: RestorePointTrigger,
    pub label: Option<String>,
    pub content_hash_sha256: String,
    pub workspace: WorkspaceSnapshotRef,
    pub documents: Vec<DocumentSnapshotRef>,
    pub metadata: RestorePointMetadata,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RestorePointTrigger {
    Manual,
    Interval,
    Checkpoint,
}

impl RestorePointTrigger {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            RestorePointTrigger::Manual => "manual",
            RestorePointTrigger::Interval => "interval",
            RestorePointTrigger::Checkpoint => "checkpoint",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct WorkspaceSnapshotRef {
    pub bytes_path: String,
    pub snapshot_path: String,
    pub size_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct DocumentSnapshotRef {
    pub document_id: String,
    pub title: String,
    pub snapshot_id: String,
    pub size_bytes: u64,
    pub block_count: u64,
    pub char_count: u64,
    /// Path (relative to the restore-point dir) to the document's Y.Doc state
    /// bytes captured at restore-point creation time. Required for v2+
    /// manifests; absent on legacy v1 manifests (which cannot be restored).
    #[serde(default)]
    pub ydoc_bytes_path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(crate) struct RestorePointMetadata {
    pub folder_count: u64,
    pub document_count: u64,
    pub artifact_count: u64,
    pub wire_count: u64,
    pub workspace_size_bytes: u64,
    pub total_document_size_bytes: u64,
}

/// Compact entry persisted in the per-graph index for fast list pagination.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct RestorePointIndexEntry {
    pub restore_point_id: String,
    pub created_at: i64,
    pub trigger: RestorePointTrigger,
    pub label: Option<String>,
    pub content_hash_sha256: String,
    pub size_bytes: u64,
    pub document_count: u64,
    pub folder_count: u64,
    pub artifact_count: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(crate) struct RestorePointIndex {
    pub schema_version: u32,
    pub graph_id: String,
    pub entries: Vec<RestorePointIndexEntry>,
}
