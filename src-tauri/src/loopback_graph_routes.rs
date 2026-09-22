use crate::{
    graph_projection_service::{hosted_graph_entries_service, hosted_graph_entry_from_record},
    graph_service::{create_graph_service_async, create_graph_service_async_with_incarnation},
    loopback_graph_export_routes::loopback_hosted_export_graph,
    loopback_graph_inputs::FencedCreateGraphInput,
    loopback_graph_job_routes::{
        loopback_graph_job_cancel, loopback_graph_job_result, loopback_graph_job_status,
        loopback_graph_query_job, loopback_graph_update_job,
    },
    loopback_graph_preflight_routes::loopback_hosted_document_preflight,
    loopback_graph_workspace_routes::{
        loopback_hosted_workspace_properties, loopback_hosted_workspace_summary,
        loopback_hosted_workspace_viz,
    },
    loopback_hosted_graph_lifecycle_routes::{
        loopback_hosted_create_graph, loopback_hosted_delete_graph,
        loopback_hosted_duplicate_graph, loopback_hosted_update_graph,
    },
    loopback_hosted_graph_read_routes::{
        loopback_hosted_graph_stats, loopback_hosted_list_graphs, loopback_hosted_read_graph,
    },
    loopback_http::{loopback_app_result, require_loopback_scope},
    loopback_import_routes::{
        loopback_hosted_import_graph, loopback_hosted_import_notion,
        loopback_hosted_import_obsidian, loopback_hosted_import_roam,
        loopback_hosted_restore_cell_archive,
    },
    loopback_rdf_import_routes::loopback_hosted_import_rdf,
    loopback_state::LoopbackState,
    loopback_web_import_routes::{loopback_hosted_import_clip, loopback_hosted_import_youtube},
};
use axum::{
    extract::State,
    http::HeaderMap,
    response::Response,
    routing::{get, post},
    Json, Router,
};
use std::sync::Arc;

pub(super) fn loopback_graph_router() -> Router<Arc<LoopbackState>> {
    Router::new()
        .merge(crate::loopback_custom_css_routes::router())
        .route(
            "/api/graphs",
            get(loopback_list_graphs).post(loopback_create_graph),
        )
        .route("/api/graphs/catalog", get(loopback_list_graph_catalog))
        .route("/graphs/catalog", get(loopback_list_graph_catalog))
        .route(
            "/graphs",
            get(loopback_hosted_list_graphs).post(loopback_hosted_create_graph),
        )
        .route("/graphs/import", post(loopback_hosted_import_graph))
        .route("/graphs/query", post(loopback_graph_query_job))
        .route("/api/graphs/query", post(loopback_graph_query_job))
        .route("/graphs/update", post(loopback_graph_update_job))
        .route("/api/graphs/update", post(loopback_graph_update_job))
        .route("/graphs/stats", get(loopback_hosted_graph_stats))
        .route(
            "/graphs/jobs/{job_id}",
            get(loopback_graph_job_status).delete(loopback_graph_job_cancel),
        )
        .route(
            "/api/graphs/jobs/{job_id}",
            get(loopback_graph_job_status).delete(loopback_graph_job_cancel),
        )
        .route(
            "/graphs/jobs/{job_id}/result",
            get(loopback_graph_job_result),
        )
        .route(
            "/api/graphs/jobs/{job_id}/result",
            get(loopback_graph_job_result),
        )
        .route(
            "/graphs/{graph_id}",
            get(loopback_hosted_read_graph)
                .put(loopback_hosted_update_graph)
                .delete(loopback_hosted_delete_graph),
        )
        .route(
            "/graphs/{graph_id}/duplicate",
            post(loopback_hosted_duplicate_graph),
        )
        .route(
            "/graphs/{graph_id}/documents/preflight",
            post(loopback_hosted_document_preflight),
        )
        .route(
            "/graphs/{graph_id}/imports/rdf",
            post(loopback_hosted_import_rdf),
        )
        .route(
            "/graphs/{graph_id}/imports/obsidian",
            post(loopback_hosted_import_obsidian),
        )
        .route(
            "/graphs/{graph_id}/imports/notion",
            post(loopback_hosted_import_notion),
        )
        .route(
            "/graphs/{graph_id}/imports/roam",
            post(loopback_hosted_import_roam),
        )
        .route(
            "/graphs/{graph_id}/imports/clip",
            post(loopback_hosted_import_clip),
        )
        .route(
            "/graphs/{graph_id}/imports/youtube",
            post(loopback_hosted_import_youtube),
        )
        .route(
            "/graphs/{graph_id}/restore-archive",
            post(loopback_hosted_restore_cell_archive),
        )
        .route(
            "/graphs/{graph_id}/summary",
            get(loopback_hosted_workspace_summary),
        )
        .route("/graphs/{graph_id}/viz", get(loopback_hosted_workspace_viz))
        .route(
            "/graphs/{graph_id}/properties",
            get(loopback_hosted_workspace_properties),
        )
        .route(
            "/graphs/{graph_id}/export",
            post(loopback_hosted_export_graph),
        )
}

pub(super) async fn loopback_list_graphs(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "graphs.read") {
        return response;
    }
    loopback_app_result(hosted_graph_entries_service(&state.app, false))
}

pub(super) async fn loopback_list_graph_catalog(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "graphs.read") {
        return response;
    }
    loopback_app_result(hosted_graph_entries_service(&state.app, true))
}

pub(super) async fn loopback_create_graph(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    Json(input): Json<FencedCreateGraphInput>,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "graphs.write") {
        return response;
    }
    let (input, graph_incarnation) = input.into_parts();
    let result = match graph_incarnation {
        Some(graph_incarnation) => {
            create_graph_service_async_with_incarnation(&state.app, input, graph_incarnation).await
        }
        None => create_graph_service_async(&state.app, input).await,
    };
    loopback_app_result(result.map(|graph| hosted_graph_entry_from_record(graph, false)))
}
