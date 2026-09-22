use crate::app_runtime::AppHandle;
use crate::{
    clock::timestamp,
    profile_paths,
    storage::{read_json, write_secret_json},
};
use serde::{Deserialize, Serialize};
use std::{
    path::PathBuf,
    sync::atomic::{AtomicBool, Ordering},
};

/// One-time flag set when `read_and_recover` reverts a pending toggle.
/// The frontend consumes it at boot via `consume_hosted_mode_recovery_flag`
/// to display a "setup didn't complete" toast, then it stays cleared for
/// the lifetime of the process.
static PENDING_RECOVERY_FLAG: AtomicBool = AtomicBool::new(false);

const HOSTED_MODE_CONFIG_FILE: &str = "hosted-mode-config.json";
const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RuntimeMode {
    Local,
    Hosted,
}

impl RuntimeMode {
    pub fn is_local(&self) -> bool {
        matches!(self, RuntimeMode::Local)
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            RuntimeMode::Local => "local",
            RuntimeMode::Hosted => "hosted",
        }
    }
}

impl Default for RuntimeMode {
    fn default() -> Self {
        RuntimeMode::Local
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct HostedModeConfig {
    #[serde(default)]
    pub schema_version: u32,
    pub mode: RuntimeMode,
    /// True between the start of a hosted-mode toggle and its successful completion.
    /// If a launch finds this true, the toggle was interrupted; revert to Local.
    #[serde(default)]
    pub pending: bool,
    pub updated_at: String,
}

impl Default for HostedModeConfig {
    fn default() -> Self {
        HostedModeConfig {
            schema_version: SCHEMA_VERSION,
            mode: RuntimeMode::Local,
            pending: false,
            updated_at: timestamp(),
        }
    }
}

fn config_path(app: &AppHandle) -> Result<PathBuf, String> {
    Ok(profile_paths::profile_dir(app)?.join(HOSTED_MODE_CONFIG_FILE))
}

/// Read the persisted hosted-mode config, recovering pending state.
///
/// Behavior:
/// - Missing file → returns default (Local, not pending).
/// - `pending: true` on read → returns Local with `pending: false` written back,
///   so the next launch is clean. The caller can detect recovery via `was_recovered`.
/// - Parse failure → propagates the error; the caller should treat as fatal
///   (we don't silently delete user state).
pub(crate) fn read_and_recover(app: &AppHandle) -> Result<(HostedModeConfig, bool), String> {
    let path = config_path(app)?;
    if !path.exists() {
        return Ok((HostedModeConfig::default(), false));
    }
    let mut config: HostedModeConfig = read_json(&path)?;
    if config.schema_version == 0 {
        config.schema_version = SCHEMA_VERSION;
    }
    if config.pending {
        log::warn!(
            "hosted-mode config marked pending on launch; reverting to local mode and clearing flag"
        );
        let recovered = HostedModeConfig {
            schema_version: SCHEMA_VERSION,
            mode: RuntimeMode::Local,
            pending: false,
            updated_at: timestamp(),
        };
        write_secret_json(&path, &recovered)?;
        PENDING_RECOVERY_FLAG.store(true, Ordering::Release);
        return Ok((recovered, true));
    }
    Ok((config, false))
}

/// Frontend-facing one-time read of the recovery flag. Returns the
/// previous value and clears it. After the first call, subsequent calls
/// return false.
#[cfg_attr(feature = "desktop", tauri::command)]
pub(crate) fn consume_hosted_mode_recovery_flag() -> bool {
    PENDING_RECOVERY_FLAG.swap(false, Ordering::AcqRel)
}

pub(crate) fn write_config(
    app: &AppHandle,
    mode: RuntimeMode,
    pending: bool,
) -> Result<HostedModeConfig, String> {
    let path = config_path(app)?;
    let config = HostedModeConfig {
        schema_version: SCHEMA_VERSION,
        mode,
        pending,
        updated_at: timestamp(),
    };
    write_secret_json(&path, &config)?;
    Ok(config)
}

#[derive(Debug, Deserialize)]
pub(crate) struct SetHostedModeConfigInput {
    pub mode: RuntimeMode,
    #[serde(default)]
    pub pending: bool,
}

#[cfg_attr(feature = "desktop", tauri::command)]
pub(crate) fn get_hosted_mode_config(app: AppHandle) -> Result<HostedModeConfig, String> {
    let (config, _) = read_and_recover(&app)?;
    Ok(config)
}

#[cfg_attr(feature = "desktop", tauri::command)]
pub(crate) fn set_hosted_mode_config(
    app: AppHandle,
    input: SetHostedModeConfigInput,
) -> Result<HostedModeConfig, String> {
    write_config(&app, input.mode, input.pending)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        path::PathBuf,
        time::{SystemTime, UNIX_EPOCH},
    };

    fn temp_dir(name: &str) -> PathBuf {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        std::env::temp_dir().join(format!("hosted-mode-{name}-{suffix}"))
    }

    fn write_at(path: &std::path::Path, value: &serde_json::Value) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        write_secret_json(path, value).unwrap();
    }

