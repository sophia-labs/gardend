use crate::{
    app_runtime::AppHandle,
    clock::timestamp,
    loopback_state::LoopbackManifest,
    process_utils::{place_child_in_new_process_group, terminate_child, DEFAULT_TERMINATE_GRACE},
    profile_paths::profile_dir,
    runtime_config::PROFILE_ID,
    storage::display_path,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, VecDeque},
    io::{BufRead, BufReader, Read},
    net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex},
    thread,
};
#[cfg(feature = "desktop")]
use tauri::Manager;
use uuid::Uuid;

pub(crate) const CHOREOGRAPH_SERVICE_ID: &str = "choreograph";
pub(crate) const KG_ULTRA_SERVICE_ID: &str = "kg-ultra";

const CHOREOGRAPH_DEFAULT_HEALTH_PATH: &str = "/health";
const CHOREOGRAPH_DEFAULT_PROXY_MOUNT: &str = "/services/choreograph";
const KG_ULTRA_DEFAULT_HEALTH_PATH: &str = "/health";
const KG_ULTRA_DEFAULT_PROXY_MOUNT: &str = "/services/kg-ultra";
const LOCAL_SERVICE_LOG_LIMIT: usize = 500;
const SUPPORTED_SERVICE_IDS: &[&str] = &[CHOREOGRAPH_SERVICE_ID, KG_ULTRA_SERVICE_ID];

#[derive(Default)]
pub(crate) struct LocalServiceHost {
    services: Mutex<BTreeMap<String, ServiceRuntime>>,
}

