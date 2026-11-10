//! HTTP surface for the ledger (W6, lane W):
//!
//! - `GET /hocuspocus/ledger/{graph_id}/{vocab}` the ledger room (WebSocket, y-sync tags with the
//!   `ledger-vv/1` and `ledger-bundle/1` payloads of ARCHITECTURE.md section 3).
//! - `GET /graphs/{graph_id}/ledger/{vocab}/version` the ledger's version map as JSON
//!   (`{origins, durable, local}`), scope `rdf.query`.
//!
//! The handlers are thin shims: the room is a [`LedgerRoom`] hosted by the shared
//! [`RoomRegistry`] under the key `ledger:{graph_id}:{vocab}`, its directory is
//! `{graph_dir}/ledger/{vocab}`, and the socket loop is the generic `serve_convergent`.

use std::path::{Path as FsPath, PathBuf};
use std::sync::Arc;

use crate::app_runtime::Manager;
use axum::{
    extract::{ws::WebSocketUpgrade, Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use tracing::{instrument::WithSubscriber, Instrument};

use crate::crdt_engine::ledger_room::LedgerRoom;
use crate::crdt_engine::rooms::{serve_convergent, LedgerAdmissionError, RoomAccess, RoomRegistry};
use crate::graph_paths::existing_graph_dir;
use crate::ids::validate_local_id;
use crate::loopback_hocuspocus_routes::room_access;
use crate::loopback_http::{loopback_error, require_loopback_scope};
use crate::loopback_state::LoopbackState;

/// The scope a scoped token needs for the ledger room: the commit route's. A peer in this room
/// appends frames of its own origin and the fold writes them to the graph's RDF, so admitting it
/// is admitting an RDF write that skips the commit route's gates (structure, contract, file
/// references, a service's declaration). A member's `workspace.write.crdt` is not that: before
/// 2026-11-09 it was, and a member token could write any vocabulary's RDF here. A supervised
/// service's own token never holds `rdf.update` (`loopback_service_rdf`), so it is refused.
const LEDGER_ROOM_SCOPE: &str = "rdf.update";

/// The scope the version route needs (the same as the log stats route).
const LEDGER_VERSION_SCOPE: &str = "rdf.query";

/// The two ledger routes, merged into the loopback router.
pub(crate) fn loopback_ledger_router() -> Router<Arc<LoopbackState>> {
    Router::new()
        .route("/hocuspocus/ledger/{graph_id}/{vocab}", get(ledger_room_ws))
        .route(
            "/graphs/{graph_id}/ledger/{vocab}/version",
            get(loopback_ledger_version),
        )
}

/// The registry key of one ledger room.
pub(crate) fn ledger_room_key(graph_id: &str, vocab: &str) -> String {
    format!("ledger:{graph_id}:{vocab}")
}

/// The directory of one ledger under its graph directory.
pub(crate) fn ledger_dir_in(graph_dir: &FsPath, vocab: &str) -> PathBuf {
    graph_dir.join("ledger").join(vocab)
}

/// Validate both ids (400), then require the graph to exist (404). Returns the graph directory.
fn checked_graph(state: &LoopbackState, graph_id: &str, vocab: &str) -> Result<PathBuf, Response> {
    if let Err(message) = validate_local_id(graph_id, "graph_id") {
        return Err(loopback_error(StatusCode::BAD_REQUEST, &message));
    }
    if let Err(message) = validate_local_id(vocab, "vocab") {
        return Err(loopback_error(StatusCode::BAD_REQUEST, &message));
    }
    existing_graph_dir(&state.app, graph_id)
        .map_err(|message| loopback_error(StatusCode::NOT_FOUND, &message))
}

/// Open (or fetch) the ledger room for `(graph_id, vocab)` in the shared registry, under the
/// graph's shared lifecycle lease and only for a graph that is not deleted
/// ([`RoomRegistry::open_ledger`]).
async fn ledger_room(
    state: &LoopbackState,
    graph_id: &str,
    vocab: &str,
) -> Result<Arc<LedgerRoom>, Response> {
    let registry = state.app.state::<RoomRegistry>();
    registry
        .open_ledger(&state.app, graph_id, vocab)
        .await
        .map_err(|error| match error {
            LedgerAdmissionError::NotFound(message) => {
                loopback_error(StatusCode::NOT_FOUND, &message)
            }
            LedgerAdmissionError::Other(message) => {
                loopback_error(StatusCode::INTERNAL_SERVER_ERROR, &message)
            }
        })
}

/// `GET /graphs/{graph_id}/ledger/{vocab}/version` — `{origins: {origin: seq}, durable:
/// {origin: seq}, local: origin}`. Requires `rdf.query`. A read: it never creates a ledger. A
/// hosted room answers from memory; otherwise the ledger on disk is read without minting an
/// `origin` file or a stream, and a vocab with no ledger yet answers
/// `{origins: {}, durable: {}, local: null}` (200).
pub(crate) async fn loopback_ledger_version(
    Path((graph_id, vocab)): Path<(String, String)>,
    headers: HeaderMap,
    State(state): State<Arc<LoopbackState>>,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, LEDGER_VERSION_SCOPE) {
        return response;
    }
    let graph_dir = match checked_graph(&state, &graph_id, &vocab) {
        Ok(dir) => dir,
        Err(response) => return response,
    };
    let registry = state.app.state::<RoomRegistry>();
    match registry
        .ledger_version(
            &ledger_room_key(&graph_id, &vocab),
            &ledger_dir_in(&graph_dir, &vocab),
        )
        .await
    {
        Ok(body) => (StatusCode::OK, Json(body)).into_response(),
        Err(error) => loopback_error(StatusCode::INTERNAL_SERVER_ERROR, &error),
    }
}

/// `GET /hocuspocus/ledger/{graph_id}/{vocab}` — the ledger room. ReadWrite only: a Viewer /
/// ReadOnly / Fenced admission is refused, because the ledger takes server-side writes and a
/// peer's frames land in it. The graph id and vocab are validated and the graph must exist.
pub(crate) async fn ledger_room_ws(
    State(state): State<Arc<LoopbackState>>,
    Path((graph_id, vocab)): Path<(String, String)>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    let (subprotocol, access) =
        match room_access(&headers, &state, &graph_id, None, LEDGER_ROOM_SCOPE).await {
            Ok(value) => value,
            Err(response) => return response,
        };
    if !matches!(access, RoomAccess::ReadWrite) {
        return loopback_error(
            StatusCode::FORBIDDEN,
            "the ledger room admits read-write credentials only",
        );
    }
    if let Err(response) = checked_graph(&state, &graph_id, &vocab) {
        return response;
    }
    let room = match ledger_room(&state, &graph_id, &vocab).await {
        Ok(room) => room,
        Err(response) => return response,
    };
    let ws = match subprotocol {
        Some(protocol) => ws.protocols([protocol]),
        None => ws,
    };
    let socket_lease = match state.lifecycle.open_websocket() {
        Ok(lease) => lease,
        Err(_) => {
            return loopback_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "graph cell is draining; retry the WebSocket through the gateway",
            );
        }
    };
    let app = state.app.clone();
    let parent = tracing::Span::current();
    let dispatch = tracing::dispatcher::get_default(Clone::clone);
    ws.on_upgrade(move |socket| {
        async move {
            let _socket_lease = socket_lease;
            serve_convergent(socket, room, app, graph_id, access).await;
        }
        .instrument(parent)
        .with_subscriber(dispatch)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn room_key_and_dir_follow_the_spec() {
        assert_eq!(ledger_room_key("g1", "workspace"), "ledger:g1:workspace");
        assert_eq!(
            ledger_dir_in(FsPath::new("/data/g1"), "workspace"),
            PathBuf::from("/data/g1/ledger/workspace")
        );
    }

    #[test]
    fn ids_that_could_escape_the_directory_are_refused() {
        assert!(validate_local_id("..", "vocab").is_err());
        assert!(validate_local_id("a/b", "vocab").is_err());
        assert!(validate_local_id("workspace", "vocab").is_ok());
    }
}