    /// Standalone equivalent of `read_and_recover` that takes a path (no AppHandle)
    /// for unit testing.
    fn read_and_recover_at(path: &std::path::Path) -> Result<(HostedModeConfig, bool), String> {
        if !path.exists() {
            return Ok((HostedModeConfig::default(), false));
        }
        let mut config: HostedModeConfig = read_json(path)?;
        if config.schema_version == 0 {
            config.schema_version = SCHEMA_VERSION;
        }
        if config.pending {
            let recovered = HostedModeConfig {
                schema_version: SCHEMA_VERSION,
                mode: RuntimeMode::Local,
                pending: false,
                updated_at: timestamp(),
            };
            write_secret_json(path, &recovered)?;
            return Ok((recovered, true));
        }
        Ok((config, false))
    }

    #[test]
    fn missing_file_returns_default_local() {
        let dir = temp_dir("missing");
        let path = dir.join("hosted-mode-config.json");
        let (config, recovered) = read_and_recover_at(&path).unwrap();
        assert_eq!(config.mode, RuntimeMode::Local);
        assert!(!config.pending);
        assert!(!recovered);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn round_trip_hosted_clean() {
        let dir = temp_dir("hosted");
        let path = dir.join("hosted-mode-config.json");
        write_at(
            &path,
            &serde_json::json!({
                "schema_version": 1,
                "mode": "hosted",
                "pending": false,
                "updated_at": "1759685328000",
            }),
        );
        let (config, recovered) = read_and_recover_at(&path).unwrap();
        assert_eq!(config.mode, RuntimeMode::Hosted);
        assert!(!config.pending);
        assert!(!recovered);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn pending_on_launch_reverts_to_local_and_clears_flag() {
        let dir = temp_dir("pending");
        let path = dir.join("hosted-mode-config.json");
        write_at(
            &path,
            &serde_json::json!({
                "schema_version": 1,
                "mode": "hosted",
                "pending": true,
                "updated_at": "1759685328000",
            }),
        );
        let (config, recovered) = read_and_recover_at(&path).unwrap();
        assert_eq!(config.mode, RuntimeMode::Local);
        assert!(!config.pending);
        assert!(recovered);
        // Re-read should NOT trigger recovery again.
        let (config2, recovered2) = read_and_recover_at(&path).unwrap();
        assert_eq!(config2.mode, RuntimeMode::Local);
        assert!(!recovered2);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn schema_version_zero_is_normalized() {
        let dir = temp_dir("schema-zero");
        let path = dir.join("hosted-mode-config.json");
        write_at(
            &path,
            &serde_json::json!({
                "mode": "local",
                "pending": false,
                "updated_at": "1759685328000",
            }),
        );
        let (config, _) = read_and_recover_at(&path).unwrap();
        assert_eq!(config.schema_version, SCHEMA_VERSION);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn runtime_mode_is_local_predicate() {
        assert!(RuntimeMode::Local.is_local());
        assert!(!RuntimeMode::Hosted.is_local());
        assert_eq!(RuntimeMode::Local.as_str(), "local");
        assert_eq!(RuntimeMode::Hosted.as_str(), "hosted");
    }
}
