use crate::app_runtime::AppHandle;
use crate::runtime_config::{LOOPBACK_MANIFEST_FILE, PROFILE_ID};
use std::path::PathBuf;
#[cfg(feature = "desktop")]
use tauri::Manager;

pub(crate) fn profile_dir(app: &AppHandle) -> Result<PathBuf, String> {
    // Headless/container override (also handy for desktop debugging):
    // GARDEN_PROFILE_DIR points directly at the profile directory.
    if let Ok(dir) = std::env::var("GARDEN_PROFILE_DIR") {
        let trimmed = dir.trim();
        if !trimmed.is_empty() {
            return Ok(PathBuf::from(trimmed));
        }
    }
    Ok(app
        .path()
        .app_data_dir()
        .map_err(|error| format!("resolve app data dir: {error}"))?
        .join("profiles")
        .join(PROFILE_ID))
}

pub(crate) fn graphs_dir(app: &AppHandle) -> Result<PathBuf, String> {
    Ok(profile_dir(app)?.join("graphs"))
}

pub(crate) fn jobs_dir(app: &AppHandle) -> Result<PathBuf, String> {
    Ok(profile_dir(app)?.join("jobs"))
}

pub(crate) fn loopback_manifest_path(app: &AppHandle) -> Result<PathBuf, String> {
    Ok(profile_dir(app)?.join(LOOPBACK_MANIFEST_FILE))
}

pub(crate) fn loopback_client_tokens_path(app: &AppHandle) -> Result<PathBuf, String> {
    Ok(profile_dir(app)?.join("loopback-client-tokens.json"))
}

pub(crate) fn loopback_audit_log_path(app: &AppHandle) -> Result<PathBuf, String> {
    Ok(profile_dir(app)?.join("loopback-audit.jsonl"))
}
