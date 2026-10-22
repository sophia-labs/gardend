//! y-websocket sync rooms on the loopback server (platform-next cells).
//!
//! Each route upgrades to a WebSocket and joins the in-process CRDT room
//! served by [`crate::crdt_engine::rooms`] (`serve_room` over the cell's
//! `RoomRegistry`); there is no external sync service:
//!   GET /hocuspocus/docs/{graph_id}/{doc_id}   (upgrade)
//!   GET /hocuspocus/workspace/{graph_id}       (upgrade)
//!
//! Auth: `Authorization: Bearer <token>` header (agents, server-side
//! clients) or `Sec-WebSocket-Protocol: bearer.<token>` subprotocol
//! (browser clients — ALB-safe; the subprotocol form survives the WS
//! upgrade through proxies that strip request headers). The token is the
//! master token, or a scoped client token holding the room's write scope
//! (`documents.write.crdt` / `workspace.write.crdt`). Self-host only: a
//! Cloudflare Access login (`Cf-Access-Jwt-Assertion`) with that scope, from
//! an allowed `Origin` (cross-site WebSocket hijacking rides the cookie).

use crate::{
    app_runtime::AppHandle,
    crdt_engine::{
        persistence_coordinator::{GraphPersistenceCoordinator, SharedLease},
        rooms::{serve_room_with, RoomAccess, RoomRegistry},
    },
    document_paths::document_dir,
    document_record_store::{read_document_record_for_identity, SidecarBackfill},
    graph_paths::{existing_graph_dir, existing_graph_dir_no_heal},
    loopback_client_token_service::resolve_loopback_client_token,
    loopback_http::{bearer_token, loopback_error},
    loopback_state::LoopbackState,
    ydoc_paths::{checked_document_ydoc_state_path, workspace_ydoc_state_path},
};
use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use axum::routing::get;
use axum::Router;
use std::collections::BTreeMap;
use std::{path::Path as FsPath, sync::Arc};
#[cfg(feature = "desktop")]
use tauri::Manager;
use tracing::{instrument::WithSubscriber, Instrument};

pub(super) fn loopback_hocuspocus_router() -> Router<Arc<LoopbackState>> {
    Router::new()
        .route("/hocuspocus/docs/{graph_id}/{doc_id}", get(doc_room))
        .route("/hocuspocus/workspace/{graph_id}", get(workspace_room))
}

/// The scope a scoped client token or an Access login needs for each room.
const DOC_ROOM_SCOPE: &str = "documents.write.crdt";
const WORKSPACE_ROOM_SCOPE: &str = "workspace.write.crdt";

/// Returns the `Sec-WebSocket-Protocol` entry to echo on accept (the matched
/// `bearer.<token>` entry, or under an Access login the first offered entry,
/// since a browser that offered subprotocols fails a handshake that selects
/// none); None when the request offered none or authenticated by header;
/// Err when unauthorized. The master token is checked first and exactly as
/// before; a refused request still gets the same 401.
pub(crate) fn authorize_ws(
    headers: &HeaderMap,
    state: &LoopbackState,
    required_scope: &str,
) -> Result<Option<String>, Response> {
    if bearer_token(headers) == Some(state.token.as_str()) {
        return Ok(None);
    }
    let offered = headers
        .get("sec-websocket-protocol")
        .and_then(|v| v.to_str().ok())
        .map(|protocols| protocols.split(',').map(str::trim).collect::<Vec<_>>())
        .unwrap_or_default();
    for entry in &offered {
        if let Some(token) = entry.strip_prefix("bearer.") {
            if token == state.token {
                return Ok(Some((*entry).to_string()));
            }
        }
    }
    // Scoped client tokens (header first, then subprotocol).
    if let Some(token) = bearer_token(headers) {
        if let Some(scopes) = ws_client_token_scopes(state, token) {
            return require_room_scope(&scopes, required_scope).map(|()| None);
        }
    }
    for entry in &offered {
        if let Some(token) = entry.strip_prefix("bearer.") {
            if let Some(scopes) = ws_client_token_scopes(state, token) {
                return require_room_scope(&scopes, required_scope)
                    .map(|()| Some((*entry).to_string()));
            }
        }
    }
    // Self-host Cloudflare Access login (None unless Access is configured).
    match state.self_host.access_grant(headers) {
        Some(Ok(grant)) => {
            if !state.self_host.ambient_origin_ok(headers) {
                return Err(loopback_error(
                    StatusCode::FORBIDDEN,
                    "invalid origin for a Cloudflare Access login",
                ));
            }
            require_room_scope(&grant.scopes(), required_scope)?;
            return Ok(offered.first().map(|entry| (*entry).to_string()));
        }
        Some(Err(error)) => {
            log::debug!("self-host: refused Cloudflare Access token on a room: {error}")
        }
        None => {}
    }
    Err(loopback_error(
        StatusCode::UNAUTHORIZED,
        "missing or invalid loopback token (Authorization header or bearer.<token> subprotocol)",
    ))
}

