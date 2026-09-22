pub(super) fn move_folder_payload(
    arguments: &serde_json::Value,
    folder_id: String,
) -> serde_json::Value {
    serde_json::json!({
        "folderId": folder_id,
        "newParentId": arguments
            .get("newParentId")
            .or_else(|| arguments.get("new_parent_id"))
            .or_else(|| arguments.get("parentId"))
            .or_else(|| arguments.get("parent_id"))
            .cloned()
            .unwrap_or(serde_json::Value::Null),
        "newOrder": arguments
            .get("newOrder")
            .or_else(|| arguments.get("new_order"))
            .or_else(|| arguments.get("order"))
            .cloned()
            .unwrap_or(serde_json::Value::Null),
    })
}

pub(super) fn mcp_raw_graph_id_argument(arguments: &serde_json::Value) -> String {
    arguments
        .get("graphId")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string()
}

pub(super) fn create_document_payload(arguments: &serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "documentId": arguments.get("documentId").cloned().unwrap_or(serde_json::Value::Null),
        "title": arguments.get("title").cloned().unwrap_or_else(|| serde_json::json!("Untitled")),
        "parentId": arguments.get("parentId").cloned().unwrap_or(serde_json::Value::Null),
        "order": arguments.get("order").cloned().unwrap_or(serde_json::Value::Null),
    })
}

pub(super) fn create_folder_payload(arguments: &serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "folderId": arguments.get("folderId").cloned().unwrap_or(serde_json::Value::Null),
        "name": arguments.get("name").cloned().unwrap_or_else(|| serde_json::json!("Untitled Folder")),
        "parentId": arguments.get("parentId").cloned().unwrap_or(serde_json::Value::Null),
        "section": arguments.get("section").cloned().unwrap_or_else(|| serde_json::json!("documents")),
        "order": arguments.get("order").cloned().unwrap_or(serde_json::Value::Null),
    })
}

pub(super) fn move_documents_payload(arguments: &serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "documentIds": arguments.get("documentIds").cloned().unwrap_or_else(|| serde_json::json!([])),
        "parentId": arguments.get("parentId").cloned().unwrap_or(serde_json::Value::Null),
        "order": arguments.get("order").cloned().unwrap_or(serde_json::Value::Null),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn move_folder_payload_preserves_aliases_and_absent_values_as_null() {
        let payload = move_folder_payload(
            &serde_json::json!({
                "new_parent_id": "folder-parent",
                "order": 7,
            }),
            "folder-a".to_string(),
        );

        assert_eq!(
            payload,
            serde_json::json!({
                "folderId": "folder-a",
                "newParentId": "folder-parent",
                "newOrder": 7,
            })
        );

        assert_eq!(
            move_folder_payload(&serde_json::json!({}), "folder-a".to_string()),
            serde_json::json!({
                "folderId": "folder-a",
                "newParentId": null,
                "newOrder": null,
            })
        );
    }

    #[test]
    fn raw_crdt_payload_helpers_preserve_route_defaults() {
        assert_eq!(
            mcp_raw_graph_id_argument(&serde_json::json!({ "graph_id": "ignored" })),
            ""
        );
        assert_eq!(
            create_document_payload(&serde_json::json!({})),
            serde_json::json!({
                "documentId": null,
                "title": "Untitled",
                "parentId": null,
                "order": null,
            })
        );
        assert_eq!(
            create_folder_payload(&serde_json::json!({})),
            serde_json::json!({
                "folderId": null,
                "name": "Untitled Folder",
                "parentId": null,
                "section": "documents",
                "order": null,
            })
        );
        assert_eq!(
            move_documents_payload(&serde_json::json!({})),
            serde_json::json!({
                "documentIds": [],
                "parentId": null,
                "order": null,
            })
        );
    }

    #[test]
    fn raw_crdt_payload_helpers_preserve_camel_case_values() {
        let arguments = serde_json::json!({
            "graphId": "graph-a",
            "documentId": "doc-a",
            "folderId": "folder-a",
            "title": "Doc A",
            "name": "Folder A",
            "parentId": "parent-a",
            "section": "library",
            "documentIds": ["doc-a", "doc-b"],
            "order": 4,
        });

        assert_eq!(mcp_raw_graph_id_argument(&arguments), "graph-a");
        assert_eq!(
            create_document_payload(&arguments),
            serde_json::json!({
                "documentId": "doc-a",
                "title": "Doc A",
                "parentId": "parent-a",
                "order": 4,
            })
        );
        assert_eq!(
            create_folder_payload(&arguments),
            serde_json::json!({
                "folderId": "folder-a",
                "name": "Folder A",
                "parentId": "parent-a",
                "section": "library",
                "order": 4,
            })
        );
        assert_eq!(
            move_documents_payload(&arguments),
            serde_json::json!({
                "documentIds": ["doc-a", "doc-b"],
                "parentId": "parent-a",
                "order": 4,
            })
        );
    }
}
