use crate::{
    emporium::openapi_emit::{open_api_store, read_api_operation},
    local_service_host::{
        Delegation, LocalServiceHealth, LocalServiceHost, LocalServiceState, LocalServiceStatus,
        MemberAccess, CHOREOGRAPH_SERVICE_ID, DELEGATED_TOKEN_HEADER, KG_ULTRA_SERVICE_ID,
    },
    loopback_http::{
        authorized_loopback_caller, loopback_caller, loopback_error, loopback_error_coded,
        loopback_result, require_loopback_scope, LoopbackCaller, LoopbackPrincipal,
    },
    loopback_state::LoopbackState,
    loopback_token_grants::{grant_profile_scopes, MEMBER_GRANT_PROFILE},
    workflow_run_mcp::resolve_executable_workflow,
};
use axum::{
    body::{Body, Bytes},
    extract::{OriginalUri, Path, Query, State},
    http::{header, HeaderMap, HeaderName, Method, StatusCode},
    response::Response,
    routing::{any, get, post},
    Router,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;
use uuid::Uuid;

const SERVICES_READ_SCOPE: &str = "services.read";
const SERVICES_MANAGE_SCOPE: &str = "services.manage";
const SERVICES_PROXY_SCOPE: &str = "services.proxy";
/// The proxy's 503 when a service cannot start on this computer (it lacks
/// what the service's `service.json` requires); its message is the reason.
pub(crate) const SERVICE_UNAVAILABLE_CODE: &str = "service_unavailable";

/// Choreograph's canonical invocation face (`choreograph.invocation-envelope.v1`).
const CHOREOGRAPH_INVOCATIONS_PATH: &str = "/api/invocations";
const INVOCATION_ENVELOPE_SCHEMA: &str = "choreograph.invocation-envelope.v1";
/// The one workflow a member may start through the proxy: the workspace chat.
const WORKSPACE_CHAT_WORKFLOW: &str = "workspace-chat-v1";
/// The headers gardend mints on every request it proxies to a local service,
/// which the service may believe together with its per-spawn secret (and
/// only then); a caller's own copies never pass:
/// - `x-internal-service`: the per-spawn secret (`MNEMOSYNE_INTERNAL_SERVICE_SECRET`);
/// - `x-user-id`: who calls (`LoopbackPrincipal::user_id`);
/// - `x-sophia-caller-role`: `owner`, `editor` or `viewer`, from the caller's grant;
/// - `x-graph-id`, `x-graph-owner` and `x-sophia-graph-role`: the graph a
///   request names, its owner ([`GRAPH_OWNER`]) and the caller's role on it,
///   where gardend knows the graph: a Choreograph invocation, and for any
///   other service a graph-scoped path ([`graph_scoped_path`]). Choreograph
///   refuses a role without its graph.
/// - `x-sophia-delegated-token`: with the graph, to a service whose manifest
///   says `"delegateCaller": true`, a token for gardend's `/mcp` that acts for
///   this caller on this graph only ([`delegated_scopes`]).
const CALLER_ROLE_HEADER: &str = "x-sophia-caller-role";
const GRAPH_ID_HEADER: &str = "x-graph-id";
const GRAPH_OWNER_HEADER: &str = "x-graph-owner";
const GRAPH_ROLE_HEADER: &str = "x-sophia-graph-role";
/// Public mode: `anonymous` (a visitor) or `account` (a Sophia account); absent
/// for every other caller.
const CALLER_KIND_HEADER: &str = "x-sophia-caller-kind";
const ACCOUNT_EMAIL_HEADER: &str = "x-sophia-account-email";
const ACCOUNT_EMAIL_VERIFIED_HEADER: &str = "x-sophia-account-email-verified";
/// Public mode: every public graph, a comma list (a public caller's delegation
/// reads them all).
const PUBLIC_GRAPHS_HEADER: &str = "x-sophia-public-graphs";
/// Who owns every graph of a gardend: the profile's owner, the identity the
/// master key is minted as (`X-User-ID: owner`). A member or an Access login
/// acts on the owner's graph with its own `actorRef`, as an editor acts on a
/// hosted owner's graph.
const GRAPH_OWNER: &str = "user:owner";

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LocalServiceLogsQuery {
    tail: Option<usize>,
}

pub(crate) fn loopback_service_router() -> Router<Arc<LoopbackState>> {
    Router::new()
        .route(
            "/api/services/{service_id}/status",
            get(loopback_local_service_status),
        )
        .route(
            "/api/services/{service_id}/start",
            post(loopback_local_service_start),
        )
        .route(
            "/api/services/{service_id}/stop",
            post(loopback_local_service_stop),
        )
        .route(
            "/api/services/{service_id}/logs",
            get(loopback_local_service_logs),
        )
        .route("/api/choreograph/status", get(loopback_choreograph_status))
        .route("/api/choreograph/start", post(loopback_choreograph_start))
        .route("/api/choreograph/stop", post(loopback_choreograph_stop))
        .route("/api/choreograph/logs", get(loopback_choreograph_logs))
        .route("/api/kg-ultra/status", get(loopback_kg_ultra_status))
        .route("/api/kg-ultra/start", post(loopback_kg_ultra_start))
        .route("/api/kg-ultra/stop", post(loopback_kg_ultra_stop))
        .route("/api/kg-ultra/logs", get(loopback_kg_ultra_logs))
        .route(
            "/services/{service_id}",
            any(loopback_local_service_proxy_root),
        )
        .route(
            "/services/{service_id}/{*path}",
            any(loopback_local_service_proxy),
        )
        .route(
            "/workflows/runs",
            get(loopback_choreograph_workflow_list).post(loopback_choreograph_workflow_run),
        )
        .route(
            "/workflows/runs/{run_id}",
            get(loopback_choreograph_workflow_detail),
        )
        .route(
            "/workflows/runs/{run_id}/events",
            get(loopback_choreograph_workflow_events),
        )
        .route(
            "/g/{graph_id}/operations/{operation_id}/invoke",
            post(loopback_invoke_graph_operation),
        )
        .route("/api/kg-ultra/intuition", post(loopback_kg_ultra_intuition))
        .route(
            "/api/kg-ultra/graph-intuition",
            post(loopback_kg_ultra_intuition),
        )
        .route(
            "/api/kg-ultra/link-predictions",
            post(loopback_kg_ultra_link_predictions),
        )
        .route(
            "/api/kg-ultra/graphs/{graph_id}/refresh",
            post(loopback_kg_ultra_graph_refresh),
        )
        .route(
            "/api/kg-ultra/graphs/{graph_id}/status",
            get(loopback_kg_ultra_graph_status),
        )
}

async fn loopback_local_service_status(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    Path(service_id): Path<String>,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, SERVICES_READ_SCOPE) {
        return response;
    }
    let mut status = state.services.status(&service_id, &state.app);
    attach_health(&state, &service_id, &mut status).await;
    loopback_result(Ok(status))
}

async fn loopback_choreograph_status(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, SERVICES_READ_SCOPE) {
        return response;
    }
    let mut status = state.services.status(CHOREOGRAPH_SERVICE_ID, &state.app);
    attach_health(&state, CHOREOGRAPH_SERVICE_ID, &mut status).await;
    loopback_result(Ok(status))
}

async fn loopback_kg_ultra_status(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, SERVICES_READ_SCOPE) {
        return response;
    }
    let mut status = state.services.status(KG_ULTRA_SERVICE_ID, &state.app);
    attach_health(&state, KG_ULTRA_SERVICE_ID, &mut status).await;
    loopback_result(Ok(status))
}

async fn loopback_local_service_start(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    Path(service_id): Path<String>,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, SERVICES_MANAGE_SCOPE) {
        return response;
    }
    start_service(&state.services, &state, &service_id)
}

async fn loopback_choreograph_start(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, SERVICES_MANAGE_SCOPE) {
        return response;
    }
    start_service(&state.services, &state, CHOREOGRAPH_SERVICE_ID)
}

async fn loopback_kg_ultra_start(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, SERVICES_MANAGE_SCOPE) {
        return response;
    }
    start_service(&state.services, &state, KG_ULTRA_SERVICE_ID)
}

async fn loopback_local_service_stop(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    Path(service_id): Path<String>,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, SERVICES_MANAGE_SCOPE) {
        return response;
    }
    loopback_result(state.services.stop(&service_id, &state.app))
}

async fn loopback_choreograph_stop(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, SERVICES_MANAGE_SCOPE) {
        return response;
    }
    loopback_result(state.services.stop(CHOREOGRAPH_SERVICE_ID, &state.app))
}

async fn loopback_kg_ultra_stop(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, SERVICES_MANAGE_SCOPE) {
        return response;
    }
    loopback_result(state.services.stop(KG_ULTRA_SERVICE_ID, &state.app))
}

async fn loopback_local_service_logs(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    Path(service_id): Path<String>,
    Query(query): Query<LocalServiceLogsQuery>,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, SERVICES_READ_SCOPE) {
        return response;
    }
    loopback_result(state.services.logs(&service_id, query.tail))
}

async fn loopback_choreograph_logs(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    Query(query): Query<LocalServiceLogsQuery>,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, SERVICES_READ_SCOPE) {
        return response;
    }
    loopback_result(state.services.logs(CHOREOGRAPH_SERVICE_ID, query.tail))
}

async fn loopback_kg_ultra_logs(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    Query(query): Query<LocalServiceLogsQuery>,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, SERVICES_READ_SCOPE) {
        return response;
    }
    loopback_result(state.services.logs(KG_ULTRA_SERVICE_ID, query.tail))
}

async fn loopback_local_service_proxy_root(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    method: Method,
    OriginalUri(uri): OriginalUri,
    Path(service_id): Path<String>,
    body: Bytes,
) -> Response {
    proxy_service_path(state, headers, method, uri.query(), &service_id, "/", body).await
}

async fn loopback_local_service_proxy(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    method: Method,
    OriginalUri(uri): OriginalUri,
    Path((service_id, path)): Path<(String, String)>,
    body: Bytes,
) -> Response {
    let upstream_path = format!("/{path}");
    proxy_service_path(
        state,
        headers,
        method,
        uri.query(),
        &service_id,
        &upstream_path,
        body,
    )
    .await
}

async fn loopback_choreograph_workflow_run(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    method: Method,
    OriginalUri(uri): OriginalUri,
    body: Bytes,
) -> Response {
    proxy_service_path(
        state,
        headers,
        method,
        uri.query(),
        CHOREOGRAPH_SERVICE_ID,
        "/api/workflows/run",
        body,
    )
    .await
}

async fn loopback_choreograph_workflow_list(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    method: Method,
    OriginalUri(uri): OriginalUri,
    body: Bytes,
) -> Response {
    proxy_service_path(
        state,
        headers,
        method,
        uri.query(),
        CHOREOGRAPH_SERVICE_ID,
        "/api/workflows/runs",
        body,
    )
    .await
}

async fn loopback_choreograph_workflow_detail(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    method: Method,
    OriginalUri(uri): OriginalUri,
    Path(run_id): Path<String>,
    body: Bytes,
) -> Response {
    let upstream_path = format!("/api/workflows/runs/{run_id}");
    proxy_service_path(
        state,
        headers,
        method,
        uri.query(),
        CHOREOGRAPH_SERVICE_ID,
        &upstream_path,
        body,
    )
    .await
}

