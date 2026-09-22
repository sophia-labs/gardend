use crate::{
    clock::timestamp,
    ids::validate_local_id,
    runtime_config::YDOC_STATE_FILE,
    storage::{create_dir_all, read_json, remove_file_if_exists, write_json},
};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use uuid::Uuid;

pub(crate) const DOCUMENT_TOMBSTONE_SCHEMA_VERSION: u32 = 1;
const DOCUMENT_TOMBSTONES_DIR: &str = "document-tombstones";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DocumentTombstone {
    pub(crate) schema_version: u32,
    pub(crate) document_id: String,
    /// Stable identity for one deletion boundary. Retries preserve this UUID;
    /// a recreation may clear only the exact boundary it prepared against.
    pub(crate) deletion_id: String,
    /// Journal identity for a structured delete. The same operation reuses
    /// its boundary; every distinct operation replaces it with a fresh UUID.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) deletion_operation_id: Option<String>,
    pub(crate) deleted_at: String,
}

pub(crate) fn document_tombstones_dir(graph_dir: &Path) -> PathBuf {
    graph_dir.join("ydocs").join(DOCUMENT_TOMBSTONES_DIR)
}

pub(crate) fn document_tombstone_path(
    graph_dir: &Path,
    document_id: &str,
) -> Result<PathBuf, String> {
    validate_local_id(document_id, "document_id")?;
    Ok(document_tombstones_dir(graph_dir).join(format!("{document_id}.json")))
}

pub(crate) fn read_document_tombstone(
    graph_dir: &Path,
    document_id: &str,
) -> Result<Option<DocumentTombstone>, String> {
    let path = document_tombstone_path(graph_dir, document_id)?;
    if !path.exists() {
        return Ok(None);
    }
    if !path.is_file() {
        return Err(format!(
            "document tombstone is not a regular file: {}",
            path.display()
        ));
    }
    let tombstone = read_json::<DocumentTombstone>(&path)?;
    if tombstone.schema_version != DOCUMENT_TOMBSTONE_SCHEMA_VERSION {
        return Err(format!(
            "unsupported document tombstone schema {} at {}",
            tombstone.schema_version,
            path.display()
        ));
    }
    validate_local_id(&tombstone.document_id, "document_tombstone.document_id")?;
    if tombstone.document_id != document_id {
        return Err(format!(
            "document tombstone identity mismatch: expected {document_id}, found {}",
            tombstone.document_id
        ));
    }
    Uuid::parse_str(&tombstone.deletion_id).map_err(|error| {
        format!(
            "invalid document tombstone deletionId {}: {error}",
            tombstone.deletion_id
        )
    })?;
    Ok(Some(tombstone))
}

pub(crate) fn document_is_tombstoned(graph_dir: &Path, document_id: &str) -> Result<bool, String> {
    read_document_tombstone(graph_dir, document_id).map(|tombstone| tombstone.is_some())
}

pub(crate) fn require_document_not_tombstoned(
    graph_dir: &Path,
    document_id: &str,
) -> Result<(), String> {
    crate::document_body_availability::require_available(graph_dir, document_id)?;
    if document_is_tombstoned(graph_dir, document_id)? {
        return Err(format!("document tombstoned: {document_id}"));
    }
    Ok(())
}

/// Verify the exact delete boundary held by a trusted recreation. This is the
/// capability check for the small set of internal persistence functions that
/// must write the fresh authority while the public tombstone remains in place.
pub(crate) fn require_document_tombstone_matches(
    graph_dir: &Path,
    document_id: &str,
    expected_deletion_id: &str,
) -> Result<DocumentTombstone, String> {
    crate::document_body_availability::require_available(graph_dir, document_id)?;
    let current = read_document_tombstone(graph_dir, document_id)?
        .ok_or_else(|| format!("document tombstone missing during recreation: {document_id}"))?;
    if current.deletion_id != expected_deletion_id {
        return Err(format!(
            "document deletion boundary changed during recreation: {document_id}"
        ));
    }
    Ok(current)
}