/// Largest frame a visitor's connection takes (W6): a fenced writer's, and a
/// read-only reader's (which should send none that matter).
const FENCED_MAX_MESSAGE: usize = 256 * 1024;
const READ_ONLY_MAX_MESSAGE: usize = 64 * 1024;

/// Who may use a room, and how (W6). Every credential `authorize_ws` admits
/// keeps `ReadWrite`, exactly as before. Then, on a public Sophia and a
/// public graph only: a Sophia account (an ID token, as a bearer or a
/// `bearer.<token>` subprotocol) reads, and writes its own documents inside
/// its fence; a caller with no credential at all reads.
pub(crate) async fn room_access(
    headers: &HeaderMap,
    state: &LoopbackState,
    graph_id: &str,
    document_id: Option<&str>,
    required_scope: &str,
) -> Result<(Option<String>, RoomAccess), Response> {
    let refused = match authorize_ws(headers, state, required_scope) {
        Ok(subprotocol) => return Ok((subprotocol, RoomAccess::ReadWrite)),
        Err(response) => response,
    };
    let Some(public) = state.self_host.public() else {
        return Err(refused);
    };
    if !public.is_public_graph(graph_id) {
        return Err(refused);
    }
    let offered = headers
        .get("sec-websocket-protocol")
        .and_then(|v| v.to_str().ok())
        .map(|protocols| {
            protocols
                .split(',')
                .map(str::trim)
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let jwt_entry = offered
        .iter()
        .find(|entry| {
            entry
                .strip_prefix("bearer.")
                .is_some_and(crate::public_mode::looks_like_jwt)
        })
        .cloned();
    let jwt = bearer_token(headers)
        .filter(|token| crate::public_mode::looks_like_jwt(token))
        .map(str::to_string)
        .or_else(|| {
            jwt_entry
                .as_deref()
                .and_then(|entry| entry.strip_prefix("bearer."))
                .map(str::to_string)
        });
    if let (Some(jwt), Some(accounts)) = (jwt, public.accounts()) {
        accounts.ensure_signing_key(&jwt).await;
        let identity = accounts.verify(&jwt).map_err(|error| {
            log::debug!("public mode: refused an account token on a room: {error}");
            loopback_error(
                StatusCode::UNAUTHORIZED,
                "invalid or expired Sophia account token (sign in again)",
            )
        })?;
        let account = crate::visitor_fence::VisitorAccount {
            sub: identity.sub,
            email_verified: identity.email_verified,
        };
        let subprotocol = jwt_entry.or_else(|| offered.first().cloned());
        let account_id = public
            .visitor_folders()
            .map(|folders| folders.account_id(&account.sub));
        if let Some(document_id) = document_id {
            if let Ok(fence) =
                crate::visitor_fence::fence_for(Some(public), Some(&account), graph_id)
            {
                return Ok((
                    subprotocol,
                    RoomAccess::Fenced {
                        fence,
                        document_id: document_id.to_string(),
                    },
                ));
            }
        }
        return Ok((
            subprotocol,
            RoomAccess::ReadOnly {
                account: account_id,
            },
        ));
    }
    // No credential of its own at all: an anonymous reader. A wrong key stays 401.
    let carries_credential = bearer_token(headers).is_some()
        || offered.iter().any(|entry| entry.starts_with("bearer."))
        || headers.contains_key(crate::loopback_self_host::ACCESS_JWT_HEADER);
    if carries_credential {
        return Err(refused);
    }
    Ok((
        offered.first().cloned(),
        RoomAccess::ReadOnly { account: None },
    ))
}

fn with_access_limits(ws: WebSocketUpgrade, access: &RoomAccess) -> WebSocketUpgrade {
    match access {
        RoomAccess::ReadWrite => ws,
        RoomAccess::ReadOnly { .. } => ws.max_message_size(READ_ONLY_MAX_MESSAGE),
        RoomAccess::Fenced { .. } => ws.max_message_size(FENCED_MAX_MESSAGE),
    }
}

fn ws_client_token_scopes(state: &LoopbackState, token: &str) -> Option<Vec<String>> {
    // A supervised local service's own MCP token (`local_service_host`) holds
    // the member grant, rooms included.
    if state.services.service_token_owner(token).is_some() {
        return Some(crate::loopback_token_grants::grant_profile_scopes(
            crate::loopback_token_grants::MEMBER_GRANT_PROFILE,
        ));
    }
    match resolve_loopback_client_token(&state.app, token) {
        Ok(scopes) => scopes,
        Err(error) => {
            log::warn!("loopback client token store unreadable on a room upgrade: {error}");
            None
        }
    }
}

fn require_room_scope(scopes: &[String], required_scope: &str) -> Result<(), Response> {
    if scopes.iter().any(|scope| scope == required_scope) {
        Ok(())
    } else {
        Err(loopback_error(
            StatusCode::FORBIDDEN,
            &format!("loopback token missing required scope {required_scope}"),
        ))
    }
}

/// A document room may be created only for an authoritative document identity:
/// either membership in the live/hydrated workspace Y.Doc or a readable cold
/// manifest whose embedded id matches the route id. This prevents a stale
/// browser reconnect after queued deletion from recreating an empty sidecar.
pub(crate) async fn document_exists_for_room_connection(
    registry: &RoomRegistry,
    graph_id: &str,
    graph_dir: &FsPath,
    document_id: &str,
) -> Result<bool, String> {
    crate::ids::validate_local_id(document_id, "document_id")?;
    crate::document_body_availability::require_available(graph_dir, document_id)?;
    if crate::document_tombstone_store::document_is_tombstoned(graph_dir, document_id)? {
        return Ok(false);
    }
    let workspace_key = format!("workspace:{graph_id}");
    let workspace_room = match registry.peek(&workspace_key).await {
        Some(room) => Some(room),
        None => {
            let state_path = workspace_ydoc_state_path(graph_dir);
            if state_path.is_file() {
                Some(registry.get_or_create(&workspace_key, state_path).await?)
            } else {
                None
            }
        }
    };
    let has_workspace_ydoc_authority = workspace_room.is_some();
    if let Some(workspace_room) = workspace_room {
        let document_id = document_id.to_string();
        if workspace_room
            .with_doc(move |doc| {
                crate::crdt_engine::workspace_ops::document_exists_in_workspace(doc, &document_id)
            })
            .await
        {
            return Ok(true);
        }
    }

    // Legacy profiles can have a cold workspace snapshot without a Y.Doc
    // sidecar. Consult it only when no live/hydrated Y.Doc authority exists;
    // otherwise a stale snapshot must not resurrect an entry removed in Y.Doc.
    if !has_workspace_ydoc_authority {
        let snapshot_path = crate::ydoc_paths::workspace_snapshot_path(graph_dir);
        if snapshot_path.is_file() {
            let snapshot: serde_json::Value = crate::storage::read_json(&snapshot_path)
                .map_err(crate::app_error::AppError::message)?;
            if snapshot
                .get("documents")
                .and_then(serde_json::Value::as_array)
                .is_some_and(|documents| {
                    documents.iter().any(|document| {
                        document
                            .get("id")
                            .or_else(|| document.get("documentId"))
                            .and_then(serde_json::Value::as_str)
                            == Some(document_id)
                    })
                })
            {
                return Ok(true);
            }
        }
    }

    let manifest = document_dir(graph_dir, document_id)?.join("document.json");
    if !manifest.is_file() {
        return Ok(false);
    }
    // ReadOnly: this runs under the room root's SharedLease, which must never
    // write — the legacy inline-payload sidecar backfill would otherwise race
    // a concurrent HotWriteLease writer (image-one panel, data-loss fatal).
    Ok(read_document_record_for_identity(
        graph_dir,
        &manifest,
        graph_id,
        document_id,
        SidecarBackfill::ReadOnly,
    )?
    .is_some())
}

async fn existing_graph_dir_for_room(
    app: &AppHandle,
    coordinator: &GraphPersistenceCoordinator,
    graph_id: &str,
    shared: SharedLease,
) -> Result<(std::path::PathBuf, SharedLease), String> {
    if crate::graph_paths::graph_json_present(app, graph_id)? {
        return existing_graph_dir_no_heal(app, graph_id).map(|graph_dir| (graph_dir, shared));
    }
    if !crate::runtime_config::self_heal_graphs_enabled() {
        return Err(format!("graph not found: {graph_id}"));
    }

    // Lock ordering is lifecycle before hot-write, and lifecycle escalation
    // is always drop, re-acquire, re-validate — never an in-place upgrade.
    //
    // Snapshot the lifecycle generation BEFORE releasing shared: deletion
    // advances it while holding its exclusive root lease, so a delete that
    // lands inside the escalation window (shared dropped, exclusive not yet
    // held) is detected below and the open aborts instead of self-heal
    // resurrecting a just-deleted graph. The generation-fence semantics are
    // pinned by `generation_allows_restart_reset_but_rejects_process_local_advance`.
    let expected_generation = coordinator.generation(graph_id)?;
    drop(shared);
    let exclusive = coordinator.acquire_lifecycle_exclusive(graph_id).await?;
    coordinator.require_generation(graph_id, expected_generation)?;
    let heal_result = existing_graph_dir(app, graph_id);
    drop(exclusive);
    heal_result?;
    let shared = coordinator.acquire_lifecycle_shared(graph_id).await?;
    coordinator.require_generation(graph_id, expected_generation)?;
    let graph_dir = existing_graph_dir_no_heal(app, graph_id)?;
    Ok((graph_dir, shared))
}

async fn doc_room(
    State(state): State<Arc<LoopbackState>>,
    Path((graph_id, doc_id)): Path<(String, String)>,
    Query(query): Query<BTreeMap<String, String>>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    let (subprotocol, access) =
        match room_access(&headers, &state, &graph_id, Some(&doc_id), DOC_ROOM_SCOPE).await {
            Ok(value) => value,
            Err(response) => return response,
        };
    // A public reader (or visitor) does not open a document in the Trash: it is
    // the owner's to see (W6, sealed exam C1).
    if !matches!(access, RoomAccess::ReadWrite) {
        if let Ok(trashed) = crate::visitor_fence::trashed_documents(&state.app, &graph_id).await {
            if trashed.contains(doc_id.as_str()) {
                return loopback_error(StatusCode::NOT_FOUND, "document not found");
            }
        }
    }
    // Room construction participates in graph lifecycle. Deletion evicts the
    // registry under this same lease, so lookup + insertion + scheduler setup
    // cannot land a fresh uncancelled room immediately after deletion.
    let coordinator = state.app.state::<GraphPersistenceCoordinator>();
    let graph_lease = match coordinator.acquire_lifecycle_shared(&graph_id).await {
        Ok(lease) => lease,
        Err(error) => return loopback_error(StatusCode::INTERNAL_SERVER_ERROR, &error),
    };
    let (graph_dir, graph_lease) =
        match existing_graph_dir_for_room(&state.app, &coordinator, &graph_id, graph_lease).await {
            Ok(result) => result,
            Err(error) => return loopback_error(StatusCode::NOT_FOUND, &error),
        };
    let state_path = match checked_document_ydoc_state_path(&graph_dir, &doc_id) {
        Ok(path) => path,
        Err(error) => return loopback_error(StatusCode::BAD_REQUEST, &error),
    };
    let registry = state.app.state::<RoomRegistry>();
    match document_exists_for_room_connection(&registry, &graph_id, &graph_dir, &doc_id).await {
        Ok(true) => {}
        Ok(false) => return loopback_error(StatusCode::NOT_FOUND, "document not found"),
        Err(error) => return loopback_error(StatusCode::INTERNAL_SERVER_ERROR, &error),
    }
    let incarnation = match crate::document_incarnation_store::ensure_document_incarnation_id(
        &graph_dir, &doc_id,
    ) {
        Ok(value) => value,
        Err(error) => return loopback_error(StatusCode::INTERNAL_SERVER_ERROR, &error),
    };
    if let Some(expected) = query.get(crate::document_incarnation_store::DOCUMENT_INCARNATION_QUERY)
    {
        if expected != &incarnation {
            return loopback_error(StatusCode::CONFLICT, "document incarnation changed");
        }
    }
    let key = format!("doc:{graph_id}:{doc_id}");
    let room = match registry.get_or_create(&key, state_path).await {
        Ok(room) => room,
        Err(error) => return loopback_error(StatusCode::INTERNAL_SERVER_ERROR, &error),
    };
    if let Err(error) = room.configure_projection_flush(graph_id.clone(), Some(doc_id.clone())) {
        return loopback_error(StatusCode::INTERNAL_SERVER_ERROR, &error);
    }
    room.schedule_projection_flush(state.app.clone());
    drop(graph_lease);
    let ws = match subprotocol {
        Some(protocol) => ws.protocols([protocol]),
        None => ws,
    };
    let ws = with_access_limits(ws, &access);
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
            serve_room_with(socket, room, app, graph_id, access).await;
        }
        .instrument(parent)
        .with_subscriber(dispatch)
    })
}

