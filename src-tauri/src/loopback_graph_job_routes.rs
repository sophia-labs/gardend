use crate::{
    cell_graph_boundary::CellGraphJson,
    clock::epoch_millis,
    local_jobs::{
        local_graph_query_submit_response, local_job_status_response, local_job_submit_response,
    },
    loopback_graph_inputs::LocalGraphQueryRequest,
    loopback_http::{
        loopback_app_error, loopback_app_result, loopback_error, require_loopback_scope,
    },
    loopback_job_responses::local_job_result_response,
    loopback_state::LoopbackState,
    rdf_service::{SparqlInput, SparqlUpdateInput},
    sparql_admission::{
        run_external_sparql_query, run_external_sparql_update, ExternalSparqlOptions,
    },
};
use axum::{
    extract::{Path as AxumPath, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use std::sync::Arc;

pub(super) async fn loopback_graph_query_job(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    CellGraphJson(input): CellGraphJson<LocalGraphQueryRequest>,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "rdf.query") {
        return response;
    }

    let graph_id = match input.graph_id() {
        Ok(graph_id) => graph_id,
        Err(error) => return loopback_error(StatusCode::BAD_REQUEST, &error),
    };
    let sparql = match input.sparql() {
        Ok(sparql) => sparql,
        Err(error) => return loopback_error(StatusCode::BAD_REQUEST, &error),
    };
    let _result_format = input.result_format.as_deref().unwrap_or("json");
    loopback_app_result(
        run_external_sparql_query(
            state.app.clone(),
            SparqlInput {
                graph_id,
                query: sparql,
            },
            ExternalSparqlOptions {
                timeout_ms: input.timeout_ms,
                max_rows: input.max_rows,
            },
        )
        .await,
    )
}

pub(super) async fn loopback_graph_update_job(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    CellGraphJson(input): CellGraphJson<LocalGraphQueryRequest>,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "rdf.update") {
        return response;
    }

    let graph_id = match input.graph_id() {
        Ok(graph_id) => graph_id,
        Err(error) => return loopback_error(StatusCode::BAD_REQUEST, &error),
    };
    let sparql = match input.sparql() {
        Ok(sparql) => sparql,
        Err(error) => return loopback_error(StatusCode::BAD_REQUEST, &error),
    };
    if input.timeout_ms.is_some() {
        return loopback_error(
            StatusCode::BAD_REQUEST,
            "timeout_ms is not accepted for SPARQL updates: Oxigraph 0.5.9 cannot cooperatively cancel every update operation",
        );
    }
    if input.max_rows.is_some() {
        return loopback_error(
            StatusCode::BAD_REQUEST,
            "max_rows applies only to SPARQL queries",
        );
    }

    loopback_app_result(
        run_external_sparql_update(
            state.app.clone(),
            SparqlUpdateInput {
                graph_id,
                update: sparql,
            },
        )
        .await,
    )
}

/*
 * `/graphs/query` and `/graphs/update` used to execute synchronously, insert
 * an already-finished job record, and only then return HTTP 202. That facade
 * was neither asynchronous nor cancellable. These handlers now await the real
 * bounded blocking worker and return ordinary HTTP 200 via
 * `loopback_app_result`. The generic job routes below remain for real jobs.
 */

pub(super) async fn loopback_graph_job_status(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    AxumPath(job_id): AxumPath<String>,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "jobs.read") {
        return response;
    }
    if let Err(error) = state.cell_graph.authorize_job_id(&state.jobs, &job_id) {
        return loopback_error(StatusCode::NOT_FOUND, &error.mcp_message());
    }

    match state.jobs.get(&job_id) {
        Ok(Some(record)) => Json(local_job_status_response(&record)).into_response(),
        Ok(None) => loopback_error(StatusCode::NOT_FOUND, "job not found"),
        Err(error) => loopback_app_error(error),
    }
}

pub(super) async fn loopback_graph_job_cancel(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    AxumPath(job_id): AxumPath<String>,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "jobs.cancel") {
        return response;
    }
    if let Err(error) = state.cell_graph.authorize_job_id(&state.jobs, &job_id) {
        return loopback_error(StatusCode::NOT_FOUND, &error.mcp_message());
    }

    match state.jobs.cancel(&job_id) {
        Ok(Some(response)) => Json(response).into_response(),
        Ok(None) => loopback_error(StatusCode::NOT_FOUND, "job not found"),
        Err(error) => loopback_app_error(error),
    }
}

pub(super) async fn loopback_graph_job_result(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    AxumPath(job_id): AxumPath<String>,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "jobs.read") {
        return response;
    }
    if let Err(error) = state.cell_graph.authorize_job_id(&state.jobs, &job_id) {
        return loopback_error(StatusCode::NOT_FOUND, &error.mcp_message());
    }

    let record = match state.jobs.get(&job_id) {
        Ok(Some(record)) => record,
        Ok(None) => return loopback_error(StatusCode::NOT_FOUND, "result not available"),
        Err(error) => return loopback_app_error(error),
    };

    local_job_result_response(&state.jobs, &record)
}
