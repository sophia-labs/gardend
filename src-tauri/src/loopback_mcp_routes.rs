use crate::{
    cell_graph_boundary::CellGraphBoundaryError,
    local_service_host::Delegation,
    loopback_audit_log::{append_loopback_audit_event, LoopbackAuditEvent},
    loopback_http::{
        authorized_loopback_caller, bearer_token, loopback_error, origin_allowed,
        require_loopback_scope,
    },
    loopback_state::LoopbackState,
    loopback_token_grants::grant_profile_scopes,
    mcp_rpc_protocol::{mcp_error, mcp_scope_error, mcp_success, McpRpcRequest, McpRpcResponse},
    mcp_tool_dispatch::handle_mcp_tool_call,
    request_actor::{acting_for, RequestActor},
};
use axum::{
    extract::{Extension, State},
    http::{HeaderMap, Method, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use serde_json::Value;
use std::sync::Arc;

pub(super) fn loopback_mcp_router() -> Router<Arc<LoopbackState>> {
    Router::new().route("/mcp", get(loopback_mcp_info).post(loopback_mcp))
}

pub(super) async fn loopback_mcp_info(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "mcp.tools.read") {
        return response;
    }
    Json(serde_json::json!({
        "protocol": "mcp",
        "transport": "streamable-http-json",
        "endpoint": state.manifest.mcp_url,
        "methods": ["initialize", "tools/list", "tools/call"],
    }))
    .into_response()
}

/// Who an MCP request speaks for and what it may reach.
struct McpCaller {
    scopes: Vec<String>,
    /// The one graph a delegation token binds every call to.
    graph: Option<Delegation>,
    /// Who the audit records name (not set in a hosted cell).
    actor: Option<RequestActor>,
    /// A public-mode visitor or account: every call names one of the public
    /// graphs (`public_mode`).
    public: bool,
    /// A public delegation's own graph (the session's): a call that names no
    /// graph reads this one.
    home_graph: Option<String>,
    /// A delegation made for a public-mode caller: its calls get the public
    /// SPARQL ceilings too.
    public_delegation: bool,
    /// W6: the Sophia account behind a public caller (or its delegation), for
    /// a write its fence may admit (`visitor_fence`).
    visitor: Option<crate::visitor_fence::VisitorAccount>,
}

/// Authenticate an MCP request. A delegation token (handed to a supervised
/// service on a graph-scoped request, `local_service_host::Delegation`) works
/// here and nowhere else: it binds every call to its graph and speaks for the
/// person the service acts for. Any other credential is checked as on every
/// loopback route. A hosted cell never sees a delegation token (its service
/// routes are denied) and is not asked to look one up.
fn mcp_caller(
    headers: &HeaderMap,
    state: &LoopbackState,
    entry_scope: &str,
) -> Result<McpCaller, Response> {
    if !state.cell_graph.is_enabled() {
        if let Some(delegation) =
            bearer_token(headers).and_then(|token| state.services.delegation(token))
        {
            if !origin_allowed(headers, state) {
                return Err(loopback_error(StatusCode::FORBIDDEN, "invalid origin"));
            }
            if !delegated_principal_active(state, &delegation) {
                return Err(loopback_error(
                    StatusCode::UNAUTHORIZED,
                    "the client token this delegation acts for is revoked or expired",
                ));
            }
            if !delegation.scopes.iter().any(|scope| scope == entry_scope) {
                return Err(loopback_error(
                    StatusCode::FORBIDDEN,
                    &format!("loopback token missing required scope {entry_scope}"),
                ));
            }
            // A delegation made for a public-mode visitor reads every public
            // graph, as the visitor may (one conversation over all of them);
            // any other is bound to its one graph.
            let public_delegation = delegation.public && state.self_host.public().is_some();
            return Ok(McpCaller {
                scopes: delegation.scopes.clone(),
                actor: Some(RequestActor {
                    user_id: delegation.user_id.clone(),
                    via: Some(format!("service:{}", delegation.service_id)),
                }),
                home_graph: public_delegation.then(|| delegation.graph_id.clone()),
                visitor: public_delegation
                    .then(|| delegation.visitor.clone())
                    .flatten(),
                graph: (!public_delegation).then_some(delegation),
                public: public_delegation,
                public_delegation,
            });
        }
    }
    let caller = authorized_loopback_caller(headers, state, &[entry_scope])?;
    Ok(McpCaller {
        actor: (!state.cell_graph.is_enabled()).then(|| RequestActor {
            user_id: caller.principal.user_id(),
            via: None,
        }),
        public: caller.is_public(),
        visitor: crate::visitor_fence::VisitorAccount::of(&caller.principal),
        scopes: caller.scopes,
        graph: None,
        home_graph: None,
        public_delegation: false,
    })
}

/// A delegation made for a client token works only while that token does: a
/// revoked or expired client token takes its delegations with it. (The master
/// key cannot be revoked; an Access login is verified when the service's
/// caller comes back, and the delegation outlives it by at most its hour.)
fn delegated_principal_active(state: &LoopbackState, delegation: &Delegation) -> bool {
    let Some(token_id) = delegation.user_id.strip_prefix("token:") else {
        return true;
    };
    crate::loopback_client_token_service::list_client_tokens(&state.app)
        .map(|tokens| {
            tokens
                .iter()
                .any(|token| token.token_id == token_id && token.active)
        })
        .unwrap_or(false)
}

pub(super) async fn loopback_mcp(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    method: Method,
    lease: Option<Extension<crate::cell_graph_boundary::VerifiedCellLease>>,
    Json(request): Json<McpRpcRequest>,
) -> Response {
    if method != Method::POST {
        return loopback_error(StatusCode::METHOD_NOT_ALLOWED, "MCP endpoint accepts POST");
    }
    let entry_scope = match request.method.as_str() {
        "tools/call" => "mcp.tools.call",
        _ => "mcp.tools.read",
    };
    let caller = match mcp_caller(&headers, &state, entry_scope) {
        Ok(caller) => caller,
        Err(response) => return response,
    };
    let role = lease
        .map(|Extension(lease)| lease.role)
        .unwrap_or(crate::cell_graph_boundary::CellRole::Owner);
    let audit = caller
        .graph
        .as_ref()
        .zip(caller.actor.as_ref())
        .and_then(|(graph, actor)| {
            write_tool_named(&request).map(|tool| (tool, graph.clone(), actor.clone()))
        });
    let user_id = caller.actor.as_ref().map(|actor| actor.user_id.clone());
    let handled = acting_for(
        caller.actor,
        handle_mcp_request(
            &state,
            request,
            &caller.scopes,
            role,
            caller.graph.as_ref(),
            caller.public.then_some(caller.home_graph.as_deref()),
            user_id.as_deref(),
            caller.visitor.as_ref(),
        ),
    );
    let response = match state.self_host.public() {
        // A service acting for a visitor: the visitor's SPARQL ceilings.
        Some(public) if caller.public_delegation => {
            crate::public_mode::as_public_request(
                crate::public_mode::PublicRequest {
                    visitor_id: String::new(),
                    ip: std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
                    account_token: false,
                    sparql: public.sparql_ceiling(),
                },
                handled,
            )
            .await
        }
        _ => handled.await,
    };
    if let Some((tool, delegation, actor)) = audit {
        audit_delegated_write(&state, &tool, &delegation, &actor, &response);
    }
    Json(response).into_response()
}

/// The tool a `tools/call` names when that tool can write: one of its scopes
/// is not a reader's.
fn write_tool_named(request: &McpRpcRequest) -> Option<String> {
    if request.method != "tools/call" {
        return None;
    }
    let name = request.params.get("name")?.as_str()?;
    let readers = grant_profile_scopes("read-only");
    crate::loopback_scopes::mcp_tool_scopes(name)
        .iter()
        .any(|scope| !readers.iter().any(|reader| reader == scope))
        .then(|| name.to_string())
}

/// One audit row for every call of a write tool a service makes for a
/// person: the person as the actor, the service it went through, the graph.
fn audit_delegated_write(
    state: &LoopbackState,
    tool: &str,
    delegation: &Delegation,
    actor: &RequestActor,
    response: &McpRpcResponse,
) {
    let event = LoopbackAuditEvent::new(
        "mcp.tools.call",
        tool,
        &actor.user_id,
        if response.is_error() {
            "failed"
        } else {
            "succeeded"
        },
        Some(delegation.graph_id.clone()),
        serde_json::json!({
            "graphId": delegation.graph_id,
            "graphRole": delegation.role,
            "via": actor.via,
        }),
    );
    if let Err(error) = append_loopback_audit_event(&state.app, &event) {
        log::debug!("failed to append the MCP audit event for {tool}: {error}");
    }
}

async fn handle_mcp_request(
    state: &LoopbackState,
    request: McpRpcRequest,
    token_scopes: &[String],
    role: crate::cell_graph_boundary::CellRole,
    bound: Option<&Delegation>,
    // `Some(home)`: a public-mode caller (`home`: a public delegation's own
    // graph, for calls that name none).
    public: Option<Option<&str>>,
    user_id: Option<&str>,
    visitor: Option<&crate::visitor_fence::VisitorAccount>,
) -> McpRpcResponse {
    if request.jsonrpc.as_deref() != Some("2.0") {
        return mcp_error(request.id, -32600, "expected JSON-RPC 2.0 request");
    }

    match request.method.as_str() {
        "initialize" => {
            if let Some(error) = mcp_scope_error(token_scopes, request.id.clone(), "mcp.tools.read")
            {
                return error;
            }
            mcp_success(
                request.id,
                serde_json::json!({
                    "protocolVersion": "2025-03-26",
                    // A public Sophia names itself as its MCP server card does
                    // (`/mcp/server-card`); any other gardend as before.
                    "serverInfo": state
                        .self_host
                        .public()
                        .and_then(crate::public_mode::PublicMode::server_info)
                        .unwrap_or_else(|| serde_json::json!({
                            "name": "sophia-local-native",
                            "version": env!("CARGO_PKG_VERSION"),
                        })),
                    "capabilities": {
                        "tools": {},
                    },
                }),
            )
        }
        "tools/list" => {
            if let Some(error) = mcp_scope_error(token_scopes, request.id.clone(), "mcp.tools.read")
            {
                return error;
            }
            let optional = match request.params.get("optionalCapabilities") {
                None => false,
                Some(Value::Array(values))
                    if values.iter().all(|v| v.as_str() == Some("custom-css")) =>
                {
                    !values.is_empty()
                }
                Some(_) => {
                    return mcp_error(
                        request.id,
                        -32602,
                        "optionalCapabilities must be an array containing only custom-css",
                    )
                }
            };
            if public.is_some() {
                let mut result = public_tools_list(state, token_scopes);
                fenced_tools_list(state, visitor, &mut result);
                let lent = crate::service_mcp::listed_service_tools(
                    state,
                    &crate::service_mcp::ServiceCaller {
                        scopes: token_scopes,
                        user_id,
                        bound,
                        public,
                    },
                )
                .await;
                if let Some(tools) = result["tools"].as_array_mut() {
                    tools.extend(lent);
                }
                return mcp_success(request.id, result);
            }
            let mut listed = match bound {
                Some(_) => crate::mcp_tool_registry::mcp_tools_list_result_for_bound_graph(
                    &state.cell_graph,
                    token_scopes,
                    optional,
                ),
                None => crate::mcp_tool_registry::mcp_tools_list_result_with_optional(
                    &state.cell_graph,
                    role,
                    optional,
                ),
            };
            // What this token may call of what is listed (an unbound list is
            // narrowed by the cell role only).
            crate::mcp_tool_registry::annotate_callable(&mut listed, token_scopes);
            let mut result =
                serde_json::to_value(listed).expect("McpToolsListResult always serializes");
            // Tools lent by local services (`service_mcp`): none unless a
            // service.json declares `mcp`, never in a hosted cell.
            let lent = crate::service_mcp::listed_service_tools(
                state,
                &crate::service_mcp::ServiceCaller {
                    scopes: token_scopes,
                    user_id,
                    bound,
                    public: None,
                },
            )
            .await;
            if !lent.is_empty() {
                if let Some(tools) = result["tools"].as_array_mut() {
                    tools.extend(lent);
                }
            }
            result["_meta"] = serde_json::json!({"sophia.optionalCapabilities":[{"id":"custom-css","description":"Explicit graph stylesheet read/guide and guarded write/reset; local application requires user consent","request":{"optionalCapabilities":["custom-css"]}}]});
            if let Some(delegation) = bound {
                result["_meta"]["sophia.boundGraph"] = serde_json::json!(delegation.graph_id);
            }
            mcp_success(request.id, result)
        }
        "tools/call" => {
            if let Some(response) = crate::service_mcp::call_service_tool(
                state,
                request.id.clone(),
                &request.params,
                &crate::service_mcp::ServiceCaller {
                    scopes: token_scopes,
                    user_id,
                    bound,
                    public,
                },
            )
            .await
            {
                return response;
            }
            let mut params = request.params;
            if let Some(home) = public {
                return public_tool_call(
                    state,
                    request.id,
                    params,
                    token_scopes,
                    role,
                    home,
                    visitor,
                )
                .await;
            }
            if let Some(delegation) = bound {
                if let Err(error) = bind_call(state, &delegation.graph_id, &mut params) {
                    return mcp_error(request.id, error.0, error.1);
                }
            }
            handle_mcp_tool_call(state, request.id, params, token_scopes, role).await
        }
        _ => mcp_error(
            request.id,
            -32601,
            format!("unknown MCP method {}", request.method),
        ),
    }
}

/// Bind a `tools/call` to the delegation's graph
/// (`CellGraphBoundary::bind_mcp_call_to_graph`), in place.
fn bind_call(
    state: &LoopbackState,
    graph_id: &str,
    params: &mut Value,
) -> Result<(), (i64, String)> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let arguments = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));
    match state
        .cell_graph
        .bind_mcp_call_to_graph(graph_id, &name, arguments, &state.jobs)
    {
        Ok(bound) => {
            if let Some(params) = params.as_object_mut() {
                params.insert("arguments".to_string(), bound);
            }
            Ok(())
        }
        Err(CellGraphBoundaryError::GraphMismatch) => Err((
            -32004,
            format!(
                "this token is bound to the graph \"{graph_id}\"; the call names another graph"
            ),
        )),
        Err(CellGraphBoundaryError::JobNotOwned) => Err((
            -32004,
            format!(
                "this token is bound to the graph \"{graph_id}\"; that job is not one of its jobs"
            ),
        )),
        Err(CellGraphBoundaryError::ProfileWideOperation(operation)) => Err((
            -32004,
            format!("{operation} reaches beyond the graph \"{graph_id}\" this token is bound to"),
        )),
        Err(CellGraphBoundaryError::UnclassifiedMcpTool(tool)) => Err((
            -32602,
            format!("MCP tool {tool} is not available to a token bound to one graph"),
        )),
        Err(error) => Err((error.mcp_code(), error.mcp_message())),
    }
}

