use crate::{
    json_utils::{json_number, json_string},
    rdf_service::snapshot_array,
};
use std::collections::BTreeSet;

// These helpers intentionally accept and return `&serde_json::Value` workspace
// entities. The workspace snapshot is a heterogeneous union of
// documents/folders/wires/artifacts projected from RDF; each kind has its own
// field set and the same logical field can appear under camelCase or
// snake_case aliases (`parentId` vs `parent_id`, `id` vs `documentId`).
// Pinning a single struct shape here would either break compatibility or
// require an enum that re-implements every projection. The free-form input
// is the contract.

/// Looks up a document entity inside a workspace snapshot. Returns
/// `serde_json::Value` because the snapshot is a heterogeneous union over
/// document/folder/wire/artifact projections — see module docs above.
pub(super) fn workspace_document_entity(
    snapshot: Option<&serde_json::Value>,
    document_id: &str,
) -> Option<serde_json::Value> {
    workspace_documents(snapshot)
        .into_iter()
        .find(|entity| workspace_entity_id(entity).as_deref() == Some(document_id))
}

pub(super) fn hosted_entity_parent_id(entity: Option<&serde_json::Value>) -> Option<String> {
    entity
        .and_then(|entity| json_string(entity.get("parentId").or_else(|| entity.get("parent_id"))))
}

pub(super) fn hosted_entity_order(entity: &serde_json::Value) -> f64 {
    json_number(entity.get("order")).unwrap_or(0.0)
}

pub(super) fn hosted_entity_timestamp(
    entity: Option<&serde_json::Value>,
    key: &str,
    fallback: Option<&str>,
) -> Option<String> {
    entity
        .and_then(|entity| json_string(entity.get(key)))
        .or_else(|| fallback.map(str::to_string))
}

/// Returns the documents array from a workspace snapshot. Entries are
/// `serde_json::Value` because each is a heterogeneous projection — see module
/// docs above.
pub(super) fn workspace_documents(snapshot: Option<&serde_json::Value>) -> Vec<serde_json::Value> {
    snapshot
        .map(|snapshot| snapshot_array(snapshot, "documents").clone())
        .unwrap_or_default()
}

/// Returns the folders array from a workspace snapshot; entries kept as
/// `serde_json::Value` for the reasons documented at module level.
pub(super) fn workspace_folders(snapshot: Option<&serde_json::Value>) -> Vec<serde_json::Value> {
    snapshot
        .map(|snapshot| snapshot_array(snapshot, "folders").clone())
        .unwrap_or_default()
}

/// Returns the wires array from a workspace snapshot; entries kept as
/// `serde_json::Value` for the reasons documented at module level.
pub(super) fn workspace_wires(snapshot: Option<&serde_json::Value>) -> Vec<serde_json::Value> {
    snapshot
        .map(|snapshot| snapshot_array(snapshot, "wires").clone())
        .unwrap_or_default()
}

pub(super) fn workspace_entity_id(entity: &serde_json::Value) -> Option<String> {
    json_string(
        entity
            .get("id")
            .or_else(|| entity.get("documentId"))
            .or_else(|| entity.get("document_id")),
    )
}

pub(super) fn workspace_entity_title(entity: &serde_json::Value) -> Option<String> {
    json_string(entity.get("title").or_else(|| entity.get("name")))
}

pub(super) fn workspace_document_title(
    documents: &[serde_json::Value],
    document_id: Option<&str>,
) -> Option<String> {
    let document_id = document_id?;
    documents
        .iter()
        .find(|document| workspace_entity_id(document).as_deref() == Some(document_id))
        .and_then(workspace_entity_title)
}

pub(super) fn folder_path(
    folders: &[serde_json::Value],
    parent_id: Option<String>,
) -> Option<String> {
    let mut path = Vec::new();
    let mut current = parent_id;
    let mut seen = BTreeSet::new();
    while let Some(folder_id) = current {
        if !seen.insert(folder_id.clone()) {
            break;
        }
        let Some(folder) = folders
            .iter()
            .find(|folder| workspace_entity_id(folder).as_deref() == Some(folder_id.as_str()))
        else {
            break;
        };
        path.push(workspace_entity_title(folder).unwrap_or(folder_id));
        current = json_string(folder.get("parentId").or_else(|| folder.get("parent_id")));
    }
    if path.is_empty() {
        None
    } else {
        path.reverse();
        Some(path.join("/"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_arrays_default_to_empty_and_clone_snapshot_lists() {
        let snapshot = serde_json::json!({
            "documents": [{ "documentId": "doc-a", "title": "Doc A" }],
            "folders": [{ "id": "folder-a", "name": "Folder A" }],
            "wires": [{ "id": "wire-a" }]
        });

        assert_eq!(workspace_documents(Some(&snapshot)).len(), 1);
        assert_eq!(workspace_folders(Some(&snapshot)).len(), 1);
        assert_eq!(workspace_wires(Some(&snapshot)).len(), 1);
        assert!(workspace_documents(None).is_empty());
        assert!(workspace_folders(None).is_empty());
        assert!(workspace_wires(None).is_empty());
    }

    #[test]
    fn workspace_entity_helpers_accept_hosted_and_local_aliases() {
        let document = serde_json::json!({
            "document_id": "doc-a",
            "name": "Document A",
            "parent_id": "folder-a"
        });
        let snapshot = serde_json::json!({
            "documents": [document],
        });

        let entity = workspace_document_entity(Some(&snapshot), "doc-a").unwrap();

        assert_eq!(workspace_entity_id(&entity), Some("doc-a".to_string()));
        assert_eq!(
            workspace_entity_title(&entity),
            Some("Document A".to_string())
        );
        assert_eq!(
            hosted_entity_parent_id(Some(&entity)),
            Some("folder-a".to_string())
        );
        assert_eq!(hosted_entity_order(&entity), 0.0);
        assert_eq!(
            hosted_entity_timestamp(Some(&entity), "updatedAt", Some("fallback")),
            Some("fallback".to_string())
        );
    }

    #[test]
    fn folder_path_walks_parents_and_stops_on_cycles() {
        let folders = vec![
            serde_json::json!({ "id": "root", "title": "Root" }),
            serde_json::json!({ "id": "child", "title": "Child", "parentId": "root" }),
            serde_json::json!({ "id": "loop", "title": "Loop", "parentId": "loop" }),
        ];

        assert_eq!(
            folder_path(&folders, Some("child".to_string())),
            Some("Root/Child".to_string())
        );
        assert_eq!(
            folder_path(&folders, Some("loop".to_string())),
            Some("Loop".to_string())
        );
        assert_eq!(folder_path(&folders, Some("missing".to_string())), None);
    }
}