async fn loopback_choreograph_workflow_events(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    method: Method,
    OriginalUri(uri): OriginalUri,
    Path(run_id): Path<String>,
    body: Bytes,
) -> Response {
    let upstream_path = format!("/api/workflows/runs/{run_id}/events");
    proxy_service_path(
        state,
        headers,
        method,
        uri.query(),
        CHOREOGRAPH_SERVICE_ID,
        &upstream_path,
        body,
    )
    .await
}

/// Execute the graph-authored Meaningful Object behind one published Operation.
/// Resolution is exact and digest-pinned; `read_api_operation` fails closed on
/// duplicate operation/binding subjects before any sandbox is launched.
async fn loopback_invoke_graph_operation(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    Path((graph_id, operation_id)): Path<(String, String)>,
    body: Bytes,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "rdf.query") {
        return response;
    }
    if let Err(response) = require_loopback_scope(&headers, &state, SERVICES_PROXY_SCOPE) {
        return response;
    }
    let caller = match loopback_caller(&headers, &state) {
        Ok(caller) => caller,
        Err(response) => return response,
    };
    // A graph operation runs a workflow; with no sandbox around a local
    // Choreograph that is an admin's act, not a member's (members chat).
    if !caller.holds(SERVICES_MANAGE_SCOPE) {
        return operation_error(
            StatusCode::FORBIDDEN,
            "admin_required",
            "running a graph operation needs an admin (services.manage); members may use chat",
        );
    }
    let input = if body.is_empty() {
        json!({})
    } else {
        match serde_json::from_slice::<Value>(&body) {
            Ok(input) => input,
            Err(error) => {
                return operation_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_input",
                    &format!("request body must be JSON: {error}"),
                )
            }
        }
    };
    let store = match open_api_store(&state.app, &graph_id) {
        Ok(store) => store,
        Err(error) => {
            return operation_error(
                StatusCode::NOT_FOUND,
                "graph_not_found",
                &format!("graph '{graph_id}': {error}"),
            )
        }
    };
    let operation = match read_api_operation(&store, &graph_id, &operation_id) {
        Ok(operation) => operation,
        Err(error) if error.starts_with("operation_not_found:") => {
            return operation_error(StatusCode::NOT_FOUND, "operation_not_found", &error)
        }
        Err(error) if error.contains("contested") => {
            return operation_error(StatusCode::CONFLICT, "contested_executable_binding", &error)
        }
        Err(error) => {
            return operation_error(StatusCode::CONFLICT, "operation_resolution_failed", &error)
        }
    };
    if operation.deprecated {
        return operation_error(
            StatusCode::GONE,
            "operation_deprecated",
            &format!("operation '{operation_id}' is deprecated"),
        );
    }
    let Some(binding) = operation.binding else {
        return operation_error(
            StatusCode::CONFLICT,
            "workflow_binding_incomplete",
            &format!("operation '{operation_id}' has no WorkflowBinding"),
        );
    };
    let Some(workflow_uri) = binding.workflow_uri.clone() else {
        return operation_error(
            StatusCode::CONFLICT,
            "workflow_binding_incomplete",
            "live invocation requires wf:bindsWorkflow",
        );
    };
    let Some(definition_digest) = binding.definition_digest.clone() else {
        return operation_error(
            StatusCode::CONFLICT,
            "workflow_binding_incomplete",
            "live invocation requires wf:definitionDigest",
        );
    };
    let definition = match resolve_executable_workflow(
        state.app.clone(),
        &graph_id,
        &workflow_uri,
        &binding.workflow_name,
        &definition_digest,
    )
    .await
    {
        Ok(definition) => definition,
        Err(error) => {
            let status = if error.code == "definition_digest_mismatch"
                || error.code == "contested_executable_binding"
            {
                StatusCode::CONFLICT
            } else {
                StatusCode::UNPROCESSABLE_ENTITY
            };
            return operation_error(status, error.code, &error.message);
        }
    };

    let invocation_id = format!("inv-{}", Uuid::new_v4().simple());
    let user_id = caller.principal.user_id();
    let envelope = operation_invocation_envelope(
        &graph_id,
        &operation_id,
        &invocation_id,
        &binding,
        &definition,
        input,
        &user_id,
    );
    invoke_operation_upstream(&state, &headers, &envelope, &caller, &graph_id).await
}

/// Pure contract join between Garden's published executable face and
/// Choreograph's canonical ingress. Keeping this construction testable makes
/// the two independently-owned implementations disagree loudly at review time.
fn operation_invocation_envelope(
    graph_id: &str,
    operation_id: &str,
    invocation_id: &str,
    binding: &crate::emporium::openapi_emit::ApiBinding,
    definition: &crate::workflow_run_mcp::ExecutableWorkflowDefinition,
    input: Value,
    user_id: &str,
) -> Value {
    let execution = binding.execution_extension();
    let mut envelope = json!({
        "schema": "choreograph.invocation-envelope.v1",
        "invocationId": invocation_id,
        "target": {
            "workflowName": definition.workflow_name,
            "workflowUri": definition.workflow_uri,
            "definitionDigest": definition.definition_digest,
        },
        "input": input,
        "actorRef": actor_ref(user_id),
        "graph": { "ownerPrincipal": GRAPH_OWNER, "graphId": graph_id },
        "cause": {
            "kind": "http",
            "ref": format!("garden-operation:{operation_id}"),
        },
        // Transport extension: Choreograph verifies the source bytes against the
        // target digest. Program-controlled source executes only in its sandbox.
        "workflow_binding": {
            "name": binding.workflow_name,
            "workflowUri": definition.workflow_uri,
            "scriptSha256": definition.definition_digest,
            "executor": binding.executor,
            "execution": execution,
            "source": definition.source,
        },
    });
    if let Some(schema) = &binding.input_schema {
        envelope["workflow_binding"]["inputSchema"] = json!(schema);
    }
    if let Some(schema) = &binding.output_schema {
        envelope["workflow_binding"]["outputSchema"] = json!(schema);
    }
    envelope
}

async fn invoke_operation_upstream(
    state: &Arc<LoopbackState>,
    headers: &HeaderMap,
    envelope: &Value,
    caller: &LoopbackCaller,
    graph_id: &str,
) -> Response {
    let target = match state.services.target(CHOREOGRAPH_SERVICE_ID) {
        Ok(target) => target,
        Err(error) => {
            return operation_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "service_unavailable",
                &error,
            )
        }
    };
    let client = match local_service_client() {
        Ok(client) => client,
        Err(error) => {
            return operation_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "service_client_failed",
                &error,
            )
        }
    };
    let url = build_upstream_url(&target.base_url, "/api/invocations", None);
    let mut request = client
        .post(url)
        .header("Accept", "application/json")
        .header("X-Internal-Service", target.internal_secret)
        .header("X-User-ID", caller.principal.user_id())
        .header(CALLER_ROLE_HEADER, graph_role(caller))
        .header(GRAPH_ID_HEADER, graph_id)
        .header(GRAPH_OWNER_HEADER, GRAPH_OWNER)
        .header(GRAPH_ROLE_HEADER, graph_role(caller))
        .json(envelope);
    if let Some(prefer) = headers.get("prefer") {
        request = request.header("prefer", prefer);
    }
    let upstream = match request.send().await {
        Ok(upstream) => upstream,
        Err(error) => {
            return operation_error(
                StatusCode::BAD_GATEWAY,
                "invocation_transport_failed",
                &error.to_string(),
            )
        }
    };
    let status =
        StatusCode::from_u16(upstream.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let bytes = match upstream.bytes().await {
        Ok(bytes) => bytes,
        Err(error) => {
            return operation_error(
                StatusCode::BAD_GATEWAY,
                "invocation_transport_failed",
                &format!("read Choreograph response: {error}"),
            )
        }
    };
    if status != StatusCode::OK {
        return Response::builder()
            .status(status)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(bytes))
            .unwrap_or_else(|_| {
                operation_error(
                    StatusCode::BAD_GATEWAY,
                    "invocation_transport_failed",
                    "build upstream response",
                )
            });
    }
    let response = match serde_json::from_slice::<Value>(&bytes) {
        Ok(response) => response,
        Err(error) => {
            return operation_error(
                StatusCode::BAD_GATEWAY,
                "invalid_invocation_receipt",
                &format!("Choreograph returned invalid JSON: {error}"),
            )
        }
    };
    if response.pointer("/receipt/status").and_then(Value::as_str) == Some("failed") {
        return operation_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            response
                .pointer("/receipt/error/code")
                .and_then(Value::as_str)
                .unwrap_or("execution_error"),
            response
                .pointer("/receipt/error/message")
                .and_then(Value::as_str)
                .unwrap_or("workflow execution failed"),
        );
    }
    let Some(result) = response.get("result") else {
        return operation_error(
            StatusCode::BAD_GATEWAY,
            "invalid_invocation_receipt",
            "synchronous Choreograph response omitted result",
        );
    };
    let serialized = match serde_json::to_vec(result) {
        Ok(serialized) => serialized,
        Err(error) => {
            return operation_error(
                StatusCode::BAD_GATEWAY,
                "invalid_invocation_receipt",
                &format!("serialize operation result: {error}"),
            )
        }
    };
    let mut builder = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json");
    if let Some(run_id) = response.get("runId").and_then(Value::as_str) {
        builder = builder.header("x-sophia-run-id", run_id);
    }
    if let Some(invocation_id) = response
        .pointer("/receipt/invocationId")
        .and_then(Value::as_str)
    {
        builder = builder.header("x-sophia-invocation-id", invocation_id);
    }
    builder.body(Body::from(serialized)).unwrap_or_else(|_| {
        operation_error(
            StatusCode::BAD_GATEWAY,
            "invalid_invocation_receipt",
            "build operation response",
        )
    })
}

fn operation_error(status: StatusCode, code: &str, message: &str) -> Response {
    let body = json!({ "error": { "code": code, "message": message } });
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap_or_else(|_| loopback_error(StatusCode::INTERNAL_SERVER_ERROR, message))
}

async fn loopback_kg_ultra_intuition(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    method: Method,
    OriginalUri(uri): OriginalUri,
    body: Bytes,
) -> Response {
    proxy_service_path(
        state,
        headers,
        method,
        uri.query(),
        KG_ULTRA_SERVICE_ID,
        "/api/intuition",
        body,
    )
    .await
}

async fn loopback_kg_ultra_link_predictions(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    method: Method,
    OriginalUri(uri): OriginalUri,
    body: Bytes,
) -> Response {
    proxy_service_path(
        state,
        headers,
        method,
        uri.query(),
        KG_ULTRA_SERVICE_ID,
        "/api/rank/link-predictions",
        body,
    )
    .await
}

async fn loopback_kg_ultra_graph_refresh(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    method: Method,
    OriginalUri(uri): OriginalUri,
    Path(graph_id): Path<String>,
    body: Bytes,
) -> Response {
    let upstream_path = format!("/api/graphs/{graph_id}/refresh");
    proxy_service_path(
        state,
        headers,
        method,
        uri.query(),
        KG_ULTRA_SERVICE_ID,
        &upstream_path,
        body,
    )
    .await
}

