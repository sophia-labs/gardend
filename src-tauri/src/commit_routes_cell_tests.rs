//! The commit seam over the real router (W7 lane S): `POST /emporium/commit/{graph}/{vocab}` and its
//! watermark route, through the real loopback router with a real token and gateway leases signed as
//! the gateway signs them, against a real graph directory and a real ledger room. No mocks.
//!
//! Mounted from `cell_graph_boundary.rs` like `ledger_rooms_cell_tests.rs` (it needs that module's
//! private `CellGraphBoundary::new_with_binding` and `bound_cell_id`):
//!
//! ```text
//! #[cfg(all(test, feature = "headless"))]
//! #[path = "commit_routes_cell_tests.rs"]
//! mod commit_routes_cell_tests;
//! ```
//!
//! The ledger is read back the way a peer reads it: the room's own `diff` over the wire encoding,
//! decoded with the crate's frame decoder. Where a test needs the fold (`?wait=folded`) it needs
//! lane T's tail to be wired into the room registry; that test says so.
use super::*;
use crate::app_runtime::Manager;
use crate::crdt_engine::convergent::Convergent;
use crate::crdt_engine::ledger_room::{decode_bundle, encode_vv};
use crate::crdt_engine::rooms::RoomRegistry;
use axum::{body::Body, http::Request};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use garden_ledger::{Mode, VersionVector};
use hmac::{Hmac, Mac};
use serde_json::{json, Value};
use std::time::Duration;
use tower::ServiceExt;

const GRAPH: &str = "commit-routes-cell-fixture-20261104";
const OWNER: &str = "user:commit-routes-owner-20261104";
const SECRET: &[u8] = b"commit-routes-cell-test-secret-at-least-32-bytes";
const TOKEN: &str = "commit-routes-cell-disposable-token";
const AO: &str = "ao";
const ANSWER_WITHIN: Duration = Duration::from_secs(30);

const AO_CLASS: &str = "urn:ao:term";
const AO_NAME: &str = "urn:ao:term#name";

const BOOKMARK_VOCAB: &str = "emporium-bookmark";
const BOOKMARK_CLASS: &str = "http://mnemosyne.dev/bookmark#Bookmark";
const BOOKMARK_URL: &str = "http://mnemosyne.dev/bookmark#url";
const BOOKMARK_TITLE: &str = "http://mnemosyne.dev/bookmark#title";

fn lease() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let claims = json!({"iss":"pn-gateway","aud":"gardend-cell","sub":OWNER,"owner":OWNER,
        "graphId":GRAPH,"generation":1,"cellId":bound_cell_id(OWNER,GRAPH,1),"role":"owner",
        "policyRevision":1,"registryRevision":1,"sessionId":"commit-routes-test","iat":now,"exp":now+600});
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
        "garden-commit-routes-{name}-{}",
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
            title: "Commit routes fixture".to_string(),
            description: None,
            operation_id: None,
        },
    )
    .unwrap();
}

/// One request through the real router as the owner.
async fn send(
    state: &Arc<LoopbackState>,
    method: &str,
    path: &str,
    body: Vec<u8>,
) -> (StatusCode, Value) {
    let request = Request::builder()
        .method(method)
        .uri(path)
        .header("authorization", format!("Bearer {TOKEN}"))
        .header("x-sophia-cell-lease", lease())
        .header("content-type", "application/json")
        .body(Body::from(body))
        .unwrap();
    let response = tokio::time::timeout(
        ANSWER_WITHIN,
        crate::loopback_router::loopback_router(state.clone()).oneshot(request),
    )
    .await
    .unwrap_or_else(|_| panic!("{method} {path} did not answer within {ANSWER_WITHIN:?}"))
    .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 4_000_000)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn commit(
    state: &Arc<LoopbackState>,
    vocab: &str,
    query: &str,
    body: Value,
) -> (StatusCode, Value) {
    send(
        state,
        "POST",
        &format!("/emporium/commit/{GRAPH}/{vocab}{query}"),
        serde_json::to_vec(&body).unwrap(),
    )
    .await
}

