//! `gardend` self-update against GitHub Releases (bare-binary installs only;
//! container users pull a new `ghcr.io/sophia-labs/gardend` image instead).
//!
//! Crude on purpose: a release is three kinds of static file, so the whole
//! protocol works against any static file server:
//!
//! * `release.json` — `{"name","version","storageFormat","assets":{"<triple>":"<file>"}}`
//! * `SHA256SUMS` — `sha256sum` output over every tarball
//! * `gardend-v<version>-<target>.tar.gz` — contains the `gardend` binary
//!   and, from 0.1.2 on, the self-host web UI as `ui/` beside it
//!
//! The binary is always replaced. A `ui/` folder next to the installed binary
//! (what `gardend serve` serves) is replaced too when the archive carries one,
//! and so is each `services/<id>/` the archive carries (from 0.1.4 on, the
//! self-host chat, `services/vehicle/`; services the archive does not carry
//! are left alone). With no `ui/` folder there, only the binary changes. All
//! are unpacked beside their targets and renamed into place.
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
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const DEFAULT_RELEASE_BASE: &str = "https://github.com/sophia-labs/gardend";
pub const BIN_NAME: &str = "gardend";
pub const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");
pub const CHECK_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);
const MAX_DOWNLOAD_BYTES: u64 = 1 << 30;
/// The folder beside the binary, in an install and in a release archive, that
/// holds the self-host web UI.
pub const UI_DIR_NAME: &str = "ui";
/// The folder beside the binary, in an install and in a release archive, that
/// holds the services gardend supervises (`services/<id>/service.json`).
pub const SERVICES_DIR_NAME: &str = "services";

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
    find_binary(tarball, bin_name).map(|(_, bytes)| bytes)
}

/// The binary's path inside the archive (e.g. `gardend-v0.1.2-<target>/gardend`)
/// and its bytes.
fn find_binary(tarball: &[u8], bin_name: &str) -> Result<(PathBuf, Vec<u8>), String> {
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
            return Ok((path, out));
        }
    }
    Err(format!("tarball does not contain {bin_name}"))
}

/// Unpack the archive's `ui_root` folder (the `ui/` beside the binary in the
/// archive) into `dest`, which should not exist yet. `Ok(false)`: the archive
/// carries no such folder. Only plain files and folders are taken, under
/// plain relative names; a carried UI must hold an `index.html`.
fn extract_ui(tarball: &[u8], ui_root: &Path, dest: &Path) -> Result<bool, String> {
    extract_folder(tarball, ui_root, dest, "index.html")
}

/// The ids of the services the archive carries beside its binary
/// (`<prefix>/services/<id>/...`).
fn archive_service_ids(tarball: &[u8], prefix: &Path) -> Result<Vec<String>, String> {
    let root = prefix.join(SERVICES_DIR_NAME);
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(tarball));
    let mut ids = std::collections::BTreeSet::new();
    for entry in archive
        .entries()
        .map_err(|e| format!("reading tarball: {e}"))?
    {
        let entry = entry.map_err(|e| format!("reading tarball: {e}"))?;
        let path = entry.path().map_err(|e| e.to_string())?.into_owned();
        if let Ok(relative) = path.strip_prefix(&root) {
            if let Some(Component::Normal(id)) = relative.components().next() {
                let id = id.to_string_lossy();
                if !id.is_empty()
                    && id.len() <= 63
                    && id.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
                    && !id.starts_with('-')
                {
                    ids.insert(id.into_owned());
                }
            }
        }
    }
    Ok(ids.into_iter().collect())
}

/// [`extract_ui`] for any folder of the archive: `required` must be in it.
fn extract_folder(
    tarball: &[u8],
    ui_root: &Path,
    dest: &Path,
    required: &str,
) -> Result<bool, String> {
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(tarball));
    let mut carried = false;
    for entry in archive
        .entries()
        .map_err(|e| format!("reading tarball: {e}"))?
    {
        let mut entry = entry.map_err(|e| format!("reading tarball: {e}"))?;
        let path = entry.path().map_err(|e| e.to_string())?.into_owned();
        let Ok(relative) = path.strip_prefix(ui_root) else {
            continue;
        };
        if relative
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
        {
            return Err(format!(
                "refusing suspicious path {} in the tarball",
                path.display()
            ));
        }
        let into = dest.join(relative);
        let kind = entry.header().entry_type();
        if kind.is_dir() {
            std::fs::create_dir_all(&into)
                .map_err(|e| format!("unpacking {}: {e}", into.display()))?;
        } else if kind.is_file() {
            if let Some(parent) = into.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| format!("unpacking {}: {e}", parent.display()))?;
            }
            entry
                .unpack(&into)
                .map_err(|e| format!("unpacking {}: {e}", into.display()))?;
        } else {
            // Links and the like have no place in a static UI build.
            continue;
        }
        carried = true;
    }
    if carried && !dest.join(required).is_file() {
        return Err(format!(
            "the release's {} has no {required}: refusing to replace the installed one",
            ui_root.display()
        ));
    }
    Ok(carried)
}

