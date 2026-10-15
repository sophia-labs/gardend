//! Tools a supervised local service lends to gardend's own `/mcp`.
//!
//! A service whose `service.json` carries
//!
//! ```json
//! "mcp": { "path": "/mcp", "graphs": ["uruguay-cows"], "toolPrefix": "cow_", "timeoutMs": 30000 }
//! ```
//!
//! has its MCP endpoint's tools (those named `toolPrefix…`) listed in
//! gardend's `tools/list` beside gardend's own, and their calls forwarded to
//! it, so an agent (and a chat acting for a person) sees one surface. gardend
//! stays the door:
//!
//! - **Who may call.** A tool the service marks read-only
//!   (`annotations.readOnlyHint: true`) needs `graphs.read`, so a viewer may
//!   call it; any other tool needs `services.manage` (an admin), or
//!   `services.proxy` (a member) when the manifest says `"memberAccess":
//!   "all"`. `tools/list` shows a caller only the lent tools it may call.
//! - **Which graph.** The tools are about the graphs the manifest names.
//!   Each listed tool takes `graph_id` (one of them; filled in when there is
//!   only one); a token bound to one graph sees the tools only when that
//!   graph is one of them, without the argument. The call reaches the service
//!   as the proxy's requests do: without the graph argument, with the headers
//!   gardend mints (`x-internal-service`, `x-user-id`, `x-sophia-caller-role`,
//!   `x-graph-id`, `x-graph-owner`, `x-sophia-graph-role`) and nothing the
//!   caller sent.
//! - **How long.** gardend waits `timeoutMs` (30 s by default, 120 s at most)
//!   and then answers with an error that says so (`data.code:
//!   "service_tool_timeout"`), closing the request; the service should stop
//!   its own work by then (a well-behaved one cancels on its own budget).
//! - **Never shadowing.** A lent name that is one of gardend's own tools, or
//!   lacks the prefix, is dropped (and logged).
//!
//! Inert unless declared: with no service declaring `mcp`, `/mcp` is exactly
//! what it was. Never in a hosted cell (its services are denied).

use crate::{
    local_service_host::{Delegation, MemberAccess, ServiceMcp},
    loopback_state::LoopbackState,
    mcp_rpc_protocol::{mcp_error, mcp_error_with_data, McpRpcResponse},
};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// How long a service's tool list is reused before gardend asks again (a
/// restart of the service asks at once).
const TOOL_LIST_TTL: Duration = Duration::from_secs(60);
/// How long gardend waits for a service's `tools/list`.
const TOOL_LIST_TIMEOUT: Duration = Duration::from_secs(5);
const GRAPH_ARGUMENTS: [&str; 2] = ["graph_id", "graphId"];
const GRAPH_OWNER: &str = "user:owner";
pub(crate) const SERVICE_TOOL_TIMEOUT_CODE: &str = "service_tool_timeout";
pub(crate) const SERVICE_TOOL_UNAVAILABLE_CODE: &str = "service_tool_unavailable";

/// One lent tool, as gardend lists and calls it.
#[derive(Debug, Clone)]
struct LentTool {
    service_id: String,
    mcp: ServiceMcp,
    /// The tool as the service listed it.
    tool: Value,
    read_only: bool,
    member_access: MemberAccess,
}

impl LentTool {
    fn name(&self) -> &str {
        self.tool["name"].as_str().unwrap_or_default()
    }

    fn required_scopes(&self) -> Vec<&'static str> {
        if self.read_only {
            vec!["graphs.read"]
        } else if self.member_access == MemberAccess::All {
            vec!["services.proxy"]
        } else {
            vec!["services.manage"]
        }
    }

    fn callable_with(&self, scopes: &[String]) -> bool {
        self.required_scopes()
            .iter()
            .all(|required| scopes.iter().any(|held| held == required))
    }

    /// The tool as gardend lists it: the service's description and schema,
    /// the graph argument gardend's own (an enum of `graphs`, the declared
    /// graphs this caller may name; none when the caller is bound to one),
    /// and where it comes from in `_meta`.
    fn listed(&self, bound: Option<&Delegation>, graphs: &[String]) -> Value {
        let mut tool = self.tool.clone();
        let schema = tool
            .get_mut("inputSchema")
            .filter(|schema| schema.is_object())
            .cloned()
            .unwrap_or_else(|| json!({"type": "object", "properties": {}}));
        let mut schema = schema;
        if schema.get("type").is_none() {
            schema["type"] = json!("object");
        }
        if !schema.get("properties").is_some_and(Value::is_object) {
            schema["properties"] = json!({});
        }
        let properties = schema["properties"].as_object_mut().expect("properties object");
        for name in GRAPH_ARGUMENTS {
            properties.remove(name);
        }
        if let Some(required) = schema.get_mut("required").and_then(Value::as_array_mut) {
            required.retain(|name| !name.as_str().is_some_and(|name| GRAPH_ARGUMENTS.contains(&name)));
        }
        if bound.is_none() {
            let description = if graphs.len() == 1 {
                format!("The graph this tool is about: {} (the default).", graphs[0])
            } else {
                format!("The graph this tool is about: one of {}.", graphs.join(", "))
            };
            schema["properties"]["graph_id"] = json!({
                "type": "string",
                "enum": graphs,
                "description": description,
            });
            if graphs.len() > 1 {
                let required = schema
                    .as_object_mut()
                    .expect("schema object")
                    .entry("required")
                    .or_insert_with(|| json!([]));
                if let Some(required) = required.as_array_mut() {
                    required.push(json!("graph_id"));
                }
            }
        }
        tool["inputSchema"] = schema;
        let mut meta = tool.get("_meta").cloned().filter(Value::is_object).unwrap_or_else(|| json!({}));
        meta["sophia.service"] = json!(self.service_id);
        meta["sophia.serviceGraphs"] = json!(graphs);
        meta["sophia.local.requiredScopes"] = json!(self.required_scopes());
        meta["sophia.local.scopeMode"] = json!(crate::loopback_scopes::TOKEN_SCOPE_MODE);
        meta["sophia.local.requiredScopeMode"] = json!("all");
        tool["_meta"] = meta;
        tool
    }
}

