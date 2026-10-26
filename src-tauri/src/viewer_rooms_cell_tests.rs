//! Hosted Viewer rooms (sharing, 2026-10-26): through the real router over
//! real HTTP, with gateway leases signed as the gateway signs them, the real
//! document and workspace rooms driven by a y-websocket client. A Viewer
//! lease reads both rooms, keeps a presence stamped with its own principal,
//! and changes nothing, not even on open; an Editor's and the Owner's same
//! frames land (so every refusal is a real one). No mocks.
use super::*;
use crate::crdt_engine::rooms::{RoomRegistry, VISITOR_REFUSED_CLOSE};
use axum::{body::Body, http::Request};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use futures_util::{SinkExt, StreamExt};
use hmac::{Hmac, Mac};
use serde_json::{json, Value};
use std::{net::SocketAddr, time::Duration};
use tokio_tungstenite::tungstenite::{client::IntoClientRequest, Message as Ws};
use tower::ServiceExt;
use yrs::{
    sync::{Message as YMessage, SyncMessage},
    updates::{decoder::Decode, encoder::Encode},
    Doc, GetString, Map, Out, ReadTxn, StateVector, Transact, Update, XmlElementPrelim,
    XmlFragment, XmlTextPrelim,
};

const GRAPH: &str = "viewer-rooms-cell-fixture-20261026";
const OWNER: &str = "user:viewer-rooms-owner-20261026";
const EDITOR: &str = "user:viewer-rooms-editor-20261026";
const VIEWER: &str = "user:viewer-rooms-viewer-20261026";
const SECRET: &[u8] = b"viewer-rooms-cell-test-secret-at-least-32-bytes";
const TOKEN: &str = "viewer-rooms-cell-disposable-token";
const ANSWER_WITHIN: Duration = Duration::from_secs(10);
const FOLDER: &str = "shelf";
const DOC: &str = "the-guide";

/// A gateway lease for `sub` as `role` ("viewer", "editor" or "owner") on the
/// owner's graph.
fn lease(sub: &str, role: &str) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let claims = json!({"iss":"pn-gateway","aud":"gardend-cell","sub":sub,"owner":OWNER,
        "graphId":GRAPH,"generation":1,"cellId":bound_cell_id(OWNER,GRAPH,1),"role":role,
        "policyRevision":1,"registryRevision":1,"sessionId":"viewer-rooms-test","iat":now,"exp":now+600});
    let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"HS256","typ":"JWT"}"#);
    let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap());
    let signed = format!("{header}.{payload}");
    let mut mac = Hmac::<Sha256>::new_from_slice(SECRET).unwrap();
    mac.update(signed.as_bytes());
    format!(
        "{signed}.{}",
        URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes())
    )
}

fn state_for(profile: &std::path::Path) -> Arc<LoopbackState> {
    let app = crate::tauri_runtime::build_mock_cell_app_for_tests(GRAPH, OWNER, 1);
    let boundary = Arc::new(
        CellGraphBoundary::new_with_binding(
            Some(GRAPH.into()),
            Some(OWNER.into()),
            Some(1),
            Some(1),
            Some(SECRET.to_vec()),
        )
        .unwrap(),
    );
    let jobs = Arc::new(
        crate::local_jobs::LocalJobRegistry::new_bound(profile.join("jobs"), &boundary).unwrap(),
    );
    let grant = crate::loopback_token_grants::session_all_token_grant();
    let manifest = crate::loopback_state::LoopbackManifest {
        runtime_profile: "test",
        bind_host: "127.0.0.1",
        port: 0,
        api_url: "test".into(),
        mcp_url: "test".into(),
        openapi_url: "test".into(),
        token: TOKEN.into(),
        pid: 0,
        started_at: "test".into(),
        manifest_path: "test".into(),
        auth_header: "test",
        token_audience: "test",
        token_storage: "test",
        security_warning: "test",
        cell_graph_id: Some(GRAPH.into()),
        cell_owner: Some(OWNER.into()),
        cell_generation: Some(1),
        cell_registry_revision: Some(1),
        capabilities: grant.scopes.clone(),
        token_scope_mode: grant.scope_mode,
        token_scopes: grant.scopes,
        scope_details: grant.scope_details,
        grant_profiles: grant.grant_profiles,
    };
    Arc::new(LoopbackState {
        app,
        token: TOKEN.into(),
        manifest,
        jobs,
        services: Arc::new(crate::local_service_host::LocalServiceHost::default()),
        lifecycle: Arc::new(crate::cell_lifecycle::CellLifecycle::new()),
        cell_graph: boundary,
        self_host: Default::default(),
    })
}

