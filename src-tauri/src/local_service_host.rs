//! Local services gardend distributes and supervises (Choreograph, KG-ULTRA),
//! see `docs/local-service-hosting.md`.
//!
//! gardend owns the process: it starts the service with a private loopback
//! port, a per-process internal secret and a member-scope MCP token of its
//! own (never the master key), restarts it after a crash with backoff, and
//! ends its whole process group when it stops or gardend exits. A service a
//! previous gardend left running (killed before it could stop it) is ended
//! before the next one starts. The service owns its API and its `DATA_DIR`.
//!
//! A service whose manifest says `"delegateCaller": true` also gets, on every
//! request gardend proxies to it on a graph-scoped path, a delegation token:
//! short-lived, in memory, bound to the caller, that graph and the caller's
//! role on it ([`Delegation`]). gardend's `/mcp` accepts it, bound to that
//! graph (`loopback_mcp_routes`); nothing else does.

use crate::{
    app_runtime::AppHandle, clock::timestamp, loopback_state::LoopbackManifest,
    process_utils::place_child_in_new_process_group, profile_paths::profile_dir,
    runtime_config::PROFILE_ID, storage::display_path,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, VecDeque},
    io::{BufRead, BufReader, Read},
    net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, OnceLock, RwLock, Weak,
    },
    thread,
    time::{Duration, Instant},
};
#[cfg(feature = "desktop")]
use tauri::Manager;
use uuid::Uuid;

pub(crate) const CHOREOGRAPH_SERVICE_ID: &str = "choreograph";
pub(crate) const KG_ULTRA_SERVICE_ID: &str = "kg-ultra";

/// A folder of services, one `<id>/service.json` each, beside the
/// `services/` folder next to the binary.
pub(crate) const SERVICES_DIR_ENV: &str = "GARDEN_SERVICES_DIR";
/// The address every supervised service is told to listen on.
pub(crate) const SERVICE_HOST_ENV: &str = "GARDEN_SERVICE_HOST";

/// The address gardend tells Choreograph to listen on. Choreograph used to
/// bind 0.0.0.0; gardend reaches it over loopback only, so nothing on the
/// LAN (or another WSL2 distribution's network) can reach the child.
pub(crate) const CHOREOGRAPH_BIND_HOST_ENV: &str = "CHOREOGRAPH_BIND_HOST";
const SERVICE_BIND_HOST: &str = "127.0.0.1";
/// The model key chat needs (OpenRouter), kept in the profile's
/// `services.env` by `gardend chat key set`.
pub(crate) const OPENROUTER_API_KEY_ENV: &str = "OPENROUTER_API_KEY";

const CHOREOGRAPH_DEFAULT_HEALTH_PATH: &str = "/health";
const CHOREOGRAPH_DEFAULT_PROXY_MOUNT: &str = "/services/choreograph";
const KG_ULTRA_DEFAULT_HEALTH_PATH: &str = "/health";
const KG_ULTRA_DEFAULT_PROXY_MOUNT: &str = "/services/kg-ultra";
const LOCAL_SERVICE_LOG_LIMIT: usize = 500;
/// The file next to a service's data dir that names the process gardend
/// started, so the next gardend can end it if this one died without doing so.
const SUPERVISOR_RECORD_SUFFIX: &str = "supervisor.json";
const SUPERVISOR_TICK: Duration = Duration::from_millis(250);
/// The header that carries a delegation token to a service that asked for
/// one. A caller's own copy never passes the proxy (it drops `x-sophia-*`).
pub(crate) const DELEGATED_TOKEN_HEADER: &str = "x-sophia-delegated-token";
/// How long a delegation token works. A request past half of it gets a fresh
/// one, so the newest token a service holds always has half an hour left.
pub(crate) const DELEGATION_TTL: Duration = Duration::from_secs(60 * 60);
const DELEGATED_TOKEN_PREFIX: &str = "sophia-dlg-";

#[derive(Debug, Clone, Copy)]
pub(crate) struct SupervisionTiming {
    /// The first restart waits this long; each further one doubles it.
    pub(crate) restart_backoff: Duration,
    pub(crate) restart_backoff_cap: Duration,
    /// A child that ran this long before it crashed starts its backoff over.
    pub(crate) stable_after: Duration,
    /// Consecutive crashes after which gardend stops restarting.
    pub(crate) max_restarts: u32,
    /// How long a stopping process group gets after SIGTERM before SIGKILL.
    pub(crate) stop_grace: Duration,
}

impl Default for SupervisionTiming {
    fn default() -> Self {
        Self {
            restart_backoff: Duration::from_secs(1),
            restart_backoff_cap: Duration::from_secs(60),
            stable_after: Duration::from_secs(60),
            max_restarts: 10,
            stop_grace: Duration::from_secs(8),
        }
    }
}

impl SupervisionTiming {
    fn backoff(&self, restarts: u32) -> Duration {
        let doublings = restarts.saturating_sub(1).min(16);
        self.restart_backoff
            .saturating_mul(1_u32 << doublings)
            .min(self.restart_backoff_cap)
    }
}

#[derive(Default)]
pub(crate) struct LocalServiceHost {
    services: Mutex<BTreeMap<String, ServiceRuntime>>,
    /// The MCP token of each running child → its service id. Kept apart from
    /// `services` so authenticating a request never waits on a spawn.
    service_tokens: RwLock<BTreeMap<String, String>>,
    /// The delegation tokens handed to services that asked for them.
    delegations: RwLock<DelegationRegistry>,
    /// gardend's own API as a child reaches it (`http://127.0.0.1:<port>`),
    /// never the public URL a Cloudflare Tunnel puts in front of it.
    local_api_url: OnceLock<String>,
    /// Manifests named in code rather than by environment or next to the
    /// binary (tests; an embedding host).
    pinned_manifests: Mutex<BTreeMap<String, PathBuf>>,
    /// Service folders named in code, ahead of `GARDEN_SERVICES_DIR` (tests).
    pinned_services_dirs: Mutex<Vec<PathBuf>>,
    timing: Mutex<Option<SupervisionTiming>>,
    supervisor_started: AtomicBool,
    shutting_down: AtomicBool,
}

/// What a delegation token lets the service that holds it do: act for one
/// caller (the `X-User-ID` gardend minted for the request it came with), on
/// one graph, with that caller's role and never more than the caller's
/// scopes. It works at gardend's `/mcp` only, until it expires or the service
/// process it was handed to ends, whichever comes first.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Delegation {
    pub(crate) service_id: String,
    pub(crate) user_id: String,
    pub(crate) graph_id: String,
    /// `owner`, `editor` or `viewer`, as the proxy minted `x-sophia-graph-role`.
    pub(crate) role: String,
    pub(crate) scopes: Vec<String>,
    /// Made for a public-mode visitor or account (`public_mode`): at `/mcp`
    /// it reads every public graph (each call names one; `graph_id`, the
    /// session's own, when it names none), never more.
    pub(crate) public: bool,
    /// W6: the Sophia account it acts for, whose fence may admit a write in
    /// the visitors' folder (`visitor_fence`); `None` for everyone else.
    pub(crate) visitor: Option<crate::visitor_fence::VisitorAccount>,
}

#[derive(Default)]
struct DelegationRegistry {
    /// token → what it delegates, and when it stops working.
    by_token: BTreeMap<String, (Delegation, Instant)>,
    /// One delegation → its newest token (reused until half its life is gone).
    newest: BTreeMap<Delegation, String>,
}

#[derive(Default)]
struct ServiceRuntime {
    child: Option<Child>,
    port: Option<u16>,
    internal_secret: Option<String>,
    mcp_token: Option<String>,
    started_at: Option<String>,
    launched: Option<Instant>,
    last_error: Option<String>,
    manifest: Option<ResolvedServiceManifest>,
    logs: Arc<Mutex<VecDeque<LocalServiceLogLine>>>,
    /// The supervisor keeps the service running while this is set: `start`
    /// sets it, `stop`, a clean exit and a crash loop clear it.
    wanted: bool,
    /// Crashes in a row (reset by `start` and by a stable run).
    restarts: u32,
    restart_at: Option<Instant>,
    /// What a restart needs: the app (manifest and profile resolution).
    app: Option<AppHandle>,
    record_path: Option<PathBuf>,
    /// Why the service cannot start on this computer (a requirement in its
    /// `service.json` is not met), in words for the person: set instead of
    /// starting it, cleared by the next start that gets through.
    unavailable: Option<String>,
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
    /// Crash restarts in a row; 0 while the service runs steadily.
    pub(crate) restarts: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) health: Option<LocalServiceHealth>,
    pub(crate) last_error: Option<String>,
    /// For `unavailable`: what the computer lacks, for the person (no paths).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) reason: Option<String>,
    /// Whether gardend starts it when it boots: the profile's choice
    /// (`gardend service enable|disable <id>`), else the manifest's
    /// `autostart`; `GARDEN_<ID>_AUTOSTART=0` turns it off. Absent for a
    /// service that is not configured.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) enabled: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum LocalServiceState {
    Unconfigured,
    Stopped,
    Running,
    /// Crashed; the supervisor starts it again after its backoff.
    Restarting,
    Error,
    /// Not started: the computer lacks what its `service.json` requires
    /// (`requires.node`). Starting it again checks again.
    Unavailable,
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
    autostart: bool,
    member_access: MemberAccess,
    /// `requires.node`: the oldest Node.js the service runs on.
    requires_node: Option<NodeVersion>,
    /// `displayName`: what the person calls the service ("Chat"), for messages.
    display_name: Option<String>,
    delegate_caller: bool,
    /// `acceptsModelKey`: the caller's own `X-Model-Key` is forwarded to it.
    accepts_model_key: bool,
    /// `mcp`: the MCP endpoint whose tools gardend lists and forwards on its
    /// own `/mcp` (`service_mcp`).
    mcp: Option<ServiceMcp>,
}

/// What a service lends gardend's own `/mcp` (`"mcp"` in its `service.json`):
/// the tools its MCP endpoint lists under `toolPrefix`, about the graphs named
/// in `graphs`. gardend lists them beside its own tools and forwards their
/// calls; a service without `mcp` adds nothing. See `service_mcp`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ServiceMcp {
    /// The service's MCP path (JSON-RPC over POST), `/mcp` by default.
    pub(crate) path: String,
    /// The graphs the tools are about; a call names one of them.
    pub(crate) graphs: Vec<String>,
    /// Every lent tool's name starts with it (`cow_`), so the service's
    /// tools never shadow gardend's own.
    pub(crate) tool_prefix: String,
    /// How long gardend waits for one call before it answers with a timeout.
    pub(crate) timeout: Duration,
}

/// The longest a manifest may ask gardend to wait for one lent tool call.
pub(crate) const SERVICE_MCP_MAX_TIMEOUT_MS: u64 = 120_000;
const SERVICE_MCP_DEFAULT_TIMEOUT_MS: u64 = 30_000;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ServiceMcpFile {
    path: Option<String>,
    #[serde(default)]
    graphs: Vec<String>,
    #[serde(default)]
    tool_prefix: String,
    timeout_ms: Option<u64>,
}

impl ServiceMcpFile {
    fn resolve(self, manifest: &Path) -> Result<ServiceMcp, String> {
        let at = || format!("local service manifest {}: mcp", display_path(manifest));
        let path = self.path.unwrap_or_else(|| "/mcp".to_string());
        if !path.starts_with('/') || path.split('/').any(|segment| segment == "..") {
            return Err(format!(
                "{}.path {path:?} must be an absolute path such as \"/mcp\"",
                at()
            ));
        }
        if self.graphs.is_empty() {
            return Err(format!("{}.graphs must name at least one graph", at()));
        }
        for graph in &self.graphs {
            crate::ids::validate_local_id(graph, "mcp.graphs")
                .map_err(|error| format!("{}: {error}", at()))?;
        }
        let prefix = &self.tool_prefix;
        let prefix_ok = (2..=32).contains(&prefix.len())
            && prefix.ends_with('_')
            && prefix.starts_with(|ch: char| ch.is_ascii_lowercase())
            && prefix
                .chars()
                .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_');
        if !prefix_ok {
            return Err(format!(
                "{}.toolPrefix {prefix:?} must be lowercase letters, digits and underscores, ending in \"_\" (\"cow_\")",
                at()
            ));
        }
        let timeout_ms = self.timeout_ms.unwrap_or(SERVICE_MCP_DEFAULT_TIMEOUT_MS);
        if timeout_ms == 0 || timeout_ms > SERVICE_MCP_MAX_TIMEOUT_MS {
            return Err(format!(
                "{}.timeoutMs {timeout_ms} must be between 1 and {SERVICE_MCP_MAX_TIMEOUT_MS}",
                at()
            ));
        }
        Ok(ServiceMcp {
            path,
            graphs: self.graphs,
            tool_prefix: self.tool_prefix,
            timeout: Duration::from_millis(timeout_ms),
        })
    }
}

/// A Node.js version, as `node --version` prints it (`v22.19.0`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct NodeVersion(pub(crate) u64, pub(crate) u64, pub(crate) u64);

impl NodeVersion {
    /// `22.19`, `22.19.0`, `v22.19.0` or `>=22.19` (a floor is all a manifest says).
    pub(crate) fn parse(raw: &str) -> Option<Self> {
        let text = raw
            .trim()
            .trim_start_matches(">=")
            .trim()
            .trim_start_matches('v');
        let mut parts = text.split('.');
        let major = parts.next()?.trim().parse().ok()?;
        let minor = parts
            .next()
            .map_or(Some(0), |part| part.trim().parse().ok())?;
        let patch = parts.next().map_or(Some(0), |part| {
            part.trim().split(['-', '+']).next()?.parse().ok()
        })?;
        if parts.next().is_some() {
            return None;
        }
        Some(Self(major, minor, patch))
    }
}

impl std::fmt::Display for NodeVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self(major, minor, 0) => write!(f, "{major}.{minor}"),
            Self(major, minor, patch) => write!(f, "{major}.{minor}.{patch}"),
        }
    }
}

