//! `gardend` self-update against GitHub Releases (bare-binary installs only;
//! container users pull a new `ghcr.io/sophia-labs/gardend` image instead).
//!
//! Crude on purpose: a release is three kinds of static file, so the whole
//! protocol works against any static file server:
//!
//! * `release.json` — `{"name","version","storageFormat","assets":{"<triple>":"<file>"}}`
//! * `SHA256SUMS` — `sha256sum` output over every tarball
//! * `gardend-v<version>-<target>.tar.gz` — contains the `gardend` binary
//!
//! The newest manifest comes from `{base}/releases/latest/download/release.json`,
//! assets from `{base}/releases/download/v{version}/…`. No GitHub API.
//!
//! Two safety gates guard the self-replace:
//! 1. **Storage format.** A release whose `storageFormat` differs from this
//!    binary's [`crate::storage_format::STORAGE_FORMAT`] is refused (and the
//!    downloaded binary's own `--version --json` must agree with its manifest).
//! 2. **Serving.** While a gardend is serving a graph (a live pid/port in the
//!    profile's `loopback.json`, or any other running `gardend` process), the
//!    update is refused. Stop it first: SIGTERM runs gardend's forced final
//!    durable flush, so a clean stop is the flush.

use crate::storage_format::STORAGE_FORMAT;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const DEFAULT_RELEASE_BASE: &str = "https://github.com/sophia-labs/gardend";
pub const BIN_NAME: &str = "gardend";
pub const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");
pub const CHECK_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);
const MAX_DOWNLOAD_BYTES: u64 = 1 << 30;

/// A release's manifest (`release.json`), also the shape of `gardend --version --json`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ReleaseManifest {
    pub name: String,
    pub version: String,
    /// Absent means "unknown", which the gate treats as a mismatch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_format: Option<u32>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub assets: BTreeMap<String, String>,
}

/// This binary's identity, printed by `gardend --version --json`.
pub fn version_json() -> String {
    serde_json::to_string(&ReleaseManifest {
        name: BIN_NAME.into(),
        version: CURRENT_VERSION.into(),
        storage_format: Some(STORAGE_FORMAT),
        assets: BTreeMap::new(),
    })
    .expect("static manifest serializes")
}

/// `GARDEN_UPDATE_URL` overrides the release base (fork, mirror, test server).
pub fn release_base() -> String {
    std::env::var("GARDEN_UPDATE_URL")
        .ok()
        .map(|v| v.trim().trim_end_matches('/').to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| DEFAULT_RELEASE_BASE.to_string())
}

pub fn current_target() -> Option<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => Some("x86_64-unknown-linux-gnu"),
        ("linux", "aarch64") => Some("aarch64-unknown-linux-gnu"),
        ("macos", "x86_64") => Some("x86_64-apple-darwin"),
        ("macos", "aarch64") => Some("aarch64-apple-darwin"),
        _ => None,
    }
}

pub fn parse_version(v: &str) -> Option<(u64, u64, u64)> {
    let v = v.trim().trim_start_matches('v');
    let core = v.split(['-', '+']).next()?;
    let mut parts = core.split('.').map(|p| p.parse::<u64>().ok());
    let out = (parts.next()??, parts.next()??, parts.next()??);
    parts.next().is_none().then_some(out)
}

pub fn is_newer(candidate: &str, current: &str) -> bool {
    matches!((parse_version(candidate), parse_version(current)), (Some(a), Some(b)) if a > b)
}

pub fn parse_sha256sums(text: &str) -> BTreeMap<String, String> {
    text.lines()
        .filter_map(|line| {
            let (hash, file) = line.trim().split_once(char::is_whitespace)?;
            let file = file.trim().trim_start_matches('*');
            (hash.len() == 64 && !file.is_empty())
                .then(|| (file.to_string(), hash.to_ascii_lowercase()))
        })
        .collect()
}