/// What one service's endpoint listed, for the process that said it.
struct CachedList {
    process: (u32, String),
    fetched: Instant,
    tools: Vec<Value>,
}

fn cache() -> &'static Mutex<BTreeMap<String, CachedList>> {
    static CACHE: OnceLock<Mutex<BTreeMap<String, CachedList>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(BTreeMap::new()))
}

fn client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|error| format!("build local service client: {error}"))
}

/// Every tool the declared, running services lend now. A service that is not
/// running, or whose endpoint does not answer, lends nothing (and says why in
/// the log); gardend's own tools are listed regardless.
async fn lent_tools(state: &LoopbackState) -> Vec<LentTool> {
    if state.cell_graph.is_enabled() {
        return Vec::new();
    }
    let declarations = state.services.mcp_declarations(&state.app);
    let mut lent = Vec::new();
    for (service_id, mcp) in declarations {
        let Some(process) = state.services.process_identity(&service_id) else {
            continue;
        };
        let cached = {
            let cache = cache().lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            cache
                .get(&service_id)
                .filter(|entry| entry.process == process && entry.fetched.elapsed() < TOOL_LIST_TTL)
                .map(|entry| entry.tools.clone())
        };
        let tools = match cached {
            Some(tools) => tools,
            None => match fetch_tool_list(state, &service_id, &mcp).await {
                Ok(tools) => {
                    cache()
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .insert(
                            service_id.clone(),
                            CachedList { process: process.clone(), fetched: Instant::now(), tools: tools.clone() },
                        );
                    tools
                }
                Err(error) => {
                    log::warn!("service {service_id} lends no MCP tools now: {error}");
                    continue;
                }
            },
        };
        let member_access = state.services.member_access(&service_id, &state.app);
        for tool in tools {
            let Some(name) = tool.get("name").and_then(Value::as_str) else {
                continue;
            };
            if !name.starts_with(&mcp.tool_prefix) || name.len() == mcp.tool_prefix.len() {
                log::warn!(
                    "service {service_id} lists MCP tool {name:?} outside its prefix {:?}; not lent",
                    mcp.tool_prefix
                );
                continue;
            }
            if crate::mcp_tool_registry::is_known_mcp_tool_name(name) {
                log::warn!("service {service_id} lists {name:?}, one of gardend's own tools; not lent");
                continue;
            }
            let read_only = tool
                .pointer("/annotations/readOnlyHint")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            lent.push(LentTool {
                service_id: service_id.clone(),
                mcp: mcp.clone(),
                tool,
                read_only,
                member_access,
            });
        }
    }
    lent
}

async fn fetch_tool_list(state: &LoopbackState, service_id: &str, mcp: &ServiceMcp) -> Result<Vec<Value>, String> {
    let target = state.services.target(service_id)?;
    let response = client()?
        .post(format!("{}{}", target.base_url, mcp.path))
        .header("x-internal-service", &target.internal_secret)
        .header("x-user-id", "gardend")
        .header("x-sophia-caller-role", "viewer")
        .json(&json!({"jsonrpc": "2.0", "id": "gardend-tools-list", "method": "tools/list", "params": {}}))
        .timeout(TOOL_LIST_TIMEOUT)
        .send()
        .await
        .map_err(|error| format!("tools/list: {error}"))?;
    let status = response.status();
    let body: Value = response
        .json()
        .await
        .map_err(|error| format!("tools/list answered HTTP {status} without JSON: {error}"))?;
    if let Some(error) = body.get("error") {
        return Err(format!("tools/list failed: {error}"));
    }
    body.pointer("/result/tools")
        .and_then(Value::as_array)
        .cloned()
        .ok_or_else(|| format!("tools/list answered HTTP {status} without result.tools"))
}

/// The lent tools a caller may call, as `tools/list` shows them: nothing in a
/// hosted cell or when no service lends any.
pub(crate) async fn listed_service_tools(state: &LoopbackState, caller: &ServiceCaller<'_>) -> Vec<Value> {
    lent_tools(state)
        .await
        .into_iter()
        .filter(|tool| tool.callable_with(caller.scopes))
        .filter_map(|tool| {
            let graphs = caller.graphs_for(state, &tool)?;
            Some(tool.listed(caller.bound, &graphs))
        })
        .collect()
}

