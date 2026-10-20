//! The local place: `gardend serve`, started by the app beside itself with everything it carries.
//!
//! The app is self-hosted Sophia in a native window: the same gardend, the same UI, the same link. It carries, in its
//! resources, the Shrubbery UI (`ui/`), the local services gardend runs (`services/`: chat, the Vehicle, and Sophia
//! Compute, Choreograph), a Node.js for them (`node/bin/node`) and pdfium for reading scans (`lib/libpdfium.dylib`).
//! Tesseract comes from the computer (`brew install tesseract`), as `gardend` itself expects.
//!
//! The profile is gardend's own default (`~/Library/Application Support/Sophia`), unless `SOPHIA_LOCAL_PROFILE` or the
//! one line of `<config>/profile` names another folder (`~/sophia`, say). The window opens the link `gardend serve`
//! prints once it is ready (`http://127.0.0.1:<port>/#key=…`, the profile's key). If a gardend already serves the
//! profile (a `gardend service` left running, or an app that quit without stopping it), the app opens that one.

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::sync::Mutex;
use std::time::Duration;

use tauri::{AppHandle, Manager, Url};

/// gardend's own port; the app keeps it so a link opened in a browser keeps working while the app runs.
const PORT: u16 = 7674;

/// The port: `SOPHIA_LOCAL_PORT` (to try the app beside a gardend already on 7674), else gardend's own.
pub fn port() -> u16 {
    std::env::var("SOPHIA_LOCAL_PORT").ok().and_then(|p| p.trim().parse().ok()).unwrap_or(PORT)
}
/// How long a profile may take to open (a large one replays its logs first).
const READY_WITHIN: Duration = Duration::from_secs(300);
/// How long gardend may take to flush and stop when the app quits.
const STOP_WITHIN: Duration = Duration::from_secs(90);

pub struct LocalCell {
    child: Mutex<Option<Child>>,
    link: Mutex<Option<Url>>,
}

impl LocalCell {
    pub fn new() -> Self {
        Self { child: Mutex::new(None), link: Mutex::new(None) }
    }

    /// The local place's link, once gardend is ready. Starts gardend unless it is running; blocks until it is ready.
    pub fn open(&self, app: &AppHandle) -> Result<Url, String> {
        let mut child = self.child.lock().map_err(|_| "local cell lock poisoned")?;
        if let Some(running) = child.as_mut() {
            if running.try_wait().map_err(|e| e.to_string())?.is_none() {
                if let Some(link) = self.link.lock().map_err(|_| "link lock poisoned")?.clone() {
                    return Ok(link);
                }
            }
        }
        let bin = gardend_binary(app)?;
        let profile = profile_dir(app)?;
        let mut command = Command::new(&bin);
        command.arg("serve").arg("--port").arg(port().to_string());
        if let Some(dir) = &profile {
            command.arg("--profile").arg(dir);
        }
        command.envs(carried_env(app)).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut spawned = command.spawn().map_err(|e| format!("start gardend at {}: {e}", bin.display()))?;

        // gardend says "Open Sophia: <link>" once it is ready (or finds one already serving the profile). Off a terminal
        // it withholds the key from that line, so the link itself comes from `gardend url`. All it says goes to the log.
        let (tx, rx) = mpsc::channel::<()>();
        let stdout = spawned.stdout.take().expect("piped stdout");
        let stderr = spawned.stderr.take().expect("piped stderr");
        let ready_on_stderr = tx.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                log_line(&line);
                if line.contains("Open Sophia:") {
                    let _ = tx.send(());
                }
            }
        });
        // What gardend says on stderr: logged as it comes, and its last few lines kept to explain an early exit.
        let said = std::sync::Arc::new(Mutex::new(String::new()));
        let said_by_gardend = said.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                log_line(&line);
                if line.contains("Open Sophia:") {
                    let _ = ready_on_stderr.send(());
                }
                if let Ok(mut said) = said_by_gardend.lock() {
                    said.push_str(&line);
                    said.push('\n');
                    if said.len() > 4096 {
                        let cut = said.len() - 4096;
                        let cut = (cut..said.len()).find(|&i| said.is_char_boundary(i)).unwrap_or(0);
                        said.drain(..cut);
                    }
                }
            }
        });

        let started = std::time::Instant::now();
        loop {
            if rx.recv_timeout(Duration::from_millis(250)).is_ok() {
                let link = existing_link(&bin, profile.as_ref())?;
                *self.link.lock().map_err(|_| "link lock poisoned")? = Some(link.clone());
                // When another gardend serves the profile, this one has already exited; there is nothing of ours to stop.
                *child = Some(spawned);
                return Ok(link);
            }
            if let Some(status) = spawned.try_wait().map_err(|e| e.to_string())? {
                // Give the readers a moment to finish.
                std::thread::sleep(Duration::from_millis(300));
                if rx.try_recv().is_ok() {
                    return existing_link(&bin, profile.as_ref());
                }
                let words = said.lock().map(|s| s.clone()).unwrap_or_default();
                return Err(format!("gardend stopped ({status}): {}", words.trim()));
            }
            if started.elapsed() > READY_WITHIN {
                terminate(&mut spawned);
                return Err("gardend did not become ready in time".into());
            }
        }
    }

    /// The link, when gardend is ready.
    pub fn known_link(&self) -> Option<Url> {
        self.link.lock().ok().and_then(|link| link.clone())
    }

    /// Flush and stop the gardend this app started (never one it only opened).
    pub fn stop(&self) {
        if let Ok(mut child) = self.child.lock() {
            if let Some(mut running) = child.take() {
                terminate(&mut running);
            }
        }
        if let Ok(mut link) = self.link.lock() {
            *link = None;
        }
    }
}

