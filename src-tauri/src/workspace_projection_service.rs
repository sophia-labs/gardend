use std::collections::{HashMap, HashSet};

use crate::app_runtime::AppHandle;
use crate::{
    crdt_projection_flush::flush_graph_projection,
    document_record_store::read_graph_document_inventory,
    document_service::read_workspace_record,
    graph_service::GraphRecord,
    mcp_utils::{mcp_arg_bool, mcp_arg_string, mcp_arg_usize, mcp_required_graph_id},
    paths::existing_graph_dir,
    storage::read_json,
    workspace_document_projection::workspace_document_rows,
};

const DEFAULT_WORKSPACE_LIMIT: usize = 50;
const MAX_WORKSPACE_LIMIT: usize = 200;
const DEFAULT_WORKSPACE_BYTES: usize = 64 * 1024;
const MIN_WORKSPACE_BYTES: usize = 16 * 1024;
const MAX_WORKSPACE_BYTES: usize = 256 * 1024;
const MAX_WORKSPACE_DEPTH: usize = 5;

pub(super) async fn mcp_local_get_workspace(
    app: AppHandle,
    arguments: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let graph_id = mcp_required_graph_id(arguments)?;
    flush_graph_projection(app.clone(), &graph_id).await?;

    let graph_dir = existing_graph_dir(&app, &graph_id)?;
    let graph = read_json::<GraphRecord>(&graph_dir.join("graph.json"))?;
    let workspace = read_workspace_record(&graph_dir, &graph_id)?;
    let document_records = read_graph_document_inventory(&graph_dir)?;
    let snapshot = workspace.snapshot.as_ref();
    // Borrow immutable inventory arrays from the already-owned snapshot.
    // Only document rows need a clone for their count overlay; fallback
    // rows are constructed lazily when the snapshot has no document rows.
    let folders = snapshot_array(snapshot, "folders");
    let documents = workspace_document_rows(snapshot, &document_records);
    let artifacts = snapshot_array(snapshot, "artifacts");
    let wires = snapshot_array(snapshot, "wires");

    let depth = mcp_arg_usize(arguments, &["depth"], 1).min(MAX_WORKSPACE_DEPTH);
    let limit =
        mcp_arg_usize(arguments, &["limit"], DEFAULT_WORKSPACE_LIMIT).clamp(1, MAX_WORKSPACE_LIMIT);
    let max_bytes = mcp_arg_usize(
        arguments,
        &["maxBytes", "max_bytes"],
        DEFAULT_WORKSPACE_BYTES,
    )
    .clamp(MIN_WORKSPACE_BYTES, MAX_WORKSPACE_BYTES);
    let cursor = workspace_cursor(arguments)?;
    let root_folder = mcp_arg_string(arguments, &["folderId", "folder_id"]);
    let folders_only = mcp_arg_bool(arguments, &["foldersOnly", "folders_only"], false);
    let folder_parents = folder_parent_map(folders);

    let mut eligible = Vec::new();
    if depth > 0 {
        for folder in folders {
            if let Some(relative_depth) =
                workspace_entry_depth(folder, root_folder.as_deref(), &folder_parents)
            {
                if relative_depth > depth {
                    continue;
                }
                let mut compact = compact_workspace_entry("folder", folder);
                if relative_depth == depth {
                    if let Some(folder_id) = workspace_entry_id(folder) {
                        let (nested_folders, nested_documents) =
                            descendant_counts(folder_id, folders, &documents, &folder_parents);
                        compact["collapsed"] = serde_json::json!({
                            "folders": nested_folders,
                            "documents": nested_documents,
                        });
                    }
                }
                eligible.push(compact);
            }
        }
        if !folders_only {
            for document in &documents {
                if visible_at_depth(document, root_folder.as_deref(), depth, &folder_parents) {
                    eligible.push(compact_workspace_entry("document", document));
                }
            }
            for artifact in artifacts {
                if visible_at_depth(artifact, root_folder.as_deref(), depth, &folder_parents) {
                    eligible.push(compact_workspace_entry("artifact", artifact));
                }
            }
        }
    }
    eligible.sort_by_key(workspace_entry_sort_key);

    let total_eligible = eligible.len();
    let start = cursor.min(total_eligible);
    let mut page_entries = Vec::new();
    let mut next_offset = start;
    for entry in eligible.into_iter().skip(start).take(limit) {
        let mut candidate = page_entries.clone();
        candidate.push(entry.clone());
        let candidate_next = start + candidate.len();
        let candidate_value = workspace_page_value(
            &graph,
            &workspace,
            snapshot.is_some(),
            depth,
            root_folder.as_deref(),
            candidate,
            total_eligible,
            candidate_next,
            limit,
            max_bytes,
            folders.len(),
            documents.len(),
            artifacts.len(),
            wires.len(),
        );
        let (_, candidate_bytes) = finalize_serialized_bytes(candidate_value)?;
        if candidate_bytes > max_bytes {
            break;
        }
        page_entries.push(entry);
        next_offset = candidate_next;
    }

    let response = workspace_page_value(
        &graph,
        &workspace,
        snapshot.is_some(),
        depth,
        root_folder.as_deref(),
        page_entries,
        total_eligible,
        next_offset,
        limit,
        max_bytes,
        folders.len(),
        documents.len(),
        artifacts.len(),
        wires.len(),
    );
    let (response, _) = finalize_serialized_bytes(response)?;
    Ok(response)
}

