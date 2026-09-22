use crate::{
    clock::epoch_millis,
    local_jobs::local_job_submit_response,
    loopback_graph_inputs::GraphExportInput,
    loopback_http::{loopback_app_error, loopback_error, require_loopback_scopes},
    loopback_state::LoopbackState,
    rdf_service::{dump_rdf, RdfDumpInput},
};
use axum::{
    extract::{Path as AxumPath, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use std::sync::Arc;

pub(super) async fn loopback_hosted_export_graph(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    AxumPath(graph_id): AxumPath<String>,
    Json(input): Json<GraphExportInput>,
) -> Response {
    if let Err(response) = require_loopback_scopes(&headers, &state, &["graphs.export", "rdf.dump"])
    {
        return response;
    }
    if input.include_artifacts {
        return loopback_error(
            StatusCode::BAD_REQUEST,
            "Graph export with artifacts is not yet supported. Retry with include_artifacts=false.",
        );
    }

    let started_at_ms = epoch_millis();
    let result = dump_rdf(
        state.app.clone(),
        RdfDumpInput {
            graph_id: graph_id.clone(),
            format: "trig".to_string(),
            source_graph_iri: None,
        },
    )
    .and_then(|dump| serde_json::to_value(dump).map_err(|error| error.to_string()));
    let record = match state.jobs.insert_finished(
        "export_graph",
        Some(graph_id),
        started_at_ms,
        result,
        "application/json",
    ) {
        Ok(record) => record,
        Err(error) => return loopback_app_error(error),
    };

    (
        StatusCode::ACCEPTED,
        Json(local_job_submit_response(&record)),
    )
        .into_response()
}
