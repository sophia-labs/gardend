use crate::app_runtime::AppHandle;
pub(crate) use crate::pdf_docling_runtime_manifest::write_docling_runtime_setup_manifest;
pub(crate) use crate::pdf_runtime_processes::{
    docling_helper_path_hint, docling_python_command, local_platform_label,
    pymupdf4llm_helper_path_hint, pymupdf4llm_python_command, uv_cache_path_hint,
    PYMUPDF4LLM_PACKAGE_REQUIREMENT, PYMUPDF4LLM_RUNTIME_ID, PYMUPDF_PACKAGE_REQUIREMENT,
};
use crate::{
    model_setup_paths::docling_runtime_setup_path,
    pdf_docling_runtime_manifest::{
        docling_runtime_manifest_matches, read_docling_runtime_setup_manifest, DOCLING_APPROACH_ID,
        DOCLING_CACHE_POLICY, DOCLING_PACKAGE_NAME, DOCLING_PACKAGE_REQUIREMENT,
        DOCLING_RUNTIME_ID,
    },
    storage::display_path,
};
use serde::Serialize;

#[derive(Debug, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DoclingRuntimeStatus {
    pub(crate) runtime_id: String,
    pub(crate) approach_id: String,
    pub(crate) package_name: String,
    pub(crate) package_requirement: String,
    pub(crate) prepared: bool,
    pub(crate) available: bool,
    pub(crate) supported: bool,
    pub(crate) setup_required: bool,
    pub(crate) setup_path: String,
    pub(crate) cache_policy: String,
    pub(crate) cache_path: String,
    pub(crate) helper_path: String,
    pub(crate) python_executable: Option<String>,
    pub(crate) docling_version: Option<String>,
    pub(crate) prepared_at: Option<String>,
    pub(crate) platform: String,
    pub(crate) status: String,
    pub(crate) reason: Option<String>,
    pub(crate) limitations: Vec<String>,
}

fn docling_platform_support() -> (bool, Option<String>) {
    if std::env::var("MNEMOSYNE_DOCLING_PYTHON").is_ok() {
        return (true, None);
    }
    if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
        return (
            false,
            Some(
                "current pdf-accurate lock resolves a torch wheel set that is unavailable on macOS x86_64"
                    .to_string(),
            ),
        );
    }
    (true, None)
}

pub(crate) fn docling_runtime_status(app: &AppHandle) -> Result<DoclingRuntimeStatus, String> {
    let setup_path = docling_runtime_setup_path(app)?;
    let manifest = read_docling_runtime_setup_manifest(app)?;
    let prepared = manifest
        .as_ref()
        .is_some_and(docling_runtime_manifest_matches);
    let (platform_supported, platform_reason) = docling_platform_support();
    let supported = platform_supported;
    let available = prepared && supported;
    let setup_required = supported && !prepared;
    let status = if available {
        "available"
    } else if !supported {
        "unsupported"
    } else {
        "setup-required"
    };
    let reason = if available {
        None
    } else if let Some(reason) = platform_reason {
        Some(reason)
    } else {
        Some("Docling runtime has not been prepared in this local profile".to_string())
    };
    let mut limitations = Vec::new();
    if let Some(reason) = reason.as_deref() {
        limitations.push(reason.to_string());
    }
    if !available {
        limitations.push(
            "pdf.docling-accurate continues to use the explicit pdf.fast-text fallback until this runtime is available"
                .to_string(),
        );
    }

    Ok(DoclingRuntimeStatus {
        runtime_id: DOCLING_RUNTIME_ID.to_string(),
        approach_id: DOCLING_APPROACH_ID.to_string(),
        package_name: DOCLING_PACKAGE_NAME.to_string(),
        package_requirement: DOCLING_PACKAGE_REQUIREMENT.to_string(),
        prepared,
        available,
        supported,
        setup_required,
        setup_path: display_path(&setup_path),
        cache_policy: DOCLING_CACHE_POLICY.to_string(),
        cache_path: uv_cache_path_hint(),
        helper_path: docling_helper_path_hint(),
        python_executable: if prepared {
            manifest
                .as_ref()
                .map(|value| value.python_executable.clone())
        } else {
            std::env::var("MNEMOSYNE_DOCLING_PYTHON").ok()
        },
        docling_version: if prepared {
            manifest
                .as_ref()
                .and_then(|value| value.docling_version.clone())
        } else {
            None
        },
        prepared_at: if prepared {
            manifest.as_ref().map(|value| value.prepared_at.clone())
        } else {
            None
        },
        platform: local_platform_label(),
        status: status.to_string(),
        reason,
        limitations,
    })
}