/// What a member (a caller without `services.manage`) may reach of a service
/// through `/services/<id>` (`memberAccess` in its `service.json`). Choreograph
/// ignores it: members reach its workspace chat and nothing else.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum MemberAccess {
    /// Admins only (the default).
    #[default]
    None,
    /// Every path; the service decides by the minted caller role.
    All,
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
    /// Start with gardend (default true).
    autostart: Option<bool>,
    #[serde(default)]
    member_access: MemberAccess,
    #[serde(default)]
    requires: ServiceRequirementsFile,
    display_name: Option<String>,
    /// Hand the service a delegation token on every graph-scoped request
    /// (default false).
    #[serde(default)]
    delegate_caller: bool,
    /// Forward a caller's own model key (`X-Model-Key`) to the service
    /// (default false: the header is dropped).
    #[serde(default)]
    accepts_model_key: bool,
    /// Lend the service's MCP tools to gardend's `/mcp` (`ServiceMcp`).
    #[serde(default)]
    mcp: Option<ServiceMcpFile>,
}

/// `requires` in a `service.json`: what must be on the computer before
/// gardend starts the service. gardend checks it at every start and marks
/// the service `unavailable`, with the reason, instead of starting it.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ServiceRequirementsFile {
    /// The oldest Node.js the service runs on (`"22.19"`), found as the
    /// manifest's command when that is `node`, else as `node` on the PATH the
    /// service gets.
    node: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ServiceManifestEnv {
    name: String,
    value: String,
}

/// What `<profile>/services/<id>.supervisor.json` says about the process a
/// gardend started.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SupervisorRecord {
    service_id: String,
    /// The child's pid, which is also its process group id.
    pid: u32,
    /// The child's start time as the OS reports it, so a recycled pid is
    /// never mistaken for the child.
    #[serde(default)]
    start: Option<String>,
    gardend_pid: u32,
    started_at: String,
}

impl LocalServiceHost {
    /// gardend's own API as a child should reach it. Set once, when the
    /// loopback server has bound its port.
    pub(crate) fn set_local_api_url(&self, url: String) {
        let _ = self.local_api_url.set(url);
    }

    /// Use this manifest for `service_id`, ahead of the environment and the
    /// `services/` folder next to the binary.
    #[allow(dead_code)]
    pub(crate) fn pin_manifest(&self, service_id: &str, path: PathBuf) {
        self.pinned_manifests
            .lock()
            .expect("pinned manifest lock")
            .insert(service_id.to_string(), path);
    }

    /// Look for services in `dir` too (`<dir>/<id>/service.json`), ahead of
    /// `GARDEN_SERVICES_DIR`.
    #[allow(dead_code)]
    pub(crate) fn pin_services_dir(&self, dir: PathBuf) {
        self.pinned_services_dirs
            .lock()
            .expect("pinned services dir lock")
            .push(dir);
    }

    fn services_dirs(&self, app: &AppHandle) -> Vec<PathBuf> {
        let mut dirs = self
            .pinned_services_dirs
            .lock()
            .expect("pinned services dir lock")
            .clone();
        dirs.extend(services_dirs_from(
            std::env::var(SERVICES_DIR_ENV).ok().as_deref(),
            app.path().resource_dir().ok().as_deref(),
        ));
        dirs
    }

    /// Every service gardend could run now: the ones pinned, any
    /// `GARDEN_<ID>_MANIFEST` or `GARDEN_<ID>_COMMAND`, and every
    /// `<dir>/<id>/service.json` under `GARDEN_SERVICES_DIR` and the
    /// `services/` folder beside the binary.
    pub(crate) fn service_ids(&self, app: &AppHandle) -> std::collections::BTreeSet<String> {
        let mut ids: std::collections::BTreeSet<String> = self
            .pinned_manifests
            .lock()
            .expect("pinned manifest lock")
            .keys()
            .cloned()
            .collect();
        ids.extend(discover_service_ids(
            std::env::vars(),
            &self.services_dirs(app),
        ));
        ids
    }

    /// What a member may reach of `service_id` (see [`MemberAccess`]).
    pub(crate) fn member_access(&self, service_id: &str, app: &AppHandle) -> MemberAccess {
        let running = self
            .services
            .lock()
            .expect("local service registry lock")
            .get(service_id)
            .and_then(|runtime| {
                runtime
                    .manifest
                    .as_ref()
                    .map(|manifest| manifest.member_access)
            });
        running.unwrap_or_else(|| {
            self.resolve_manifest(app, service_id)
                .map(|manifest| manifest.member_access)
                .unwrap_or_default()
        })
    }

    /// Whether `service_id`'s manifest takes a caller's own model key
    /// (`"acceptsModelKey": true`).
    pub(crate) fn accepts_model_key(&self, service_id: &str, app: &AppHandle) -> bool {
        let running = self
            .services
            .lock()
            .expect("local service registry lock")
            .get(service_id)
            .and_then(|runtime| {
                runtime
                    .manifest
                    .as_ref()
                    .map(|manifest| manifest.accepts_model_key)
            });
        running.unwrap_or_else(|| {
            self.resolve_manifest(app, service_id)
                .map(|manifest| manifest.accepts_model_key)
                .unwrap_or(false)
        })
    }

    /// Whether `service_id`'s manifest asks for delegation tokens
    /// (`"delegateCaller": true`).
    pub(crate) fn delegates_caller(&self, service_id: &str, app: &AppHandle) -> bool {
        let running = self
            .services
            .lock()
            .expect("local service registry lock")
            .get(service_id)
            .and_then(|runtime| {
                runtime
                    .manifest
                    .as_ref()
                    .map(|manifest| manifest.delegate_caller)
            });
        running.unwrap_or_else(|| {
            self.resolve_manifest(app, service_id)
                .map(|manifest| manifest.delegate_caller)
                .unwrap_or(false)
        })
    }

    /// Every configured service that lends its MCP tools to gardend's `/mcp`
    /// (`"mcp"` in its `service.json`), with what it declares. The manifest
    /// of a running service wins over the one on disk.
    pub(crate) fn mcp_declarations(&self, app: &AppHandle) -> Vec<(String, ServiceMcp)> {
        self.service_ids(app)
            .into_iter()
            .filter_map(|service_id| {
                let running = self
                    .services
                    .lock()
                    .expect("local service registry lock")
                    .get(&service_id)
                    .and_then(|runtime| {
                        runtime
                            .manifest
                            .as_ref()
                            .map(|manifest| manifest.mcp.clone())
                    });
                let mcp = match running {
                    Some(mcp) => mcp,
                    None => self.resolve_manifest(app, &service_id).ok()?.mcp,
                };
                mcp.map(|mcp| (service_id, mcp))
            })
            .collect()
    }

    /// The running process of `service_id` (pid and start time), which a
    /// cache of what it said may be keyed by: a restart is another process.
    pub(crate) fn process_identity(&self, service_id: &str) -> Option<(u32, String)> {
        let services = self.services.lock().expect("local service registry lock");
        let runtime = services.get(service_id)?;
        let child = runtime.child.as_ref()?;
        Some((child.id(), runtime.started_at.clone().unwrap_or_default()))
    }

    /// A delegation token for `delegation`: the newest one while more than
    /// half of its life is left, else a new one. Expired tokens are dropped.
    /// `None` while the service has no running process to hand it to.
    pub(crate) fn delegate(&self, delegation: Delegation) -> Option<String> {
        self.delegate_at(delegation, Instant::now())
    }

    fn delegate_at(&self, delegation: Delegation, now: Instant) -> Option<String> {
        let running = self
            .service_tokens
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .any(|service_id| *service_id == delegation.service_id);
        if !running {
            return None;
        }
        let mut registry = self
            .delegations
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        registry.by_token.retain(|_, (_, expires)| *expires > now);
        let DelegationRegistry { by_token, newest } = &mut *registry;
        newest.retain(|_, token| by_token.contains_key(token));
        if let Some(token) = newest.get(&delegation) {
            if let Some((_, expires)) = by_token.get(token) {
                if expires.saturating_duration_since(now) > DELEGATION_TTL / 2 {
                    return Some(token.clone());
                }
            }
        }
        let token = format!(
            "{DELEGATED_TOKEN_PREFIX}{}{}",
            Uuid::new_v4().simple(),
            Uuid::new_v4().simple()
        );
        by_token.insert(token.clone(), (delegation.clone(), now + DELEGATION_TTL));
        newest.insert(delegation, token.clone());
        Some(token)
    }

    /// What a delegation token delegates, while it works.
    pub(crate) fn delegation(&self, token: &str) -> Option<Delegation> {
        self.delegation_at(token, Instant::now())
    }

    fn delegation_at(&self, token: &str, now: Instant) -> Option<Delegation> {
        if !token.starts_with(DELEGATED_TOKEN_PREFIX) {
            return None;
        }
        self.delegations
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .by_token
            .get(token)
            .filter(|(_, expires)| *expires > now)
            .map(|(delegation, _)| delegation.clone())
    }

    /// End every delegation token handed to `service_id`: its process is gone.
    fn forget_delegations(&self, service_id: &str) {
        let mut registry = self
            .delegations
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        registry
            .by_token
            .retain(|_, (delegation, _)| delegation.service_id != service_id);
        registry
            .newest
            .retain(|delegation, _| delegation.service_id != service_id);
    }

    #[allow(dead_code)]
    pub(crate) fn set_timing(&self, timing: SupervisionTiming) {
        *self.timing.lock().expect("supervision timing lock") = Some(timing);
    }

    fn timing(&self) -> SupervisionTiming {
        let configured = *self.timing.lock().expect("supervision timing lock");
        configured.unwrap_or_default()
    }

    /// The service a supervised child's own MCP token belongs to.
    pub(crate) fn service_token_owner(&self, token: &str) -> Option<String> {
        if token.is_empty() {
            return None;
        }
        self.service_tokens
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(token)
            .cloned()
    }

    fn set_service_token(&self, service_id: &str, previous: Option<&str>, next: Option<&str>) {
        let mut tokens = self
            .service_tokens
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(previous) = previous {
            tokens.remove(previous);
        }
        if let Some(next) = next {
            tokens.insert(next.to_string(), service_id.to_string());
        }
    }

    pub(crate) fn configured(&self, service_id: &str, app: &AppHandle) -> bool {
        self.resolve_manifest(app, service_id).is_ok()
    }

    fn resolve_manifest(
        &self,
        app: &AppHandle,
        service_id: &str,
    ) -> Result<ResolvedServiceManifest, String> {
        let pinned = self
            .pinned_manifests
            .lock()
            .expect("pinned manifest lock")
            .get(service_id)
            .cloned();
        match pinned {
            Some(path) => read_service_manifest(&path, service_id),
            None => resolve_service_manifest(service_id, &self.services_dirs(app)),
        }
    }

    pub(crate) fn status(&self, service_id: &str, app: &AppHandle) -> LocalServiceStatus {
        if !is_supported_service_id(service_id) {
            return unsupported_status(service_id);
        }
        let manifest = self.resolve_manifest(app, service_id).ok();
        let enabled = manifest.as_ref().map(|manifest| {
            service_autostarts(
                service_id,
                manifest.autostart,
                profile_dir(app).ok().as_deref(),
            )
        });
        let configured = manifest.is_some();
        let timing = self.timing();
        let mut services = self.services.lock().expect("local service registry lock");
        let runtime = services.entry(service_id.to_string()).or_default();
        self.reap(service_id, runtime, &timing);
        let mut status = runtime.status(service_id, configured || runtime.manifest.is_some());
        status.enabled = enabled;
        status
    }

    /// Start `service_id` (no-op when it runs) and keep it running: a crash
    /// is restarted after a backoff until `stop`. A process a previous gardend
    /// left behind is ended first.
    pub(crate) fn start(
        self: &Arc<Self>,
        service_id: &str,
        app: &AppHandle,
        loopback_manifest: &LoopbackManifest,
    ) -> Result<LocalServiceStatus, String> {
        if !is_supported_service_id(service_id) {
            return Err(format!("unsupported local service: {service_id}"));
        }
        if self.shutting_down.load(Ordering::SeqCst) {
            return Err("gardend is shutting down".to_string());
        }
        if self.local_api_url.get().is_none() {
            self.set_local_api_url(loopback_manifest.api_url.clone());
        }
        let timing = self.timing();
        {
            let mut services = self.services.lock().expect("local service registry lock");
            let runtime = services.entry(service_id.to_string()).or_default();
            self.reap(service_id, runtime, &timing);
            if runtime.child.is_some() {
                runtime.wanted = true;
                return Ok(runtime.status(service_id, true));
            }
        }
        // Outside the registry lock: ending an orphan can take the grace.
        let profile = profile_dir(app)?;
        if let Some(ended) = end_orphan(&supervisor_record_path(&profile, service_id), &timing) {
            log::warn!("local service {service_id}: {ended}");
        }
        let mut services = self.services.lock().expect("local service registry lock");
        let runtime = services.entry(service_id.to_string()).or_default();
        if runtime.child.is_none() {
            runtime.app = Some(app.clone());
            runtime.wanted = true;
            runtime.restarts = 0;
            runtime.restart_at = None;
            if let Err(error) = self.launch(service_id, runtime) {
                runtime.wanted = false;
                runtime.last_error = Some(error.clone());
                return Err(error);
            }
        }
        runtime.wanted = true;
        let status = runtime.status(service_id, true);
        drop(services);
        self.ensure_supervisor();
        Ok(status)
    }

