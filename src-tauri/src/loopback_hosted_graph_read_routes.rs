use crate::{
    app_error::AppError,
    clock::epoch_millis,
    graph_projection_service::{hosted_graph_entries_service, hosted_graph_entry_from_record},
    graph_service::{hosted_graph_stats, read_graph_record},
    local_jobs::local_graph_query_submit_response,
    loopback_graph_inputs::HostedGraphListQuery,
    loopback_http::{
        loopback_app_error, loopback_app_result, loopback_result, require_loopback_scope,
    },
    loopback_state::LoopbackState,
};
use axum::{
    extract::{Path as AxumPath, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use std::sync::Arc;

pub(super) async fn loopback_hosted_list_graphs(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    Query(query): Query<HostedGraphListQuery>,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "graphs.read") {
        return response;
    }

    let result = hosted_graph_entries_service(&state.app, false);
    if query.wait_ms.is_some() {
        return loopback_app_result(result);
    }

    let started_at_ms = epoch_millis();
    let result = result
        .and_then(|entries| {
            serde_json::to_value(entries).map_err(|error| {
                AppError::serialization(format!("serialize hosted graph entries: {error}"))
            })
        })
        .map_err(AppError::message);
    let record = match state.jobs.insert_finished(
        "list_graphs",
        None,
        started_at_ms,
        result,
        "application/json",
    ) {
        Ok(record) => record,
        Err(error) => return loopback_app_error(error),
    };

    (
        StatusCode::ACCEPTED,
        Json(local_graph_query_submit_response(&record)),
    )
        .into_response()
}

pub(super) async fn loopback_hosted_read_graph(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    AxumPath(graph_id): AxumPath<String>,
    Query(query): Query<HostedGraphListQuery>,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "graphs.read") {
        return response;
    }

    let result = read_graph_record(&state.app, &graph_id)
        .map(|(_, graph)| hosted_graph_entry_from_record(graph, true));
    if query.wait_ms.is_some() {
        return loopback_app_result(result);
    }

    let started_at_ms = epoch_millis();
    let record = match state.jobs.insert_finished(
        "read_graph",
        Some(graph_id),
        started_at_ms,
        result.map_err(AppError::message),
        "application/json",
    ) {
        Ok(record) => record,
        Err(error) => return loopback_app_error(error),
    };

    (
        StatusCode::ACCEPTED,
        Json(local_graph_query_submit_response(&record)),
    )
        .into_response()
}

pub(super) async fn loopback_hosted_graph_stats(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    Query(query): Query<HostedGraphListQuery>,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "graphs.read") {
        return response;
    }

    let result = hosted_graph_stats(state.app.clone());
    if query.wait_ms.is_some() {
        return loopback_result(result);
    }

    let started_at_ms = epoch_millis();
    let record = match state.jobs.insert_finished(
        "read_graph_stats",
        None,
        started_at_ms,
        result,
        "application/json",
    ) {
        Ok(record) => record,
        Err(error) => return loopback_app_error(error),
    };

    (
        StatusCode::ACCEPTED,
        Json(local_graph_query_submit_response(&record)),
    )
        .into_response()
}
