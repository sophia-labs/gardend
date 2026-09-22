use crate::{
    cell_graph_boundary::CellGraphJson,
    crdt_queue::{enqueue_crdt_operation, EnqueueCrdtOperationInput},
    loopback_http::{loopback_error, loopback_result, require_loopback_scopes},
    loopback_scopes::crdt_operation_scopes,
    loopback_state::LoopbackState,
};
use axum::{
    extract::State, http::HeaderMap, http::StatusCode, response::Response, routing::post, Router,
};
use std::sync::Arc;

pub(super) fn loopback_crdt_router() -> Router<Arc<LoopbackState>> {
    Router::new().route("/api/crdt/operations", post(loopback_crdt_operation))
}

pub(super) async fn loopback_crdt_operation(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    CellGraphJson(mut input): CellGraphJson<EnqueueCrdtOperationInput>,
) -> Response {
    input.kind = input.kind.trim().to_string();
    let Some(required_scopes) = crdt_operation_scopes(&input.kind) else {
        return loopback_error(
            StatusCode::BAD_REQUEST,
            &format!("unsupported local CRDT operation kind {}", input.kind),
        );
    };
    if let Err(response) = require_loopback_scopes(&headers, &state, &required_scopes) {
        return response;
    }
    loopback_result(enqueue_crdt_operation(state.app.clone(), input).await)
}
