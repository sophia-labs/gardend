use crate::{
    loopback_http::{authorized_loopback_scopes, loopback_error, require_loopback_scope},
    loopback_state::LoopbackState,
    mcp_rpc_protocol::{mcp_error, mcp_scope_error, mcp_success, McpRpcRequest, McpRpcResponse},
    mcp_tool_dispatch::handle_mcp_tool_call,
};
use axum::{
    extract::{Extension, State},
    http::{HeaderMap, Method, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use std::sync::Arc;
use serde_json::Value;

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
    let token_scopes = match authorized_loopback_scopes(&headers, &state, &[entry_scope]) {
        Ok(scopes) => scopes,
        Err(response) => return response,
    };
    let role = lease
        .map(|Extension(lease)| lease.role)
        .unwrap_or(crate::cell_graph_boundary::CellRole::Owner);
    Json(handle_mcp_request(&state, request, &token_scopes, role).await).into_response()
}

async fn handle_mcp_request(
    state: &LoopbackState,
    request: McpRpcRequest,
    token_scopes: &[String],
    role: crate::cell_graph_boundary::CellRole,
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
                    "serverInfo": {
                        "name": "sophia-local-native",
                        "version": env!("CARGO_PKG_VERSION"),
                    },
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
                Some(Value::Array(values)) if values.iter().all(|v| v.as_str() == Some("custom-css")) => !values.is_empty(),
                Some(_) => return mcp_error(request.id, -32602, "optionalCapabilities must be an array containing only custom-css"),
            };
            let mut result = serde_json::to_value(crate::mcp_tool_registry::mcp_tools_list_result_with_optional(&state.cell_graph, role, optional)).expect("McpToolsListResult always serializes");
            result["_meta"] = serde_json::json!({"sophia.optionalCapabilities":[{"id":"custom-css","description":"Explicit graph stylesheet read/guide and guarded write/reset; local application requires user consent","request":{"optionalCapabilities":["custom-css"]}}]});
            mcp_success(request.id, result)
        }
        "tools/call" => {
            handle_mcp_tool_call(state, request.id, request.params, token_scopes, role).await
        }
        _ => mcp_error(
            request.id,
            -32601,
            format!("unknown MCP method {}", request.method),
        ),
    }
}
