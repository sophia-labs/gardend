use crate::{
    app_error::AppError,
    clock::epoch_millis,
    graph_duplicate_service::duplicate_graph,
    graph_projection_service::hosted_graph_entry_from_record,
    graph_service::{
        create_graph_service_async, create_graph_service_async_with_incarnation,
        soft_delete_graph_service_async, update_graph_metadata_service_async,
        UpdateGraphMetadataInput,
    },
    local_jobs::local_job_submit_response,
    loopback_graph_inputs::{DeleteGraphQuery, FencedCreateGraphInput, GraphDuplicateInput},
    loopback_http::{loopback_app_error, require_loopback_scope, require_loopback_scopes},
    loopback_state::LoopbackState,
};
use axum::{
    extract::{Path as AxumPath, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use std::sync::Arc;

pub(super) async fn loopback_hosted_create_graph(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    Json(input): Json<FencedCreateGraphInput>,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "graphs.write") {
        return response;
    }

    let started_at_ms = epoch_millis();
    let graph_id = input.graph_id.clone().unwrap_or_else(|| {
        input
            .title
            .to_lowercase()
            .replace(|c: char| !c.is_alphanumeric(), "-")
            .trim_matches('-')
            .to_string()
    });
    let (input, graph_incarnation) = input.into_parts();
    let create_result = match graph_incarnation {
        Some(graph_incarnation) => {
            create_graph_service_async_with_incarnation(&state.app, input, graph_incarnation).await
        }
        None => create_graph_service_async(&state.app, input).await,
    };
    let result = create_result
        .and_then(|_| {
            serde_json::to_value(serde_json::Value::Null).map_err(|error| {
                AppError::serialization(format!("serialize create graph response: {error}"))
            })
        })
        .map_err(AppError::message);
    let record = match state.jobs.insert_finished(
        "create_graph",
        Some(graph_id),
        started_at_ms,
        result,
        "application/json",
    ) {
        Ok(record) => record,
        Err(error) => return loopback_app_error(error),
    };

    (
        StatusCode::ACCEPTED,
        Json(local_job_submit_response(&record)),
    )
        .into_response()
}

pub(super) async fn loopback_hosted_update_graph(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    AxumPath(graph_id): AxumPath<String>,
    Json(input): Json<UpdateGraphMetadataInput>,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "graphs.write") {
        return response;
    }

    let started_at_ms = epoch_millis();
    let result = update_graph_metadata_service_async(&state.app, graph_id.clone(), input)
        .await
        .map(|graph| hosted_graph_entry_from_record(graph, true))
        .map_err(AppError::message);
    let record = match state.jobs.insert_finished(
        "update_graph_metadata",
        Some(graph_id),
        started_at_ms,
        result,
        "application/json",
    ) {
        Ok(record) => record,
        Err(error) => return loopback_app_error(error),
    };

    (
        StatusCode::ACCEPTED,
        Json(local_job_submit_response(&record)),
    )
        .into_response()
}

pub(super) async fn loopback_hosted_delete_graph(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    AxumPath(graph_id): AxumPath<String>,
    Query(query): Query<DeleteGraphQuery>,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "graphs.delete") {
        return response;
    }

    let started_at_ms = epoch_millis();
    let result = soft_delete_graph_service_async(&state.app, graph_id.clone(), query.hard)
        .await
        .map_err(AppError::message);
    let record = match state.jobs.insert_finished(
        "delete_graph",
        Some(graph_id),
        started_at_ms,
        result,
        "application/json",
    ) {
        Ok(record) => record,
        Err(error) => return loopback_app_error(error),
    };

    (
        StatusCode::ACCEPTED,
        Json(local_job_submit_response(&record)),
    )
        .into_response()
}

pub(super) async fn loopback_hosted_duplicate_graph(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    AxumPath(graph_id): AxumPath<String>,
    Json(input): Json<GraphDuplicateInput>,
) -> Response {
    if let Err(response) =
        require_loopback_scopes(&headers, &state, &["graphs.read", "graphs.write"])
    {
        return response;
    }

    let started_at_ms = epoch_millis();
    let result = duplicate_graph(
        state.app.clone(),
        graph_id.clone(),
        input.new_graph_id,
        input.new_title,
    )
    .await;
    let record = match state.jobs.insert_finished(
        "duplicate_graph",
        Some(graph_id),
        started_at_ms,
        result,
        "application/json",
    ) {
        Ok(record) => record,
        Err(error) => return loopback_app_error(error),
    };

    (
        StatusCode::ACCEPTED,
        Json(local_job_submit_response(&record)),
    )
        .into_response()
}
