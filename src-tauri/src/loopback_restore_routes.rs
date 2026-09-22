use crate::{
    loopback_http::{
        loopback_app_error, loopback_app_result, loopback_error, require_loopback_scope,
    },
    loopback_state::LoopbackState,
    time_travel_restore_service::{cancel_restore_operation, get_restore_operation, start_restore},
};
use axum::{
    extract::{Path as AxumPath, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use std::sync::Arc;

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct StartRestoreRequest {
    #[serde(default)]
    restore_point_id: Option<String>,
    #[serde(default)]
    dry_run: bool,
}

pub(super) fn loopback_restore_router() -> Router<Arc<LoopbackState>> {
    Router::new()
        .route(
            "/v1/time-travel/{graph_id}/restores",
            post(loopback_start_restore),
        )
        .route(
            "/v1/time-travel/{graph_id}/restores/{operation_id}",
            get(loopback_get_restore_operation),
        )
        .route(
            "/v1/time-travel/{graph_id}/restores/{operation_id}/cancel",
            post(loopback_cancel_restore_operation),
        )
}

async fn loopback_start_restore(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    AxumPath(graph_id): AxumPath<String>,
    body: Option<Json<StartRestoreRequest>>,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "time-travel.restore") {
        return response;
    }
    let request = body.map(|Json(value)| value).unwrap_or_default();
    let restore_point_id = match request.restore_point_id.as_deref() {
        Some(value) if !value.trim().is_empty() => value,
        _ => return loopback_error(StatusCode::BAD_REQUEST, "restore_point_id is required"),
    };
    match start_restore(
        &state.app,
        state.jobs.clone(),
        &graph_id,
        restore_point_id,
        request.dry_run,
    ) {
        Ok(value) => {
            let status = if request.dry_run {
                StatusCode::OK
            } else {
                StatusCode::ACCEPTED
            };
            (status, Json(value)).into_response()
        }
        Err(error) => loopback_app_error(error),
    }
}

async fn loopback_get_restore_operation(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    AxumPath((_graph_id, operation_id)): AxumPath<(String, String)>,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "time-travel.read") {
        return response;
    }
    if let Err(error) = state
        .cell_graph
        .authorize_job_id(state.jobs.as_ref(), &operation_id)
    {
        return error.http_response();
    }
    loopback_app_result(get_restore_operation(state.jobs.as_ref(), &operation_id))
}

async fn loopback_cancel_restore_operation(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    AxumPath((_graph_id, operation_id)): AxumPath<(String, String)>,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "time-travel.restore") {
        return response;
    }
    if let Err(error) = state
        .cell_graph
        .authorize_job_id(state.jobs.as_ref(), &operation_id)
    {
        return error.http_response();
    }
    loopback_app_result(cancel_restore_operation(state.jobs.as_ref(), &operation_id))
}
