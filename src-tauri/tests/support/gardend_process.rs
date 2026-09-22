//! Reusable drivers for the real compiled headless `gardend` example.

use reqwest::{Client, StatusCode};
use serde::Deserialize;
use std::{
    collections::HashMap,
    fs::File,
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus},
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};
use uuid::Uuid;

/// Per-test scratch root that is removed after all child guards have dropped.
pub struct ScratchDir {
    path: PathBuf,
}

impl ScratchDir {
    pub fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!("garden-u8-{label}-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&path).expect("create test scratch dir");
        Self { path }
    }

    pub fn child(&self, name: &str) -> PathBuf {
        let path = self.path.join(name);
        std::fs::create_dir_all(&path).expect("create test scratch child");
        path
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        if std::env::var_os("GARDEN_TEST_KEEP_SCRATCH").is_some() {
            eprintln!("preserving test scratch {}", self.path.display());
            return;
        }
        if let Err(error) = std::fs::remove_dir_all(&self.path) {
            if error.kind() != std::io::ErrorKind::NotFound {
                eprintln!(
                    "warning: failed to remove test scratch {}: {error}",
                    self.path.display()
                );
            }
        }
    }
}

pub fn gardend_binary() -> PathBuf {
    static BINARY: OnceLock<PathBuf> = OnceLock::new();
    BINARY
        .get_or_init(|| {
            let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
            let status = Command::new(env!("CARGO"))
                .args([
                    "build",
                    "--no-default-features",
                    "--features",
                    "headless",
                    "--example",
                    "gardend",
                ])
                .env("CARGO_NET_OFFLINE", "true")
                .current_dir(&manifest_dir)
                .status()
                .expect("build gardend");
            assert!(status.success(), "gardend build failed: {status}");
            let binary = manifest_dir.join("target/debug/examples/gardend");
            assert!(binary.is_file(), "missing {}", binary.display());
            binary
        })
        .clone()
}

pub struct ChildGuard {
    pub child: Child,
    profile_dir: PathBuf,
}

impl ChildGuard {
    pub fn is_running(&mut self) -> bool {
        self.child.try_wait().expect("poll gardend").is_none()
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        expected_manifest_pids()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&self.profile_dir);
    }
}

