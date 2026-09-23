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

// Server-side READS resolve env → macOS Keychain → this file (see
// `resolve_provider_key_with`; Vera's ruling 2026-09-22). The settings UI below
// still WRITES this file, which is now the deprecated last resort.
//
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

/// The macOS Keychain service Garden reads provider keys from (generic
/// password; the account is the provider name, e.g. `openrouter`). Add one with
/// `security add-generic-password -s dev.sophia.garden -a openrouter -w`.
pub(crate) const KEYCHAIN_SERVICE: &str = "dev.sophia.garden";

/// Where a resolved provider key came from. Never carries the value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProviderKeySource {
    /// `SOPHIA_{PROVIDER}_API_KEY` — hosted cells (Sirin mounts it from a
    /// Kubernetes Secret via `secretKeyRef`).
    Env,
    /// macOS Keychain, service [`KEYCHAIN_SERVICE`], account = provider.
    Keychain,
    /// DEPRECATED: the profile's `provider-keys.json`. Kept for back-compat.
    ProfileFile,
}

/// `SOPHIA_OPENROUTER_API_KEY` for `openrouter`, etc.
pub(crate) fn provider_key_env_var(provider: &str) -> String {
    format!("SOPHIA_{}_API_KEY", provider.to_ascii_uppercase())
}

fn non_empty(value: String) -> Option<String> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

#[cfg(target_os = "macos")]
pub(crate) fn keychain_provider_key(service: &str, provider: &str) -> Option<String> {
    security_framework::passwords::get_generic_password(service, provider)
        .ok()
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .and_then(non_empty)
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn keychain_provider_key(_service: &str, _provider: &str) -> Option<String> {
    None
}

/// Resolution order (Vera, 2026-09-22): env var → macOS Keychain → profile
/// file (deprecated). The Keychain service is a parameter so tests can use a
/// throwaway item; production passes [`KEYCHAIN_SERVICE`].
pub(crate) fn resolve_provider_key_with(
    provider: &str,
    keychain_service: &str,
    profile_file_key: impl FnOnce() -> Option<String>,
) -> Option<(String, ProviderKeySource)> {
    if !is_known_provider(provider) {
        return None;
    }
    if let Some(key) = std::env::var(provider_key_env_var(provider)).ok().and_then(non_empty) {
        return Some((key, ProviderKeySource::Env));
    }
    if let Some(key) = keychain_provider_key(keychain_service, provider) {
        return Some((key, ProviderKeySource::Keychain));
    }
    profile_file_key()
        .and_then(non_empty)
        .map(|key| (key, ProviderKeySource::ProfileFile))
}

/// Read a single provider's API key for server-side use (the shared
/// `openrouter_image` path behind `edit_artifact_image` and `agent_self_image`),
/// in the order env → Keychain → deprecated profile file. None if unset/unknown.
pub(crate) fn provider_key(app: &AppHandle, provider: &str) -> Option<String> {
    resolve_provider_key_with(provider, KEYCHAIN_SERVICE, || profile_file_key(app, provider))
        .map(|(key, source)| {
            if source == ProviderKeySource::ProfileFile {
                log::warn!(
                    "provider key for {provider} read from the deprecated profile provider-keys.json; \
                     move it to {} or the macOS Keychain ({KEYCHAIN_SERVICE}/{provider})",
                    provider_key_env_var(provider)
                );
            }
            key
        })
}

/// DEPRECATED source: the profile's `provider-keys.json` entry.
fn profile_file_key(app: &AppHandle, provider: &str) -> Option<String> {
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

    // ── resolution order: env → Keychain → profile file (no mocks) ──

    /// A REAL env var wins over everything; the profile file is the last
    /// resort. Uses `anthropic` so no other test's provider path is disturbed.
    #[test]
    fn resolution_order_env_then_profile_file() {
        let var = provider_key_env_var("anthropic");
        assert_eq!(var, "SOPHIA_ANTHROPIC_API_KEY");
        let previous = std::env::var_os(&var);
        let service = format!("dev.sophia.garden.test-absent-{}", uuid::Uuid::new_v4().simple());
        std::env::remove_var(&var);
        assert_eq!(resolve_provider_key_with("anthropic", &service, || None), None);
        assert_eq!(
            resolve_provider_key_with("anthropic", &service, || Some("from-file".into())),
            Some(("from-file".into(), ProviderKeySource::ProfileFile))
        );
        std::env::set_var(&var, "  from-env  ");
        assert_eq!(
            resolve_provider_key_with("anthropic", &service, || Some("from-file".into())),
            Some(("from-env".into(), ProviderKeySource::Env))
        );
        std::env::set_var(&var, "   ");
        assert_eq!(
            resolve_provider_key_with("anthropic", &service, || Some("from-file".into())),
            Some(("from-file".into(), ProviderKeySource::ProfileFile)),
            "a blank env var is unset"
        );
        match previous {
            Some(value) => std::env::set_var(&var, value),
            None => std::env::remove_var(&var),
        }
        assert_eq!(resolve_provider_key_with("not-a-provider", &service, || Some("x".into())), None);
    }

    /// The REAL macOS Keychain: write a throwaway generic password under a
    /// unique service, resolve it (Keychain beats the file, env beats the
    /// Keychain), delete it, and prove it is gone.
    #[cfg(target_os = "macos")]
    #[test]
    fn resolution_order_reads_the_real_macos_keychain() {
        use security_framework::passwords::{delete_generic_password, set_generic_password};
        let var = provider_key_env_var("openai");
        let previous = std::env::var_os(&var);
        std::env::remove_var(&var);
        let service = format!("dev.sophia.garden.test-{}", uuid::Uuid::new_v4().simple());
        set_generic_password(&service, "openai", b"from-keychain").expect("write throwaway keychain item");
        let outcome = std::panic::catch_unwind(|| {
            assert_eq!(keychain_provider_key(&service, "openai").as_deref(), Some("from-keychain"));
            assert_eq!(
                resolve_provider_key_with("openai", &service, || Some("from-file".into())),
                Some(("from-keychain".into(), ProviderKeySource::Keychain))
            );
            std::env::set_var(&var, "from-env");
            let env_wins = resolve_provider_key_with("openai", &service, || None);
            std::env::remove_var(&var);
            assert_eq!(env_wins, Some(("from-env".into(), ProviderKeySource::Env)));
        });
        delete_generic_password(&service, "openai").expect("delete throwaway keychain item");
        assert_eq!(keychain_provider_key(&service, "openai"), None, "throwaway item is gone");
        if let Some(value) = previous {
            std::env::set_var(&var, value);
        }
        if let Err(panic) = outcome {
            std::panic::resume_unwind(panic);
        }
    }

}