    /// Spawn the child for `runtime` (the registry lock is held).
    fn launch(&self, service_id: &str, runtime: &mut ServiceRuntime) -> Result<(), String> {
        let app = runtime
            .app
            .clone()
            .ok_or_else(|| format!("{service_id}: no app to start it with"))?;
        let manifest = self.resolve_manifest(&app, service_id)?;
        let profile = profile_dir(&app)?;
        let data_dir = profile.join("services").join(service_id);
        std::fs::create_dir_all(&data_dir)
            .map_err(|error| format!("create {service_id} service data dir: {error}"))?;
        let mut secrets = read_service_env_file(&shared_service_env_file(&profile))?;
        secrets.extend(read_service_env_file(&service_env_file(
            &profile, service_id,
        ))?);
        if let Some(required) = manifest.requires_node {
            let path_var = child_path_var(&manifest.env, &secrets);
            let label = manifest.display_name.as_deref().unwrap_or(service_id);
            if let Err((reason, detail)) =
                check_node_requirement(required, &manifest.command, path_var.as_deref(), label)
            {
                runtime.unavailable = Some(reason);
                runtime.manifest = Some(manifest);
                return Err(detail);
            }
        }
        runtime.unavailable = None;
        let api_url =
            self.local_api_url.get().cloned().ok_or_else(|| {
                format!("{service_id}: gardend's local API address is not known yet")
            })?;
        let port = reserve_loopback_port()?;
        let internal_secret = Uuid::new_v4().simple().to_string();
        let mcp_token = format!(
            "sophia-svc-{}{}",
            Uuid::new_v4().simple(),
            Uuid::new_v4().simple()
        );

        let mut command = Command::new(&manifest.command);
        command.args(&manifest.args);
        if let Some(working_dir) = &manifest.working_dir {
            command.current_dir(working_dir);
        }
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // gardend's own configuration stays with gardend: above all the master
        // key (`GARDEN_LOOPBACK_TOKEN`, which `gardend serve` puts in its
        // environment). The child gets its own member-scope token below.
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("GARDEN_") {
                command.env_remove(&key);
            }
        }
        for (key, value) in manifest.env.iter().chain(secrets.iter()) {
            command.env(key, value);
        }
        if service_id == CHOREOGRAPH_SERVICE_ID {
            command
                .env("CHOREOGRAPH_SANDBOX_MODE", "local-native")
                .env(CHOREOGRAPH_BIND_HOST_ENV, SERVICE_BIND_HOST);
        }
        command.env(SERVICE_HOST_ENV, SERVICE_BIND_HOST);
        let api_url = api_url.trim_end_matches('/').to_string();
        command
            .env("PORT", port.to_string())
            .env("DATA_DIR", &data_dir)
            .env("MNEMOSYNE_MCP_URL", format!("{api_url}/mcp"))
            .env("MNEMOSYNE_MCP_ACCESS_TOKEN", &mcp_token)
            .env("MNEMOSYNE_INTERNAL_SERVICE_SECRET", &internal_secret)
            .env("GARDEN_LOOPBACK_API_URL", &api_url)
            .env("GARDEN_SERVICE_ID", service_id)
            .env("GARDEN_PROFILE_ID", PROFILE_ID);
        place_child_in_new_process_group(&mut command);

        let mut child = command.spawn().map_err(|error| {
            format!(
                "start {service_id} service ({}): {error}",
                display_path(&manifest.command)
            )
        })?;
        let pid = child.id();
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        runtime.logs = Arc::new(Mutex::new(VecDeque::new()));
        if let Some(stdout) = stdout {
            spawn_log_reader(runtime.logs.clone(), "stdout", stdout);
        }
        if let Some(stderr) = stderr {
            spawn_log_reader(runtime.logs.clone(), "stderr", stderr);
        }

        let record_path = supervisor_record_path(&profile, service_id);
        let record = SupervisorRecord {
            service_id: service_id.to_string(),
            pid,
            start: process_start_signature(pid),
            gardend_pid: std::process::id(),
            started_at: timestamp(),
        };
        if let Err(error) = crate::storage::write_secret_json(&record_path, &record) {
            log::warn!(
                "local service {service_id}: write {}: {error}",
                display_path(&record_path)
            );
        }

        self.set_service_token(service_id, runtime.mcp_token.as_deref(), Some(&mcp_token));
        runtime.port = Some(port);
        runtime.internal_secret = Some(internal_secret);
        runtime.mcp_token = Some(mcp_token);
        runtime.started_at = Some(timestamp());
        runtime.launched = Some(Instant::now());
        runtime.last_error = None;
        runtime.manifest = Some(manifest);
        runtime.record_path = Some(record_path);
        runtime.child = Some(child);
        log::info!("local service {service_id}: started (pid {pid}, port {port})");
        Ok(())
    }

    /// Notice a child that exited: forget it, end what it left in its process
    /// group, and schedule a restart (a crash while wanted), or give up after
    /// too many crashes in a row.
    fn reap(&self, service_id: &str, runtime: &mut ServiceRuntime, timing: &SupervisionTiming) {
        let Some(child) = runtime.child.as_mut() else {
            return;
        };
        let status = match child.try_wait() {
            Ok(None) => return,
            Ok(Some(status)) => Ok(status),
            Err(error) => Err(error),
        };
        let pid = child.id();
        runtime.child = None;
        end_process_group(pid, timing.stop_grace.min(Duration::from_secs(2)));
        self.forget_process(service_id, runtime);
        let ran_for = runtime.launched.take().map(|at| at.elapsed());
        match status {
            Ok(status) if status.success() => {
                // A clean exit (someone stopped it on purpose) is not a crash.
                runtime.wanted = false;
                runtime.restarts = 0;
                runtime.restart_at = None;
                runtime.last_error = None;
                log::info!("local service {service_id}: exited cleanly");
            }
            outcome => {
                let why = match outcome {
                    Ok(status) => format!("exited with {status}"),
                    Err(error) => format!("poll service process: {error}"),
                };
                if !runtime.wanted {
                    runtime.last_error = Some(why);
                    return;
                }
                if ran_for.is_some_and(|ran| ran >= timing.stable_after) {
                    runtime.restarts = 0;
                }
                runtime.restarts += 1;
                if runtime.restarts > timing.max_restarts {
                    runtime.wanted = false;
                    runtime.restart_at = None;
                    runtime.last_error = Some(format!(
                        "{why}; gave up after {} restarts in a row (start it again with POST /api/{service_id}/start)",
                        timing.max_restarts
                    ));
                    log::error!("local service {service_id}: {why}; gave up restarting it");
                } else {
                    let wait = timing.backoff(runtime.restarts);
                    runtime.restart_at = Some(Instant::now() + wait);
                    runtime.last_error = Some(format!(
                        "{why}; restarting in {} ms (restart {})",
                        wait.as_millis(),
                        runtime.restarts
                    ));
                    log::warn!(
                        "local service {service_id}: {why}; restarting in {} ms",
                        wait.as_millis()
                    );
                }
            }
        }
    }

    /// Clear what identified a child that is gone.
    fn forget_process(&self, service_id: &str, runtime: &mut ServiceRuntime) {
        self.set_service_token(service_id, runtime.mcp_token.as_deref(), None);
        self.forget_delegations(service_id);
        runtime.mcp_token = None;
        runtime.port = None;
        runtime.internal_secret = None;
        runtime.started_at = None;
        if let Some(path) = runtime.record_path.take() {
            let _ = std::fs::remove_file(path);
        }
    }

    /// One supervision pass: reap exited children, restart the due ones.
    fn supervise_once(&self) {
        if self.shutting_down.load(Ordering::SeqCst) {
            return;
        }
        let timing = self.timing();
        let mut services = self.services.lock().expect("local service registry lock");
        for (service_id, runtime) in services.iter_mut() {
            self.reap(service_id, runtime, &timing);
            let due = runtime.wanted
                && runtime.child.is_none()
                && runtime.restart_at.is_some_and(|at| Instant::now() >= at);
            if !due {
                continue;
            }
            runtime.restart_at = None;
            if let Err(error) = self.launch(service_id, runtime) {
                if runtime.unavailable.is_some() {
                    // The computer lacks what it needs: retrying cannot help.
                    runtime.wanted = false;
                    runtime.last_error = Some(error.clone());
                    log::warn!("local service {service_id}: not restarted: {error}");
                    continue;
                }
                // A failed start counts like a crash.
                runtime.restarts += 1;
                if runtime.restarts > timing.max_restarts {
                    runtime.wanted = false;
                    runtime.last_error = Some(format!(
                        "{error}; gave up after {} restarts in a row",
                        timing.max_restarts
                    ));
                } else {
                    let wait = timing.backoff(runtime.restarts);
                    runtime.restart_at = Some(Instant::now() + wait);
                    runtime.last_error =
                        Some(format!("{error}; retrying in {} ms", wait.as_millis()));
                }
                log::warn!("local service {service_id}: restart failed: {error}");
            }
        }
    }

    fn ensure_supervisor(self: &Arc<Self>) {
        if self.supervisor_started.swap(true, Ordering::SeqCst) {
            return;
        }
        let host: Weak<Self> = Arc::downgrade(self);
        let spawned = thread::Builder::new()
            .name("gardend-service-supervisor".into())
            .spawn(move || loop {
                thread::sleep(SUPERVISOR_TICK);
                let Some(host) = host.upgrade() else {
                    return;
                };
                if host.shutting_down.load(Ordering::SeqCst) {
                    return;
                }
                host.supervise_once();
            });
        if let Err(error) = spawned {
            self.supervisor_started.store(false, Ordering::SeqCst);
            log::error!("local services: cannot start the supervisor thread: {error}");
        }
    }

    pub(crate) fn stop(
        &self,
        service_id: &str,
        app: &AppHandle,
    ) -> Result<LocalServiceStatus, String> {
        if !is_supported_service_id(service_id) {
            return Err(format!("unsupported local service: {service_id}"));
        }
        let configured = self.configured(service_id, app);
        let timing = self.timing();
        let child = {
            let mut services = self.services.lock().expect("local service registry lock");
            let runtime = services.entry(service_id.to_string()).or_default();
            runtime.wanted = false;
            runtime.restart_at = None;
            runtime.restarts = 0;
            runtime.launched = None;
            let child = runtime.child.take();
            self.forget_process(service_id, runtime);
            child
        };
        if let Some(mut child) = child {
            stop_process_group(&mut child, timing.stop_grace);
            log::info!("local service {service_id}: stopped");
        }
        let mut services = self.services.lock().expect("local service registry lock");
        let runtime = services.entry(service_id.to_string()).or_default();
        runtime.last_error = None;
        runtime.unavailable = None;
        Ok(runtime.status(service_id, configured || runtime.manifest.is_some()))
    }

    /// Stop every service and its whole process group, and start nothing
    /// again (gardend's exit path). Blocks up to the stop grace.
    pub(crate) fn shutdown(&self) {
        self.shutting_down.store(true, Ordering::SeqCst);
        let timing = self.timing();
        let children: Vec<(String, Child)> = {
            let mut services = self.services.lock().expect("local service registry lock");
            services
                .iter_mut()
                .filter_map(|(service_id, runtime)| {
                    runtime.wanted = false;
                    runtime.restart_at = None;
                    let child = runtime.child.take();
                    self.forget_process(service_id, runtime);
                    child.map(|child| (service_id.clone(), child))
                })
                .collect()
        };
        let handles: Vec<_> = children
            .into_iter()
            .map(|(service_id, mut child)| {
                thread::spawn(move || {
                    stop_process_group(&mut child, timing.stop_grace);
                    log::info!("local service {service_id}: stopped (gardend exiting)");
                })
            })
            .collect();
        for handle in handles {
            let _ = handle.join();
        }
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

    /// Why `service_id` is not running, when the reason is that this computer
    /// lacks what its `service.json` requires (words for the person).
    pub(crate) fn unavailable_reason(&self, service_id: &str) -> Option<String> {
        let services = self.services.lock().expect("local service registry lock");
        services
            .get(service_id)
            .filter(|runtime| runtime.child.is_none())
            .and_then(|runtime| runtime.unavailable.clone())
    }

    pub(crate) fn target(&self, service_id: &str) -> Result<LocalServiceTarget, String> {
        if !is_supported_service_id(service_id) {
            return Err(format!("unsupported local service: {service_id}"));
        }
        let timing = self.timing();
        let mut services = self.services.lock().expect("local service registry lock");
        let runtime = services.entry(service_id.to_string()).or_default();
        self.reap(service_id, runtime, &timing);
        if runtime.child.is_none() {
            return Err(if runtime.restart_at.is_some() {
                "local service is restarting".to_string()
            } else {
                "local service is not running".to_string()
            });
        }
        let port = runtime
            .port
            .ok_or_else(|| "local service has no private port".to_string())?;
        let internal_secret = runtime
            .internal_secret
            .clone()
            .ok_or_else(|| "local service has no internal secret".to_string())?;
        Ok(LocalServiceTarget {
            base_url: format!("http://{SERVICE_BIND_HOST}:{port}"),
            internal_secret,
        })
    }
}

/// Start the services gardend runs on its own: every configured service
/// ([`LocalServiceHost::service_ids`]) that autostarts
/// ([`service_autostarts`]: the profile's `gardend service enable|disable`,
/// else its `service.json`'s `autostart`, and never when
/// `GARDEN_<ID>_AUTOSTART` is off: `0`, `false`, `off`, `no`). Never in a
/// hosted cell. One log-ready line per service considered; nothing
/// configured, nothing started.
pub(crate) fn autostart_local_services(
    host: &Arc<LocalServiceHost>,
    app: &AppHandle,
    loopback_manifest: &LoopbackManifest,
    hosted_cell: bool,
) -> Vec<String> {
    if hosted_cell {
        return Vec::new();
    }
    let mut lines = Vec::new();
    let profile = profile_dir(app).ok();
    for service_id in host.service_ids(app) {
        let manifest = match host.resolve_manifest(app, &service_id) {
            Ok(manifest) => manifest,
            Err(reason) => {
                lines.push(format!("{service_id}: not started: {reason}"));
                continue;
            }
        };
        if !service_autostarts(&service_id, manifest.autostart, profile.as_deref()) {
            lines.push(format!(
                "{service_id}: configured, not started (off at boot; `gardend service enable {service_id}` turns it on, POST /api/services/{service_id}/start starts it now)"
            ));
            continue;
        }
        lines.push(match host.start(&service_id, app, loopback_manifest) {
            Ok(status) => format!(
                "{service_id}: started (pid {})",
                status.pid.map(|pid| pid.to_string()).unwrap_or_default()
            ),
            Err(error) => format!("{service_id}: could not start: {error}"),
        });
    }
    lines
}

/// Whether gardend starts `service_id` when it boots: never when
/// `GARDEN_<ID>_AUTOSTART` is off; else the profile's choice
/// ([`read_service_autostart`]); else the manifest's `autostart`.
pub(crate) fn service_autostarts(
    service_id: &str,
    manifest_autostart: bool,
    profile_dir: Option<&Path>,
) -> bool {
    let autostart_var = format!("GARDEN_{}_AUTOSTART", service_env_prefix(service_id));
    if !autostart_enabled(std::env::var(&autostart_var).ok().as_deref()) {
        return false;
    }
    profile_dir
        .and_then(|dir| read_service_autostart(dir, service_id).ok().flatten())
        .unwrap_or(manifest_autostart)
}

/// `<profile>/services/<id>.autostart`: `on` or `off`, the person's choice
/// for this profile (`gardend service enable|disable <id>`), over the
/// manifest's `autostart`. Owner-only.
pub(crate) fn service_autostart_file(profile_dir: &Path, service_id: &str) -> PathBuf {
    profile_dir
        .join("services")
        .join(format!("{service_id}.autostart"))
}