fn get_bytes(url: &str, timeout: Duration) -> Result<Vec<u8>, String> {
    let agent = ureq::Agent::config_builder()
        .timeout_global(Some(timeout))
        .user_agent(concat!("gardend/", env!("CARGO_PKG_VERSION")))
        .build()
        .new_agent();
    let mut resp = agent
        .get(url)
        .call()
        .map_err(|e| format!("GET {url}: {e}"))?;
    resp.body_mut()
        .with_config()
        .limit(MAX_DOWNLOAD_BYTES)
        .read_to_vec()
        .map_err(|e| format!("GET {url}: reading body: {e}"))
}

pub fn fetch_latest(base: &str, timeout: Duration) -> Result<ReleaseManifest, String> {
    let url = format!("{base}/releases/latest/download/release.json");
    let manifest: ReleaseManifest = serde_json::from_slice(&get_bytes(&url, timeout)?)
        .map_err(|e| format!("parsing {url}: {e}"))?;
    if parse_version(&manifest.version).is_none() {
        return Err(format!(
            "release manifest has unparseable version {:?}",
            manifest.version
        ));
    }
    Ok(manifest)
}

/// The storage-format gate: `Err` explains why this release must not replace us.
pub fn storage_format_gate(manifest: &ReleaseManifest) -> Result<(), String> {
    match manifest.storage_format {
        Some(f) if f == STORAGE_FORMAT => Ok(()),
        Some(f) => Err(format!(
            "gardend {} uses storage format {f}, this binary uses {STORAGE_FORMAT}: \
             refusing to self-update across a storage-format change. Back up the \
             profile and durable dirs, read the release notes, and install it by hand.",
            manifest.version
        )),
        None => Err(format!(
            "gardend {}'s release manifest declares no storageFormat: refusing to self-update",
            manifest.version
        )),
    }
}

pub fn extract_binary(tarball: &[u8], bin_name: &str) -> Result<Vec<u8>, String> {
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(tarball));
    for entry in archive
        .entries()
        .map_err(|e| format!("reading tarball: {e}"))?
    {
        let mut entry = entry.map_err(|e| format!("reading tarball: {e}"))?;
        let path = entry.path().map_err(|e| e.to_string())?.into_owned();
        if entry.header().entry_type().is_file()
            && path.file_name().and_then(|n| n.to_str()) == Some(bin_name)
        {
            let mut out = Vec::new();
            entry.read_to_end(&mut out).map_err(|e| e.to_string())?;
            if out.is_empty() {
                return Err(format!("{bin_name} in the tarball is empty"));
            }
            return Ok(out);
        }
    }
    Err(format!("tarball does not contain {bin_name}"))
}

pub fn download_verified(
    base: &str,
    manifest: &ReleaseManifest,
    target: &str,
) -> Result<Vec<u8>, String> {
    let asset = manifest
        .assets
        .get(target)
        .ok_or_else(|| format!("release {} has no build for {target}", manifest.version))?;
    if asset.contains('/') || asset.contains("..") {
        return Err(format!("refusing suspicious asset name {asset:?}"));
    }
    let dir = format!("{base}/releases/download/v{}", manifest.version);
    let sums = String::from_utf8(get_bytes(
        &format!("{dir}/SHA256SUMS"),
        Duration::from_secs(30),
    )?)
    .map_err(|_| "SHA256SUMS is not UTF-8".to_string())?;
    let expected = parse_sha256sums(&sums)
        .remove(asset)
        .ok_or_else(|| format!("SHA256SUMS lists no checksum for {asset}"))?;
    let tarball = get_bytes(&format!("{dir}/{asset}"), Duration::from_secs(600))?;
    let actual: String = Sha256::digest(&tarball)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    if actual != expected {
        return Err(format!(
            "checksum mismatch for {asset}: expected {expected}, got {actual}"
        ));
    }
    extract_binary(&tarball, BIN_NAME)
}

