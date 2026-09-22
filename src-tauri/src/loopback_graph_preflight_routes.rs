use crate::{
    graph_service::local_graph_storage_usage,
    loopback_graph_inputs::DocumentPreflightQuery,
    loopback_http::{loopback_error, require_loopback_scope},
    loopback_state::LoopbackState,
};
use axum::{
    extract::{Path as AxumPath, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use std::sync::Arc;

pub(super) async fn loopback_hosted_document_preflight(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    AxumPath(graph_id): AxumPath<String>,
    Query(query): Query<DocumentPreflightQuery>,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "documents.write.crdt") {
        return response;
    }

    let mut usage = match local_graph_storage_usage(&state.app, &graph_id) {
        Ok(value) => value,
        Err(error) if error.contains("not found") => {
            return loopback_error(StatusCode::NOT_FOUND, &error);
        }
        Err(error) => return loopback_error(StatusCode::BAD_REQUEST, &error),
    };

    match query.simulate.as_deref() {
        Some("blocked") => {
            if let Some(object) = usage.as_object_mut() {
                object.insert("simulated".to_string(), serde_json::json!(true));
                object.insert("limit_reached".to_string(), serde_json::json!(true));
            }
            (
                StatusCode::FORBIDDEN,
                Json(serde_json::json!({
                    "detail": {
                        "error_code": "GRAPH_STORAGE_LIMIT_REACHED",
                        "message": "Your graph has reached its storage limit. Current tier limit: local profile. Remove documents to create new ones.",
                        "tier": "local",
                        "graph_storage": usage,
                    },
                })),
            )
                .into_response()
        }
        Some("warning") => {
            if let Some(object) = usage.as_object_mut() {
                object.insert("simulated".to_string(), serde_json::json!(true));
                object.insert(
                    "warning_threshold_reached".to_string(),
                    serde_json::json!(true),
                );
            }
            Json(serde_json::json!({
                "allowed": true,
                "graph_storage": usage,
            }))
            .into_response()
        }
        _ => Json(serde_json::json!({
            "allowed": true,
            "graph_storage": usage,
        }))
        .into_response(),
    }
}