/// Establish the durable deletion boundary. A retry carrying the same journal
/// operation id reuses its UUID. A different operation — or an unjournaled
/// direct delete — always establishes a fresh boundary, fencing a recreation
/// that observed an older delete.
pub(crate) fn write_document_tombstone_for_operation(
    graph_dir: &Path,
    document_id: &str,
    deletion_operation_id: Option<&str>,
) -> Result<DocumentTombstone, String> {
    let _durability_guard = crate::cell_durability::write_guard();
    if let Some(existing) = read_document_tombstone(graph_dir, document_id)? {
        if deletion_operation_id.is_some()
            && existing.deletion_operation_id.as_deref() == deletion_operation_id
        {
            return Ok(existing);
        }
    }
    let path = document_tombstone_path(graph_dir, document_id)?;
    let parent = path.parent().ok_or_else(|| {
        format!(
            "document tombstone has no parent directory: {}",
            path.display()
        )
    })?;
    create_dir_all(parent)?;
    let tombstone = DocumentTombstone {
        schema_version: DOCUMENT_TOMBSTONE_SCHEMA_VERSION,
        document_id: document_id.to_string(),
        deletion_id: Uuid::new_v4().to_string(),
        deletion_operation_id: deletion_operation_id.map(str::to_string),
        deleted_at: timestamp(),
    };
    write_json(&path, &tombstone).map_err(|error| error.to_string())?;
    Ok(tombstone)
}

/// Clear only the deletion boundary a completed recreation observed. A
/// different UUID means another delete won and must remain authoritative.
pub(crate) fn clear_document_tombstone_if_matches(
    graph_dir: &Path,
    document_id: &str,
    expected_deletion_id: &str,
) -> Result<bool, String> {
    // Deletes establish/replace their fence under this same per-document
    // lock. Keep compare + unlink in one critical section so a newer delete
    // cannot land after our UUID check and then have its marker removed.
    crate::document_history_service::with_document_history_lock(graph_dir, document_id, || {
        let Some(current) = read_document_tombstone(graph_dir, document_id)? else {
            return Ok(false);
        };
        if current.deletion_id != expected_deletion_id {
            return Err(format!(
                "stale document recreation for {document_id}: deletion boundary changed"
            ));
        }
        remove_file_if_exists(&document_tombstone_path(graph_dir, document_id)?).map_err(Into::into)
    })
}

/// Recover the canonical graph/document identity encoded by a document
/// sidecar path. Non-canonical test or workspace paths return `None` and do
/// not participate in document tombstone enforcement.
pub(crate) fn document_identity_from_ydoc_state_path(
    state_path: &Path,
) -> Result<Option<(PathBuf, String)>, String> {
    if state_path.file_name().and_then(|value| value.to_str()) != Some(YDOC_STATE_FILE) {
        return Ok(None);
    }
    let Some(document_dir) = state_path.parent() else {
        return Ok(None);
    };
    let Some(documents_dir) = document_dir.parent() else {
        return Ok(None);
    };
    if documents_dir.file_name().and_then(|value| value.to_str()) != Some("documents") {
        return Ok(None);
    }
    let Some(ydocs_dir) = documents_dir.parent() else {
        return Ok(None);
    };
    if ydocs_dir.file_name().and_then(|value| value.to_str()) != Some("ydocs") {
        return Ok(None);
    }
    let Some(graph_dir) = ydocs_dir.parent() else {
        return Ok(None);
    };
    let document_id = document_dir
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| {
            format!(
                "document sidecar directory is not valid UTF-8: {}",
                document_dir.display()
            )
        })?
        .to_string();
    validate_local_id(&document_id, "document_id")?;
    Ok(Some((graph_dir.to_path_buf(), document_id)))
}

