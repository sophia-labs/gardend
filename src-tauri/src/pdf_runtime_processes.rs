use crate::storage::display_path;
use std::{
    path::{Path, PathBuf},
    process,
};

pub(crate) const PYMUPDF4LLM_RUNTIME_ID: &str = "pymupdf4llm-python";
pub(crate) const PYMUPDF4LLM_PACKAGE_REQUIREMENT: &str = "pymupdf4llm>=0.3.4";
pub(crate) const PYMUPDF_PACKAGE_REQUIREMENT: &str = "pymupdf>=1.24.0";

pub(crate) fn uv_cache_path_hint() -> String {
    if let Ok(cache_dir) = std::env::var("UV_CACHE_DIR") {
        return display_path(Path::new(&cache_dir));
    }
    if let Ok(home) = std::env::var("HOME") {
        return display_path(&Path::new(&home).join(".cache/uv"));
    }
    "uv default cache".to_string()
}

pub(crate) fn local_platform_label() -> String {
    format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH)
}

fn repo_root_hint() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")))
}

pub(crate) fn docling_helper_path_hint() -> String {
    display_path(&repo_root_hint().join("scripts/docling_pdf_to_markdown.py"))
}

pub(crate) fn pymupdf4llm_helper_path_hint() -> String {
    display_path(&repo_root_hint().join("scripts/pymupdf4llm_pdf_to_markdown.py"))
}

pub(crate) fn docling_python_command() -> process::Command {
    if let Ok(python) = std::env::var("MNEMOSYNE_DOCLING_PYTHON") {
        process::Command::new(python)
    } else {
        let mut command = process::Command::new("uv");
        command
            .arg("run")
            .arg("--extra")
            .arg("pdf-accurate")
            .arg("python")
            .current_dir(repo_root_hint());
        command
    }
}

pub(crate) fn pymupdf4llm_python_command() -> process::Command {
    if let Ok(python) = std::env::var("MNEMOSYNE_PYMUPDF_PYTHON") {
        process::Command::new(python)
    } else {
        let mut command = process::Command::new("uv");
        command
            .arg("run")
            .arg("--no-project")
            .arg("--python")
            .arg(">=3.10")
            .arg("--with")
            .arg(PYMUPDF4LLM_PACKAGE_REQUIREMENT)
            .arg("--with")
            .arg(PYMUPDF_PACKAGE_REQUIREMENT)
            .arg("python")
            .current_dir(repo_root_hint());
        command
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_helper_paths_point_at_packaged_scripts() {
        assert!(docling_helper_path_hint().ends_with("scripts/docling_pdf_to_markdown.py"));
        assert!(pymupdf4llm_helper_path_hint().ends_with("scripts/pymupdf4llm_pdf_to_markdown.py"));
    }

    #[test]
    fn local_platform_label_contains_os_and_arch() {
        let label = local_platform_label();

        assert!(label.contains(std::env::consts::OS));
        assert!(label.contains(std::env::consts::ARCH));
    }
}
