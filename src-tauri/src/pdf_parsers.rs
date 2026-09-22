use crate::app_runtime::AppHandle;
use crate::{
    local_jobs::LocalJobRegistry,
    model_setup_paths::docling_runtime_setup_dir,
    pdf_runtimes::{
        docling_helper_path_hint, docling_python_command, docling_runtime_status,
        pymupdf4llm_helper_path_hint, pymupdf4llm_python_command,
        write_docling_runtime_setup_manifest, DoclingRuntimeStatus,
    },
    process_utils::{
        command_output_with_timeout, command_output_with_timeout_and_cancel,
        parse_json_command_output,
    },
    storage::{create_dir_all, display_path},
};
use std::{path::Path, time::Duration};

fn run_docling_runtime_probe(timeout: Duration) -> Result<serde_json::Value, String> {
    let probe = r#"
import importlib.metadata
import json
import sys
from docling.document_converter import DocumentConverter
from docling.datamodel.base_models import DocumentStream

print(json.dumps({
    "doclingVersion": importlib.metadata.version("docling"),
    "pythonExecutable": sys.executable,
}))
"#;
    let mut command = docling_python_command();
    command.arg("-c").arg(probe);
    let output = command_output_with_timeout(&mut command, "Docling runtime probe", timeout)?;
    parse_json_command_output(output, "Docling runtime probe")
}

pub(crate) fn prepare_docling_runtime_impl(
    app: &AppHandle,
) -> Result<DoclingRuntimeStatus, String> {
    let status = docling_runtime_status(app)?;
    if status.available || !status.supported {
        return Ok(status);
    }
    let probe = run_docling_runtime_probe(Duration::from_secs(180))?;
    write_docling_runtime_setup_manifest(app, &probe)?;
    docling_runtime_status(app)
}

pub(crate) fn run_docling_pdf_to_markdown(
    app: &AppHandle,
    input_path: &Path,
    filename: &str,
    title: Option<&str>,
    timeout: Duration,
) -> Result<serde_json::Value, String> {
    let helper_path = docling_helper_path_hint();
    let helper = Path::new(&helper_path);
    if !helper.is_file() {
        return Err(format!(
            "Docling helper script is missing: {}",
            display_path(helper)
        ));
    }
    let rapidocr_cache = docling_runtime_setup_dir(app)?.join("rapidocr-models");
    create_dir_all(&rapidocr_cache)?;
    let mut command = docling_python_command();
    command
        .arg(helper)
        .arg("--input")
        .arg(input_path)
        .arg("--filename")
        .arg(filename)
        .arg("--rapidocr-cache")
        .arg(&rapidocr_cache);
    if let Some(title) = title.filter(|value| !value.trim().is_empty()) {
        command.arg("--title").arg(title);
    }
    let output = command_output_with_timeout(&mut command, "Docling PDF conversion", timeout)?;
    parse_json_command_output(output, "Docling PDF conversion")
}

pub(crate) fn run_pymupdf4llm_pdf_to_markdown(
    input_path: &Path,
    filename: &str,
    title: Option<&str>,
    timeout: Duration,
    jobs: &LocalJobRegistry,
    job_id: &str,
) -> Result<serde_json::Value, String> {
    let helper_path = pymupdf4llm_helper_path_hint();
    let helper = Path::new(&helper_path);
    if !helper.is_file() {
        return Err(format!(
            "PyMuPDF4LLM helper script is missing: {}",
            display_path(helper)
        ));
    }
    let mut command = pymupdf4llm_python_command();
    command
        .arg(helper)
        .arg("--input")
        .arg(input_path)
        .arg("--filename")
        .arg(filename);
    if let Some(title) = title.filter(|value| !value.trim().is_empty()) {
        command.arg("--title").arg(title);
    }
    let output = command_output_with_timeout_and_cancel(
        &mut command,
        "PyMuPDF4LLM PDF conversion",
        timeout,
        jobs,
        job_id,
    )?;
    parse_json_command_output(output, "PyMuPDF4LLM PDF conversion")
}
