use crate::{
    cell_graph_boundary::CellGraphJson,
    graph_maintenance_service::{
        reindex_graph_job_response, rematerialize_graph_response, ReindexGraphInput,
        RematerializeGraphInput,
    },
    loopback_http::{
        loopback_error, loopback_result, require_loopback_scope, require_loopback_scopes,
    },
    loopback_semantic_routes::loopback_semantic_router,
    loopback_state::LoopbackState,
    search_projection_service::{hosted_block_search_response, hosted_semantic_search_response},
};
use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
    Json, Router,
};
use std::sync::Arc;

pub(super) fn loopback_ai_router() -> Router<Arc<LoopbackState>> {
    Router::new()
        .route("/search", post(loopback_hosted_semantic_search))
        .route("/search/blocks", post(loopback_hosted_block_search))
        .route("/search/hybrid", post(loopback_hosted_hybrid_search))
        .route(
            "/search/rematerialize",
            post(loopback_hosted_rematerialize_graph),
        )
        .route("/search/reindex", post(loopback_hosted_reindex_graph))
        .merge(loopback_semantic_router())
}

pub(super) async fn loopback_hosted_semantic_search(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    CellGraphJson(input): CellGraphJson<serde_json::Value>,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "search.semantic.read") {
        return response;
    }
    loopback_result(hosted_semantic_search_response(state.app.clone(), &input))
}

pub(super) async fn loopback_hosted_block_search(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    CellGraphJson(input): CellGraphJson<serde_json::Value>,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "search.lexical.read") {
        return response;
    }
    loopback_result(hosted_block_search_response(
        state.app.clone(),
        &input,
        "lexical",
    ))
}

pub(super) async fn loopback_hosted_hybrid_search(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    CellGraphJson(input): CellGraphJson<serde_json::Value>,
) -> Response {
    if let Err(response) = require_loopback_scopes(
        &headers,
        &state,
        &["search.lexical.read", "search.semantic.read"],
    ) {
        return response;
    }
    loopback_result(hosted_block_search_response(
        state.app.clone(),
        &input,
        "hybrid",
    ))
}

pub(super) async fn loopback_hosted_rematerialize_graph(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    CellGraphJson(input): CellGraphJson<RematerializeGraphInput>,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "semantic.index.write") {
        return response;
    }
    loopback_result(rematerialize_graph_response(state.app.clone(), input))
}

pub(super) async fn loopback_hosted_reindex_graph(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    CellGraphJson(input): CellGraphJson<ReindexGraphInput>,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "semantic.index.write") {
        return response;
    }
    match reindex_graph_job_response(state.app.clone(), state.jobs.clone(), input) {
        Ok(value) => Json(value).into_response(),
        Err(error) if error.contains("embedding model is not prepared") => {
            loopback_error(StatusCode::SERVICE_UNAVAILABLE, &error)
        }
        Err(error) => loopback_error(StatusCode::BAD_REQUEST, &error),
    }
}
