use crate::app_runtime::AppHandle;
use crate::{json_utils::json_string, original_file_service::save_artifact_original_file};

pub(super) fn normalize_entity_folder_payload(folder_id: &str, input: &mut serde_json::Value) {
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
        if !object.contains_key("label") {
            if let Some(name) = object
                .get("name")
                .and_then(|value| json_string(Some(value)))
            {
                object.insert("label".to_string(), serde_json::Value::String(name));
            }
        }
    }
}

pub(super) fn require_hosted_entity_string(
    input: &serde_json::Value,
    keys: &[&str],
    field_name: &str,
) -> Result<(), String> {
    let present = keys.iter().any(|key| {
        input
            .get(*key)
            .and_then(serde_json::Value::as_str)
            .is_some()
    });
    if present {
        Ok(())
    } else {
        Err(format!("{field_name} is required"))
    }
}

pub(super) fn normalize_entity_artifact_payload(
    app: &AppHandle,
    graph_id: &str,
    artifact_id: &str,
    input: &mut serde_json::Value,
) -> Result<(), String> {
    if let Some(object) = input.as_object_mut() {
        object.insert(
            "artifactId".to_string(),
            serde_json::Value::String(artifact_id.to_string()),
        );
        object.insert(
            "id".to_string(),
            serde_json::Value::String(artifact_id.to_string()),
        );
        // A full replace, never the Files PATCH merge (`patch` is the cell's
        // internal marker for `workspace.putArtifact`).
        object.remove("patch");
        let data_base64 = object
            .remove("dataBase64")
            .or_else(|| object.remove("data_base64"))
            .and_then(|value| json_string(Some(&value)));
        if let Some(data_base64) = data_base64 {
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
            let manifest = save_artifact_original_file(
                app,
                graph_id,
                artifact_id,
                &filename,
                &mime_type,
                &data_base64,
            )?;
            object.insert(
                "storageKey".to_string(),
                serde_json::Value::String(format!(
                    "local://artifacts/{artifact_id}/original/{}",
                    manifest.filename
                )),
            );
            object.insert(
                "originalFilename".to_string(),
                serde_json::Value::String(manifest.filename),
            );
            object.insert(
                "mimeType".to_string(),
                serde_json::Value::String(manifest.mime_type),
            );
            object.insert(
                "sizeBytes".to_string(),
                serde_json::json!(manifest.size_bytes),
            );
            object
                .entry("status".to_string())
                .or_insert_with(|| serde_json::Value::String("ready".to_string()));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folder_payload_normalization_preserves_label_name_aliases() {
        let mut from_label = serde_json::json!({ "label": "Folder A" });
        normalize_entity_folder_payload("folder-a", &mut from_label);
        assert_eq!(from_label["id"], "folder-a");
        assert_eq!(from_label["folderId"], "folder-a");
        assert_eq!(from_label["name"], "Folder A");
        assert_eq!(from_label["label"], "Folder A");

        let mut from_name = serde_json::json!({ "name": "Folder B" });
        normalize_entity_folder_payload("folder-b", &mut from_name);
        assert_eq!(from_name["id"], "folder-b");
        assert_eq!(from_name["folderId"], "folder-b");
        assert_eq!(from_name["name"], "Folder B");
        assert_eq!(from_name["label"], "Folder B");
    }

    #[test]
    fn hosted_entity_string_requirement_reports_contract_field_name() {
        let input = serde_json::json!({
            "original_filename": "paper.pdf"
        });

        assert!(require_hosted_entity_string(
            &input,
            &["originalFilename", "original_filename"],
            "originalFilename"
        )
        .is_ok());
        assert_eq!(
            require_hosted_entity_string(&input, &["label"], "label").unwrap_err(),
            "label is required"
        );
    }
}
