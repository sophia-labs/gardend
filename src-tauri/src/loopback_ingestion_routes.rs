use crate::{
    loopback_http::{loopback_result, require_loopback_scope},
    loopback_state::LoopbackState,
    pdf_ingestion_commands::{
        get_docling_runtime_status, get_pdf_ingestion_pipeline_status, list_ingestion_approaches,
        prepare_docling_runtime, set_pdf_ingestion_pipeline_config,
    },
    pdf_pipeline::PdfIngestionPipelineConfigInput,
};
use axum::{
    extract::State,
    http::HeaderMap,
    response::Response,
    routing::{get, post},
    Json, Router,
};
use std::sync::Arc;

pub(super) fn loopback_ingestion_config_router() -> Router<Arc<LoopbackState>> {
    Router::new()
        .route(
            "/api/artifacts/ingestion/approaches",
            get(loopback_ingestion_approaches),
        )
        .route(
            "/api/artifacts/ingestion/docling/status",
            get(loopback_docling_runtime_status),
        )
        .route(
            "/api/artifacts/ingestion/pdf/pipeline",
            get(loopback_pdf_ingestion_pipeline_status)
                .put(loopback_set_pdf_ingestion_pipeline_config),
        )
        .route(
            "/api/artifacts/ingestion/docling/prepare",
            post(loopback_prepare_docling_runtime),
        )
}

pub(super) async fn loopback_ingestion_approaches(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "ingestion.config.read") {
        return response;
    }
    loopback_result(list_ingestion_approaches(state.app.clone()))
}

pub(super) async fn loopback_docling_runtime_status(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "ingestion.config.read") {
        return response;
    }
    loopback_result(get_docling_runtime_status(state.app.clone()))
}

pub(super) async fn loopback_pdf_ingestion_pipeline_status(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "ingestion.config.read") {
        return response;
    }
    loopback_result(get_pdf_ingestion_pipeline_status(state.app.clone()))
}

pub(super) async fn loopback_set_pdf_ingestion_pipeline_config(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    Json(input): Json<PdfIngestionPipelineConfigInput>,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "ingestion.config.write") {
        return response;
    }
    loopback_result(set_pdf_ingestion_pipeline_config(state.app.clone(), input))
}

pub(super) async fn loopback_prepare_docling_runtime(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "ingestion.config.write") {
        return response;
    }
    loopback_result(prepare_docling_runtime(state.app.clone()))
}
