use crate::app_runtime::AppHandle;
use crate::{
    document_projection_service::hosted_document_summaries,
    document_service::read_workspace_record,
    json_utils::{json_number, json_string},
    paths::existing_graph_dir,
    rdf_service::snapshot_array,
    workspace_entity_projection::{
        hosted_entity_order, hosted_entity_parent_id, hosted_entity_timestamp, workspace_entity_id,
        workspace_entity_title, workspace_folders,
    },
};

pub(super) fn hosted_folder_value(entity: &serde_json::Value, graph_id: &str) -> serde_json::Value {
    let id = workspace_entity_id(entity).unwrap_or_default();
    serde_json::json!({
        "entityType": "folder",
        "id": id,
        "graphId": graph_id,
        "label": workspace_entity_title(entity).unwrap_or(id),
        "parentId": hosted_entity_parent_id(Some(entity)),
        "order": hosted_entity_order(entity),
        "section": json_string(entity.get("section")).unwrap_or_else(|| "documents".to_string()),
        "createdAt": hosted_entity_timestamp(Some(entity), "createdAt", None),
        "updatedAt": hosted_entity_timestamp(Some(entity), "updatedAt", None),
    })
}

pub(super) fn hosted_artifact_file_type(entity: &serde_json::Value) -> String {
    json_string(entity.get("fileType"))
        .or_else(|| {
            entity
                .get("sourceFile")
                .and_then(|source| json_string(source.get("sf_fileType")))
        })
        .or_else(|| {
            workspace_entity_title(entity).and_then(|title| {
                title
                    .rsplit_once('.')
                    .map(|(_, extension)| extension.to_string())
            })
        })
        .unwrap_or_else(|| "unknown".to_string())
}

pub(super) fn hosted_artifact_value(
    entity: &serde_json::Value,
    graph_id: &str,
) -> serde_json::Value {
    let id = workspace_entity_id(entity).unwrap_or_default();
    let label = workspace_entity_title(entity).unwrap_or_else(|| id.clone());
    let source_file = entity.get("sourceFile");
    serde_json::json!({
        "entityType": "artifact",
        "id": id,
        "graphId": graph_id,
        "label": label,
        "parentId": hosted_entity_parent_id(Some(entity)),
        "order": hosted_entity_order(entity),
        "fileType": hosted_artifact_file_type(entity),
        "status": json_string(entity.get("status")).unwrap_or_else(|| "ready".to_string()),
        "errorMessage": json_string(entity.get("errorMessage")),
        "storageKey": source_file
            .and_then(|source| json_string(source.get("sf_storageKey")))
            .or_else(|| json_string(entity.get("storageKey"))),
        "originalFilename": source_file
            .and_then(|source| json_string(source.get("sf_originalFilename")))
            .or_else(|| json_string(entity.get("originalFilename")))
            .unwrap_or(label),
        "mimeType": json_string(entity.get("mimeType"))
            .or_else(|| source_file.and_then(|source| json_string(source.get("sf_mimeType")))),
        "sizeBytes": json_number(entity.get("size"))
            .or_else(|| json_number(entity.get("sizeBytes")))
            .or_else(|| source_file.and_then(|source| json_number(source.get("sf_sizeBytes")))),
        "ingestedDocId": json_string(entity.get("ingestedDocId")),
        "createdAt": hosted_entity_timestamp(Some(entity), "createdAt", None),
        "updatedAt": hosted_entity_timestamp(Some(entity), "updatedAt", None),
    })
}

pub(super) fn hosted_navigation_parts(
    app: &AppHandle,
    graph_id: &str,
) -> Result<
    (
        Vec<serde_json::Value>,
        Vec<serde_json::Value>,
        Vec<serde_json::Value>,
    ),
    String,
> {
    let graph_dir = existing_graph_dir(app, graph_id)?;
    let workspace = read_workspace_record(&graph_dir, graph_id)?;
    let snapshot = workspace.snapshot.as_ref();
    let folders = workspace_folders(snapshot)
        .iter()
        .map(|folder| hosted_folder_value(folder, graph_id))
        .collect::<Vec<_>>();
    let documents = hosted_document_summaries(app, graph_id)?
        .into_iter()
        .map(|summary| {
            serde_json::to_value(summary).expect("HostedDocumentSummary always serializes")
        })
        .collect::<Vec<_>>();
    let artifacts = snapshot
        .map(|snapshot| {
            snapshot_array(snapshot, "artifacts")
                .iter()
                .map(|artifact| hosted_artifact_value(artifact, graph_id))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    Ok((folders, documents, artifacts))
}

pub(super) fn hosted_navigation_response(
    app: &AppHandle,
    graph_id: &str,
) -> Result<serde_json::Value, String> {
    let (folders, documents, artifacts) = hosted_navigation_parts(app, graph_id)?;
    Ok(serde_json::json!({
        "graphId": graph_id,
        "folders": folders,
        "documents": documents,
        "artifacts": artifacts,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hosted_folder_value_preserves_navigation_contract_shape() {
        let folder = serde_json::json!({
            "id": "folder-a",
            "name": "Folder A",
            "parentId": "root",
            "order": 2.5,
            "section": "artifacts",
            "createdAt": "1000",
            "updatedAt": "2000"
        });

        let value = hosted_folder_value(&folder, "graph-a");

        assert_eq!(value["entityType"], "folder");
        assert_eq!(value["id"], "folder-a");
        assert_eq!(value["graphId"], "graph-a");
        assert_eq!(value["label"], "Folder A");
        assert_eq!(value["parentId"], "root");
        assert_eq!(value["order"], 2.5);
        assert_eq!(value["section"], "artifacts");
        assert_eq!(value["createdAt"], "1000");
        assert_eq!(value["updatedAt"], "2000");
    }

    #[test]
    fn hosted_artifact_value_uses_source_file_metadata_and_title_extension_fallback() {
        let artifact = serde_json::json!({
            "id": "artifact-a",
            "title": "paper.pdf",
            "parent_id": "folder-a",
            "order": 4,
            "sourceFile": {
                "sf_fileType": "pdf",
                "sf_storageKey": "objects/paper.pdf",
                "sf_originalFilename": "paper-original.pdf",
                "sf_mimeType": "application/pdf",
                "sf_sizeBytes": 1234
            },
            "ingestedDocId": "doc-a",
            "createdAt": "1000",
            "updatedAt": "2000"
        });

        let value = hosted_artifact_value(&artifact, "graph-a");

        assert_eq!(hosted_artifact_file_type(&artifact), "pdf");
        assert_eq!(value["entityType"], "artifact");
        assert_eq!(value["id"], "artifact-a");
        assert_eq!(value["label"], "paper.pdf");
        assert_eq!(value["parentId"], "folder-a");
        assert_eq!(value["fileType"], "pdf");
        assert_eq!(value["status"], "ready");
        assert_eq!(value["storageKey"], "objects/paper.pdf");
        assert_eq!(value["originalFilename"], "paper-original.pdf");
        assert_eq!(value["mimeType"], "application/pdf");
        assert_eq!(value["sizeBytes"], 1234.0);
        assert_eq!(value["ingestedDocId"], "doc-a");
    }
}