fn finalize_serialized_bytes(
    mut value: serde_json::Value,
) -> Result<(serde_json::Value, usize), String> {
    // The byte count is part of the payload, so setting it can change the
    // payload's size. Iterate until the stored count and actual JSON size agree.
    loop {
        let bytes = serialized_bytes(&value)?;
        if value["page"]["serializedBytes"].as_u64() == Some(bytes as u64) {
            return Ok((value, bytes));
        }
        value["page"]["serializedBytes"] = serde_json::json!(bytes);
    }
}

#[allow(clippy::too_many_arguments)]
fn workspace_page_value(
    graph: &GraphRecord,
    workspace: &crate::document_service::WorkspaceRecord,
    has_snapshot: bool,
    depth: usize,
    root_folder: Option<&str>,
    entries: Vec<serde_json::Value>,
    total_eligible: usize,
    next_offset: usize,
    limit: usize,
    max_bytes: usize,
    folder_count: usize,
    document_count: usize,
    artifact_count: usize,
    wire_count: usize,
) -> serde_json::Value {
    let folders = entries
        .iter()
        .filter(|entry| entry["type"] == "folder")
        .cloned()
        .collect::<Vec<_>>();
    let documents = entries
        .iter()
        .filter(|entry| entry["type"] == "document")
        .cloned()
        .collect::<Vec<_>>();
    let artifacts = entries
        .iter()
        .filter(|entry| entry["type"] == "artifact")
        .cloned()
        .collect::<Vec<_>>();
    let complete = next_offset >= total_eligible;
    serde_json::json!({
        "graph_id": graph.graph_id,
        "graphId": graph.graph_id,
        "title": graph.title,
        "depth": depth,
        "folderId": root_folder,
        "folders": folders,
        "documents": documents,
        "artifacts": artifacts,
        "wires": [],
        "tree": {
            "type": "page",
            "title": graph.title,
            "children": entries,
        },
        "counts": {
            "documents": document_count,
            "folders": folder_count,
            "artifacts": artifact_count,
            "wires": wire_count,
            "eligible": total_eligible,
        },
        "page": {
            "limit": limit,
            "maxBytes": max_bytes,
            "cursor": next_offset.saturating_sub(
                documents.len() + folders.len() + artifacts.len()
            ).to_string(),
            "nextCursor": if complete { serde_json::Value::Null } else { serde_json::json!(next_offset.to_string()) },
            "complete": complete,
            "returned": documents.len() + folders.len() + artifacts.len(),
            "serializedBytes": 0,
        },
        "workspace": {
            "updatedAt": workspace.updated_at,
            "ydocUpdateBytes": workspace.ydoc_update_base64.len() * 3 / 4,
            "materialization": if has_snapshot { "workspace-snapshot" } else { "flat-document-fallback" },
        },
    })
}

fn snapshot_array<'a>(snapshot: Option<&'a serde_json::Value>, key: &str) -> &'a [serde_json::Value] {
    snapshot
        .and_then(|value| value.get(key))
        .and_then(serde_json::Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
}

