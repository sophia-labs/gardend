use crate::app_runtime::AppHandle;
use crate::{
    paths::{cleanup_pending_upload_file, existing_graph_dir, validate_pending_upload_path},
    profile_paths::profile_dir,
    storage::{display_path, read_bytes},
};
use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _};
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PendingUploadFileInput {
    #[serde(default)]
    pub(crate) graph_id: Option<String>,
    pub(crate) pending_path: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PendingUploadFileRecord {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) graph_id: Option<String>,
    pub(crate) pending_path: String,
    pub(crate) size_bytes: usize,
    pub(crate) data_base64: String,
}

#[cfg_attr(feature = "desktop", tauri::command)]
pub(super) fn read_pending_upload_file(
    app: AppHandle,
    input: PendingUploadFileInput,
) -> Result<PendingUploadFileRecord, String> {
    let pending_path =
        resolved_pending_upload_path(&app, input.graph_id.as_deref(), &input.pending_path)?;
    let bytes = read_bytes(&pending_path)?;

    Ok(PendingUploadFileRecord {
        graph_id: input.graph_id,
        pending_path: display_path(&pending_path),
        size_bytes: bytes.len(),
        data_base64: BASE64_STANDARD.encode(bytes),
    })
}

#[cfg_attr(feature = "desktop", tauri::command)]
pub(super) fn cleanup_pending_upload(
    app: AppHandle,
    input: PendingUploadFileInput,
) -> Result<(), String> {
    let pending_path =
        resolved_pending_upload_path(&app, input.graph_id.as_deref(), &input.pending_path)?;
    cleanup_pending_upload_file(&pending_path);
    Ok(())
}

pub(crate) fn resolved_pending_upload_path(
    app: &AppHandle,
    graph_id: Option<&str>,
    pending_path: &str,
) -> Result<std::path::PathBuf, String> {
    let pending_root = pending_upload_root(app, graph_id)?;
    validate_pending_upload_path(&pending_root, pending_path)
}

fn pending_upload_root(
    app: &AppHandle,
    graph_id: Option<&str>,
) -> Result<std::path::PathBuf, String> {
    match graph_id.map(str::trim).filter(|value| !value.is_empty()) {
        Some(graph_id) => existing_graph_dir(app, graph_id),
        None => profile_dir(app),
    }
}