async fn loopback_kg_ultra_graph_status(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    method: Method,
    OriginalUri(uri): OriginalUri,
    Path(graph_id): Path<String>,
    body: Bytes,
) -> Response {
    let upstream_path = format!("/api/graphs/{graph_id}/status");
    proxy_service_path(
        state,
        headers,
        method,
        uri.query(),
        KG_ULTRA_SERVICE_ID,
        &upstream_path,
        body,
    )
    .await
}

fn start_service(
    services: &Arc<LocalServiceHost>,
    state: &Arc<LoopbackState>,
    service_id: &str,
) -> Response {
    match services.start(service_id, &state.app, &state.manifest) {
        Ok(status) => loopback_result(Ok(status)),
        Err(error) if error.contains("unsupported local service") => {
            loopback_error(StatusCode::NOT_FOUND, &error)
        }
        Err(error) => loopback_error(StatusCode::BAD_REQUEST, &error),
    }
}

async fn attach_health(
    state: &Arc<LoopbackState>,
    service_id: &str,
    status: &mut LocalServiceStatus,
) {
    if !matches!(status.state, LocalServiceState::Running) {
        return;
    }
    let target = match state.services.target(service_id) {
        Ok(target) => target,
        Err(error) => {
            status.health = Some(LocalServiceHealth {
                ok: false,
                status_code: None,
                error: Some(error),
            });
            return;
        }
    };
    let client = match local_service_client() {
        Ok(client) => client,
        Err(error) => {
            status.health = Some(LocalServiceHealth {
                ok: false,
                status_code: None,
                error: Some(error),
            });
            return;
        }
    };
    let url = build_upstream_url(&target.base_url, &status.health_path, None);
    status.health = Some(match client.get(url).send().await {
        Ok(response) => LocalServiceHealth {
            ok: response.status().is_success(),
            status_code: Some(response.status().as_u16()),
            error: None,
        },
        Err(error) => LocalServiceHealth {
            ok: false,
            status_code: None,
            error: Some(error.to_string()),
        },
    });
}

async fn proxy_service_path(
    state: Arc<LoopbackState>,
    headers: HeaderMap,
    method: Method,
    query: Option<&str>,
    service_id: &str,
    upstream_path: &str,
    body: Bytes,
) -> Response {
    let caller = match authorized_loopback_caller(&headers, &state, &[SERVICES_PROXY_SCOPE]) {
        Ok(caller) => caller,
        Err(response) => return response,
    };
    if let Err(why) = check_upstream_path(upstream_path) {
        return loopback_error(StatusCode::BAD_REQUEST, why);
    }
    // A public-mode visitor or account reaches only the services public.json
    // lists, and on a graph-scoped path only a public graph.
    if caller.is_public() {
        let public = state.self_host.public();
        if !public.is_some_and(|public| public.is_public_service(service_id)) {
            return loopback_error_coded(
                StatusCode::FORBIDDEN,
                "this service is not open to visitors",
                Some("public_read_only"),
            );
        }
        if let Ok(Some((graph_id, _))) = graph_scoped_path(upstream_path) {
            if !public.is_some_and(|public| public.is_public_graph(&graph_id)) {
                return crate::public_mode::graph_not_public();
            }
        }
    }
    let admin = caller.holds(SERVICES_MANAGE_SCOPE);
    if !admin {
        let access = state.services.member_access(service_id, &state.app);
        if let Err(why) = member_door(service_id, access, &method, upstream_path) {
            return loopback_error(StatusCode::FORBIDDEN, why);
        }
    }
    let user_id = caller.principal.user_id();
    let (body, graph_id, upstream_path) = if service_id == CHOREOGRAPH_SERVICE_ID {
        if method == Method::POST && upstream_path == CHOREOGRAPH_INVOCATIONS_PATH {
            match bind_invocation(&body, &user_id, admin) {
                Ok((body, graph_id)) => (body, graph_id, upstream_path.to_string()),
                Err(response) => return response,
            }
        } else {
            (body, None, upstream_path.to_string())
        }
    } else {
        // Any other service learns its graph from the path, as the gateway
        // binds `/g/{graph_id}/{rest}` for a cell: gardend takes the graph off
        // the path, mints it, and forwards the rest.
        match graph_scoped_path(upstream_path) {
            Ok(Some((graph_id, rest))) => (body, Some(graph_id), rest),
            Ok(None) => (body, None, upstream_path.to_string()),
            Err(why) => return loopback_error(StatusCode::BAD_REQUEST, why),
        }
    };
    let upstream_path = upstream_path.as_str();
    let target = match state.services.target(service_id) {
        Ok(target) => target,
        Err(error) => {
            return match state.services.unavailable_reason(service_id) {
                // The computer lacks what the service needs (Node.js): say so,
                // in the words /self-host.json uses, with a code a client can
                // tell from "not running yet".
                Some(reason) => loopback_error_coded(
                    StatusCode::SERVICE_UNAVAILABLE,
                    &reason,
                    Some(SERVICE_UNAVAILABLE_CODE),
                ),
                None => loopback_error(StatusCode::SERVICE_UNAVAILABLE, &error),
            };
        }
    };
    let url = build_upstream_url(&target.base_url, upstream_path, query);
    let client = match local_service_client() {
        Ok(client) => client,
        Err(error) => return loopback_error(StatusCode::INTERNAL_SERVER_ERROR, &error),
    };
    let mut request = client.request(method, &url);
    for (name, value) in headers.iter() {
        if should_forward_header(name) {
            request = request.header(name, value);
        }
    }
    request = request
        .header("X-Internal-Service", target.internal_secret)
        .header("X-User-ID", &user_id)
        .header(CALLER_ROLE_HEADER, graph_role(&caller));
    // What kind of public caller this is, and an account's email as its ID
    // token says it (a service grants an account's credit on a verified one).
    // A public caller reads every public graph: the service may let one
    // conversation read them all (its delegation does, `public_mode`).
    if caller.is_public() {
        if let Some(public) = state.self_host.public() {
            let graphs: Vec<&str> = public.graphs().iter().map(String::as_str).collect();
            request = request.header(PUBLIC_GRAPHS_HEADER, graphs.join(","));
        }
    }
    match &caller.principal {
        LoopbackPrincipal::Visitor { .. } => {
            request = request.header(CALLER_KIND_HEADER, "anonymous");
        }
        LoopbackPrincipal::Account {
            email,
            email_verified,
            ..
        } => {
            request = request
                .header(CALLER_KIND_HEADER, "account")
                .header(ACCOUNT_EMAIL_VERIFIED_HEADER, if *email_verified { "true" } else { "false" });
            if let Some(email) = email {
                request = request.header(ACCOUNT_EMAIL_HEADER, email);
            }
        }
        _ => {}
    }
    // The caller's own model key, to a service that takes one, and to no other.
    if let Some(key) = headers.get(crate::public_mode::MODEL_KEY_HEADER) {
        if state.services.accepts_model_key(service_id, &state.app) {
            request = request.header(crate::public_mode::MODEL_KEY_HEADER, key);
        }
    }
    // The graph and the caller's role on it, as gardend authenticated them:
    // minted only where the request names its graph (Choreograph refuses a
    // role without the graph it was minted for).
    if let Some(graph_id) = &graph_id {
        request = request
            .header(GRAPH_ID_HEADER, graph_id)
            .header(GRAPH_OWNER_HEADER, GRAPH_OWNER)
            .header(GRAPH_ROLE_HEADER, graph_role(&caller));
        // A service that asked for it (`"delegateCaller": true`) may act for
        // this caller on this graph, through gardend's `/mcp`, for an hour.
        if state.services.delegates_caller(service_id, &state.app) {
            let role = graph_role(&caller);
            let delegation = Delegation {
                service_id: service_id.to_string(),
                user_id: user_id.clone(),
                graph_id: graph_id.clone(),
                role: role.to_string(),
                scopes: delegated_scopes(&caller, role),
                public: caller.is_public(),
            };
            if let Some(token) = state.services.delegate(delegation) {
                request = request.header(DELEGATED_TOKEN_HEADER, token);
            }
        }
    }
    match request.body(body).send().await {
        Ok(response) => upstream_response(response),
        Err(error) => loopback_error(
            StatusCode::BAD_GATEWAY,
            &format!("local service proxy failed: {error}"),
        ),
    }
}

/// The caller's role on a graph, from its grant, as Choreograph reads it
/// (`x-sophia-graph-role`): an admin (the master key, `session-all`, a listed
/// Access admin) is the owner; anyone who may write documents (a member) an
/// editor; anyone else a viewer.
fn graph_role(caller: &LoopbackCaller) -> &'static str {
    if caller.holds(SERVICES_MANAGE_SCOPE) {
        "owner"
    } else if caller.holds("documents.write.crdt") {
        "editor"
    } else {
        "viewer"
    }
}

/// What a delegation token may do for `caller` with `role` on its graph: a
/// reader's scopes for a viewer, a member's for an editor or owner (reads, the
/// authoring writes; no deletes, RDF loads or updates, administration), never
/// a scope the caller lacks and never the service proxy. The MCP entry scopes
/// (`mcp.tools.read`, `mcp.tools.call`) are always there: they open the door,
/// each tool still checks its own scopes, so a viewer's chat reads with the
/// viewer's read scopes and writes nothing.
fn delegated_scopes(caller: &LoopbackCaller, role: &str) -> Vec<String> {
    const MCP_ENTRY_SCOPES: [&str; 2] = ["mcp.tools.read", "mcp.tools.call"];
    let profile = if role == "viewer" {
        "read-only"
    } else {
        MEMBER_GRANT_PROFILE
    };
    let mut scopes: Vec<String> = grant_profile_scopes(profile)
        .into_iter()
        .filter(|scope| {
            !scope.starts_with("services.")
                && !MCP_ENTRY_SCOPES.contains(&scope.as_str())
                && caller.holds(scope)
        })
        .collect();
    scopes.extend(MCP_ENTRY_SCOPES.iter().map(|scope| scope.to_string()));
    scopes
}

/// A graph-scoped service path, `/g/{graph_id}/{rest...}`: the graph and the
/// path the service sees (`/{rest...}`, `/` when there is none). Any other path
/// is not graph-scoped (`None`). A path that starts with `/g` but names no
/// graph is refused. On a self-host every graph belongs to the profile owner
/// and the caller's own grant reaches every graph, so this binds a request to
/// a graph; it does not widen what the caller may reach. The service believes
/// the minted `x-graph-id`, never a graph in its request body.
fn graph_scoped_path(path: &str) -> Result<Option<(String, String)>, &'static str> {
    let mut segments = path.trim_start_matches('/').splitn(3, '/');
    if segments.next() != Some("g") {
        return Ok(None);
    }
    let graph_id = segments.next().unwrap_or_default();
    if !is_graph_id(graph_id) {
        return Err("a /g/ path must name a graph: /g/{graph_id}/...");
    }
    let rest = format!("/{}", segments.next().unwrap_or_default());
    Ok(Some((graph_id.to_string(), rest)))
}

fn actor_ref(user_id: &str) -> String {
    if user_id.starts_with("user:") {
        user_id.to_string()
    } else {
        format!("user:{user_id}")
    }
}

