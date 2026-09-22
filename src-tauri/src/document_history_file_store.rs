use crate::{
    document_history_store::{
        document_history_dir, document_history_store_path, document_snapshot_payload_path,
        document_tail_commit_path, LocalDocumentHistoryStore, LocalDocumentSnapshotPayload,
        LocalDocumentTailCommit, HISTORY_STORE_SCHEMA_VERSION,
    },
    storage::{create_dir_all, read_json, remove_file_if_exists, write_json},
};
#[cfg(test)]
use std::fs;
use std::path::Path;
#[cfg(test)]
use std::sync::{Mutex, OnceLock};
#[cfg(test)]
use uuid::Uuid;

fn default_document_history_store(graph_id: &str, document_id: &str) -> LocalDocumentHistoryStore {
    LocalDocumentHistoryStore {
        schema_version: HISTORY_STORE_SCHEMA_VERSION,
        graph_id: graph_id.to_string(),
        document_id: document_id.to_string(),
        total_count: 0,
        latest_automatic_revision: None,
        latest_automatic_snapshot_id: None,
        snapshots: Vec::new(),
    }
}

#[cfg(test)]
static FAIL_NEXT_HISTORY_STORE_WRITE: OnceLock<Mutex<Option<std::path::PathBuf>>> = OnceLock::new();

#[cfg(test)]
pub(super) fn fail_next_history_store_write(path: std::path::PathBuf) {
    *FAIL_NEXT_HISTORY_STORE_WRITE
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(path);
}

pub(super) fn read_document_history_store(
    graph_dir: &Path,
    graph_id: &str,
    document_id: &str,
) -> Result<LocalDocumentHistoryStore, String> {
    let path = document_history_store_path(graph_dir, document_id);
    if !path.is_file() {
        return Ok(default_document_history_store(graph_id, document_id));
    }
    let mut store = read_json::<LocalDocumentHistoryStore>(&path)?;
    store.schema_version = HISTORY_STORE_SCHEMA_VERSION;
    store.graph_id = graph_id.to_string();
    store.document_id = document_id.to_string();
    store.total_count = store.total_count.max(store.snapshots.len() as u64);
    for snapshot in &mut store.snapshots {
        snapshot.graph_id = graph_id.to_string();
        snapshot.document_id = document_id.to_string();
        if snapshot.tier.trim().is_empty() {
            snapshot.tier = "20min".to_string();
        }
        if snapshot.snapshot_count == 0 {
            snapshot.snapshot_count = 1;
        }
    }
    Ok(store)
}

pub(super) fn write_document_history_store(
    graph_dir: &Path,
    store: &LocalDocumentHistoryStore,
) -> Result<(), String> {
    let path = document_history_store_path(graph_dir, &store.document_id);
    #[cfg(test)]
    {
        let mut failure = FAIL_NEXT_HISTORY_STORE_WRITE
            .get_or_init(|| Mutex::new(None))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if failure.as_ref() == Some(&path) {
            *failure = None;
            return Err(format!(
                "injected history store write failure: {}",
                path.display()
            ));
        }
    }
    create_dir_all(&document_history_dir(graph_dir, &store.document_id))?;
    write_json(&path, store).map_err(Into::into)
}

pub(super) fn read_document_tail_commit(
    graph_dir: &Path,
    document_id: &str,
) -> Result<Option<LocalDocumentTailCommit>, String> {
    let path = document_tail_commit_path(graph_dir, document_id);
    if !path.is_file() {
        return Ok(None);
    }
    read_json::<LocalDocumentTailCommit>(&path)
        .map(Some)
        .map_err(Into::into)
}

pub(super) fn write_document_tail_commit(
    graph_dir: &Path,
    commit: &LocalDocumentTailCommit,
) -> Result<(), String> {
    create_dir_all(&document_history_dir(graph_dir, &commit.document_id))?;
    write_json(
        &document_tail_commit_path(graph_dir, &commit.document_id),
        commit,
    )
    .map_err(Into::into)
}

pub(super) fn read_document_snapshot_payload(
    graph_dir: &Path,
    document_id: &str,
    snapshot_id: &str,
) -> Result<LocalDocumentSnapshotPayload, String> {
    let path = document_snapshot_payload_path(graph_dir, document_id, snapshot_id);
    if !path.is_file() {
        return Err(format!("snapshot not found: {snapshot_id}"));
    }
    read_json::<LocalDocumentSnapshotPayload>(&path).map_err(Into::into)
}

pub(super) fn remove_snapshot_payload(graph_dir: &Path, document_id: &str, snapshot_id: &str) {
    let path = document_snapshot_payload_path(graph_dir, document_id, snapshot_id);
    let _ = remove_file_if_exists(&path);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_store_read_repairs_schema_identity_and_counts() {
        let graph_dir =
            std::env::temp_dir().join(format!("sophia-history-store-{}", Uuid::new_v4()));
        let document_id = "doc-a";
        fs::create_dir_all(document_history_dir(&graph_dir, document_id)).unwrap();
        fs::write(
            document_history_store_path(&graph_dir, document_id),
            serde_json::to_vec_pretty(&serde_json::json!({
                "schemaVersion": 0,
                "graphId": "old-graph",
                "documentId": "old-doc",
                "totalCount": 0,
                "snapshots": [{
                    "snapshotId": "snapshot-a",
                    "graphId": "old-graph",
                    "documentId": "old-doc",
                    "tier": "",
                    "snapshotCount": 0,
                    "createdAt": "1000"
                }]
            }))
            .unwrap(),
        )
        .unwrap();

        let store = read_document_history_store(&graph_dir, "graph-a", document_id).unwrap();
        assert_eq!(store.schema_version, HISTORY_STORE_SCHEMA_VERSION);
        assert_eq!(store.graph_id, "graph-a");
        assert_eq!(store.document_id, document_id);
        assert_eq!(store.total_count, 1);
        assert_eq!(store.snapshots[0].tier, "20min");
        assert_eq!(store.snapshots[0].snapshot_count, 1);
        assert_eq!(store.snapshots[0].graph_id, "graph-a");
        assert_eq!(store.snapshots[0].document_id, document_id);

        fs::remove_dir_all(graph_dir).unwrap();
    }
}