pub(crate) fn tombstone_for_ydoc_state_path(
    state_path: &Path,
) -> Result<Option<DocumentTombstone>, String> {
    let Some((graph_dir, document_id)) = document_identity_from_ydoc_state_path(state_path)? else {
        return Ok(None);
    };
    read_document_tombstone(&graph_dir, &document_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tombstone_round_trip_and_matching_clear_are_identity_fenced() {
        let graph_dir =
            std::env::temp_dir().join(format!("garden-document-tombstone-{}", Uuid::new_v4()));
        let document_id = "document-a";
        let first = write_document_tombstone_for_operation(
            &graph_dir,
            document_id,
            Some("delete-operation-a"),
        )
        .expect("write tombstone");
        let retry = write_document_tombstone_for_operation(
            &graph_dir,
            document_id,
            Some("delete-operation-a"),
        )
        .expect("retry tombstone");
        assert_eq!(first, retry);
        assert!(document_is_tombstoned(&graph_dir, document_id).expect("tombstoned"));
        assert!(
            clear_document_tombstone_if_matches(&graph_dir, document_id, &first.deletion_id)
                .expect("clear matching tombstone")
        );
        assert!(!document_is_tombstoned(&graph_dir, document_id).expect("cleared"));
        let _ = std::fs::remove_dir_all(graph_dir);
    }

    #[test]
    fn canonical_sidecar_path_resolves_its_tombstone() {
        let graph_dir = std::env::temp_dir().join(format!(
            "garden-document-tombstone-sidecar-{}",
            Uuid::new_v4()
        ));
        let document_id = "document-sidecar";
        let expected = write_document_tombstone_for_operation(
            &graph_dir,
            document_id,
            Some("delete-operation-sidecar"),
        )
        .expect("write tombstone");
        let state_path = crate::ydoc_paths::document_ydoc_state_path(&graph_dir, document_id);
        assert_eq!(
            tombstone_for_ydoc_state_path(&state_path).expect("sidecar tombstone"),
            Some(expected)
        );
        assert!(
            tombstone_for_ydoc_state_path(&graph_dir.join("update-v1.bin"))
                .expect("noncanonical path")
                .is_none()
        );
        let _ = std::fs::remove_dir_all(graph_dir);
    }

    #[test]
    fn newer_delete_fence_survives_stale_recreation_clear() {
        let graph_dir = std::env::temp_dir().join(format!(
            "garden-document-tombstone-fence-{}",
            Uuid::new_v4()
        ));
        let document_id = "document-fenced";
        let observed = write_document_tombstone_for_operation(
            &graph_dir,
            document_id,
            Some("delete-operation-one"),
        )
        .expect("first delete");
        let newer = write_document_tombstone_for_operation(
            &graph_dir,
            document_id,
            Some("delete-operation-two"),
        )
        .expect("second delete");
        assert_ne!(observed.deletion_id, newer.deletion_id);
        let error =
            clear_document_tombstone_if_matches(&graph_dir, document_id, &observed.deletion_id)
                .expect_err("stale recreation must not clear a newer delete");
        assert!(error.contains("deletion boundary changed"), "{error}");
        assert_eq!(
            read_document_tombstone(&graph_dir, document_id)
                .expect("read newer tombstone")
                .expect("newer tombstone remains")
                .deletion_id,
            newer.deletion_id
        );
        let _ = std::fs::remove_dir_all(graph_dir);
    }

    #[test]
    fn concurrent_new_delete_fence_survives_blocked_stale_recreation_clear() {
        let graph_dir = std::env::temp_dir().join(format!(
            "garden-document-tombstone-concurrent-fence-{}",
            Uuid::new_v4()
        ));
        let document_id = "document-concurrently-fenced";
        let observed = write_document_tombstone_for_operation(
            &graph_dir,
            document_id,
            Some("delete-operation-before-recreation"),
        )
        .expect("first delete");

        let (newer_ready_tx, newer_ready_rx) = std::sync::mpsc::channel();
        let (release_newer_tx, release_newer_rx) = std::sync::mpsc::channel();
        let delete_graph_dir = graph_dir.clone();
        let delete_document_id = document_id.to_string();
        let delete_thread = std::thread::spawn(move || {
            crate::document_history_service::with_document_history_lock(
                &delete_graph_dir,
                &delete_document_id,
                || {
                    let newer = write_document_tombstone_for_operation(
                        &delete_graph_dir,
                        &delete_document_id,
                        Some("delete-operation-racing-recreation"),
                    )?;
                    newer_ready_tx
                        .send(newer.clone())
                        .map_err(|error| format!("announce newer delete: {error}"))?;
                    release_newer_rx
                        .recv()
                        .map_err(|error| format!("release newer delete: {error}"))?;
                    Ok(newer)
                },
            )
        });
        let newer = newer_ready_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("new delete wrote its fence while holding the document lock");

        let (clear_attempt_tx, clear_attempt_rx) = std::sync::mpsc::channel();
        crate::document_history_service::install_document_history_lock_attempt_hook(
            &graph_dir,
            document_id,
            clear_attempt_tx,
        )
        .expect("install stale clear lock hook");
        let clear_graph_dir = graph_dir.clone();
        let clear_document_id = document_id.to_string();
        let observed_deletion_id = observed.deletion_id.clone();
        let clear_thread = std::thread::spawn(move || {
            clear_document_tombstone_if_matches(
                &clear_graph_dir,
                &clear_document_id,
                &observed_deletion_id,
            )
        });
        clear_attempt_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("stale recreation clear blocked behind newer delete");
        release_newer_tx.send(()).expect("release newer delete");
        assert_eq!(
            delete_thread
                .join()
                .expect("new delete thread")
                .expect("new delete result"),
            newer
        );
        let clear_error = clear_thread
            .join()
            .expect("stale clear thread")
            .expect_err("stale recreation cannot clear concurrent newer delete");
        crate::document_history_service::clear_document_history_lock_attempt_hook();
        assert!(
            clear_error.contains("deletion boundary changed"),
            "{clear_error}"
        );
        assert_eq!(
            read_document_tombstone(&graph_dir, document_id)
                .expect("read surviving newer tombstone")
                .expect("newer tombstone remains"),
            newer
        );
        let _ = std::fs::remove_dir_all(graph_dir);
    }
}