/// `tools/list` for a public-mode caller: the tools that read one graph and
/// that the public scopes can call, each keeping its graph argument (which
/// must name a public graph), and `list_graphs` (the public graphs only).
fn public_tools_list(state: &LoopbackState, scopes: &[String]) -> Value {
    let Some(public) = state.self_host.public() else {
        return serde_json::json!({ "tools": [] });
    };
    let listed = crate::mcp_tool_registry::mcp_tools_list_result_with_optional(
        &state.cell_graph,
        crate::cell_graph_boundary::CellRole::Viewer,
        false,
    );
    let mut result = serde_json::to_value(listed).expect("McpToolsListResult always serializes");
    if let Some(tools) = result.get_mut("tools").and_then(Value::as_array_mut) {
        tools.retain(|tool| {
            tool.get("name")
                .and_then(Value::as_str)
                .is_some_and(|name| {
                    name == "list_graphs" || public_tool_callable(state, name, scopes)
                })
        });
    }
    result["_meta"] = serde_json::json!({
        "sophia.publicGraphs": public.graphs(),
        "sophia.public": "read-only: every call names one of sophia.publicGraphs as its graphId",
    });
    result
}

/// A tool a public caller may call: it reads one graph (or nothing graph-wide:
/// vocabularies), and the public scopes hold every scope it needs.
fn public_tool_callable(state: &LoopbackState, name: &str, scopes: &[String]) -> bool {
    let needed = crate::loopback_scopes::mcp_tool_scopes(name);
    !needed.is_empty()
        && needed
            .iter()
            .all(|scope| scopes.iter().any(|held| held == scope))
        && state.cell_graph.mcp_tool_public(name)
}

