use crate::app_runtime::AppHandle;
use crate::{
    profile_paths,
    storage::{read_json, write_secret_json},
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

const PROVIDER_KEYS_FILE: &str = "provider-keys.json";
const SCHEMA_VERSION: u32 = 1;

/// Providers whose API keys Garden will persist. The webview can only read or
/// write keys for these names — an allowlist so the frontend can't stuff
/// arbitrary entries (and, once the keychain backend lands, can't enumerate or
/// overwrite arbitrary keychain accounts under our service).
const KNOWN_PROVIDERS: &[&str] = &["openrouter", "anthropic", "openai"];

// Storage backend. Today: a 0o600 JSON file in the profile dir, the same tier
// as `hosted-credentials.json` (which already stores a Cognito refresh token in
// plaintext-owner-only form). The OS keychain is a planned FOLLOW-UP, deferred
// because Garden is ad-hoc signed (`signingIdentity: "-"` in tauri.conf.json):
// macOS keychain ACLs are bound to the code-signing identity, so with ad-hoc
// signing the entry is unstable across rebuilds (repeated "allow access"
// prompts, intermittent failures). When the app gains a stable Developer ID
// signature, swap the `read_key_file`/`write_key_file` bodies below to a
// `keyring` (v3, `apple-native`/`windows-native` features) implementation —
// the command surface and the pure helpers do not change.

/// On-disk shape: `profile_dir/provider-keys.json`, mode 0o600 via
/// `write_secret_json` (atomic, see `storage_atomic.rs`).
#[derive(Debug, Default, Serialize, Deserialize)]
struct ProviderKeyFile {
    #[serde(default)]
    schema_version: u32,
    #[serde(default)]
    keys: HashMap<String, String>,
}

fn provider_keys_path(app: &AppHandle) -> Result<PathBuf, String> {
    Ok(profile_paths::profile_dir(app)?.join(PROVIDER_KEYS_FILE))
}

fn is_known_provider(provider: &str) -> bool {
    KNOWN_PROVIDERS.contains(&provider)
}

fn read_key_file(app: &AppHandle) -> Result<ProviderKeyFile, String> {
    let path = provider_keys_path(app)?;
    if !path.exists() {
        return Ok(ProviderKeyFile::default());
    }
    let file: ProviderKeyFile = read_json(&path)?;
    Ok(file)
}

fn write_key_file(app: &AppHandle, mut file: ProviderKeyFile) -> Result<(), String> {
    file.schema_version = SCHEMA_VERSION;
    let path = provider_keys_path(app)?;
    write_secret_json(&path, &file).map_err(Into::into)
}

/// Pure: drop unknown/empty entries from a stored map. The agent only ever
/// needs keys we recognise, and an empty string is treated as "not set".
fn filter_known_keys(keys: HashMap<String, String>) -> HashMap<String, String> {
    keys.into_iter()
        .filter(|(provider, value)| is_known_provider(provider) && !value.is_empty())
        .collect()
}

/// Pure: derive the non-secret set/unset status for every known provider.
fn derive_status(file: &ProviderKeyFile) -> HashMap<String, bool> {
    KNOWN_PROVIDERS
        .iter()
        .map(|provider| {
            let set = file
                .keys
                .get(*provider)
                .map(|value| !value.is_empty())
                .unwrap_or(false);
            ((*provider).to_string(), set)
        })
        .collect()
}

/// Read a single provider's API key (for server-side use, e.g. the
/// `edit_artifact_image` MCP tool calling an image model). Returns None if unset
/// or unknown.
pub(crate) fn provider_key(app: &AppHandle, provider: &str) -> Option<String> {
    if !is_known_provider(provider) {
        return None;
    }
    read_key_file(app)
        .ok()
        .and_then(|file| file.keys.get(provider).cloned())
        .filter(|value| !value.is_empty())
}

/// Returns the full provider→key map for the Pi agent runtime in the webview.
///
/// This intentionally returns secrets: Pi runs in the webview and must hold the
/// provider key to call OpenRouter directly. Handing the key to the webview
/// does not expand the blast radius — the same process already holds the
/// full-scope loopback session token. The command is IPC-only (never proxied
/// over the loopback HTTP server), reachable solely by the app's own webview.
#[cfg_attr(feature = "desktop", tauri::command)]
pub(crate) fn get_provider_keys(app: AppHandle) -> Result<HashMap<String, String>, String> {
    let file = read_key_file(&app)?;
    Ok(filter_known_keys(file.keys))
}

#[cfg_attr(feature = "desktop", tauri::command)]
pub(crate) fn store_provider_key(
    app: AppHandle,
    provider: String,
    api_key: String,
) -> Result<(), String> {
    if !is_known_provider(&provider) {
        return Err(format!("unknown provider: {provider}"));
    }
    let trimmed = api_key.trim();
    if trimmed.is_empty() {
        return Err("api key must not be empty".to_string());
    }
    let mut file = read_key_file(&app)?;
    file.keys.insert(provider, trimmed.to_string());
    write_key_file(&app, file)
}

#[cfg_attr(feature = "desktop", tauri::command)]
pub(crate) fn delete_provider_key(app: AppHandle, provider: String) -> Result<(), String> {
    if !is_known_provider(&provider) {
        return Err(format!("unknown provider: {provider}"));
    }
    let mut file = read_key_file(&app)?;
    file.keys.remove(&provider);
    write_key_file(&app, file)
}

/// Non-secret status surface for the settings UI: which providers have a key
/// set. Never returns the key material itself.
#[cfg_attr(feature = "desktop", tauri::command)]
pub(crate) fn provider_key_status(app: AppHandle) -> Result<HashMap<String, bool>, String> {
    let file = read_key_file(&app)?;
    Ok(derive_status(&file))
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
        std::env::temp_dir().join(format!("provider-keys-{name}-{suffix}"))
    }

    fn sample_file() -> ProviderKeyFile {
        let mut keys = HashMap::new();
        keys.insert("openrouter".to_string(), "sk-or-secret".to_string());
        ProviderKeyFile {
            schema_version: SCHEMA_VERSION,
            keys,
        }
    }

    #[test]
    fn round_trip_writes_and_reads() {
        let dir = temp_dir("round-trip");
        let path = dir.join(PROVIDER_KEYS_FILE);
        write_secret_json(&path, &sample_file()).unwrap();
        let read: ProviderKeyFile = read_json(&path).unwrap();
        assert_eq!(
            read.keys.get("openrouter").map(String::as_str),
            Some("sk-or-secret")
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn allowlist_rejects_unknown_providers() {
        assert!(is_known_provider("openrouter"));
        assert!(!is_known_provider("evil"));
        assert!(!is_known_provider("openrouter/../../etc"));
    }

    #[test]
    fn filter_drops_unknown_and_empty() {
        let mut keys = HashMap::new();
        keys.insert("openrouter".to_string(), "sk".to_string());
        keys.insert("openai".to_string(), String::new()); // empty -> dropped
        keys.insert("bogus".to_string(), "x".to_string()); // unknown -> dropped
        let filtered = filter_known_keys(keys);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered.get("openrouter").map(String::as_str), Some("sk"));
    }

    #[test]
    fn status_reports_set_and_unset() {
        let status = derive_status(&sample_file());
        assert_eq!(status.get("openrouter"), Some(&true));
        assert_eq!(status.get("anthropic"), Some(&false));
        assert_eq!(status.get("openai"), Some(&false));
        // never leaks key material — status values are booleans only
        assert_eq!(status.len(), KNOWN_PROVIDERS.len());
    }

    #[test]
    fn permissions_are_owner_only_on_unix() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let dir = temp_dir("perms");
            let path = dir.join(PROVIDER_KEYS_FILE);
            write_secret_json(&path, &sample_file()).unwrap();
            let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
            let _ = fs::remove_dir_all(&dir);
        }
    }
}