fn expected_manifest_pids() -> &'static Mutex<HashMap<PathBuf, u32>> {
    static EXPECTED: OnceLock<Mutex<HashMap<PathBuf, u32>>> = OnceLock::new();
    EXPECTED.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn wait_with_timeout(child: &mut Child, timeout: Duration) -> Option<ExitStatus> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(Some(status)) = child.try_wait() {
            return Some(status);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

pub fn wait_until(timeout: Duration, mut condition: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if condition() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    condition()
}

pub fn log_files(dir: &Path, label: &str) -> (File, File) {
    std::fs::create_dir_all(dir).expect("create log dir");
    (
        File::create(dir.join(format!("{label}.stdout.log"))).expect("create stdout log"),
        File::create(dir.join(format!("{label}.stderr.log"))).expect("create stderr log"),
    )
}

pub fn read_log(dir: &Path, label: &str, stream: &str) -> String {
    std::fs::read_to_string(dir.join(format!("{label}.{stream}.log"))).unwrap_or_default()
}

pub fn wait_for_log(
    dir: &Path,
    label: &str,
    stream: &str,
    needle: &str,
    timeout: Duration,
) -> bool {
    wait_until(timeout, || read_log(dir, label, stream).contains(needle))
}

pub fn prime_graph(profile_dir: &Path, graph_id: &str) {
    let graph_dir = profile_dir.join("graphs").join(graph_id);
    std::fs::create_dir_all(&graph_dir).expect("create graph dir");
    let now = "2026-07-29T00:00:00.000Z";
    let record = serde_json::json!({
        "graphId": graph_id, "title": graph_id, "status": "active", "origin": "local",
        "providerId": "local-profile", "localPath": graph_dir.to_string_lossy(),
        "createdAt": now, "updatedAt": now, "capabilities": []
    });
    std::fs::write(
        graph_dir.join("graph.json"),
        serde_json::to_vec_pretty(&record).unwrap(),
    )
    .expect("write graph.json");
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoopbackManifest {
    pub port: u16,
    pub token: String,
    pub pid: u32,
}

#[derive(Clone)]
pub struct LoopbackEndpoint {
    pub port: u16,
    pub token: String,
    client: Client,
}

impl LoopbackEndpoint {
    pub fn from_profile(profile_dir: &Path, timeout: Duration) -> Option<Self> {
        let deadline = Instant::now() + timeout;
        let path = profile_dir.join("loopback.json");
        while Instant::now() < deadline {
            if let Ok(bytes) = std::fs::read(&path) {
                if let Ok(manifest) = serde_json::from_slice::<LoopbackManifest>(&bytes) {
                    // Hydration legitimately restores the predecessor's
                    // loopback.json before this process writes its own. If
                    // the predecessor is still alive, probing that stale
                    // endpoint can even succeed and silently drive the wrong
                    // process. Bind readiness to the child PID registered by
                    // spawn_gardend, not merely to "some healthy manifest".
                    let expected_pid = expected_manifest_pids()
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .get(profile_dir)
                        .copied();
                    if expected_pid.is_some_and(|pid| manifest.pid != pid) {
                        std::thread::sleep(Duration::from_millis(25));
                        continue;
                    }
                    // The manifest is written as the API comes up, just
                    // before gardend arms its shutdown select. Give that
                    // final boot continuation one scheduler turn so an
                    // immediate test SIGTERM exercises graceful shutdown
                    // instead of the OS default action.
                    std::thread::sleep(Duration::from_millis(100));
                    return Some(Self {
                        port: manifest.port,
                        token: manifest.token,
                        client: Client::new(),
                    });
                }
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        None
    }

    fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.port)
    }

    pub async fn health(&self) -> (StatusCode, serde_json::Value) {
        let response = self
            .client
            .get(self.url("/health"))
            .send()
            .await
            .expect("GET /health");
        let status = response.status();
        let value = response.json().await.expect("parse /health JSON");
        (status, value)
    }

    pub async fn put_document(
        &self,
        graph_id: &str,
        document_id: &str,
        title: &str,
        content: &str,
    ) -> (StatusCode, serde_json::Value) {
        self.try_put_document(graph_id, document_id, title, content)
            .await
            .expect("PUT document")
    }

    pub async fn try_put_document(
        &self,
        graph_id: &str,
        document_id: &str,
        title: &str,
        content: &str,
    ) -> Result<(StatusCode, serde_json::Value), reqwest::Error> {
        let response = self
            .client
            .put(self.url(&format!("/documents/{graph_id}/{document_id}")))
            .bearer_auth(&self.token)
            .json(&serde_json::json!({
                "title": title,
                "blocks": [{
                    "id": format!("{document_id}-block"),
                    "type": "paragraph",
                    "content": content
                }]
            }))
            .send()
            .await?;
        let status = response.status();
        let bytes = response.bytes().await?;
        let value = serde_json::from_slice(&bytes).unwrap_or_else(
            |_| serde_json::json!({"raw": String::from_utf8_lossy(&bytes).into_owned()}),
        );
        Ok((status, value))
    }

    pub async fn get_document(
        &self,
        graph_id: &str,
        document_id: &str,
    ) -> (StatusCode, serde_json::Value) {
        let response = self
            .client
            .get(self.url(&format!("/documents/{graph_id}/{document_id}")))
            .bearer_auth(&self.token)
            .send()
            .await
            .expect("GET document");
        let status = response.status();
        let bytes = response.bytes().await.expect("read GET response");
        let value = serde_json::from_slice(&bytes).unwrap_or_else(
            |_| serde_json::json!({"raw": String::from_utf8_lossy(&bytes).into_owned()}),
        );
        (status, value)
    }
}

pub fn durable_current_snapshot_id(durable_dir: &Path) -> Option<String> {
    std::fs::read_to_string(durable_dir.join("CURRENT"))
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

pub fn durable_current_seq(durable_dir: &Path) -> Option<u64> {
    durable_current_snapshot_id(durable_dir)
        .and_then(|name| name.strip_prefix("snap-").and_then(|seq| seq.parse().ok()))
}

pub fn wait_for_current_seq(durable_dir: &Path, seq: u64, timeout: Duration) -> bool {
    wait_until(timeout, || durable_current_seq(durable_dir) == Some(seq))
}

pub fn list_snap_dirs(durable_dir: &Path) -> Vec<String> {
    let mut entries = list_prefixed(durable_dir, "snap-");
    entries.sort();
    entries
}

pub fn list_orphan_dirs(durable_dir: &Path) -> Vec<String> {
    let mut entries = list_prefixed(durable_dir, ".orphan-");
    entries.sort();
    entries
}

fn list_prefixed(dir: &Path, prefix: &str) -> Vec<String> {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            entry
                .file_type()
                .ok()
                .filter(|file_type| file_type.is_dir())
                .map(|_| entry.file_name().to_string_lossy().into_owned())
        })
        .filter(|name| name.starts_with(prefix))
        .collect()
}

pub fn semantic_document(value: &serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "id": value.get("id").cloned().unwrap_or(serde_json::Value::Null),
        "title": value.get("title").cloned().unwrap_or(serde_json::Value::Null),
        "blocks": value.get("blocks").cloned().unwrap_or(serde_json::Value::Null),
    })
}

