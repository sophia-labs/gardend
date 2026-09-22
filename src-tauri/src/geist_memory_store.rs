use crate::storage::{create_dir_all, read_json, write_json};
use serde::{Deserialize, Serialize};
#[cfg(test)]
use std::fs;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};
#[cfg(test)]
use uuid::Uuid;

const MEMORY_STORE_SCHEMA_VERSION: u32 = 1;
const MEMORY_STORE_FILE: &str = "memory-queue.json";

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LocalMemoryStore {
    pub(crate) schema_version: u32,
    pub(crate) graph_id: String,
    #[serde(default = "default_next_memory_number")]
    pub(crate) next_number: u64,
    #[serde(default)]
    pub(crate) memories: BTreeMap<String, LocalMemoryRecord>,
    #[serde(default)]
    pub(crate) archives: Vec<LocalMemoryArchiveRecord>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LocalMemoryRecord {
    pub(crate) number: u64,
    pub(crate) block_id: String,
    pub(crate) content: String,
    pub(crate) created_at: String,
    pub(crate) last_active: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LocalMemoryArchiveRecord {
    pub(crate) archive_doc_id: String,
    pub(crate) archived_at: String,
    pub(crate) kept: usize,
    pub(crate) archived: usize,
    pub(crate) memories: Vec<LocalMemoryRecord>,
}

fn memory_store_dir(graph_dir: &Path) -> PathBuf {
    graph_dir.join("memory")
}

fn memory_store_path(graph_dir: &Path) -> PathBuf {
    memory_store_dir(graph_dir).join(MEMORY_STORE_FILE)
}

fn memory_archive_dir(graph_dir: &Path) -> PathBuf {
    memory_store_dir(graph_dir).join("archives")
}

fn default_next_memory_number() -> u64 {
    1
}

fn default_memory_store(graph_id: &str) -> LocalMemoryStore {
    LocalMemoryStore {
        schema_version: MEMORY_STORE_SCHEMA_VERSION,
        graph_id: graph_id.to_string(),
        next_number: default_next_memory_number(),
        memories: BTreeMap::new(),
        archives: Vec::new(),
    }
}

pub(crate) fn read_memory_store(
    graph_dir: &Path,
    graph_id: &str,
) -> Result<LocalMemoryStore, String> {
    let path = memory_store_path(graph_dir);
    if !path.is_file() {
        return Ok(default_memory_store(graph_id));
    }
    let mut store = read_json::<LocalMemoryStore>(&path)?;
    store.graph_id = graph_id.to_string();
    store.schema_version = MEMORY_STORE_SCHEMA_VERSION;
    if store.next_number == 0 {
        store.next_number = default_next_memory_number();
    }
    let highest_number = store
        .memories
        .values()
        .map(|memory| memory.number)
        .max()
        .unwrap_or(0);
    if store.next_number <= highest_number {
        store.next_number = highest_number + 1;
    }
    Ok(store)
}

pub(crate) fn write_memory_store(graph_dir: &Path, store: &LocalMemoryStore) -> Result<(), String> {
    create_dir_all(&memory_store_dir(graph_dir))?;
    write_json(&memory_store_path(graph_dir), store).map_err(Into::into)
}

pub(crate) fn memory_text(memory: &LocalMemoryRecord) -> String {
    format!("{}. {}", memory.number, memory.content)
}

pub(crate) fn memory_sort_key(memory: &LocalMemoryRecord) -> &str {
    if memory.last_active >= memory.created_at {
        memory.last_active.as_str()
    } else {
        memory.created_at.as_str()
    }
}

pub(crate) fn memory_json(memory: &LocalMemoryRecord) -> serde_json::Value {
    serde_json::json!({
        "number": memory.number,
        "text": memory_text(memory),
        "content": memory.content,
        "block_id": memory.block_id,
        "blockId": memory.block_id,
        "created_at": memory.created_at,
        "createdAt": memory.created_at,
        "last_active": memory.last_active,
        "lastActive": memory.last_active,
    })
}

pub(crate) fn write_memory_archive_file(
    graph_dir: &Path,
    archive_doc_id: &str,
    memories: &[LocalMemoryRecord],
) -> Result<(), String> {
    create_dir_all(&memory_archive_dir(graph_dir))?;
    write_json(
        &memory_archive_dir(graph_dir).join(format!("{archive_doc_id}.json")),
        &memories.to_vec(),
    )
    .map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_sort_key_prefers_last_active_when_newer() {
        let recent = LocalMemoryRecord {
            number: 1,
            block_id: "memory-1".to_string(),
            content: "recent".to_string(),
            created_at: "1000".to_string(),
            last_active: "2000".to_string(),
        };
        let legacy = LocalMemoryRecord {
            number: 2,
            block_id: "memory-2".to_string(),
            content: "legacy".to_string(),
            created_at: "3000".to_string(),
            last_active: "1000".to_string(),
        };

        assert_eq!(memory_sort_key(&recent), "2000");
        assert_eq!(memory_sort_key(&legacy), "3000");
    }

    #[test]
    fn read_memory_store_backfills_next_number() {
        let graph_dir =
            std::env::temp_dir().join(format!("sophia-memory-store-{}", Uuid::new_v4()));
        let memory_dir = graph_dir.join("memory");
        fs::create_dir_all(&memory_dir).unwrap();
        fs::write(
            memory_dir.join(MEMORY_STORE_FILE),
            serde_json::to_vec_pretty(&serde_json::json!({
                "schemaVersion": 0,
                "graphId": "legacy-graph",
                "nextNumber": 1,
                "memories": {
                    "4": {
                        "number": 4,
                        "blockId": "memory-4",
                        "content": "kept",
                        "createdAt": "1000",
                        "lastActive": "2000"
                    }
                }
            }))
            .unwrap(),
        )
        .unwrap();

        let store = read_memory_store(&graph_dir, "current-graph").unwrap();
        assert_eq!(store.graph_id, "current-graph");
        assert_eq!(store.schema_version, MEMORY_STORE_SCHEMA_VERSION);
        assert_eq!(store.next_number, 5);

        fs::remove_dir_all(graph_dir).unwrap();
    }
}
