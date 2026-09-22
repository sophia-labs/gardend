use crate::app_runtime::AppHandle;
use crate::{
    paths::profile_dir,
    runtime_config::{DOCLING_SETUP_FILE, PDF_PIPELINE_CONFIG_FILE, SEMANTIC_MODEL_CONFIG_FILE},
};
use std::path::PathBuf;

pub(crate) fn semantic_model_setup_dir(app: &AppHandle) -> Result<PathBuf, String> {
    Ok(profile_dir(app)?.join("models/semantic"))
}

pub(crate) fn semantic_model_config_path(app: &AppHandle) -> Result<PathBuf, String> {
    Ok(semantic_model_setup_dir(app)?.join(SEMANTIC_MODEL_CONFIG_FILE))
}

pub(crate) fn semantic_model_setup_path(
    app: &AppHandle,
    model_id: &str,
) -> Result<PathBuf, String> {
    Ok(semantic_model_setup_dir(app)?.join(format!("{}.json", semantic_model_file_stem(model_id))))
}

pub(crate) fn legacy_semantic_model_setup_path(app: &AppHandle) -> Result<PathBuf, String> {
    Ok(semantic_model_setup_dir(app)?.join("model.json"))
}

pub(crate) fn docling_runtime_setup_dir(app: &AppHandle) -> Result<PathBuf, String> {
    Ok(profile_dir(app)?.join("models/docling"))
}

pub(crate) fn docling_runtime_setup_path(app: &AppHandle) -> Result<PathBuf, String> {
    Ok(docling_runtime_setup_dir(app)?.join(DOCLING_SETUP_FILE))
}

pub(crate) fn pdf_pipeline_setup_dir(app: &AppHandle) -> Result<PathBuf, String> {
    Ok(profile_dir(app)?.join("models/pdf"))
}

pub(crate) fn pdf_pipeline_config_path(app: &AppHandle) -> Result<PathBuf, String> {
    Ok(pdf_pipeline_setup_dir(app)?.join(PDF_PIPELINE_CONFIG_FILE))
}

fn semantic_model_file_stem(model_id: &str) -> String {
    let mut stem = model_id
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect::<String>();
    while stem.contains("__") {
        stem = stem.replace("__", "_");
    }
    stem.trim_matches('_').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn semantic_model_file_stems_are_stable_and_path_safe() {
        assert_eq!(
            semantic_model_file_stem("fastembed/qdrant/bge-small-en-v1.5-onnx-q"),
            "fastembed_qdrant_bge-small-en-v1_5-onnx-q"
        );
    }
}
