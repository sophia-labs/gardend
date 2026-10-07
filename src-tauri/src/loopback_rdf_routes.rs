use crate::{
    cell_graph_boundary::CellGraphJson,
    loopback_graph_job_routes::{loopback_graph_query_job, loopback_graph_update_job},
    loopback_http::{loopback_app_result, require_loopback_scope},
    loopback_state::LoopbackState,
    rdf_service::{dump_rdf_service, load_rdf_service, RdfDumpInput, RdfLoadInput},
};
use axum::{extract::State, http::HeaderMap, response::Response, routing::post, Router};
use std::sync::Arc;

pub(super) fn loopback_rdf_router() -> Router<Arc<LoopbackState>> {
    Router::new()
        .route("/api/sparql/query", post(loopback_graph_query_job))
        .route("/api/sparql/update", post(loopback_graph_update_job))
        .route("/api/rdf/load", post(loopback_rdf_load))
        .route("/api/rdf/dump", post(loopback_rdf_dump))
}

pub(super) async fn loopback_rdf_load(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    CellGraphJson(input): CellGraphJson<RdfLoadInput>,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "rdf.load") {
        return response;
    }
    // Recorded in the source ledger on a graph under source authority
    // (`source_sync::record_authored_write`); unchanged elsewhere.
    let graph_id = input.graph_id.clone();
    let writer = state.app.clone();
    loopback_app_result(
        crate::source_sync::record_authored_write(
            &state.app,
            &graph_id,
            "rdfLoad",
            crate::source_sync::AuthoredScope::UserGraphs,
            async { load_rdf_service(writer, input) },
        )
        .await,
    )
}

pub(super) async fn loopback_rdf_dump(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    CellGraphJson(input): CellGraphJson<RdfDumpInput>,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "rdf.dump") {
        return response;
    }
    loopback_app_result(dump_rdf_service(state.app.clone(), input))
}
