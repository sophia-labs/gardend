//! The local place: a `gardend` this app starts beside itself, with its profile in the app's data folder.
//!
//! Shrubbery's desktop bootstrap asks the host for `get_loopback_manifest` and then speaks the ordinary gardend contract
//! to the loopback, so the window needs nothing else from us. gardend takes its configuration from the environment; the
//! app mints the session token and hands it over, so it never has to trust a token read back from disk.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager, State};

pub struct LocalCell {
    child: Mutex<Option<Child>>,
    token: String,
}

impl LocalCell {
    pub fn new() -> Self {
        Self { child: Mutex::new(None), token: session_token() }
    }

    /// Start gardend unless it is already running. Called when the window opens the local place.
    pub fn ensure_started(&self, app: &AppHandle) -> Result<(), String> {
        let mut child = self.child.lock().map_err(|_| "local cell lock poisoned")?;
        if let Some(running) = child.as_mut() {
            if running.try_wait().map_err(|e| e.to_string())?.is_none() {
                return Ok(());
            }
        }
        let profile = profile_dir(app)?;
        std::fs::create_dir_all(&profile).map_err(|e| format!("create {}: {e}", profile.display()))?;
        // A manifest left by an earlier run names a dead port; the bootstrap would wait on it forever.
        let _ = std::fs::remove_file(profile.join("loopback.json"));
        let bin = gardend_binary(app)?;
        let log = std::fs::File::create(profile.join("gardend.log")).map_err(|e| e.to_string())?;
        let spawned = Command::new(&bin)
            .env("GARDEN_PROFILE_DIR", &profile)
            .env("GARDEN_LOOPBACK_HOST", "127.0.0.1")
            .env("GARDEN_LOOPBACK_PORT", "0")
            .env("GARDEN_LOOPBACK_TOKEN", &self.token)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(log)
            .spawn()
            .map_err(|e| format!("start gardend at {}: {e}", bin.display()))?;
        *child = Some(spawned);
        Ok(())
    }

    pub fn stop(&self) {
        if let Ok(mut child) = self.child.lock() {
            if let Some(mut running) = child.take() {
                terminate(&mut running);
            }
        }
    }

    fn pid(&self) -> Option<u32> {
        self.child.lock().ok().and_then(|c| c.as_ref().map(Child::id))
    }
}

#[cfg(unix)]
fn terminate(child: &mut Child) {
    // SIGTERM lets gardend flush and release its store lock; it exits within a second or two.
    let _ = Command::new("kill").arg("-TERM").arg(child.id().to_string()).status();
    for _ in 0..50 {
        if matches!(child.try_wait(), Ok(Some(_))) {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(not(unix))]
fn terminate(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

fn session_token() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

pub fn profile_dir(app: &AppHandle) -> Result<PathBuf, String> {
    if let Ok(dir) = std::env::var("SOPHIA_LOCAL_PROFILE") {
        return Ok(PathBuf::from(dir));
    }
    Ok(app.path().app_data_dir().map_err(|e| e.to_string())?.join("local"))
}

/// gardend: `SOPHIA_GARDEND`, then the copy bundled beside the app's executable, then `gardend` on the PATH.
fn gardend_binary(app: &AppHandle) -> Result<PathBuf, String> {
    if let Ok(path) = std::env::var("SOPHIA_GARDEND") {
        return Ok(PathBuf::from(path));
    }
    let name = if cfg!(windows) { "gardend.exe" } else { "gardend" };
    let beside = std::env::current_exe().ok().and_then(|exe| exe.parent().map(|dir| dir.join(name)));
    if let Some(path) = beside.filter(|p| p.is_file()) {
        return Ok(path);
    }
    if let Ok(dir) = app.path().resource_dir() {
        let path = dir.join(name);
        if path.is_file() {
            return Ok(path);
        }
    }
    Ok(PathBuf::from(name))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DiskManifest {
    api_url: String,
    mcp_url: String,
    pid: Option<u32>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LoopbackManifest {
    api_url: String,
    mcp_url: String,
    token: String,
}

/// The loopback coordinates of the gardend this app started. Shrubbery retries until gardend has written them and
/// answers `/health`, so "not yet" is an ordinary answer here.
#[tauri::command]
pub fn get_loopback_manifest(app: AppHandle, cell: State<'_, LocalCell>) -> Result<LoopbackManifest, String> {
    cell.ensure_started(&app)?;
    let path = profile_dir(&app)?.join("loopback.json");
    let manifest: DiskManifest = read_json(&path).map_err(|e| format!("gardend is starting ({e})"))?;
    if let (Some(written), Some(ours)) = (manifest.pid, cell.pid()) {
        if written != ours {
            return Err("gardend is starting (the manifest belongs to an earlier run)".into());
        }
    }
    Ok(LoopbackManifest { api_url: manifest.api_url, mcp_url: manifest.mcp_url, token: cell.token.clone() })
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T, String> {
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    serde_json::from_slice(&bytes).map_err(|e| e.to_string())
}

// Provider keys for local AI features (image generation, local model settings): one owner-only file in the app's
// config folder, the same shape the desktop app's commands return.

fn keys_path(app: &AppHandle) -> Result<PathBuf, String> {
    Ok(app.path().app_config_dir().map_err(|e| e.to_string())?.join("provider-keys.json"))
}

fn read_keys(app: &AppHandle) -> Result<BTreeMap<String, String>, String> {
    let path = keys_path(app)?;
    if !path.exists() {
        return Ok(BTreeMap::new());
    }
    read_json(&path)
}

fn write_keys(app: &AppHandle, keys: &BTreeMap<String, String>) -> Result<(), String> {
    let path = keys_path(app)?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&path).map_err(|e| e.to_string())?;
    std::io::Write::write_all(&mut file, &serde_json::to_vec_pretty(keys).map_err(|e| e.to_string())?).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn get_provider_keys(app: AppHandle) -> Result<BTreeMap<String, String>, String> {
    read_keys(&app)
}

#[tauri::command]
pub fn store_provider_key(app: AppHandle, provider: String, api_key: String) -> Result<(), String> {
    let mut keys = read_keys(&app)?;
    keys.insert(provider, api_key);
    write_keys(&app, &keys)
}

#[tauri::command]
pub fn delete_provider_key(app: AppHandle, provider: String) -> Result<(), String> {
    let mut keys = read_keys(&app)?;
    keys.remove(&provider);
    write_keys(&app, &keys)
}