/// `tools/call` for a public-mode caller: the graph it names must be public,
/// and the call is bound to it as a delegation token binds one (another graph
/// anywhere in the arguments is refused).
async fn public_tool_call(
    state: &LoopbackState,
    id: Option<Value>,
    mut params: Value,
    scopes: &[String],
    role: crate::cell_graph_boundary::CellRole,
    home: Option<&str>,
    visitor: Option<&crate::visitor_fence::VisitorAccount>,
) -> McpRpcResponse {
    let Some(public) = state.self_host.public() else {
        return mcp_error(id, -32004, "graph not found");
    };
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    if name == "list_graphs" {
        return match crate::graph_catalog_projection::hosted_graph_entries_service(&state.app, true)
        {
            Ok(entries) => {
                let graphs = crate::public_mode::public_graph_entries(public, entries);
                let value = serde_json::json!({ "graphs": graphs, "count": graphs.len() });
                mcp_success(
                    id,
                    serde_json::json!({
                        "content": [{ "type": "text", "text": serde_json::to_string_pretty(&value).unwrap_or_default() }],
                        "structuredContent": value,
                    }),
                )
            }
            Err(error) => mcp_error(id, -32000, error.message()),
        };
    }
    if !public_tool_callable(state, &name, scopes) {
        // W6: only where public.json opens a visitors' folder; elsewhere public
        // mode answers exactly as it did before.
        if crate::visitor_fence::FENCED_TOOLS.contains(&name.as_str())
            && visitor.is_some()
            && public.visitor_folders().is_some()
        {
            return fenced_tool_call(state, id, params, scopes, role, home, visitor).await;
        }
        return mcp_error(
            id,
            -32003,
            format!("{name} is not open to visitors: this Sophia is open to read, and only its read tools are"),
        );
    }
    let arguments = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));
    let named = ["graphId", "graph_id"]
        .iter()
        .find_map(|key| arguments.get(*key).and_then(Value::as_str))
        .map(str::trim)
        .map(str::to_string);
    let graph = match named {
        Some(graph) if public.is_public_graph(&graph) => graph,
        Some(_) => return mcp_error(id, -32004, "graph not found"),
        None if state.cell_graph.mcp_tool_global(&name) => String::new(),
        None if home.is_some_and(|home| public.is_public_graph(home)) => {
            home.unwrap_or_default().to_string()
        }
        None => {
            return mcp_error(
                id,
                -32602,
                format!(
                    "name the graph (graphId): one of {}",
                    public
                        .graphs()
                        .iter()
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            )
        }
    };
    if !graph.is_empty() {
        if let Err(error) = bind_call(state, &graph, &mut params) {
            return mcp_error(id, error.0, error.1);
        }
        // What is in the Trash is the owner's to see (W6, sealed exam C1).
        let named = crate::visitor_fence::named_document_ids(params.get("arguments"));
        if !named.is_empty() {
            match crate::visitor_fence::trashed_documents(&state.app, &graph).await {
                Ok(trashed) => {
                    if let Some(document) = named
                        .iter()
                        .find(|document| trashed.contains(document.as_str()))
                    {
                        return mcp_error(id, -32004, format!("no document {document} in {graph}"));
                    }
                }
                Err(error) => return mcp_error(id, -32000, error),
            }
        }
    }
    handle_mcp_tool_call(state, id, params, scopes, role).await
}

