use crate::{
    loopback_http::{loopback_app_error, loopback_app_result, require_loopback_scope},
    loopback_state::LoopbackState,
    time_travel_service::{
        capture_restore_point, delete_restore_point_response, diff_restore_points_response,
        get_restore_point_response, list_restore_points_response, restore_point_workspace_payload,
    },
    time_travel_types::RestorePointTrigger,
};
use axum::{
    extract::{Path as AxumPath, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use serde::Deserialize;
use std::{collections::BTreeMap, sync::Arc};

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct CaptureRestorePointRequest {
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    trigger: Option<String>,
}

pub(super) fn loopback_time_travel_router() -> Router<Arc<LoopbackState>> {
    Router::new()
        .route(
            "/v1/time-travel/{graph_id}/restore-points",
            get(loopback_list_restore_points).post(loopback_create_restore_point),
        )
        .route(
            "/v1/time-travel/{graph_id}/restore-points/{restore_point_id}",
            get(loopback_get_restore_point).delete(loopback_delete_restore_point),
        )
        .route(
            "/v1/time-travel/{graph_id}/restore-points/{restore_point_id}/snapshot",
            get(loopback_get_restore_point_snapshot),
        )
        .route(
            "/v1/time-travel/{graph_id}/restore-points/{restore_point_id}/diff",
            get(loopback_get_restore_point_diff),
        )
}

async fn loopback_list_restore_points(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    AxumPath(graph_id): AxumPath<String>,
    Query(params): Query<BTreeMap<String, String>>,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "time-travel.read") {
        return response;
    }
    let cursor = params.get("cursor").map(String::as_str);
    let limit = params
        .get("limit")
        .and_then(|value| value.parse::<usize>().ok());
    loopback_app_result(list_restore_points_response(
        &state.app, &graph_id, cursor, limit,
    ))
}

async fn loopback_get_restore_point(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    AxumPath((graph_id, restore_point_id)): AxumPath<(String, String)>,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "time-travel.read") {
        return response;
    }
    loopback_app_result(get_restore_point_response(
        &state.app,
        &graph_id,
        &restore_point_id,
    ))
}

async fn loopback_get_restore_point_snapshot(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    AxumPath((graph_id, restore_point_id)): AxumPath<(String, String)>,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "time-travel.read") {
        return response;
    }
    loopback_app_result(restore_point_workspace_payload(
        &state.app,
        &graph_id,
        &restore_point_id,
    ))
}

async fn loopback_get_restore_point_diff(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    AxumPath((graph_id, restore_point_id)): AxumPath<(String, String)>,
    Query(params): Query<BTreeMap<String, String>>,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "time-travel.read") {
        return response;
    }
    let against = params.get("against").map(String::as_str);
    loopback_app_result(diff_restore_points_response(
        &state.app,
        &graph_id,
        &restore_point_id,
        against,
    ))
}

async fn loopback_create_restore_point(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    AxumPath(graph_id): AxumPath<String>,
    body: Option<Json<CaptureRestorePointRequest>>,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "time-travel.write") {
        return response;
    }
    let request = body.map(|Json(value)| value).unwrap_or_default();
    let trigger = match request.trigger.as_deref() {
        Some("interval") => RestorePointTrigger::Interval,
        Some("checkpoint") => RestorePointTrigger::Checkpoint,
        _ => RestorePointTrigger::Manual,
    };
    let label = request
        .label
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    match capture_restore_point(&state.app, &graph_id, trigger, label) {
        Ok(value) => (StatusCode::CREATED, Json(value)).into_response(),
        Err(error) => loopback_app_error(error),
    }
}

async fn loopback_delete_restore_point(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    AxumPath((graph_id, restore_point_id)): AxumPath<(String, String)>,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "time-travel.write") {
        return response;
    }
    loopback_app_result(delete_restore_point_response(
        &state.app,
        &graph_id,
        &restore_point_id,
    ))
}