pub fn send_sigterm(child: &Child) {
    let status = Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status()
        .expect("send SIGTERM");
    assert!(status.success(), "kill -TERM failed: {status}");
}

pub struct GardendConfig {
    pub profile_dir: PathBuf,
    pub durable_dir: PathBuf,
    pub graph_id: String,
    pub loopback_host: String,
    pub loopback_port: u16,
    pub extra_env: HashMap<String, String>,
    pub log_dir: PathBuf,
    pub log_label: String,
}

pub fn spawn_gardend(config: GardendConfig) -> (ChildGuard, PathBuf) {
    std::fs::create_dir_all(&config.profile_dir).expect("profile dir");
    std::fs::create_dir_all(&config.durable_dir).expect("durable dir");
    let (stdout, stderr) = log_files(&config.log_dir, &config.log_label);
    let mut command = Command::new(gardend_binary());
    // Process-boundary tests must not inherit a developer shell's canary
    // lease/capture envelope. In particular, Z7 proves the epoch-absent
    // legacy/default-off paths; per-test `extra_env` below is the only
    // authority for opting a child back into these protocols.
    for variable in [
        "GARDEN_CELL_ID",
        "GARDEN_CELL_OWNER",
        "GARDEN_CELL_GRAPH_GENERATION",
        "GARDEN_CELL_REGISTRY_REVISION",
        "GARDEN_CELL_REGISTRY_TABLE",
        "GARDEN_CELL_REGISTRY_SNAPSHOT_JSON",
        "GARDEN_CELL_LEASE_SECRET",
        "GARDEN_DURABLE_EPOCH",
        "GARDEN_LEASE_URL",
        "GARDEN_LEASE_MODE",
        "GARDEN_LEASE_RENEW_MS",
        "GARDEN_LEASE_MARGIN_MS",
        "GARDEN_LEASE_FENCED_MAX_MS",
        "GARDEN_LEASE_PUBLISH_TIMEOUT_MS",
        "GARDEN_LEASE_LAST_SNAP",
        "GARDEN_LEASE_PENDING_SNAP",
        "GARDEN_LEASE_TEST_BYPASS_GATE_A",
        "GARDEN_LEASE_TEST_POST_HYDRATE_DELAY_MS",
        "GARDEN_LEASE_TEST_PRE_LOOPBACK_DELAY_MS",
        "GARDEN_LEASE_TEST_POST_LOOPBACK_PRE_READY_DELAY_MS",
        "GARDEN_CELL_MACHINE_ID",
        "GARDEN_CELL_MACHINE_RUN_ID",
        "GARDEN_LOOPBACK_TOKEN",
        "SOPHIA_OBSERVATORY_CAPTURE_ENABLED",
        "SOPHIA_OBSERVATORY_CONTRACT_BUNDLE_SHA256",
        "GARDEN_CAPTURE_QUEUE_CAPACITY",
        "GARDEN_CAPTURE_HEARTBEAT_SECONDS",
    ] {
        command.env_remove(variable);
    }
    command
        .env("GARDEN_PROFILE_DIR", &config.profile_dir)
        .env("GARDEN_DURABLE_DIR", &config.durable_dir)
        .env("GARDEN_CELL_GRAPH_ID", &config.graph_id)
        .env("GARDEN_LOOPBACK_HOST", &config.loopback_host)
        .env("GARDEN_LOOPBACK_PORT", config.loopback_port.to_string())
        .stdout(stdout)
        .stderr(stderr);
    for (key, value) in config.extra_env {
        command.env(key, value);
    }
    let child = command.spawn().expect("spawn gardend");
    expected_manifest_pids()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(config.profile_dir.clone(), child.id());
    (
        ChildGuard {
            child,
            profile_dir: config.profile_dir,
        },
        config.log_dir,
    )
}
