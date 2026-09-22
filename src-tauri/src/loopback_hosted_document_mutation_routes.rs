use crate::{
    clock::{duration_ms, epoch_millis},
    crdt_projection_flush::{flush_document_projection_phase, flush_graph_projection_phase},
    crdt_queue::{enqueue_crdt_operation_outcome, CrdtOperationQueue, EnqueueCrdtOperationInput},
    document_mutation_service::{current_document_revision, hosted_duplicate_document_result},
    document_projection_service::{hosted_document_response, hosted_document_write_payload},
    json_utils::json_u64,
    local_jobs::local_job_submit_response,
    loopback_document_inputs::{DocumentDescriptionInput, DocumentFlushQuery},
    loopback_http::{
        loopback_app_error, loopback_error, require_loopback_scope, require_loopback_scopes,
    },
    loopback_state::LoopbackState,
};
use axum::{
    extract::{Path as AxumPath, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use std::{sync::Arc, time::Instant};
#[cfg(feature = "desktop")]
use tauri::Manager;

pub(super) async fn loopback_hosted_put_document(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    AxumPath((graph_id, document_id)): AxumPath<(String, String)>,
    Json(input): Json<serde_json::Value>,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "documents.write.crdt") {
        return response;
    }

    if let Some(expected_revision) = json_u64(
        input
            .get("expectedRevision")
            .or_else(|| input.get("expected_revision")),
    ) {
        let current_revision = match current_document_revision(&state.app, &graph_id, &document_id)
        {
            Ok(revision) => revision,
            Err(error) => return loopback_error(StatusCode::BAD_REQUEST, &error),
        };
        if current_revision != expected_revision {
            return loopback_error(
                StatusCode::CONFLICT,
                &format!(
                    "revision mismatch: expected {expected_revision}, actual {current_revision}"
                ),
            );
        }
    }

    let payload = match hosted_document_write_payload(&document_id, input) {
        Ok(payload) => payload,
        Err(error) => return loopback_error(StatusCode::BAD_REQUEST, &error),
    };

    match enqueue_crdt_operation_outcome(
        state.app.clone(),
        EnqueueCrdtOperationInput {
            kind: "document.write".to_string(),
            graph_id: graph_id.clone(),
            document_id: Some(document_id.clone()),
            payload,
        },
    )
    .await
    {
        Ok(outcome) => {
            if let Err(error) = flush_document_projection_phase(
                state.app.clone(),
                &graph_id,
                &document_id,
                &outcome.operation_id,
                "documentPutWorkspaceFlushMs",
            )
            .await
            {
                return loopback_error(StatusCode::BAD_REQUEST, &error);
            }
            let readback_started = Instant::now();
            let response = hosted_document_response(&state.app, &graph_id, &document_id);
            let queue = state.app.state::<CrdtOperationQueue>();
            let _ = queue.add_phase(
                &outcome.operation_id,
                "hostedResponseReadbackMs",
                duration_ms(readback_started.elapsed()),
            );
            crate::loopback_http::loopback_result(response)
        }
        Err(error) => loopback_error(StatusCode::BAD_REQUEST, &error),
    }
}

pub(super) async fn loopback_hosted_set_document_description(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    AxumPath((graph_id, document_id)): AxumPath<(String, String)>,
    Json(input): Json<DocumentDescriptionInput>,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "workspace.write.crdt") {
        return response;
    }
    let target_graph_id = graph_id.clone();
    let target_document_id = document_id.clone();
    match enqueue_crdt_operation_outcome(
        state.app.clone(),
        EnqueueCrdtOperationInput {
            kind: "workspace.updateDocument".to_string(),
            graph_id,
            document_id: Some(document_id.clone()),
            payload: serde_json::json!({
                "documentId": document_id,
                "description": input.description,
                "describedAt": epoch_millis(),
            }),
        },
    )
    .await
    {
        Ok(outcome) => match flush_document_projection_phase(
            state.app.clone(),
            &target_graph_id,
            &target_document_id,
            &outcome.operation_id,
            "descriptionWorkspaceFlushMs",
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

pub(super) async fn loopback_hosted_delete_document(
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
            document_id: Some(document_id.clone()),
            payload: serde_json::json!({}),
        },
    )
    .await
    {
        Ok(outcome) => {
            if let Err(error) = flush_graph_projection_phase(
                state.app.clone(),
                &graph_id,
                &outcome.operation_id,
                "documentDeleteWorkspaceFlushMs",
            )
            .await
            {
                return loopback_error(StatusCode::BAD_REQUEST, &error);
            }
            let response_started = Instant::now();
            let response = serde_json::json!({
                "id": document_id,
                "graphId": graph_id,
                "status": "deleted",
            });
            let queue = state.app.state::<CrdtOperationQueue>();
            let _ = queue.add_phase(
                &outcome.operation_id,
                "hostedDeleteResponseBuildMs",
                duration_ms(response_started.elapsed()),
            );
            Json(response).into_response()
        }
        Err(error) => loopback_error(StatusCode::BAD_REQUEST, &error),
    }
}

pub(super) async fn loopback_hosted_duplicate_document(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    AxumPath((graph_id, document_id)): AxumPath<(String, String)>,
) -> Response {
    if let Err(response) = require_loopback_scopes(
        &headers,
        &state,
        &[
            "documents.read",
            "documents.write.crdt",
            "workspace.write.crdt",
        ],
    ) {
        return response;
    }
    let started_at_ms = epoch_millis();
    match hosted_duplicate_document_result(&state.app, &graph_id, &document_id).await {
        Ok(result) => match state.jobs.insert_finished(
            "duplicate_doc",
            Some(graph_id),
            started_at_ms,
            Ok(result),
            "application/json",
        ) {
            Ok(record) => (
                StatusCode::ACCEPTED,
                Json(local_job_submit_response(&record)),
            )
                .into_response(),
            Err(error) => loopback_app_error(error),
        },
        Err(error) if error.contains("not found") => loopback_error(StatusCode::NOT_FOUND, &error),
        Err(error) => loopback_error(StatusCode::BAD_REQUEST, &error),
    }
}

pub(super) async fn loopback_hosted_flush_document(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    AxumPath((graph_id, document_id)): AxumPath<(String, String)>,
    Query(query): Query<DocumentFlushQuery>,
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
            kind: "crdt.flush".to_string(),
            graph_id: graph_id.clone(),
            document_id: Some(document_id.clone()),
            payload: serde_json::json!({
                "includeMaterialization": query.include_materialization,
            }),
        },
    )
    .await
    {
        Ok(_) => Json(serde_json::json!({
            "id": document_id,
            "graphId": graph_id,
            "activeSession": true,
            "includeMaterialization": query.include_materialization,
            "status": "persisted",
        }))
        .into_response(),
        Err(error) if error.contains("not found") => loopback_error(StatusCode::NOT_FOUND, &error),
        Err(error) => loopback_error(StatusCode::BAD_REQUEST, &error),
    }
}
