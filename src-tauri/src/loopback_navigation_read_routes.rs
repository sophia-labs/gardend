use crate::{
    hosted_navigation_projection::{hosted_navigation_parts, hosted_navigation_response},
    json_utils::json_string,
    local_jobs::local_job_graph_id,
    loopback_http::{loopback_app_error, loopback_error, loopback_result, require_loopback_scope},
    loopback_job_responses::local_job_result_response,
    loopback_state::LoopbackState,
};
use axum::{
    extract::{Path as AxumPath, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use std::sync::Arc;

pub(super) async fn loopback_hosted_navigation(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    AxumPath(graph_id): AxumPath<String>,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "workspace.read") {
        return response;
    }
    loopback_result(hosted_navigation_response(&state.app, &graph_id))
}

pub(super) async fn loopback_hosted_navigation_job_result(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    AxumPath((graph_id, job_id)): AxumPath<(String, String)>,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "jobs.read") {
        return response;
    }
    if let Err(error) = state.cell_graph.authorize_job_id(&state.jobs, &job_id) {
        return loopback_error(StatusCode::NOT_FOUND, &error.mcp_message());
    }

    let record = match state.jobs.get(&job_id) {
        Ok(Some(record)) => record,
        Ok(None) => return loopback_error(StatusCode::NOT_FOUND, "job not found"),
        Err(error) => return loopback_app_error(error),
    };

    if let Some(record_graph_id) = local_job_graph_id(&record) {
        if record_graph_id != graph_id {
            return loopback_error(StatusCode::FORBIDDEN, "job not valid for this graph");
        }
    }
    local_job_result_response(&state.jobs, &record)
}

pub(super) async fn loopback_hosted_navigation_folders(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    AxumPath(graph_id): AxumPath<String>,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "workspace.read") {
        return response;
    }
    loopback_result(hosted_navigation_parts(&state.app, &graph_id).map(|(folders, _, _)| folders))
}

pub(super) async fn loopback_hosted_navigation_folder(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    AxumPath((graph_id, folder_id)): AxumPath<(String, String)>,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "workspace.read") {
        return response;
    }
    match hosted_navigation_parts(&state.app, &graph_id).and_then(|(folders, _, _)| {
        folders
            .into_iter()
            .find(|folder| json_string(folder.get("id")).as_deref() == Some(folder_id.as_str()))
            .ok_or_else(|| format!("folder {folder_id} not found"))
    }) {
        Ok(folder) => Json(folder).into_response(),
        Err(error) if error.contains("not found") => loopback_error(StatusCode::NOT_FOUND, &error),
        Err(error) => loopback_error(StatusCode::BAD_REQUEST, &error),
    }
}

pub(super) async fn loopback_hosted_navigation_artifacts(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    AxumPath(graph_id): AxumPath<String>,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "artifacts.read") {
        return response;
    }
    loopback_result(
        hosted_navigation_parts(&state.app, &graph_id).map(|(_, _, artifacts)| artifacts),
    )
}

pub(super) async fn loopback_hosted_navigation_artifact(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    AxumPath((graph_id, artifact_id)): AxumPath<(String, String)>,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "artifacts.read") {
        return response;
    }
    match hosted_navigation_parts(&state.app, &graph_id).and_then(|(_, _, artifacts)| {
        artifacts
            .into_iter()
            .find(|artifact| {
                json_string(artifact.get("id")).as_deref() == Some(artifact_id.as_str())
            })
            .ok_or_else(|| format!("artifact {artifact_id} not found"))
    }) {
        Ok(artifact) => Json(artifact).into_response(),
        Err(error) if error.contains("not found") => loopback_error(StatusCode::NOT_FOUND, &error),
        Err(error) => loopback_error(StatusCode::BAD_REQUEST, &error),
    }
}
