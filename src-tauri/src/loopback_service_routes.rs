use crate::{
    emporium::openapi_emit::{open_api_store, read_api_operation},
    local_service_host::{
        LocalServiceHealth, LocalServiceHost, LocalServiceState, LocalServiceStatus,
        CHOREOGRAPH_SERVICE_ID, KG_ULTRA_SERVICE_ID,
    },
    loopback_http::{loopback_error, loopback_result, require_loopback_scope},
    loopback_state::LoopbackState,
    runtime_config::PROFILE_ID,
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
    let envelope = operation_invocation_envelope(
        &graph_id,
        &operation_id,
        &invocation_id,
        &binding,
        &definition,
        input,
    );
    invoke_operation_upstream(&state, &headers, &envelope).await
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
        "actorRef": format!("user:{PROFILE_ID}"),
        "graph": { "graphId": graph_id },
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
        .header("X-User-ID", PROFILE_ID)
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
    match services.start(service_id, &state.app, &state.manifest, &state.token) {
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
    if let Err(response) = require_loopback_scope(&headers, &state, SERVICES_PROXY_SCOPE) {
        return response;
    }
    if wants_event_stream(&headers) {
        return loopback_error(
            StatusCode::NOT_IMPLEMENTED,
            "local service proxy currently supports JSON polling; use Accept: application/json for workflow events",
        );
    }
    let target = match state.services.target(service_id) {
        Ok(target) => target,
        Err(error) => return loopback_error(StatusCode::SERVICE_UNAVAILABLE, &error),
    };
    let url = build_upstream_url(&target.base_url, upstream_path, query);
    let client = match local_service_client() {
        Ok(client) => client,
        Err(error) => return loopback_error(StatusCode::INTERNAL_SERVER_ERROR, &error),
    };
    let mut request = client
        .request(method, &url)
        .header("X-Internal-Service", target.internal_secret)
        .header("X-User-ID", PROFILE_ID)
        .body(body);
    for (name, value) in headers.iter() {
        if should_forward_header(name) {
            request = request.header(name, value);
        }
    }
    match request.send().await {
        Ok(response) => upstream_response(response).await,
        Err(error) => loopback_error(
            StatusCode::BAD_GATEWAY,
            &format!("local service proxy failed: {error}"),
        ),
    }
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

fn wants_event_stream(headers: &HeaderMap) -> bool {
    headers
        .get(header::ACCEPT)
        .and_then(|value| value.to_str().ok())
        .map(|accept| {
            accept
                .split(',')
                .any(|part| part.trim() == "text/event-stream")
        })
        .unwrap_or(false)
}

fn should_forward_header(name: &HeaderName) -> bool {
    !matches!(
        name.as_str(),
        "authorization"
            | "connection"
            | "host"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
            | "x-internal-service"
            | "x-user-id"
    )
}

fn local_service_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|error| format!("build local service client: {error}"))
}

async fn upstream_response(upstream: reqwest::Response) -> Response {
    let status =
        StatusCode::from_u16(upstream.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let mut builder = Response::builder().status(status);
    for (name, value) in upstream.headers().iter() {
        if should_return_header(name) {
            builder = builder.header(name, value);
        }
    }
    match upstream.bytes().await {
        Ok(bytes) => builder
            .body(Body::from(bytes))
            .unwrap_or_else(|_| loopback_error(StatusCode::BAD_GATEWAY, "build proxy response")),
        Err(error) => loopback_error(
            StatusCode::BAD_GATEWAY,
            &format!("read local service response: {error}"),
        ),
    }
}

fn should_return_header(name: &HeaderName) -> bool {
    !matches!(
        name.as_str(),
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
    fn proxy_drops_boundary_auth_headers() {
        assert!(!should_forward_header(&HeaderName::from_static(
            "authorization"
        )));
        assert!(!should_forward_header(&HeaderName::from_static(
            "x-internal-service"
        )));
        assert!(should_forward_header(&HeaderName::from_static("accept")));
    }
}