/// Run `test` in a fresh profile (the profile directory is process-global).
fn with_cell(name: &str, test: impl FnOnce(&std::path::Path)) {
    let _serial = crate::tauri_runtime::profile_env_serial()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let root =
        std::env::temp_dir().join(format!("garden-viewer-rooms-{name}-{}", uuid::Uuid::new_v4()));
    let profile = root.join("profile");
    std::fs::create_dir_all(&profile).unwrap();
    let previous = std::env::var_os("GARDEN_PROFILE_DIR");
    std::env::set_var("GARDEN_PROFILE_DIR", &profile);
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| test(&profile)));
    match previous {
        Some(previous) => std::env::set_var("GARDEN_PROFILE_DIR", previous),
        None => std::env::remove_var("GARDEN_PROFILE_DIR"),
    }
    let _ = std::fs::remove_dir_all(&root);
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}

fn block_on<F: std::future::Future>(future: F) -> F::Output {
    crate::app_runtime::async_runtime::block_on(future)
}

/// The owner's graph, before any request reaches the cell.
fn create_fixture_graph() {
    let app = crate::tauri_runtime::build_mock_cell_app_for_tests(GRAPH, OWNER, 1);
    crate::graph_service::create_graph_service(
        &app,
        crate::graph_service::CreateGraphInput {
            graph_id: Some(GRAPH.into()),
            title: "Viewer rooms fixture".into(),
            description: None,
            operation_id: None,
        },
    )
    .unwrap();
}

/// One request through the real router as `sub` with `role`; a reply that
/// does not come within `ANSWER_WITHIN` fails the test.
async fn call(
    state: &Arc<LoopbackState>,
    method: &str,
    path: &str,
    (sub, role): (&str, &str),
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header("authorization", format!("Bearer {TOKEN}"))
        .header("x-sophia-cell-lease", lease(sub, role));
    if body.is_some() {
        request = request.header("content-type", "application/json");
    }
    let body = match body {
        Some(body) => Body::from(serde_json::to_vec(&body).unwrap()),
        None => Body::empty(),
    };
    let request = request.body(body).unwrap();
    let response = tokio::time::timeout(
        ANSWER_WITHIN,
        crate::loopback_router::loopback_router(state.clone()).oneshot(request),
    )
    .await
    .unwrap_or_else(|_| panic!("{method} {path} did not answer within {ANSWER_WITHIN:?}"))
    .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1_000_000)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// An MCP tool call as `who`; a tool error fails the test.
async fn tool(state: &Arc<LoopbackState>, who: (&str, &str), name: &str, arguments: Value) {
    let body = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": {"name": name, "arguments": arguments}});
    let (status, reply) = call(state, "POST", "/mcp", who, Some(body)).await;
    assert_eq!(status, StatusCode::OK, "{name}: {reply}");
    assert!(reply["error"].is_null(), "{name}: {reply}");
    assert_ne!(reply["result"]["isError"], json!(true), "{name}: {reply}");
}

/// The owner's folder `shelf`, and in it the document `the-guide`.
async fn fill(state: &Arc<LoopbackState>) {
    let owner = (OWNER, "owner");
    tool(
        state,
        owner,
        "create_folder",
        json!({ "graphId": GRAPH, "folderId": FOLDER, "name": "Shelf" }),
    )
    .await;
    tool(
        state,
        owner,
        "create_document",
        json!({ "graphId": GRAPH, "documentId": DOC, "title": "The Guide", "parentId": FOLDER }),
    )
    .await;
    let content = "# The Guide\n\nThe owner wrote this.";
    tool(
        state,
        owner,
        "write_document",
        json!({ "graphId": GRAPH, "documentId": DOC, "content": content, "format": "markdown" }),
    )
    .await;
}

/// Serve the real router on a loopback port; the WebSocket base URL.
async fn serve(state: &Arc<LoopbackState>) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let router = crate::loopback_router::loopback_router(state.clone());
    tokio::spawn(async move {
        let _ = axum::serve(
            listener,
            router.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await;
    });
    format!("ws://{address}")
}

