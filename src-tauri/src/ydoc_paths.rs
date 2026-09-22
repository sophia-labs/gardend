use crate::runtime_config::{WORKSPACE_SNAPSHOT_FILE, YDOC_STATE_FILE};
use std::path::{Path, PathBuf};

pub(crate) fn document_ydoc_dir(graph_dir: &Path, document_id: &str) -> PathBuf {
    graph_dir.join("ydocs/documents").join(document_id)
}

pub(crate) fn document_ydoc_state_path(graph_dir: &Path, document_id: &str) -> PathBuf {
    document_ydoc_dir(graph_dir, document_id).join(YDOC_STATE_FILE)
}

/// Resolve a sidecar path from an externally-derived document id. Keep the raw
/// helper for ids already loaded from validated canonical records; room roots
/// must use this checked boundary before they read or create files.
pub(crate) fn checked_document_ydoc_state_path(
    graph_dir: &Path,
    document_id: &str,
) -> Result<PathBuf, String> {
    crate::ids::validate_local_id(document_id, "document_id")?;
    Ok(document_ydoc_state_path(graph_dir, document_id))
}

pub(crate) fn workspace_ydoc_dir(graph_dir: &Path) -> PathBuf {
    graph_dir.join("ydocs/workspace")
}

pub(crate) fn workspace_ydoc_state_path(graph_dir: &Path) -> PathBuf {
    workspace_ydoc_dir(graph_dir).join(YDOC_STATE_FILE)
}

pub(crate) fn workspace_snapshot_path(graph_dir: &Path) -> PathBuf {
    workspace_ydoc_dir(graph_dir).join(WORKSPACE_SNAPSHOT_FILE)
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    #[test]
    fn checked_document_sidecar_rejects_traversal_and_slashes() {
        let graph_dir =
            std::env::temp_dir().join(format!("garden-ydoc-path-safety-{}", Uuid::new_v4()));
        for invalid in ["..", "../escape", "nested/escape", r"nested\escape"] {
            assert!(
                checked_document_ydoc_state_path(&graph_dir, invalid).is_err(),
                "invalid document id was accepted: {invalid}"
            );
        }
        let valid = checked_document_ydoc_state_path(&graph_dir, "valid-doc_1")
            .expect("valid sidecar path");
        assert_eq!(
            valid,
            graph_dir.join("ydocs/documents/valid-doc_1/update-v1.bin")
        );
        assert!(
            !graph_dir.join("ydocs/update-v1.bin").exists()
                && !graph_dir.join("escape/update-v1.bin").exists(),
            "rejected ids must not create files outside the document sidecar root"
        );
    }
}
