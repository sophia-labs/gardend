use crate::{
    crdt_queue::CrdtOperationQueue,
    get_profile,
    loopback_http::{loopback_error, loopback_result, require_loopback_scope},
    loopback_state::{LoopbackManifestPublic, LoopbackState},
    runtime_capabilities::get_capabilities,
    runtime_config::LOCAL_OPENAPI_JSON,
};
use axum::{
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use serde::Deserialize;
use std::sync::Arc;
#[cfg(feature = "desktop")]
use tauri::Manager;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LocalCrdtTimingQuery {
    limit: Option<usize>,
    clear: Option<bool>,
    kind: Option<String>,
    #[serde(alias = "document_id")]
    document_id: Option<String>,
    #[serde(alias = "operation_id")]
    operation_id: Option<String>,
}

pub(crate) fn loopback_core_router() -> Router<Arc<LoopbackState>> {
    Router::new()
        .route("/health", get(loopback_health))
        .route("/manifest", get(loopback_manifest))
        .route("/openapi.json", get(loopback_openapi))
        .route("/api/capabilities", get(loopback_capabilities))
        .route("/api/profile", get(loopback_profile))
        .route("/api/local/crdt-timings", get(loopback_local_crdt_timings))
}

pub(crate) async fn loopback_health(State(state): State<Arc<LoopbackState>>) -> Response {
    // The cell is a self-contained in-process runtime (Oxigraph + CRDT engine);
    // there is no Redis or other external dependency to report on. Keep the
    // payload to the single honest fact: the server is up.
    if crate::cell_lease::handle().is_some_and(|lease| {
        lease.mode() == crate::cell_lease::LeaseMode::Enforce && lease.terminal_reason().is_some()
    }) {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({
                "status": "lease_terminal",
            })),
        )
            .into_response()
    } else if state.lifecycle.is_draining() {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({
                "status": "draining",
            })),
        )
            .into_response()
    } else if crate::cell_lease::handle()
        .map(|lease| lease.is_fenced())
        .unwrap_or(false)
    {
        // U8 cross-process write lease (spec §3.5): FENCED is a liveness,
        // not a health, condition — reads keep serving and this is NOT
        // `draining` (never 503). A fenced-but-not-yet-terminal cell must
        // stay in the gateway's rotation so reads keep flowing; only the
        // durable-flush gates in `cell_durability` (Gate A/B/C) refuse.
        Json(serde_json::json!({
            "status": "fenced",
        }))
        .into_response()
    } else {
        let drift = state.cell_graph.surface_drift();
        Json(serde_json::json!({
            "status": "ok",
            "surfacePolicy": if drift.is_empty() { "current" } else { "drift" },
            "surfaceDriftCount": drift.len(),
        }))
        .into_response()
    }
}

pub(crate) async fn loopback_manifest(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "loopback.manifest.read") {
        return response;
    }
    // Secret-free for every principal, including the Owner/master-token
    // caller — see `LoopbackManifestPublic` doc comment. The full
    // `LoopbackManifest` (with `token`) is never serialized to HTTP; disk
    // `loopback.json` remains the only channel that carries it.
    Json(LoopbackManifestPublic::from(&state.manifest)).into_response()
}

pub(crate) async fn loopback_openapi(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "loopback.openapi.read") {
        return response;
    }
    match serde_json::from_str::<serde_json::Value>(LOCAL_OPENAPI_JSON) {
        Ok(mut value) => {
            state.cell_graph.filter_openapi(&mut value);
            Json(value).into_response()
        }
        Err(error) => loopback_error(StatusCode::INTERNAL_SERVER_ERROR, &error.to_string()),
    }
}

pub(crate) async fn loopback_capabilities(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "runtime.capabilities.read") {
        return response;
    }
    Json(get_capabilities()).into_response()
}

pub(crate) async fn loopback_profile(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "profile.read") {
        return response;
    }
    loopback_result(get_profile(state.app.clone()))
}

pub(crate) async fn loopback_local_crdt_timings(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    Query(query): Query<LocalCrdtTimingQuery>,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "diagnostics.crdt-timings.read")
    {
        return response;
    }
    let queue = state.app.state::<CrdtOperationQueue>();
    loopback_result(
        queue
            .recent_traces(
                query.limit,
                query.clear.unwrap_or(false),
                query.kind.as_deref(),
                query.document_id.as_deref(),
                query.operation_id.as_deref(),
            )
            .map(|traces| {
                serde_json::json!({
                    "count": traces.len(),
                    "traces": traces,
                })
            }),
    )
}
