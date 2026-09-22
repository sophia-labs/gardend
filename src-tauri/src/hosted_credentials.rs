use crate::app_runtime::AppHandle;
use crate::{
    profile_paths,
    storage::{read_json, remove_file_if_exists, write_secret_json},
};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

const HOSTED_CREDENTIALS_FILE: &str = "hosted-credentials.json";
const SCHEMA_VERSION: u32 = 1;

/// On-disk representation of Cognito tokens for hosted-mode launches.
///
/// Storage: `profile_dir/hosted-credentials.json`, mode 0o600 via
/// `write_secret_json`. Atomic writes via the existing `storage_atomic.rs`
/// pipeline — important because Cognito rotates refresh tokens (default
/// behavior on User Pools created in 2024+), so a crash mid-refresh must
/// not leave a torn file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct HostedCredentials {
    #[serde(default)]
    pub schema_version: u32,
    pub access_token: String,
    pub id_token: String,
    pub refresh_token: String,
    /// Epoch milliseconds when the access/id token expire. The frontend
    /// `SecureTokenStorage` uses the same convention.
    pub expires_at: u128,
    /// Optional user email cached at sign-in for UI display before the
    /// JWT is decoded. Not load-bearing for auth.
    #[serde(default)]
    pub user_email: Option<String>,
}

fn credentials_path(app: &AppHandle) -> Result<PathBuf, String> {
    Ok(profile_paths::profile_dir(app)?.join(HOSTED_CREDENTIALS_FILE))
}

/// Read credentials. Returns None when the file is absent (signed-out
/// state). A parse error is propagated so callers don't silently treat a
/// corrupt file as "signed out" and trigger a redundant OAuth flow.
pub(crate) fn read_credentials(app: &AppHandle) -> Result<Option<HostedCredentials>, String> {
    let path = credentials_path(app)?;
    if !path.exists() {
        return Ok(None);
    }
    let mut credentials: HostedCredentials = read_json(&path)?;
    if credentials.schema_version == 0 {
        credentials.schema_version = SCHEMA_VERSION;
    }
    Ok(Some(credentials))
}

pub(crate) fn write_credentials(
    app: &AppHandle,
    mut credentials: HostedCredentials,
) -> Result<(), String> {
    credentials.schema_version = SCHEMA_VERSION;
    let path = credentials_path(app)?;
    write_secret_json(&path, &credentials).map_err(Into::into)
}

pub(crate) fn delete_credentials(app: &AppHandle) -> Result<(), String> {
    let path = credentials_path(app)?;
    remove_file_if_exists(&path).map(|_| ()).map_err(Into::into)
}

#[derive(Debug, Deserialize)]
pub(crate) struct StoreHostedCredentialsInput {
    pub access_token: String,
    pub id_token: String,
    pub refresh_token: String,
    pub expires_at: u128,
    #[serde(default)]
    pub user_email: Option<String>,
}

#[cfg_attr(feature = "desktop", tauri::command)]
pub(crate) fn get_hosted_credentials(app: AppHandle) -> Result<Option<HostedCredentials>, String> {
    read_credentials(&app)
}

#[cfg_attr(feature = "desktop", tauri::command)]
pub(crate) fn store_hosted_credentials(
    app: AppHandle,
    input: StoreHostedCredentialsInput,
) -> Result<(), String> {
    let credentials = HostedCredentials {
        schema_version: SCHEMA_VERSION,
        access_token: input.access_token,
        id_token: input.id_token,
        refresh_token: input.refresh_token,
        expires_at: input.expires_at,
        user_email: input.user_email,
    };
    write_credentials(&app, credentials)
}

#[cfg_attr(feature = "desktop", tauri::command)]
pub(crate) fn clear_hosted_credentials(app: AppHandle) -> Result<(), String> {
    delete_credentials(&app)
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
        std::env::temp_dir().join(format!("hosted-creds-{name}-{suffix}"))
    }

    fn sample_credentials() -> HostedCredentials {
        HostedCredentials {
            schema_version: SCHEMA_VERSION,
            access_token: "access.token.value".to_string(),
            id_token: "id.token.value".to_string(),
            refresh_token: "refresh.token.value".to_string(),
            expires_at: 1759685328000,
            user_email: Some("user@example.com".to_string()),
        }
    }

    #[test]
    fn round_trip_writes_and_reads() {
        let dir = temp_dir("round-trip");
        let path = dir.join("hosted-credentials.json");
        write_secret_json(&path, &sample_credentials()).unwrap();
        let read: HostedCredentials = read_json(&path).unwrap();
        assert_eq!(read.access_token, "access.token.value");
        assert_eq!(read.refresh_token, "refresh.token.value");
        assert_eq!(read.expires_at, 1759685328000);
        assert_eq!(read.user_email.as_deref(), Some("user@example.com"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_user_email_defaults_to_none() {
        let dir = temp_dir("no-email");
        let path = dir.join("hosted-credentials.json");
        write_secret_json(
            &path,
            &serde_json::json!({
                "schema_version": 1,
                "access_token": "a",
                "id_token": "b",
                "refresh_token": "c",
                "expires_at": 1u64,
            }),
        )
        .unwrap();
        let read: HostedCredentials = read_json(&path).unwrap();
        assert!(read.user_email.is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn schema_version_defaults_when_missing() {
        let dir = temp_dir("schema-default");
        let path = dir.join("hosted-credentials.json");
        write_secret_json(
            &path,
            &serde_json::json!({
                "access_token": "a",
                "id_token": "b",
                "refresh_token": "c",
                "expires_at": 1u64,
            }),
        )
        .unwrap();
        let mut read: HostedCredentials = read_json(&path).unwrap();
        if read.schema_version == 0 {
            read.schema_version = SCHEMA_VERSION;
        }
        assert_eq!(read.schema_version, SCHEMA_VERSION);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn permissions_are_owner_only_on_unix() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let dir = temp_dir("perms");
            let path = dir.join("hosted-credentials.json");
            write_secret_json(&path, &sample_credentials()).unwrap();
            let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
            let _ = fs::remove_dir_all(&dir);
        }
    }
}