type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Open a room as the gateway opens it: the master token and `who`'s lease.
async fn connect(base: &str, path: &str, (sub, role): (&str, &str)) -> Result<Socket, String> {
    let mut request = format!("{base}{path}")
        .into_client_request()
        .map_err(|error| error.to_string())?;
    let headers = request.headers_mut();
    headers.insert("authorization", format!("Bearer {TOKEN}").parse().unwrap());
    headers.insert("x-sophia-cell-lease", lease(sub, role).parse().unwrap());
    tokio_tungstenite::connect_async(request)
        .await
        .map(|(socket, _)| socket)
        .map_err(|error| error.to_string())
}

/// Sync `doc` with the room: send our state vector, apply what comes back.
async fn sync(socket: &mut Socket, doc: &Doc) {
    let step1 = YMessage::Sync(SyncMessage::SyncStep1(doc.transact().state_vector())).encode_v1();
    socket.send(Ws::Binary(step1.into())).await.unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while tokio::time::Instant::now() < deadline {
        let Ok(Some(Ok(frame))) =
            tokio::time::timeout(Duration::from_millis(500), socket.next()).await
        else {
            continue;
        };
        if let Ws::Binary(data) = frame {
            if let Ok(YMessage::Sync(SyncMessage::SyncStep2(update))) = YMessage::decode_v1(&data) {
                doc.transact_mut()
                    .apply_update(Update::decode_v1(&update).unwrap())
                    .unwrap();
                return;
            }
        }
    }
    panic!("no SyncStep2 from the room");
}

/// Send one update made by `change` on `doc`; `Some(code)` when the room
/// closes on it.
async fn send_change(socket: &mut Socket, doc: &Doc, change: impl FnOnce(&Doc)) -> Option<u16> {
    let before = doc.transact().state_vector();
    change(doc);
    let update = doc.transact().encode_state_as_update_v1(&before);
    let message = YMessage::Sync(SyncMessage::Update(update)).encode_v1();
    socket.send(Ws::Binary(message.into())).await.ok()?;
    let deadline = tokio::time::Instant::now() + Duration::from_millis(1_500);
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(300), socket.next()).await {
            Ok(Some(Ok(Ws::Close(Some(frame))))) => return Some(u16::from(frame.code)),
            Ok(Some(Ok(Ws::Close(None)))) | Ok(None) | Ok(Some(Err(_))) => return Some(0),
            _ => {}
        }
    }
    None
}

fn paragraph(doc: &Doc, text: &str) {
    let fragment = doc.get_or_insert_xml_fragment("content");
    let mut txn = doc.transact_mut();
    let element = fragment.push_back(&mut txn, XmlElementPrelim::empty("paragraph"));
    element.push_back(&mut txn, XmlTextPrelim::new(text));
}

fn rename_folder(doc: &Doc, name: &str) {
    let folders = doc.get_or_insert_map("folders");
    let mut txn = doc.transact_mut();
    if let Some(Out::YMap(folder)) = folders.get(&txn, FOLDER) {
        folder.insert(&mut txn, "name", name);
    }
}

fn doc_text(doc: &Doc) -> String {
    let fragment = doc.get_or_insert_xml_fragment("content");
    let txn = doc.transact();
    fragment.get_string(&txn)
}

/// The document's text as the Owner reads it from its room.
async fn room_text(base: &str) -> String {
    let path = format!("/hocuspocus/docs/{GRAPH}/{DOC}");
    let mut socket = connect(base, &path, (OWNER, "owner")).await.unwrap();
    let doc = Doc::new();
    sync(&mut socket, &doc).await;
    doc_text(&doc)
}

/// A room's whole state as the cell holds it, byte for byte.
async fn room_state(state: &Arc<LoopbackState>, key: &str) -> Vec<u8> {
    let room = state
        .app
        .state::<RoomRegistry>()
        .peek(key)
        .await
        .unwrap_or_else(|| panic!("room {key} is open"));
    room.with_doc(|doc| {
        doc.transact()
            .encode_state_as_update_v1(&StateVector::default())
    })
    .await
}