/// A path the proxy may send upstream: no dot segments and nothing a URL
/// parser would read as one, or as the start of a query or fragment, after
/// the router has percent-decoded it (so a door check and the request that
/// leaves agree on the path).
fn check_upstream_path(path: &str) -> Result<(), &'static str> {
    if path
        .chars()
        .any(|ch| ch.is_control() || ch.is_whitespace() || matches!(ch, '%' | '?' | '#' | '\\'))
    {
        return Err("the service path holds a character the proxy does not forward");
    }
    if path
        .split('/')
        .any(|segment| segment == "." || segment == "..")
    {
        return Err("the service path holds a dot segment");
    }
    Ok(())
}

/// What a member (a caller without `services.manage`) may do through the
/// proxy. Choreograph: its workspace chat, the same surface the hosted gateway
/// opens to a person in the chat panel; starting any other workflow,
/// notebooks, schedules, billing, kernels and the browser stay with admins,
/// because a local Choreograph runs bodies without a sandbox (local-native).
/// Any other service: everything when its `service.json` says
/// `"memberAccess": "all"` (it decides by the minted caller role), else
/// nothing.
fn member_door(
    service_id: &str,
    access: MemberAccess,
    method: &Method,
    path: &str,
) -> Result<(), &'static str> {
    const REFUSED: &str = "members may use chat only; this needs an admin (services.manage)";
    if service_id != CHOREOGRAPH_SERVICE_ID {
        return match access {
            MemberAccess::All => Ok(()),
            MemberAccess::None => Err("this service is for admins (services.manage)"),
        };
    }
    let segments: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    let allowed = match (method, segments.as_slice()) {
        (&Method::GET | &Method::HEAD, ["health"]) => true,
        (&Method::GET | &Method::HEAD, ["api", "workflows"]) => true,
        (&Method::GET | &Method::HEAD, ["api", "workflows", "runs"]) => true,
        (&Method::GET | &Method::HEAD, ["api", "workflows", "runs", run]) => !run.is_empty(),
        (&Method::GET | &Method::HEAD, ["api", "workflows", "runs", run, "events"]) => {
            !run.is_empty()
        }
        (&Method::POST, ["api", "invocations"]) => true,
        (_, ["api", "agent-sessions", session, ..]) => !session.is_empty(),
        (_, ["api", "chat-seeds", ..]) => true,
        _ => false,
    };
    if allowed {
        Ok(())
    } else {
        Err(REFUSED)
    }
}

/// Bind a canonical invocation as the gateway does
/// (`force_canonical_invocation_binding`): `actorRef` is the authenticated
/// caller, and `graph` is `{ownerPrincipal: GRAPH_OWNER, graphId}`, whatever
/// the page sent (Choreograph's workspace chat needs the owner scope). A
/// member may start the workspace chat and nothing else. Returns the body to
/// forward and the graph to mint for it.
fn bind_invocation(
    body: &[u8],
    user_id: &str,
    admin: bool,
) -> Result<(Bytes, Option<String>), Response> {
    let mut value: Value = serde_json::from_slice(body).map_err(|error| {
        loopback_error(
            StatusCode::BAD_REQUEST,
            &format!("invocation body must be JSON: {error}"),
        )
    })?;
    let Some(envelope) = value.as_object_mut() else {
        return Err(loopback_error(
            StatusCode::BAD_REQUEST,
            "invocation body must be a JSON object",
        ));
    };
    if !admin && !starts_only_workspace_chat(envelope) {
        return Err(loopback_error(
            StatusCode::FORBIDDEN,
            "members may start the workspace chat only; this invocation needs an admin (services.manage)",
        ));
    }
    let graph_id = match envelope.get("graph") {
        None | Some(Value::Null) => None,
        Some(Value::Object(graph)) => match graph.get("graphId").and_then(Value::as_str) {
            Some(graph_id) if is_graph_id(graph_id) => Some(graph_id.to_string()),
            _ => {
                return Err(loopback_error(
                    StatusCode::BAD_REQUEST,
                    "graph.graphId must name a graph",
                ))
            }
        },
        Some(_) => {
            return Err(loopback_error(
                StatusCode::BAD_REQUEST,
                "graph must be an object",
            ))
        }
    };
    envelope.insert("actorRef".to_string(), Value::String(actor_ref(user_id)));
    match &graph_id {
        Some(graph_id) => {
            envelope.insert(
                "graph".to_string(),
                json!({ "ownerPrincipal": GRAPH_OWNER, "graphId": graph_id }),
            );
        }
        None => {
            envelope.remove("graph");
        }
    }
    let bytes = serde_json::to_vec(&value).map_err(|error| {
        loopback_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("re-serialize the invocation: {error}"),
        )
    })?;
    Ok((Bytes::from(bytes), graph_id))
}

fn is_graph_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | ':' | '-'))
}

/// An envelope that starts `workspace-chat-v1` and carries nothing that could
/// make Choreograph run another program: only the keys the chat panel posts,
/// and a target that is only a workflow name, URI and digest (the gateway's
/// Viewer door, `starts_only_workspace_chat`).
fn starts_only_workspace_chat(envelope: &serde_json::Map<String, Value>) -> bool {
    const ENVELOPE_KEYS: [&str; 8] = [
        "schema",
        "invocationId",
        "target",
        "input",
        "actorRef",
        "graph",
        "cause",
        "idempotencyKey",
    ];
    if envelope
        .keys()
        .any(|key| !ENVELOPE_KEYS.contains(&key.as_str()))
    {
        return false;
    }
    if envelope.get("schema").and_then(Value::as_str) != Some(INVOCATION_ENVELOPE_SCHEMA) {
        return false;
    }
    let Some(target) = envelope.get("target").and_then(Value::as_object) else {
        return false;
    };
    if target.keys().any(|key| {
        !matches!(
            key.as_str(),
            "workflowName" | "workflowUri" | "definitionDigest"
        )
    }) {
        return false;
    }
    target.get("workflowName").and_then(Value::as_str) == Some(WORKSPACE_CHAT_WORKFLOW)
}

fn build_upstream_url(base_url: &str, path: &str, query: Option<&str>) -> String {
    let base = base_url.trim_end_matches('/');
    let path = if path.starts_with('/') {
        path.to_string()
    } else {
        format!("/{path}")
    };
    match query {
        Some(query) if !query.is_empty() => format!("{base}{path}?{query}"),
        _ => format!("{base}{path}"),
    }
}

/// Request headers the proxy passes on. Never the caller's credentials (a
/// bearer, the Access token or cookie), hop-by-hop headers, the length (the
/// body may be rewritten), nor any header by which Choreograph learns who
/// calls or on which graph and with what role (`x-user-id`, `x-graph-*`,
/// `x-sophia-*`, `x-internal-*`, `x-pn-*`): gardend mints those itself. That
/// includes a delegation token (`x-sophia-delegated-token`): a caller cannot
/// hand a service one.
fn should_forward_header(name: &HeaderName) -> bool {
    let name = name.as_str();
    if [
        "x-graph-",
        "x-sophia-",
        "x-internal-",
        "x-user-",
        "x-pn-",
        "cf-access-",
        "proxy-",
    ]
    .iter()
    .any(|prefix| name.starts_with(prefix))
    {
        return false;
    }
    !matches!(
        name,
        "authorization"
            | "x-model-key"
            | "x-visitor-id"
            | "cookie"
            | "connection"
            | "content-length"
            | "host"
            | "keep-alive"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
    )
}

fn local_service_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|error| format!("build local service client: {error}"))
}