/// Who calls, for the headers gardend mints.
pub(crate) struct ServiceCaller<'a> {
    pub(crate) scopes: &'a [String],
    pub(crate) user_id: Option<&'a str>,
    pub(crate) bound: Option<&'a Delegation>,
    /// `Some(home)`: a public-mode visitor or account (`public_mode`), or a
    /// service acting for one (`home`: its session's graph).
    pub(crate) public: Option<Option<&'a str>>,
}

impl ServiceCaller<'_> {
    /// The declared graphs of `tool` this caller may name: all of them for
    /// an ordinary caller; the bound one for a delegation; for a public caller
    /// the public ones, and only from a service public mode lists and a tool
    /// marked read-only. `None`: the tool is not this caller's at all.
    fn graphs_for(&self, state: &LoopbackState, tool: &LentTool) -> Option<Vec<String>> {
        if let Some(delegation) = self.bound {
            return tool
                .mcp
                .graphs
                .contains(&delegation.graph_id)
                .then(|| vec![delegation.graph_id.clone()]);
        }
        if self.public.is_some() {
            let public = state.self_host.public()?;
            if !tool.read_only || !public.is_public_service(&tool.service_id) {
                return None;
            }
            let graphs: Vec<String> = tool
                .mcp
                .graphs
                .iter()
                .filter(|graph| public.is_public_graph(graph))
                .cloned()
                .collect();
            return (!graphs.is_empty()).then_some(graphs);
        }
        Some(tool.mcp.graphs.clone())
    }
}

/// A `tools/call` of a lent tool, answered here; `None` when `params.name`
/// is not one (gardend's own dispatch takes it).
pub(crate) async fn call_service_tool(
    state: &LoopbackState,
    id: Option<Value>,
    params: &Value,
    caller: &ServiceCaller<'_>,
) -> Option<McpRpcResponse> {
    if state.cell_graph.is_enabled() {
        return None;
    }
    let name = params.get("name").and_then(Value::as_str)?;
    if crate::mcp_tool_registry::is_known_mcp_tool_name(name) {
        return None;
    }
    let declarations = state.services.mcp_declarations(&state.app);
    let (service_id, mcp) = declarations
        .into_iter()
        .find(|(_, mcp)| name.starts_with(&mcp.tool_prefix))?;
    let tool = lent_tools(state)
        .await
        .into_iter()
        .find(|tool| tool.service_id == service_id && tool.name() == name);
    let Some(tool) = tool else {
        let message = match state.services.target(&service_id) {
            Err(why) => format!(
                "{name} is lent by the service {service_id}, which is unavailable ({why}); \
                 its tools return when it runs"
            ),
            Ok(_) => format!("unknown tool {name}: the service {service_id} does not list it"),
        };
        return Some(mcp_error_with_data(
            id,
            -32602,
            message,
            Some(json!({"code": SERVICE_TOOL_UNAVAILABLE_CODE, "service": service_id})),
        ));
    };
    if !caller.scopes.iter().any(|scope| scope == "mcp.tools.call") {
        return Some(mcp_error(id, -32003, "MCP token missing required scope mcp.tools.call"));
    }
    if let Some(missing) = tool
        .required_scopes()
        .into_iter()
        .find(|required| !caller.scopes.iter().any(|held| held == required))
    {
        return Some(mcp_error(
            id,
            -32003,
            format!("{name} needs scope {missing}: {}", if tool.read_only {
                "it reads the service's data"
            } else {
                "it is not marked read-only, so only an admin may call it"
            }),
        ));
    }
    let mut arguments = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
    if !arguments.is_object() {
        return Some(mcp_error(id, -32602, format!("{name}: arguments must be an object")));
    }
    let Some(allowed) = caller.graphs_for(state, &tool) else {
        // A public caller and a tool that is not public (or a delegation
        // bound to another graph): not this caller's tool.
        return Some(mcp_error(
            id,
            -32004,
            match caller.bound {
                Some(delegation) => format!(
                    "this token is bound to the graph \"{}\"; {name} is about {}",
                    delegation.graph_id,
                    mcp.graphs.join(", ")
                ),
                None => format!("{name} is not open to public callers"),
            },
        ));
    };
    let named = GRAPH_ARGUMENTS
        .iter()
        .find_map(|key| arguments.get(*key).and_then(Value::as_str).map(str::to_string));
    let home = caller.public.flatten().filter(|home| allowed.iter().any(|graph| graph == home));
    let graph_id = match (named, home) {
        (Some(named), _) => named,
        (None, Some(home)) => home.to_string(),
        (None, None) if allowed.len() == 1 => allowed[0].clone(),
        (None, None) => {
            return Some(mcp_error(
                id,
                -32602,
                format!("{name} needs graph_id: one of {}", allowed.join(", ")),
            ));
        }
    };
    if !allowed.contains(&graph_id) {
        return Some(mcp_error(
            id,
            if caller.bound.is_some() || caller.public.is_some() { -32004 } else { -32602 },
            match caller.bound {
                Some(delegation) => format!(
                    "this token is bound to the graph \"{}\"; the call names another graph",
                    delegation.graph_id
                ),
                None => format!(
                    "{name} is about the graph{} {}; this call names {graph_id}",
                    if allowed.len() == 1 { "" } else { "s" },
                    allowed.join(", ")
                ),
            },
        ));
    }
    if let Some(object) = arguments.as_object_mut() {
        for key in GRAPH_ARGUMENTS {
            object.remove(key);
        }
    }
    let role = match caller.bound {
        Some(delegation) => delegation.role.clone(),
        None => caller_role(caller.scopes).to_string(),
    };
    let user_id = caller
        .bound
        .map(|delegation| delegation.user_id.clone())
        .or_else(|| caller.user_id.map(str::to_string))
        .unwrap_or_else(|| "anonymous".to_string());
    Some(forward(state, id, &service_id, &mcp, name, arguments, &graph_id, &role, &user_id).await)
}

