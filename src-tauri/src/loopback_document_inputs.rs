use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct WorkspaceCreateDocumentInput {
    pub(super) document_id: Option<String>,
    pub(super) title: String,
    pub(super) parent_id: Option<String>,
    pub(super) order: Option<f64>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct WorkspaceCreateFolderInput {
    pub(super) folder_id: Option<String>,
    pub(super) name: String,
    pub(super) parent_id: Option<String>,
    pub(super) section: Option<String>,
    pub(super) order: Option<f64>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct WorkspaceMoveDocumentsInput {
    pub(super) document_ids: Vec<String>,
    pub(super) parent_id: Option<String>,
    pub(super) order: Option<f64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct DocumentDescriptionInput {
    pub(super) description: String,
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub(super) struct ManualSnapshotRequest {
    #[serde(default)]
    pub(super) label: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct DocumentFlushQuery {
    #[serde(default, alias = "include_materialization")]
    pub(super) include_materialization: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct DocumentExportQuery {
    pub(super) format: Option<String>,
    pub(super) theme: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_document_inputs_preserve_camel_case_contract() {
        let create = serde_json::from_value::<WorkspaceCreateDocumentInput>(serde_json::json!({
            "documentId": "doc-a",
            "title": "Doc A",
            "parentId": "folder-a",
            "order": 2.5,
        }))
        .unwrap();
        assert_eq!(create.document_id.as_deref(), Some("doc-a"));
        assert_eq!(create.title, "Doc A");
        assert_eq!(create.parent_id.as_deref(), Some("folder-a"));
        assert_eq!(create.order, Some(2.5));

        assert_eq!(
            serde_json::to_value(create).unwrap(),
            serde_json::json!({
                "documentId": "doc-a",
                "title": "Doc A",
                "parentId": "folder-a",
                "order": 2.5,
            })
        );
    }

    #[test]
    fn workspace_folder_and_move_inputs_preserve_route_payload_shape() {
        let folder = serde_json::from_value::<WorkspaceCreateFolderInput>(serde_json::json!({
            "folderId": "folder-a",
            "name": "Folder A",
            "parentId": null,
            "section": "documents",
            "order": 1,
        }))
        .unwrap();
        assert_eq!(folder.folder_id.as_deref(), Some("folder-a"));
        assert_eq!(folder.name, "Folder A");
        assert_eq!(folder.section.as_deref(), Some("documents"));

        let moved = serde_json::from_value::<WorkspaceMoveDocumentsInput>(serde_json::json!({
            "documentIds": ["doc-a", "doc-b"],
            "parentId": "folder-a",
            "order": 3,
        }))
        .unwrap();
        assert_eq!(moved.document_ids, vec!["doc-a", "doc-b"]);
        assert_eq!(moved.parent_id.as_deref(), Some("folder-a"));
        assert_eq!(moved.order, Some(3.0));
    }

    #[test]
    fn document_option_inputs_preserve_aliases_and_defaults() {
        let flush = serde_json::from_value::<DocumentFlushQuery>(serde_json::json!({
            "include_materialization": true,
        }))
        .unwrap();
        assert!(flush.include_materialization);

        let manual =
            serde_json::from_value::<ManualSnapshotRequest>(serde_json::json!({})).unwrap();
        assert_eq!(manual.label, None);

        let export = serde_json::from_value::<DocumentExportQuery>(serde_json::json!({
            "format": "html",
            "theme": "dusk",
        }))
        .unwrap();
        assert_eq!(export.format.as_deref(), Some("html"));
        assert_eq!(export.theme.as_deref(), Some("dusk"));
    }
}
