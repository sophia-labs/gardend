use crate::{
    loopback_scopes::{crdt_operation_scopes, mcp_tool_scopes},
    loopback_state::LoopbackState,
    mcp_dispatch_registry::{lookup, McpCallCtx},
    mcp_rpc_protocol::{mcp_error, mcp_error_with_data, mcp_scope_error, mcp_success, McpRpcResponse},
};

pub(crate) async fn handle_mcp_tool_call(
    state: &LoopbackState,
    id: Option<serde_json::Value>,
    params: serde_json::Value,
    token_scopes: &[String],
    role: crate::cell_graph_boundary::CellRole,
) -> McpRpcResponse {
    if let Some(error) = mcp_scope_error(token_scopes, id.clone(), "mcp.tools.call") {
        return error;
    }
    let name = params
        .get("name")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    let arguments = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));
    if let Err(error) = state.cell_graph.authorize_mcp_role(name, role) {
        return mcp_error(id, error.mcp_code(), error.mcp_message());
    }
    let required_scopes = match mcp_call_required_scopes(name, &arguments) {
        Ok(scopes) => scopes,
        Err(error) => return mcp_error(id, -32602, error),
    };
    for required_scope in required_scopes {
        if let Some(error) = mcp_scope_error(token_scopes, id.clone(), required_scope) {
            return error;
        }
    }

    let Some(entry) = lookup(name) else {
        return mcp_error(id, -32602, format!("unknown tool {name}"));
    };
    let arguments = match state
        .cell_graph
        .scope_mcp_arguments(name, arguments, &state.jobs)
    {
        Ok(arguments) => arguments,
        Err(error) => return mcp_error(id, error.mcp_code(), error.mcp_message()),
    };

    let ctx = McpCallCtx {
        app: state.app.clone(),
        jobs: &state.jobs,
    };
    // The dispatcher boundary is no longer lossy about KIND-adjacent meaning:
    // every registry handler returns `AppResult<Value>`; the message stays
    // verbatim (doctrine: errors surface verbatim) and the taxonomy code
    // (`app_error_codes`), when the error was classified, rides in `data`.
    // The JSON-RPC code stays -32000 so no client's envelope handling changes.
    let result = (entry.handler)(&ctx, &arguments).await;

    match result {
        Ok(value) => mcp_success(
            id,
            serde_json::json!({
                "content": [
                    {
                        "type": "text",
                        "text": serde_json::to_string_pretty(&value).unwrap_or_else(|_| "{}".to_string()),
                    }
                ],
                "structuredContent": value,
            }),
        ),
        Err(error) => tool_failure_response(id, error),
    }
}

/// The dispatcher boundary's error→envelope mapping (§4.2.5): the message
/// stays verbatim (doctrine: errors surface verbatim) and the taxonomy code
/// (`app_error_codes`), when the error was classified, rides in `data`. The
/// JSON-RPC code stays `-32000` so no client's envelope handling changes.
/// Factored out of [`handle_mcp_tool_call`] so it is unit-testable without a
/// full `LoopbackState`/tool registry.
fn tool_failure_response(
    id: Option<serde_json::Value>,
    error: crate::app_error::AppError,
) -> McpRpcResponse {
    let data = error.code().map(|code| serde_json::json!({ "code": code }));
    mcp_error_with_data(id, -32000, error.message(), data)
}

fn mcp_call_required_scopes(
    name: &str,
    arguments: &serde_json::Value,
) -> Result<Vec<&'static str>, String> {
    if name != "crdt_operation" {
        return Ok(mcp_tool_scopes(name));
    }

    let kind = arguments
        .get("kind")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .trim();
    crdt_operation_scopes(kind)
        .ok_or_else(|| format!("unsupported local CRDT operation kind {kind}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcp_crdt_operation_scopes_are_selected_from_kind_argument() {
        assert_eq!(
            mcp_call_required_scopes(
                "crdt_operation",
                &serde_json::json!({"kind": " workspace.deleteDocument "}),
            ),
            Ok(vec!["documents.delete.crdt", "workspace.delete.crdt"])
        );
        assert_eq!(
            mcp_call_required_scopes("write_document", &serde_json::json!({})),
            Ok(vec!["documents.write.crdt"])
        );
        assert!(mcp_call_required_scopes(
            "crdt_operation",
            &serde_json::json!({"kind": "unknown.operation"}),
        )
        .is_err());
    }

    #[test]
    fn tool_failure_carries_the_code_in_error_data() {
        let response = tool_failure_response(
            Some(serde_json::json!(7)),
            crate::app_error::AppError::conflict(
                "stale graph incarnation for g: expected a, actual b",
            )
            .with_code(crate::app_error_codes::STALE_GRAPH_INCARNATION),
        );
        let value = serde_json::to_value(&response).unwrap();
        assert_eq!(value["error"]["code"], -32000);
        assert_eq!(
            value["error"]["message"],
            "stale graph incarnation for g: expected a, actual b"
        );
        assert_eq!(value["error"]["data"]["code"], "stale_graph_incarnation");
    }

    #[test]
    fn unclassified_tool_failure_omits_error_data() {
        let response =
            tool_failure_response(None, crate::app_error::AppError::internal("boom"));
        let value = serde_json::to_value(&response).unwrap();
        assert_eq!(value["error"]["code"], -32000);
        assert_eq!(value["error"]["message"], "boom");
        assert!(
            value["error"].get("data").is_none(),
            "data must be absent, not null, when the error carries no code"
        );
    }
}