/// The service's answer, streamed through as it arrives (server-sent events
/// included): nothing is buffered, and a client that goes away closes the
/// upstream request.
fn upstream_response(upstream: reqwest::Response) -> Response {
    let status =
        StatusCode::from_u16(upstream.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let mut builder = Response::builder().status(status);
    for (name, value) in upstream.headers().iter() {
        if should_return_header(name) {
            builder = builder.header(name, value);
        }
    }
    let chunks = futures_util::stream::unfold(Some(upstream), |upstream| async move {
        let mut upstream = upstream?;
        match upstream.chunk().await {
            Ok(Some(bytes)) => Some((Ok::<Bytes, std::io::Error>(bytes), Some(upstream))),
            Ok(None) => None,
            Err(error) => Some((Err(std::io::Error::other(error.to_string())), None)),
        }
    });
    builder
        .body(Body::from_stream(chunks))
        .unwrap_or_else(|_| loopback_error(StatusCode::BAD_GATEWAY, "build proxy response"))
}

/// Response headers the proxy passes back: not hop-by-hop ones, and not the
/// service's own CORS answer (gardend's CORS layer answers for its origin; a
/// second `Access-Control-Allow-Origin` makes a browser refuse the response).
fn should_return_header(name: &HeaderName) -> bool {
    let name = name.as_str();
    !name.starts_with("access-control-")
        && !matches!(
            name,
            "connection"
                | "keep-alive"
                | "proxy-authenticate"
                | "proxy-authorization"
                | "te"
                | "trailer"
                | "transfer-encoding"
                | "upgrade"
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn operation_face_builds_the_canonical_digest_pinned_invocation() {
        let binding = crate::emporium::openapi_emit::ApiBinding {
            workflow_name: "domain-agent-activity-loop-v1".to_string(),
            workflow_uri: Some("urn:sophia:wf:domain-agent-activity-loop-v1".to_string()),
            definition_digest: Some("abc123".to_string()),
            executor: "gated".to_string(),
            controller: Some("composite".to_string()),
            model_use: Some("agentic".to_string()),
            isolation: Some("sandbox".to_string()),
            effects: Some("capability-bound".to_string()),
            reproducibility: Some("nondeterministic".to_string()),
            runtime_kind: None,
            capabilities: vec!["choreograph.agent".to_string(), "garden.graph".to_string()],
            minimum_role: Some("editor".to_string()),
            dangerous_ops: Some(false),
            may_invoke_children: Some(true),
            input_schema: Some(r#"{"type":"object"}"#.to_string()),
            output_schema: Some(r#"{"type":"object"}"#.to_string()),
        };
        let definition = crate::workflow_run_mcp::ExecutableWorkflowDefinition {
            workflow_name: binding.workflow_name.clone(),
            workflow_uri: binding.workflow_uri.clone().unwrap(),
            definition_digest: binding.definition_digest.clone().unwrap(),
            source: "return { ok: true }".to_string(),
        };

        let envelope = operation_invocation_envelope(
            "phanes",
            "phanes.respond",
            "inv-proof",
            &binding,
            &definition,
            json!({"turn": "one"}),
            "owner",
        );

        assert_eq!(envelope["schema"], "choreograph.invocation-envelope.v1");
        assert_eq!(envelope["target"]["definitionDigest"], "abc123");
        assert_eq!(envelope["workflow_binding"]["scriptSha256"], "abc123");
        assert_eq!(
            envelope["workflow_binding"]["inputSchema"],
            r#"{"type":"object"}"#
        );
        assert_eq!(
            envelope["workflow_binding"]["execution"]["controller"],
            "composite"
        );
        assert_eq!(
            envelope["workflow_binding"]["execution"]["mayInvokeChildren"],
            true
        );
        assert_eq!(envelope["graph"]["graphId"], "phanes");
        assert_eq!(envelope["actorRef"], "user:owner");
        assert_eq!(envelope["graph"]["ownerPrincipal"], GRAPH_OWNER);
        assert_eq!(envelope["cause"]["ref"], "garden-operation:phanes.respond");
        assert!(envelope.get("apiToken").is_none());
    }

    #[test]
    fn workflow_facade_url_maps_to_service_target() {
        assert_eq!(
            build_upstream_url(
                "http://127.0.0.1:3456",
                "/api/workflows/runs",
                Some("since=2")
            ),
            "http://127.0.0.1:3456/api/workflows/runs?since=2"
        );
    }

    #[test]
    fn kg_ultra_facade_url_maps_to_service_target() {
        assert_eq!(
            build_upstream_url(
                "http://127.0.0.1:4567",
                "/api/rank/link-predictions",
                Some("limit=5")
            ),
            "http://127.0.0.1:4567/api/rank/link-predictions?limit=5"
        );
    }

    #[test]
    fn proxy_drops_boundary_auth_and_identity_headers() {
        for refused in [
            "authorization",
            "cookie",
            "x-internal-service",
            "x-user-id",
            "x-graph-id",
            "x-graph-owner",
            "x-sophia-graph-role",
            "x-sophia-human-delegation",
            "x-sophia-delegated-token",
            "x-sophia-trigger-adapter",
            "x-pn-on-behalf-of",
            "cf-access-jwt-assertion",
            "cf-access-authenticated-user-email",
            "content-length",
        ] {
            assert!(
                !should_forward_header(&HeaderName::from_static(refused)),
                "{refused} must not pass"
            );
        }
        for passed in ["accept", "content-type", "prefer", "last-event-id"] {
            assert!(
                should_forward_header(&HeaderName::from_static(passed)),
                "{passed}"
            );
        }
        assert!(!should_return_header(&HeaderName::from_static(
            "access-control-allow-origin"
        )));
        assert!(should_return_header(&HeaderName::from_static(
            "content-type"
        )));
    }

    #[test]
    fn a_graph_scoped_path_names_its_graph_and_keeps_the_rest() {
        let scoped = |path: &str| graph_scoped_path(path);
        assert_eq!(
            scoped("/g/atopile-led-badge/sessions/s-1/events"),
            Ok(Some((
                "atopile-led-badge".into(),
                "/sessions/s-1/events".into()
            )))
        );
        assert_eq!(
            scoped("/g/notes/sessions"),
            Ok(Some(("notes".into(), "/sessions".into())))
        );
        assert_eq!(scoped("/g/notes"), Ok(Some(("notes".into(), "/".into()))));
        assert_eq!(scoped("/g/notes/"), Ok(Some(("notes".into(), "/".into()))));
        assert_eq!(scoped("/sessions/s-1"), Ok(None));
        assert_eq!(scoped("/health"), Ok(None));
        assert_eq!(scoped("/"), Ok(None));
        assert_eq!(scoped("/graphs/notes"), Ok(None));
        for refused in ["/g", "/g/", "/g//sessions", "/g/not a graph/x", "/g/a$b/x"] {
            assert!(scoped(refused).is_err(), "{refused}");
        }
    }

    #[test]
    fn the_member_door_opens_chat_and_nothing_else() {
        let door = |method: Method, path: &str| {
            member_door(CHOREOGRAPH_SERVICE_ID, MemberAccess::All, &method, path)
        };
        for (method, path) in [
            (Method::GET, "/health"),
            (Method::GET, "/api/workflows"),
            (Method::GET, "/api/workflows/runs"),
            (Method::GET, "/api/workflows/runs/wfr-1"),
            (Method::GET, "/api/workflows/runs/wfr-1/events"),
            (Method::POST, "/api/invocations"),
            (Method::GET, "/api/agent-sessions/ags-1"),
            (Method::GET, "/api/agent-sessions/ags-1/events"),
            (Method::POST, "/api/agent-sessions/ags-1/driver"),
            (Method::POST, "/api/agent-sessions/ags-1/commands"),
            (Method::POST, "/api/chat-seeds"),
        ] {
            assert!(door(method.clone(), path).is_ok(), "{method} {path}");
        }
        for (method, path) in [
            (Method::POST, "/api/workflows/run"),
            (Method::POST, "/api/workflows/runs/wfr-1/pause"),
            (Method::GET, "/api/notebooks"),
            (Method::POST, "/api/sandboxes"),
            (Method::GET, "/api/billing/balance"),
            (Method::GET, "/api/agent-sessions/"),
            (Method::DELETE, "/api/workflows"),
        ] {
            assert!(door(method.clone(), path).is_err(), "{method} {path}");
        }
        assert!(member_door(
            KG_ULTRA_SERVICE_ID,
            MemberAccess::None,
            &Method::GET,
            "/health"
        )
        .is_err());
        assert!(member_door("vehicle", MemberAccess::None, &Method::GET, "/health").is_err());
        assert!(member_door(
            "vehicle",
            MemberAccess::All,
            &Method::POST,
            "/sessions/s-1/commands"
        )
        .is_ok());
        assert!(check_upstream_path("/api/agent-sessions/x/../../admin").is_err());
        assert!(check_upstream_path("/api/agent-sessions/%2e%2e").is_err());
        assert!(check_upstream_path("/api/chat-seeds?x").is_err());
        assert!(check_upstream_path("/api/agent-sessions/ags-1/events").is_ok());
    }

    #[test]
    fn invocations_are_bound_to_the_caller_and_members_start_only_the_chat() {
        let chat = json!({
            "schema": INVOCATION_ENVELOPE_SCHEMA,
            "invocationId": "inv-1",
            "target": { "workflowName": WORKSPACE_CHAT_WORKFLOW, "definitionDigest": "abc" },
            "input": { "text": "hola" },
            "actorRef": "user:attacker",
            "graph": { "ownerPrincipal": "user:attacker", "graphId": "notes" },
        });
        let (bytes, graph) =
            bind_invocation(&serde_json::to_vec(&chat).unwrap(), "token:t-1", false).unwrap();
        let bound: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(graph.as_deref(), Some("notes"));
        assert_eq!(bound["actorRef"], "user:token:t-1");
        assert_eq!(
            bound["graph"]["ownerPrincipal"], "user:owner",
            "the profile owner owns the graph; the caller acts on it"
        );
        assert_eq!(bound["graph"]["graphId"], "notes");
        assert_eq!(bound["input"]["text"], "hola");

        let mut other = chat.clone();
        other["target"]["workflowName"] = json!("build-and-deploy");
        assert_eq!(
            bind_invocation(&serde_json::to_vec(&other).unwrap(), "token:t-1", false)
                .unwrap_err()
                .status(),
            StatusCode::FORBIDDEN
        );
        assert!(bind_invocation(&serde_json::to_vec(&other).unwrap(), "owner", true).is_ok());
        let mut smuggled = chat.clone();
        smuggled["workflow_binding"] = json!({ "source": "process.exit(1)" });
        assert_eq!(
            bind_invocation(&serde_json::to_vec(&smuggled).unwrap(), "token:t-1", false)
                .unwrap_err()
                .status(),
            StatusCode::FORBIDDEN
        );
        let mut bad_graph = chat.clone();
        bad_graph["graph"]["graphId"] = json!("../etc");
        assert_eq!(
            bind_invocation(&serde_json::to_vec(&bad_graph).unwrap(), "owner", true)
                .unwrap_err()
                .status(),
            StatusCode::BAD_REQUEST
        );
        let mut no_graph = chat;
        no_graph.as_object_mut().unwrap().remove("graph");
        let (bytes, graph) =
            bind_invocation(&serde_json::to_vec(&no_graph).unwrap(), "owner", true).unwrap();
        assert_eq!(graph, None);
        assert_eq!(
            serde_json::from_slice::<Value>(&bytes).unwrap()["actorRef"],
            "user:owner"
        );
        assert_eq!(
            bind_invocation(b"[1]", "owner", true).unwrap_err().status(),
            StatusCode::BAD_REQUEST
        );
    }
}

/// The proxy end to end: the real router in front of a real child process
/// (the test binary re-run as a tiny service, `local_service_host::child`).
#[cfg(all(test, feature = "headless"))]
mod proxy_tests {
    use super::*;
    use crate::cell_graph_boundary::CellGraphBoundary;
    use crate::local_service_host::child::{
        fast_timing, wait_healthy, wait_service_healthy, write_manifest, write_service_manifest,
        RELEASE_ENV,
    };
    use crate::loopback_client_token_service::create_client_token;
    use crate::loopback_client_token_types::CreateLoopbackClientTokenInput;
    use crate::loopback_self_host::{test_support::*, SelfHost, SelfHostSettings};
    use crate::loopback_state::LoopbackManifest;
    use futures_util::StreamExt;
    use std::path::Path;
    use std::time::Duration;
    use tower::ServiceExt;

    const MASTER: &str = "proxy-test-master-token";
    const PUBLIC: &str = "https://sophia.example.com";

    fn state(
        app: crate::app_runtime::AppHandle,
        dir: &Path,
        self_host: SelfHost,
        cell_graph: CellGraphBoundary,
    ) -> Arc<LoopbackState> {
        let grant = crate::loopback_token_grants::session_all_token_grant();
        Arc::new(LoopbackState {
            app,
            token: MASTER.into(),
            manifest: LoopbackManifest {
                runtime_profile: "test",
                bind_host: "127.0.0.1",
                port: 0,
                api_url: PUBLIC.into(),
                mcp_url: format!("{PUBLIC}/mcp"),
                openapi_url: format!("{PUBLIC}/openapi.json"),
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

    /// Start the child service behind `state` and wait until it answers.
    fn start_child(state: &Arc<LoopbackState>, root: &Path, env: &[(&str, String)]) {
        state.services.pin_manifest(
            CHOREOGRAPH_SERVICE_ID,
            write_manifest(&root.join("svc"), env),
        );
        state.services.set_timing(fast_timing());
        state
            .services
            .set_local_api_url("http://127.0.0.1:9".into());
        state
            .services
            .start(CHOREOGRAPH_SERVICE_ID, &state.app, &state.manifest)
            .unwrap();
        wait_healthy(&state.services);
    }

    fn with_profile(name: &str, test: impl FnOnce(&Path)) {
        let _serial = crate::tauri_runtime::profile_env_serial()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let root = std::env::temp_dir().join(format!("garden-proxy-{name}-{}", Uuid::new_v4()));
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

    async fn call(
        state: &Arc<LoopbackState>,
        method: &str,
        path: &str,
        headers: &[(&str, String)],
        body: Body,
    ) -> Response {
        let mut request = axum::http::Request::builder().method(method).uri(path);
        for (name, value) in headers {
            request = request.header(*name, value.as_str());
        }
        crate::loopback_router::loopback_router(state.clone())
            .oneshot(request.body(body).unwrap())
            .await
            .unwrap()
    }

    async fn json_of(response: Response) -> (StatusCode, Value) {
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 10_000_000)
            .await
            .unwrap();
        let value = serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into()));
        (status, value)
    }

    fn bearer(token: &str) -> (&'static str, String) {
        ("authorization", format!("Bearer {token}"))
    }

    /// What someone might send to pass as someone else: none of it may reach
    /// Choreograph.
    fn forged() -> Vec<(&'static str, String)> {
        vec![
            ("x-user-id", "mallory".into()),
            ("x-sophia-graph-role", "owner".into()),
            ("x-graph-id", "someone-elses-graph".into()),
            ("x-graph-owner", "user:mallory".into()),
            ("x-sophia-human-delegation", "guess".into()),
            ("cookie", "CF_Authorization=stolen".into()),
        ]
    }

    fn chat_envelope(workflow: &str) -> Value {
        json!({
            "schema": INVOCATION_ENVELOPE_SCHEMA,
            "invocationId": format!("inv-{}", Uuid::new_v4().simple()),
            "target": { "workflowName": workflow, "definitionDigest": "sha256:abc" },
            "input": { "text": "hola" },
            "actorRef": "user:mallory",
            "graph": { "ownerPrincipal": "user:mallory", "graphId": "notes" },
        })
    }

    /// A run gardend's own MCP starts (`workflow_run_start`, admins only) is
    /// the profile owner's: gardend calls Choreograph as the identity its
    /// proxy mints for the master key, so the owner reads that run through
    /// `/services/choreograph` (as `default` it was refused with 403).
    #[test]
    fn gardends_workflow_calls_and_the_owner_through_the_proxy_are_one_person() {
        with_profile("workflow-owner", |root| {
            let app = crate::tauri_runtime::build_mock_app_for_tests(true);
            let state = state(
                app,
                root,
                SelfHost::default(),
                CellGraphBoundary::for_test(None),
            );
            start_child(&state, root, &[]);
            crate::app_runtime::async_runtime::block_on(async {
                let direct = crate::workflow_run_mcp::choreograph_json_request(
                    &state,
                    reqwest::Method::POST,
                    "/api/workflows/run",
                    None,
                    Some(json!({ "graph_id": "notes", "workflow_name": "w" })),
                )
                .await
                .unwrap();
                assert!(direct.status.is_success(), "{}", direct.body);
                assert_eq!(direct.body["path"], "/api/workflows/run");
                assert_eq!(direct.body["internalServiceOk"], true);
                let (status, proxied) = json_of(
                    call(
                        &state,
                        "GET",
                        "/services/choreograph/api/workflows/runs/wfr-1",
                        &[bearer(MASTER)],
                        Body::empty(),
                    )
                    .await,
                )
                .await;
                assert_eq!(status, StatusCode::OK, "{proxied}");
                assert_eq!(direct.body["userId"], "owner");
                assert_eq!(proxied["userId"], direct.body["userId"]);
            });
            state.services.shutdown();
        });
    }

    /// Server-sent events pass through as they come: the first event reaches
    /// the caller while the service still holds the second back.
    #[test]
    fn the_proxy_streams_server_sent_events_as_they_come() {
        with_profile("sse", |root| {
            let release = root.join("release");
            let app = crate::tauri_runtime::build_mock_app_for_tests(true);
            let state = state(
                app,
                root,
                SelfHost::default(),
                CellGraphBoundary::for_test(None),
            );
            start_child(
                &state,
                root,
                &[(RELEASE_ENV, release.display().to_string())],
            );
            crate::app_runtime::async_runtime::block_on(async {
                let response = tokio::time::timeout(
                    Duration::from_secs(10),
                    call(
                        &state,
                        "GET",
                        "/services/choreograph/api/agent-sessions/ags-1/events",
                        &[bearer(MASTER), ("accept", "text/event-stream".into())],
                        Body::empty(),
                    ),
                )
                .await
                .expect("the proxy answers before the stream ends: it does not buffer");
                assert_eq!(response.status(), StatusCode::OK);
                assert_eq!(
                    response.headers().get(header::CONTENT_TYPE).unwrap(),
                    "text/event-stream"
                );
                assert!(
                    response
                        .headers()
                        .get("access-control-allow-origin")
                        .is_none(),
                    "the service's own CORS answer is not passed on"
                );
                let mut stream = response.into_body().into_data_stream();
                let first = tokio::time::timeout(Duration::from_secs(10), stream.next())
                    .await
                    .expect("the first event arrives while the second is held back")
                    .unwrap()
                    .unwrap();
                let first = String::from_utf8_lossy(&first).to_string();
                assert!(first.contains("data: one"), "{first}");
                assert!(!first.contains("data: two"), "{first}");
                std::fs::write(&release, b"go").unwrap();
                let mut rest = String::new();
                while let Some(chunk) = tokio::time::timeout(Duration::from_secs(10), stream.next())
                    .await
                    .expect("the stream ends after the second event")
                {
                    rest.push_str(&String::from_utf8_lossy(&chunk.unwrap()));
                }
                assert!(rest.contains("data: two"), "{rest}");
            });
            state.services.shutdown();
        });
    }

    /// Choreograph learns who calls from gardend alone: the master key is
    /// `owner`, a client token `token:<id>`, an Access login its email (or
    /// `access-service:<id>`); a caller's own identity, role, graph and
    /// cookie headers never pass. An invocation is bound to the caller and
    /// its graph, with the role from the caller's grant (a member sending
    /// `x-sophia-graph-role: owner` is an editor). Members chat and do
    /// nothing else; read-only tokens do not reach the service at all.
    #[test]
    fn choreograph_learns_the_caller_role_and_graph_from_gardend_only() {
        with_profile("identity", |root| {
            let key = TestKey::generate("k1");
            let self_host = SelfHost::from_settings(SelfHostSettings {
                cf_access_team_domain: Some(TEAM.into()),
                cf_access_aud: Some(AUD.into()),
                cf_access_admin_emails: Some("vera@example.com".into()),
                public_base_url: Some(PUBLIC.into()),
                ..SelfHostSettings::default()
            })
            .unwrap();
            self_host
                .access()
                .unwrap()
                .install_keys(jwks(&[&key]).as_bytes())
                .unwrap();
            let app = crate::tauri_runtime::build_mock_app_for_tests(true);
            let state = state(app, root, self_host, CellGraphBoundary::for_test(None));
            start_child(&state, root, &[]);
            let issue = |grant: &str| {
                create_client_token(
                    &state.app,
                    CreateLoopbackClientTokenInput {
                        label: Some(format!("{grant} test")),
                        grant_profile_id: Some(grant.into()),
                        scopes: None,
                        expires_in_days: None,
                    },
                )
                .unwrap()
            };
            let member = issue("member");
            let read_only = issue("read-only");
            let vera = key.sign(&person_claims("vera@example.com"));
            let cousin = key.sign(&person_claims("cousin@example.com"));
            let agent = key.sign(&service_claims("6fe0d2a1.access"));

            crate::app_runtime::async_runtime::block_on(async {
                let whoami = |credential: (&'static str, String)| {
                    let state = state.clone();
                    async move {
                        let mut headers = forged();
                        headers.push(credential);
                        json_of(
                            call(
                                &state,
                                "GET",
                                "/services/choreograph/api/workflows",
                                &headers,
                                Body::empty(),
                            )
                            .await,
                        )
                        .await
                    }
                };
                for (credential, user, role) in [
                    (bearer(MASTER), "owner".to_string(), "owner"),
                    (
                        bearer(&member.token),
                        format!("token:{}", member.record.token_id),
                        "editor",
                    ),
                    (
                        ("cf-access-jwt-assertion", vera.clone()),
                        "vera@example.com".into(),
                        "owner",
                    ),
                    (
                        ("cf-access-jwt-assertion", cousin.clone()),
                        "cousin@example.com".into(),
                        "editor",
                    ),
                    (
                        ("cf-access-jwt-assertion", agent.clone()),
                        "access-service:6fe0d2a1.access".into(),
                        "editor",
                    ),
                ] {
                    let (status, echoed) = whoami(credential).await;
                    assert_eq!(status, StatusCode::OK, "{echoed}");
                    assert_eq!(echoed["userId"], user);
                    assert_eq!(echoed["callerRole"], role, "{echoed}");
                    assert_eq!(echoed["internalServiceOk"], true);
                    for stripped in [
                        "graphRole",
                        "graphId",
                        "graphOwner",
                        "humanDelegation",
                        "cookie",
                        "authorization",
                        "cfAccess",
                    ] {
                        assert!(echoed[stripped].is_null(), "{stripped} passed: {echoed}");
                    }
                }
                let (status, _) = whoami(bearer(&read_only.token)).await;
                assert_eq!(
                    status,
                    StatusCode::FORBIDDEN,
                    "read-only cannot reach services"
                );

                let invoke = |credential: (&'static str, String), envelope: Value| {
                    let state = state.clone();
                    async move {
                        let mut headers = forged();
                        headers.push(credential);
                        headers.push(("content-type", "application/json".into()));
                        json_of(
                            call(
                                &state,
                                "POST",
                                "/services/choreograph/api/invocations",
                                &headers,
                                Body::from(serde_json::to_vec(&envelope).unwrap()),
                            )
                            .await,
                        )
                        .await
                    }
                };
                for (credential, actor, role) in [
                    (
                        bearer(&member.token),
                        format!("user:token:{}", member.record.token_id),
                        "editor",
                    ),
                    (
                        ("cf-access-jwt-assertion", cousin.clone()),
                        "user:cousin@example.com".to_string(),
                        "editor",
                    ),
                    (
                        ("cf-access-jwt-assertion", vera.clone()),
                        "user:vera@example.com".to_string(),
                        "owner",
                    ),
                    (bearer(MASTER), "user:owner".to_string(), "owner"),
                ] {
                    let (status, echoed) =
                        invoke(credential, chat_envelope(WORKSPACE_CHAT_WORKFLOW)).await;
                    assert_eq!(status, StatusCode::OK, "{echoed}");
                    assert_eq!(echoed["graphRole"], role, "{echoed}");
                    assert_eq!(echoed["graphId"], "notes");
                    assert_eq!(echoed["graphOwner"], "user:owner");
                    assert_eq!(echoed["body"]["actorRef"], actor);
                    assert_eq!(echoed["body"]["graph"]["ownerPrincipal"], "user:owner");
                    assert_eq!(echoed["body"]["graph"]["graphId"], "notes");
                    assert_eq!(echoed["body"]["input"]["text"], "hola");
                }

                // Members chat, and only chat.
                let (status, said) =
                    invoke(bearer(&member.token), chat_envelope("build-and-deploy")).await;
                assert_eq!(status, StatusCode::FORBIDDEN, "{said}");
                let (status, _) = invoke(bearer(MASTER), chat_envelope("build-and-deploy")).await;
                assert_eq!(status, StatusCode::OK, "an admin starts any workflow");
                for (method, path) in [
                    ("POST", "/services/choreograph/api/workflows/run"),
                    ("POST", "/workflows/runs"),
                    ("GET", "/services/choreograph/api/notebooks"),
                    ("GET", "/services/kg-ultra/health"),
                ] {
                    let response = call(
                        &state,
                        method,
                        path,
                        &[
                            bearer(&member.token),
                            ("content-type", "application/json".into()),
                        ],
                        Body::from("{}"),
                    )
                    .await;
                    assert_eq!(response.status(), StatusCode::FORBIDDEN, "{method} {path}");
                }
                let (status, echoed) = json_of(
                    call(
                        &state,
                        "GET",
                        "/services/choreograph/api/agent-sessions/ags-1",
                        &[
                            bearer(&member.token),
                            ("x-sophia-graph-role", "owner".into()),
                        ],
                        Body::empty(),
                    )
                    .await,
                )
                .await;
                assert_eq!(status, StatusCode::OK, "{echoed}");
                assert!(
                    echoed["graphRole"].is_null(),
                    "no graph named, no role minted"
                );
                let response = call(
                    &state,
                    "GET",
                    "/services/choreograph/api/agent-sessions/x/%2e%2e/%2e%2e/notebooks",
                    &[bearer(&member.token)],
                    Body::empty(),
                )
                .await;
                assert_eq!(response.status(), StatusCode::BAD_REQUEST);

                // The child's own token is a member's, never the master key.
                let (_, echoed) = json_of(
                    call(
                        &state,
                        "GET",
                        "/services/choreograph/api/env",
                        &[bearer(MASTER)],
                        Body::empty(),
                    )
                    .await,
                )
                .await;
                let service_token = echoed["env"]["MNEMOSYNE_MCP_ACCESS_TOKEN"]
                    .as_str()
                    .unwrap()
                    .to_string();
                assert_ne!(service_token, MASTER);
                let mcp = |call_body: Value| {
                    let state = state.clone();
                    let token = service_token.clone();
                    async move {
                        json_of(
                            super::proxy_tests::call(
                                &state,
                                "POST",
                                "/mcp",
                                &[bearer(&token), ("content-type", "application/json".into())],
                                Body::from(serde_json::to_vec(&call_body).unwrap()),
                            )
                            .await,
                        )
                        .await
                    }
                };
                let (status, listed) =
                    mcp(json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" })).await;
                assert_eq!(status, StatusCode::OK, "{listed}");
                let (status, refused) = mcp(json!({
                    "jsonrpc": "2.0", "id": 2, "method": "tools/call",
                    "params": { "name": "sparql_update", "arguments": { "graph_id": "g", "query": "INSERT DATA {}" } },
                }))
                .await;
                assert_eq!(status, StatusCode::OK);
                assert!(
                    refused
                        .to_string()
                        .contains("missing required scope rdf.update"),
                    "{refused}"
                );

                // /self-host.json says chat is there.
                let (status, described) =
                    json_of(call(&state, "GET", "/self-host.json", &[], Body::empty()).await).await;
                assert_eq!(status, StatusCode::OK);
                assert_eq!(
                    described["services"]["choreograph"],
                    json!({ "configured": true, "state": "running", "enabled": true })
                );
            });
            state.services.shutdown();
        });
    }

    /// A service whose `service.json` requires a Node.js this computer lacks
    /// is not started: `/self-host.json` says `unavailable` with the reason,
    /// and the proxy answers 503 `service_unavailable` with the same words
    /// (the chat panel shows them).
    #[cfg(unix)]
    #[test]
    fn a_service_without_its_node_is_unavailable_with_the_reason_everywhere() {
        with_profile("no-node", |root| {
            let ui = root.join("ui");
            std::fs::create_dir_all(&ui).unwrap();
            std::fs::write(ui.join("index.html"), "<!doctype html>").unwrap();
            let self_host = SelfHost::from_settings(SelfHostSettings {
                ui_dir: Some(ui.display().to_string()),
                ..SelfHostSettings::default()
            })
            .unwrap();
            let app = crate::tauri_runtime::build_mock_app_for_tests(true);
            let state = state(app, root, self_host, CellGraphBoundary::for_test(None));
            let bin = root.join("bin");
            crate::local_service_host::child::put_node_on(&bin, "18.20.4");
            let dir = root.join("services");
            write_service_manifest(
                &dir.join("vehicle"),
                "vehicle",
                &[("PATH", bin.display().to_string())],
                json!({ "memberAccess": "all", "requires": { "node": "22.19" }, "displayName": "Chat" }),
            );
            state.services.pin_services_dir(dir);
            state.services.set_timing(fast_timing());
            state.services.set_local_api_url("http://127.0.0.1:9".into());
            assert!(state.services.start("vehicle", &state.app, &state.manifest).is_err());
            let reason = "Chat needs Node.js 22.19 or newer; the node on gardend's PATH is 18.20.4. Put Node.js 22.19 or newer first on gardend's PATH, then restart gardend.";
            crate::app_runtime::async_runtime::block_on(async {
                let (status, described) =
                    json_of(call(&state, "GET", "/self-host.json", &[], Body::empty()).await).await;
                assert_eq!(status, StatusCode::OK);
                assert_eq!(
                    described["services"]["vehicle"],
                    json!({ "configured": true, "state": "unavailable", "enabled": true, "reason": reason })
                );
                let (status, said) = json_of(
                    call(
                        &state,
                        "POST",
                        "/services/vehicle/g/notes/sessions",
                        &[bearer(MASTER), ("content-type", "application/json".into())],
                        Body::from("{}"),
                    )
                    .await,
                )
                .await;
                assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{said}");
                assert_eq!(said["code"], SERVICE_UNAVAILABLE_CODE);
                assert_eq!(said["error"], reason);
            });
            state.services.shutdown();
        });
    }

    /// In a hosted cell the service routes stay denied, even for the master key.
    #[test]
    fn a_hosted_cell_denies_the_service_proxy() {
        with_profile("cell", |root| {
            let app = crate::tauri_runtime::build_mock_app_for_tests(true);
            let state = state(
                app,
                root,
                SelfHost::default(),
                CellGraphBoundary::for_test(Some("notes")),
            );
            crate::app_runtime::async_runtime::block_on(async {
                for path in [
                    "/services/choreograph/health",
                    "/services/choreograph/api/invocations",
                ] {
                    let (status, said) =
                        json_of(call(&state, "GET", path, &[bearer(MASTER)], Body::empty()).await)
                            .await;
                    // Refused before any proxying: without the gateway's lease
                    // the cell answers 401; with one, the closed policy's 403
                    // (`restProfileDeniedAnyMethod`, checked by the policy tests).
                    assert!(
                        status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN,
                        "{path}: {status} {said}"
                    );
                    let said = said.to_string();
                    assert!(
                        said.contains("lease") || said.contains("single-graph"),
                        "{said}"
                    );
                }
            });
        });
    }

    /// Any local service, not only Choreograph: mounted at `/services/<id>`,
    /// told who calls and with what role by the same minted headers, and
    /// reachable by members only when its manifest says `"memberAccess": "all"`.
    #[test]
    fn any_service_gets_the_minted_headers_and_its_declared_member_access() {
        with_profile("generic", |root| {
            let app = crate::tauri_runtime::build_mock_app_for_tests(true);
            let state = state(
                app,
                root,
                SelfHost::default(),
                CellGraphBoundary::for_test(None),
            );
            let dir = root.join("services");
            write_service_manifest(
                &dir.join("vehicle"),
                "vehicle",
                &[],
                json!({ "memberAccess": "all" }),
            );
            write_service_manifest(&dir.join("tools"), "tools", &[], json!({}));
            state.services.pin_services_dir(dir);
            state.services.set_timing(fast_timing());
            state
                .services
                .set_local_api_url("http://127.0.0.1:9".into());
            for service_id in ["vehicle", "tools"] {
                state
                    .services
                    .start(service_id, &state.app, &state.manifest)
                    .unwrap();
                wait_service_healthy(&state.services, service_id);
            }
            let member = create_client_token(
                &state.app,
                CreateLoopbackClientTokenInput {
                    label: Some("member".into()),
                    grant_profile_id: Some("member".into()),
                    scopes: None,
                    expires_in_days: None,
                },
            )
            .unwrap();
            crate::app_runtime::async_runtime::block_on(async {
                let get = |path: &'static str, credential: (&'static str, String)| {
                    let state = state.clone();
                    async move {
                        let mut headers = forged();
                        headers.push(credential);
                        json_of(call(&state, "GET", path, &headers, Body::empty()).await).await
                    }
                };
                let (status, echoed) =
                    get("/services/vehicle/sessions/s-1", bearer(&member.token)).await;
                assert_eq!(status, StatusCode::OK, "{echoed}");
                assert_eq!(echoed["path"], "/sessions/s-1");
                assert_eq!(
                    echoed["userId"],
                    format!("token:{}", member.record.token_id)
                );
                assert_eq!(echoed["callerRole"], "editor");
                assert_eq!(echoed["internalServiceOk"], true);
                for stripped in [
                    "graphRole",
                    "graphId",
                    "graphOwner",
                    "cookie",
                    "authorization",
                ] {
                    assert!(echoed[stripped].is_null(), "{stripped} passed: {echoed}");
                }
                let (status, echoed) = get("/services/vehicle/sessions", bearer(MASTER)).await;
                assert_eq!(status, StatusCode::OK);
                assert_eq!(echoed["callerRole"], "owner");

                // A graph-scoped path: gardend takes the graph off the path and
                // mints it with the caller's role; the forged x-graph-id and
                // x-sophia-graph-role never pass.
                let (status, echoed) = get(
                    "/services/vehicle/g/atopile-led-badge/sessions/s-1",
                    bearer(&member.token),
                )
                .await;
                assert_eq!(status, StatusCode::OK, "{echoed}");
                assert_eq!(echoed["path"], "/sessions/s-1");
                assert_eq!(echoed["graphId"], "atopile-led-badge");
                assert_eq!(echoed["graphOwner"], GRAPH_OWNER);
                assert_eq!(echoed["graphRole"], "editor");
                assert_eq!(echoed["callerRole"], "editor");
                let (status, echoed) =
                    get("/services/vehicle/g/atopile-led-badge", bearer(MASTER)).await;
                assert_eq!(status, StatusCode::OK, "{echoed}");
                assert_eq!(echoed["path"], "/");
                assert_eq!(echoed["graphRole"], "owner");
                let (status, said) = get("/services/vehicle/g//sessions", bearer(MASTER)).await;
                assert_eq!(status, StatusCode::BAD_REQUEST, "{said}");
                // An admin-only service is still closed to a member, graph or not.
                let (status, said) = get(
                    "/services/tools/g/atopile-led-badge/x",
                    bearer(&member.token),
                )
                .await;
                assert_eq!(status, StatusCode::FORBIDDEN, "{said}");

                let (status, said) = get("/services/tools/anything", bearer(&member.token)).await;
                assert_eq!(status, StatusCode::FORBIDDEN, "{said}");
                let (status, echoed) = get("/services/tools/anything", bearer(MASTER)).await;
                assert_eq!(status, StatusCode::OK, "{echoed}");
                assert_eq!(echoed["userId"], "owner");
                let (status, _) = get("/services/nothing-here/x", bearer(MASTER)).await;
                assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
            });
            state.services.shutdown();
        });
    }

    /// A service that asks for it (`"delegateCaller": true`) gets, on every
    /// graph-scoped request, a delegation token for gardend's `/mcp` that
    /// acts for the caller on that graph only: the caller's own copy of the
    /// header never passes; the token lists no graph argument and fills its
    /// graph in; another graph and profile-wide tools are refused; a viewer's
    /// token cannot write (gardend refuses it, whatever the service asks);
    /// the audit log names the person, through the service, where the
    /// service's own token names the service; it is no credential anywhere
    /// else, and it dies with the service.
    #[test]
    fn a_delegation_token_acts_for_the_caller_on_its_graph_only() {
        with_profile("delegation", |root| {
            let app = crate::tauri_runtime::build_mock_app_for_tests(true);
            let state = state(
                app,
                root,
                SelfHost::default(),
                CellGraphBoundary::for_test(None),
            );
            let dir = root.join("services");
            write_service_manifest(
                &dir.join("vehicle"),
                "vehicle",
                &[],
                json!({ "memberAccess": "all", "delegateCaller": true }),
            );
            write_service_manifest(
                &dir.join("tools"),
                "tools",
                &[],
                json!({ "memberAccess": "all" }),
            );
            state.services.pin_services_dir(dir);
            state.services.set_timing(fast_timing());
            state
                .services
                .set_local_api_url("http://127.0.0.1:9".into());
            for service_id in ["vehicle", "tools"] {
                state
                    .services
                    .start(service_id, &state.app, &state.manifest)
                    .unwrap();
                wait_service_healthy(&state.services, service_id);
            }
            for graph in ["notes", "other"] {
                crate::graph_service::create_graph_service(
                    &state.app,
                    crate::graph_service::CreateGraphInput {
                        graph_id: Some(graph.into()),
                        title: format!("{graph} graph"),
                        description: None,
                        operation_id: None,
                    },
                )
                .unwrap();
            }
            let issue = |grant: Option<&str>, scopes: Option<Vec<String>>| {
                create_client_token(
                    &state.app,
                    CreateLoopbackClientTokenInput {
                        label: Some("delegation test".into()),
                        grant_profile_id: grant.map(str::to_string),
                        scopes,
                        expires_in_days: None,
                    },
                )
                .unwrap()
            };
            let member = issue(Some("member"), None);
            let mut viewer_scopes = grant_profile_scopes("read-only");
            viewer_scopes.push("services.proxy".into());
            let viewer = issue(None, Some(viewer_scopes));

            crate::app_runtime::async_runtime::block_on(async {
                let echo = |path: &'static str, credential: (&'static str, String)| {
                    let state = state.clone();
                    async move {
                        let mut headers = forged();
                        headers.push(("x-sophia-delegated-token", "forged-by-the-caller".into()));
                        headers.push(credential);
                        json_of(call(&state, "GET", path, &headers, Body::empty()).await).await
                    }
                };
                let token_for = |credential: (&'static str, String)| {
                    let echo = &echo;
                    async move {
                        let (status, echoed) =
                            echo("/services/vehicle/g/notes/sessions", credential).await;
                        assert_eq!(status, StatusCode::OK, "{echoed}");
                        assert_eq!(echoed["graphId"], "notes");
                        let token = echoed["delegatedToken"].as_str().unwrap_or_default().to_string();
                        assert!(token.starts_with("sophia-dlg-"), "{echoed}");
                        token
                    }
                };
                let owner_token = token_for(bearer(MASTER)).await;
                assert_ne!(owner_token, "forged-by-the-caller");
                assert_eq!(
                    token_for(bearer(MASTER)).await,
                    owner_token,
                    "the same caller on the same graph keeps its token while it is young"
                );
                let member_token = token_for(bearer(&member.token)).await;
                let viewer_token = token_for(bearer(&viewer.token)).await;
                assert!(member_token != owner_token && viewer_token != member_token);
                // No graph, no token; a service that did not ask gets none;
                // the caller's own copy never passes.
                for (path, why) in [
                    ("/services/vehicle/sessions", "not graph-scoped"),
                    ("/services/tools/g/notes/x", "the service did not ask"),
                ] {
                    let (status, echoed) = echo(path, bearer(MASTER)).await;
                    assert_eq!(status, StatusCode::OK, "{echoed}");
                    assert!(echoed["delegatedToken"].is_null(), "{why}: {echoed}");
                }

                let mcp = |token: String, method: &'static str, params: Value| {
                    let state = state.clone();
                    async move {
                        let body = json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params });
                        json_of(
                            call(
                                &state,
                                "POST",
                                "/mcp",
                                &[bearer(&token), ("content-type", "application/json".into())],
                                Body::from(serde_json::to_vec(&body).unwrap()),
                            )
                            .await,
                        )
                        .await
                    }
                };
                let tool = |token: &String, name: &'static str, arguments: Value| {
                    mcp(token.clone(), "tools/call", json!({ "name": name, "arguments": arguments }))
                };
                let listed_names = |listed: &Value| {
                    listed["result"]["tools"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|tool| tool["name"].as_str().unwrap().to_string())
                        .collect::<Vec<_>>()
                };

                // tools/list: no graph argument anywhere, nothing profile-wide,
                // and for the viewer no tool that writes.
                let (status, listed) = mcp(owner_token.clone(), "tools/list", json!({})).await;
                assert_eq!(status, StatusCode::OK, "{listed}");
                assert_eq!(listed["result"]["_meta"]["sophia.boundGraph"], "notes");
                let owner_tools = listed_names(&listed);
                for schema in listed["result"]["tools"].as_array().unwrap() {
                    let schema = schema["inputSchema"].to_string();
                    assert!(!schema.contains("\"graphId\"") && !schema.contains("\"graph_id\""), "{schema}");
                }
                assert!(owner_tools.iter().any(|name| name == "create_document"));
                assert!(owner_tools.iter().any(|name| name == "datasheet_facts"));
                assert!(!owner_tools.iter().any(|name| name == "list_graphs"));
                let (_, listed) = mcp(viewer_token.clone(), "tools/list", json!({})).await;
                let viewer_tools = listed_names(&listed);
                assert!(viewer_tools.iter().any(|name| name == "read_document"));
                for writer in ["create_document", "write_document", "edit_comment", "create_wires"] {
                    assert!(!viewer_tools.iter().any(|name| name == writer), "{writer}");
                }

                // (i) a write through a delegation token is the caller's.
                for (token, title) in [
                    (&owner_token, "Written for the owner"),
                    (&member_token, "Written for the member"),
                ] {
                    let (status, created) =
                        tool(token, "create_document", json!({ "title": title })).await;
                    assert_eq!(status, StatusCode::OK);
                    assert!(created["error"].is_null(), "{created}");
                }
                // (ii) the viewer's token cannot write, whatever the service
                // asks: gardend refuses it.
                let (_, refused) =
                    tool(&viewer_token, "create_document", json!({ "title": "Not for a viewer" })).await;
                assert!(
                    refused["error"]["message"]
                        .as_str()
                        .is_some_and(|message| message.contains("missing required scope") && message.contains(".write.crdt")),
                    "{refused}"
                );
                // (iii) the graph is gardend's to fill in: none named, the
                // bound one read; another graph or every graph refused.
                let (_, listed) = tool(&viewer_token, "list_documents", json!({})).await;
                assert!(listed["error"].is_null(), "{listed}");
                let listed = listed.to_string();
                assert!(listed.contains("Written for the owner") && listed.contains("Written for the member"), "{listed}");
                assert!(!listed.contains("Not for a viewer"));
                for (name, arguments, said) in [
                    ("list_documents", json!({ "graphId": "other" }), "bound to the graph \"notes\""),
                    ("read_document", json!({ "graph_id": "other", "documentId": "x" }), "bound to the graph \"notes\""),
                    ("list_graphs", json!({}), "reaches beyond the graph \"notes\""),
                ] {
                    let (_, refused) = tool(&owner_token, name, arguments).await;
                    assert!(
                        refused["error"]["message"].as_str().is_some_and(|message| message.contains(said)),
                        "{name}: {refused}"
                    );
                }

                // The old way: the service's own process token, attributed to
                // the service.
                let (_, echoed) = echo("/services/vehicle/api/env", bearer(MASTER)).await;
                let service_token = echoed["env"]["MNEMOSYNE_MCP_ACCESS_TOKEN"].as_str().unwrap().to_string();
                let (_, created) = tool(
                    &service_token,
                    "create_document",
                    json!({ "graphId": "notes", "title": "Written by the service" }),
                )
                .await;
                assert!(created["error"].is_null(), "{created}");

                // The audit log names who: every CRDT write and every write
                // tool call a delegation token made.
                let events = serde_json::to_value(
                    crate::loopback_audit_log::list_loopback_audit_events(state.app.clone(), None).unwrap(),
                )
                .unwrap();
                let rows = events.as_array().unwrap();
                let crdt_actors = rows
                    .iter()
                    .filter(|row| row["category"] == "crdt.operation" && row["outcome"] == "succeeded")
                    .map(|row| (row["actor"].as_str().unwrap().to_string(), row["details"]["via"].clone()))
                    .collect::<Vec<_>>();
                let member_id = format!("token:{}", member.record.token_id);
                for expected in [
                    ("owner".to_string(), json!("service:vehicle")),
                    (member_id.clone(), json!("service:vehicle")),
                    ("service:vehicle".to_string(), Value::Null),
                ] {
                    assert!(crdt_actors.contains(&expected), "{expected:?} in {crdt_actors:?}");
                }
                assert!(
                    !crdt_actors.iter().any(|(actor, _)| actor == "local-runtime"),
                    "{crdt_actors:?}"
                );
                let calls = rows
                    .iter()
                    .filter(|row| row["category"] == "mcp.tools.call")
                    .map(|row| {
                        (
                            row["actor"].as_str().unwrap().to_string(),
                            row["action"].as_str().unwrap().to_string(),
                            row["outcome"].as_str().unwrap().to_string(),
                            row["targetId"].clone(),
                            row["details"]["via"].clone(),
                        )
                    })
                    .collect::<Vec<_>>();
                let viewer_id = format!("token:{}", viewer.record.token_id);
                for (actor, outcome) in [
                    ("owner".to_string(), "succeeded"),
                    (member_id.clone(), "succeeded"),
                    (viewer_id.clone(), "failed"),
                ] {
                    let expected = (
                        actor,
                        "create_document".to_string(),
                        outcome.to_string(),
                        json!("notes"),
                        json!("service:vehicle"),
                    );
                    assert!(calls.contains(&expected), "{expected:?} in {calls:?}");
                }
                assert!(
                    !calls.iter().any(|call| call.1 == "list_documents"),
                    "reads are not audited: {calls:?}"
                );

                // A delegation token is no credential anywhere but /mcp.
                for (method, path) in [
                    ("GET", "/services/vehicle/g/notes/sessions"),
                    ("GET", "/api/services/vehicle/status"),
                ] {
                    let response = call(&state, method, path, &[bearer(&owner_token)], Body::empty()).await;
                    assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{path}");
                }
                // A revoked client token takes its delegations with it.
                crate::loopback_client_token_service::revoke_client_token(
                    &state.app,
                    crate::loopback_client_token_types::RevokeLoopbackClientTokenInput {
                        token_id: member.record.token_id.clone(),
                    },
                )
                .unwrap();
                let (status, said) = mcp(member_token.clone(), "tools/list", json!({})).await;
                assert_eq!(status, StatusCode::UNAUTHORIZED, "{said}");
                let (status, _) = mcp(owner_token.clone(), "tools/list", json!({})).await;
                assert_eq!(status, StatusCode::OK);
                // It dies with the service process it was handed to.
                state.services.stop("vehicle", &state.app).unwrap();
                let (status, said) = mcp(owner_token.clone(), "tools/list", json!({})).await;
                assert_eq!(status, StatusCode::UNAUTHORIZED, "{said}");
            });
            state.services.shutdown();
        });
    }
}
