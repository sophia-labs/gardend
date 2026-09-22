use crate::{
    loopback_http::{loopback_error, require_loopback_scope},
    loopback_state::LoopbackState,
    paths::existing_graph_dir,
    pdf_ingest_payloads::pdf_accurate_upload_response,
    pdf_ingest_submission::submit_pdf_accurate_ingest,
    pdf_ingest_upload::read_pdf_accurate_upload,
};
use axum::{
    extract::{Multipart, Path as AxumPath, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use std::sync::Arc;

pub(super) async fn loopback_hosted_ingest_pdf_accurate(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    AxumPath(graph_id): AxumPath<String>,
    multipart: Multipart,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "artifacts.ingest") {
        return response;
    }

    let graph_dir = match existing_graph_dir(&state.app, &graph_id) {
        Ok(graph_dir) => graph_dir,
        Err(error) => return loopback_error(StatusCode::NOT_FOUND, &error),
    };
    let upload = match read_pdf_accurate_upload(&graph_dir, multipart).await {
        Ok(upload) => upload,
        Err(error) => return loopback_error(error.status, &error.message),
    };
    let (record, document_id) =
        match submit_pdf_accurate_ingest(state.clone(), graph_id, upload).await {
            Ok(result) => result,
            Err(error) => return loopback_error(StatusCode::INTERNAL_SERVER_ERROR, &error),
        };

    match pdf_accurate_upload_response(&record, &document_id) {
        Ok(value) => (StatusCode::ACCEPTED, Json(value)).into_response(),
        Err(error) => loopback_error(StatusCode::INTERNAL_SERVER_ERROR, &error),
    }
}
