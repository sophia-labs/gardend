use crate::{
    document_service::{list_documents, read_document},
    loopback_document_history_routes::{
        loopback_hosted_copy_document_snapshot, loopback_hosted_create_manual_document_snapshot,
        loopback_hosted_delete_document_snapshot, loopback_hosted_document_snapshot_count,
        loopback_hosted_document_snapshot_html, loopback_hosted_document_snapshot_text,
        loopback_hosted_list_document_snapshots,
        loopback_legacy_history_list, loopback_legacy_history_get,
    },
    loopback_hosted_document_mutation_routes::{
        loopback_hosted_delete_document, loopback_hosted_duplicate_document,
        loopback_hosted_flush_document, loopback_hosted_put_document,
        loopback_hosted_set_document_description,
    },
    loopback_hosted_document_read_routes::{
        loopback_hosted_block_context, loopback_hosted_document_blob,
        loopback_hosted_document_blocks, loopback_hosted_download_document_original,
        loopback_hosted_export_document, loopback_hosted_list_documents,
        loopback_hosted_read_document, loopback_hosted_workspace_blob,
    },
    loopback_http::{loopback_result, require_loopback_scope},
    loopback_state::LoopbackState,
    loopback_workspace_document_routes::{
        loopback_create_workspace_document, loopback_create_workspace_folder,
        loopback_delete_workspace_document, loopback_move_workspace_documents,
    },
};
use axum::{
    extract::{Path as AxumPath, State},
    http::HeaderMap,
    response::Response,
    routing::{delete, get, patch, post},
    Router,
};
use std::sync::Arc;

pub(super) fn loopback_document_router() -> Router<Arc<LoopbackState>> {
    Router::new()
        .route("/v1/graphs/{graph_id}/legacy-history",get(loopback_legacy_history_list))
        .route("/v1/documents/{graph_id}/{doc_id}/legacy-history",get(loopback_legacy_history_list))
        .route("/v1/documents/{graph_id}/{doc_id}/legacy-history/{snapshot_id}",get(loopback_legacy_history_get))
        .route("/v1/documents/{graph_id}/{doc_id}/legacy-history/{snapshot_id}/{view}",get(loopback_legacy_history_get))
        .route(
            "/api/graphs/{graph_id}/documents",
            get(loopback_list_documents).post(loopback_create_workspace_document),
        )
        .route(
            "/api/graphs/{graph_id}/documents/move",
            post(loopback_move_workspace_documents),
        )
        .route(
            "/api/graphs/{graph_id}/folders",
            post(loopback_create_workspace_folder),
        )
        .route(
            "/api/graphs/{graph_id}/documents/{document_id}",
            get(loopback_read_document).delete(loopback_delete_workspace_document),
        )
        .route("/documents/{graph_id}", get(loopback_hosted_list_documents))
        .route(
            "/documents/{graph_id}/{document_id}",
            get(loopback_hosted_read_document)
                .put(loopback_hosted_put_document)
                .delete(loopback_hosted_delete_document),
        )
        .route(
            "/documents/{graph_id}/workspace/blob",
            get(loopback_hosted_workspace_blob),
        )
        .route(
            "/documents/{graph_id}/{document_id}/blob",
            get(loopback_hosted_document_blob),
        )
        .route(
            "/documents/{graph_id}/{document_id}/blocks",
            get(loopback_hosted_document_blocks),
        )
        .route(
            "/documents/{graph_id}/{document_id}/export",
            get(loopback_hosted_export_document),
        )
        .route(
            "/documents/{graph_id}/{document_id}/description",
            patch(loopback_hosted_set_document_description),
        )
        .route(
            "/documents/{graph_id}/{document_id}/duplicate",
            post(loopback_hosted_duplicate_document),
        )
        .route(
            "/documents/{graph_id}/{document_id}/flush",
            post(loopback_hosted_flush_document),
        )
        .route(
            "/documents/{graph_id}/{document_id}/block-context",
            get(loopback_hosted_block_context),
        )
        .route(
            "/v1/documents/{graph_id}/{doc_id}/snapshots",
            get(loopback_hosted_list_document_snapshots)
                .post(loopback_hosted_create_manual_document_snapshot),
        )
        .route(
            "/v1/documents/{graph_id}/{doc_id}/snapshots/count",
            get(loopback_hosted_document_snapshot_count),
        )
        .route(
            "/v1/documents/{graph_id}/{doc_id}/snapshots/{snapshot_id}/html",
            get(loopback_hosted_document_snapshot_html),
        )
        .route(
            "/v1/documents/{graph_id}/{doc_id}/snapshots/{snapshot_id}/text",
            get(loopback_hosted_document_snapshot_text),
        )
        .route(
            "/v1/documents/{graph_id}/{doc_id}/snapshots/{snapshot_id}/copy",
            post(loopback_hosted_copy_document_snapshot),
        )
        .route(
            "/v1/documents/{graph_id}/{doc_id}/snapshots/{snapshot_id}",
            delete(loopback_hosted_delete_document_snapshot),
        )
        .route(
            "/artifacts/{graph_id}/documents/{document_id}/download-original",
            get(loopback_hosted_download_document_original),
        )
}

pub(super) async fn loopback_list_documents(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    AxumPath(graph_id): AxumPath<String>,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "documents.read") {
        return response;
    }
    loopback_result(list_documents(state.app.clone(), graph_id))
}

pub(super) async fn loopback_read_document(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    AxumPath((graph_id, document_id)): AxumPath<(String, String)>,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "documents.read") {
        return response;
    }
    loopback_result(read_document(state.app.clone(), graph_id, document_id))
}
