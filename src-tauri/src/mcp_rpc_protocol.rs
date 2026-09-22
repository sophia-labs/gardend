use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct McpRpcRequest {
    pub(crate) jsonrpc: Option<String>,
    pub(crate) id: Option<serde_json::Value>,
    pub(crate) method: String,
    #[serde(default)]
    pub(crate) params: serde_json::Value,
}

#[derive(Debug, Serialize)]
pub(crate) struct McpRpcResponse {
    jsonrpc: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<McpRpcError>,
}

#[derive(Debug, Serialize)]
struct McpRpcError {
    code: i64,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<serde_json::Value>,
}

pub(crate) fn mcp_success(
    id: Option<serde_json::Value>,
    result: serde_json::Value,
) -> McpRpcResponse {
    McpRpcResponse {
        jsonrpc: "2.0",
        id,
        result: Some(result),
        error: None,
    }
}

/// Unchanged signature — every existing call site keeps compiling.
pub(crate) fn mcp_error(
    id: Option<serde_json::Value>,
    code: i64,
    message: impl Into<String>,
) -> McpRpcResponse {
    mcp_error_with_data(id, code, message, None)
}

/// The taxonomy code (`app_error_codes`) rides in `data`, never displacing
/// the JSON-RPC `code` (which stays `-32000` for tool failures — changing it
/// would break `McpClient.toolsCall`'s envelope handling on older clients)
/// or the verbatim `message`.
pub(crate) fn mcp_error_with_data(
    id: Option<serde_json::Value>,
    code: i64,
    message: impl Into<String>,
    data: Option<serde_json::Value>,
) -> McpRpcResponse {
    McpRpcResponse {
        jsonrpc: "2.0",
        id,
        result: None,
        error: Some(McpRpcError {
            code,
            message: message.into(),
            data,
        }),
    }
}

pub(crate) fn mcp_scope_error(
    token_scopes: &[String],
    id: Option<serde_json::Value>,
    required_scope: &str,
) -> Option<McpRpcResponse> {
    if token_scopes
        .iter()
        .any(|candidate| candidate == required_scope)
    {
        None
    } else {
        Some(mcp_error(
            id,
            -32003,
            format!("loopback token missing required scope {required_scope}"),
        ))
    }
}
