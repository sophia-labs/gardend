//! Running your own Sophia with `gardend`: the `serve`, `url`, `token`,
//! `service` and `chat` verbs (docs/headless-gardend.md, "Self-hosting").
//!
//! Plain `gardend` with no verb never reaches this module: hosted cells boot
//! exactly as before. Everything here is the friendly front of knobs that
//! already exist (`GARDEN_PROFILE_DIR`, `GARDEN_LOOPBACK_PORT`,
//! `GARDEN_LOOPBACK_TOKEN`, the client-token store) plus the default UI
//! location (`ui/` next to the binary, served when `GARDEN_UI_DIR` is unset).

mod chat;
mod cli;
mod index;
mod load;
mod serve;
mod service;
mod tokens;
mod visitors;

pub use cli::{parse, run, Command, ServeArgs, VisitorsAction};
pub use load::{LoadArgs, LoadReport};
pub use serve::{prepare_serve, ServePlan, ServeStart};
pub use service::ServiceArgs;
pub use tokens::TokenCreateArgs;

use std::net::IpAddr;
use std::path::{Path, PathBuf};

/// The port `gardend serve` binds when neither `--port` nor
/// `GARDEN_LOOPBACK_PORT` says otherwise: S-O-P-H on a phone keypad. Hosted
/// cells and the desktop app keep their OS-assigned ports; 7090 and 7091 are
/// left to the gardends people already run by hand.
pub const DEFAULT_PORT: u16 = 7674;

/// Where the master key of a self-hosted profile lives (owner-only JSON).
pub const KEY_FILE: &str = "selfhost-key.json";

/// The knobs a self-host run reads from the environment.
pub(crate) const ENV_PROFILE_DIR: &str = "GARDEN_PROFILE_DIR";
pub(crate) const ENV_PORT: &str = "GARDEN_LOOPBACK_PORT";
pub(crate) const ENV_HOST: &str = "GARDEN_LOOPBACK_HOST";
pub(crate) const ENV_TOKEN: &str = "GARDEN_LOOPBACK_TOKEN";
pub(crate) const ENV_UI_DIR: &str = "GARDEN_UI_DIR";

/// An environment lookup: the process environment in production, a fixed
/// table in tests (so tests never mutate process-global state).
pub(crate) type Env<'a> = &'a dyn Fn(&str) -> Option<String>;