async fn version(state: &Arc<LoopbackState>, vocab: &str) -> Value {
    let (status, body) = send(
        state,
        "GET",
        &format!("/graphs/{GRAPH}/ledger/{vocab}/version"),
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body
}

/// The cell's seq in the ledger (0 when it has none).
fn seq_of(version: &Value) -> u64 {
    match version["local"].as_str() {
        Some(local) => version["origins"][local].as_u64().unwrap_or(0),
        None => 0,
    }
}

fn set_name(subject: &str, name: &str) -> Value {
    json!({"op": "set", "subject": subject, "class": AO_CLASS, "predicate": AO_NAME, "value": name})
}

/// Every frame of the vocab's ledger, in seq order, read the way a peer reads it: make the room
/// durable, diff against an empty version vector, decode each raw frame.
async fn frames_of(state: &Arc<LoopbackState>, vocab: &str) -> Vec<garden_ledger::frame::Decoded> {
    let registry = state.app.state::<RoomRegistry>();
    let room = registry
        .peek_ledger(&format!("ledger:{GRAPH}:{vocab}"))
        .expect("the commit opened the ledger room");
    room.durable().await.unwrap();
    let bundles = room
        .diff(&encode_vv(&VersionVector::default()), 1 << 24)
        .await
        .unwrap();
    let mut out = Vec::new();
    for bundle in &bundles {
        for (_origin, raw) in decode_bundle(bundle).unwrap() {
            let mut at = 0;
            while at < raw.len() {
                let decoded = garden_ledger::frame::decode(&raw[at..]).unwrap();
                at += decoded.frame_len;
                out.push(decoded);
            }
        }
    }
    out.sort_by_key(|d| d.header.seq);
    out
}

/// A set is acked on the barrier with the frame's coordinates, and the version route then shows the
/// cell's seq advanced by exactly the frames the response named.
#[test]
fn a_set_is_acked_and_the_version_route_shows_the_seq_advance() {
    with_cell("ack", |profile| {
        block_on(async {
            create_fixture_graph();
            let state = state_for(profile);

            let (status, first) = commit(
                &state,
                AO,
                "",
                json!({"witness": "user-42", "ops": [set_name("urn:ao:term/id=1", "Customer")]}),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{first}");
            assert_eq!(first["ok"], true);
            assert_eq!(first["committed"], true);
            assert_eq!(first["folded"], false);
            let frames = first["frames"].as_array().unwrap();
            assert_eq!(frames.len(), 1);
            assert_eq!(frames[0]["seq"], 1);
            assert!(frames[0]["origin"].as_str().unwrap().starts_with("cell-"));
            assert_eq!(frames[0]["id"].as_str().unwrap().len(), 64);
            assert_eq!(seq_of(&version(&state, AO).await), 1);

            let (status, second) = commit(
                &state,
                AO,
                "",
                json!({"witness": "user-42", "ops": [
                    set_name("urn:ao:term/id=2", "Order"),
                    {"op": "add", "subject": "urn:ao:term/id=2", "class": AO_CLASS,
                     "predicate": "urn:ao:term#tag", "values": ["a", {"@value": "b", "@language": "en"}]}]}),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{second}");
            assert_eq!(
                second["frames"][0]["seq"], 2,
                "one request, one frame, no commit frame"
            );
            assert_eq!(seq_of(&version(&state, AO).await), 2);

            // The watermark route answers without opening anything new, and names the same version.
            let (status, mark) = send(
                &state,
                "GET",
                &format!("/emporium/commit/{GRAPH}/{AO}/watermark"),
                Vec::new(),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{mark}");
            let local = mark["local"].as_str().unwrap();
            assert_eq!(mark["version"][local], 2);
        });
    });
}

/// With a contract, the SHACL gate judges the subjects a set leaves behind: a conformant bookmark
/// is accepted; one missing its required url is 422 with violations and nothing is appended.
#[test]
fn a_shacl_halt_is_422_and_appends_nothing() {
    with_cell("gate", |profile| {
        block_on(async {
            create_fixture_graph();
            let state = state_for(profile);

            let good = json!({"witness": "w", "ops": [
                {"op": "set", "subject": "urn:test:bookmark/1", "class": BOOKMARK_CLASS,
                 "predicate": BOOKMARK_URL, "value": {"@id": "https://example.org/a"}},
                {"op": "set", "subject": "urn:test:bookmark/1", "class": BOOKMARK_CLASS,
                 "predicate": BOOKMARK_TITLE, "value": "A page"}]});
            let (status, body) = commit(&state, BOOKMARK_VOCAB, "", good).await;
            assert_eq!(status, StatusCode::OK, "{body}");
            let before = version(&state, BOOKMARK_VOCAB).await;
            assert_eq!(seq_of(&before), 1);

            let bad = json!({"witness": "w", "ops": [
                {"op": "set", "subject": "urn:test:bookmark/2", "class": BOOKMARK_CLASS,
                 "predicate": BOOKMARK_TITLE, "value": "No url"}]});
            let (status, body) = commit(&state, BOOKMARK_VOCAB, "", bad).await;
            assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
            assert_eq!(body["ok"], false);
            assert!(
                !body["violations"]
                    .as_array()
                    .expect("violations")
                    .is_empty(),
                "{body}"
            );
            assert!(
                body["error"].as_str().unwrap().starts_with("SHACL:"),
                "{body}"
            );
            assert_eq!(version(&state, BOOKMARK_VOCAB).await, before);
        });
    });
}

/// A body that is malformed appends nothing and creates no ledger.
#[test]
fn a_bad_body_is_400_and_creates_no_ledger() {
    with_cell("badbody", |profile| {
        block_on(async {
            create_fixture_graph();
            let state = state_for(profile);
            let graph_dir = crate::graph_paths::existing_graph_dir(&state.app, GRAPH).unwrap();

            // The witness is required.
            let (status, body) = commit(
                &state,
                AO,
                "",
                json!({"ops": [set_name("urn:ao:term/id=1", "x")]}),
            )
            .await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
            assert!(
                body["error"].as_str().unwrap().contains("witness"),
                "{body}"
            );

            for bad in [
                json!({"witness": "w", "ops": [{"op": "set", "subject": "no iri", "class": AO_CLASS,
                    "predicate": AO_NAME, "value": "x"}]}),
                json!({"witness": "w", "ops": [{"op": "set", "subject": "urn:ao:term/id=1",
                    "class": AO_CLASS, "predicate": AO_NAME,
                    "value": {"@value": "abc", "@type": "http://www.w3.org/2001/XMLSchema#integer"}}]}),
                json!({"witness": "w", "ops": []}),
                json!({"witness": "w", "typo": 1, "ops": [set_name("urn:ao:term/id=1", "x")]}),
            ] {
                let (status, body) = commit(&state, AO, "", bad.clone()).await;
                assert_eq!(status, StatusCode::BAD_REQUEST, "{bad} -> {body}");
            }
            let (status, _) = commit(
                &state,
                AO,
                "?wait=never",
                json!({"witness": "w", "ops": [set_name("urn:ao:term/id=1", "x")]}),
            )
            .await;
            assert_eq!(status, StatusCode::BAD_REQUEST);

            let after = version(&state, AO).await;
            assert_eq!(after, json!({"origins": {}, "durable": {}, "local": null}));
            assert!(
                !graph_dir.join("ledger").exists(),
                "a refusal made a ledger"
            );
        });
    });
}

/// A transaction without `commit` is an open run; another caller's request waits and is then refused
/// 409; the later commit-only request closes it, and the frames are what a peer would see.
#[test]
fn a_txn_without_commit_stays_open_and_a_later_commit_closes_it() {
    with_cell("txn", |profile| {
        block_on(async {
            create_fixture_graph();
            let state = state_for(profile);

            let (status, opened) = commit(
                &state,
                AO,
                "",
                json!({"witness": "w", "txn": "import-1", "ops": [set_name("urn:ao:term/id=1", "a")]}),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{opened}");
            assert_eq!(opened["committed"], false);
            assert_eq!(opened["txn"], "import-1");

            let frames = frames_of(&state, AO).await;
            assert_eq!(frames.len(), 1);
            assert_eq!(frames[0].header.txn.as_deref(), Some("import-1"));
            assert_ne!(frames[0].header.mode, Mode::Commit, "no commit frame yet");

            // A request that would break the run is refused, and appends nothing.
            let (status, refused) = commit(
                &state,
                AO,
                "",
                json!({"witness": "w", "ops": [set_name("urn:ao:term/id=9", "intruder")]}),
            )
            .await;
            assert_eq!(status, StatusCode::CONFLICT, "{refused}");
            assert_eq!(refused["openTxn"], "import-1");
            assert_eq!(seq_of(&version(&state, AO).await), 1);

            // The same txn continues the run.
            let (status, more) = commit(
                &state,
                AO,
                "",
                json!({"witness": "w", "txn": "import-1", "ops": [set_name("urn:ao:term/id=2", "b")]}),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{more}");
            assert_eq!(more["committed"], false);

            // A request with txn and no ops and commit:true closes it.
            let (status, closed) = commit(
                &state,
                AO,
                "",
                json!({"witness": "w", "txn": "import-1", "commit": true}),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{closed}");
            assert_eq!(closed["committed"], true);
            assert_eq!(closed["frames"].as_array().unwrap().len(), 1);

            let frames = frames_of(&state, AO).await;
            assert_eq!(frames.len(), 3);
            assert!(frames
                .iter()
                .all(|f| f.header.txn.as_deref() == Some("import-1")));
            assert_eq!(frames[2].header.mode, Mode::Commit);
            assert!(frames[2].payload.is_empty());
            assert_eq!(frames[2].header.witness, "w");
            assert_eq!(frames[2].header.vocab, AO);

            // Closed: an ordinary request goes through again.
            let (status, body) = commit(
                &state,
                AO,
                "",
                json!({"witness": "w", "ops": [set_name("urn:ao:term/id=3", "c")]}),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{body}");
            assert_eq!(body["frames"][0]["seq"], 4);
        });
    });
}

/// The witness is the frame's witness and the origin stays the cell's own; the payload is exactly
/// what lane F's reader decodes (it refuses any key but `ops`). The caller's writer is validated,
/// echoed, and recorded in the header's `extra.writer` (W7b F2); the response frames carry the hlc.
#[test]
fn the_witness_lands_in_the_frame_and_the_payload_is_what_the_fold_reads() {
    with_cell("writer", |profile| {
        block_on(async {
            create_fixture_graph();
            let state = state_for(profile);

            let (status, body) = commit(
                &state,
                AO,
                "",
                json!({"witness": "user-42", "writer": "nidra-worker-1",
                       "ops": [set_name("urn:ao:term/id=1", "Customer")]}),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{body}");
            assert_eq!(body["writer"], "nidra-worker-1");

            let frames = frames_of(&state, AO).await;
            assert_eq!(frames.len(), 1);
            let frame = &frames[0];
            assert_eq!(frame.header.witness, "user-42");
            assert_eq!(frame.header.vocab, AO);
            assert_eq!(frame.header.mode, Mode::Assert);
            assert_eq!(frame.header.txn, None);
            assert_eq!(
                frame.header.origin.0,
                body["frames"][0]["origin"].as_str().unwrap()
            );
            assert!(
                frame.header.origin.0.starts_with("cell-"),
                "the origin is the cell, not the writer"
            );
            assert_eq!(frame.header.subjects.len(), 1);
            assert_eq!(frame.header.subjects[0].subject, "urn:ao:term/id=1");
            assert_eq!(frame.header.subjects[0].class, AO_CLASS);
            let payload: Value = serde_json::from_slice(&frame.payload).unwrap();
            assert_eq!(payload.as_object().unwrap().len(), 1, "only `ops`");
            assert_eq!(payload["ops"].as_array().unwrap().len(), 1);
            assert_eq!(payload["ops"][0]["op"], "set");
            let ops = crate::emporium::ledger_fold::ops_from_payload(&frame.payload)
                .expect("the fold reads what the route wrote");
            assert_eq!(ops.len(), 1);
            // W7b F2: the writer is in the header, the hlc in the response is the header's.
            assert_eq!(frame.header.extra["writer"], "nidra-worker-1");
            assert_eq!(frame.header.extra.len(), 1);
            assert_eq!(
                body["frames"][0]["hlc"],
                json!({"ms": frame.header.hlc.ms, "count": frame.header.hlc.count}),
                "the response names the frame's hlc"
            );
            assert_eq!(
                body["frames"][0]["id"],
                garden_ledger::frame::to_hex(&frame.id)
            );

            // A request without a writer leaves the header's extra empty; a txn carries the writer
            // onto its commit frame too.
            let (status, plain) = commit(
                &state,
                AO,
                "",
                json!({"witness": "user-42", "ops": [set_name("urn:ao:term/id=2", "Order")]}),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{plain}");
            assert!(plain.get("writer").is_none());
            let (status, closed) = commit(
                &state,
                AO,
                "",
                json!({"witness": "user-42", "writer": "nidra-worker-2", "txn": "w-1", "commit": true,
                       "ops": [set_name("urn:ao:term/id=3", "Rental")]}),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{closed}");
            let frames = frames_of(&state, AO).await;
            assert_eq!(frames.len(), 4);
            assert!(frames[1].header.extra.is_empty());
            assert_eq!(frames[2].header.extra["writer"], "nidra-worker-2");
            assert_eq!(frames[3].header.mode, Mode::Commit);
            assert_eq!(frames[3].header.extra["writer"], "nidra-worker-2");
            let stamps: Vec<_> = frames.iter().map(|f| f.header.hlc).collect();
            for (frame, hlc) in closed["frames"]
                .as_array()
                .unwrap()
                .iter()
                .zip(&stamps[2..])
            {
                assert_eq!(frame["hlc"], json!({"ms": hlc.ms, "count": hlc.count}));
            }
        });
    });
}

/// A body over 64 MiB is 413 before it is parsed, and nothing is opened.
#[test]
fn a_body_over_64_mib_is_413() {
    with_cell("toolarge", |profile| {
        block_on(async {
            create_fixture_graph();
            let state = state_for(profile);
            let limit = crate::loopback_commit_routes::MAX_REQUEST_BYTES;
            for size in [limit + 1, limit + 2] {
                let (status, body) = send(
                    &state,
                    "POST",
                    &format!("/emporium/commit/{GRAPH}/{AO}"),
                    vec![b' '; size],
                )
                .await;
                assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{size}: {body}");
            }
            assert_eq!(
                version(&state, AO).await,
                json!({"origins": {}, "durable": {}, "local": null})
            );
        });
    });
}

/// A graph that does not exist is 404 on both routes.
#[test]
fn an_unknown_graph_is_404() {
    with_cell("unknown", |profile| {
        block_on(async {
            create_fixture_graph();
            let state = state_for(profile);
            let (status, _) = send(
                &state,
                "POST",
                &format!("/emporium/commit/{GRAPH}-missing/{AO}"),
                serde_json::to_vec(
                    &json!({"witness": "w", "ops": [set_name("urn:ao:term/id=1", "x")]}),
                )
                .unwrap(),
            )
            .await;
            // The graph-bound cell refuses a path graph that is not its own before the handler sees
            // it (403); a cell not bound to a graph answers 404. Either way nothing is written.
            assert!(
                status == StatusCode::NOT_FOUND || status == StatusCode::FORBIDDEN,
                "{status}"
            );
        });
    });
}

/// `?wait=folded` returns once the fold's watermark covers the frame, and the sink then holds the
/// triples. Needs lane T's tail wired into the room registry and the `ao` sink (`user:rdf`).
#[test]
fn wait_folded_returns_once_the_sink_holds_the_frame() {
    with_cell("folded", |profile| {
        block_on(async {
            create_fixture_graph();
            let state = state_for(profile);

            let (status, body) = commit(
                &state,
                AO,
                "?wait=folded",
                json!({"witness": "w", "ops": [set_name("urn:ao:term/id=1", "Customer")]}),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{body}");
            assert_eq!(body["folded"], true, "{body}");

            let (status, rows) = send(
                &state,
                "POST",
                "/api/sparql/query",
                serde_json::to_vec(&json!({
                    "graphId": GRAPH,
                    "sparql": format!(
                        "SELECT ?o WHERE {{ GRAPH ?g {{ <urn:ao:term/id=1> <{AO_NAME}> ?o }} }}"
                    )
                }))
                .unwrap(),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{rows}");
            assert_eq!(rows["rows"][0]["o"], "\"Customer\"", "{rows}");

            let (status, mark) = send(
                &state,
                "GET",
                &format!("/emporium/commit/{GRAPH}/{AO}/watermark"),
                Vec::new(),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{mark}");
            assert_eq!(mark["folded"], true, "{mark}");
            assert_eq!(mark["lagFrames"], 0, "{mark}");
        });
    });
}

/// W7b F1. A transaction left open and idle no longer blocks a stranger, whose frame then breaks the
/// run; the late continuation and the late commit are both refused 409 `txnBroken` with the seq of
/// the frame that broke it, and nothing is appended by either. Mutation: skip `check_run_intact` in
/// `commit_inner` and the commit-only request is acked `committed: true` over a run that closes
/// nothing (the F1 finding).
#[test]
fn a_stranger_after_the_idle_breaks_a_txn_and_the_late_commit_is_refused() {
    with_cell("broken", |profile| {
        block_on(async {
            create_fixture_graph();
            let state = state_for(profile);

            let (status, opened) = commit(
                &state,
                AO,
                "",
                json!({"witness": "w", "txn": "import-9", "ops": [set_name("urn:ao:term/id=1", "a")]}),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{opened}");
            assert_eq!(opened["committed"], false);

            // Past the idle window the open transaction blocks nobody.
            tokio::time::sleep(
                crate::loopback_commit_routes::txn_idle() + Duration::from_millis(250),
            )
            .await;
            let (status, stranger) = commit(
                &state,
                AO,
                "",
                json!({"witness": "w", "ops": [set_name("urn:ao:term/id=9", "intruder")]}),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{stranger}");
            assert_eq!(stranger["frames"][0]["seq"], 2);

            // The commit-only request is refused, and says which frame broke the run.
            let (status, refused) = commit(
                &state,
                AO,
                "",
                json!({"witness": "w", "txn": "import-9", "commit": true}),
            )
            .await;
            assert_eq!(status, StatusCode::CONFLICT, "{refused}");
            assert_eq!(refused["ok"], false);
            assert_eq!(refused["code"], "txnBroken");
            assert_eq!(refused["txn"], "import-9");
            assert_eq!(refused["broken_by_seq"], 2);
            assert_eq!(
                seq_of(&version(&state, AO).await),
                2,
                "a commit was appended"
            );

            // So is a continuation, with or without commit.
            for commit_flag in [false, true] {
                let (status, refused) = commit(
                    &state,
                    AO,
                    "",
                    json!({"witness": "w", "txn": "import-9", "commit": commit_flag,
                           "ops": [set_name("urn:ao:term/id=2", "b")]}),
                )
                .await;
                assert_eq!(status, StatusCode::CONFLICT, "{refused}");
                assert_eq!(refused["code"], "txnBroken");
                assert_eq!(seq_of(&version(&state, AO).await), 2);
            }

            let frames = frames_of(&state, AO).await;
            assert_eq!(frames.len(), 2);
            assert!(frames.iter().all(|f| f.header.mode != Mode::Commit));

            // The write can be redone under a new txn, and ordinary requests still pass.
            let (status, redone) = commit(
                &state,
                AO,
                "",
                json!({"witness": "w", "txn": "import-10", "commit": true,
                       "ops": [set_name("urn:ao:term/id=1", "a")]}),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{redone}");
            assert_eq!(redone["committed"], true);
        });
    });
}

/// W7b F1. A restart forgets the gate. The first request to the ledger reads the runs it already
/// holds: an intact open run can be committed (once), a broken one cannot, and a commit-only request
/// for a transaction that was never opened has nothing to close. Mutation: start every process with
/// an empty record (no `sync_gate` scan) and the broken run is committed over a stranger's frame.
#[test]
fn a_cold_gate_learns_the_runs_the_ledger_already_holds() {
    with_cell("cold", |profile| {
        block_on(async {
            create_fixture_graph();
            let state = state_for(profile);

            // Two open runs: t-intact is the last thing written, t-broken is followed by a stranger.
            let open = |txn: &str, id: u32| json!({"witness": "w", "txn": txn, "ops": [set_name(&format!("urn:ao:term/id={id}"), "x")]});
            assert_eq!(
                commit(&state, AO, "", open("t-broken", 1)).await.0,
                StatusCode::OK
            );
            crate::loopback_commit_routes::forget_gates_for_test();
            // Cold: nothing blocks, so a stranger goes in and breaks t-broken.
            let (status, stranger) = commit(
                &state,
                AO,
                "",
                json!({"witness": "w", "ops": [set_name("urn:ao:term/id=9", "intruder")]}),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{stranger}");
            assert_eq!(
                commit(&state, AO, "", open("t-intact", 2)).await.0,
                StatusCode::OK
            );
            assert_eq!(seq_of(&version(&state, AO).await), 3);

            // The restart.
            crate::loopback_commit_routes::forget_gates_for_test();

            let (status, refused) = commit(
                &state,
                AO,
                "",
                json!({"witness": "w", "txn": "t-broken", "commit": true}),
            )
            .await;
            assert_eq!(status, StatusCode::CONFLICT, "{refused}");
            assert_eq!(refused["code"], "txnBroken");
            assert_eq!(refused["broken_by_seq"], 2);
            assert_eq!(seq_of(&version(&state, AO).await), 3);

            let (status, closed) = commit(
                &state,
                AO,
                "",
                json!({"witness": "w", "txn": "t-intact", "commit": true}),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{closed}");
            assert_eq!(closed["committed"], true);
            assert_eq!(closed["frames"][0]["seq"], 4);

            // A second commit finds the transaction closed (W7b-2 c); a transaction nobody opened
            // has no run to close.
            let (status, again) = commit(
                &state,
                AO,
                "",
                json!({"witness": "w", "txn": "t-intact", "commit": true}),
            )
            .await;
            assert_eq!(status, StatusCode::CONFLICT, "{again}");
            assert_eq!(again["code"], "txnClosed");
            for txn in ["t-never"] {
                let (status, refused) = commit(
                    &state,
                    AO,
                    "",
                    json!({"witness": "w", "txn": txn, "commit": true}),
                )
                .await;
                assert_eq!(status, StatusCode::CONFLICT, "{txn}: {refused}");
                assert_eq!(refused["code"], "txnBroken");
                assert!(refused["broken_by_seq"].is_null());
            }
            assert_eq!(seq_of(&version(&state, AO).await), 4);
        });
    });
}

/// W7b F5. The replay mode is `?into=` or the body `{into}`; they must agree; an unknown body is 400.
#[test]
fn replay_takes_its_mode_from_the_query_or_the_body() {
    with_cell("replay", |profile| {
        block_on(async {
            create_fixture_graph();
            let state = state_for(profile);
            let (status, body) = commit(
                &state,
                AO,
                "?wait=folded",
                json!({"witness": "w", "ops": [set_name("urn:ao:term/id=1", "Customer")]}),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{body}");
            async fn replay(
                state: &Arc<LoopbackState>,
                query: &str,
                body: &str,
            ) -> (StatusCode, Value) {
                send(
                    state,
                    "POST",
                    &format!("/emporium/fold/{GRAPH}/{AO}/replay{query}"),
                    body.as_bytes().to_vec(),
                )
                .await
            }

            let (status, report) = replay(&state, "", r#"{"into":"oracle"}"#).await;
            assert_eq!(status, StatusCode::OK, "{report}");
            assert_eq!(report["into"], "oracle");
            assert_eq!(report["equal"], true);
            let (status, report) = replay(&state, "?into=oracle", "").await;
            assert_eq!(status, StatusCode::OK, "{report}");
            assert_eq!(report["into"], "oracle");
            let (status, report) = replay(&state, "", r#"{"into":"live"}"#).await;
            assert_eq!(status, StatusCode::OK, "{report}");
            assert_eq!(report["into"], "live");
            let (status, report) = replay(&state, "?into=live", r#"{"into":"live"}"#).await;
            assert_eq!(status, StatusCode::OK, "{report}");

            for (query, body) in [
                ("?into=live", r#"{"into":"oracle"}"#),
                ("?into=sideways", ""),
                ("", r#"{"into":"sideways"}"#),
                ("", r#"{"mode":"live"}"#),
                ("", "not json"),
            ] {
                let (status, refused) = replay(&state, query, body).await;
                assert_eq!(status, StatusCode::BAD_REQUEST, "{query} {body}: {refused}");
            }
        });
    });
}

/// W7b-2 c. A retry of `ops + txn + commit:true` after a lost response must not append a second run
/// under the same txn: the ledger holds a commit frame for it, so the retry (and a commit-only
/// retry) is `409 txnClosed {txn}` with nothing appended, warm and after a restart. Mutation: drop
/// the `closed` test in `check_run_intact` and the retry appends a second run (the doc's old claim).
#[test]
fn a_retry_after_a_lost_response_of_a_committed_txn_is_refused_txn_closed() {
    with_cell("retry", |profile| {
        block_on(async {
            create_fixture_graph();
            let state = state_for(profile);
            let request = json!({"witness": "w", "txn": "import-1", "commit": true,
                                 "ops": [set_name("urn:ao:term/id=1", "a")]});
            let (status, first) = commit(&state, AO, "", request.clone()).await;
            assert_eq!(status, StatusCode::OK, "{first}");
            assert_eq!(first["committed"], true);
            let after_first = seq_of(&version(&state, AO).await);

            for restart in [false, true] {
                if restart {
                    crate::loopback_commit_routes::forget_gates_for_test();
                }
                for body in [
                    request.clone(),
                    json!({"witness": "w", "txn": "import-1", "commit": true}),
                ] {
                    let (status, refused) = commit(&state, AO, "", body).await;
                    assert_eq!(status, StatusCode::CONFLICT, "restart {restart}: {refused}");
                    assert_eq!(refused["ok"], false);
                    assert_eq!(refused["code"], "txnClosed");
                    assert_eq!(refused["txn"], "import-1");
                    assert_eq!(seq_of(&version(&state, AO).await), after_first);
                }
            }
            // A different txn is unaffected.
            let mut next = request.clone();
            next["txn"] = json!("import-2");
            assert_eq!(commit(&state, AO, "", next).await.0, StatusCode::OK);
        });
    });
}

/// W7c, the Nidra shape end to end: one predicate carries an `rdf:JSON` whole and its members. The
/// column is written with `add`, then rewritten twice with `set values[]` through the real router;
/// the sink holds exactly one `rdf:JSON` whole (the readers' `FILTER(datatype(?v) = rdf:JSON)`) and
/// the newest members, and an oracle replay is equal. Mutation: make `set` clear only the single
/// layer (the W7b fold) and the first rewrite leaves two wholes.
#[test]
fn a_json_whole_and_members_rewritten_with_set_values_leave_one_whole() {
    with_cell("setvalues", |profile| {
        block_on(async {
            create_fixture_graph();
            let state = state_for(profile);
            let col = "urn:ao:column#values";
            let json_t = "http://www.w3.org/1999/02/22-rdf-syntax-ns#JSON";
            let whole = |text: &str| json!({"@value": text, "@type": json_t});
            let op = |kind: &str, values: Value| {
                json!({"op": kind, "subject": "urn:ao:column/id=1", "class": "urn:ao:column",
                       "predicate": col, "values": values})
            };
            let write = |ops: Vec<Value>| json!({"witness": "w", "ops": ops});

            let (status, first) = commit(
                &state,
                AO,
                "?wait=folded",
                write(vec![op("add", json!([whole("[\"a\"]"), "a"]))]),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{first}");
            for (rewrite, wanted) in [
                (json!([whole("[\"b\",\"c\"]"), "b", "c"]), 3),
                (json!([whole("[\"d\"]"), "d"]), 2),
            ] {
                let (status, done) =
                    commit(&state, AO, "?wait=folded", write(vec![op("set", rewrite)])).await;
                assert_eq!(status, StatusCode::OK, "{done}");
                assert_eq!(done["folded"], true, "{done}");
                let q = |filter: &str| {
                    format!(
                        "SELECT ?o WHERE {{ GRAPH ?g {{ <urn:ao:column/id=1> <{col}> ?o {filter} }} }}"
                    )
                };
                let ask = |sparql: String| {
                    let state = state.clone();
                    async move {
                        let (status, rows) = send(
                            &state,
                            "POST",
                            "/api/sparql/query",
                            serde_json::to_vec(&json!({"graphId": GRAPH, "sparql": sparql}))
                                .unwrap(),
                        )
                        .await;
                        assert_eq!(status, StatusCode::OK, "{rows}");
                        rows["rows"].as_array().unwrap().len()
                    }
                };
                assert_eq!(ask(q("")).await, wanted, "all values");
                assert_eq!(
                    ask(q(&format!("FILTER(datatype(?o) = <{json_t}>)"))).await,
                    1,
                    "exactly one rdf:JSON whole"
                );
            }
            let (status, report) = send(
                &state,
                "POST",
                &format!("/emporium/fold/{GRAPH}/{AO}/replay?into=oracle"),
                Vec::new(),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{report}");
            assert_eq!(report["equal"], true, "{report}");
        });
    });
}

/// W7d, Nidra's own case end to end: the aliases column rewritten to the empty list as the whole
/// `[]` alone (one value), then the documented `remove [whole]` + `add [whole2, members]` in one
/// request leaves exactly one `rdf:JSON` whole (the readers' `FILTER(datatype(?v) = rdf:JSON)`), and
/// a scalar set followed by a remove of that value leaves nothing. An oracle replay is equal.
/// Mutation: put a one-element set back into the single layer and the remove leaves the old whole
/// (two wholes, one scalar left over).
#[test]
fn a_set_of_one_value_composes_with_remove_and_add_through_the_router() {
    with_cell("setone", |profile| {
        block_on(async {
            create_fixture_graph();
            let state = state_for(profile);
            let col = "urn:ao:column#aliases";
            let json_t = "http://www.w3.org/1999/02/22-rdf-syntax-ns#JSON";
            let whole = |text: &str| json!({"@value": text, "@type": json_t});
            let op = |kind: &str, values: Value| {
                json!({"op": kind, "subject": "urn:ao:column/id=2", "class": "urn:ao:column",
                       "predicate": col, "values": values})
            };
            let write = |ops: Vec<Value>| json!({"witness": "w", "ops": ops});
            let ask = |sparql: String| {
                let state = state.clone();
                async move {
                    let (status, rows) = send(
                        &state,
                        "POST",
                        "/api/sparql/query",
                        serde_json::to_vec(&json!({"graphId": GRAPH, "sparql": sparql})).unwrap(),
                    )
                    .await;
                    assert_eq!(status, StatusCode::OK, "{rows}");
                    rows["rows"].as_array().unwrap().len()
                }
            };
            let q = |filter: &str| {
                format!(
                    "SELECT ?o WHERE {{ GRAPH ?g {{ <urn:ao:column/id=2> <{col}> ?o {filter} }} }}"
                )
            };
            let wholes_q = format!("FILTER(datatype(?o) = <{json_t}>)");

            // the column held members, then is rewritten to the empty list: the whole alone
            let (status, r) = commit(
                &state,
                AO,
                "?wait=folded",
                write(vec![op("set", json!([whole("[\"a\"]"), "a"]))]),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{r}");
            let (status, r) = commit(
                &state,
                AO,
                "?wait=folded",
                write(vec![op("set", json!([whole("[]")]))]),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{r}");
            assert_eq!(ask(q("")).await, 1);
            assert_eq!(ask(q(&wholes_q)).await, 1);

            // remove [whole] + add [whole2, members] in one request
            let (status, r) = commit(
                &state,
                AO,
                "?wait=folded",
                write(vec![
                    op("remove", json!([whole("[]")])),
                    op("add", json!([whole("[\"m1\",\"m2\"]"), "m1", "m2"])),
                ]),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{r}");
            assert_eq!(r["folded"], true, "{r}");
            assert_eq!(ask(q("")).await, 3, "whole2 and two members, nothing else");
            assert_eq!(ask(q(&wholes_q)).await, 1, "exactly one rdf:JSON whole");

            // a scalar set, then a remove of that value, leaves nothing
            let scalar = json!({"op": "set", "subject": "urn:ao:column/id=2", "class": "urn:ao:column",
                                "predicate": "urn:ao:column#label", "value": "W"});
            let drop = json!({"op": "remove", "subject": "urn:ao:column/id=2", "class": "urn:ao:column",
                              "predicate": "urn:ao:column#label", "values": ["W"]});
            for op in [scalar, drop] {
                let (status, r) = commit(&state, AO, "?wait=folded", write(vec![op])).await;
                assert_eq!(status, StatusCode::OK, "{r}");
            }
            assert_eq!(
                ask("SELECT ?o WHERE { GRAPH ?g { <urn:ao:column/id=2> <urn:ao:column#label> ?o } }"
                    .to_string())
                .await,
                0
            );

            let (status, report) = send(
                &state,
                "POST",
                &format!("/emporium/fold/{GRAPH}/{AO}/replay?into=oracle"),
                Vec::new(),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{report}");
            assert_eq!(report["equal"], true, "{report}");
        });
    });
}