/// The profile's choice for `service_id`, if it made one.
pub(crate) fn read_service_autostart(
    profile_dir: &Path,
    service_id: &str,
) -> Result<Option<bool>, String> {
    let path = service_autostart_file(profile_dir, service_id);
    match std::fs::read_to_string(&path) {
        Ok(text) => match text.trim() {
            "on" => Ok(Some(true)),
            "off" => Ok(Some(false)),
            other => Err(format!(
                "{} says {other:?}; expected on or off",
                display_path(&path)
            )),
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("read {}: {error}", display_path(&path))),
    }
}

pub(crate) fn write_service_autostart(
    profile_dir: &Path,
    service_id: &str,
    enabled: bool,
) -> Result<PathBuf, String> {
    if !is_service_id(service_id) {
        return Err(format!(
            "{service_id:?} is not a service id (lowercase letters, digits and dashes)"
        ));
    }
    let path = service_autostart_file(profile_dir, service_id);
    let text = if enabled { "on\n" } else { "off\n" };
    crate::storage_file_ops::write_secret_bytes(&path, text.as_bytes())
        .map_err(|error| format!("write {}: {error}", display_path(&path)))?;
    Ok(path)
}

fn autostart_enabled(value: Option<&str>) -> bool {
    !matches!(
        value
            .map(|value| value.trim().to_ascii_lowercase())
            .as_deref(),
        Some("0" | "false" | "off" | "no")
    )
}

impl ServiceRuntime {
    fn status(&self, service_id: &str, configured: bool) -> LocalServiceStatus {
        let state = if self.child.is_some() {
            LocalServiceState::Running
        } else if self.wanted && self.restart_at.is_some() {
            LocalServiceState::Restarting
        } else if !configured {
            LocalServiceState::Unconfigured
        } else if self.unavailable.is_some() {
            LocalServiceState::Unavailable
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
            restarts: self.restarts,
            health: None,
            last_error: self.last_error.clone(),
            reason: (state == LocalServiceState::Unavailable)
                .then(|| self.unavailable.clone())
                .flatten(),
            enabled: None,
        }
    }
}

// ---- process groups -----------------------------------------------------

#[cfg(unix)]
mod signals {
    extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }
    pub(super) const SIGTERM: i32 = 15;
    pub(super) const SIGKILL: i32 = 9;
    const EPERM: i32 = 1;

    /// Signal a process (`target > 0`) or a process group (`target < 0`).
    /// Never 0, -1 or 1: those address the caller's group, everything, init.
    pub(super) fn send(target: i32, signal: i32) -> bool {
        if matches!(target, -1..=1) {
            return false;
        }
        unsafe { kill(target, signal) == 0 }
    }

    /// The process or group exists (possibly owned by someone else).
    pub(super) fn exists(target: i32) -> bool {
        if matches!(target, -1..=1) {
            return false;
        }
        if unsafe { kill(target, 0) } == 0 {
            return true;
        }
        std::io::Error::last_os_error().raw_os_error() == Some(EPERM)
    }
}

/// Stop a child gardend spawned with its own process group: SIGTERM to the
/// group (as systemd stops a unit), then SIGKILL to whatever is left once the
/// grace runs out. Reaps the child.
fn stop_process_group(child: &mut Child, grace: Duration) {
    #[cfg(unix)]
    {
        let pgid = child.id() as i32;
        signals::send(-pgid, signals::SIGTERM);
        let deadline = Instant::now() + grace;
        let mut leader_done = false;
        loop {
            if !leader_done {
                leader_done = !matches!(child.try_wait(), Ok(None));
            }
            if leader_done && !signals::exists(-pgid) {
                break;
            }
            if Instant::now() >= deadline {
                signals::send(-pgid, signals::SIGKILL);
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// End what is left of a process group whose leader is gone.
fn end_process_group(pgid: u32, grace: Duration) {
    #[cfg(unix)]
    {
        let pgid = pgid as i32;
        if !signals::exists(-pgid) {
            return;
        }
        signals::send(-pgid, signals::SIGTERM);
        let deadline = Instant::now() + grace;
        while Instant::now() < deadline {
            if !signals::exists(-pgid) {
                return;
            }
            thread::sleep(Duration::from_millis(50));
        }
        signals::send(-pgid, signals::SIGKILL);
    }
    #[cfg(not(unix))]
    let _ = (pgid, grace);
}

/// The OS's record of when `pid` started: field 22 of `/proc/<pid>/stat` on
/// Linux, `ps -o lstart=` elsewhere. Equal values mean the same process.
fn process_start_signature(pid: u32) -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let after_name = &stat[stat.rfind(')')? + 1..];
        after_name.split_whitespace().nth(19).map(str::to_string)
    }
    #[cfg(all(unix, not(target_os = "linux")))]
    {
        let output = Command::new("ps")
            .args(["-o", "lstart=", "-p", &pid.to_string()])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .ok()?;
        let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
        (output.status.success() && !text.is_empty()).then_some(text)
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        None
    }
}

pub(crate) fn supervisor_record_path(profile_dir: &Path, service_id: &str) -> PathBuf {
    profile_dir
        .join("services")
        .join(format!("{service_id}.{SUPERVISOR_RECORD_SUFFIX}"))
}

/// End the process group a previous gardend started and could not stop (it
/// was killed, or crashed). Only a process that is provably that child (the
/// same pid with the same start time), or the leaderless remains of its group,
/// is signalled; a recycled pid is left alone. Removes the record. Returns
/// what was done, for the log.
fn end_orphan(record_path: &Path, timing: &SupervisionTiming) -> Option<String> {
    let raw = std::fs::read(record_path).ok()?;
    let _ = std::fs::remove_file(record_path);
    let record: SupervisorRecord = serde_json::from_slice(&raw).ok()?;
    #[cfg(unix)]
    {
        let pid = record.pid as i32;
        if pid <= 1 || record.pid == std::process::id() {
            return None;
        }
        if signals::exists(pid) {
            let same_process = match (&record.start, process_start_signature(record.pid)) {
                (Some(recorded), Some(now)) => *recorded == now,
                _ => false,
            };
            if !same_process {
                return Some(format!(
                    "pid {} from a previous gardend is now another process; left alone",
                    record.pid
                ));
            }
        } else if !signals::exists(-pid) {
            return None;
        }
        end_process_group(record.pid, timing.stop_grace);
        Some(format!(
            "ended the {} a previous gardend (pid {}) left running (process group {})",
            record.service_id, record.gardend_pid, record.pid
        ))
    }
    #[cfg(not(unix))]
    {
        let _ = (record, timing);
        None
    }
}

// ---- per-profile service settings ----------------------------------------

/// The profile's settings and secrets for the services gardend supervises,
/// which a shipped `service.json` cannot hold, above all model keys
/// (`gardend chat key set`, `gardend service env set`): `KEY=VALUE` lines,
/// `#` comments, owner-only. `<profile>/services.env` reaches every service;
/// `<profile>/services/<id>.env` one service, over it. Both are merged into
/// a service's environment over the manifest's `env`, under the values
/// gardend owns (`PORT`, `DATA_DIR`, the tokens and secrets).
pub(crate) fn shared_service_env_file(profile_dir: &Path) -> PathBuf {
    profile_dir.join("services.env")
}

pub(crate) fn service_env_file(profile_dir: &Path, service_id: &str) -> PathBuf {
    profile_dir
        .join("services")
        .join(format!("{service_id}.env"))
}

/// Variables gardend sets itself; a settings file cannot override them.
pub(crate) fn gardend_owned_env(name: &str) -> bool {
    name.starts_with("GARDEN_")
        || name.starts_with("MNEMOSYNE_")
        || matches!(
            name,
            "PORT" | "DATA_DIR" | "CHOREOGRAPH_SANDBOX_MODE" | CHOREOGRAPH_BIND_HOST_ENV
        )
}

pub(crate) fn is_env_name(name: &str) -> bool {
    name.chars()
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
        && name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
}

pub(crate) fn parse_service_env(text: &str) -> Vec<(String, String)> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .filter_map(|line| {
            let line = line.strip_prefix("export ").unwrap_or(line);
            let (key, value) = line.split_once('=')?;
            let key = key.trim();
            is_env_name(key).then(|| (key.to_string(), value.trim().to_string()))
        })
        .collect()
}

pub(crate) fn read_service_env_file(path: &Path) -> Result<Vec<(String, String)>, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(parse_service_env(&text)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(format!("read {}: {error}", display_path(path))),
    }
}

/// Write a settings file (owner-only), keeping its order. `reaches` says
/// which services read it, for the reader of the file.
pub(crate) fn write_service_env_file(
    path: &Path,
    reaches: &str,
    entries: &[(String, String)],
) -> Result<(), String> {
    let mut text = format!(
        "# Settings and secrets gardend gives {reaches}.\n\
         # KEY=VALUE per line. Written by `gardend service env set` and `gardend chat key set`;\n\
         # keep it owner-only.\n"
    );
    for (key, value) in entries {
        text.push_str(&format!("{key}={value}\n"));
    }
    crate::storage_file_ops::write_secret_bytes(path, text.as_bytes())
        .map_err(|error| format!("write {}: {error}", display_path(path)))
}

// ---- manifests -----------------------------------------------------------

/// A service id: lowercase letters, digits and dashes, starting with a
/// letter or digit, at most 63 characters (it names a folder, a URL segment
/// and a `GARDEN_<ID>_*` variable).
pub(crate) fn is_service_id(service_id: &str) -> bool {
    !service_id.is_empty()
        && service_id.len() <= 63
        && service_id
            .chars()
            .next()
            .is_some_and(|first| first.is_ascii_lowercase() || first.is_ascii_digit())
        && service_id
            .chars()
            .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-')
}

fn is_supported_service_id(service_id: &str) -> bool {
    is_service_id(service_id)
}

/// The service folders: `GARDEN_SERVICES_DIR`, then `services/` beside the
/// binary.
pub(crate) fn services_dirs_from(
    services_dir: Option<&str>,
    resource_dir: Option<&Path>,
) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(dir) = services_dir.map(str::trim).filter(|dir| !dir.is_empty()) {
        dirs.push(PathBuf::from(dir));
    }
    if let Some(dir) = resource_dir {
        dirs.push(dir.join("services"));
    }
    dirs
}

/// The ids named by `GARDEN_<ID>_MANIFEST` / `GARDEN_<ID>_COMMAND` in `vars`
/// and by `<dir>/<id>/service.json` in `dirs`.
pub(crate) fn discover_service_ids(
    vars: impl IntoIterator<Item = (String, String)>,
    dirs: &[PathBuf],
) -> std::collections::BTreeSet<String> {
    let mut ids = std::collections::BTreeSet::new();
    for (name, value) in vars {
        let Some(rest) = name.strip_prefix("GARDEN_") else {
            continue;
        };
        let Some(stem) = rest
            .strip_suffix("_MANIFEST")
            .or_else(|| rest.strip_suffix("_COMMAND"))
        else {
            continue;
        };
        let id = stem.to_ascii_lowercase().replace('_', "-");
        if !value.trim().is_empty() && is_service_id(&id) {
            ids.insert(id);
        }
    }
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let id = entry.file_name().to_string_lossy().to_string();
            if is_service_id(&id) && entry.path().join("service.json").is_file() {
                ids.insert(id);
            }
        }
    }
    ids
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
        restarts: 0,
        health: None,
        last_error: Some(format!("unsupported local service: {service_id}")),
        reason: None,
        enabled: None,
    }
}

fn resolve_service_manifest(
    service_id: &str,
    services_dirs: &[PathBuf],
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
                autostart: true,
                member_access: MemberAccess::None,
                requires_node: None,
                display_name: None,
                delegate_caller: false,
                accepts_model_key: false,
                mcp: None,
            });
        }
    }
    // `GARDEN_SERVICES_DIR`, then beside the binary: `resource_dir` is the
    // binary's own folder with symlinks resolved
    // (`app_runtime::binary_resource_dir`) for gardend, and the bundle's
    // resources for the desktop app.
    for dir in services_dirs {
        let path = dir.join(service_id).join("service.json");
        if path.is_file() {
            return read_service_manifest(&path, service_id);
        }
    }
    Err(format!(
        "{service_id} service is not configured; set {manifest_var} or {command_var}, or put {service_id}/service.json in {SERVICES_DIR_ENV} or in services/ next to gardend"
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
        autostart: parsed.autostart.unwrap_or(true),
        member_access: parsed.member_access,
        requires_node: match parsed.requires.node.as_deref() {
            None => None,
            Some(raw) => Some(NodeVersion::parse(raw).ok_or_else(|| {
                format!(
                    "local service manifest {}: requires.node {raw:?} is not a version like \"22.19\"",
                    display_path(path)
                )
            })?),
        },
        display_name: parsed
            .display_name
            .map(|name| name.trim().to_string())
            .filter(|name| !name.is_empty()),
        delegate_caller: parsed.delegate_caller,
        accepts_model_key: parsed.accepts_model_key,
        mcp: parsed.mcp.map(|mcp| mcp.resolve(path)).transpose()?,
    })
}

/// The PATH a service's child gets: the last `PATH` its settings name (the
/// manifest's `env`, then the profile's settings files), else gardend's own.
fn child_path_var(
    manifest_env: &[(String, String)],
    settings: &[(String, String)],
) -> Option<std::ffi::OsString> {
    manifest_env
        .iter()
        .chain(settings.iter())
        .filter(|(key, _)| key == "PATH")
        .last()
        .map(|(_, value)| std::ffi::OsString::from(value))
        .or_else(|| std::env::var_os("PATH"))
}

/// The Node.js a service would run on: its command when that is `node` (a
/// path, or a name looked up on `path_var`), else `node` on `path_var`.
fn node_program(command: &Path, path_var: Option<&std::ffi::OsStr>) -> Option<PathBuf> {
    let is_node = command
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name == "node" || name.eq_ignore_ascii_case("node.exe"));
    if is_node && command.components().count() > 1 {
        return is_executable_file(command).then(|| command.to_path_buf());
    }
    let names: &[&str] = if cfg!(windows) {
        &["node.exe", "node"]
    } else {
        &["node"]
    };
    let dirs: Vec<PathBuf> = path_var
        .map(|path| std::env::split_paths(path).collect())
        .unwrap_or_default();
    dirs.iter()
        .filter(|dir| !dir.as_os_str().is_empty())
        .flat_map(|dir| names.iter().map(move |name| dir.join(name)))
        .find(|candidate| is_executable_file(candidate))
}