/// The caller's role, as the proxy mints it: an admin is the owner, a member
/// (who may write documents) an editor, anyone else a viewer.
fn caller_role(scopes: &[String]) -> &'static str {
    let holds = |scope: &str| scopes.iter().any(|held| held == scope);
    if holds("services.manage") {
        "owner"
    } else if holds("documents.write.crdt") {
        "editor"
    } else {
        "viewer"
    }
}

#[allow(clippy::too_many_arguments)]
async fn forward(
    state: &LoopbackState,
    id: Option<Value>,
    service_id: &str,
    mcp: &ServiceMcp,
    name: &str,
    arguments: Value,
    graph_id: &str,
    role: &str,
    user_id: &str,
) -> McpRpcResponse {
    let target = match state.services.target(service_id) {
        Ok(target) => target,
        Err(why) => {
            return mcp_error_with_data(
                id,
                -32000,
                format!("{name}: service {service_id} {why}"),
                Some(json!({"code": SERVICE_TOOL_UNAVAILABLE_CODE, "service": service_id})),
            )
        }
    };
    let client = match client() {
        Ok(client) => client,
        Err(error) => return mcp_error(id, -32000, error),
    };
    let request = client
        .post(format!("{}{}", target.base_url, mcp.path))
        .header("x-internal-service", &target.internal_secret)
        .header("x-user-id", user_id)
        .header("x-sophia-caller-role", role)
        .header("x-graph-id", graph_id)
        .header("x-graph-owner", GRAPH_OWNER)
        .header("x-sophia-graph-role", role)
        .json(&json!({
            "jsonrpc": "2.0",
            "id": id.clone().unwrap_or(Value::Null),
            "method": "tools/call",
            "params": { "name": name, "arguments": arguments },
        }))
        .send();
    let started = Instant::now();
    let response = match tokio::time::timeout(mcp.timeout, async {
        let response = request.await?;
        let status = response.status();
        let body = response.bytes().await?;
        Ok::<_, reqwest::Error>((status, body))
    })
    .await
    {
        Err(_) => {
            return mcp_error_with_data(
                id,
                -32000,
                format!(
                    "{name} did not answer within {} ms (service {service_id}); gardend stopped \
                     waiting and closed the request. Narrow the question and ask again.",
                    mcp.timeout.as_millis()
                ),
                Some(json!({
                    "code": SERVICE_TOOL_TIMEOUT_CODE,
                    "service": service_id,
                    "timeoutMs": mcp.timeout.as_millis() as u64,
                })),
            )
        }
        Ok(Err(error)) => {
            return mcp_error_with_data(
                id,
                -32000,
                format!("{name}: service {service_id} failed to answer: {error}"),
                Some(json!({"code": SERVICE_TOOL_UNAVAILABLE_CODE, "service": service_id})),
            )
        }
        Ok(Ok(answer)) => answer,
    };
    let (status, body) = response;
    let parsed: Value = match serde_json::from_slice(&body) {
        Ok(value) => value,
        Err(_) => {
            return mcp_error(
                id,
                -32000,
                format!(
                    "{name}: service {service_id} answered HTTP {status} with no JSON-RPC: {}",
                    String::from_utf8_lossy(&body[..body.len().min(300)])
                ),
            )
        }
    };
    log::debug!(
        "service tool {name} ({service_id}) answered HTTP {status} in {:?}",
        started.elapsed()
    );
    if let Some(error) = parsed.get("error") {
        let code = error.get("code").and_then(Value::as_i64).unwrap_or(-32000);
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("the service's tool failed")
            .to_string();
        let data = error.get("data").cloned().filter(|data| !data.is_null());
        return mcp_error_with_data(id, code, message, data);
    }
    match parsed.get("result").cloned() {
        Some(result) => crate::mcp_rpc_protocol::mcp_success(id, result),
        None => mcp_error(
            id,
            -32000,
            format!("{name}: service {service_id} answered HTTP {status} with neither result nor error"),
        ),
    }
}

#[cfg(test)]
pub(crate) fn clear_tool_list_cache() {
    cache().lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clear();
}

// Named so the cells lane's gating filter (`local_service_host`) runs them.
#[cfg(all(test, feature = "headless"))]
mod local_service_host_manifest_tests {
    use crate::local_service_host::child::write_service_manifest;

