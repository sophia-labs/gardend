use std::path::{Path, PathBuf};

pub(crate) const RESTORE_POINTS_DIR: &str = "restore-points";
pub(crate) const RESTORE_POINT_INDEX_FILE: &str = "index.json";
pub(crate) const RESTORE_POINT_MANIFEST_FILE: &str = "manifest.json";
pub(crate) const RESTORE_POINT_WORKSPACE_BYTES_FILE: &str = "workspace.bin";
pub(crate) const RESTORE_POINT_WORKSPACE_SNAPSHOT_FILE: &str = "workspace.json";

pub(crate) fn restore_points_dir(graph_dir: &Path) -> PathBuf {
    graph_dir.join(RESTORE_POINTS_DIR)
}

pub(crate) fn restore_point_index_path(graph_dir: &Path) -> PathBuf {
    restore_points_dir(graph_dir).join(RESTORE_POINT_INDEX_FILE)
}

pub(crate) fn restore_point_dir(graph_dir: &Path, restore_point_id: &str) -> PathBuf {
    restore_points_dir(graph_dir).join(restore_point_id)
}

pub(crate) fn restore_point_manifest_path(graph_dir: &Path, restore_point_id: &str) -> PathBuf {
    restore_point_dir(graph_dir, restore_point_id).join(RESTORE_POINT_MANIFEST_FILE)
}

pub(crate) fn restore_point_workspace_bytes_path(
    graph_dir: &Path,
    restore_point_id: &str,
) -> PathBuf {
    restore_point_dir(graph_dir, restore_point_id).join(RESTORE_POINT_WORKSPACE_BYTES_FILE)
}

pub(crate) fn restore_point_workspace_snapshot_path(
    graph_dir: &Path,
    restore_point_id: &str,
) -> PathBuf {
    restore_point_dir(graph_dir, restore_point_id).join(RESTORE_POINT_WORKSPACE_SNAPSHOT_FILE)
}

pub(crate) fn restore_point_documents_dir(graph_dir: &Path, restore_point_id: &str) -> PathBuf {
    restore_point_dir(graph_dir, restore_point_id).join("documents")
}

pub(crate) fn restore_point_document_bytes_path(
    graph_dir: &Path,
    restore_point_id: &str,
    document_id: &str,
) -> PathBuf {
    restore_point_documents_dir(graph_dir, restore_point_id).join(format!("{document_id}.bin"))
}

/// Relative path stored inside the manifest for a document's Y.Doc bytes.
/// Resolved against `restore_point_dir(graph_dir, restore_point_id)` at read time.
pub(crate) fn document_bytes_relative_path(document_id: &str) -> String {
    format!("documents/{document_id}.bin")
}
