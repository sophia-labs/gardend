use crate::{
    cell_graph_boundary::CellGraphJson,
    crdt_queue::{enqueue_crdt_operation, EnqueueCrdtOperationInput},
    loopback_http::{authorized_loopback_caller, loopback_error, loopback_result},
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
    let caller = match authorized_loopback_caller(&headers, &state, &required_scopes) {
        Ok(caller) => caller,
        Err(response) => return response,
    };
    // Who this write acts for, as on /mcp: the audit's actor and the author
    // of any comment it writes (2026-10-14). A hosted cell names its caller
    // through the gateway lease instead.
    let actor = (!state.cell_graph.is_enabled()).then(|| crate::request_actor::RequestActor {
        user_id: caller.principal.user_id(),
        via: None,
    });
    loopback_result(
        crate::request_actor::acting_for(actor, enqueue_crdt_operation(state.app.clone(), input))
            .await,
    )
}
