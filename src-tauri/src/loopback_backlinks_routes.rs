//! `GET /backlinks/{graph_id}?documentId=&iri=&blockId=`: the app's "Linked from" and "Used in" (SPRINT D10).
//!
//! Owned by G4. Scope `wires.read` (no new scope). Not merged into `loopback_router.rs` by U0: G4 adds the
//! `.merge(loopback_backlinks_router())` line there, and the parity snapshots, in the same unit.

use crate::loopback_state::LoopbackState;
use axum::{
    extract::{Path as AxumPath, Query, State},
    http::HeaderMap,
    response::Response,
    Router,
};
use std::sync::Arc;

/// The query string: exactly one of `documentId` and `iri` (G4 refuses neither or both); `blockId` narrows a document.
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BacklinksQuery {
    pub(crate) document_id: Option<String>,
    pub(crate) iri: Option<String>,
    pub(crate) block_id: Option<String>,
}

#[allow(dead_code)]
pub(super) fn loopback_backlinks_router() -> Router<Arc<LoopbackState>> {
    unimplemented!("G4")
}

#[allow(dead_code)]
pub(super) async fn loopback_get_backlinks(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    AxumPath(graph_id): AxumPath<String>,
    Query(query): Query<BacklinksQuery>,
) -> Response {
    let _ = (state, headers, graph_id, query);
    unimplemented!("G4")
}
