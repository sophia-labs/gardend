use crate::{
    cell_graph_boundary::CellGraphJson,
    local_jobs::local_job_status_response,
    loopback_http::{
        loopback_app_error, loopback_app_result, loopback_error, loopback_result,
        require_loopback_scope,
    },
    loopback_state::LoopbackState,
    semantic_models::SemanticModelConfigInput,
    semantic_service::{
        get_semantic_index_status_service, get_semantic_model_status_service,
        list_semantic_models_service, prepare_semantic_model_service, refresh_semantic_index,
        semantic_reason, semantic_search, set_semantic_model_config_service,
        submit_semantic_index_refresh_job, RefreshSemanticIndexInput, SemanticReasonInput,
        SemanticSearchInput,
    },
};
use axum::{
    extract::{Path as AxumPath, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post, put},
    Json, Router,
};
use std::sync::Arc;

pub(super) fn loopback_semantic_router() -> Router<Arc<LoopbackState>> {
    Router::new()
        .route(
            "/api/semantic/model/status",
            get(loopback_semantic_model_status),
        )
        .route("/api/semantic/models", get(loopback_semantic_models))
        .route(
            "/api/semantic/model/config",
            put(loopback_set_semantic_model_config),
        )
        .route(
            "/api/semantic/model/prepare",
            post(loopback_prepare_semantic_model),
        )
        .route(
            "/api/semantic/index/status/{graph_id}",
            get(loopback_semantic_index_status),
        )
        .route(
            "/api/semantic/index/refresh",
            post(loopback_refresh_semantic_index),
        )
        .route(
            "/api/semantic/index/refresh/jobs",
            post(loopback_start_semantic_index_refresh),
        )
        .route(
            "/api/semantic/index/refresh/jobs/{job_id}",
            get(loopback_semantic_index_refresh_job)
                .delete(loopback_cancel_semantic_index_refresh_job),
        )
        .route("/api/semantic/search", post(loopback_semantic_search))
        .route("/api/semantic/reason", post(loopback_semantic_reason))
}

pub(super) async fn loopback_semantic_model_status(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "semantic.models.read") {
        return response;
    }
    loopback_app_result(get_semantic_model_status_service(&state.app))
}

pub(super) async fn loopback_semantic_models(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "semantic.models.read") {
        return response;
    }
    loopback_app_result(list_semantic_models_service(&state.app))
}

pub(super) async fn loopback_set_semantic_model_config(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    Json(input): Json<SemanticModelConfigInput>,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "semantic.models.write") {
        return response;
    }
    loopback_app_result(set_semantic_model_config_service(&state.app, input))
}

pub(super) async fn loopback_prepare_semantic_model(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "semantic.models.write") {
        return response;
    }
    loopback_app_result(prepare_semantic_model_service(&state.app))
}

pub(super) async fn loopback_semantic_index_status(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    AxumPath(graph_id): AxumPath<String>,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "semantic.index.read") {
        return response;
    }
    loopback_app_result(get_semantic_index_status_service(&state.app, graph_id))
}

pub(super) async fn loopback_refresh_semantic_index(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    CellGraphJson(input): CellGraphJson<RefreshSemanticIndexInput>,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "semantic.index.write") {
        return response;
    }
    loopback_result(refresh_semantic_index(state.app.clone(), input))
}

pub(super) async fn loopback_start_semantic_index_refresh(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    CellGraphJson(input): CellGraphJson<RefreshSemanticIndexInput>,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "semantic.index.write") {
        return response;
    }
    match submit_semantic_index_refresh_job(state.app.clone(), state.jobs.clone(), input) {
        Ok(response) => (StatusCode::ACCEPTED, Json(response)).into_response(),
        Err(error) if error.contains("not found") => loopback_error(StatusCode::NOT_FOUND, &error),
        Err(error) => loopback_error(StatusCode::BAD_REQUEST, &error),
    }
}

pub(super) async fn loopback_semantic_index_refresh_job(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    AxumPath(job_id): AxumPath<String>,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "semantic.index.read") {
        return response;
    }
    if let Err(error) = state.cell_graph.authorize_job_id(&state.jobs, &job_id) {
        return loopback_error(StatusCode::NOT_FOUND, &error.mcp_message());
    }
    match state.jobs.get_fresh(&job_id) {
        Ok(Some(record)) => Json(local_job_status_response(&record)).into_response(),
        Ok(None) => loopback_error(
            StatusCode::NOT_FOUND,
            "semantic index refresh job not found",
        ),
        Err(error) => loopback_app_error(error),
    }
}

pub(super) async fn loopback_cancel_semantic_index_refresh_job(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    AxumPath(job_id): AxumPath<String>,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "semantic.index.cancel") {
        return response;
    }
    if let Err(error) = state.cell_graph.authorize_job_id(&state.jobs, &job_id) {
        return loopback_error(StatusCode::NOT_FOUND, &error.mcp_message());
    }
    match state.jobs.cancel(&job_id) {
        Ok(Some(response)) => Json(response).into_response(),
        Ok(None) => loopback_error(
            StatusCode::NOT_FOUND,
            "semantic index refresh job not found",
        ),
        Err(error) => loopback_app_error(error),
    }
}

pub(super) async fn loopback_semantic_search(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    CellGraphJson(input): CellGraphJson<SemanticSearchInput>,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "search.semantic.read") {
        return response;
    }
    loopback_result(semantic_search(state.app.clone(), input))
}

pub(super) async fn loopback_semantic_reason(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    CellGraphJson(input): CellGraphJson<SemanticReasonInput>,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "search.semantic.read") {
        return response;
    }
    loopback_result(semantic_reason(state.app.clone(), input))
}
