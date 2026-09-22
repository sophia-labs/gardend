use crate::{
    graph_projection_service::{
        hosted_workspace_properties, hosted_workspace_summary, hosted_workspace_viz,
    },
    loopback_graph_inputs::{HostedGraphListQuery, HostedWorkspaceVizQuery},
    loopback_http::{loopback_result, require_loopback_scope},
    loopback_state::LoopbackState,
};
use axum::{
    extract::{Path as AxumPath, Query, State},
    http::HeaderMap,
    response::Response,
};
use std::sync::Arc;

pub(super) async fn loopback_hosted_workspace_summary(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    AxumPath(graph_id): AxumPath<String>,
    Query(_query): Query<HostedGraphListQuery>,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "graphs.read") {
        return response;
    }
    loopback_result(hosted_workspace_summary(state.app.clone(), &graph_id))
}

pub(super) async fn loopback_hosted_workspace_properties(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    AxumPath(graph_id): AxumPath<String>,
    Query(_query): Query<HostedGraphListQuery>,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "graphs.read") {
        return response;
    }
    loopback_result(hosted_workspace_properties(state.app.clone(), &graph_id))
}

pub(super) async fn loopback_hosted_workspace_viz(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    AxumPath(graph_id): AxumPath<String>,
    Query(query): Query<HostedWorkspaceVizQuery>,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "graphs.read") {
        return response;
    }
    let _wait_ms = query.wait_ms;
    let edge_limit = query.limit_edges.unwrap_or(200).clamp(1, 5000);
    loopback_result(hosted_workspace_viz(
        state.app.clone(),
        &graph_id,
        query.limit_nodes,
        edge_limit,
    ))
}