    fn read(extra: serde_json::Value) -> Result<(), String> {
        let dir = std::env::temp_dir().join(format!("svc-mcp-manifest-{}", uuid::Uuid::new_v4()));
        let path = write_service_manifest(&dir, "toys", &[], extra);
        let host = crate::local_service_host::LocalServiceHost::default();
        host.pin_manifest("toys", path);
        let app = crate::tauri_runtime::build_mock_app_for_tests(false);
        let outcome = if host.configured("toys", &app) {
            Ok(())
        } else {
            Err(host.status("toys", &app).last_error.unwrap_or_else(|| "unconfigured".into()))
        };
        let _ = std::fs::remove_dir_all(&dir);
        outcome
    }

    /// A malformed `mcp` leaves the service unconfigured rather than lending
    /// tools under a prefix that could shadow gardend's own.
    #[test]
    fn a_manifest_lends_tools_only_under_a_sound_declaration() {
        let sound = serde_json::json!({"mcp": {"graphs": ["toy-graph"], "toolPrefix": "toy_"}});
        assert_eq!(read(sound), Ok(()));
        for broken in [
            serde_json::json!({"mcp": {"graphs": [], "toolPrefix": "toy_"}}),
            serde_json::json!({"mcp": {"graphs": ["toy-graph"], "toolPrefix": ""}}),
            serde_json::json!({"mcp": {"graphs": ["toy-graph"], "toolPrefix": "read"}}),
            serde_json::json!({"mcp": {"graphs": ["toy-graph"], "toolPrefix": "Toy_"}}),
            serde_json::json!({"mcp": {"graphs": ["../x"], "toolPrefix": "toy_"}}),
            serde_json::json!({"mcp": {"graphs": ["toy-graph"], "toolPrefix": "toy_", "timeoutMs": 999999}}),
            serde_json::json!({"mcp": {"graphs": ["toy-graph"], "toolPrefix": "toy_", "path": "mcp"}}),
            serde_json::json!({"mcp": {"graphs": ["toy-graph"], "toolPrefix": "toy_", "tools": ["x"]}}),
        ] {
            assert!(read(broken.clone()).is_err(), "{broken}");
        }
    }
}

/// End to end: the real router's `/mcp` in front of a real child process
/// (the test binary re-run as a tiny MCP service, `local_service_host::child`).
/// Named so the cells lane's gating filter (`local_service_host`) runs them.
#[cfg(all(test, feature = "headless"))]
mod local_service_host_lending_tests {
    use super::*;
    use crate::cell_graph_boundary::CellGraphBoundary;
    use crate::local_service_host::child::{
        fast_timing, wait_service_healthy, write_service_manifest, MCP_ENV,
    };
    use crate::local_service_host::LocalServiceHost;
    use crate::loopback_client_token_service::create_client_token;
    use crate::loopback_client_token_types::CreateLoopbackClientTokenInput;
    use crate::loopback_self_host::SelfHost;
    use crate::loopback_state::LoopbackManifest;
    use axum::body::Body;
    use std::path::Path;
    use std::sync::Arc;
    use tower::ServiceExt;

    const MASTER: &str = "service-mcp-test-master";
    const SERVICE: &str = "toys";
    const GRAPH: &str = "toy-graph";

    fn state(app: crate::app_runtime::AppHandle, dir: &Path, cell_graph: CellGraphBoundary) -> Arc<LoopbackState> {
        state_with(app, dir, cell_graph, SelfHost::default())
    }

    fn state_with(
        app: crate::app_runtime::AppHandle,
        dir: &Path,
        cell_graph: CellGraphBoundary,
        self_host: SelfHost,
    ) -> Arc<LoopbackState> {
        let grant = crate::loopback_token_grants::session_all_token_grant();
        Arc::new(LoopbackState {
            app,
            token: MASTER.into(),
            manifest: LoopbackManifest {
                runtime_profile: "test",
                bind_host: "127.0.0.1",
                port: 0,
                api_url: "https://sophia.example.com".into(),
                mcp_url: "https://sophia.example.com/mcp".into(),
                openapi_url: String::new(),
                token: MASTER.into(),
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
            },
            jobs: Arc::new(crate::local_jobs::LocalJobRegistry::new(dir.join("jobs")).unwrap()),
            services: Arc::new(LocalServiceHost::default()),
            lifecycle: Arc::new(crate::cell_lifecycle::CellLifecycle::new()),
            cell_graph: Arc::new(cell_graph),
            self_host: Arc::new(self_host),
        })
    }

    fn start_toys(state: &Arc<LoopbackState>, root: &Path, extra: Value) {
        state.services.pin_manifest(
            SERVICE,
            write_service_manifest(&root.join("svc"), SERVICE, &[(MCP_ENV, "1".to_string())], extra),
        );
        state.services.set_timing(fast_timing());
        state.services.set_local_api_url("http://127.0.0.1:9".into());
        state.services.start(SERVICE, &state.app, &state.manifest).unwrap();
        wait_service_healthy(&state.services, SERVICE);
        clear_tool_list_cache();
    }