/// Replace `exe` with `bytes` atomically, after the new binary's own
/// `--version --json` matches the manifest's version AND storage format.
pub fn replace_executable(
    exe: &Path,
    bytes: &[u8],
    manifest: &ReleaseManifest,
) -> Result<(), String> {
    let exe = std::fs::canonicalize(exe).unwrap_or_else(|_| exe.to_path_buf());
    let dir = exe
        .parent()
        .ok_or_else(|| format!("{} has no parent directory", exe.display()))?;
    let staged = dir.join(format!(".{BIN_NAME}.update-{}", std::process::id()));
    std::fs::write(&staged, bytes).map_err(|e| {
        format!(
            "writing {} (is the install dir writable?): {e}",
            staged.display()
        )
    })?;
    let result =
        (|| {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755))
                    .map_err(|e| e.to_string())?;
            }
            let out = std::process::Command::new(&staged)
                .args(["--version", "--json"])
                .output()
                .map_err(|e| format!("running the downloaded binary: {e}"))?;
            let reported: ReleaseManifest = serde_json::from_slice(&out.stdout).map_err(|e| {
                format!(
                    "downloaded binary's --version --json is not a manifest ({e}): {:?}",
                    String::from_utf8_lossy(&out.stdout).trim()
                )
            })?;
            if reported.version != manifest.version
                || reported.storage_format != manifest.storage_format
            {
                return Err(format!(
                "downloaded binary reports {} / storageFormat {:?}, manifest promised {} / {:?}",
                reported.version, reported.storage_format, manifest.version, manifest.storage_format
            ));
            }
            std::fs::rename(&staged, &exe).map_err(|e| format!("replacing {}: {e}", exe.display()))
        })();
    if result.is_err() {
        let _ = std::fs::remove_file(&staged);
    }
    result
}

// ------------------------------------------------------------- serving gate

/// The profile dir gardend would use: `GARDEN_PROFILE_DIR`, else
/// `GARDEN_HEADLESS_APP_DATA_DIR/profiles/default` (see `profile_paths`).
pub fn configured_profile_dir() -> Option<PathBuf> {
    let env = |k: &str| {
        std::env::var_os(k)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    };
    env("GARDEN_PROFILE_DIR").or_else(|| {
        env("GARDEN_HEADLESS_APP_DATA_DIR")
            .map(|d| d.join("profiles").join(crate::runtime_config::PROFILE_ID))
    })
}

fn pid_alive(pid: u32) -> bool {
    if pid == 0 || pid == std::process::id() {
        return false;
    }
    if cfg!(target_os = "linux") {
        return Path::new("/proc").join(pid.to_string()).exists();
    }
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Evidence that some gardend is serving a graph right now. Empty = safe.
pub fn serving_evidence(profile_dir: Option<&Path>) -> Vec<String> {
    let mut evidence = Vec::new();
    if let Some(profile) = profile_dir {
        let manifest_path = profile.join(crate::runtime_config::LOOPBACK_MANIFEST_FILE);
        if let Ok(raw) = std::fs::read(&manifest_path) {
            if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&raw) {
                let pid = v
                    .get("pid")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(0) as u32;
                if pid_alive(pid) {
                    evidence.push(format!(
                        "gardend pid {pid} is serving profile {}",
                        profile.display()
                    ));
                }
                if let Some(port) = v.get("port").and_then(serde_json::Value::as_u64) {
                    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port as u16));
                    if port != 0
                        && std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(500))
                            .is_ok()
                    {
                        evidence.push(format!(
                            "the loopback port {port} in {} is accepting connections",
                            manifest_path.display()
                        ));
                    }
                }
            }
        }
    }
    if let Ok(out) = std::process::Command::new("pgrep")
        .args(["-x", BIN_NAME])
        .output()
    {
        for pid in String::from_utf8_lossy(&out.stdout)
            .split_whitespace()
            .filter_map(|p| p.parse::<u32>().ok())
            .filter(|p| *p != std::process::id())
        {
            evidence.push(format!("another gardend process is running (pid {pid})"));
        }
    }
    evidence
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateOutcome {
    UpToDate {
        latest: String,
    },
    Available {
        latest: String,
        storage_format_ok: Result<(), String>,
    },
    Installed {
        latest: String,
        path: PathBuf,
    },
}

