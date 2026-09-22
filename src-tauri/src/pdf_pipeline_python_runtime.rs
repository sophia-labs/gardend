use crate::{
    pdf_pipeline_runtime::PdfEngineRuntimeState,
    process_utils::{command_output_summary, command_output_with_timeout, executable_path},
    storage::display_path,
};
use std::{process, time::Duration};

fn pdf_python_module_runtime_state(
    env_var: &str,
    module_name: &str,
    command_candidates: &[&str],
) -> PdfEngineRuntimeState {
    let configured_python = std::env::var(env_var)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    let python = configured_python.clone().or_else(|| {
        command_candidates
            .iter()
            .find_map(|command| executable_path(command).map(|path| display_path(&path)))
    });
    let Some(python) = python else {
        return PdfEngineRuntimeState {
            status: "setup-required".to_string(),
            runtime_available: false,
            setup_required: true,
            reason: Some(format!(
                "No Python executable found; set {env_var} to a Python with {module_name} installed."
            )),
            setup_hint: Some(format!(
                "Install {module_name} and set {env_var} if it is not on the default Python path."
            )),
        };
    };

    let probe = format!(
        "import importlib.util, sys; sys.exit(0 if importlib.util.find_spec({module_name:?}) else 1)"
    );
    let mut command = process::Command::new(&python);
    command.arg("-c").arg(probe);
    match command_output_with_timeout(
        &mut command,
        &format!("probe Python module {module_name}"),
        Duration::from_secs(2),
    ) {
        Ok(output) if output.status.success() => PdfEngineRuntimeState {
            status: "runtime-ready".to_string(),
            runtime_available: true,
            setup_required: false,
            reason: Some(format!("{module_name} is importable via {python}.")),
            setup_hint: None,
        },
        Ok(output) => {
            let details = command_output_summary(&output);
            PdfEngineRuntimeState {
                status: "setup-required".to_string(),
                runtime_available: false,
                setup_required: true,
                reason: Some(if details.is_empty() {
                    format!("{module_name} is not importable via {python}.")
                } else {
                    format!("{module_name} is not importable via {python}: {details}")
                }),
                setup_hint: Some(format!(
                    "Install {module_name} in {python}, or set {env_var} to a prepared Python."
                )),
            }
        }
        Err(error) => PdfEngineRuntimeState {
            status: "setup-required".to_string(),
            runtime_available: false,
            setup_required: true,
            reason: Some(error),
            setup_hint: Some(format!(
                "Install {module_name} in a local Python and set {env_var} if needed."
            )),
        },
    }
}

pub(super) fn pdf_python_module_or_uv_runtime_state(
    env_var: &str,
    module_name: &str,
    command_candidates: &[&str],
    uv_packages: &[&str],
) -> PdfEngineRuntimeState {
    if std::env::var(env_var)
        .ok()
        .map(|value| !value.trim().is_empty())
        .unwrap_or(false)
    {
        return pdf_python_module_runtime_state(env_var, module_name, command_candidates);
    }

    let module_status = pdf_python_module_runtime_state(env_var, module_name, command_candidates);
    if module_status.runtime_available {
        return module_status;
    }

    if let Some(uv_path) = executable_path("uv") {
        return PdfEngineRuntimeState {
            status: "uv-runtime-ready".to_string(),
            runtime_available: true,
            setup_required: false,
            reason: Some(format!(
                "uv is available at {}; {} will be provisioned with {} on first run.",
                display_path(&uv_path),
                module_name,
                uv_packages.join(", ")
            )),
            setup_hint: None,
        };
    }

    PdfEngineRuntimeState {
        status: "setup-required".to_string(),
        runtime_available: false,
        setup_required: true,
        reason: module_status.reason.or_else(|| {
            Some(format!(
                "{module_name} is not importable and uv is not available to provision it."
            ))
        }),
        setup_hint: Some(format!(
            "Install uv, or set {env_var} to a Python with {} installed.",
            uv_packages.join(" and ")
        )),
    }
}