    fn with_profile(name: &str, test: impl FnOnce(&Path)) {
        let _serial = crate::tauri_runtime::profile_env_serial()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let root = std::env::temp_dir().join(format!("garden-svc-mcp-{name}-{}", uuid::Uuid::new_v4()));
        let profile = root.join("profile");
        std::fs::create_dir_all(&profile).unwrap();
        let previous = std::env::var_os("GARDEN_PROFILE_DIR");
        std::env::set_var("GARDEN_PROFILE_DIR", &profile);
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| test(&root)));
        match previous {
            Some(previous) => std::env::set_var("GARDEN_PROFILE_DIR", previous),
            None => std::env::remove_var("GARDEN_PROFILE_DIR"),
        }
        let _ = std::fs::remove_dir_all(&root);
        if let Err(error) = outcome {
            std::panic::resume_unwind(error);
        }
    }

    async fn rpc(state: &Arc<LoopbackState>, token: &str, method: &str, params: Value) -> Value {
        let request = axum::http::Request::builder()
            .method("POST")
            .uri("/mcp")
            .header("authorization", format!("Bearer {token}"))
            .header("content-type", "application/json")
            .body(Body::from(
                json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).to_string(),
            ))
            .unwrap();
        let response = crate::loopback_router::loopback_router(state.clone())
            .oneshot(request)
            .await
            .unwrap();
        let bytes = axum::body::to_bytes(response.into_body(), 50_000_000).await.unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    fn names(listed: &Value) -> Vec<String> {
        listed["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["name"].as_str().unwrap().to_string())
            .collect()
    }

    fn tool<'a>(listed: &'a Value, name: &str) -> &'a Value {
        listed["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|tool| tool["name"] == name)
            .unwrap_or_else(|| panic!("{name} not listed"))
    }

    fn token(state: &Arc<LoopbackState>, grant: Option<&str>, scopes: Option<Vec<String>>) -> (String, String) {
        let created = create_client_token(
            &state.app,
            CreateLoopbackClientTokenInput {
                label: Some("svc-mcp".into()),
                grant_profile_id: grant.map(str::to_string),
                scopes,
                expires_in_days: None,
            },
        )
        .unwrap();
        (created.token, created.record.token_id)
    }

    /// A service that declares `mcp` lends its prefixed read tools to every
    /// reader and the rest to admins; gardend fills the graph in, mints who
    /// calls, passes the service's errors through, and answers loudly when the
    /// service takes too long.
    #[test]
    fn lent_tools_join_gardends_mcp_and_answer_through_it() {
        with_profile("lent", |root| {
            let app = crate::tauri_runtime::build_mock_app_for_tests(true);
            let state = state(app, root, CellGraphBoundary::for_test(None));
            let baseline = crate::app_runtime::async_runtime::block_on(rpc(&state, MASTER, "tools/list", json!({})));
            let baseline = names(&baseline);
            start_toys(
                &state,
                root,
                json!({"mcp": {"graphs": [GRAPH], "toolPrefix": "toy_", "timeoutMs": 1500}}),
            );
            crate::app_runtime::async_runtime::block_on(async {
                let listed = rpc(&state, MASTER, "tools/list", json!({})).await;
                let listed_names = names(&listed);
                assert_eq!(listed_names.len(), baseline.len() + 4, "{listed_names:?}");
                for lent in ["toy_read", "toy_write", "toy_slow", "toy_fail"] {
                    assert!(listed_names.iter().any(|name| name == lent), "{lent}");
                }
                assert!(!listed_names.iter().any(|name| name == "stray"), "outside the prefix");
                assert_eq!(
                    listed_names.iter().filter(|name| *name == "read_document").count(),
                    1,
                    "a lent tool never shadows gardend's own"
                );
                let read = tool(&listed, "toy_read");
                assert_eq!(read["inputSchema"]["properties"]["graph_id"]["enum"], json!([GRAPH]));
                assert!(read["inputSchema"]["properties"].get("graphId").is_none());
                assert_eq!(read["inputSchema"]["required"], json!(["q"]));
                assert_eq!(read["_meta"]["sophia.service"], SERVICE);
                assert_eq!(read["_meta"]["sophia.local.requiredScopes"], json!(["graphs.read"]));
                assert_eq!(tool(&listed, "toy_write")["_meta"]["sophia.local.requiredScopes"], json!(["services.manage"]));

                // The master key: the owner; the graph filled in and taken off
                // the arguments; nothing the caller sent passes.
                let called = rpc(&state, MASTER, "tools/call", json!({"name": "toy_read", "arguments": {"q": "cows"}})).await;
                let seen = &called["result"]["structuredContent"];
                assert_eq!(seen["arguments"], json!({"q": "cows"}), "{called}");
                assert_eq!(seen["graphId"], GRAPH);
                assert_eq!(seen["graphOwner"], "user:owner");
                assert_eq!(seen["callerRole"], "owner");
                assert_eq!(seen["graphRole"], "owner");
                assert_eq!(seen["userId"], "owner");
                assert_eq!(seen["authorization"], Value::Null, "the caller's credential never reaches the service");
                let named = rpc(&state, MASTER, "tools/call", json!({"name": "toy_read", "arguments": {"q": "x", "graphId": GRAPH}})).await;
                assert_eq!(named["result"]["structuredContent"]["arguments"], json!({"q": "x"}));

                let elsewhere = rpc(&state, MASTER, "tools/call", json!({"name": "toy_read", "arguments": {"q": "x", "graph_id": "notes"}})).await;
                assert_eq!(elsewhere["error"]["code"], -32602, "{elsewhere}");
                assert!(elsewhere["error"]["message"].as_str().unwrap().contains(GRAPH));

                let failed = rpc(&state, MASTER, "tools/call", json!({"name": "toy_fail", "arguments": {}})).await;
                assert_eq!(failed["error"]["code"], -32040, "the service's error passes through: {failed}");
                assert_eq!(failed["error"]["message"], "toy_fail exceeded its budget");
                assert_eq!(failed["error"]["data"]["phase"], "query");

                let started = std::time::Instant::now();
                let slow = rpc(&state, MASTER, "tools/call", json!({"name": "toy_slow", "arguments": {}})).await;
                assert!(started.elapsed() < Duration::from_secs(10), "gardend waited {:?}", started.elapsed());
                assert_eq!(slow["error"]["data"]["code"], SERVICE_TOOL_TIMEOUT_CODE, "{slow}");
                assert!(slow["error"]["message"].as_str().unwrap().contains("1500 ms"), "{slow}");

                let unknown = rpc(&state, MASTER, "tools/call", json!({"name": "toy_nothing", "arguments": {}})).await;
                assert_eq!(unknown["error"]["code"], -32602, "{unknown}");

                // A reader who may call tools: the read tools, as a viewer.
                let mut viewer_scopes = crate::loopback_token_grants::grant_profile_scopes("read-only");
                viewer_scopes.push("mcp.tools.call".into());
                let (viewer, viewer_id) = token(&state, None, Some(viewer_scopes));
                let listed = rpc(&state, &viewer, "tools/list", json!({})).await;
                let viewer_names = names(&listed);
                assert!(viewer_names.iter().any(|name| name == "toy_read"));
                assert!(!viewer_names.iter().any(|name| name == "toy_write"), "a viewer is not offered a write");
                let called = rpc(&state, &viewer, "tools/call", json!({"name": "toy_read", "arguments": {"q": "cows"}})).await;
                assert_eq!(called["result"]["structuredContent"]["callerRole"], "viewer", "{called}");
                assert_eq!(called["result"]["structuredContent"]["userId"], format!("token:{viewer_id}"));
                let refused = rpc(&state, &viewer, "tools/call", json!({"name": "toy_write", "arguments": {}})).await;
                assert_eq!(refused["error"]["code"], -32003, "{refused}");

                // A member: not an admin, and the manifest keeps the rest to admins.
                let (member, _) = token(&state, Some("member"), None);
                let refused = rpc(&state, &member, "tools/call", json!({"name": "toy_write", "arguments": {}})).await;
                assert_eq!(refused["error"]["code"], -32003, "{refused}");
                let called = rpc(&state, &member, "tools/call", json!({"name": "toy_read", "arguments": {"q": "y"}})).await;
                assert_eq!(called["result"]["structuredContent"]["callerRole"], "editor", "{called}");

                // A token bound to the graph sees the tools without the
                // argument; one bound elsewhere does not see them at all.
                let mut entry = crate::loopback_token_grants::grant_profile_scopes("read-only");
                entry.extend(["mcp.tools.read".to_string(), "mcp.tools.call".to_string()]);
                let bound = state
                    .services
                    .delegate(Delegation {
                        service_id: SERVICE.into(),
                        user_id: "vera@example.com".into(),
                        graph_id: GRAPH.into(),
                        role: "viewer".into(),
                        scopes: entry.clone(),
                        public: false,
                    })
                    .unwrap();
                let listed = rpc(&state, &bound, "tools/list", json!({})).await;
                let read = tool(&listed, "toy_read");
                assert!(read["inputSchema"]["properties"].get("graph_id").is_none(), "{read}");
                let called = rpc(&state, &bound, "tools/call", json!({"name": "toy_read", "arguments": {"q": "z"}})).await;
                assert_eq!(called["result"]["structuredContent"]["userId"], "vera@example.com", "{called}");
                assert_eq!(called["result"]["structuredContent"]["graphId"], GRAPH);
                let elsewhere = state
                    .services
                    .delegate(Delegation {
                        service_id: SERVICE.into(),
                        user_id: "vera@example.com".into(),
                        graph_id: "notes".into(),
                        role: "viewer".into(),
                        scopes: entry,
                        public: false,
                    })
                    .unwrap();
                let listed = rpc(&state, &elsewhere, "tools/list", json!({})).await;
                assert!(!names(&listed).iter().any(|name| name.starts_with("toy_")));
                let refused = rpc(&state, &elsewhere, "tools/call", json!({"name": "toy_read", "arguments": {"q": "z"}})).await;
                assert_eq!(refused["error"]["code"], -32004, "{refused}");
            });
            state.services.shutdown();
        });
    }

    /// Without `mcp` in the manifest nothing is lent, and a hosted cell never
    /// asks a service for tools.
    #[test]
    fn nothing_is_lent_without_a_declaration_or_in_a_cell() {
        with_profile("inert", |root| {
            let app = crate::tauri_runtime::build_mock_app_for_tests(true);
            let state = state(app, root, CellGraphBoundary::for_test(None));
            let baseline = crate::app_runtime::async_runtime::block_on(rpc(&state, MASTER, "tools/list", json!({})));
            start_toys(&state, root, json!({}));
            crate::app_runtime::async_runtime::block_on(async {
                let listed = rpc(&state, MASTER, "tools/list", json!({})).await;
                assert_eq!(names(&listed), names(&baseline));
                let called = rpc(&state, MASTER, "tools/call", json!({"name": "toy_read", "arguments": {"q": "x"}})).await;
                assert!(called.get("result").is_none(), "{called}");
                assert!(called["error"]["message"].as_str().unwrap().contains("toy_read"), "{called}");
            });
            state.services.shutdown();

            let app = crate::tauri_runtime::build_mock_app_for_tests(true);
            let cell = state_for_cell(app, root);
            cell.services.pin_manifest(
                SERVICE,
                write_service_manifest(
                    &root.join("svc-cell"),
                    SERVICE,
                    &[(MCP_ENV, "1".to_string())],
                    json!({"mcp": {"graphs": [GRAPH], "toolPrefix": "toy_"}}),
                ),
            );
            crate::app_runtime::async_runtime::block_on(async {
                let scopes = crate::loopback_token_grants::session_all_token_grant()
                    .scopes
                    .iter()
                    .map(|scope| scope.to_string())
                    .collect::<Vec<_>>();
                let caller = ServiceCaller { scopes: &scopes, user_id: None, bound: None, public: None };
                assert!(listed_service_tools(&cell, &caller).await.is_empty());
                let params = json!({"name": "toy_read", "arguments": {}});
                assert!(call_service_tool(&cell, None, &params, &caller).await.is_none());
            });
        });
    }

    /// Public mode: a visitor with no credential is offered the read-only
    /// lent tools of a service public mode lists, about the public graphs
    /// only, and calls them as a viewer; nothing else of the service, and
    /// nothing at all of a service public mode does not list.
    #[test]
    fn a_public_visitor_reads_through_lent_tools_on_public_graphs_only() {
        with_profile("public", |root| {
            let profile = root.join("profile");
            for (services, lends) in [(json!([SERVICE]), true), (json!([]), false)] {
                let app = crate::tauri_runtime::build_mock_app_for_tests(true);
                let self_host = SelfHost::from_settings(crate::loopback_self_host::SelfHostSettings::default())
                    .unwrap()
                    .with_public(
                        Some(crate::public_mode::PublicMode::for_test(
                            json!({ "graphs": [GRAPH], "services": services }),
                            &profile,
                        )),
                        Some("https://cow-tools.example.com"),
                    );
                let state = state_with(app, root, CellGraphBoundary::for_test(None), self_host);
                start_toys(
                    &state,
                    root,
                    json!({"mcp": {"graphs": [GRAPH, "notes"], "toolPrefix": "toy_", "timeoutMs": 1500}}),
                );
                crate::app_runtime::async_runtime::block_on(async {
                    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                    let address = listener.local_addr().unwrap();
                    let router = crate::loopback_router::loopback_router(state.clone());
                    tokio::spawn(async move {
                        let _ = axum::serve(
                            listener,
                            router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
                        )
                        .await;
                    });
                    let client = reqwest::Client::new();
                    let visit = |method: &'static str, params: Value| {
                        let client = client.clone();
                        async move {
                            client
                                .post(format!("http://{address}/mcp"))
                                .json(&json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}))
                                .send()
                                .await
                                .unwrap()
                                .json::<Value>()
                                .await
                                .unwrap()
                        }
                    };
                    let listed = visit("tools/list", json!({})).await;
                    let lent: Vec<&Value> = listed["result"]["tools"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .filter(|tool| tool["name"].as_str().unwrap().starts_with("toy_"))
                        .collect();
                    if !lends {
                        assert!(lent.is_empty(), "a service public mode does not list lends nothing: {lent:?}");
                        return;
                    }
                    let names: Vec<&str> = lent.iter().map(|tool| tool["name"].as_str().unwrap()).collect();
                    assert!(names.contains(&"toy_read") && !names.contains(&"toy_write"), "{names:?}");
                    let read = lent.iter().find(|tool| tool["name"] == "toy_read").unwrap();
                    assert_eq!(read["inputSchema"]["properties"]["graph_id"]["enum"], json!([GRAPH]), "public graphs only");
                    let called = visit("tools/call", json!({"name": "toy_read", "arguments": {"q": "vacas"}})).await;
                    let seen = &called["result"]["structuredContent"];
                    assert_eq!(seen["graphId"], GRAPH, "{called}");
                    assert_eq!(seen["callerRole"], "viewer");
                    assert!(seen["userId"].as_str().unwrap().starts_with("anon:"), "{seen}");
                    let private = visit("tools/call", json!({"name": "toy_read", "arguments": {"q": "x", "graph_id": "notes"}})).await;
                    assert_eq!(private["error"]["code"], -32004, "{private}");
                    let write = visit("tools/call", json!({"name": "toy_write", "arguments": {}})).await;
                    assert!(write.get("error").is_some(), "{write}");
                });
                state.services.shutdown();
            }
        });
    }

    fn state_for_cell(app: crate::app_runtime::AppHandle, dir: &Path) -> Arc<LoopbackState> {
        let plain = state(app, &dir.join("cell"), CellGraphBoundary::for_test(Some(GRAPH)));
        plain
    }
}