/// `gardend --check-update` (`check_only`) / `gardend update`.
pub fn run_update(
    base: &str,
    exe: &Path,
    current: &str,
    check_only: bool,
    profile_dir: Option<&Path>,
) -> Result<UpdateOutcome, String> {
    let manifest = fetch_latest(base, Duration::from_secs(15))?;
    if !is_newer(&manifest.version, current) {
        return Ok(UpdateOutcome::UpToDate {
            latest: manifest.version,
        });
    }
    if check_only {
        return Ok(UpdateOutcome::Available {
            storage_format_ok: storage_format_gate(&manifest),
            latest: manifest.version,
        });
    }
    storage_format_gate(&manifest)?;
    let serving = serving_evidence(profile_dir);
    if !serving.is_empty() {
        return Err(format!(
            "refusing to self-update while a graph is being served ({}). Stop gardend \
             first — SIGTERM runs its forced final durable flush and it exits 0 once \
             flushed — then run `gardend update` again.",
            serving.join("; ")
        ));
    }
    let target = current_target().ok_or("no gardend release builds for this platform")?;
    let bytes = download_verified(base, &manifest, target)?;
    replace_executable(exe, &bytes, &manifest)?;
    Ok(UpdateOutcome::Installed {
        latest: manifest.version,
        path: exe.to_path_buf(),
    })
}

// ------------------------------------------------------------ startup check

/// Off with `GARDEN_NO_UPDATE_CHECK=1`, any non-empty `CI`, or inside a
/// platform cell (`GARDEN_CELL_GRAPH_ID` set) — cells update by image.
pub fn startup_check_disabled(get: impl Fn(&str) -> Option<String>) -> bool {
    let set = |k: &str| {
        get(k)
            .map(|v| {
                let v = v.trim();
                !v.is_empty() && v != "0" && v != "false"
            })
            .unwrap_or(false)
    };
    set("GARDEN_NO_UPDATE_CHECK") || set("CI") || set("GARDEN_CELL_GRAPH_ID")
}

pub fn cache_dir() -> Option<PathBuf> {
    if let Some(d) = std::env::var_os("GARDEN_CACHE_DIR").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(d));
    }
    if let Some(x) = std::env::var_os("XDG_CACHE_HOME").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(x).join("gardend"));
    }
    let home = PathBuf::from(std::env::var_os("HOME").filter(|v| !v.is_empty())?);
    Some(if cfg!(target_os = "macos") {
        home.join("Library/Caches/gardend")
    } else {
        home.join(".cache/gardend")
    })
}

pub fn notice(latest: &str, current: &str) -> String {
    format!("gardend {latest} available (you have {current}): run `gardend update` (containers: pull the new image)")
}