/// The full link of the gardend serving the profile (`gardend url`, which prints it with the key).
fn existing_link(bin: &PathBuf, profile: Option<&PathBuf>) -> Result<Url, String> {
    let mut command = Command::new(bin);
    command.arg("url").arg("--port").arg(port().to_string());
    if let Some(dir) = profile {
        command.arg("--profile").arg(dir);
    }
    let out = command.stdin(Stdio::null()).output().map_err(|e| format!("gardend url: {e}"))?;
    String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .find_map(|word| Url::parse(word).ok().filter(|u| u.scheme() == "http"))
        .ok_or_else(|| "gardend is ready, but `gardend url` gave no link".to_string())
}

/// What `gardend serve` needs from the app: where its UI, services, Node.js and pdfium are, and a PATH a Finder launch
/// lacks (Homebrew's, for tesseract).
fn carried_env(app: &AppHandle) -> Vec<(String, String)> {
    let mut env = vec![("GARDEN_NO_UPDATE_CHECK".to_string(), "1".to_string())];
    let resources = app.path().resource_dir().ok();
    let mut path: Vec<String> = Vec::new();
    if let Some(dir) = &resources {
        let ui = dir.join("ui");
        if ui.join("index.html").is_file() {
            env.push(("GARDEN_UI_DIR".into(), ui.to_string_lossy().into_owned()));
        }
        let services = dir.join("services");
        if services.is_dir() {
            env.push(("GARDEN_SERVICES_DIR".into(), services.to_string_lossy().into_owned()));
        }
        let node = dir.join("node").join("bin");
        if node.join("node").is_file() {
            path.push(node.to_string_lossy().into_owned());
        }
        let pdfium = dir.join("lib").join("libpdfium.dylib");
        if pdfium.is_file() {
            env.push(("GARDEN_PDFIUM_LIB".into(), pdfium.to_string_lossy().into_owned()));
        }
    }
    path.extend(["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin", "/bin", "/usr/sbin", "/sbin"].map(String::from));
    env.push(("PATH".into(), path.join(":")));
    env
}

#[cfg(unix)]
fn terminate(child: &mut Child) {
    // SIGTERM lets gardend stop its services, flush every graph and release the profile.
    let _ = Command::new("kill").arg("-TERM").arg(child.id().to_string()).status();
    let started = std::time::Instant::now();
    while started.elapsed() < STOP_WITHIN {
        if matches!(child.try_wait(), Ok(Some(_))) {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(not(unix))]
fn terminate(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

/// The profile folder the person chose (`SOPHIA_LOCAL_PROFILE`, else the one line of `<config>/profile`), or None for
/// gardend's own default.
pub fn profile_dir(app: &AppHandle) -> Result<Option<PathBuf>, String> {
    let chosen = std::env::var("SOPHIA_LOCAL_PROFILE").ok().or_else(|| {
        let file = app.path().app_config_dir().ok()?.join("profile");
        std::fs::read_to_string(file).ok()
    });
    Ok(chosen.map(|raw| raw.trim().to_string()).filter(|raw| !raw.is_empty()).map(|raw| expand_home(&raw)))
}

/// The folder the local place keeps its data in, for "Show Local Data in Finder".
pub fn data_dir(app: &AppHandle) -> Result<PathBuf, String> {
    if let Some(dir) = profile_dir(app)? {
        return Ok(dir);
    }
    let home = std::env::var("HOME").map_err(|_| "HOME is not set")?;
    Ok(PathBuf::from(home).join("Library/Application Support/Sophia"))
}

fn expand_home(raw: &str) -> PathBuf {
    match (raw.strip_prefix("~/"), std::env::var("HOME")) {
        (Some(rest), Ok(home)) => PathBuf::from(home).join(rest),
        _ => PathBuf::from(raw),
    }
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

/// Where the app keeps what gardend says: `~/Library/Logs/Sophia/app.log`.
pub fn log_file() -> Option<PathBuf> {
    std::env::var("HOME").ok().map(|home| PathBuf::from(home).join("Library/Logs/Sophia/app.log"))
}

/// One line of what gardend said, appended to the log. A line that would carry the key is not kept.
fn log_line(line: &str) {
    if line.contains("key=") && !line.contains("key=…") {
        return;
    }
    let Some(path) = log_file() else { return };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = std::io::Write::write_all(&mut file, format!("{line}\n").as_bytes());
    }
}
