use crate::{
    app_error::{AppError, AppResult},
    storage::{read_json, write_json},
    storage_file_ops::{read_bytes, remove_dir_all, write_bytes},
    time_travel_paths::{
        restore_point_dir, restore_point_index_path, restore_point_manifest_path,
        restore_point_workspace_bytes_path, restore_point_workspace_snapshot_path,
    },
    time_travel_types::{
        RestorePointIndex, RestorePointIndexEntry, RestorePointManifest,
        RESTORE_POINT_MANIFEST_SCHEMA_VERSION,
    },
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use serde::{Deserialize, Serialize};
use std::path::Path;

const RESTORE_POINT_INDEX_SCHEMA_VERSION: u32 = 1;
const DEFAULT_LIST_LIMIT: usize = 50;
const MAX_LIST_LIMIT: usize = 200;

#[derive(Debug, Serialize, Deserialize)]
struct RestorePointCursor {
    after_created_at: i64,
    after_id: String,
}

pub(crate) fn read_index(graph_dir: &Path, graph_id: &str) -> AppResult<RestorePointIndex> {
    let path = restore_point_index_path(graph_dir);
    if !path.is_file() {
        return Ok(RestorePointIndex {
            schema_version: RESTORE_POINT_INDEX_SCHEMA_VERSION,
            graph_id: graph_id.to_string(),
            entries: Vec::new(),
        });
    }
    let mut index: RestorePointIndex = read_json(&path).map_err(AppError::storage)?;
    if index.schema_version == 0 {
        index.schema_version = RESTORE_POINT_INDEX_SCHEMA_VERSION;
    }
    if index.graph_id.is_empty() {
        index.graph_id = graph_id.to_string();
    }
    Ok(index)
}

pub(crate) fn write_index(graph_dir: &Path, index: &RestorePointIndex) -> AppResult<()> {
    let path = restore_point_index_path(graph_dir);
    write_json(&path, index).map_err(AppError::storage)
}

pub(crate) fn read_manifest(
    graph_dir: &Path,
    restore_point_id: &str,
) -> AppResult<RestorePointManifest> {
    let path = restore_point_manifest_path(graph_dir, restore_point_id);
    if !path.is_file() {
        return Err(AppError::not_found(format!(
            "restore point {restore_point_id} not found"
        )));
    }
    let manifest: RestorePointManifest = read_json(&path).map_err(AppError::storage)?;
    // Read accepts older schema versions so listings continue to work after
    // a schema bump. The restore execution path enforces a higher minimum
    // (`RESTORE_POINT_MIN_RESTORABLE_SCHEMA_VERSION`).
    if manifest.schema_version > RESTORE_POINT_MANIFEST_SCHEMA_VERSION {
        return Err(AppError::validation(format!(
            "restore point {restore_point_id} manifest schema {} is newer than this build supports (max {})",
            manifest.schema_version, RESTORE_POINT_MANIFEST_SCHEMA_VERSION
        )));
    }
    Ok(manifest)
}

pub(crate) fn write_document_bytes(
    graph_dir: &Path,
    restore_point_id: &str,
    document_id: &str,
    bytes: &[u8],
) -> AppResult<()> {
    let path = crate::time_travel_paths::restore_point_document_bytes_path(
        graph_dir,
        restore_point_id,
        document_id,
    );
    crate::storage_file_ops::write_bytes(&path, bytes).map_err(AppError::storage)
}

pub(crate) fn read_document_bytes(
    graph_dir: &Path,
    restore_point_id: &str,
    document_id: &str,
) -> AppResult<Vec<u8>> {
    let path = crate::time_travel_paths::restore_point_document_bytes_path(
        graph_dir,
        restore_point_id,
        document_id,
    );
    if !path.is_file() {
        return Ok(Vec::new());
    }
    crate::storage_file_ops::read_bytes(&path).map_err(AppError::storage)
}

pub(crate) fn write_manifest(graph_dir: &Path, manifest: &RestorePointManifest) -> AppResult<()> {
    let path = restore_point_manifest_path(graph_dir, &manifest.restore_point_id);
    write_json(&path, manifest).map_err(AppError::storage)
}

pub(crate) fn write_workspace_bundle(
    graph_dir: &Path,
    restore_point_id: &str,
    ydoc_bytes: &[u8],
    snapshot: &serde_json::Value,
) -> AppResult<()> {
    let bytes_path = restore_point_workspace_bytes_path(graph_dir, restore_point_id);
    write_bytes(&bytes_path, ydoc_bytes).map_err(AppError::storage)?;
    let snapshot_path = restore_point_workspace_snapshot_path(graph_dir, restore_point_id);
    write_json(&snapshot_path, snapshot).map_err(AppError::storage)
}

pub(crate) fn read_workspace_bundle(
    graph_dir: &Path,
    restore_point_id: &str,
) -> AppResult<(Vec<u8>, serde_json::Value)> {
    let bytes_path = restore_point_workspace_bytes_path(graph_dir, restore_point_id);
    let snapshot_path = restore_point_workspace_snapshot_path(graph_dir, restore_point_id);
    let bytes = if bytes_path.is_file() {
        read_bytes(&bytes_path).map_err(AppError::storage)?
    } else {
        Vec::new()
    };
    let snapshot = if snapshot_path.is_file() {
        read_json::<serde_json::Value>(&snapshot_path).map_err(AppError::storage)?
    } else {
        serde_json::Value::Null
    };
    Ok((bytes, snapshot))
}

pub(crate) fn delete_restore_point(graph_dir: &Path, restore_point_id: &str) -> AppResult<()> {
    let dir = restore_point_dir(graph_dir, restore_point_id);
    if dir.is_dir() {
        remove_dir_all(&dir).map_err(AppError::storage)?;
    }
    Ok(())
}

pub(crate) fn upsert_index_entry(
    graph_dir: &Path,
    graph_id: &str,
    entry: RestorePointIndexEntry,
) -> AppResult<RestorePointIndex> {
    let mut index = read_index(graph_dir, graph_id)?;
    if index.schema_version == 0 {
        index.schema_version = RESTORE_POINT_INDEX_SCHEMA_VERSION;
    }
    index
        .entries
        .retain(|existing| existing.restore_point_id != entry.restore_point_id);
    index.entries.push(entry);
    sort_index(&mut index);
    write_index(graph_dir, &index)?;
    Ok(index)
}

pub(crate) fn remove_index_entry(
    graph_dir: &Path,
    graph_id: &str,
    restore_point_id: &str,
) -> AppResult<RestorePointIndex> {
    let mut index = read_index(graph_dir, graph_id)?;
    let initial_len = index.entries.len();
    index
        .entries
        .retain(|entry| entry.restore_point_id != restore_point_id);
    if index.entries.len() != initial_len {
        write_index(graph_dir, &index)?;
    }
    Ok(index)
}

fn sort_index(index: &mut RestorePointIndex) {
    // Newest first; ties broken by id (lexicographic) to keep cursor pagination stable.
    index.entries.sort_by(|a, b| {
        b.created_at
            .cmp(&a.created_at)
            .then_with(|| b.restore_point_id.cmp(&a.restore_point_id))
    });
}

pub(crate) struct ListPage {
    pub entries: Vec<RestorePointIndexEntry>,
    pub next_cursor: Option<String>,
    pub total_count: u64,
}

pub(crate) fn list_index_page(
    graph_dir: &Path,
    graph_id: &str,
    cursor: Option<&str>,
    limit: Option<usize>,
) -> AppResult<ListPage> {
    let index = read_index(graph_dir, graph_id)?;
    let total = index.entries.len() as u64;
    let resolved_limit = limit.unwrap_or(DEFAULT_LIST_LIMIT).clamp(1, MAX_LIST_LIMIT);
    let cursor_decoded = match cursor.filter(|value| !value.is_empty()) {
        Some(raw) => Some(decode_cursor(raw)?),
        None => None,
    };
    let mut iter = index.entries.into_iter();
    if let Some(cursor) = cursor_decoded.as_ref() {
        // Skip everything up to and including the entry matching the cursor.
        for entry in iter.by_ref() {
            if entry.created_at == cursor.after_created_at
                && entry.restore_point_id == cursor.after_id
            {
                break;
            }
        }
    }
    let mut entries = Vec::with_capacity(resolved_limit + 1);
    for entry in iter.take(resolved_limit + 1) {
        entries.push(entry);
    }
    let next_cursor = if entries.len() > resolved_limit {
        let last = entries.pop();
        if entries.is_empty() {
            None
        } else {
            // Cursor points at the last returned entry; the next request returns
            // everything strictly after it.
            entries.last().map(|tail| {
                encode_cursor(&RestorePointCursor {
                    after_created_at: tail.created_at,
                    after_id: tail.restore_point_id.clone(),
                })
            })
        }
        .or_else(|| {
            // Edge case: limit=1 with results — fall back to the dropped tail.
            last.map(|tail| {
                encode_cursor(&RestorePointCursor {
                    after_created_at: tail.created_at,
                    after_id: tail.restore_point_id,
                })
            })
        })
    } else {
        None
    };
    Ok(ListPage {
        entries,
        next_cursor,
        total_count: total,
    })
}

fn encode_cursor(cursor: &RestorePointCursor) -> String {
    let json = serde_json::to_vec(cursor).unwrap_or_default();
    URL_SAFE_NO_PAD.encode(json)
}

fn decode_cursor(raw: &str) -> AppResult<RestorePointCursor> {
    let decoded = URL_SAFE_NO_PAD
        .decode(raw)
        .map_err(|error| AppError::validation(format!("invalid restore-point cursor: {error}")))?;
    serde_json::from_slice::<RestorePointCursor>(&decoded).map_err(|error| {
        AppError::validation(format!("invalid restore-point cursor payload: {error}"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::time_travel_types::RestorePointTrigger;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn unique_dir(name: &str) -> std::path::PathBuf {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        std::env::temp_dir().join(format!("mnemosyne-tt-store-{name}-{suffix}"))
    }

    fn make_entry(id: &str, ts: i64) -> RestorePointIndexEntry {
        RestorePointIndexEntry {
            restore_point_id: id.to_string(),
            created_at: ts,
            trigger: RestorePointTrigger::Manual,
            label: None,
            content_hash_sha256: format!("hash-{id}"),
            size_bytes: 100,
            document_count: 1,
            folder_count: 0,
            artifact_count: 0,
        }
    }

    #[test]
    fn upsert_orders_newest_first_and_replaces_duplicates() {
        let dir = unique_dir("order");
        std::fs::create_dir_all(&dir).expect("graph dir");
        upsert_index_entry(&dir, "g1", make_entry("rp-old", 100)).unwrap();
        upsert_index_entry(&dir, "g1", make_entry("rp-mid", 200)).unwrap();
        upsert_index_entry(&dir, "g1", make_entry("rp-new", 300)).unwrap();
        // Replace existing rp-mid with newer timestamp; should move to top.
        let updated = upsert_index_entry(&dir, "g1", make_entry("rp-mid", 400)).unwrap();
        let order: Vec<&str> = updated
            .entries
            .iter()
            .map(|e| e.restore_point_id.as_str())
            .collect();
        assert_eq!(order, vec!["rp-mid", "rp-new", "rp-old"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn list_paginates_with_cursor() {
        let dir = unique_dir("paginate");
        std::fs::create_dir_all(&dir).expect("graph dir");
        for i in 0..7 {
            upsert_index_entry(&dir, "g1", make_entry(&format!("rp-{i}"), i as i64)).unwrap();
        }
        let page1 = list_index_page(&dir, "g1", None, Some(3)).unwrap();
        assert_eq!(page1.entries.len(), 3);
        assert_eq!(page1.total_count, 7);
        assert!(page1.next_cursor.is_some());
        let page2 = list_index_page(&dir, "g1", page1.next_cursor.as_deref(), Some(3)).unwrap();
        assert_eq!(page2.entries.len(), 3);
        let page3 = list_index_page(&dir, "g1", page2.next_cursor.as_deref(), Some(3)).unwrap();
        assert_eq!(page3.entries.len(), 1);
        assert!(page3.next_cursor.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
