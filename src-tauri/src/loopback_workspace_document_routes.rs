use crate::{
    crdt_projection_flush::flush_graph_projection_phase,
    crdt_queue::{enqueue_crdt_operation_outcome, EnqueueCrdtOperationInput},
    loopback_document_inputs::{
        WorkspaceCreateDocumentInput, WorkspaceCreateFolderInput, WorkspaceMoveDocumentsInput,
    },
    loopback_http::{
        loopback_error, loopback_result, require_loopback_scope, require_loopback_scopes,
    },
    loopback_state::LoopbackState,
};
use axum::{
    extract::{Path as AxumPath, State},
    http::{HeaderMap, StatusCode},
    response::Response,
    Json,
};
use std::sync::Arc;

pub(super) async fn loopback_create_workspace_document(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    AxumPath(graph_id): AxumPath<String>,
    Json(input): Json<WorkspaceCreateDocumentInput>,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "workspace.write.crdt") {
        return response;
    }
    let payload = match serde_json::to_value(input) {
        Ok(value) => value,
        Err(error) => return loopback_error(StatusCode::BAD_REQUEST, &error.to_string()),
    };
    match enqueue_crdt_operation_outcome(
        state.app.clone(),
        EnqueueCrdtOperationInput {
            kind: "workspace.createDocument".to_string(),
            graph_id: graph_id.clone(),
            document_id: None,
            payload,
        },
    )
    .await
    {
        Ok(outcome) => match flush_graph_projection_phase(
            state.app.clone(),
            &graph_id,
            &outcome.operation_id,
            "workspaceCreateDocumentFlushMs",
        )
        .await
        {
            Ok(()) => loopback_result(Ok(outcome.value)),
            Err(error) => loopback_error(StatusCode::BAD_REQUEST, &error),
        },
        Err(error) => loopback_error(StatusCode::BAD_REQUEST, &error),
    }
}

pub(super) async fn loopback_move_workspace_documents(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    AxumPath(graph_id): AxumPath<String>,
    Json(input): Json<WorkspaceMoveDocumentsInput>,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "workspace.write.crdt") {
        return response;
    }
    let payload = match serde_json::to_value(input) {
        Ok(value) => value,
        Err(error) => return loopback_error(StatusCode::BAD_REQUEST, &error.to_string()),
    };
    match enqueue_crdt_operation_outcome(
        state.app.clone(),
        EnqueueCrdtOperationInput {
            kind: "workspace.moveDocuments".to_string(),
            graph_id: graph_id.clone(),
            document_id: None,
            payload,
        },
    )
    .await
    {
        Ok(outcome) => match flush_graph_projection_phase(
            state.app.clone(),
            &graph_id,
            &outcome.operation_id,
            "workspaceMoveDocumentsFlushMs",
        )
        .await
        {
            Ok(()) => loopback_result(Ok(outcome.value)),
            Err(error) => loopback_error(StatusCode::BAD_REQUEST, &error),
        },
        Err(error) => loopback_error(StatusCode::BAD_REQUEST, &error),
    }
}

pub(super) async fn loopback_create_workspace_folder(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    AxumPath(graph_id): AxumPath<String>,
    Json(input): Json<WorkspaceCreateFolderInput>,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "workspace.write.crdt") {
        return response;
    }
    let payload = match serde_json::to_value(input) {
        Ok(value) => value,
        Err(error) => return loopback_error(StatusCode::BAD_REQUEST, &error.to_string()),
    };
    match enqueue_crdt_operation_outcome(
        state.app.clone(),
        EnqueueCrdtOperationInput {
            kind: "workspace.createFolder".to_string(),
            graph_id: graph_id.clone(),
            document_id: None,
            payload,
        },
    )
    .await
    {
        Ok(outcome) => match flush_graph_projection_phase(
            state.app.clone(),
            &graph_id,
            &outcome.operation_id,
            "workspaceCreateFolderFlushMs",
        )
        .await
        {
            Ok(()) => loopback_result(Ok(outcome.value)),
            Err(error) => loopback_error(StatusCode::BAD_REQUEST, &error),
        },
        Err(error) => loopback_error(StatusCode::BAD_REQUEST, &error),
    }
}

pub(super) async fn loopback_delete_workspace_document(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    AxumPath((graph_id, document_id)): AxumPath<(String, String)>,
) -> Response {
    if let Err(response) = require_loopback_scopes(
        &headers,
        &state,
        &["documents.delete.crdt", "workspace.delete.crdt"],
    ) {
        return response;
    }
    match enqueue_crdt_operation_outcome(
        state.app.clone(),
        EnqueueCrdtOperationInput {
            kind: "workspace.deleteDocument".to_string(),
            graph_id: graph_id.clone(),
            document_id: Some(document_id),
            payload: serde_json::json!({}),
        },
    )
    .await
    {
        Ok(outcome) => match flush_graph_projection_phase(
            state.app.clone(),
            &graph_id,
            &outcome.operation_id,
            "workspaceDeleteDocumentFlushMs",
        )
        .await
        {
            Ok(()) => loopback_result(Ok(outcome.value)),
            Err(error) => loopback_error(StatusCode::BAD_REQUEST, &error),
        },
        Err(error) => loopback_error(StatusCode::BAD_REQUEST, &error),
    }
}