pub(crate) fn process_env(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// The per-OS default profile directory for a self-hosted Sophia:
/// - macOS: `~/Library/Application Support/Sophia`
/// - Linux and other Unix: `$XDG_DATA_HOME/sophia`, else `~/.local/share/sophia`
/// - Windows: `%LOCALAPPDATA%\Sophia`
///
/// Deliberately not the desktop app's profile (`dev.sophia.garden`): the two
/// can never hold the same profile lock at once.
pub(crate) fn default_profile_dir(os: &str, env: Env) -> Result<PathBuf, String> {
    let home = || {
        env("HOME")
            .map(PathBuf::from)
            .ok_or_else(|| "HOME is not set; pass --profile <dir>".to_string())
    };
    match os {
        "macos" => Ok(home()?.join("Library/Application Support/Sophia")),
        "windows" => env("LOCALAPPDATA")
            .map(|dir| PathBuf::from(dir).join("Sophia"))
            .ok_or_else(|| "LOCALAPPDATA is not set; pass --profile <dir>".to_string()),
        _ => match env("XDG_DATA_HOME") {
            Some(dir) => Ok(PathBuf::from(dir).join("sophia")),
            None => Ok(home()?.join(".local/share/sophia")),
        },
    }
}

/// `--profile` (or `--profile-dir`), then `GARDEN_PROFILE_DIR`, then the
/// per-OS default. Always absolute; a leading `~/` is expanded.
pub(crate) fn resolve_profile_dir(flag: Option<&str>, os: &str, env: Env) -> Result<PathBuf, String> {
    let chosen = match flag.map(str::trim).filter(|value| !value.is_empty()) {
        Some(value) => PathBuf::from(value),
        None => match env(ENV_PROFILE_DIR) {
            Some(value) => PathBuf::from(value),
            None => default_profile_dir(os, env)?,
        },
    };
    let expanded = match chosen.strip_prefix("~") {
        Ok(rest) if chosen.starts_with("~") => match env("HOME") {
            Some(home) => PathBuf::from(home).join(rest),
            None => chosen.clone(),
        },
        _ => chosen.clone(),
    };
    std::path::absolute(&expanded)
        .map_err(|error| format!("resolve profile dir {}: {error}", expanded.display()))
}

/// `--port`, then `GARDEN_LOOPBACK_PORT`, then [`DEFAULT_PORT`].
pub(crate) fn resolve_port(flag: Option<u16>, env: Env) -> Result<u16, String> {
    if let Some(port) = flag {
        return Ok(port);
    }
    match env(ENV_PORT) {
        Some(value) => value
            .parse::<u16>()
            .map_err(|error| format!("{ENV_PORT}={value:?} is not a port: {error}")),
        None => Ok(DEFAULT_PORT),
    }
}

/// The bind host `gardend` will use (`GARDEN_LOOPBACK_HOST`, default
/// 127.0.0.1), and a warning when it reaches beyond this machine.
pub(crate) fn resolve_host(env: Env) -> Result<(IpAddr, Option<String>), String> {
    let Some(value) = env(ENV_HOST) else {
        return Ok((IpAddr::from([127, 0, 0, 1]), None));
    };
    let ip: IpAddr = value
        .parse()
        .map_err(|error| format!("{ENV_HOST}={value:?} must be an IP address: {error}"))?;
    let warning = (!ip.is_loopback()).then(|| {
        format!(
            "WARNING: {ENV_HOST}={value} makes Sophia reachable from other machines. The key in \
             the open link grants everything in this profile, and gardend has no TLS. Unset it to \
             stay on 127.0.0.1 and reach Sophia from elsewhere through a Cloudflare Tunnel to \
             127.0.0.1 instead."
        )
    });
    Ok((ip, warning))
}

/// The host to put in links: the bind address itself, except that a loopback
/// or unspecified (0.0.0.0, ::) bind is reached as 127.0.0.1.
pub(crate) fn link_host(ip: IpAddr) -> String {
    if ip.is_loopback() || ip.is_unspecified() {
        "127.0.0.1".to_string()
    } else if ip.is_ipv6() {
        format!("[{ip}]")
    } else {
        ip.to_string()
    }
}

/// The open-URL contract (plans/2026-10-12-self-host.md):
/// `http://127.0.0.1:<port>/#key=<token>`. The key rides in the fragment, so
/// it never reaches a server log or a proxy; the SPA stores it and strips it.
pub fn open_url(host: &str, port: u16, key: &str) -> String {
    format!("http://{host}:{port}/#key={key}")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeySource {
    /// Read from `<profile>/selfhost-key.json`.
    ProfileFile,
    /// Minted now and written to `<profile>/selfhost-key.json`.
    Minted,
    /// `GARDEN_LOOPBACK_TOKEN` was set; it is used as is and not stored.
    Environment,
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct KeyFile {
    key: String,
    created_at: String,
    #[serde(default)]
    note: String,
}

/// The profile's stable master key: the loopback token `gardend serve` runs
/// with, so the open link survives restarts. Minted once (256 bits of
/// randomness), stored owner-only in the profile. Rotate it by deleting the
/// file and restarting.
pub(crate) fn ensure_profile_key(profile_dir: &Path) -> Result<(String, KeySource), String> {
    let path = profile_dir.join(KEY_FILE);
    if path.is_file() {
        let file: KeyFile = crate::storage::read_json(&path)
            .map_err(|error| format!("read {}: {error}", path.display()))?;
        let key = file.key.trim().to_string();
        if !key.is_empty() {
            return Ok((key, KeySource::ProfileFile));
        }
    }
    ensure_profile_dir(profile_dir)?;
    let key = format!(
        "sophia-key-{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    );
    let file = KeyFile {
        key: key.clone(),
        created_at: crate::clock::timestamp(),
        note: "Master key for `gardend serve`: it grants every scope in this profile. \
               Delete this file and restart gardend to rotate it."
            .to_string(),
    };
    crate::storage::write_secret_json(&path, &file)
        .map_err(|error| format!("write {}: {error}", path.display()))?;
    Ok((key, KeySource::Minted))
}

/// Create the profile directory if needed, owner-only on Unix (it holds the
/// key, the token store and every graph).
pub(crate) fn ensure_profile_dir(profile_dir: &Path) -> Result<(), String> {
    if profile_dir.is_dir() {
        return Ok(());
    }
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder
        .create(profile_dir)
        .map_err(|error| format!("create profile dir {}: {error}", profile_dir.display()))
}

/// What `<profile>/loopback.json` says about a gardend serving this profile.
#[derive(Debug, Clone)]
pub(crate) struct RunningManifest {
    pub(crate) pid: u32,
    pub(crate) port: u16,
    pub(crate) token: String,
}

pub(crate) fn read_running_manifest(profile_dir: &Path) -> Option<RunningManifest> {
    let raw = std::fs::read(profile_dir.join(crate::runtime_config::LOOPBACK_MANIFEST_FILE)).ok()?;
    let value: serde_json::Value = serde_json::from_slice(&raw).ok()?;
    Some(RunningManifest {
        pid: value.get("pid")?.as_u64()? as u32,
        port: value.get("port")?.as_u64()? as u16,
        token: value
            .get("token")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string(),
    })
}

/// A gardend (not this process) is serving `profile_dir` right now: its
/// manifest names a live pid and its port accepts connections.
pub(crate) fn serving_now(profile_dir: &Path) -> Option<RunningManifest> {
    let manifest = read_running_manifest(profile_dir)?;
    if manifest.pid == std::process::id() || !crate::self_update::pid_alive(manifest.pid) {
        return None;
    }
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], manifest.port));
    std::net::TcpStream::connect_timeout(&addr, std::time::Duration::from_millis(500))
        .ok()
        .map(|_| manifest)
}

/// The directory a `ui/` folder must sit in: next to the binary, as invoked
/// and with symlinks resolved (a Homebrew-style link points elsewhere).
pub(crate) fn default_ui_dir(exe: Option<&Path>) -> Option<PathBuf> {
    let exe = exe?;
    let mut candidates = Vec::new();
    if let Some(dir) = exe.parent() {
        candidates.push(dir.join("ui"));
    }
    if let Ok(real) = std::fs::canonicalize(exe) {
        if let Some(dir) = real.parent() {
            candidates.push(dir.join("ui"));
        }
    }
    candidates
        .into_iter()
        .find(|dir| dir.join("index.html").is_file())
}

#[cfg(test)]
pub(crate) mod test_support {
    use std::collections::BTreeMap;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    pub(crate) fn tempdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "gardend-selfhost-{tag}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    pub(crate) fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let table: BTreeMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |key: &str| table.get(key).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{env_of, tempdir};
    use super::*;

    #[test]
    fn default_profile_dirs_per_os() {
        let env = env_of(&[("HOME", "/home/ana"), ("LOCALAPPDATA", r"C:\Users\ana\AppData\Local")]);
        assert_eq!(
            default_profile_dir("macos", &env).unwrap(),
            PathBuf::from("/home/ana/Library/Application Support/Sophia")
        );
        assert_eq!(
            default_profile_dir("linux", &env).unwrap(),
            PathBuf::from("/home/ana/.local/share/sophia")
        );
        assert_eq!(
            default_profile_dir("windows", &env).unwrap(),
            PathBuf::from(r"C:\Users\ana\AppData\Local").join("Sophia")
        );
        let xdg = env_of(&[("HOME", "/home/ana"), ("XDG_DATA_HOME", "/data")]);
        assert_eq!(
            default_profile_dir("linux", &xdg).unwrap(),
            PathBuf::from("/data/sophia")
        );
        assert!(default_profile_dir("linux", &env_of(&[])).is_err());
    }

    #[test]
    fn profile_dir_precedence_is_flag_then_env_then_default() {
        let env = env_of(&[("HOME", "/home/ana"), (ENV_PROFILE_DIR, "/srv/sophia")]);
        assert_eq!(
            resolve_profile_dir(Some("/tmp/flag"), "linux", &env).unwrap(),
            PathBuf::from("/tmp/flag")
        );
        assert_eq!(
            resolve_profile_dir(None, "linux", &env).unwrap(),
            PathBuf::from("/srv/sophia")
        );
        let no_env = env_of(&[("HOME", "/home/ana")]);
        assert_eq!(
            resolve_profile_dir(None, "linux", &no_env).unwrap(),
            PathBuf::from("/home/ana/.local/share/sophia")
        );
        assert_eq!(
            resolve_profile_dir(Some("~/my-sophia"), "linux", &no_env).unwrap(),
            PathBuf::from("/home/ana/my-sophia")
        );
        assert!(resolve_profile_dir(Some("relative/dir"), "linux", &no_env)
            .unwrap()
            .is_absolute());
    }

    #[test]
    fn port_precedence_is_flag_then_env_then_default() {
        assert_eq!(resolve_port(None, &env_of(&[])).unwrap(), DEFAULT_PORT);
        assert_eq!(resolve_port(None, &env_of(&[(ENV_PORT, "7800")])).unwrap(), 7800);
        assert_eq!(resolve_port(Some(7900), &env_of(&[(ENV_PORT, "7800")])).unwrap(), 7900);
        assert!(resolve_port(None, &env_of(&[(ENV_PORT, "seventy")])).is_err());
        assert_ne!(DEFAULT_PORT, 7090);
        assert_ne!(DEFAULT_PORT, 7091);
    }

    #[test]
    fn non_loopback_hosts_warn_and_links_stay_reachable() {
        let (ip, warning) = resolve_host(&env_of(&[])).unwrap();
        assert!(ip.is_loopback() && warning.is_none());
        let (ip, warning) = resolve_host(&env_of(&[(ENV_HOST, "127.0.0.1")])).unwrap();
        assert!(ip.is_loopback() && warning.is_none());
        let (ip, warning) = resolve_host(&env_of(&[(ENV_HOST, "0.0.0.0")])).unwrap();
        assert!(warning.unwrap().contains("reachable from other machines"));
        assert_eq!(link_host(ip), "127.0.0.1");
        let (ip, warning) = resolve_host(&env_of(&[(ENV_HOST, "192.168.1.5")])).unwrap();
        assert!(warning.is_some());
        assert_eq!(link_host(ip), "192.168.1.5");
        assert!(resolve_host(&env_of(&[(ENV_HOST, "my-laptop.local")])).is_err());
    }

    #[test]
    fn open_url_follows_the_contract() {
        assert_eq!(
            open_url("127.0.0.1", 7674, "sophia-key-abc"),
            "http://127.0.0.1:7674/#key=sophia-key-abc"
        );
    }

    #[test]
    fn profile_key_is_minted_once_owner_only_and_reused() {
        let profile = tempdir("key").join("profile");
        let (first, source) = ensure_profile_key(&profile).unwrap();
        assert_eq!(source, KeySource::Minted);
        assert!(first.starts_with("sophia-key-") && first.len() == "sophia-key-".len() + 64);
        let (again, source) = ensure_profile_key(&profile).unwrap();
        assert_eq!(source, KeySource::ProfileFile);
        assert_eq!(first, again);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let file_mode = std::fs::metadata(profile.join(KEY_FILE)).unwrap().permissions().mode();
            assert_eq!(file_mode & 0o077, 0, "key file must be owner-only");
            let dir_mode = std::fs::metadata(&profile).unwrap().permissions().mode();
            assert_eq!(dir_mode & 0o077, 0, "a profile dir serve creates is owner-only");
        }
    }

    #[test]
    fn ui_dir_is_found_next_to_the_binary_only_when_it_has_an_index() {
        let root = tempdir("ui");
        let exe = root.join("gardend");
        std::fs::write(&exe, b"").unwrap();
        assert_eq!(default_ui_dir(Some(&exe)), None);
        std::fs::create_dir_all(root.join("ui")).unwrap();
        assert_eq!(default_ui_dir(Some(&exe)), None, "an empty ui/ is not a UI");
        std::fs::write(root.join("ui/index.html"), b"<!doctype html>").unwrap();
        assert_eq!(default_ui_dir(Some(&exe)), Some(root.join("ui")));
        assert_eq!(default_ui_dir(None), None);
    }

    #[test]
    fn a_dead_manifest_is_not_serving() {
        let profile = tempdir("manifest");
        assert!(serving_now(&profile).is_none());
        std::fs::write(
            profile.join("loopback.json"),
            br#"{"pid": 4194303, "port": 1, "token": "t"}"#,
        )
        .unwrap();
        let manifest = read_running_manifest(&profile).unwrap();
        assert_eq!((manifest.pid, manifest.port, manifest.token.as_str()), (4194303, 1, "t"));
        assert!(serving_now(&profile).is_none());
    }
}