#[derive(Default)]
struct ServiceRuntime {
    child: Option<Child>,
    port: Option<u16>,
    internal_secret: Option<String>,
    started_at: Option<String>,
    last_error: Option<String>,
    manifest: Option<ResolvedServiceManifest>,
    logs: Arc<Mutex<VecDeque<LocalServiceLogLine>>>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LocalServiceStatus {
    pub(crate) service_id: String,
    pub(crate) state: LocalServiceState,
    pub(crate) pid: Option<u32>,
    pub(crate) started_at: Option<String>,
    pub(crate) health_path: String,
    pub(crate) proxy_mount: String,
    pub(crate) profile_id: &'static str,
    pub(crate) configured: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) health: Option<LocalServiceHealth>,
    pub(crate) last_error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum LocalServiceState {
    Unconfigured,
    Stopped,
    Running,
    Error,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LocalServiceHealth {
    pub(crate) ok: bool,
    pub(crate) status_code: Option<u16>,
    pub(crate) error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LocalServiceLogLine {
    pub(crate) ts: String,
    pub(crate) stream: &'static str,
    pub(crate) line: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LocalServiceLogs {
    pub(crate) service_id: String,
    pub(crate) lines: Vec<LocalServiceLogLine>,
}

#[derive(Debug, Clone)]
pub(crate) struct LocalServiceTarget {
    pub(crate) base_url: String,
    pub(crate) internal_secret: String,
}

#[derive(Debug, Clone)]
struct ResolvedServiceManifest {
    command: PathBuf,
    args: Vec<String>,
    working_dir: Option<PathBuf>,
    env: Vec<(String, String)>,
    health_path: String,
    proxy_mount: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ServiceManifestFile {
    service_id: Option<String>,
    command: String,
    #[serde(default)]
    args: Vec<String>,
    working_dir: Option<String>,
    #[serde(default)]
    env: Vec<ServiceManifestEnv>,
    health_path: Option<String>,
    proxy_mount: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ServiceManifestEnv {
    name: String,
    value: String,
}

impl LocalServiceHost {
    pub(crate) fn status(&self, service_id: &str, app: &AppHandle) -> LocalServiceStatus {
        if !is_supported_service_id(service_id) {
            return unsupported_status(service_id);
        }
        let mut services = self.services.lock().expect("local service registry lock");
        let runtime = services.entry(service_id.to_string()).or_default();
        runtime.reap_if_exited();
        runtime.status(service_id, app)
    }

    pub(crate) fn start(
        &self,
        service_id: &str,
        app: &AppHandle,
        loopback_manifest: &LoopbackManifest,
        loopback_token: &str,
    ) -> Result<LocalServiceStatus, String> {
        if !is_supported_service_id(service_id) {
            return Err(format!("unsupported local service: {service_id}"));
        }
        let manifest = resolve_service_manifest(app, service_id)?;
        let port = reserve_loopback_port()?;
        let data_dir = profile_dir(app)?.join("services").join(service_id);
        std::fs::create_dir_all(&data_dir)
            .map_err(|error| format!("create {service_id} service data dir: {error}"))?;

        let mut services = self.services.lock().expect("local service registry lock");
        let runtime = services.entry(service_id.to_string()).or_default();
        runtime.reap_if_exited();
        if runtime.is_running() {
            return Ok(runtime.status(service_id, app));
        }

        let internal_secret = Uuid::new_v4().simple().to_string();
        let mut command = Command::new(&manifest.command);
        command.args(&manifest.args);
        if let Some(working_dir) = &manifest.working_dir {
            command.current_dir(working_dir);
        }
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
        for (key, value) in &manifest.env {
            command.env(key, value);
        }
        if service_id == CHOREOGRAPH_SERVICE_ID {
            command.env("CHOREOGRAPH_SANDBOX_MODE", "local-native");
        }
        command
            .env("PORT", port.to_string())
            .env("DATA_DIR", &data_dir)
            .env("MNEMOSYNE_MCP_URL", &loopback_manifest.mcp_url)
            .env("MNEMOSYNE_MCP_ACCESS_TOKEN", loopback_token)
            .env("MNEMOSYNE_INTERNAL_SERVICE_SECRET", &internal_secret)
            .env("GARDEN_LOOPBACK_API_URL", &loopback_manifest.api_url)
            .env("GARDEN_SERVICE_ID", service_id)
            .env("GARDEN_PROFILE_ID", PROFILE_ID);
        place_child_in_new_process_group(&mut command);

        let mut child = command
            .spawn()
            .map_err(|error| format!("start {service_id} service: {error}"))?;
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        runtime.logs = Arc::new(Mutex::new(VecDeque::new()));
        if let Some(stdout) = stdout {
            spawn_log_reader(runtime.logs.clone(), "stdout", stdout);
        }
        if let Some(stderr) = stderr {
            spawn_log_reader(runtime.logs.clone(), "stderr", stderr);
        }

        runtime.port = Some(port);
        runtime.internal_secret = Some(internal_secret);
        runtime.started_at = Some(timestamp());
        runtime.last_error = None;
        runtime.manifest = Some(manifest);
        runtime.child = Some(child);
        Ok(runtime.status(service_id, app))
    }

    pub(crate) fn stop(
        &self,
        service_id: &str,
        app: &AppHandle,
    ) -> Result<LocalServiceStatus, String> {
        if !is_supported_service_id(service_id) {
            return Err(format!("unsupported local service: {service_id}"));
        }
        let mut services = self.services.lock().expect("local service registry lock");
        let runtime = services.entry(service_id.to_string()).or_default();
        if let Some(child) = runtime.child.as_mut() {
            terminate_child(child, DEFAULT_TERMINATE_GRACE);
        }
        runtime.child = None;
        runtime.port = None;
        runtime.internal_secret = None;
        runtime.started_at = None;
        Ok(runtime.status(service_id, app))
    }

    pub(crate) fn logs(
        &self,
        service_id: &str,
        tail: Option<usize>,
    ) -> Result<LocalServiceLogs, String> {
        if !is_supported_service_id(service_id) {
            return Err(format!("unsupported local service: {service_id}"));
        }
        let mut services = self.services.lock().expect("local service registry lock");
        let runtime = services.entry(service_id.to_string()).or_default();
        let logs = runtime.logs.lock().expect("local service log lock");
        let limit = tail.unwrap_or(200).min(LOCAL_SERVICE_LOG_LIMIT);
        let start = logs.len().saturating_sub(limit);
        Ok(LocalServiceLogs {
            service_id: service_id.to_string(),
            lines: logs.iter().skip(start).cloned().collect(),
        })
    }

    pub(crate) fn target(&self, service_id: &str) -> Result<LocalServiceTarget, String> {
        if !is_supported_service_id(service_id) {
            return Err(format!("unsupported local service: {service_id}"));
        }
        let mut services = self.services.lock().expect("local service registry lock");
        let runtime = services.entry(service_id.to_string()).or_default();
        runtime.reap_if_exited();
        if !runtime.is_running() {
            return Err("local service is not running".to_string());
        }
        let port = runtime
            .port
            .ok_or_else(|| "local service has no private port".to_string())?;
        let internal_secret = runtime
            .internal_secret
            .clone()
            .ok_or_else(|| "local service has no internal secret".to_string())?;
        Ok(LocalServiceTarget {
            base_url: format!("http://127.0.0.1:{port}"),
            internal_secret,
        })
    }
}

impl ServiceRuntime {
    fn is_running(&mut self) -> bool {
        self.reap_if_exited();
        self.child.is_some()
    }

    fn reap_if_exited(&mut self) {
        let Some(child) = self.child.as_mut() else {
            return;
        };
        match child.try_wait() {
            Ok(Some(status)) => {
                self.last_error = if status.success() {
                    None
                } else {
                    Some(format!("service exited with {status}"))
                };
                self.child = None;
                self.port = None;
                self.internal_secret = None;
                self.started_at = None;
            }
            Ok(None) => {}
            Err(error) => {
                self.last_error = Some(format!("poll service process: {error}"));
                self.child = None;
                self.port = None;
                self.internal_secret = None;
                self.started_at = None;
            }
        }
    }

    fn status(&self, service_id: &str, app: &AppHandle) -> LocalServiceStatus {
        let configured =
            resolve_service_manifest(app, service_id).is_ok() || self.manifest.is_some();
        let state = if self.child.is_some() {
            LocalServiceState::Running
        } else if !configured {
            LocalServiceState::Unconfigured
        } else if self.last_error.is_some() {
            LocalServiceState::Error
        } else {
            LocalServiceState::Stopped
        };
        let manifest = self.manifest.as_ref();
        LocalServiceStatus {
            service_id: service_id.to_string(),
            state,
            pid: self.child.as_ref().map(Child::id),
            started_at: self.started_at.clone(),
            health_path: manifest
                .map(|manifest| manifest.health_path.clone())
                .unwrap_or_else(|| default_health_path(service_id).to_string()),
            proxy_mount: manifest
                .map(|manifest| manifest.proxy_mount.clone())
                .unwrap_or_else(|| default_proxy_mount(service_id).to_string()),
            profile_id: PROFILE_ID,
            configured,
            health: None,
            last_error: self.last_error.clone(),
        }
    }
}

fn is_supported_service_id(service_id: &str) -> bool {
    SUPPORTED_SERVICE_IDS.contains(&service_id)
}

fn service_env_prefix(service_id: &str) -> String {
    service_id
        .chars()
        .map(|ch| match ch {
            'a'..='z' => ch.to_ascii_uppercase(),
            'A'..='Z' | '0'..='9' => ch,
            _ => '_',
        })
        .collect()
}

fn default_health_path(service_id: &str) -> &'static str {
    match service_id {
        CHOREOGRAPH_SERVICE_ID => CHOREOGRAPH_DEFAULT_HEALTH_PATH,
        KG_ULTRA_SERVICE_ID => KG_ULTRA_DEFAULT_HEALTH_PATH,
        _ => "/health",
    }
}

fn default_proxy_mount(service_id: &str) -> String {
    match service_id {
        CHOREOGRAPH_SERVICE_ID => CHOREOGRAPH_DEFAULT_PROXY_MOUNT.to_string(),
        KG_ULTRA_SERVICE_ID => KG_ULTRA_DEFAULT_PROXY_MOUNT.to_string(),
        _ => format!("/services/{service_id}"),
    }
}

fn unsupported_status(service_id: &str) -> LocalServiceStatus {
    LocalServiceStatus {
        service_id: service_id.to_string(),
        state: LocalServiceState::Unconfigured,
        pid: None,
        started_at: None,
        health_path: "/health".to_string(),
        proxy_mount: format!("/services/{service_id}"),
        profile_id: PROFILE_ID,
        configured: false,
        health: None,
        last_error: Some(format!("unsupported local service: {service_id}")),
    }
}

fn resolve_service_manifest(
    app: &AppHandle,
    service_id: &str,
) -> Result<ResolvedServiceManifest, String> {
    if !is_supported_service_id(service_id) {
        return Err(format!("unsupported local service: {service_id}"));
    }
    let prefix = service_env_prefix(service_id);
    let manifest_var = format!("GARDEN_{prefix}_MANIFEST");
    let command_var = format!("GARDEN_{prefix}_COMMAND");
    let args_var = format!("GARDEN_{prefix}_ARGS");
    let workdir_var = format!("GARDEN_{prefix}_WORKDIR");

    if let Ok(path) = std::env::var(&manifest_var) {
        let trimmed = path.trim();
        if !trimmed.is_empty() {
            return read_service_manifest(Path::new(trimmed), service_id);
        }
    }
    if let Ok(command) = std::env::var(&command_var) {
        let trimmed = command.trim();
        if !trimmed.is_empty() {
            let args = std::env::var(&args_var)
                .ok()
                .map(|args| split_env_args(&args))
                .unwrap_or_default();
            let working_dir = std::env::var(&workdir_var)
                .ok()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
                .map(PathBuf::from);
            return Ok(ResolvedServiceManifest {
                command: PathBuf::from(trimmed),
                args,
                working_dir,
                env: Vec::new(),
                health_path: default_health_path(service_id).to_string(),
                proxy_mount: default_proxy_mount(service_id),
            });
        }
    }
    let resource_manifest = app
        .path()
        .resource_dir()
        .ok()
        .map(|dir| dir.join("services").join(service_id).join("service.json"));
    if let Some(path) = resource_manifest.filter(|path| path.is_file()) {
        return read_service_manifest(&path, service_id);
    }
    Err(format!(
        "{service_id} service is not configured; set {manifest_var} or {command_var}, or bundle resources/services/{service_id}/service.json"
    ))
}

fn read_service_manifest(
    path: &Path,
    expected_service_id: &str,
) -> Result<ResolvedServiceManifest, String> {
    let raw = std::fs::read_to_string(path).map_err(|error| {
        format!(
            "read local service manifest {}: {error}",
            display_path(path)
        )
    })?;
    let parsed: ServiceManifestFile = serde_json::from_str(&raw).map_err(|error| {
        format!(
            "parse local service manifest {}: {error}",
            display_path(path)
        )
    })?;
    let base_dir = path.parent().ok_or_else(|| {
        format!(
            "local service manifest has no parent: {}",
            display_path(path)
        )
    })?;
    let service_id = parsed
        .service_id
        .unwrap_or_else(|| expected_service_id.to_string());
    if service_id != expected_service_id {
        return Err(format!(
            "unsupported local service manifest serviceId {service_id}; expected {expected_service_id}"
        ));
    }
    Ok(ResolvedServiceManifest {
        command: resolve_manifest_command_path(base_dir, &parsed.command),
        args: parsed.args,
        working_dir: parsed
            .working_dir
            .map(|working_dir| resolve_manifest_path(base_dir, &working_dir)),
        env: parsed
            .env
            .into_iter()
            .filter(|entry| !entry.name.trim().is_empty())
            .map(|entry| (entry.name, entry.value))
            .collect(),
        health_path: parsed
            .health_path
            .unwrap_or_else(|| default_health_path(expected_service_id).to_string()),
        proxy_mount: parsed
            .proxy_mount
            .unwrap_or_else(|| default_proxy_mount(expected_service_id)),
    })
}

fn resolve_manifest_command_path(base_dir: &Path, raw: &str) -> PathBuf {
    let path = Path::new(raw);
    if path.is_absolute() || path.components().count() == 1 {
        path.to_path_buf()
    } else {
        base_dir.join(path)
    }
}

fn resolve_manifest_path(base_dir: &Path, raw: &str) -> PathBuf {
    let path = Path::new(raw);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        base_dir.join(path)
    }
}

fn split_env_args(raw: &str) -> Vec<String> {
    raw.split_whitespace().map(str::to_string).collect()
}

fn reserve_loopback_port() -> Result<u16, String> {
    let listener = TcpListener::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
        .map_err(|error| format!("reserve local service port: {error}"))?;
    let port = listener
        .local_addr()
        .map_err(|error| format!("read local service port: {error}"))?
        .port();
    drop(listener);
    Ok(port)
}

fn spawn_log_reader<R>(
    logs: Arc<Mutex<VecDeque<LocalServiceLogLine>>>,
    stream: &'static str,
    reader: R,
) where
    R: Read + Send + 'static,
{
    thread::spawn(move || {
        let mut reader = BufReader::new(reader);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) => break,
                Ok(_) => push_log_line(&logs, stream, line.trim_end().to_string()),
                Err(error) => {
                    push_log_line(&logs, stream, format!("log read failed: {error}"));
                    break;
                }
            }
        }
    });
}

fn push_log_line(
    logs: &Arc<Mutex<VecDeque<LocalServiceLogLine>>>,
    stream: &'static str,
    line: String,
) {
    let mut logs = logs.lock().expect("local service log lock");
    while logs.len() >= LOCAL_SERVICE_LOG_LIMIT {
        logs.pop_front();
    }
    logs.push_back(LocalServiceLogLine {
        ts: timestamp(),
        stream,
        line,
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_paths_keep_pathless_commands_on_path() {
        let base = Path::new("/tmp/service");
        assert_eq!(
            resolve_manifest_command_path(base, "node"),
            PathBuf::from("node")
        );
        assert_eq!(
            resolve_manifest_command_path(base, "bin/choreograph"),
            PathBuf::from("/tmp/service/bin/choreograph")
        );
        assert_eq!(
            resolve_manifest_path(base, "choreograph"),
            PathBuf::from("/tmp/service/choreograph")
        );
    }

    #[test]
    fn service_env_prefix_normalizes_service_ids() {
        assert_eq!(service_env_prefix(CHOREOGRAPH_SERVICE_ID), "CHOREOGRAPH");
        assert_eq!(service_env_prefix(KG_ULTRA_SERVICE_ID), "KG_ULTRA");
    }

    #[test]
    fn service_manifest_resolves_paths_relative_to_manifest_dir() {
        let dir = std::env::temp_dir().join(format!(
            "garden-service-manifest-{}",
            Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&dir).expect("create temp service dir");
        let manifest_path = dir.join("service.json");
        std::fs::write(
            &manifest_path,
            r#"{
              "serviceId": "choreograph",
              "command": "choreograph/node_modules/.bin/tsx",
              "args": ["src/orchestrator.ts"],
              "workingDir": "choreograph",
              "healthPath": "/healthz",
              "proxyMount": "/services/choreograph",
              "env": [{"name": "NODE_ENV", "value": "production"}]
            }"#,
        )
        .expect("write service manifest");

        let manifest = read_service_manifest(&manifest_path, CHOREOGRAPH_SERVICE_ID)
            .expect("read service manifest");
        assert_eq!(
            manifest.command,
            dir.join("choreograph").join("node_modules/.bin/tsx")
        );
        assert_eq!(manifest.args, vec!["src/orchestrator.ts"]);
        assert_eq!(manifest.working_dir, Some(dir.join("choreograph")));
        assert_eq!(manifest.health_path, "/healthz");
        assert_eq!(
            manifest.env,
            vec![("NODE_ENV".to_string(), "production".to_string())]
        );

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn service_manifest_rejects_the_wrong_service_id() {
        let dir = std::env::temp_dir().join(format!(
            "garden-service-manifest-{}",
            Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&dir).expect("create temp service dir");
        let manifest_path = dir.join("service.json");
        std::fs::write(
            &manifest_path,
            r#"{
              "serviceId": "kg-ultra",
              "command": "python",
              "args": ["-m", "kg_ultra_service.server"]
            }"#,
        )
        .expect("write service manifest");

        let error = read_service_manifest(&manifest_path, CHOREOGRAPH_SERVICE_ID)
            .expect_err("wrong service id must fail");
        assert!(error.contains("expected choreograph"));

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn kg_ultra_manifest_uses_kg_ultra_defaults() {
        let dir = std::env::temp_dir().join(format!(
            "garden-service-manifest-{}",
            Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&dir).expect("create temp service dir");
        let manifest_path = dir.join("service.json");
        std::fs::write(
            &manifest_path,
            r#"{
              "serviceId": "kg-ultra",
              "command": "kg-ultra/.venv/bin/python",
              "args": ["-m", "kg_ultra_service.server"],
              "workingDir": "kg-ultra"
            }"#,
        )
        .expect("write service manifest");

        let manifest = read_service_manifest(&manifest_path, KG_ULTRA_SERVICE_ID)
            .expect("read kg-ultra service manifest");
        assert_eq!(
            manifest.command,
            dir.join("kg-ultra").join(".venv/bin/python")
        );
        assert_eq!(manifest.health_path, KG_ULTRA_DEFAULT_HEALTH_PATH);
        assert_eq!(manifest.proxy_mount, KG_ULTRA_DEFAULT_PROXY_MOUNT);

        let _ = std::fs::remove_dir_all(dir);
    }
}