/// The `ui/` folder next to the installed binary (symlinks of the binary
/// resolved), when there is a real folder there.
pub fn installed_ui_dir(exe: &Path) -> Option<PathBuf> {
    let exe = std::fs::canonicalize(exe).unwrap_or_else(|_| exe.to_path_buf());
    let ui = exe.parent()?.join(UI_DIR_NAME);
    std::fs::symlink_metadata(&ui)
        .is_ok_and(|meta| meta.is_dir())
        .then_some(ui)
}

pub fn download_verified(
    base: &str,
    manifest: &ReleaseManifest,
    target: &str,
) -> Result<Vec<u8>, String> {
    extract_binary(
        &download_verified_archive(base, manifest, target)?,
        BIN_NAME,
    )
}

/// The release archive for `target`, checked against the release's `SHA256SUMS`.
pub fn download_verified_archive(
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
    Ok(tarball)
}

/// Replace `exe` with `bytes` atomically, after the new binary's own
/// `--version --json` matches the manifest's version AND storage format.
pub fn replace_executable(
    exe: &Path,
    bytes: &[u8],
    manifest: &ReleaseManifest,
) -> Result<(), String> {
    let exe = std::fs::canonicalize(exe).unwrap_or_else(|_| exe.to_path_buf());
    let staged = stage_executable(&exe, bytes, manifest)?;
    std::fs::rename(&staged, &exe).map_err(|e| {
        let _ = std::fs::remove_file(&staged);
        format!("replacing {}: {e}", exe.display())
    })
}

/// Write `bytes` beside `exe` (a canonical path) and check the staged
/// binary's own `--version --json` against the manifest. The caller renames
/// the returned path into place; on `Err` nothing is left behind.
fn stage_executable(
    exe: &Path,
    bytes: &[u8],
    manifest: &ReleaseManifest,
) -> Result<PathBuf, String> {
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
            let out = output_when_not_busy(
                std::process::Command::new(&staged).args(["--version", "--json"]),
            )
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
            Ok(())
        })();
    match result {
        Ok(()) => Ok(staged),
        Err(error) => {
            let _ = std::fs::remove_file(&staged);
            Err(error)
        }
    }
}

/// `cmd.output()`, waiting out a moment where its executable is "busy".
///
/// Linux refuses to run a file that any process holds open for writing
/// (ETXTBSY). Our own handle on a staged binary is closed before we run it,
/// and Rust opens files close-on-exec, but close-on-exec acts only at exec: a
/// child that another thread forks while we write the file holds a copy of
/// our handle until that child execs. The window is short, so retry with a
/// short backoff (about 1.3 s in all) instead of failing the update.
fn output_when_not_busy(cmd: &mut std::process::Command) -> std::io::Result<std::process::Output> {
    for pause_ms in [10, 20, 40, 80, 160, 320, 640] {
        match cmd.output() {
            Err(e) if e.kind() == std::io::ErrorKind::ExecutableFileBusy => {
                std::thread::sleep(Duration::from_millis(pause_ms));
            }
            done => return done,
        }
    }
    cmd.output()
}

/// What an update replaced beside the binary.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReplacedFolders {
    /// The `ui/` folder, when it was replaced.
    pub ui: Option<PathBuf>,
    /// Each `services/<id>/` the archive carried, replaced or added.
    pub services: Vec<PathBuf>,
}

