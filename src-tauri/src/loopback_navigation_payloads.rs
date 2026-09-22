use crate::{json_utils::json_string, original_file_types::OriginalFileManifest};

pub(crate) struct NavigationArtifactOriginalUpload {
    pub(crate) filename: String,
    pub(crate) mime_type: String,
    pub(crate) data_base64: String,
}

pub(crate) fn normalize_navigation_folder_payload(
    mut input: serde_json::Value,
    folder_id: &str,
) -> serde_json::Value {
    if let Some(object) = input.as_object_mut() {
        object.insert(
            "folderId".to_string(),
            serde_json::Value::String(folder_id.to_string()),
        );
        object.insert(
            "id".to_string(),
            serde_json::Value::String(folder_id.to_string()),
        );
        if !object.contains_key("name") {
            if let Some(label) = object
                .get("label")
                .and_then(|value| json_string(Some(value)))
            {
                object.insert("name".to_string(), serde_json::Value::String(label));
            }
        }
    }
    input
}

pub(crate) fn normalize_navigation_artifact_payload(
    mut input: serde_json::Value,
    artifact_id: &str,
) -> (serde_json::Value, Option<NavigationArtifactOriginalUpload>) {
    let Some(object) = input.as_object_mut() else {
        return (input, None);
    };
    object.insert(
        "artifactId".to_string(),
        serde_json::Value::String(artifact_id.to_string()),
    );
    object.insert(
        "id".to_string(),
        serde_json::Value::String(artifact_id.to_string()),
    );
    let data_base64 = object
        .remove("dataBase64")
        .or_else(|| object.remove("data_base64"))
        .and_then(|value| json_string(Some(&value)));
    let Some(data_base64) = data_base64 else {
        return (input, None);
    };
    let label = json_string(
        object
            .get("label")
            .or_else(|| object.get("name"))
            .or_else(|| object.get("title")),
    )
    .unwrap_or_else(|| artifact_id.to_string());
    let filename = json_string(
        object
            .get("originalFilename")
            .or_else(|| object.get("original_filename")),
    )
    .unwrap_or(label);
    let mime_type = json_string(object.get("mimeType").or_else(|| object.get("mime_type")))
        .unwrap_or_else(|| "application/octet-stream".to_string());
    (
        input,
        Some(NavigationArtifactOriginalUpload {
            filename,
            mime_type,
            data_base64,
        }),
    )
}

pub(crate) fn apply_artifact_original_manifest(
    input: &mut serde_json::Value,
    artifact_id: &str,
    manifest: &OriginalFileManifest,
) {
    let Some(object) = input.as_object_mut() else {
        return;
    };
    object.insert(
        "storageKey".to_string(),
        serde_json::Value::String(format!(
            "local://artifacts/{artifact_id}/original/{}",
            manifest.filename
        )),
    );
    object.insert(
        "originalFilename".to_string(),
        serde_json::Value::String(manifest.filename.clone()),
    );
    object.insert(
        "mimeType".to_string(),
        serde_json::Value::String(manifest.mime_type.clone()),
    );
    object.insert(
        "sizeBytes".to_string(),
        serde_json::json!(manifest.size_bytes),
    );
    object
        .entry("status".to_string())
        .or_insert_with(|| serde_json::Value::String("ready".to_string()));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folder_payload_preserves_hosted_aliases() {
        let payload =
            normalize_navigation_folder_payload(serde_json::json!({"label": "Research"}), "f-1");

        assert_eq!(
            payload.get("id").and_then(serde_json::Value::as_str),
            Some("f-1")
        );
        assert_eq!(
            payload.get("folderId").and_then(serde_json::Value::as_str),
            Some("f-1")
        );
        assert_eq!(
            payload.get("name").and_then(serde_json::Value::as_str),
            Some("Research")
        );
    }

    #[test]
    fn artifact_payload_extracts_original_file_and_applies_manifest() {
        let (mut payload, upload) = normalize_navigation_artifact_payload(
            serde_json::json!({
                "label": "Spec",
                "data_base64": "cGRm",
                "mime_type": "application/pdf"
            }),
            "a-1",
        );
        let upload = upload.expect("original upload");
        assert_eq!(upload.filename, "Spec");
        assert_eq!(upload.mime_type, "application/pdf");
        assert_eq!(upload.data_base64, "cGRm");
        assert!(payload.get("data_base64").is_none());

        apply_artifact_original_manifest(
            &mut payload,
            "a-1",
            &OriginalFileManifest {
                source_filename: None,
                filename: "spec.pdf".to_string(),
                mime_type: "application/pdf".to_string(),
                size_bytes: 12,
                local_path: "/tmp/spec.pdf".to_string(),
                created_at: "1".to_string(),
                updated_at: "2".to_string(),
            },
        );

        assert_eq!(
            payload
                .get("storageKey")
                .and_then(serde_json::Value::as_str),
            Some("local://artifacts/a-1/original/spec.pdf")
        );
        assert_eq!(
            payload.get("status").and_then(serde_json::Value::as_str),
            Some("ready")
        );
        assert_eq!(
            payload.get("sizeBytes").and_then(serde_json::Value::as_u64),
            Some(12)
        );
    }
}
