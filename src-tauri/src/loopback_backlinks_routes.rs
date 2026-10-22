//! `GET /backlinks/{graph_id}?documentId=&iri=&blockId=`: the app's "Linked from" and "Used in" (SPRINT D10).
//!
//! Owned by G4. Scope `wires.read` (no new scope). Not merged into `loopback_router.rs` by U0: G4 adds the
//! `.merge(loopback_backlinks_router())` line there, and the parity snapshots, in the same unit.

use crate::backlinks_service::mcp_local_get_backlinks;
use crate::loopback_http::{loopback_result, require_loopback_scope};
use crate::loopback_state::LoopbackState;
use axum::{
    extract::{Path as AxumPath, Query, State},
    http::HeaderMap,
    response::Response,
    routing::get,
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

pub(super) fn loopback_backlinks_router() -> Router<Arc<LoopbackState>> {
    Router::new().route("/backlinks/{graph_id}", get(loopback_get_backlinks))
}

/// Answers exactly what the MCP tool answers: `{graphId, target, backlinks, count}`. A call that gives neither or
/// both of `documentId` and `iri` is a 400.
pub(super) async fn loopback_get_backlinks(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    AxumPath(graph_id): AxumPath<String>,
    Query(query): Query<BacklinksQuery>,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "wires.read") {
        return response;
    }
    loopback_result(mcp_local_get_backlinks(
        state.app.clone(),
        &query.arguments(&graph_id),
    ))
}

impl BacklinksQuery {
    /// The MCP tool's arguments for this query string.
    pub(crate) fn arguments(&self, graph_id: &str) -> serde_json::Value {
        serde_json::json!({
            "graphId": graph_id,
            "documentId": self.document_id,
            "iri": self.iri,
            "blockId": self.block_id,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The route is built (axum refuses a malformed path pattern when the router is made) and the query string
    /// turns into the tool's arguments, empty values read as absent.
    #[test]
    fn the_route_builds_and_the_query_becomes_the_tools_arguments() {
        let _router = loopback_backlinks_router();
        let query = BacklinksQuery {
            document_id: Some("doc-a".into()),
            iri: None,
            block_id: Some("b1".into()),
        };
        let arguments = query.arguments("g");
        assert_eq!(arguments["graphId"], "g");
        assert_eq!(arguments["documentId"], "doc-a");
        assert_eq!(arguments["blockId"], "b1");
        assert!(arguments["iri"].is_null());
        assert_eq!(
            crate::mcp_utils::mcp_arg_string(&arguments, &["iri"]),
            None,
            "a null reads as absent"
        );
    }
}