fn workspace_cursor(arguments: &serde_json::Value) -> Result<usize, String> {
    let Some(value) = arguments.get("cursor") else {
        return Ok(0);
    };
    if let Some(offset) = value.as_u64() {
        return usize::try_from(offset).map_err(|_| "workspace cursor is too large".to_string());
    }
    value
        .as_str()
        .ok_or_else(|| "workspace cursor must be a non-negative integer string".to_string())?
        .parse::<usize>()
        .map_err(|_| "workspace cursor must be a non-negative integer string".to_string())
}

fn folder_parent_map(folders: &[serde_json::Value]) -> HashMap<String, Option<String>> {
    folders
        .iter()
        .filter_map(|folder| {
            workspace_entry_id(folder).map(|id| (id.to_string(), workspace_parent_id(folder)))
        })
        .collect()
}

fn visible_at_depth(
    entry: &serde_json::Value,
    root_folder: Option<&str>,
    max_depth: usize,
    folder_parents: &HashMap<String, Option<String>>,
) -> bool {
    workspace_entry_depth(entry, root_folder, folder_parents)
        .is_some_and(|depth| depth <= max_depth)
}

fn workspace_entry_depth(
    entry: &serde_json::Value,
    root_folder: Option<&str>,
    folder_parents: &HashMap<String, Option<String>>,
) -> Option<usize> {
    let mut current = workspace_parent_id(entry);
    let mut distance = 1;
    let mut seen = HashSet::new();
    loop {
        match (root_folder, current.as_deref()) {
            (Some(root), Some(parent)) if parent == root => return Some(distance),
            (Some(_), None) => return None,
            (None, None) => return Some(distance),
            _ => {}
        }
        let Some(parent) = current else {
            return None;
        };
        if !seen.insert(parent.clone()) {
            return None;
        }
        current = folder_parents.get(&parent).cloned().flatten();
        distance += 1;
    }
}

fn descendant_counts(
    folder_id: &str,
    folders: &[serde_json::Value],
    documents: &[serde_json::Value],
    folder_parents: &HashMap<String, Option<String>>,
) -> (usize, usize) {
    let nested_folders = folders
        .iter()
        .filter(|folder| {
            workspace_entry_id(folder) != Some(folder_id)
                && parent_descends_from(workspace_parent_id(folder), folder_id, folder_parents)
        })
        .count();
    let nested_documents = documents
        .iter()
        .filter(|document| {
            parent_descends_from(workspace_parent_id(document), folder_id, folder_parents)
        })
        .count();
    (nested_folders, nested_documents)
}

fn parent_descends_from(
    mut parent: Option<String>,
    ancestor: &str,
    folder_parents: &HashMap<String, Option<String>>,
) -> bool {
    let mut seen = HashSet::new();
    while let Some(current) = parent {
        if current == ancestor {
            return true;
        }
        if !seen.insert(current.clone()) {
            return false;
        }
        parent = folder_parents.get(&current).cloned().flatten();
    }
    false
}

fn compact_workspace_entry(kind: &str, entry: &serde_json::Value) -> serde_json::Value {
    let id = workspace_entry_id(entry).unwrap_or_default();
    let title = entry
        .get("title")
        .or_else(|| entry.get("name"))
        .and_then(serde_json::Value::as_str)
        .map(|value| value.chars().take(512).collect::<String>())
        .unwrap_or_default();
    serde_json::json!({
        "type": kind,
        "id": id,
        "documentId": if kind == "document" { serde_json::json!(id) } else { serde_json::Value::Null },
        "title": title,
        "parentId": workspace_parent_id(entry),
        "order": entry.get("order").cloned().unwrap_or(serde_json::Value::Null),
        "updatedAt": entry.get("updatedAt").or_else(|| entry.get("updated_at")).cloned().unwrap_or(serde_json::Value::Null),
        "blockCount": entry.get("blockCount").or_else(|| entry.get("block_count")).cloned().unwrap_or(serde_json::Value::Null),
        "rdfTripleCount": entry.get("rdfTripleCount").cloned().unwrap_or(serde_json::Value::Null),
    })
}

fn workspace_entry_id(entry: &serde_json::Value) -> Option<&str> {
    entry
        .get("id")
        .or_else(|| entry.get("documentId"))
        .or_else(|| entry.get("document_id"))
        .or_else(|| entry.get("folderId"))
        .or_else(|| entry.get("artifactId"))
        .and_then(serde_json::Value::as_str)
}