/// The folder's name in the cell's workspace room.
async fn folder_name(state: &Arc<LoopbackState>) -> Option<String> {
    let room = state
        .app
        .state::<RoomRegistry>()
        .peek(&format!("workspace:{GRAPH}"))
        .await?;
    room.with_doc(|doc| {
        let txn = doc.transact();
        let folders = txn.get_map("folders")?;
        let Some(Out::YMap(folder)) = folders.get(&txn, FOLDER) else {
            return None;
        };
        match folder.get(&txn, "name") {
            Some(Out::Any(yrs::Any::String(name))) => Some(name.to_string()),
            _ => None,
        }
    })
    .await
}

fn graph_incarnation(state: &Arc<LoopbackState>) -> String {
    crate::graph_record_store::read_graph_record_no_heal(&state.app, GRAPH)
        .unwrap()
        .1
        .incarnation_id
        .expect("a new graph has an incarnation")
}

/// A Viewer reads the document and the tree; its paragraph and its rename
/// close the socket (4403) and leave each room byte for byte as it was. An
/// Editor's and the Owner's same frames land, on both rooms.
#[test]
fn a_viewer_reads_both_rooms_and_its_frames_change_nothing() {
    with_cell("frames", |profile| {
        block_on(async {
            create_fixture_graph();
            let state = state_for(profile);
            fill(&state).await;
            let base = serve(&state).await;
            let doc_path = format!("/hocuspocus/docs/{GRAPH}/{DOC}");
            let workspace_path = format!(
                "/hocuspocus/workspace/{GRAPH}?graph_incarnation={}",
                graph_incarnation(&state)
            );
            let doc_key = format!("doc:{GRAPH}:{DOC}");
            let workspace_key = format!("workspace:{GRAPH}");

            let mut socket = connect(&base, &doc_path, (VIEWER, "viewer")).await.unwrap();
            let doc = Doc::new();
            sync(&mut socket, &doc).await;
            assert!(
                doc_text(&doc).contains("The owner wrote this."),
                "a Viewer reads the document: {}",
                doc_text(&doc)
            );
            let before = room_state(&state, &doc_key).await;
            let closed =
                send_change(&mut socket, &doc, |doc| paragraph(doc, "typed by a viewer")).await;
            assert_eq!(closed, Some(VISITOR_REFUSED_CLOSE));
            assert_eq!(
                before,
                room_state(&state, &doc_key).await,
                "a refused Viewer frame changed the document"
            );
            assert!(!room_text(&base).await.contains("typed by a viewer"));

            let mut socket = connect(&base, &workspace_path, (VIEWER, "viewer"))
                .await
                .unwrap();
            let tree = Doc::new();
            sync(&mut socket, &tree).await;
            assert!(
                tree.transact().get_map("folders").is_some(),
                "a Viewer reads the tree"
            );
            let before = room_state(&state, &workspace_key).await;
            let closed =
                send_change(&mut socket, &tree, |doc| rename_folder(doc, "RENAMED BY A VIEWER"))
                    .await;
            assert_eq!(closed, Some(VISITOR_REFUSED_CLOSE));
            assert_eq!(
                before,
                room_state(&state, &workspace_key).await,
                "a refused Viewer frame changed the tree"
            );
            assert_eq!(folder_name(&state).await.as_deref(), Some("Shelf"));

            for who in [(EDITOR, "editor"), (OWNER, "owner")] {
                let marker = format!("typed by the {}", who.1);
                let mut socket = connect(&base, &doc_path, who).await.unwrap();
                let doc = Doc::new();
                sync(&mut socket, &doc).await;
                let closed = send_change(&mut socket, &doc, |doc| paragraph(doc, &marker)).await;
                assert_eq!(closed, None, "{who:?}");
                assert!(room_text(&base).await.contains(&marker), "{who:?}");

                let name = format!("Renamed by the {}", who.1);
                let mut socket = connect(&base, &workspace_path, who).await.unwrap();
                let tree = Doc::new();
                sync(&mut socket, &tree).await;
                let closed = send_change(&mut socket, &tree, |doc| rename_folder(doc, &name)).await;
                assert_eq!(closed, None, "{who:?}");
                assert_eq!(
                    folder_name(&state).await.as_deref(),
                    Some(name.as_str()),
                    "{who:?}"
                );
            }
        });
    });
}

/// A browser tab's presence identity, as Shrubbery sends it.
fn tab(human: &str, epoch: u64) -> Value {
    json!({
        "humanId": human,
        "deviceId": "device-1",
        "clientId": "tab-1",
        "connectionEpoch": epoch
    })
}