fn is_executable_file(path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.is_file() && meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        meta.is_file()
    }
}

/// `node --version`, run as the service would run it (its PATH), within a
/// few seconds.
fn node_version(program: &Path, path_var: Option<&std::ffi::OsStr>) -> Result<NodeVersion, String> {
    let mut command = Command::new(program);
    command
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    if let Some(path) = path_var {
        command.env("PATH", path);
    }
    let mut child = command
        .spawn()
        .map_err(|error| format!("cannot run it: {error}"))?;
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(20)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("`node --version` did not answer within 10 s".to_string());
            }
            Err(error) => return Err(format!("waiting for `node --version`: {error}")),
        }
    };
    let mut out = String::new();
    if let Some(mut stdout) = child.stdout.take() {
        let _ = stdout.read_to_string(&mut out);
    }
    if !status.success() {
        return Err(format!("`node --version` exited with {status}"));
    }
    let first = out.lines().next().unwrap_or_default().trim();
    NodeVersion::parse(first).ok_or_else(|| format!("`node --version` printed {first:?}"))
}

/// Check `requires.node`. Err: (the reason for the person, which
/// `/self-host.json` and the chat panel show and which names no path; the
/// same with the path, for gardend's log).
fn check_node_requirement(
    required: NodeVersion,
    command: &Path,
    path_var: Option<&std::ffi::OsStr>,
    label: &str,
) -> Result<(), (String, String)> {
    let shown_path = path_var
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_default();
    let Some(program) = node_program(command, path_var) else {
        let reason = format!(
            "{label} needs Node.js {required} or newer, and there is no node on gardend's PATH. Install Node.js {required} or newer, then restart gardend."
        );
        return Err((reason.clone(), format!("{reason} (PATH: {shown_path})")));
    };
    match node_version(&program, path_var) {
        Ok(found) if found >= required => Ok(()),
        Ok(found) => {
            let reason = format!(
                "{label} needs Node.js {required} or newer; the node on gardend's PATH is {found}. Put Node.js {required} or newer first on gardend's PATH, then restart gardend."
            );
            Err((
                reason.clone(),
                format!("{reason} ({})", display_path(&program)),
            ))
        }
        Err(why) => {
            let reason = format!(
                "{label} needs Node.js {required} or newer; the node on gardend's PATH did not say its version."
            );
            Err((
                reason.clone(),
                format!("{reason} ({}: {why})", display_path(&program)),
            ))
        }
    }
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

    fn delegation(role: &str, graph_id: &str) -> Delegation {
        Delegation {
            service_id: "vehicle".into(),
            user_id: "owner".into(),
            graph_id: graph_id.into(),
            role: role.into(),
            scopes: vec!["mcp.tools.call".into(), "documents.read".into()],
            public: false,
            visitor: None,
        }
    }

    /// A delegation token works while it is young and its service runs: it is
    /// reused until half its life is gone, then a fresh one is handed out (the
    /// older one works on until it expires), and every one of them dies with
    /// the service's process.
    #[test]
    fn delegation_tokens_rotate_at_half_life_expire_and_die_with_the_process() {
        let host = LocalServiceHost::default();
        let owner = delegation("owner", "notes");
        assert_eq!(
            host.delegate(owner.clone()),
            None,
            "no running process, nothing to hand a token to"
        );
        host.set_service_token("vehicle", None, Some("sophia-svc-process"));
        let t0 = Instant::now();
        let minute = Duration::from_secs(60);
        let first = host.delegate_at(owner.clone(), t0).unwrap();
        assert!(first.starts_with(DELEGATED_TOKEN_PREFIX), "{first}");
        assert_eq!(first.len(), DELEGATED_TOKEN_PREFIX.len() + 64);
        assert_eq!(host.delegation_at(&first, t0), Some(owner.clone()));
        assert_eq!(
            host.delegate_at(owner.clone(), t0 + 29 * minute).as_deref(),
            Some(first.as_str()),
            "reused while more than half its life is left"
        );
        let second = host.delegate_at(owner.clone(), t0 + 31 * minute).unwrap();
        assert_ne!(second, first, "a fresh token past half-life");
        assert_eq!(
            host.delegation_at(&first, t0 + 31 * minute),
            Some(owner.clone()),
            "the older token works on until it expires"
        );
        assert_eq!(host.delegation_at(&first, t0 + 61 * minute), None);
        assert_eq!(
            host.delegation_at(&second, t0 + 61 * minute),
            Some(owner.clone())
        );

        let viewer = host.delegate_at(delegation("viewer", "notes"), t0).unwrap();
        let elsewhere = host.delegate_at(delegation("owner", "other"), t0).unwrap();
        assert!(viewer != first && viewer != elsewhere && elsewhere != first);
        assert_eq!(
            host.delegation_at(&viewer, t0).map(|d| d.role),
            Some("viewer".to_string())
        );
        assert_eq!(
            host.delegation_at(&elsewhere, t0).map(|d| d.graph_id),
            Some("other".to_string())
        );
        for not_a_delegation in ["sophia-svc-process", "", "sophia-dlg-unknown", "master"] {
            assert_eq!(
                host.delegation(not_a_delegation),
                None,
                "{not_a_delegation}"
            );
        }

        host.forget_delegations("vehicle");
        assert_eq!(host.delegation_at(&second, t0 + 31 * minute), None);
        assert_eq!(host.delegation_at(&viewer, t0), None);
    }

    /// `"delegateCaller": true` in `service.json` asks for delegation tokens;
    /// a service that does not say so gets none.
    #[test]
    fn a_manifest_asks_for_delegation_tokens() {
        let dir = std::env::temp_dir().join(format!("garden-delegate-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("service.json");
        std::fs::write(
            &path,
            br#"{"serviceId":"vehicle","command":"node","delegateCaller":true}"#,
        )
        .unwrap();
        assert!(
            read_service_manifest(&path, "vehicle")
                .unwrap()
                .delegate_caller
        );
        std::fs::write(&path, br#"{"serviceId":"vehicle","command":"node"}"#).unwrap();
        assert!(
            !read_service_manifest(&path, "vehicle")
                .unwrap()
                .delegate_caller
        );
        let _ = std::fs::remove_dir_all(dir);
    }

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

    #[test]
    fn node_versions_parse_as_manifests_and_node_write_them() {
        assert_eq!(NodeVersion::parse("22.19"), Some(NodeVersion(22, 19, 0)));
        assert_eq!(
            NodeVersion::parse(">=22.19.0"),
            Some(NodeVersion(22, 19, 0))
        );
        assert_eq!(
            NodeVersion::parse("v22.23.2\n"),
            Some(NodeVersion(22, 23, 2))
        );
        assert_eq!(
            NodeVersion::parse("v24.0.0-nightly2026"),
            Some(NodeVersion(24, 0, 0))
        );
        assert_eq!(NodeVersion::parse("22"), Some(NodeVersion(22, 0, 0)));
        for bad in ["", "node", "v22.x", "1.2.3.4", "^22"] {
            assert_eq!(NodeVersion::parse(bad), None, "{bad}");
        }
        assert!(NodeVersion(22, 19, 0) > NodeVersion(22, 18, 9));
        assert!(NodeVersion(23, 0, 0) > NodeVersion(22, 19, 0));
        assert_eq!(NodeVersion(22, 19, 0).to_string(), "22.19");
        assert_eq!(NodeVersion(20, 11, 1).to_string(), "20.11.1");
        let dir = std::env::temp_dir().join(format!("garden-svc-manifest-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("service.json");
        std::fs::write(&path, r#"{"serviceId":"vehicle","command":"node","args":["app/main.mjs"],"requires":{"node":"22.19"},"displayName":" Chat "}"#).unwrap();
        let manifest = read_service_manifest(&path, "vehicle").unwrap();
        assert_eq!(manifest.requires_node, Some(NodeVersion(22, 19, 0)));
        assert_eq!(manifest.display_name.as_deref(), Some("Chat"));
        std::fs::write(
            &path,
            r#"{"serviceId":"vehicle","command":"node","requires":{"node":"latest"}}"#,
        )
        .unwrap();
        let error = read_service_manifest(&path, "vehicle").unwrap_err();
        assert!(error.contains("requires.node"), "{error}");
        std::fs::write(&path, r#"{"serviceId":"vehicle","command":"node"}"#).unwrap();
        assert_eq!(
            read_service_manifest(&path, "vehicle")
                .unwrap()
                .requires_node,
            None
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_service_finds_node_on_the_path_it_will_get() {
        let setting = |pairs: &[(&str, &str)]| -> Vec<(String, String)> {
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect()
        };
        assert_eq!(
            child_path_var(
                &setting(&[("PATH", "/manifest")]),
                &setting(&[("PATH", "/settings")])
            ),
            Some(std::ffi::OsString::from("/settings")),
            "the profile's settings come after the manifest"
        );
        assert_eq!(
            child_path_var(&setting(&[("PATH", "/manifest")]), &[]),
            Some(std::ffi::OsString::from("/manifest"))
        );
        assert_eq!(child_path_var(&[], &[]), std::env::var_os("PATH"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let root = std::env::temp_dir().join(format!("garden-svc-path-{}", Uuid::new_v4()));
            let (empty, bin) = (root.join("empty"), root.join("bin"));
            std::fs::create_dir_all(&empty).unwrap();
            std::fs::create_dir_all(&bin).unwrap();
            let node = bin.join("node");
            std::fs::write(&node, "#!/bin/sh\necho v22.19.0\n").unwrap();
            let path = std::env::join_paths([&empty, &bin]).unwrap();
            assert_eq!(
                node_program(Path::new("node"), Some(&path)),
                None,
                "not executable yet"
            );
            std::fs::set_permissions(&node, std::fs::Permissions::from_mode(0o755)).unwrap();
            assert_eq!(
                node_program(Path::new("node"), Some(&path)),
                Some(node.clone())
            );
            assert_eq!(
                node_program(Path::new("/elsewhere/python3"), Some(&path)),
                Some(node.clone()),
                "a non-node command: node on the PATH"
            );
            assert_eq!(
                node_program(&node, None),
                Some(node.clone()),
                "a node path is taken as it is"
            );
            assert_eq!(node_program(&empty.join("node"), Some(&path)), None);
            assert_eq!(node_version(&node, Some(&path)), Ok(NodeVersion(22, 19, 0)));
            assert!(check_node_requirement(
                NodeVersion(22, 19, 0),
                Path::new("node"),
                Some(&path),
                "Chat"
            )
            .is_ok());
            let (reason, detail) = check_node_requirement(
                NodeVersion(24, 0, 0),
                Path::new("node"),
                Some(&path),
                "Chat",
            )
            .unwrap_err();
            assert!(
                reason.starts_with(
                    "Chat needs Node.js 24.0 or newer; the node on gardend's PATH is 22.19."
                ),
                "{reason}"
            );
            assert!(detail.contains(&node.display().to_string()), "{detail}");
            let _ = std::fs::remove_dir_all(&root);
        }
    }

    #[test]
    fn service_env_files_parse_keys_values_and_comments() {
        let parsed = parse_service_env(
            "# comment\n\nOPENROUTER_API_KEY=sk-or-v1-abc=def\n export CHOREOGRAPH_WORKSPACE_CHAT_MODEL = google/gemini-2.5-flash \n9BAD=x\nno equals\nBAD-KEY=y\n",
        );
        assert_eq!(
            parsed,
            vec![
                (
                    "OPENROUTER_API_KEY".to_string(),
                    "sk-or-v1-abc=def".to_string()
                ),
                (
                    "CHOREOGRAPH_WORKSPACE_CHAT_MODEL".to_string(),
                    "google/gemini-2.5-flash".to_string()
                ),
            ]
        );
        assert_eq!(
            service_env_file(Path::new("/p"), CHOREOGRAPH_SERVICE_ID),
            PathBuf::from("/p/services/choreograph.env")
        );
    }

    #[test]
    fn backoff_doubles_up_to_the_cap_and_autostart_can_be_turned_off() {
        let timing = SupervisionTiming::default();
        assert_eq!(timing.backoff(1), Duration::from_secs(1));
        assert_eq!(timing.backoff(2), Duration::from_secs(2));
        assert_eq!(timing.backoff(4), Duration::from_secs(8));
        assert_eq!(timing.backoff(9), Duration::from_secs(60));
        assert_eq!(timing.backoff(40), Duration::from_secs(60));
        assert!(autostart_enabled(None));
        assert!(autostart_enabled(Some("1")));
        assert!(autostart_enabled(Some("")));
        for off in ["0", "false", "OFF", " no "] {
            assert!(!autostart_enabled(Some(off)), "{off}");
        }
    }

    /// The `services/` folder is found beside the real binary, not beside a
    /// symlink to it (`/usr/local/bin/gardend` → an unpacked release).
    #[cfg(unix)]
    #[test]
    fn the_binary_resource_dir_follows_symlinks() {
        let root = std::env::temp_dir().join(format!("garden-exe-link-{}", Uuid::new_v4()));
        let release = root.join("gardend-0.1.4");
        let bin = root.join("bin");
        std::fs::create_dir_all(&release).unwrap();
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(release.join("gardend"), b"").unwrap();
        std::os::unix::fs::symlink(release.join("gardend"), bin.join("gardend")).unwrap();
        let resolved = crate::app_runtime::binary_resource_dir(&bin.join("gardend")).unwrap();
        assert_eq!(resolved, std::fs::canonicalize(&release).unwrap());
        // No symlink: the binary's own folder; a missing path is taken as is.
        assert_eq!(
            crate::app_runtime::binary_resource_dir(&release.join("gardend")).unwrap(),
            std::fs::canonicalize(&release).unwrap()
        );
        assert_eq!(
            crate::app_runtime::binary_resource_dir(Path::new("/nonexistent/dir/gardend")),
            Some(PathBuf::from("/nonexistent/dir"))
        );
        let _ = std::fs::remove_dir_all(root);
    }
}

/// Supervision tests against real child processes: the test binary itself,
/// re-run as a tiny HTTP service ([`child::child_main`]), which may start a
/// grandchild in its own process group that writes a heartbeat file.
#[cfg(all(test, feature = "headless"))]
pub(crate) mod child {
    use super::*;
    use axum::{
        body::{Body, Bytes},
        extract::Request,
        http::{header, StatusCode},
        response::{IntoResponse, Response},
        routing::{get, post},
        Json, Router,
    };

    /// Set on the child: what the re-run test binary should be.
    const ROLE_ENV: &str = "SOPHIA_TEST_SERVICE_CHILD";
    /// A file the child's grandchild touches every 50 ms.
    pub(crate) const HEARTBEAT_ENV: &str = "SOPHIA_TEST_HEARTBEAT";
    /// The SSE stream sends its second event once this file exists.
    pub(crate) const RELEASE_ENV: &str = "SOPHIA_TEST_RELEASE";
    /// The child exits 3 at once (a crash loop).
    pub(crate) const CRASH_ENV: &str = "SOPHIA_TEST_CRASH_AT_START";
    /// The child answers MCP JSON-RPC on `/mcp` ([`mcp`]).
    pub(crate) const MCP_ENV: &str = "SOPHIA_TEST_CHILD_MCP";

    /// The entry point of the re-run test binary. Run normally (no role in
    /// the environment) it does nothing.
    #[test]
    #[ignore = "a child process of the supervision tests, not a test of its own"]
    fn child_main() {
        match std::env::var(ROLE_ENV).as_deref() {
            Ok("serve") => serve(),
            Ok("heartbeat") => heartbeat(),
            _ => {}
        }
    }

    /// The arguments that make the test binary run [`child_main`] only.
    pub(crate) fn child_args() -> Vec<String> {
        [
            "--exact",
            "local_service_host::child::child_main",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ]
        .iter()
        .map(|arg| arg.to_string())
        .collect()
    }

    /// A Choreograph `service.json` in `dir` that runs the child server.
    /// A `node` on a PATH: a real executable that answers `--version` as
    /// Node.js does (the version is all `requires.node` asks of it).
    #[cfg(unix)]
    pub(crate) fn put_node_on(dir: &Path, version: &str) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::create_dir_all(dir).unwrap();
        let node = dir.join("node");
        std::fs::write(&node, format!("#!/bin/sh\necho v{version}\n")).unwrap();
        std::fs::set_permissions(&node, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    pub(crate) fn write_manifest(dir: &Path, env: &[(&str, String)]) -> PathBuf {
        write_service_manifest(dir, CHOREOGRAPH_SERVICE_ID, env, serde_json::json!({}))
    }

    /// A `service.json` for `service_id` in `dir` that runs the child server,
    /// with `env` and the extra manifest fields in `extra`.
    pub(crate) fn write_service_manifest(
        dir: &Path,
        service_id: &str,
        env: &[(&str, String)],
        extra: serde_json::Value,
    ) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let mut entries = vec![serde_json::json!({ "name": ROLE_ENV, "value": "serve" })];
        entries.extend(
            env.iter()
                .map(|(name, value)| serde_json::json!({ "name": name, "value": value })),
        );
        let mut manifest = serde_json::json!({
            "serviceId": service_id,
            "command": std::env::current_exe().unwrap().display().to_string(),
            "args": child_args(),
            "env": entries,
        });
        if let Some(extra) = extra.as_object() {
            for (key, value) in extra {
                manifest[key] = value.clone();
            }
        }
        let path = dir.join("service.json");
        std::fs::write(&path, serde_json::to_vec_pretty(&manifest).unwrap()).unwrap();
        path
    }

    pub(crate) fn test_manifest() -> LoopbackManifest {
        let grant = crate::loopback_token_grants::session_all_token_grant();
        LoopbackManifest {
            runtime_profile: "test",
            bind_host: "127.0.0.1",
            port: 0,
            api_url: "https://sophia.example.com".into(),
            mcp_url: "https://sophia.example.com/mcp".into(),
            openapi_url: String::new(),
            token: "master".into(),
            pid: 0,
            started_at: String::new(),
            manifest_path: String::new(),
            auth_header: "Authorization: Bearer <token>",
            token_audience: "test",
            token_storage: "memory",
            security_warning: "synthetic test manifest",
            cell_graph_id: None,
            cell_owner: None,
            cell_generation: None,
            cell_registry_revision: None,
            capabilities: grant.scopes.clone(),
            token_scope_mode: grant.scope_mode,
            token_scopes: grant.scopes,
            scope_details: grant.scope_details,
            grant_profiles: grant.grant_profiles,
        }
    }

    pub(crate) fn fast_timing() -> SupervisionTiming {
        SupervisionTiming {
            restart_backoff: Duration::from_millis(200),
            restart_backoff_cap: Duration::from_secs(2),
            stable_after: Duration::from_secs(60),
            max_restarts: 3,
            stop_grace: Duration::from_secs(3),
        }
    }

    pub(crate) fn wait_until(what: &str, timeout: Duration, mut done: impl FnMut() -> bool) {
        let deadline = Instant::now() + timeout;
        while !done() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            thread::sleep(Duration::from_millis(50));
        }
    }

    pub(crate) fn get_json(url: &str) -> Option<serde_json::Value> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let response = reqwest::Client::new()
                .get(url)
                .timeout(Duration::from_secs(5))
                .send()
                .await
                .ok()?;
            response.json::<serde_json::Value>().await.ok()
        })
    }

    pub(crate) fn wait_healthy(host: &LocalServiceHost) -> LocalServiceTarget {
        wait_service_healthy(host, CHOREOGRAPH_SERVICE_ID)
    }

    pub(crate) fn wait_service_healthy(
        host: &LocalServiceHost,
        service_id: &str,
    ) -> LocalServiceTarget {
        let mut target = None;
        wait_until(
            "the child to answer /health",
            Duration::from_secs(30),
            || {
                target = host.target(service_id).ok();
                target
                    .as_ref()
                    .and_then(|target| get_json(&format!("{}/health", target.base_url)))
                    .is_some()
            },
        );
        target.unwrap()
    }

    fn heartbeat() {
        let path = std::env::var(HEARTBEAT_ENV).expect("heartbeat path");
        loop {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let _ = std::fs::write(&path, nanos.to_string());
            thread::sleep(Duration::from_millis(50));
        }
    }

    fn serve() {
        if std::env::var(CRASH_ENV).is_ok() {
            std::process::exit(3);
        }
        if let Ok(path) = std::env::var(HEARTBEAT_ENV) {
            // Same process group as this child: no setsid.
            Command::new(std::env::current_exe().unwrap())
                .args(child_args())
                .env(ROLE_ENV, "heartbeat")
                .env(HEARTBEAT_ENV, path)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("start heartbeat grandchild");
        }
        let host = std::env::var(CHOREOGRAPH_BIND_HOST_ENV)
            .or_else(|_| std::env::var(SERVICE_HOST_ENV))
            .unwrap_or_else(|_| "0.0.0.0".into());
        let port: u16 = std::env::var("PORT").unwrap().parse().unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async move {
            let mut app = Router::new()
                .route(
                    "/health",
                    get(|| async { Json(serde_json::json!({"ok": true})) }),
                )
                .route("/exit", post(exit_soon));
            if std::env::var(MCP_ENV).is_ok() {
                app = app.route("/mcp", post(mcp));
            }
            let app = app
                .route("/api/agent-sessions/{id}/events", get(events))
                .fallback(echo);
            let listener = tokio::net::TcpListener::bind((host.as_str(), port))
                .await
                .expect("bind the child's port");
            axum::serve(listener, app).await.unwrap();
        });
    }

    /// A tiny MCP server: `toy_read` (read-only) answers with what gardend
    /// told it, `toy_write` is not read-only, `toy_slow` sleeps past any
    /// test timeout, `toy_fail` fails; `read_document` (gardend's own name)
    /// and `stray` (outside the prefix) must never be lent. A request without
    /// the per-spawn secret is refused, as a real service refuses it.
    async fn mcp(request: Request) -> Response {
        let (parts, body) = request.into_parts();
        let header = |name: &str| {
            parts
                .headers
                .get(name)
                .and_then(|value| value.to_str().ok())
                .map(str::to_string)
        };
        if header("x-internal-service") != std::env::var("MNEMOSYNE_INTERNAL_SERVICE_SECRET").ok() {
            return (StatusCode::UNAUTHORIZED, "no secret").into_response();
        }
        let body = axum::body::to_bytes(body, 1 << 20)
            .await
            .unwrap_or_default();
        let rpc: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
        let id = rpc["id"].clone();
        let read_only = serde_json::json!({"readOnlyHint": true});
        let answer = match rpc["method"].as_str() {
            Some("tools/list") => {
                serde_json::json!({"jsonrpc": "2.0", "id": id, "result": {"tools": [
                    {"name": "toy_read", "description": "Read toy facts.", "annotations": read_only,
                     "inputSchema": {"type": "object", "required": ["graphId", "q"],
                                     "properties": {"graphId": {"type": "string"}, "q": {"type": "string"}}}},
                    {"name": "toy_write", "description": "Change toy facts.",
                     "inputSchema": {"type": "object", "properties": {}}},
                    {"name": "toy_slow", "description": "Take forever.", "annotations": read_only,
                     "inputSchema": {"type": "object", "properties": {}}},
                    {"name": "toy_fail", "description": "Fail loudly.", "annotations": read_only,
                     "inputSchema": {"type": "object", "properties": {}}},
                    {"name": "read_document", "description": "A shadow.", "annotations": read_only,
                     "inputSchema": {"type": "object", "properties": {}}},
                    {"name": "stray", "description": "Outside the prefix.", "annotations": read_only,
                     "inputSchema": {"type": "object", "properties": {}}}
                ]}})
            }
            Some("tools/call") => match rpc["params"]["name"].as_str() {
                Some("toy_slow") => {
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    serde_json::json!({"jsonrpc": "2.0", "id": id, "result": {"content": []}})
                }
                Some("toy_fail") => serde_json::json!({"jsonrpc": "2.0", "id": id,
                    "error": {"code": -32040, "message": "toy_fail exceeded its budget", "data": {"phase": "query"}}}),
                Some(name) => {
                    let seen = serde_json::json!({
                        "tool": name,
                        "arguments": rpc["params"]["arguments"],
                        "userId": header("x-user-id"),
                        "callerRole": header("x-sophia-caller-role"),
                        "graphId": header("x-graph-id"),
                        "graphOwner": header("x-graph-owner"),
                        "graphRole": header("x-sophia-graph-role"),
                        "authorization": header("authorization"),
                    });
                    serde_json::json!({"jsonrpc": "2.0", "id": id, "result": {
                        "content": [{"type": "text", "text": seen.to_string()}],
                        "structuredContent": seen}})
                }
                None => serde_json::json!({"jsonrpc": "2.0", "id": id,
                    "error": {"code": -32602, "message": "no tool"}}),
            },
            _ => serde_json::json!({"jsonrpc": "2.0", "id": id,
                "error": {"code": -32601, "message": "no such method"}}),
        };
        Json(answer).into_response()
    }

    async fn exit_soon() -> &'static str {
        thread::spawn(|| {
            thread::sleep(Duration::from_millis(50));
            std::process::exit(3);
        });
        "exiting"
    }

    /// Server-sent events: one at once, the second only after the release
    /// file appears, so a proxy that buffers whole responses never delivers
    /// the first in time.
    async fn events() -> Response {
        let release = std::env::var(RELEASE_ENV).ok();
        let stream = futures_util::stream::unfold(0_u8, move |step| {
            let release = release.clone();
            async move {
                match step {
                    0 => Some((Ok::<Bytes, std::io::Error>(Bytes::from("data: one\n\n")), 1)),
                    1 => {
                        let deadline = Instant::now() + Duration::from_secs(20);
                        while let Some(path) = &release {
                            if Path::new(path).exists() || Instant::now() > deadline {
                                break;
                            }
                            tokio::time::sleep(Duration::from_millis(20)).await;
                        }
                        Some((Ok(Bytes::from("data: two\n\n")), 2))
                    }
                    _ => None,
                }
            }
        });
        Response::builder()
            .header(header::CONTENT_TYPE, "text/event-stream")
            .header("access-control-allow-origin", "*")
            .body(Body::from_stream(stream))
            .unwrap()
    }

    /// Everything a test wants to see of a request and of this process.
    async fn echo(request: Request) -> Response {
        let (parts, body) = request.into_parts();
        let body = axum::body::to_bytes(body, 1 << 20)
            .await
            .unwrap_or_default();
        let header = |name: &str| {
            parts
                .headers
                .get(name)
                .and_then(|value| value.to_str().ok())
                .map(str::to_string)
        };
        let env = |name: &str| std::env::var(name).ok();
        let internal_ok = header("x-internal-service") == env("MNEMOSYNE_INTERNAL_SERVICE_SECRET");
        let garden_vars: Vec<String> = std::env::vars()
            .map(|(key, _)| key)
            .filter(|key| key.starts_with("GARDEN_"))
            .collect();
        let echoed = serde_json::json!({
            "method": parts.method.as_str(),
            "path": parts.uri.path(),
            "query": parts.uri.query(),
            "internalServiceOk": internal_ok,
            "userId": header("x-user-id"),
            "authorization": header("authorization"),
            "cookie": header("cookie"),
            "graphRole": header("x-sophia-graph-role"),
            "callerRole": header("x-sophia-caller-role"),
            "graphId": header("x-graph-id"),
            "graphOwner": header("x-graph-owner"),
            "humanDelegation": header("x-sophia-human-delegation"),
            "delegatedToken": header(DELEGATED_TOKEN_HEADER),
            "cfAccess": header("cf-access-jwt-assertion"),
            "callerKind": header("x-sophia-caller-kind"),
            "accountEmail": header("x-sophia-account-email"),
            "accountEmailVerified": header("x-sophia-account-email-verified"),
            "modelKey": header("x-model-key"),
            "visitorId": header("x-visitor-id"),
            "publicGraphs": header("x-sophia-public-graphs"),
            "writeFence": header("x-sophia-write-fence"),
            "visitorAccount": header("x-sophia-visitor-account"),
            "body": serde_json::from_slice::<serde_json::Value>(&body)
                .unwrap_or_else(|_| serde_json::Value::String(String::from_utf8_lossy(&body).into())),
            "env": {
                "PORT": env("PORT"),
                "DATA_DIR": env("DATA_DIR"),
                "MNEMOSYNE_MCP_URL": env("MNEMOSYNE_MCP_URL"),
                "MNEMOSYNE_MCP_ACCESS_TOKEN": env("MNEMOSYNE_MCP_ACCESS_TOKEN"),
                "OPENROUTER_API_KEY": env("OPENROUTER_API_KEY"),
                "SHARED_SETTING": env("SHARED_SETTING"),
                "GARDEN_SERVICE_HOST": env("GARDEN_SERVICE_HOST"),
                "CHOREOGRAPH_BIND_HOST": env("CHOREOGRAPH_BIND_HOST"),
                "CHOREOGRAPH_SANDBOX_MODE": env("CHOREOGRAPH_SANDBOX_MODE"),
                "FROM_MANIFEST": env("FROM_MANIFEST"),
                "GARDEN_LOOPBACK_API_URL": env("GARDEN_LOOPBACK_API_URL"),
                "GARDEN_SERVICE_ID": env("GARDEN_SERVICE_ID"),
                "gardenVars": garden_vars,
            },
            "pid": std::process::id(),
        });
        (StatusCode::OK, Json(echoed)).into_response()
    }
}

#[cfg(all(test, feature = "headless"))]
mod supervision_tests {
    use super::child::*;
    use super::*;

    /// Serializes on the crate's profile-env mutex (these tests set
    /// `GARDEN_PROFILE_DIR`) and hands the test a fresh root.
    fn with_profile(name: &str, test: impl FnOnce(&Path, &Path)) {
        let _serial = crate::tauri_runtime::profile_env_serial()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let root = std::env::temp_dir().join(format!("garden-svc-{name}-{}", Uuid::new_v4()));
        let profile = root.join("profile");
        std::fs::create_dir_all(&profile).unwrap();
        let previous = std::env::var_os("GARDEN_PROFILE_DIR");
        std::env::set_var("GARDEN_PROFILE_DIR", &profile);
        let outcome =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| test(&root, &profile)));
        match previous {
            Some(previous) => std::env::set_var("GARDEN_PROFILE_DIR", previous),
            None => std::env::remove_var("GARDEN_PROFILE_DIR"),
        }
        let _ = std::fs::remove_dir_all(&root);
        if let Err(error) = outcome {
            std::panic::resume_unwind(error);
        }
    }

    fn host_with(manifest: PathBuf) -> Arc<LocalServiceHost> {
        let host = Arc::new(LocalServiceHost::default());
        host.pin_manifest(CHOREOGRAPH_SERVICE_ID, manifest);
        host.set_timing(fast_timing());
        host.set_local_api_url("http://127.0.0.1:7999".into());
        host
    }

    fn heartbeat_age(path: &Path) -> Option<Duration> {
        let nanos: u128 = std::fs::read_to_string(path).ok()?.trim().parse().ok()?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        Some(Duration::from_nanos(now.saturating_sub(nanos) as u64))
    }

    fn assert_heartbeat_stops(path: &Path) {
        // Ending a group may take its whole stop grace (SIGTERM, then SIGKILL) and the
        // cells lane runs this beside 48 build jobs: 10 s timed out there twice
        // (ship-cells:62791d77 on 10-14, ship-cells:32549f45 on 10-15).
        wait_until(
            "the grandchild's heartbeat to stop",
            Duration::from_secs(30),
            || heartbeat_age(path).is_some_and(|age| age > Duration::from_millis(600)),
        );
        let before = std::fs::read_to_string(path).unwrap();
        thread::sleep(Duration::from_millis(400));
        assert_eq!(
            std::fs::read_to_string(path).unwrap(),
            before,
            "nothing in the stopped process group writes any more"
        );
    }

    /// `requires.node`: without the Node.js a service needs, gardend does not
    /// start it, says why (the person's words, no path) and does not retry;
    /// with it, the service starts. The PATH is the service's own (its
    /// manifest's here), as the child would get it.
    #[cfg(unix)]
    #[test]
    fn a_service_without_the_node_it_requires_is_unavailable_and_says_why() {
        with_profile("node", |root, _profile| {
            let bin = root.join("bin");
            std::fs::create_dir_all(&bin).unwrap();
            let manifest = write_service_manifest(
                &root.join("vehicle"),
                "vehicle",
                &[("PATH", bin.display().to_string())],
                serde_json::json!({ "requires": { "node": "22.19" }, "displayName": "Chat" }),
            );
            let host = Arc::new(LocalServiceHost::default());
            host.pin_manifest("vehicle", manifest);
            host.set_timing(fast_timing());
            host.set_local_api_url("http://127.0.0.1:7999".into());
            let app = crate::tauri_runtime::build_mock_app_for_tests(false);

            let error = host.start("vehicle", &app, &test_manifest()).unwrap_err();
            assert!(
                error.contains(&bin.display().to_string()),
                "the log names the PATH: {error}"
            );
            let status = host.status("vehicle", &app);
            assert_eq!(status.state, LocalServiceState::Unavailable);
            assert!(status.configured);
            assert!(status.pid.is_none());
            let reason = status.reason.clone().unwrap();
            assert_eq!(
                reason,
                "Chat needs Node.js 22.19 or newer, and there is no node on gardend's PATH. Install Node.js 22.19 or newer, then restart gardend."
            );
            assert_eq!(
                host.unavailable_reason("vehicle").as_deref(),
                Some(reason.as_str())
            );
            assert!(host.target("vehicle").is_err());
            let lines = autostart_local_services(&host, &app, &test_manifest(), false);
            assert!(
                lines
                    .iter()
                    .any(|line| line
                        .starts_with("vehicle: could not start: Chat needs Node.js 22.19")),
                "{lines:?}"
            );
            host.ensure_supervisor();
            thread::sleep(Duration::from_millis(800));
            let status = host.status("vehicle", &app);
            assert_eq!(
                status.state,
                LocalServiceState::Unavailable,
                "no retry loop"
            );
            assert_eq!(status.restarts, 0);

            put_node_on(&bin, "20.11.1");
            host.start("vehicle", &app, &test_manifest()).unwrap_err();
            let reason = host.status("vehicle", &app).reason.unwrap();
            assert!(
                reason.contains("the node on gardend's PATH is 20.11.1")
                    && !reason.contains(&bin.display().to_string()),
                "{reason}"
            );

            put_node_on(&bin, "22.19.0");
            let status = host.start("vehicle", &app, &test_manifest()).unwrap();
            assert_eq!(status.state, LocalServiceState::Running);
            assert!(status.reason.is_none());
            wait_service_healthy(&host, "vehicle");
            assert!(host.unavailable_reason("vehicle").is_none());
            host.shutdown();
        });
    }

    /// The child runs with the boundary environment: its own member-scope MCP
    /// token (never the master key nor any GARDEN_* setting of gardend's),
    /// the model key from the profile, the loopback bind host, the local API.
    #[test]
    fn a_started_service_gets_its_own_token_the_profile_key_and_a_loopback_bind() {
        with_profile("env", |root, profile| {
            let manifest = write_manifest(
                &root.join("svc"),
                &[
                    ("FROM_MANIFEST", "yes".into()),
                    ("OPENROUTER_API_KEY", "from-manifest".into()),
                ],
            );
            write_service_env_file(
                &shared_service_env_file(profile),
                "every service",
                &[
                    ("OPENROUTER_API_KEY".into(), "sk-or-shared".into()),
                    ("SHARED_SETTING".into(), "from-shared".into()),
                ],
            )
            .unwrap();
            write_service_env_file(
                &service_env_file(profile, CHOREOGRAPH_SERVICE_ID),
                "the choreograph service",
                &[("OPENROUTER_API_KEY".into(), "sk-or-profile".into())],
            )
            .unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = std::fs::metadata(service_env_file(profile, CHOREOGRAPH_SERVICE_ID))
                    .unwrap()
                    .permissions()
                    .mode();
                assert_eq!(mode & 0o777, 0o600);
            }
            std::env::set_var("GARDEN_TEST_SENTINEL_SECRET", "must-not-leak");
            let host = host_with(manifest);
            let app = crate::tauri_runtime::build_mock_app_for_tests(false);
            let status = host
                .start(CHOREOGRAPH_SERVICE_ID, &app, &test_manifest())
                .unwrap();
            std::env::remove_var("GARDEN_TEST_SENTINEL_SECRET");
            assert!(matches!(status.state, LocalServiceState::Running));
            assert!(status.configured);

            let target = wait_healthy(&host);
            assert!(target.base_url.starts_with("http://127.0.0.1:"));
            let echoed = get_json(&format!("{}/env", target.base_url)).unwrap();
            let env = &echoed["env"];
            let token = env["MNEMOSYNE_MCP_ACCESS_TOKEN"].as_str().unwrap();
            assert!(token.starts_with("sophia-svc-"), "{token}");
            assert_ne!(token, "master");
            assert_eq!(
                host.service_token_owner(token).as_deref(),
                Some(CHOREOGRAPH_SERVICE_ID)
            );
            assert_eq!(
                env["OPENROUTER_API_KEY"], "sk-or-profile",
                "the service's own file wins over the shared one and the manifest"
            );
            assert_eq!(env["SHARED_SETTING"], "from-shared");
            assert_eq!(env["GARDEN_SERVICE_HOST"], "127.0.0.1");
            assert_eq!(env["FROM_MANIFEST"], "yes");
            assert_eq!(env["CHOREOGRAPH_BIND_HOST"], "127.0.0.1");
            assert_eq!(env["CHOREOGRAPH_SANDBOX_MODE"], "local-native");
            assert_eq!(env["MNEMOSYNE_MCP_URL"], "http://127.0.0.1:7999/mcp");
            assert_eq!(env["GARDEN_LOOPBACK_API_URL"], "http://127.0.0.1:7999");
            assert_eq!(
                env["DATA_DIR"],
                profile.join("services/choreograph").display().to_string()
            );
            let garden_vars: Vec<&str> = env["gardenVars"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|value| value.as_str())
                .collect();
            for var in &garden_vars {
                assert!(
                    [
                        "GARDEN_LOOPBACK_API_URL",
                        "GARDEN_SERVICE_ID",
                        "GARDEN_PROFILE_ID",
                        "GARDEN_SERVICE_HOST"
                    ]
                    .contains(var),
                    "gardend's own {var} reached the child"
                );
            }
            assert!(supervisor_record_path(profile, CHOREOGRAPH_SERVICE_ID).is_file());

            host.stop(CHOREOGRAPH_SERVICE_ID, &app).unwrap();
            assert_eq!(
                host.service_token_owner(token),
                None,
                "the token dies with the child"
            );
            assert!(!supervisor_record_path(profile, CHOREOGRAPH_SERVICE_ID).exists());
            assert!(matches!(
                host.status(CHOREOGRAPH_SERVICE_ID, &app).state,
                LocalServiceState::Stopped
            ));
        });
    }

    /// Stopping a service, and gardend's exit, end its whole process group:
    /// a grandchild the service started stops too.
    #[test]
    fn stop_and_shutdown_end_the_whole_process_group() {
        with_profile("group", |root, _profile| {
            for via_shutdown in [false, true] {
                let heartbeat = root.join(format!("heartbeat-{via_shutdown}"));
                let manifest = write_manifest(
                    &root.join(format!("svc-{via_shutdown}")),
                    &[(HEARTBEAT_ENV, heartbeat.display().to_string())],
                );
                let host = host_with(manifest);
                let app = crate::tauri_runtime::build_mock_app_for_tests(false);
                host.start(CHOREOGRAPH_SERVICE_ID, &app, &test_manifest())
                    .unwrap();
                wait_healthy(&host);
                wait_until(
                    "the grandchild's heartbeat",
                    Duration::from_secs(20),
                    || {
                        heartbeat_age(&heartbeat)
                            .is_some_and(|age| age < Duration::from_millis(500))
                    },
                );
                if via_shutdown {
                    host.shutdown();
                    assert!(
                        host.start(CHOREOGRAPH_SERVICE_ID, &app, &test_manifest())
                            .is_err(),
                        "nothing starts once gardend is exiting"
                    );
                } else {
                    host.stop(CHOREOGRAPH_SERVICE_ID, &app).unwrap();
                }
                assert_heartbeat_stops(&heartbeat);
                assert!(host.target(CHOREOGRAPH_SERVICE_ID).is_err());
            }
        });
    }

    /// A crash is restarted after a backoff with a fresh process and token;
    /// a crash loop is given up after `max_restarts`.
    #[test]
    fn a_crash_is_restarted_and_a_crash_loop_is_given_up() {
        with_profile("restart", |root, _profile| {
            let host = host_with(write_manifest(&root.join("svc"), &[]));
            let app = crate::tauri_runtime::build_mock_app_for_tests(false);
            let first = host
                .start(CHOREOGRAPH_SERVICE_ID, &app, &test_manifest())
                .unwrap();
            let target = wait_healthy(&host);
            let first_token = get_json(&format!("{}/env", target.base_url)).unwrap()["env"]
                ["MNEMOSYNE_MCP_ACCESS_TOKEN"]
                .as_str()
                .unwrap()
                .to_string();
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async {
                let _ = reqwest::Client::new()
                    .post(format!("{}/exit", target.base_url))
                    .send()
                    .await;
            });
            let mut saw_restarting = false;
            wait_until("a restarted child", Duration::from_secs(30), || {
                let status = host.status(CHOREOGRAPH_SERVICE_ID, &app);
                saw_restarting |= status.state == LocalServiceState::Restarting;
                status.state == LocalServiceState::Running && status.pid != first.pid
            });
            let status = host.status(CHOREOGRAPH_SERVICE_ID, &app);
            assert_eq!(status.restarts, 1);
            assert!(saw_restarting || status.restarts == 1);
            let target = wait_healthy(&host);
            let second_token = get_json(&format!("{}/env", target.base_url)).unwrap()["env"]
                ["MNEMOSYNE_MCP_ACCESS_TOKEN"]
                .as_str()
                .unwrap()
                .to_string();
            assert_ne!(first_token, second_token);
            assert_eq!(host.service_token_owner(&first_token), None);
            host.stop(CHOREOGRAPH_SERVICE_ID, &app).unwrap();

            // A service that dies at once, every time.
            let crashing = host_with(write_manifest(
                &root.join("crashing"),
                &[(CRASH_ENV, "1".into())],
            ));
            crashing
                .start(CHOREOGRAPH_SERVICE_ID, &app, &test_manifest())
                .unwrap();
            wait_until("the supervisor to give up", Duration::from_secs(30), || {
                crashing.status(CHOREOGRAPH_SERVICE_ID, &app).state == LocalServiceState::Error
            });
            let status = crashing.status(CHOREOGRAPH_SERVICE_ID, &app);
            assert!(
                status
                    .last_error
                    .as_deref()
                    .unwrap()
                    .contains("gave up after 3 restarts"),
                "{:?}",
                status.last_error
            );
            thread::sleep(Duration::from_millis(800));
            assert_eq!(
                crashing.status(CHOREOGRAPH_SERVICE_ID, &app).state,
                LocalServiceState::Error,
                "nothing restarts it after giving up"
            );
        });
    }

    /// A service a previous gardend left running (gardend was killed) is
    /// ended before the next one starts; a recorded pid that now belongs to
    /// another process is left alone.
    #[test]
    fn an_orphan_from_a_previous_gardend_is_ended_and_a_recycled_pid_is_not() {
        with_profile("orphan", |root, profile| {
            let heartbeat = root.join("orphan-heartbeat");
            let mut orphan = Command::new(std::env::current_exe().unwrap());
            orphan
                .args(child_args())
                .env("SOPHIA_TEST_SERVICE_CHILD", "heartbeat")
                .env(HEARTBEAT_ENV, &heartbeat)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            place_child_in_new_process_group(&mut orphan);
            let mut orphan = orphan.spawn().unwrap();
            wait_until("the orphan's heartbeat", Duration::from_secs(20), || {
                heartbeat_age(&heartbeat).is_some_and(|age| age < Duration::from_millis(500))
            });
            let record_path = supervisor_record_path(profile, CHOREOGRAPH_SERVICE_ID);
            let record = SupervisorRecord {
                service_id: CHOREOGRAPH_SERVICE_ID.into(),
                pid: orphan.id(),
                start: process_start_signature(orphan.id()),
                gardend_pid: 999_999,
                started_at: timestamp(),
            };
            assert!(
                record.start.is_some(),
                "the start time of a live process is readable"
            );
            crate::storage::write_secret_json(&record_path, &record).unwrap();

            // The next gardend starts the service: the orphan goes first.
            let host = host_with(write_manifest(&root.join("svc"), &[]));
            let app = crate::tauri_runtime::build_mock_app_for_tests(false);
            host.start(CHOREOGRAPH_SERVICE_ID, &app, &test_manifest())
                .unwrap();
            assert_heartbeat_stops(&heartbeat);
            let _ = orphan.wait();
            host.stop(CHOREOGRAPH_SERVICE_ID, &app).unwrap();

            // A record naming a live process with another start time.
            let mut bystander = Command::new(std::env::current_exe().unwrap());
            let bystander_beat = root.join("bystander-heartbeat");
            bystander
                .args(child_args())
                .env("SOPHIA_TEST_SERVICE_CHILD", "heartbeat")
                .env(HEARTBEAT_ENV, &bystander_beat)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            place_child_in_new_process_group(&mut bystander);
            let mut bystander = bystander.spawn().unwrap();
            wait_until("the bystander's heartbeat", Duration::from_secs(20), || {
                heartbeat_age(&bystander_beat).is_some_and(|age| age < Duration::from_millis(500))
            });
            crate::storage::write_secret_json(
                &record_path,
                &SupervisorRecord {
                    start: Some("not when it started".into()),
                    pid: bystander.id(),
                    ..record
                },
            )
            .unwrap();
            let said = end_orphan(&record_path, &fast_timing()).unwrap();
            assert!(said.contains("left alone"), "{said}");
            assert!(!record_path.exists());
            thread::sleep(Duration::from_millis(300));
            assert!(
                heartbeat_age(&bystander_beat).is_some_and(|age| age < Duration::from_millis(500)),
                "the bystander still runs"
            );
            let _ = bystander.kill();
            let _ = bystander.wait();
        });
    }

    /// Autostart starts a configured Choreograph, never in a hosted cell, and
    /// does nothing when nothing is configured.
    #[test]
    fn autostart_starts_only_a_configured_service_and_never_in_a_cell() {
        with_profile("autostart", |root, _profile| {
            let app = crate::tauri_runtime::build_mock_app_for_tests(false);
            let unconfigured = Arc::new(LocalServiceHost::default());
            // Nothing pinned; the test binary's folder has no services/.
            if std::env::var_os("GARDEN_CHOREOGRAPH_MANIFEST").is_none()
                && std::env::var_os("GARDEN_CHOREOGRAPH_COMMAND").is_none()
            {
                assert!(
                    autostart_local_services(&unconfigured, &app, &test_manifest(), false)
                        .is_empty()
                );
                assert_eq!(
                    unconfigured.status(CHOREOGRAPH_SERVICE_ID, &app).state,
                    LocalServiceState::Unconfigured
                );
            }

            let host = host_with(write_manifest(&root.join("svc"), &[]));
            assert!(autostart_local_services(&host, &app, &test_manifest(), true).is_empty());
            assert_eq!(
                host.status(CHOREOGRAPH_SERVICE_ID, &app).state,
                LocalServiceState::Stopped,
                "a hosted cell never starts it"
            );
            let lines = autostart_local_services(&host, &app, &test_manifest(), false);
            assert_eq!(lines.len(), 1);
            assert!(lines[0].starts_with("choreograph: started"), "{lines:?}");
            wait_healthy(&host);
            host.shutdown();
        });
    }

    /// `gardend service enable|disable <id>`: the profile's choice decides
    /// whether a service starts with gardend, over its manifest's
    /// `autostart`, both ways; `GARDEN_<ID>_AUTOSTART=0` still keeps it off;
    /// status and `/self-host.json` say `enabled`.
    #[test]
    fn the_profile_enables_and_disables_a_service_at_boot_over_its_manifest() {
        with_profile("enable", |root, profile| {
            let dir = root.join("services");
            write_service_manifest(
                &dir.join("compute"),
                "compute",
                &[],
                serde_json::json!({ "autostart": false }),
            );
            write_service_manifest(&dir.join("chatty"), "chatty", &[], serde_json::json!({}));
            let host = Arc::new(LocalServiceHost::default());
            host.pin_services_dir(dir.clone());
            host.set_timing(fast_timing());
            host.set_local_api_url("http://127.0.0.1:7999".into());
            let app = crate::tauri_runtime::build_mock_app_for_tests(false);

            // The manifests' defaults: compute off, chatty on.
            assert_eq!(host.status("compute", &app).enabled, Some(false));
            assert_eq!(host.status("chatty", &app).enabled, Some(true));
            assert_eq!(host.status("absent", &app).enabled, None);

            // The person turns them around for this profile.
            let on = write_service_autostart(profile, "compute", true).unwrap();
            write_service_autostart(profile, "chatty", false).unwrap();
            assert_eq!(std::fs::read_to_string(&on).unwrap(), "on\n");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                assert_eq!(
                    std::fs::metadata(&on).unwrap().permissions().mode() & 0o777,
                    0o600
                );
            }
            assert!(write_service_autostart(profile, "../etc", true).is_err());
            assert_eq!(
                read_service_autostart(profile, "compute").unwrap(),
                Some(true)
            );
            assert_eq!(
                read_service_autostart(profile, "chatty").unwrap(),
                Some(false)
            );
            assert_eq!(read_service_autostart(profile, "absent").unwrap(), None);
            assert_eq!(host.status("compute", &app).enabled, Some(true));
            assert_eq!(host.status("chatty", &app).enabled, Some(false));

            let lines = autostart_local_services(&host, &app, &test_manifest(), false);
            assert!(
                lines
                    .iter()
                    .any(|line| line.starts_with("compute: started")),
                "{lines:?}"
            );
            assert!(
                lines
                    .iter()
                    .any(|line| line.starts_with("chatty: configured, not started")
                        && line.contains("gardend service enable chatty")),
                "{lines:?}"
            );
            wait_service_healthy(&host, "compute");
            assert_eq!(
                host.status("chatty", &app).state,
                LocalServiceState::Stopped
            );
            host.stop("compute", &app).unwrap();

            // gardend's environment keeps a service off whatever the profile says.
            std::env::set_var("GARDEN_COMPUTE_AUTOSTART", "0");
            let off = service_autostarts("compute", false, Some(profile));
            let lines = autostart_local_services(&host, &app, &test_manifest(), false);
            std::env::remove_var("GARDEN_COMPUTE_AUTOSTART");
            assert!(!off);
            assert!(
                lines
                    .iter()
                    .any(|line| line.starts_with("compute: configured, not started")),
                "{lines:?}"
            );
            assert_eq!(
                host.status("compute", &app).state,
                LocalServiceState::Stopped
            );

            // A file that says neither on nor off is not a choice: the manifest decides.
            std::fs::write(service_autostart_file(profile, "compute"), "maybe\n").unwrap();
            assert!(read_service_autostart(profile, "compute").is_err());
            assert!(!service_autostarts("compute", false, Some(profile)));
            assert!(service_autostarts("compute", true, Some(profile)));
            assert!(service_autostarts("compute", true, None));
            host.shutdown();
        });
    }

    /// More than one service, from a services folder (`GARDEN_SERVICES_DIR`
    /// or `services/` beside the binary): each is its own `<id>`, autostarts
    /// unless its manifest says otherwise, gets its own token and the shared
    /// settings, and declares what members may reach.
    #[test]
    fn a_services_folder_holds_several_services_each_supervised_on_its_own() {
        with_profile("services-dir", |root, profile| {
            let dir = root.join("services");
            write_service_manifest(
                &dir.join("vehicle"),
                "vehicle",
                &[],
                serde_json::json!({ "memberAccess": "all" }),
            );
            write_service_manifest(
                &dir.join("quiet"),
                "quiet",
                &[],
                serde_json::json!({ "autostart": false }),
            );
            std::fs::create_dir_all(dir.join("Not-An-Id")).unwrap();
            std::fs::write(dir.join("Not-An-Id/service.json"), b"{}").unwrap();
            std::fs::create_dir_all(dir.join("empty")).unwrap();
            write_service_env_file(
                &shared_service_env_file(profile),
                "every service",
                &[("SHARED_SETTING".into(), "for-everyone".into())],
            )
            .unwrap();

            let host = Arc::new(LocalServiceHost::default());
            host.pin_services_dir(dir.clone());
            host.set_timing(fast_timing());
            host.set_local_api_url("http://127.0.0.1:7999".into());
            let app = crate::tauri_runtime::build_mock_app_for_tests(false);
            let ids = host.service_ids(&app);
            assert!(ids.contains("vehicle") && ids.contains("quiet"), "{ids:?}");
            assert!(
                !ids.contains("Not-An-Id") && !ids.contains("empty"),
                "{ids:?}"
            );
            assert_eq!(host.member_access("vehicle", &app), MemberAccess::All);
            assert_eq!(host.member_access("quiet", &app), MemberAccess::None);
            assert_eq!(host.member_access("absent", &app), MemberAccess::None);
            assert_eq!(
                host.status("absent", &app).state,
                LocalServiceState::Unconfigured
            );
            assert!(host.start("absent", &app, &test_manifest()).is_err());
            assert!(host.start("../etc", &app, &test_manifest()).is_err());

            let lines = autostart_local_services(&host, &app, &test_manifest(), false);
            assert!(
                lines
                    .iter()
                    .any(|line| line.starts_with("vehicle: started")),
                "{lines:?}"
            );
            assert!(
                lines
                    .iter()
                    .any(|line| line.starts_with("quiet: configured, not started")),
                "{lines:?}"
            );
            assert_eq!(host.status("quiet", &app).state, LocalServiceState::Stopped);
            let target = wait_service_healthy(&host, "vehicle");
            let env = &get_json(&format!("{}/env", target.base_url)).unwrap()["env"];
            assert_eq!(env["GARDEN_SERVICE_ID"], "vehicle");
            assert_eq!(env["GARDEN_SERVICE_HOST"], "127.0.0.1");
            assert_eq!(env["SHARED_SETTING"], "for-everyone");
            assert!(
                env["CHOREOGRAPH_BIND_HOST"].is_null(),
                "Choreograph's knobs stay Choreograph's"
            );
            assert_eq!(
                env["DATA_DIR"],
                profile.join("services/vehicle").display().to_string()
            );
            let token = env["MNEMOSYNE_MCP_ACCESS_TOKEN"].as_str().unwrap();
            assert_eq!(host.service_token_owner(token).as_deref(), Some("vehicle"));

            // A second service runs beside it, with a token of its own.
            host.start("quiet", &app, &test_manifest()).unwrap();
            let quiet = wait_service_healthy(&host, "quiet");
            assert_ne!(quiet.base_url, target.base_url);
            let quiet_token = get_json(&format!("{}/env", quiet.base_url)).unwrap()["env"]
                ["MNEMOSYNE_MCP_ACCESS_TOKEN"]
                .as_str()
                .unwrap()
                .to_string();
            assert_ne!(quiet_token, token);
            assert_eq!(
                host.service_token_owner(&quiet_token).as_deref(),
                Some("quiet")
            );
            host.shutdown();
            assert!(host.target("vehicle").is_err() && host.target("quiet").is_err());
        });
    }

    #[test]
    fn service_ids_come_from_manifest_and_command_variables_and_are_checked() {
        let vars = [
            ("GARDEN_VEHICLE_MANIFEST", "/opt/vehicle/service.json"),
            ("GARDEN_KG_ULTRA_COMMAND", "python"),
            ("GARDEN_CHOREOGRAPH_MANIFEST", " "),
            ("GARDEN_UI_DIR", "/opt/ui"),
            ("GARDEN_CHOREOGRAPH_AUTOSTART", "0"),
            ("OTHER_MANIFEST", "/x"),
        ]
        .map(|(name, value)| (name.to_string(), value.to_string()));
        let ids = discover_service_ids(vars, &[]);
        assert_eq!(
            ids.into_iter().collect::<Vec<_>>(),
            vec!["kg-ultra".to_string(), "vehicle".to_string()]
        );
        for good in ["choreograph", "kg-ultra", "vehicle", "v2"] {
            assert!(is_service_id(good), "{good}");
        }
        for bad in ["", "-x", "Vehicle", "a/b", "..", "a_b", &"x".repeat(64)] {
            assert!(!is_service_id(bad), "{bad}");
        }
        assert_eq!(
            services_dirs_from(Some(" /srv/services "), Some(Path::new("/opt/gardend"))),
            vec![
                PathBuf::from("/srv/services"),
                PathBuf::from("/opt/gardend/services")
            ]
        );
        assert!(
            gardend_owned_env("PORT")
                && gardend_owned_env("GARDEN_X")
                && gardend_owned_env("MNEMOSYNE_MCP_URL")
        );
        assert!(!gardend_owned_env("OPENROUTER_API_KEY"));
    }
}
