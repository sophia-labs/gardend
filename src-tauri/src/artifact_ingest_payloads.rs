use crate::{
    artifact_ingest_title::artifact_import_document_title, json_utils::json_string,
    original_file_types::OriginalFileManifest,
};
use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _};

pub(super) fn already_ingested_update_payload(
    document_id: &str,
    read_only: bool,
    parent_id_override: Option<String>,
) -> Option<serde_json::Value> {
    if read_only && parent_id_override.is_none() {
        return None;
    }
    let mut payload = serde_json::json!({
        "documentId": document_id,
    });
    if let Some(object) = payload.as_object_mut() {
        if !read_only {
            object.insert("readOnly".to_string(), serde_json::Value::Bool(false));
        }
        if let Some(parent_id) = parent_id_override {
            object.insert("parentId".to_string(), serde_json::Value::String(parent_id));
        }
    }
    Some(payload)
}

pub(super) fn artifact_upload_ingest_payload(
    artifact_id: &str,
    artifact: &serde_json::Value,
    manifest: &OriginalFileManifest,
    bytes: &[u8],
    requested_title: Option<String>,
    parent_id_override: Option<String>,
) -> serde_json::Value {
    let title = artifact_import_document_title(artifact, requested_title);
    let parent_id = parent_id_override.or_else(|| json_string(artifact.get("parentId")));
    let file_type = json_string(artifact.get("fileType"));
    let storage_key = json_string(artifact.get("storageKey")).unwrap_or_else(|| {
        format!(
            "local://artifacts/{artifact_id}/original/{}",
            manifest.filename
        )
    });

    serde_json::json!({
        "filename": manifest.filename,
        "mimeType": manifest.mime_type,
        "sizeBytes": bytes.len(),
        "dataBase64": BASE64_STANDARD.encode(bytes),
        "parentId": parent_id,
        "sourceFile": {
            "storageKey": storage_key,
            "originalFilename": manifest.filename,
            "mimeType": manifest.mime_type,
            "sizeBytes": bytes.len(),
            "fileType": file_type,
        },
        "title": title,
    })
}

pub(super) fn editable_document_payload(document_id: &str) -> serde_json::Value {
    serde_json::json!({
        "documentId": document_id,
        "readOnly": false,
    })
}

pub(super) fn artifact_ingested_payload(
    artifact_id: &str,
    artifact: &serde_json::Value,
    manifest: &OriginalFileManifest,
    size_bytes: usize,
    document_id: &str,
    keep_ingested_link: bool,
    mark_artifact_ingested: bool,
) -> serde_json::Value {
    serde_json::json!({
        "artifactId": artifact_id,
        "label": json_string(artifact.get("label")),
        "parentId": json_string(artifact.get("parentId")),
        "order": artifact.get("order").cloned().unwrap_or(serde_json::Value::Null),
        "fileType": json_string(artifact.get("fileType")),
        "status": if mark_artifact_ingested {
            serde_json::Value::String("ingested".to_string())
        } else {
            artifact
                .get("status")
                .cloned()
                .unwrap_or_else(|| serde_json::Value::String("ready".to_string()))
        },
        "storageKey": json_string(artifact.get("storageKey")),
        "originalFilename": manifest.filename,
        "mimeType": manifest.mime_type,
        "sizeBytes": size_bytes,
        "ingestedDocId": if keep_ingested_link {
            serde_json::Value::String(document_id.to_string())
        } else {
            serde_json::Value::Null
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_manifest() -> OriginalFileManifest {
        OriginalFileManifest {
            source_filename: None,
            filename: "paper.pdf".to_string(),
            mime_type: "application/pdf".to_string(),
            size_bytes: 3,
            local_path: "/tmp/paper.pdf".to_string(),
            created_at: "1".to_string(),
            updated_at: "2".to_string(),
        }
    }

    #[test]
    fn already_ingested_update_payload_only_emits_needed_mutations() {
        assert!(
            already_ingested_update_payload("doc-a", true, None).is_none(),
            "read-only already-ingested documents do not need a workspace mutation"
        );

        let import_payload =
            already_ingested_update_payload("doc-a", false, Some("folder-a".to_string()))
                .expect("import payload");
        assert_eq!(import_payload["documentId"], "doc-a");
        assert_eq!(import_payload["readOnly"], false);
        assert_eq!(import_payload["parentId"], "folder-a");
    }

    #[test]
    fn artifact_upload_ingest_payload_preserves_source_file_contract() {
        let artifact = serde_json::json!({
            "label": "Ignored.pdf",
            "parentId": "folder-a",
            "fileType": "pdf"
        });
        let payload = artifact_upload_ingest_payload(
            "artifact-a",
            &artifact,
            &test_manifest(),
            b"abc",
            Some("Requested.pdf".to_string()),
            Some("folder-b".to_string()),
        );

        assert_eq!(payload["title"], "Requested");
        assert_eq!(payload["dataBase64"], "YWJj");
        assert_eq!(payload["parentId"], "folder-b");
        assert_eq!(
            payload["sourceFile"]["storageKey"],
            "local://artifacts/artifact-a/original/paper.pdf"
        );
        assert_eq!(payload["sourceFile"]["fileType"], "pdf");
    }

    #[test]
    fn artifact_ingested_payload_controls_status_and_document_link() {
        let artifact = serde_json::json!({
            "label": "Paper",
            "parentId": "folder-a",
            "order": 3,
            "fileType": "pdf",
            "status": "ready",
            "storageKey": "local://artifact"
        });
        let payload = artifact_ingested_payload(
            "artifact-a",
            &artifact,
            &test_manifest(),
            3,
            "doc-a",
            false,
            false,
        );

        assert_eq!(payload["status"], "ready");
        assert!(payload["ingestedDocId"].is_null());
        assert_eq!(payload["storageKey"], "local://artifact");

        let marked = artifact_ingested_payload(
            "artifact-a",
            &artifact,
            &test_manifest(),
            3,
            "doc-a",
            true,
            true,
        );
        assert_eq!(marked["status"], "ingested");
        assert_eq!(marked["ingestedDocId"], "doc-a");
    }
}
