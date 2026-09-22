use crate::{
    app_error::AppError,
    crdt_engine::persistence_coordinator::GraphPersistenceCoordinator,
    document_projection_service::{
        hosted_block_context_response, hosted_document_blob_response,
        hosted_document_blocks_response, hosted_document_export_response, hosted_document_response,
        hosted_document_summaries, hosted_workspace_blob_response,
    },
    loopback_document_inputs::DocumentExportQuery,
    loopback_http::{
        loopback_error, loopback_original_file_error, loopback_result, require_loopback_scope,
    },
    loopback_state::LoopbackState,
    original_file_service::{
        original_file_download_response, query_bool, read_document_original_file,
    },
};
use axum::{
    extract::{Path as AxumPath, Query, State},
    http::{HeaderMap, StatusCode},
    response::Response,
};
use std::{collections::BTreeMap, sync::Arc};
#[cfg(feature = "desktop")]
use tauri::Manager;

pub(super) async fn loopback_hosted_list_documents(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    AxumPath(graph_id): AxumPath<String>,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "documents.read") {
        return response;
    }
    loopback_result(hosted_document_summaries(&state.app, &graph_id))
}

pub(super) async fn loopback_hosted_read_document(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    AxumPath((graph_id, document_id)): AxumPath<(String, String)>,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "documents.read") {
        return response;
    }
    loopback_result(hosted_document_response(
        &state.app,
        &graph_id,
        &document_id,
    ))
}

pub(super) async fn loopback_hosted_document_blocks(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    AxumPath((graph_id, document_id)): AxumPath<(String, String)>,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "documents.read") {
        return response;
    }
    loopback_result(hosted_document_blocks_response(
        &state.app,
        &graph_id,
        &document_id,
    ))
}

pub(super) async fn loopback_hosted_export_document(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    AxumPath((graph_id, document_id)): AxumPath<(String, String)>,
    Query(query): Query<DocumentExportQuery>,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "documents.read") {
        return response;
    }
    match hosted_document_export_response(
        &state.app,
        &graph_id,
        &document_id,
        query.format.as_deref(),
        query.theme.as_deref(),
    ) {
        Ok(response) => response,
        Err(error) if error.contains("not found") => loopback_error(StatusCode::NOT_FOUND, &error),
        Err(error) => loopback_error(StatusCode::BAD_REQUEST, &error),
    }
}

pub(super) async fn loopback_hosted_workspace_blob(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    AxumPath(graph_id): AxumPath<String>,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "workspace.read") {
        return response;
    }
    // Keep the workspace bytes and graph-incarnation header in one lifecycle
    // snapshot. A same-ID graph replacement cannot race between reading the
    // Y.Doc and reading its identity fence.
    let _graph_lease = match state
        .app
        .state::<GraphPersistenceCoordinator>()
        .acquire_lifecycle_shared(&graph_id)
        .await
    {
        Ok(lease) => lease,
        Err(error) => return loopback_error(StatusCode::INTERNAL_SERVER_ERROR, &error),
    };
    match hosted_workspace_blob_response(&state.app, &graph_id) {
        Ok(response) => response,
        Err(error) if error.contains("not found") => loopback_error(StatusCode::NOT_FOUND, &error),
        Err(error) => loopback_error(StatusCode::BAD_REQUEST, &error),
    }
}

pub(super) async fn loopback_hosted_document_blob(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    AxumPath((graph_id, document_id)): AxumPath<(String, String)>,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "documents.read") {
        return response;
    }
    // The bytes and incarnation header are one identity-bearing snapshot.
    // Serialize them against delete/recreate so an old body can never be paired
    // with a replacement document's freshly-created incarnation token.
    let _graph_lease = match state
        .app
        .state::<GraphPersistenceCoordinator>()
        .acquire_lifecycle_shared(&graph_id)
        .await
    {
        Ok(lease) => lease,
        Err(error) => return loopback_error(StatusCode::INTERNAL_SERVER_ERROR, &error),
    };
    match hosted_document_blob_response(&state.app, &graph_id, &document_id) {
        Ok(response) => response,
        Err(error) if error.contains("not found") || error.contains("document tombstoned") => {
            loopback_error(StatusCode::NOT_FOUND, &error)
        }
        Err(error) => loopback_error(StatusCode::BAD_REQUEST, &error),
    }
}

pub(super) async fn loopback_hosted_block_context(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    AxumPath((graph_id, document_id)): AxumPath<(String, String)>,
    Query(params): Query<BTreeMap<String, String>>,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "documents.read") {
        return response;
    }
    let block_id = params
        .get("block_id")
        .or_else(|| params.get("blockId"))
        .map(String::as_str);
    loopback_result(hosted_block_context_response(
        &state.app,
        &graph_id,
        &document_id,
        block_id,
    ))
}

pub(super) async fn loopback_hosted_download_document_original(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    AxumPath((graph_id, document_id)): AxumPath<(String, String)>,
    Query(params): Query<BTreeMap<String, String>>,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "artifacts.read") {
        return response;
    }
    let inline = query_bool(&params, "inline");
    match read_document_original_file(&state.app, &graph_id, &document_id).and_then(
        |(manifest, bytes)| {
            original_file_download_response(&manifest, bytes, inline).map_err(AppError::storage)
        },
    ) {
        Ok(response) => response,
        Err(error) => loopback_original_file_error(error.message_ref()),
    }
}
