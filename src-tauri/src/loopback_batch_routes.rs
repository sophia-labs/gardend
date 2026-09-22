use crate::{
    crdt_projection_flush::{flush_document_projection_phase, flush_graph_projection_phase},
    crdt_queue::{enqueue_crdt_operation_outcome, EnqueueCrdtOperationInput},
    json_utils::json_string,
    loopback_http::{loopback_error, require_loopback_scopes},
    loopback_state::LoopbackState,
};
use axum::{
    extract::{Path as AxumPath, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use std::sync::Arc;

fn first_batch_document_id(input: &serde_json::Value) -> Option<String> {
    input
        .get("documents")
        .and_then(serde_json::Value::as_array)
        .and_then(|documents| documents.first())
        .and_then(|document| {
            json_string(
                document
                    .get("documentId")
                    .or_else(|| document.get("document_id")),
            )
        })
}

pub(super) async fn loopback_hosted_batch_prepare(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    AxumPath(graph_id): AxumPath<String>,
    Json(input): Json<serde_json::Value>,
) -> Response {
    if let Err(response) = require_loopback_scopes(
        &headers,
        &state,
        &["documents.write.crdt", "workspace.write.crdt"],
    ) {
        return response;
    }
    match enqueue_crdt_operation_outcome(
        state.app.clone(),
        EnqueueCrdtOperationInput {
            kind: "document.batchPrepare".to_string(),
            graph_id: graph_id.clone(),
            document_id: None,
            payload: input,
        },
    )
    .await
    {
        Ok(outcome) => match flush_graph_projection_phase(
            state.app.clone(),
            &graph_id,
            &outcome.operation_id,
            "batchPrepareWorkspaceFlushMs",
        )
        .await
        {
            Ok(()) => (StatusCode::CREATED, Json(outcome.value)).into_response(),
            Err(error) => loopback_error(StatusCode::BAD_REQUEST, &error),
        },
        Err(error) => loopback_error(StatusCode::BAD_REQUEST, &error),
    }
}

pub(super) async fn loopback_hosted_batch_register(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    AxumPath(graph_id): AxumPath<String>,
    Json(input): Json<serde_json::Value>,
) -> Response {
    if let Err(response) = require_loopback_scopes(
        &headers,
        &state,
        &["documents.write.crdt", "workspace.write.crdt"],
    ) {
        return response;
    }
    let target_graph_id = graph_id.clone();
    let target_document_id = first_batch_document_id(&input);
    match enqueue_crdt_operation_outcome(
        state.app.clone(),
        EnqueueCrdtOperationInput {
            kind: "document.batchRegister".to_string(),
            graph_id,
            document_id: None,
            payload: input,
        },
    )
    .await
    {
        Ok(outcome) => {
            let flush_result = match target_document_id.as_deref() {
                Some(document_id) => {
                    flush_document_projection_phase(
                        state.app.clone(),
                        &target_graph_id,
                        document_id,
                        &outcome.operation_id,
                        "batchWorkspaceFlushMs",
                    )
                    .await
                }
                None => {
                    flush_graph_projection_phase(
                        state.app.clone(),
                        &target_graph_id,
                        &outcome.operation_id,
                        "batchWorkspaceFlushMs",
                    )
                    .await
                }
            };
            match flush_result {
                Ok(()) => Json(outcome.value).into_response(),
                Err(error) => loopback_error(StatusCode::BAD_REQUEST, &error),
            }
        }
        Err(error) => loopback_error(StatusCode::BAD_REQUEST, &error),
    }
}