async fn send_awareness(socket: &mut Socket, client: u64, clock: u32, state: Value) {
    let mut clients = std::collections::HashMap::new();
    clients.insert(
        yrs::block::ClientID::new(client),
        yrs::sync::awareness::AwarenessUpdateEntry {
            clock,
            json: state.to_string().into(),
        },
    );
    let message = YMessage::Awareness(yrs::sync::AwarenessUpdate { clients }).encode_v1();
    socket.send(Ws::Binary(message.into())).await.unwrap();
}

/// Every awareness state `socket` hears within `window`, in order.
async fn presences(socket: &mut Socket, window: Duration) -> Vec<(u64, Value)> {
    let deadline = tokio::time::Instant::now() + window;
    let mut heard = Vec::new();
    while tokio::time::Instant::now() < deadline {
        if let Ok(Some(Ok(Ws::Binary(data)))) =
            tokio::time::timeout(Duration::from_millis(200), socket.next()).await
        {
            if let Ok(YMessage::Awareness(update)) = YMessage::decode_v1(&data) {
                for (client, entry) in update.clients {
                    let state = serde_json::from_str(&entry.json).unwrap_or(Value::Null);
                    heard.push((client.get(), state));
                }
            }
        }
    }
    heard
}

/// A Viewer's presence reaches the other people in the room as its own,
/// whatever it claims: it cannot speak as the Editor, take over the Editor's
/// tab session, or take over a client id the Editor's socket holds.
#[test]
fn a_viewers_presence_is_stamped_with_its_lease_principal() {
    with_cell("presence", |profile| {
        block_on(async {
            create_fixture_graph();
            let state = state_for(profile);
            fill(&state).await;
            let base = serve(&state).await;
            let path = format!("/hocuspocus/docs/{GRAPH}/{DOC}");
            let mut owner = connect(&base, &path, (OWNER, "owner")).await.unwrap();
            sync(&mut owner, &Doc::new()).await;
            let mut editor = connect(&base, &path, (EDITOR, "editor")).await.unwrap();
            sync(&mut editor, &Doc::new()).await;
            let mut viewer = connect(&base, &path, (VIEWER, "viewer")).await.unwrap();
            sync(&mut viewer, &Doc::new()).await;

            let claim = |epoch: u64| {
                json!({
                    "user": { "userId": EDITOR, "name": "Eddie", "color": "#336699" },
                    "presence": tab(EDITOR, epoch)
                })
            };
            send_awareness(&mut editor, 5151, 1, claim(1)).await;
            let heard = presences(&mut owner, Duration::from_secs(2)).await;
            assert!(
                heard
                    .iter()
                    .any(|(client, state)| *client == 5151 && state["user"]["userId"] == EDITOR),
                "the owner sees the editor: {heard:?}"
            );

            // The Viewer claims to be the Editor, in the Editor's tab, one connection later.
            send_awareness(&mut viewer, 4242, 1, claim(2)).await;
            let heard = presences(&mut owner, Duration::from_secs(2)).await;
            let stamped = heard
                .iter()
                .rev()
                .find(|(client, _)| *client == 4242)
                .map(|(_, state)| state.clone())
                .unwrap_or_else(|| panic!("the owner hears the viewer: {heard:?}"));
            assert_eq!(stamped["user"]["userId"], json!(VIEWER), "{stamped}");
            assert_eq!(stamped["presence"]["humanId"], json!(VIEWER), "{stamped}");
            assert_eq!(stamped["user"]["viewer"], json!(true), "{stamped}");
            assert!(
                !heard.iter().any(|(client, _)| *client == 5151),
                "the Viewer displaced the Editor's presence: {heard:?}"
            );

            // A higher clock on the Editor's client id is dropped.
            send_awareness(
                &mut viewer,
                5151,
                999,
                json!({ "user": { "name": "A viewer was here" } }),
            )
            .await;
            let heard = presences(&mut owner, Duration::from_millis(1_500)).await;
            assert!(
                !heard.iter().any(|(client, _)| *client == 5151),
                "the Viewer took over the Editor's client id: {heard:?}"
            );
        });
    });
}