fn workspace_parent_id(entry: &serde_json::Value) -> Option<String> {
    entry
        .get("parentId")
        .or_else(|| entry.get("parent_id"))
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn workspace_entry_sort_key(entry: &serde_json::Value) -> (String, String, String, String) {
    (
        entry["parentId"].as_str().unwrap_or_default().to_string(),
        entry["type"].as_str().unwrap_or_default().to_string(),
        entry["title"].as_str().unwrap_or_default().to_lowercase(),
        entry["id"].as_str().unwrap_or_default().to_string(),
    )
}

fn serialized_bytes(value: &serde_json::Value) -> Result<usize, String> {
    serde_json::to_vec(value)
        .map(|bytes| bytes.len())
        .map_err(|error| format!("serialize bounded workspace response: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_inventory_borrows_snapshot_arrays_without_mutating_custom_fields() {
        let snapshot = serde_json::json!({
            "folders": [{"id": "folder-a", "custom": "x".repeat(100_000)}],
            "artifacts": [{"id": "artifact-a", "extra": {"contentHashSha256": "keep"}}],
            "wires": [{"id": "wire-a", "custom": [1, 2, 3]}],
            "customWorkspace": {"untouched": true},
        });
        let before = snapshot.clone();
        for key in ["folders", "artifacts", "wires"] {
            let borrowed = snapshot_array(Some(&snapshot), key);
            let original = snapshot[key].as_array().unwrap().as_slice();
            assert!(std::ptr::eq(borrowed, original), "{key} must borrow the same source allocation");
        }
        assert!(snapshot_array(Some(&snapshot), "missing").is_empty());
        assert!(snapshot_array(None, "folders").is_empty());
        assert_eq!(snapshot, before);
    }

    #[test]
    fn depth_filter_is_relative_to_the_requested_folder() {
        let folders = vec![
            serde_json::json!({"id": "a", "title": "A"}),
            serde_json::json!({"id": "b", "parentId": "a", "title": "B"}),
            serde_json::json!({"id": "c", "parentId": "b", "title": "C"}),
        ];
        let parents = folder_parent_map(&folders);
        assert!(visible_at_depth(&folders[1], Some("a"), 1, &parents));
        assert!(!visible_at_depth(&folders[2], Some("a"), 1, &parents));
        assert!(visible_at_depth(&folders[2], Some("a"), 2, &parents));
    }

    #[test]
    fn compact_entries_drop_unbounded_document_content() {
        let entry = serde_json::json!({
            "documentId": "doc-a",
            "title": "A",
            "body": "x".repeat(900_000),
            "ydocUpdateBase64": "y".repeat(900_000),
            "blockCount": 7,
        });
        let compact = compact_workspace_entry("document", &entry);
        let bytes = serde_json::to_vec(&compact).unwrap().len();
        assert!(bytes < 1_024);
        assert_eq!(compact["blockCount"], 7);
        assert!(compact.get("body").is_none());
    }

    #[test]
    fn depth_boundary_reports_collapsed_descendant_counts() {
        let folders = vec![
            serde_json::json!({"id": "a"}),
            serde_json::json!({"id": "b", "parentId": "a"}),
            serde_json::json!({"id": "c", "parentId": "b"}),
        ];
        let documents = vec![
            serde_json::json!({"documentId": "one", "parentId": "a"}),
            serde_json::json!({"documentId": "two", "parentId": "c"}),
        ];
        let parents = folder_parent_map(&folders);
        assert_eq!(
            descendant_counts("a", &folders, &documents, &parents),
            (2, 2)
        );
    }

    #[test]
    fn cursor_accepts_opaque_decimal_strings_and_rejects_garbage() {
        assert_eq!(
            workspace_cursor(&serde_json::json!({"cursor": "42"})).unwrap(),
            42
        );
        assert!(workspace_cursor(&serde_json::json!({"cursor": "later"})).is_err());
    }

    #[test]
    fn serialized_byte_metadata_is_exact() {
        let value = serde_json::json!({
            "tree": { "children": ["x".repeat(10_000)] },
            "page": { "serializedBytes": 0 }
        });
        let (value, bytes) = finalize_serialized_bytes(value).unwrap();
        assert_eq!(value["page"]["serializedBytes"], bytes);
        assert_eq!(serde_json::to_vec(&value).unwrap().len(), bytes);
    }
}