/// Install a verified release archive over `exe`. The binary is always
/// replaced ([`replace_executable`]'s checks). When a `ui/` folder sits next
/// to the binary and the archive carries one, the UI is replaced with it, and
/// so is each `services/<id>/` the archive carries (added when missing; a
/// service the archive does not carry is left alone): everything new is first
/// unpacked beside its target, then renamed into place (old folders aside, new
/// ones in, binary in), and a failed rename puts the old ones back. With no
/// `ui/` folder next to the binary this is exactly [`replace_executable`].
pub fn install_release(
    exe: &Path,
    tarball: &[u8],
    manifest: &ReleaseManifest,
) -> Result<ReplacedFolders, String> {
    let exe = std::fs::canonicalize(exe).unwrap_or_else(|_| exe.to_path_buf());
    let (binary_path, bytes) = find_binary(tarball, BIN_NAME)?;
    let Some(ui) = installed_ui_dir(&exe) else {
        replace_executable(&exe, &bytes, manifest)?;
        return Ok(ReplacedFolders::default());
    };
    let dir = exe
        .parent()
        .ok_or_else(|| format!("{} has no parent directory", exe.display()))?;
    let pid = std::process::id();
    let prefix = binary_path
        .parent()
        .unwrap_or_else(|| Path::new(""))
        .to_path_buf();
    // (installed folder, staged new one, where the old one waits)
    let mut swaps: Vec<(PathBuf, PathBuf, PathBuf)> = Vec::new();
    let discard = |swaps: &[(PathBuf, PathBuf, PathBuf)]| {
        for (_, staged, _) in swaps {
            let _ = std::fs::remove_dir_all(staged);
        }
    };
    let staged_ui = dir.join(format!(".{UI_DIR_NAME}.update-{pid}"));
    let old_ui = dir.join(format!(".{UI_DIR_NAME}.old-{pid}"));
    let _ = std::fs::remove_dir_all(&staged_ui);
    let _ = std::fs::remove_dir_all(&old_ui);
    match extract_ui(tarball, &prefix.join(UI_DIR_NAME), &staged_ui) {
        Ok(true) => swaps.push((ui.clone(), staged_ui, old_ui)),
        Ok(false) => {
            // An archive without ui/ (before 0.1.2): the binary only, as before.
            let _ = std::fs::remove_dir_all(&staged_ui);
            replace_executable(&exe, &bytes, manifest)?;
            return Ok(ReplacedFolders::default());
        }
        Err(error) => {
            let _ = std::fs::remove_dir_all(&staged_ui);
            return Err(error);
        }
    }
    let services = dir.join(SERVICES_DIR_NAME);
    for id in archive_service_ids(tarball, &prefix)? {
        let target = services.join(&id);
        let staged = services.join(format!(".{id}.update-{pid}"));
        let old = services.join(format!(".{id}.old-{pid}"));
        let _ = std::fs::remove_dir_all(&staged);
        let _ = std::fs::remove_dir_all(&old);
        let carried = std::fs::create_dir_all(&services)
            .map_err(|e| format!("creating {}: {e}", services.display()))
            .and_then(|()| {
                extract_folder(
                    tarball,
                    &prefix.join(SERVICES_DIR_NAME).join(&id),
                    &staged,
                    "service.json",
                )
            });
        match carried {
            Ok(true) => swaps.push((target, staged, old)),
            Ok(false) => {}
            Err(error) => {
                let _ = std::fs::remove_dir_all(&staged);
                discard(&swaps);
                return Err(error);
            }
        }
    }
    let staged_bin = match stage_executable(&exe, &bytes, manifest) {
        Ok(staged) => staged,
        Err(error) => {
            discard(&swaps);
            return Err(error);
        }
    };
    // Swap the folders in, one by one; on a failure put back what was done.
    let mut done: Vec<(PathBuf, PathBuf, PathBuf, bool)> = Vec::new();
    let undo = |done: &[(PathBuf, PathBuf, PathBuf, bool)]| {
        for (target, staged, old, existed) in done.iter().rev() {
            let _ = std::fs::rename(target, staged);
            if *existed {
                let _ = std::fs::rename(old, target);
            }
        }
    };
    let mut failure = None;
    for (target, staged, old) in &swaps {
        let existed = std::fs::symlink_metadata(target).is_ok();
        if existed {
            if let Err(e) = std::fs::rename(target, old) {
                failure = Some(format!("moving {} aside: {e}", target.display()));
                break;
            }
        }
        if let Err(e) = std::fs::rename(staged, target) {
            if existed {
                let _ = std::fs::rename(old, target);
            }
            failure = Some(format!("replacing {}: {e}", target.display()));
            break;
        }
        done.push((target.clone(), staged.clone(), old.clone(), existed));
    }
    if failure.is_none() {
        if let Err(e) = std::fs::rename(&staged_bin, &exe) {
            failure = Some(format!("replacing {}: {e}", exe.display()));
        }
    }
    if let Some(error) = failure {
        undo(&done);
        discard(&swaps);
        let _ = std::fs::remove_file(&staged_bin);
        return Err(error);
    }
    for (_, _, old) in &swaps {
        let _ = std::fs::remove_dir_all(old);
    }
    let mut replaced = ReplacedFolders::default();
    for (target, _, _) in swaps {
        if target == ui {
            replaced.ui = Some(target);
        } else {
            replaced.services.push(target);
        }
    }
    Ok(replaced)
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

pub(crate) fn pid_alive(pid: u32) -> bool {
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
        /// The `ui/` folder next to the binary, when it was replaced too.
        ui: Option<PathBuf>,
        /// The `services/<id>/` folders replaced (or added) beside it.
        services: Vec<PathBuf>,
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
    let tarball = download_verified_archive(base, &manifest, target)?;
    let replaced = install_release(exe, &tarball, &manifest)?;
    Ok(UpdateOutcome::Installed {
        latest: manifest.version,
        path: exe.to_path_buf(),
        ui: replaced.ui,
        services: replaced.services,
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
        publish_with_ui(root, version, declared_format, reports_format, &[]);
    }

    /// [`publish`], with `ui` files (relative path, contents) packed as `ui/`
    /// beside the binary, as release archives carry them from 0.1.2.
    fn publish_with_ui(
        root: &Path,
        version: &str,
        declared_format: Option<u32>,
        reports_format: u32,
        ui: &[(&str, &str)],
    ) {
        let files: Vec<(String, &str)> = ui
            .iter()
            .map(|(name, contents)| (format!("ui/{name}"), *contents))
            .collect();
        let files: Vec<(&str, &str)> = files
            .iter()
            .map(|(name, contents)| (name.as_str(), *contents))
            .collect();
        publish_files(root, version, declared_format, reports_format, &files);
    }

    /// [`publish`], with `files` (path beside the binary, contents) in the
    /// archive: `ui/...`, `services/<id>/...`.
    fn publish_files(
        root: &Path,
        version: &str,
        declared_format: Option<u32>,
        reports_format: u32,
        files: &[(&str, &str)],
    ) {
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
        for (name, contents) in files {
            let mut header = tar::Header::new_gnu();
            header.set_size(contents.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            tar.append_data(
                &mut header,
                format!("gardend-v{version}-{target}/{name}"),
                contents.as_bytes(),
            )
            .unwrap();
        }
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
        let _standins = standins();
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

    fn ui_at(dir: &Path, files: &[(&str, &str)]) {
        for (name, contents) in files {
            let path = dir.join("ui").join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, contents).unwrap();
        }
    }

    /// Tests here run stand-ins named `gardend`, which the serving gate's
    /// `pgrep -x gardend` takes for another gardend: the test that goes
    /// through the gate holds this, and every run of a stand-in takes it, so
    /// tests running side by side never trip the gate for each other.
    static GARDEND_STANDINS: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn standins() -> std::sync::MutexGuard<'static, ()> {
        GARDEND_STANDINS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn reports_version(exe: &Path, version: &str) -> bool {
        let _standins = standins();
        let out = std::process::Command::new(exe).output().unwrap();
        String::from_utf8_lossy(&out.stdout).contains(&format!("\"version\":\"{version}\""))
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    /// The release archive carries `ui/` and the install has one: both the
    /// binary and the UI are replaced, and nothing is left beside them.
    #[cfg(unix)]
    #[test]
    fn update_refreshes_the_selfhost_ui_beside_the_binary() {
        let root = tempdir("rel-ui");
        let new_ui = [
            ("index.html", "<title>new</title>"),
            ("assets/index-N3w.js", "new()"),
        ];
        publish_with_ui(
            &root,
            "9.9.9",
            Some(STORAGE_FORMAT),
            STORAGE_FORMAT,
            &new_ui,
        );
        let base = serve(root);
        let dir = tempdir("inst-ui");
        let exe = installed(&dir);
        ui_at(
            &dir,
            &[
                ("index.html", "<title>old</title>"),
                ("assets/index-0ld.js", "old()"),
            ],
        );

        let manifest = fetch_latest(&base, Duration::from_secs(5)).unwrap();
        let tarball =
            download_verified_archive(&base, &manifest, current_target().unwrap()).unwrap();
        let replaced = install_release(&exe, &tarball, &manifest).unwrap();

        let real = std::fs::canonicalize(&dir).unwrap();
        assert_eq!(replaced.ui, Some(real.join("ui")));
        assert!(replaced.services.is_empty());
        assert!(reports_version(&exe, "9.9.9"));
        assert_eq!(
            std::fs::read_to_string(dir.join("ui/index.html")).unwrap(),
            "<title>new</title>"
        );
        assert!(dir.join("ui/assets/index-N3w.js").is_file());
        assert!(
            !dir.join("ui/assets/index-0ld.js").exists(),
            "the old UI is gone, not merged"
        );
        assert_eq!(names(&dir), ["gardend", "ui"], "no staging debris");
    }

    /// From 0.1.4 the archive carries `services/vehicle/` (the self-host
    /// chat): an update replaces each service the archive carries, adds one
    /// the install lacks, and leaves the person's own services alone. A
    /// carried service without its `service.json` changes nothing at all.
    #[cfg(unix)]
    #[test]
    fn update_refreshes_the_services_the_archive_carries() {
        let root = tempdir("rel-svc");
        publish_files(
            &root,
            "9.9.9",
            Some(STORAGE_FORMAT),
            STORAGE_FORMAT,
            &[
                ("ui/index.html", "<title>new</title>"),
                ("services/vehicle/service.json", "{\"v\":2}"),
                ("services/vehicle/app/main.mjs", "new()"),
                ("services/tools/service.json", "{}"),
            ],
        );
        let base = serve(root);
        let manifest = fetch_latest(&base, Duration::from_secs(5)).unwrap();
        let target = current_target().unwrap();
        let tarball = download_verified_archive(&base, &manifest, target).unwrap();

        let dir = tempdir("inst-svc");
        let exe = installed(&dir);
        ui_at(&dir, &[("index.html", "<title>old</title>")]);
        for (path, contents) in [
            ("services/vehicle/service.json", "{\"v\":1}"),
            ("services/vehicle/app/dist/main.js", "old()"),
            ("services/mine/service.json", "{\"mine\":true}"),
        ] {
            let path = dir.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, contents).unwrap();
        }
        let replaced = install_release(&exe, &tarball, &manifest).unwrap();
        let real = std::fs::canonicalize(&dir).unwrap();
        assert_eq!(replaced.ui, Some(real.join("ui")));
        assert_eq!(
            replaced.services,
            vec![real.join("services/tools"), real.join("services/vehicle")]
        );
        assert!(reports_version(&exe, "9.9.9"));
        assert_eq!(
            std::fs::read_to_string(dir.join("services/vehicle/service.json")).unwrap(),
            "{\"v\":2}"
        );
        assert!(dir.join("services/vehicle/app/main.mjs").is_file());
        assert!(
            !dir.join("services/vehicle/app/dist").exists(),
            "the old service is gone, not merged"
        );
        assert!(dir.join("services/tools/service.json").is_file(), "added");
        assert_eq!(
            std::fs::read_to_string(dir.join("services/mine/service.json")).unwrap(),
            "{\"mine\":true}",
            "a service the archive does not carry is left alone"
        );
        assert_eq!(names(&dir), ["gardend", "services", "ui"], "no staging debris");
        assert_eq!(names(&dir.join("services")), ["mine", "tools", "vehicle"]);

        let broken_root = tempdir("rel-svc-broken");
        publish_files(
            &broken_root,
            "9.9.9",
            Some(STORAGE_FORMAT),
            STORAGE_FORMAT,
            &[
                ("ui/index.html", "<title>new</title>"),
                ("services/vehicle/app/main.mjs", "no manifest"),
            ],
        );
        let broken_base = serve(broken_root);
        let broken = download_verified_archive(&broken_base, &manifest, target).unwrap();
        let dir = tempdir("inst-svc-broken");
        let exe = installed(&dir);
        ui_at(&dir, &[("index.html", "<title>old</title>")]);
        let err = install_release(&exe, &broken, &manifest).unwrap_err();
        assert!(err.contains("no service.json"), "{err}");
        assert_eq!(std::fs::read_to_string(&exe).unwrap(), "#!/bin/sh\necho old\n");
        assert_eq!(
            std::fs::read_to_string(dir.join("ui/index.html")).unwrap(),
            "<title>old</title>"
        );
        assert_eq!(names(&dir), ["gardend", "services", "ui"]);
        assert!(names(&dir.join("services")).is_empty(), "no staging debris");
    }

    /// Without a `ui/` folder next to the binary, an update is what it always
    /// was: the binary only, even from an archive that carries `ui/`. With one,
    /// an archive without `ui/` (an older release) leaves it alone, and an
    /// archive whose `ui/` has no index.html changes nothing at all.
    #[cfg(unix)]
    #[test]
    fn update_without_a_selfhost_ui_folder_replaces_only_the_binary() {
        let root = tempdir("rel-noui");
        publish_with_ui(
            &root,
            "9.9.9",
            Some(STORAGE_FORMAT),
            STORAGE_FORMAT,
            &[("index.html", "new")],
        );
        let base = serve(root);
        let manifest = fetch_latest(&base, Duration::from_secs(5)).unwrap();
        let target = current_target().unwrap();
        let carrying = download_verified_archive(&base, &manifest, target).unwrap();

        let dir = tempdir("inst-noui");
        let exe = installed(&dir);
        assert_eq!(
            install_release(&exe, &carrying, &manifest).unwrap(),
            ReplacedFolders::default()
        );
        assert!(reports_version(&exe, "9.9.9"));
        assert_eq!(names(&dir), ["gardend"]);

        let bare_root = tempdir("rel-bare");
        publish(&bare_root, "9.9.9", Some(STORAGE_FORMAT), STORAGE_FORMAT);
        let bare_base = serve(bare_root);
        let bare = download_verified_archive(&bare_base, &manifest, target).unwrap();
        let dir = tempdir("inst-keepui");
        let exe = installed(&dir);
        ui_at(&dir, &[("index.html", "old")]);
        assert_eq!(
            install_release(&exe, &bare, &manifest).unwrap(),
            ReplacedFolders::default()
        );
        assert!(reports_version(&exe, "9.9.9"));
        assert_eq!(
            std::fs::read_to_string(dir.join("ui/index.html")).unwrap(),
            "old"
        );
        assert_eq!(names(&dir), ["gardend", "ui"]);

        let broken_root = tempdir("rel-broken-ui");
        publish_with_ui(
            &broken_root,
            "9.9.9",
            Some(STORAGE_FORMAT),
            STORAGE_FORMAT,
            &[("app.js", "x")],
        );
        let broken_base = serve(broken_root);
        let broken = download_verified_archive(&broken_base, &manifest, target).unwrap();
        let dir = tempdir("inst-brokenui");
        let exe = installed(&dir);
        ui_at(&dir, &[("index.html", "old")]);
        let err = install_release(&exe, &broken, &manifest).unwrap_err();
        assert!(err.contains("no index.html"), "{err}");
        assert_eq!(
            std::fs::read_to_string(&exe).unwrap(),
            "#!/bin/sh\necho old\n"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("ui/index.html")).unwrap(),
            "old"
        );
        assert_eq!(names(&dir), ["gardend", "ui"]);
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

    /// Linux will not run a file that a process holds open for writing
    /// (ETXTBSY), and a child forked by another thread holds a copy of the
    /// handle a staged binary was written through until that child execs.
    /// The staged binary's check waits such a moment out.
    #[cfg(unix)]
    #[test]
    fn a_briefly_busy_executable_runs_once_its_writer_lets_go() {
        use std::os::unix::fs::PermissionsExt;
        let tool = tempdir("busy").join("busy-tool");
        std::fs::write(&tool, "#!/bin/sh\necho ran\n").unwrap();
        std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();
        // The handle a forked child would hold, let go of after 100 ms.
        let writer = std::fs::OpenOptions::new().write(true).open(&tool).unwrap();
        let busy = std::process::Command::new(&tool).output();
        if cfg!(target_os = "linux") {
            assert_eq!(
                busy.unwrap_err().kind(),
                std::io::ErrorKind::ExecutableFileBusy,
                "the case under test: a file open for writing does not run"
            );
        }
        let release = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            drop(writer);
        });
        let out = output_when_not_busy(&mut std::process::Command::new(&tool)).unwrap();
        release.join().unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout), "ran\n");
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
        // A dead manifest: no live pid and no port. Re-probing the dropped port raced the parallel
        // suite on CI (another test can bind the same ephemeral port, and the "accepting
        // connections" evidence names this "profile" tempdir).
        std::fs::write(
            profile.join("loopback.json"),
            serde_json::json!({ "pid": 0, "port": 0 }).to_string(),
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