async fn workspace_room(
    State(state): State<Arc<LoopbackState>>,
    Path(graph_id): Path<String>,
    Query(query): Query<BTreeMap<String, String>>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    let (subprotocol, access) =
        match room_access(&headers, &state, &graph_id, None, WORKSPACE_ROOM_SCOPE).await {
            Ok(value) => value,
            Err(response) => return response,
        };
    let coordinator = state.app.state::<GraphPersistenceCoordinator>();
    let graph_lease = match coordinator.acquire_lifecycle_shared(&graph_id).await {
        Ok(lease) => lease,
        Err(error) => return loopback_error(StatusCode::INTERNAL_SERVER_ERROR, &error),
    };
    let (graph_dir, graph_lease) =
        match existing_graph_dir_for_room(&state.app, &coordinator, &graph_id, graph_lease).await {
            Ok(result) => result,
            Err(error) => return loopback_error(StatusCode::NOT_FOUND, &error),
        };
    let incarnation =
        match crate::graph_record_store::ensure_graph_incarnation(&state.app, &graph_id) {
            Ok(value) => value,
            Err(error) => {
                return loopback_error(StatusCode::INTERNAL_SERVER_ERROR, error.message_ref())
            }
        };
    match query.get(crate::graph_record_store::GRAPH_INCARNATION_QUERY) {
        Some(expected) if expected == &incarnation => {}
        Some(_) => return loopback_error(StatusCode::CONFLICT, "graph incarnation changed"),
        None => {
            return loopback_error(
                StatusCode::PRECONDITION_REQUIRED,
                "graph_incarnation query parameter is required",
            )
        }
    }
    let state_path = workspace_ydoc_state_path(&graph_dir);
    let registry = state.app.state::<RoomRegistry>();
    let key = format!("workspace:{graph_id}");
    let room = match registry.get_or_create(&key, state_path).await {
        Ok(room) => room,
        Err(error) => return loopback_error(StatusCode::INTERNAL_SERVER_ERROR, &error),
    };
    if let Err(error) = room.configure_projection_flush(graph_id.clone(), None) {
        return loopback_error(StatusCode::INTERNAL_SERVER_ERROR, &error);
    }
    room.schedule_projection_flush(state.app.clone());
    drop(graph_lease);
    let ws = match subprotocol {
        Some(protocol) => ws.protocols([protocol]),
        None => ws,
    };
    let ws = with_access_limits(ws, &access);
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
            serve_room_with(socket, room, app, graph_id, access).await;
        }
        .instrument(parent)
        .with_subscriber(dispatch)
    })
}
