use crate::{
    crdt_projection_flush::flush_graph_projection_phase,
    crdt_queue::{enqueue_crdt_operation_outcome, EnqueueCrdtOperationInput},
    loopback_http::{loopback_error, loopback_result, require_loopback_scope},
    loopback_state::LoopbackState,
    wire_projection_service::{
        hosted_wire_bundle, hosted_wire_predicates, hosted_wired_blocks, hosted_wires_for_document,
    },
};
use axum::{
    extract::{Path as AxumPath, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{delete, get, post},
    Json, Router,
};
use std::sync::Arc;

pub(super) fn loopback_wire_router() -> Router<Arc<LoopbackState>> {
    Router::new()
        .route(
            "/wires/{graph_id}/predicates",
            get(loopback_hosted_wire_predicates),
        )
        .route(
            "/wires/{graph_id}/document/{document_id}",
            post(loopback_hosted_create_wire),
        )
        .route(
            "/wires/{graph_id}/document/{document_id}/outgoing",
            get(loopback_hosted_outgoing_wires),
        )
        .route(
            "/wires/{graph_id}/document/{document_id}/incoming",
            get(loopback_hosted_incoming_wires),
        )
        .route(
            "/wires/{graph_id}/document/{document_id}/bundle",
            get(loopback_hosted_wire_bundle),
        )
        .route(
            "/wires/{graph_id}/document/{document_id}/wired-blocks",
            get(loopback_hosted_wired_blocks),
        )
        .route(
            "/wires/{graph_id}/document/{document_id}/has-incoming",
            get(loopback_hosted_has_incoming_wires),
        )
        .route(
            "/wires/{graph_id}/{wire_id}/refresh",
            post(loopback_hosted_refresh_wire),
        )
        .route(
            "/wires/{graph_id}/{wire_id}",
            delete(loopback_hosted_delete_wire),
        )
}

pub(super) async fn loopback_hosted_wire_predicates(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    AxumPath(graph_id): AxumPath<String>,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "wires.read") {
        return response;
    }
    loopback_result(hosted_wire_predicates(&state.app, &graph_id))
}

pub(super) async fn loopback_hosted_create_wire(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    AxumPath((graph_id, document_id)): AxumPath<(String, String)>,
    Json(input): Json<serde_json::Value>,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "wires.write") {
        return response;
    }
    if let Err(error) = state.cell_graph.authorize_json_graph_references(&input) {
        return error.http_response();
    }
    match enqueue_crdt_operation_outcome(
        state.app.clone(),
        EnqueueCrdtOperationInput {
            kind: "workspace.createWire".to_string(),
            graph_id: graph_id.clone(),
            document_id: Some(document_id),
            payload: input,
        },
    )
    .await
    {
        Ok(outcome) => match flush_graph_projection_phase(
            state.app.clone(),
            &graph_id,
            &outcome.operation_id,
            "wireCreateWorkspaceFlushMs",
        )
        .await
        {
            Ok(()) => (StatusCode::CREATED, Json(outcome.value)).into_response(),
            Err(error) => loopback_error(StatusCode::BAD_REQUEST, &error),
        },
        Err(error) => loopback_error(StatusCode::BAD_REQUEST, &error),
    }
}

pub(super) async fn loopback_hosted_outgoing_wires(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    AxumPath((graph_id, document_id)): AxumPath<(String, String)>,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "wires.read") {
        return response;
    }
    loopback_result(hosted_wires_for_document(
        state.app.clone(),
        &graph_id,
        &document_id,
        "outgoing",
    ))
}

pub(super) async fn loopback_hosted_incoming_wires(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    AxumPath((graph_id, document_id)): AxumPath<(String, String)>,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "wires.read") {
        return response;
    }
    loopback_result(hosted_wires_for_document(
        state.app.clone(),
        &graph_id,
        &document_id,
        "incoming",
    ))
}

pub(super) async fn loopback_hosted_wire_bundle(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    AxumPath((graph_id, document_id)): AxumPath<(String, String)>,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "wires.read") {
        return response;
    }
    loopback_result(hosted_wire_bundle(
        state.app.clone(),
        &graph_id,
        &document_id,
    ))
}

pub(super) async fn loopback_hosted_wired_blocks(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    AxumPath((graph_id, document_id)): AxumPath<(String, String)>,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "wires.read") {
        return response;
    }
    loopback_result(
        hosted_wired_blocks(&state.app, &graph_id, &document_id)
            .map(|block_ids| serde_json::json!({ "block_ids": block_ids })),
    )
}

pub(super) async fn loopback_hosted_has_incoming_wires(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    AxumPath((graph_id, document_id)): AxumPath<(String, String)>,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "wires.read") {
        return response;
    }
    loopback_result(
        hosted_wires_for_document(state.app.clone(), &graph_id, &document_id, "incoming")
            .map(|wires| !wires.as_array().map(Vec::is_empty).unwrap_or(true)),
    )
}

pub(super) async fn loopback_hosted_refresh_wire(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    AxumPath((graph_id, wire_id)): AxumPath<(String, String)>,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "wires.write") {
        return response;
    }
    match enqueue_crdt_operation_outcome(
        state.app.clone(),
        EnqueueCrdtOperationInput {
            kind: "workspace.refreshWire".to_string(),
            graph_id: graph_id.clone(),
            document_id: None,
            payload: serde_json::json!({ "wireId": wire_id }),
        },
    )
    .await
    {
        Ok(outcome) => match flush_graph_projection_phase(
            state.app.clone(),
            &graph_id,
            &outcome.operation_id,
            "wireRefreshWorkspaceFlushMs",
        )
        .await
        {
            Ok(()) => Json(outcome.value).into_response(),
            Err(error) => loopback_error(StatusCode::BAD_REQUEST, &error),
        },
        Err(error) if error.contains("not found") => loopback_error(StatusCode::NOT_FOUND, &error),
        Err(error) => loopback_error(StatusCode::BAD_REQUEST, &error),
    }
}

pub(super) async fn loopback_hosted_delete_wire(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    AxumPath((graph_id, wire_id)): AxumPath<(String, String)>,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "wires.delete") {
        return response;
    }
    match enqueue_crdt_operation_outcome(
        state.app.clone(),
        EnqueueCrdtOperationInput {
            kind: "workspace.deleteWire".to_string(),
            graph_id: graph_id.clone(),
            document_id: None,
            payload: serde_json::json!({ "wireId": wire_id }),
        },
    )
    .await
    {
        Ok(outcome) => match flush_graph_projection_phase(
            state.app.clone(),
            &graph_id,
            &outcome.operation_id,
            "wireDeleteWorkspaceFlushMs",
        )
        .await
        {
            Ok(()) => StatusCode::NO_CONTENT.into_response(),
            Err(error) => loopback_error(StatusCode::BAD_REQUEST, &error),
        },
        Err(error) if error.contains("not found") => loopback_error(StatusCode::NOT_FOUND, &error),
        Err(error) => loopback_error(StatusCode::BAD_REQUEST, &error),
    }
}
