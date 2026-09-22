use crate::{
    pdf_pipeline_catalog::{PdfEngineRuntimeKind, PdfEngineSpec},
    pdf_pipeline_python_runtime::pdf_python_module_or_uv_runtime_state,
    pdf_runtimes::DoclingRuntimeStatus,
    process_utils::executable_path,
    storage::display_path,
};

pub(super) struct PdfEngineRuntimeState {
    pub(super) status: String,
    pub(super) runtime_available: bool,
    pub(super) setup_required: bool,
    pub(super) reason: Option<String>,
    pub(super) setup_hint: Option<String>,
}

pub(super) fn pdf_engine_runtime_state(
    spec: &PdfEngineSpec,
    docling_status: &DoclingRuntimeStatus,
) -> PdfEngineRuntimeState {
    match spec.runtime_kind {
        PdfEngineRuntimeKind::BuiltIn => PdfEngineRuntimeState {
            status: "available".to_string(),
            runtime_available: true,
            setup_required: false,
            reason: None,
            setup_hint: None,
        },
        PdfEngineRuntimeKind::Docling => {
            let setup_hint = if docling_status.available {
                None
            } else if docling_status.supported {
                Some("Prepare Docling to enable high-fidelity local PDF conversion.".to_string())
            } else {
                Some(
                    "Set MNEMOSYNE_DOCLING_PYTHON to a compatible Python with Docling installed to override the platform gate."
                        .to_string(),
                )
            };
            PdfEngineRuntimeState {
                status: docling_status.status.clone(),
                runtime_available: docling_status.available,
                setup_required: docling_status.setup_required,
                reason: docling_status.reason.clone(),
                setup_hint,
            }
        }
        PdfEngineRuntimeKind::PythonModuleOrUv {
            env_var,
            module_name,
            command_candidates,
            uv_packages,
        } => pdf_python_module_or_uv_runtime_state(
            env_var,
            module_name,
            command_candidates,
            uv_packages,
        ),
        PdfEngineRuntimeKind::CliCommands { commands } => pdf_cli_runtime_state(commands),
        PdfEngineRuntimeKind::ServiceUrl { env_vars } => pdf_service_runtime_state(env_vars),
    }
}

fn pdf_cli_runtime_state(commands: &[&str]) -> PdfEngineRuntimeState {
    let found = commands
        .iter()
        .filter_map(|command| executable_path(command).map(|path| (*command, display_path(&path))))
        .collect::<Vec<_>>();
    let missing = commands
        .iter()
        .filter(|command| !found.iter().any(|(name, _)| name == *command))
        .copied()
        .collect::<Vec<_>>();
    if missing.is_empty() {
        return PdfEngineRuntimeState {
            status: "runtime-ready".to_string(),
            runtime_available: true,
            setup_required: false,
            reason: Some(format!(
                "Found {}.",
                found
                    .iter()
                    .map(|(name, path)| format!("{name} at {path}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
            setup_hint: None,
        };
    }
    PdfEngineRuntimeState {
        status: "setup-required".to_string(),
        runtime_available: false,
        setup_required: true,
        reason: Some(format!(
            "Missing required commands on PATH: {}.",
            missing.join(", ")
        )),
        setup_hint: Some(format!(
            "Install {} and make them available on PATH.",
            missing.join(" and ")
        )),
    }
}

fn pdf_service_runtime_state(env_vars: &[&str]) -> PdfEngineRuntimeState {
    if let Some((env_var, value)) = env_vars.iter().find_map(|env_var| {
        std::env::var(env_var)
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .map(|value| (*env_var, value))
    }) {
        return PdfEngineRuntimeState {
            status: "runtime-configured".to_string(),
            runtime_available: true,
            setup_required: false,
            reason: Some(format!("{env_var} is configured as {value}.")),
            setup_hint: None,
        };
    }
    PdfEngineRuntimeState {
        status: "setup-required".to_string(),
        runtime_available: false,
        setup_required: true,
        reason: Some(format!(
            "No service URL configured in {}.",
            env_vars.join(" or ")
        )),
        setup_hint: Some(format!(
            "Run the service locally and set {}.",
            env_vars.first().copied().unwrap_or("the service URL")
        )),
    }
}