/// W6: `tools/list` for a signed-in account whose fence opens a folder: the
/// write tools it may call there, and where (`_meta["sophia.writeFence"]`).
fn fenced_tools_list(
    state: &LoopbackState,
    visitor: Option<&crate::visitor_fence::VisitorAccount>,
    result: &mut Value,
) {
    let Some(public) = state.self_host.public() else {
        return;
    };
    let fences: Vec<Value> = public
        .graphs()
        .iter()
        .filter_map(|graph| crate::visitor_fence::fence_for(Some(public), visitor, graph).ok())
        .map(|fence| {
            serde_json::json!({
                "graphId": fence.graph_id,
                "folderId": fence.root_folder_id,
                "account": fence.account,
                "name": fence.name,
                "color": fence.color,
                "rules": "create documents in this folder (no documentId: the server names them); edit, rename, move inside it, wire and trash only your own",
            })
        })
        .collect();
    if fences.is_empty() {
        return;
    }
    let listed = crate::mcp_tool_registry::mcp_tools_list_result_with_optional(
        &state.cell_graph,
        crate::cell_graph_boundary::CellRole::Owner,
        false,
    );
    let all = serde_json::to_value(listed).unwrap_or_default();
    if let (Some(tools), Some(every)) = (
        result.get_mut("tools").and_then(Value::as_array_mut),
        all.get("tools").and_then(Value::as_array),
    ) {
        for tool in every {
            let name = tool.get("name").and_then(Value::as_str).unwrap_or_default();
            let listed_already = tools
                .iter()
                .any(|known| known.get("name").and_then(Value::as_str) == Some(name));
            if crate::visitor_fence::FENCED_TOOLS.contains(&name) && !listed_already {
                tools.push(tool.clone());
            }
        }
    }
    result["_meta"]["sophia.writeFence"] = Value::Array(fences);
}