/// At most once a day; returns the notice line for the caller to log. Never
/// panics, never blocks the caller if run on its own thread.
pub fn startup_check() -> Option<String> {
    if startup_check_disabled(|k| std::env::var(k).ok()) {
        return None;
    }
    let stamp = cache_dir()?.join("update-check.json");
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    if let Ok(raw) = std::fs::read(&stamp) {
        let checked_at = serde_json::from_slice::<serde_json::Value>(&raw)
            .ok()
            .and_then(|v| v.get("checkedAt").and_then(serde_json::Value::as_u64))
            .unwrap_or(0);
        if now >= checked_at && now - checked_at < CHECK_INTERVAL.as_secs() {
            return None;
        }
    }
    let latest = fetch_latest(&release_base(), Duration::from_secs(5));
    if let Some(parent) = stamp.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&stamp, serde_json::json!({ "checkedAt": now }).to_string());
    match latest {
        Ok(m) if is_newer(&m.version, CURRENT_VERSION) => Some(notice(&m.version, CURRENT_VERSION)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    fn tempdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "gardend-update-{tag}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A real (tiny) static HTTP file server over `root`.
    fn serve(root: PathBuf) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let root = root.clone();
                std::thread::spawn(move || handle(stream, &root));
            }
        });
        format!("http://{addr}")
    }

    fn handle(mut stream: TcpStream, root: &Path) {
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut request_line = String::new();
        let _ = reader.read_line(&mut request_line);
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                break;
            }
        }
        let path = request_line.split_whitespace().nth(1).unwrap_or("/");
        let (status, body) = match std::fs::read(root.join(path.trim_start_matches('/'))) {
            Ok(b) if !path.contains("..") => ("200 OK", b),
            _ => ("404 Not Found", b"not found".to_vec()),
        };
        let _ = write!(
            stream,
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        let _ = stream.write_all(&body);
    }

    /// Publish release `version` whose "binary" is a script answering
    /// `--version --json` with `reports_format`; the manifest declares `declared_format`.
    fn publish(root: &Path, version: &str, declared_format: Option<u32>, reports_format: u32) {
        let target = current_target().unwrap();
        let asset = format!("gardend-v{version}-{target}.tar.gz");
        let script = format!(
            "#!/bin/sh\necho '{{\"name\":\"gardend\",\"version\":\"{version}\",\"storageFormat\":{reports_format}}}'\n"
        );
        let mut tar = tar::Builder::new(flate2::write::GzEncoder::new(
            Vec::new(),
            flate2::Compression::default(),
        ));
        let mut header = tar::Header::new_gnu();
        header.set_size(script.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        tar.append_data(
            &mut header,
            format!("gardend-v{version}-{target}/gardend"),
            script.as_bytes(),
        )
        .unwrap();
        let tarball = tar.into_inner().unwrap().finish().unwrap();
        let digest: String = Sha256::digest(&tarball)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let dir = root.join(format!("releases/download/v{version}"));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(&asset), &tarball).unwrap();
        std::fs::write(dir.join("SHA256SUMS"), format!("{digest}  {asset}\n")).unwrap();
        let manifest = ReleaseManifest {
            name: "gardend".into(),
            version: version.into(),
            storage_format: declared_format,
            assets: BTreeMap::from([(target.to_string(), asset)]),
        };
        let latest = root.join("releases/latest/download");
        std::fs::create_dir_all(&latest).unwrap();
        std::fs::write(
            latest.join("release.json"),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
    }

    fn installed(dir: &Path) -> PathBuf {
        let exe = dir.join("gardend");
        std::fs::write(&exe, "#!/bin/sh\necho old\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        exe
    }

    fn empty_profile() -> PathBuf {
        tempdir("profile")
    }

    #[test]
    fn version_json_declares_storage_format() {
        let v: ReleaseManifest = serde_json::from_str(&version_json()).unwrap();
        assert_eq!(v.name, "gardend");
        assert_eq!(v.version, CURRENT_VERSION);
        assert_eq!(v.storage_format, Some(STORAGE_FORMAT));
    }

    #[cfg(unix)]
    #[test]
    fn check_then_install_from_a_real_local_release() {
        let root = tempdir("rel");
        publish(&root, "9.9.9", Some(STORAGE_FORMAT), STORAGE_FORMAT);
        let base = serve(root);
        let dir = tempdir("inst");
        let exe = installed(&dir);
        let profile = empty_profile();

        let checked = run_update(&base, &exe, "0.1.0", true, Some(&profile)).unwrap();
        assert_eq!(
            checked,
            UpdateOutcome::Available {
                latest: "9.9.9".into(),
                storage_format_ok: Ok(())
            }
        );
        let serving = serving_evidence(Some(&profile));
        if !serving.is_empty() {
            // A developer's real gardend is running on this machine: the gate
            // must refuse, and that is the behavior under test here too.
            let err = run_update(&base, &exe, "0.1.0", false, Some(&profile)).unwrap_err();
            assert!(err.contains("being served"), "{err}");
            return;
        }
        let done = run_update(&base, &exe, "0.1.0", false, Some(&profile)).unwrap();
        assert!(matches!(done, UpdateOutcome::Installed { ref latest, .. } if latest == "9.9.9"));
        let out = std::process::Command::new(&exe).output().unwrap();
        assert!(String::from_utf8_lossy(&out.stdout).contains("\"version\":\"9.9.9\""));
        assert_eq!(
            std::fs::read_dir(&dir).unwrap().count(),
            1,
            "no staging debris"
        );
    }

    #[test]
    fn a_storage_format_change_is_refused_and_the_binary_untouched() {
        let root = tempdir("fmt");
        publish(&root, "9.9.9", Some(STORAGE_FORMAT + 1), STORAGE_FORMAT + 1);
        let base = serve(root);
        let dir = tempdir("inst-fmt");
        let exe = installed(&dir);
        let checked = run_update(&base, &exe, "0.1.0", true, None).unwrap();
        assert!(matches!(
            checked,
            UpdateOutcome::Available {
                storage_format_ok: Err(_),
                ..
            }
        ));
        let err = run_update(&base, &exe, "0.1.0", false, None).unwrap_err();
        assert!(err.contains("storage format"), "{err}");
        assert_eq!(
            std::fs::read_to_string(&exe).unwrap(),
            "#!/bin/sh\necho old\n"
        );
    }

    #[test]
    fn a_manifest_without_storage_format_is_refused() {
        let root = tempdir("nofmt");
        publish(&root, "9.9.9", None, STORAGE_FORMAT);
        let base = serve(root);
        let dir = tempdir("inst-nofmt");
        let exe = installed(&dir);
        let err = run_update(&base, &exe, "0.1.0", false, None).unwrap_err();
        assert!(err.contains("declares no storageFormat"), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn a_binary_that_lies_about_its_format_is_not_installed() {
        let root = tempdir("liar");
        publish(&root, "9.9.9", Some(STORAGE_FORMAT), STORAGE_FORMAT + 7);
        let base = serve(root);
        let dir = tempdir("inst-liar");
        let exe = installed(&dir);
        let manifest = fetch_latest(&base, Duration::from_secs(5)).unwrap();
        let bytes = download_verified(&base, &manifest, current_target().unwrap()).unwrap();
        let err = replace_executable(&exe, &bytes, &manifest).unwrap_err();
        assert!(err.contains("manifest promised"), "{err}");
        assert_eq!(
            std::fs::read_to_string(&exe).unwrap(),
            "#!/bin/sh\necho old\n"
        );
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn a_live_loopback_manifest_is_serving_evidence() {
        // A real listener standing in for a serving gardend's loopback port,
        // and this test process's parent as a live pid.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let profile = empty_profile();
        let parent = std::os::unix::process::parent_id();
        std::fs::write(
            profile.join("loopback.json"),
            serde_json::json!({ "pid": parent, "port": port }).to_string(),
        )
        .unwrap();
        let evidence = serving_evidence(Some(&profile));
        assert!(
            evidence
                .iter()
                .any(|e| e.contains(&format!("pid {parent}"))),
            "{evidence:?}"
        );
        assert!(
            evidence.iter().any(|e| e.contains(&format!("port {port}"))),
            "{evidence:?}"
        );
        drop(listener);
        std::fs::write(
            profile.join("loopback.json"),
            serde_json::json!({ "pid": 0, "port": port }).to_string(),
        )
        .unwrap();
        assert!(serving_evidence(Some(&profile))
            .iter()
            .all(|e| !e.contains("profile")));
    }

    #[test]
    fn startup_check_is_off_in_ci_cells_and_by_env() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |k: &str| {
                pairs
                    .iter()
                    .find(|(n, _)| *n == k)
                    .map(|(_, v)| v.to_string())
            }
        };
        assert!(startup_check_disabled(env(&[(
            "GARDEN_NO_UPDATE_CHECK",
            "1"
        )])));
        assert!(startup_check_disabled(env(&[("CI", "true")])));
        assert!(startup_check_disabled(env(&[(
            "GARDEN_CELL_GRAPH_ID",
            "sophia-cluster"
        )])));
        assert!(!startup_check_disabled(env(&[(
            "GARDEN_NO_UPDATE_CHECK",
            "0"
        )])));
        assert!(!startup_check_disabled(env(&[])));
    }

    #[test]
    fn versions_and_sums_parse() {
        assert!(is_newer("0.2.0", "0.1.9"));
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert_eq!(parse_version("1.2"), None);
        let a = "a".repeat(64);
        assert_eq!(
            parse_sha256sums(&format!("{a}  x.tar.gz\n")).get("x.tar.gz"),
            Some(&a)
        );
    }
}
