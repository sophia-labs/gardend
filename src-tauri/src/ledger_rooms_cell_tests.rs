//! The ledger room over real sockets (W6b item 8; M4, M6): through the real router over real HTTP and
//! WebSocket, with gateway leases signed as the gateway signs them, two clients speaking the
//! `ledger-vv/1` and `ledger-bundle/1` wire on `/hocuspocus/ledger/{graph}/{vocab}`. Each client's own
//! frames come from a second ledger (its own origin) in a scratch directory, so the bytes on the wire
//! are the crate's, the ones the bench client reproduces byte for byte. No mocks.
use super::*;
use crate::app_runtime::Manager;
use crate::crdt_engine::convergent::Convergent;
use crate::crdt_engine::ledger_room::{decode_bundle, decode_vv, encode_vv, LedgerRoom};
use crate::crdt_engine::rooms::{
    LedgerAdmissionError, RoomRegistry, LEDGER_DECODE_CLOSE, LEDGER_VERSION_CLOSE,
};
use axum::{body::Body, http::Request};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use futures_util::{SinkExt, StreamExt};
use garden_ledger::{Draft, Mode, OriginId, Subject, VersionVector};
use hmac::{Hmac, Mac};
use serde_json::{json, Value};
use std::{net::SocketAddr, time::Duration};
use tokio_tungstenite::tungstenite::{client::IntoClientRequest, Message as Ws};
use tower::ServiceExt;

const GRAPH: &str = "ledger-rooms-cell-fixture-20261104";
const OWNER: &str = "user:ledger-rooms-owner-20261104";
const VIEWER: &str = "user:ledger-rooms-viewer-20261104";
const SECRET: &[u8] = b"ledger-rooms-cell-test-secret-at-least-32-bytes";
const TOKEN: &str = "ledger-rooms-cell-disposable-token";
const VOCAB: &str = "notes";
const ANSWER_WITHIN: Duration = Duration::from_secs(10);

fn lease(sub: &str, role: &str) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let claims = json!({"iss":"pn-gateway","aud":"gardend-cell","sub":sub,"owner":OWNER,
        "graphId":GRAPH,"generation":1,"cellId":bound_cell_id(OWNER,GRAPH,1),"role":role,
        "policyRevision":1,"registryRevision":1,"sessionId":"ledger-rooms-test","iat":now,"exp":now+600});
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
    let root = std::env::temp_dir().join(format!(
        "garden-ledger-rooms-{name}-{}",
        uuid::Uuid::new_v4()
    ));
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

fn create_fixture_graph() {
    let app = crate::tauri_runtime::build_mock_cell_app_for_tests(GRAPH, OWNER, 1);
    crate::graph_service::create_graph_service(
        &app,
        crate::graph_service::CreateGraphInput {
            graph_id: Some(GRAPH.to_string()),
            title: "Ledger rooms fixture".to_string(),
            description: None,
            operation_id: None,
        },
    )
    .unwrap();
}

