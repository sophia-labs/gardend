use crate::json_utils::json_string;

pub(super) fn artifact_import_document_title(
    artifact: &serde_json::Value,
    requested_title: Option<String>,
) -> String {
    let mut title = requested_title
        .or_else(|| json_string(artifact.get("label")))
        .or_else(|| json_string(artifact.get("originalFilename")))
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "Imported Document".to_string());
    if let Some(file_type) =
        json_string(artifact.get("fileType")).filter(|value| !value.trim().is_empty())
    {
        let suffix = format!(".{file_type}");
        if title.ends_with(&suffix) {
            title.truncate(title.len().saturating_sub(suffix.len()));
        }
    }
    title
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn artifact_import_document_title_prefers_requested_title_and_strips_file_type_suffix() {
        let artifact = serde_json::json!({
            "label": "Label wins never.pdf",
            "originalFilename": "original.pdf",
            "fileType": "pdf",
        });

        assert_eq!(
            artifact_import_document_title(&artifact, Some("Requested.pdf".to_string())),
            "Requested"
        );
    }

    #[test]
    fn artifact_import_document_title_falls_back_to_label_then_original_filename() {
        let with_label = serde_json::json!({
            "label": "Paper.pdf",
            "originalFilename": "original.pdf",
            "fileType": "pdf",
        });
        let with_original = serde_json::json!({
            "originalFilename": "scan.txt",
            "fileType": "txt",
        });

        assert_eq!(artifact_import_document_title(&with_label, None), "Paper");
        assert_eq!(artifact_import_document_title(&with_original, None), "scan");
    }

    #[test]
    fn artifact_import_document_title_uses_default_for_empty_metadata_or_blank_label() {
        let blank_label = serde_json::json!({
            "label": "   ",
            "originalFilename": "scan.txt",
            "fileType": "txt",
        });

        assert_eq!(
            artifact_import_document_title(&serde_json::json!({}), None),
            "Imported Document"
        );
        assert_eq!(
            artifact_import_document_title(&blank_label, None),
            "Imported Document"
        );
    }
}