/// A Viewer's open writes nothing: a document that does not exist gets no
/// room, no directory, no incarnation and no Y.Doc; a document whose
/// incarnation was never published (an older one) opens and stays without
/// one, where an Editor's open publishes it. A document in the Trash opens,
/// as for an Editor (a public visitor's does not).
#[test]
fn a_viewer_never_creates_on_open() {
    with_cell("creates", |profile| {
        block_on(async {
            create_fixture_graph();
            let state = state_for(profile);
            fill(&state).await;
            let base = serve(&state).await;
            let graph_dir = crate::profile_paths::graphs_dir(&state.app)
                .unwrap()
                .join(GRAPH);

            const MISSING: &str = "never-made";
            let missing = format!("/hocuspocus/docs/{GRAPH}/{MISSING}");
            assert!(connect(&base, &missing, (VIEWER, "viewer")).await.is_err());
            assert!(
                state
                    .app
                    .state::<RoomRegistry>()
                    .peek(&format!("doc:{GRAPH}:{MISSING}"))
                    .await
                    .is_none(),
                "a room was made for a missing document"
            );
            assert!(
                !crate::document_paths::document_dir(&graph_dir, MISSING)
                    .unwrap()
                    .exists(),
                "a directory (or an incarnation) was made for a missing document"
            );
            assert!(
                !crate::ydoc_paths::checked_document_ydoc_state_path(&graph_dir, MISSING)
                    .unwrap()
                    .exists(),
                "a Y.Doc was made for a missing document"
            );

            let path = format!("/hocuspocus/docs/{GRAPH}/{DOC}");
            let published = crate::document_paths::document_dir(&graph_dir, DOC)
                .unwrap()
                .join(crate::document_incarnation_store::DOCUMENT_INCARNATION_FILE);
            let _ = std::fs::remove_file(&published);
            let mut socket = connect(&base, &path, (VIEWER, "viewer")).await.unwrap();
            sync(&mut socket, &Doc::new()).await;
            assert!(!published.exists(), "a Viewer's open published an incarnation");
            let mut socket = connect(&base, &path, (EDITOR, "editor")).await.unwrap();
            sync(&mut socket, &Doc::new()).await;
            assert!(
                published.exists(),
                "an Editor's open publishes the incarnation"
            );

            let (status, reply) = call(
                &state,
                "DELETE",
                &format!("/navigation/{GRAPH}/documents/{DOC}"),
                (OWNER, "owner"),
                None,
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{reply}");
            assert!(crate::visitor_fence::trashed_documents(&state.app, GRAPH)
                .await
                .unwrap()
                .contains(DOC));
            let mut socket = connect(&base, &path, (VIEWER, "viewer"))
                .await
                .expect("a Viewer opens a document in the Trash");
            let doc = Doc::new();
            sync(&mut socket, &doc).await;
            assert!(doc_text(&doc).contains("The owner wrote this."));
        });
    });
}

/// In a cell whose graph does not exist yet, a Viewer's open of either room
/// is refused and heals nothing: no graph record, no graph directory.
#[test]
fn a_viewer_heals_no_missing_graph() {
    with_cell("heal", |profile| {
        block_on(async {
            let state = state_for(profile);
            let base = serve(&state).await;
            let graph_dir = crate::profile_paths::graphs_dir(&state.app)
                .unwrap()
                .join(GRAPH);
            const ANY_INCARNATION: &str = "00000000-0000-4000-8000-000000000000";
            for path in [
                format!("/hocuspocus/workspace/{GRAPH}?graph_incarnation={ANY_INCARNATION}"),
                format!("/hocuspocus/docs/{GRAPH}/{DOC}"),
            ] {
                assert!(
                    connect(&base, &path, (VIEWER, "viewer")).await.is_err(),
                    "{path}"
                );
                assert!(
                    !graph_dir.exists(),
                    "a Viewer's open made the graph: {path}"
                );
            }
        });
    });
}

/// `GET /health` names what the gateway checks before it sends a Viewer's
/// socket here.
#[test]
fn health_names_the_viewer_rooms_capability() {
    with_cell("health", |profile| {
        block_on(async {
            let state = state_for(profile);
            let request = Request::builder()
                .uri("/health")
                .body(Body::empty())
                .unwrap();
            let response = crate::loopback_router::loopback_router(state.clone())
                .oneshot(request)
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let bytes = axum::body::to_bytes(response.into_body(), 1_000_000)
                .await
                .unwrap();
            let health: Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(health["status"], "ok", "{health}");
            assert_eq!(
                health["capabilities"],
                json!(["viewer-rooms-v1"]),
                "{health}"
            );
        });
    });
}