/// One request through the real router as `sub` with `role`.
async fn call(
    state: &Arc<LoopbackState>,
    path: &str,
    (sub, role): (&str, &str),
) -> (StatusCode, Value) {
    let request = Request::builder()
        .method("GET")
        .uri(path)
        .header("authorization", format!("Bearer {TOKEN}"))
        .header("x-sophia-cell-lease", lease(sub, role))
        .body(Body::empty())
        .unwrap();
    let response = tokio::time::timeout(
        ANSWER_WITHIN,
        crate::loopback_router::loopback_router(state.clone()).oneshot(request),
    )
    .await
    .unwrap_or_else(|_| panic!("GET {path} did not answer within {ANSWER_WITHIN:?}"))
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

async fn connect(base: &str, (sub, role): (&str, &str)) -> Result<Socket, String> {
    let mut request = format!("{base}/hocuspocus/ledger/{GRAPH}/{VOCAB}")
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

/// `[0][sub][varuint len][payload]`: a y-sync sync message.
fn sync_message(sub: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = vec![0u8, sub];
    let mut len = payload.len() as u64;
    loop {
        let byte = (len & 0x7f) as u8;
        len >>= 7;
        if len == 0 {
            out.push(byte);
            break;
        }
        out.push(byte | 0x80);
    }
    out.extend_from_slice(payload);
    out
}

/// A sync message's `(sub, payload)`; `None` for presence and anything else.
fn parse_sync(data: &[u8]) -> Option<(u8, Vec<u8>)> {
    if data.first() != Some(&0) {
        return None;
    }
    let sub = *data.get(1)?;
    let (mut len, mut shift, mut at) = (0u64, 0u32, 2usize);
    loop {
        let byte = *data.get(at)?;
        at += 1;
        len |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            break;
        }
        shift += 7;
    }
    Some((sub, data.get(at..at + len as usize)?.to_vec()))
}

/// The next sync message with sub-tag `want`, within `within`; anything else is skipped.
async fn next_sync(socket: &mut Socket, want: u8, within: Duration) -> Option<Vec<u8>> {
    let deadline = tokio::time::Instant::now() + within;
    while tokio::time::Instant::now() < deadline {
        let left = deadline - tokio::time::Instant::now();
        match tokio::time::timeout(left, socket.next()).await {
            Ok(Some(Ok(Ws::Binary(data)))) => {
                if let Some((sub, payload)) = parse_sync(&data) {
                    if sub == want {
                        return Some(payload);
                    }
                }
            }
            Ok(Some(Ok(_))) => {}
            _ => return None,
        }
    }
    None
}

/// The close frame the room sends (code, reason), within `within`.
async fn next_close(socket: &mut Socket, within: Duration) -> Option<(u16, String)> {
    let deadline = tokio::time::Instant::now() + within;
    while tokio::time::Instant::now() < deadline {
        let left = deadline - tokio::time::Instant::now();
        match tokio::time::timeout(left, socket.next()).await {
            Ok(Some(Ok(Ws::Close(Some(frame))))) => {
                return Some((u16::from(frame.code), frame.reason.to_string()))
            }
            Ok(Some(Ok(Ws::Close(None)))) | Ok(None) | Ok(Some(Err(_))) => {
                return Some((0, String::new()))
            }
            Ok(Some(Ok(_))) => {}
            Err(_) => return None,
        }
    }
    None
}

fn o(s: &str) -> OriginId {
    OriginId(s.to_string())
}

fn draft(tag: &str) -> Draft {
    Draft {
        witness: "https://example.org/witness".into(),
        vocab: VOCAB.into(),
        subjects: vec![Subject {
            subject: format!("https://example.org/s/{tag}"),
            class: "https://example.org/C".into(),
            parents: vec![],
        }],
        mode: Mode::Assert,
        payload: format!("{{\"tag\":\"{tag}\"}}").into_bytes(),
        txn: None,
    }
}

/// A client: a ledger of its own in a scratch directory, with its own origin.
struct Client {
    dir: std::path::PathBuf,
    room: Arc<LedgerRoom>,
}

impl Client {
    fn new(origin: &str) -> Client {
        let dir =
            std::env::temp_dir().join(format!("garden-ledger-client-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let room = LedgerRoom::open(&dir, VOCAB, o(origin)).unwrap();
        Client { dir, room }
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Connect as the Owner, take the room's SyncStep1, answer with ours and take its SyncStep2. Returns
/// the socket and the version vector the room announced.
async fn join(base: &str, client: &Client) -> (Socket, VersionVector) {
    let mut socket = connect(base, (OWNER, "owner")).await.unwrap();
    let theirs = next_sync(&mut socket, 0, Duration::from_secs(5))
        .await
        .expect("the room sends its SyncStep1");
    let theirs = decode_vv(&theirs).expect("a ledger-vv/1");
    let ours = client.room.version().await;
    socket
        .send(Ws::Binary(sync_message(0, &ours).into()))
        .await
        .unwrap();
    let step2 = next_sync(&mut socket, 1, Duration::from_secs(5))
        .await
        .expect("the room answers our SyncStep1 (an empty bundle when we lack nothing)");
    if theirs.0.is_empty() {
        assert!(decode_bundle(&step2).unwrap().is_empty());
    }
    (socket, theirs)
}

/// Client A appends two frames and sends them; client B, connected and in step, receives them from
/// the room's own ship task, with no frame of its own; the version route then reports them. The
/// version route reads a vocab that has no ledger without making one (M4).
#[test]
fn a_frame_sent_by_one_client_reaches_the_other_and_the_version_route_sees_it() {
    with_cell("converge", |profile| {
        block_on(async {
            create_fixture_graph();
            let state = state_for(profile);
            let base = serve(&state).await;
            let owner = (OWNER, "owner");
            let version_path = format!("/graphs/{GRAPH}/ledger/{VOCAB}/version");
            let graph_dir = crate::graph_paths::existing_graph_dir(&state.app, GRAPH).unwrap();

            // M4: a read of a vocab with no ledger makes nothing.
            let (status, body) = call(&state, &version_path, owner).await;
            assert_eq!(status, StatusCode::OK, "{body}");
            assert_eq!(body, json!({"origins": {}, "durable": {}, "local": null}));
            assert!(
                !graph_dir.join("ledger").exists(),
                "the version route made the ledger directory"
            );
            assert!(
                state
                    .app
                    .state::<RoomRegistry>()
                    .peek_ledger(&format!("ledger:{GRAPH}:{VOCAB}"))
                    .is_none(),
                "the version route hosted a room"
            );

            let (a, b) = (Client::new("client-a"), Client::new("client-b"));
            let (mut socket_a, server_vv) = join(&base, &a).await;
            let (mut socket_b, _) = join(&base, &b).await;
            assert!(server_vv.0.is_empty());

            for tag in ["one", "two"] {
                a.room.append(draft(tag)).await.unwrap();
            }
            a.room.durable().await.unwrap();
            let bundles = a.room.diff(&encode_vv(&server_vv), 1 << 20).await.unwrap();
            assert_eq!(bundles.len(), 1);
            socket_a
                .send(Ws::Binary(sync_message(2, &bundles[0]).into()))
                .await
                .unwrap();

            // B receives it as an Update, on the room's ship window, having sent nothing.
            let update = next_sync(&mut socket_b, 2, Duration::from_secs(2))
                .await
                .expect("the room shipped A's frames to B");
            assert!(b.room.apply(&update, 1).await.unwrap().changed);
            assert_eq!(
                decode_vv(&b.room.version().await)
                    .unwrap()
                    .get(&o("client-a")),
                2
            );

            let (status, body) = call(&state, &version_path, owner).await;
            assert_eq!(status, StatusCode::OK, "{body}");
            assert_eq!(body["origins"]["client-a"], 2);
            let local = body["local"].as_str().expect("the cell's origin");
            assert!(local.starts_with("cell-"), "{local}");
            // The shipped frames are durable by the time they ship.
            assert_eq!(body["durable"]["client-a"], 2);
        });
    });
}

/// M6: a version the room does not speak is named in a close frame, not skipped or dropped.
#[test]
fn an_unknown_wire_version_closes_with_a_reason() {
    with_cell("versions", |profile| {
        block_on(async {
            create_fixture_graph();
            let state = state_for(profile);
            let base = serve(&state).await;
            let client = Client::new("client-v");

            let (mut socket, _) = join(&base, &client).await;
            socket
                .send(Ws::Binary(sync_message(0, &[2, 0, 0, 0, 0]).into()))
                .await
                .unwrap();
            let (code, reason) = next_close(&mut socket, Duration::from_secs(3))
                .await
                .expect("a close");
            assert_eq!(code, LEDGER_VERSION_CLOSE);
            assert_eq!(reason, "ledger-vv version 2 unsupported");

            let (mut socket, _) = join(&base, &client).await;
            socket
                .send(Ws::Binary(sync_message(2, &[9]).into()))
                .await
                .unwrap();
            let (code, reason) = next_close(&mut socket, Duration::from_secs(3))
                .await
                .expect("a close");
            assert_eq!(code, LEDGER_VERSION_CLOSE);
            assert_eq!(reason, "ledger-bundle version 9 unsupported");

            // An update that does not decode closes with the decode reason.
            let (mut socket, _) = join(&base, &client).await;
            socket
                .send(Ws::Binary(sync_message(2, &[1, 0xff]).into()))
                .await
                .unwrap();
            let (code, reason) = next_close(&mut socket, Duration::from_secs(3))
                .await
                .expect("a close");
            assert_eq!(code, LEDGER_DECODE_CLOSE);
            assert!(reason.contains("short read"), "{reason}");
            assert!(reason.chars().count() <= 110);
        });
    });
}

/// A Viewer does not write the ledger; a deleted graph hosts no ledger room, and a graph deleted
/// under a connected socket closes it (B1, M4).
#[test]
fn a_deleted_graph_hosts_no_ledger_and_closes_the_sockets_it_had() {
    with_cell("deleted", |profile| {
        block_on(async {
            create_fixture_graph();
            let state = state_for(profile);
            let base = serve(&state).await;
            let registry = state.app.state::<RoomRegistry>();
            let key = format!("ledger:{GRAPH}:{VOCAB}");
            let client = Client::new("client-d");

            assert!(
                connect(&base, (VIEWER, "viewer")).await.is_err(),
                "a Viewer was admitted"
            );

            let (mut socket, _) = join(&base, &client).await;
            let room = registry.peek_ledger(&key).expect("the room is hosted");
            assert!(!room.is_evicted());

            // Graph deletion evicts the graph's rooms under the exclusive lease.
            crate::graph_service::soft_delete_graph_service_async(
                &state.app,
                GRAPH.to_string(),
                false,
            )
            .await
            .unwrap();
            registry.evict_graph(GRAPH);
            assert!(room.is_evicted());
            assert!(registry.peek_ledger(&key).is_none());
            let (code, _) = next_close(&mut socket, Duration::from_secs(3))
                .await
                .expect("the socket was closed");
            assert!(
                code == 0 || code == 1000 || code == 1005,
                "close code {code}"
            );

            // Nothing is hosted for the deleted graph any more.
            match registry.open_ledger(&state.app, GRAPH, VOCAB).await {
                Err(LedgerAdmissionError::NotFound(message)) => {
                    assert!(message.contains("graph not found"), "{message}")
                }
                other => panic!(
                    "a deleted graph opened a ledger room: {:?}",
                    other.map(|_| ())
                ),
            }
            assert!(registry.peek_ledger(&key).is_none());
            assert!(connect(&base, (OWNER, "owner")).await.is_err());
            let (status, _) = call(
                &state,
                &format!("/graphs/{GRAPH}/ledger/{VOCAB}/version"),
                (OWNER, "owner"),
            )
            .await;
            assert_eq!(status, StatusCode::NOT_FOUND);
        });
    });
}