/// W6: a write tool called by a signed-in account. Admitted only where its
/// fence opens a folder, with the tool's arguments checked (and shaped), the
/// scopes of that one tool lent for the call, and the fence carried into the
/// CRDT queue, where the executor checks ownership and place again.
async fn fenced_tool_call(
    state: &LoopbackState,
    id: Option<Value>,
    mut params: Value,
    scopes: &[String],
    role: crate::cell_graph_boundary::CellRole,
    home: Option<&str>,
    visitor: Option<&crate::visitor_fence::VisitorAccount>,
) -> McpRpcResponse {
    let Some(public) = state.self_host.public() else {
        return mcp_error(id, -32004, "graph not found");
    };
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let arguments = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));
    let named = ["graphId", "graph_id"]
        .iter()
        .find_map(|key| arguments.get(*key).and_then(Value::as_str))
        .map(str::trim)
        .map(str::to_string);
    let graph = match named.or_else(|| home.map(str::to_string)) {
        Some(graph) if public.is_public_graph(&graph) => graph,
        Some(_) => return mcp_error(id, -32004, "graph not found"),
        None => {
            return mcp_error(
                id,
                -32602,
                "name the graph (graphId) of the visitors' folder",
            )
        }
    };
    let fence = match crate::visitor_fence::fence_for(Some(public), visitor, &graph) {
        Ok(fence) => fence,
        Err(why) => return mcp_error(id, -32003, why.message(&graph)),
    };
    let folders = public
        .visitor_folders()
        .expect("a fence comes from a visitor folder");
    if let Err(retry) = folders.take_write(&fence.account, &fence.quotas) {
        return mcp_error(id, -32003, format!("slow down: try again in {retry} s"));
    }
    if let Err(error) = bind_call(state, &graph, &mut params) {
        return mcp_error(id, error.0, error.1);
    }
    if let Some(arguments) = params.get_mut("arguments") {
        if let Err(error) = crate::visitor_fence::admit_tool(&name, arguments, &fence) {
            return mcp_error(id, -32003, error);
        }
    }
    if name == "create_document" {
        if let Err(error) = folders.count_create(&fence.quotas) {
            return mcp_error(id, -32003, error);
        }
    }
    let mut lent: Vec<String> = scopes.to_vec();
    for scope in crate::visitor_fence::lent_scopes(&name) {
        if !lent.contains(&scope) {
            lent.push(scope);
        }
    }
    crate::visitor_fence::writing_within(
        fence,
        Vec::new(),
        handle_mcp_tool_call(state, id, params, &lent, role),
    )
    .await
}
