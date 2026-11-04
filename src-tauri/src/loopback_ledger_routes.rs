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
use serde_json::Value;
use tracing::{instrument::WithSubscriber, Instrument};

use crate::crdt_engine::ledger_room::LedgerRoom;
use crate::crdt_engine::rooms::{serve_convergent, RoomAccess, RoomRegistry};
use crate::graph_paths::existing_graph_dir;
use crate::ids::validate_local_id;
use crate::loopback_hocuspocus_routes::room_access;
use crate::loopback_http::{loopback_error, require_loopback_scope};
use crate::loopback_state::LoopbackState;

/// The scope a scoped token needs for the ledger room. The same as the workspace room and the
/// emporium log room: `emporium.write.crdt` does not exist in the scope tables, and a new scope
/// would need a grant in `loopback_token_grants.rs`.
const LEDGER_ROOM_SCOPE: &str = "workspace.write.crdt";

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

/// Open (or fetch) the ledger room for `(graph_id, vocab)` in the shared registry.
async fn ledger_room(
    state: &LoopbackState,
    graph_dir: &FsPath,
    graph_id: &str,
    vocab: &str,
) -> Result<Arc<LedgerRoom>, String> {
    let registry = state.app.state::<RoomRegistry>();
    registry
        .get_or_create_ledger(
            &ledger_room_key(graph_id, vocab),
            ledger_dir_in(graph_dir, vocab),
        )
        .await
}

/// The version map of a room as JSON. Kept in one place so the room's accessor is called once.
async fn room_version(room: &LedgerRoom) -> Value {
    room.version_json().await
}

/// `GET /graphs/{graph_id}/ledger/{vocab}/version` — `{origins: {origin: seq}, durable:
/// {origin: seq}, local: origin}`. Requires `rdf.query`. Opens the ledger (and so creates its
/// directory and the cell's `origin` file) when this vocab has none yet.
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
    let room = match ledger_room(&state, &graph_dir, &graph_id, &vocab).await {
        Ok(room) => room,
        Err(error) => return loopback_error(StatusCode::INTERNAL_SERVER_ERROR, &error),
    };
    let body = room_version(&room).await;
    (StatusCode::OK, Json(body)).into_response()
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
    let graph_dir = match checked_graph(&state, &graph_id, &vocab) {
        Ok(dir) => dir,
        Err(response) => return response,
    };
    let room = match ledger_room(&state, &graph_dir, &graph_id, &vocab).await {
        Ok(room) => room,
        Err(error) => return loopback_error(StatusCode::INTERNAL_SERVER_ERROR, &error),
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
