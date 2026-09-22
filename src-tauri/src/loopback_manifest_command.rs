use crate::app_runtime::AppHandle;
use crate::{paths::loopback_manifest_path, storage::read_json};

#[cfg_attr(feature = "desktop", tauri::command)]
pub(crate) fn get_loopback_manifest(app: AppHandle) -> Result<serde_json::Value, String> {
    read_json::<serde_json::Value>(&loopback_manifest_path(&app)?).map_err(Into::into)
}
