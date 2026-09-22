use crate::app_runtime::AppHandle;
use crate::{
    clock::timestamp,
    model_setup_paths::{docling_runtime_setup_dir, docling_runtime_setup_path},
    pdf_runtime_processes::local_platform_label,
    storage::{create_dir_all, read_json, write_json},
};
use serde::{Deserialize, Serialize};

pub(crate) const DOCLING_RUNTIME_SCHEMA_VERSION: u32 = 1;
pub(crate) const DOCLING_RUNTIME_ID: &str = "docling-python";
pub(crate) const DOCLING_APPROACH_ID: &str = "pdf.docling-accurate";
pub(crate) const DOCLING_PACKAGE_NAME: &str = "docling";
pub(crate) const DOCLING_PACKAGE_REQUIREMENT: &str = "docling>=2.0.0";
pub(crate) const DOCLING_CACHE_POLICY: &str = "uv-extra-pdf-accurate";

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DoclingRuntimeSetupManifest {
    pub(crate) schema_version: u32,
    pub(crate) runtime_id: String,
    pub(crate) approach_id: String,
    pub(crate) package_name: String,
    pub(crate) package_requirement: String,
    pub(crate) cache_policy: String,
    pub(crate) platform: String,
    pub(crate) python_executable: String,
    pub(crate) docling_version: Option<String>,
    pub(crate) prepared_at: String,
}

pub(crate) fn read_docling_runtime_setup_manifest(
    app: &AppHandle,
) -> Result<Option<DoclingRuntimeSetupManifest>, String> {
    let path = docling_runtime_setup_path(app)?;
    if !path.is_file() {
        return Ok(None);
    }
    read_json::<DoclingRuntimeSetupManifest>(&path)
        .map(Some)
        .map_err(Into::into)
}

pub(crate) fn docling_runtime_manifest_matches(manifest: &DoclingRuntimeSetupManifest) -> bool {
    manifest.schema_version == DOCLING_RUNTIME_SCHEMA_VERSION
        && manifest.runtime_id == DOCLING_RUNTIME_ID
        && manifest.approach_id == DOCLING_APPROACH_ID
        && manifest.package_name == DOCLING_PACKAGE_NAME
        && manifest.package_requirement == DOCLING_PACKAGE_REQUIREMENT
        && manifest.cache_policy == DOCLING_CACHE_POLICY
}

pub(crate) fn write_docling_runtime_setup_manifest(
    app: &AppHandle,
    probe: &serde_json::Value,
) -> Result<(), String> {
    let setup_dir = docling_runtime_setup_dir(app)?;
    create_dir_all(&setup_dir)?;
    let python_executable = json_string(probe.get("pythonExecutable"))
        .or_else(|| json_string(probe.get("python_executable")))
        .unwrap_or_else(|| "uv run --extra pdf-accurate python".to_string());
    let docling_version = json_string(probe.get("doclingVersion"))
        .or_else(|| json_string(probe.get("docling_version")));
    let manifest = DoclingRuntimeSetupManifest {
        schema_version: DOCLING_RUNTIME_SCHEMA_VERSION,
        runtime_id: DOCLING_RUNTIME_ID.to_string(),
        approach_id: DOCLING_APPROACH_ID.to_string(),
        package_name: DOCLING_PACKAGE_NAME.to_string(),
        package_requirement: DOCLING_PACKAGE_REQUIREMENT.to_string(),
        cache_policy: DOCLING_CACHE_POLICY.to_string(),
        platform: local_platform_label(),
        python_executable,
        docling_version,
        prepared_at: timestamp(),
    };
    write_json(&docling_runtime_setup_path(app)?, &manifest).map_err(Into::into)
}

fn json_string(value: Option<&serde_json::Value>) -> Option<String> {
    match value? {
        serde_json::Value::String(value) if !value.is_empty() => Some(value.clone()),
        serde_json::Value::Number(value) => Some(value.to_string()),
        serde_json::Value::Bool(value) => Some(value.to_string()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn docling_runtime_manifest_matches_current_contract() {
        let manifest = DoclingRuntimeSetupManifest {
            schema_version: DOCLING_RUNTIME_SCHEMA_VERSION,
            runtime_id: DOCLING_RUNTIME_ID.to_string(),
            approach_id: DOCLING_APPROACH_ID.to_string(),
            package_name: DOCLING_PACKAGE_NAME.to_string(),
            package_requirement: DOCLING_PACKAGE_REQUIREMENT.to_string(),
            cache_policy: DOCLING_CACHE_POLICY.to_string(),
            platform: local_platform_label(),
            python_executable: "/tmp/python".to_string(),
            docling_version: Some("2.0.0".to_string()),
            prepared_at: timestamp(),
        };

        assert!(docling_runtime_manifest_matches(&manifest));
    }
}
