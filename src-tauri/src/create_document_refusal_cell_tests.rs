//! The hosted cell's side of `crdt_engine::workspace_ops_poison_tests`:
//! through the real router, with a gateway lease, a create the engine refuses
//! reaches the caller at once, as an MCP tool error and as an HTTP 400, and
//! the graph takes the next write. Before the fix the call never answered
//! (agent-core V4, 2026-10-13).
use super::*;
use axum::{body::Body, http::Request};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use hmac::{Hmac, Mac};
use serde_json::{json, Value};
use tower::ServiceExt;

const GRAPH: &str = "create-refusal-cell-fixture-20261013";
const OWNER: &str = "user:create-refusal-cell-owner-20261013";
const SECRET: &[u8] = b"create-refusal-cell-test-secret-at-least-32-bytes";
const TOKEN: &str = "create-refusal-cell-disposable-token";
const ANSWER_WITHIN: std::time::Duration = std::time::Duration::from_secs(10);
const LONG_TITLE: &str = "Which document in this workspace describes the battery charger? \
Answer with its title, in one short sentence.";

fn editor_lease() -> String {
    editor_lease_for(OWNER)
}

/// A gateway lease for `sub`, an editor of the owner's graph.
fn editor_lease_for(sub: &str) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let claims = json!({"iss":"pn-gateway","aud":"gardend-cell","sub":sub,"owner":OWNER,
        "graphId":GRAPH,"generation":1,"cellId":bound_cell_id(OWNER,GRAPH,1),"role":"editor",
        "policyRevision":1,"registryRevision":1,"sessionId":"create-refusal-test","iat":now,"exp":now+600});
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

/// One request through the real router; a reply that does not come within
/// `ANSWER_WITHIN` fails the test (the operation is parked for retry).
async fn send(state: &Arc<LoopbackState>, path: &str, body: Value) -> (StatusCode, Value) {
    send_as(state, OWNER, path, body).await
}

async fn send_as(
    state: &Arc<LoopbackState>,
    sub: &str,
    path: &str,
    body: Value,
) -> (StatusCode, Value) {
    let request = Request::builder()
        .method("POST")
        .uri(path)
        .header("authorization", format!("Bearer {TOKEN}"))
        .header("x-sophia-cell-lease", editor_lease_for(sub))
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let response = tokio::time::timeout(
        ANSWER_WITHIN,
        crate::loopback_router::loopback_router(state.clone()).oneshot(request),
    )
    .await
    .unwrap_or_else(|_| panic!("POST {path} did not answer within {ANSWER_WITHIN:?}: {body}"))
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

fn create_document_call(arguments: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": {"name": "create_document", "arguments": arguments}})
}

#[test]
fn a_refused_create_is_an_mcp_error_and_an_http_400_at_once() {
    let _serial = crate::tauri_runtime::profile_env_serial()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let root = std::env::temp_dir().join(format!("garden-create-refusal-{}", uuid::Uuid::new_v4()));
    let profile = root.join("profile");
    std::fs::create_dir_all(&profile).unwrap();
    std::env::set_var("GARDEN_PROFILE_DIR", &profile);
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        crate::app_runtime::async_runtime::block_on(async {
            let app = crate::tauri_runtime::build_mock_cell_app_for_tests(GRAPH, OWNER, 1);
            crate::graph_service::create_graph_service(
                &app,
                crate::graph_service::CreateGraphInput {
                    graph_id: Some(GRAPH.into()),
                    title: "Create refusal fixture".into(),
                    description: None,
                    operation_id: None,
                },
            )
            .unwrap();
            drop(app);
            let state = state_for(&profile);

            let (status, reply) = send(
                &state,
                "/mcp",
                create_document_call(json!({"graphId": GRAPH, "title": LONG_TITLE})),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{reply}");
            assert!(
                reply["error"]["message"]
                    .as_str()
                    .is_some_and(|message| message.contains("title must be 96 characters or fewer")),
                "{reply}"
            );

            let (status, reply) = send(
                &state,
                &format!("/api/graphs/{GRAPH}/documents"),
                json!({"title": LONG_TITLE}),
            )
            .await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{reply}");
            assert!(reply.to_string().contains("96 characters"), "{reply}");

            let (status, reply) = send(
                &state,
                "/mcp",
                create_document_call(json!({"graphId": GRAPH, "title": "Short enough"})),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{reply}");
            assert!(
                reply["error"].is_null(),
                "the graph takes the next write: {reply}"
            );
            assert_eq!(
                reply["result"]["structuredContent"]["title"], "Short enough",
                "{reply}"
            );
        })
    }));
    std::env::remove_var("GARDEN_PROFILE_DIR");
    let _ = std::fs::remove_dir_all(&root);
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}

fn tool_call(name: &str, arguments: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": {"name": name, "arguments": arguments}})
}

/// The hosted side of `integrity_mcp_tests` (2026-10-14): in a cell the
/// gateway's lease names the caller. A create over an existing document is
/// refused, and a comment is signed by the lease's principal (the graph's
/// owner as "Owner"), whatever `author` the caller sends.
#[test]
fn a_hosted_cell_refuses_a_takeover_and_signs_comments_by_the_lease() {
    let _serial = crate::tauri_runtime::profile_env_serial()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let root = std::env::temp_dir().join(format!("garden-cell-integrity-{}", uuid::Uuid::new_v4()));
    let profile = root.join("profile");
    std::fs::create_dir_all(&profile).unwrap();
    std::env::set_var("GARDEN_PROFILE_DIR", &profile);
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        crate::app_runtime::async_runtime::block_on(async {
            let app = crate::tauri_runtime::build_mock_cell_app_for_tests(GRAPH, OWNER, 1);
            crate::graph_service::create_graph_service(
                &app,
                crate::graph_service::CreateGraphInput {
                    graph_id: Some(GRAPH.into()),
                    title: "Integrity fixture".into(),
                    description: None,
                    operation_id: None,
                },
            )
            .unwrap();
            drop(app);
            let state = state_for(&profile);
            const VISITOR: &str = "user:a-visitor-20261014";

            let (_, reply) = send(
                &state,
                "/mcp",
                tool_call(
                    "create_document",
                    json!({"graphId": GRAPH, "documentId": "the-guide", "title": "The Guide"}),
                ),
            )
            .await;
            assert!(reply["error"].is_null(), "{reply}");
            let (_, reply) = send_as(
                &state,
                VISITOR,
                "/mcp",
                tool_call(
                    "create_document",
                    json!({"graphId": GRAPH, "documentId": "the-guide", "title": "HIJACKED"}),
                ),
            )
            .await;
            assert!(
                reply["error"]["message"]
                    .as_str()
                    .is_some_and(|message| message.contains("already exists")),
                "{reply}"
            );

            let (_, reply) = send(
                &state,
                "/mcp",
                tool_call(
                    "write_document",
                    json!({"graphId": GRAPH, "documentId": "the-guide", "content": "Hello world."}),
                ),
            )
            .await;
            assert!(reply["error"].is_null(), "{reply}");
            for (sub, comment_id, author) in
                [(VISITOR, "c-visitor", VISITOR), (OWNER, "c-owner", "Owner")]
            {
                let (_, reply) = send_as(&state, sub, "/mcp", tool_call("edit_comment", json!({
                    "graphId": GRAPH, "documentId": "the-guide", "commentId": comment_id, "action": "set",
                    "text": "A comment", "author": "Vera (owner)", "authorId": "owner",
                }))).await;
                assert!(reply["error"].is_null(), "{reply}");
                let comment = &reply["result"]["structuredContent"]["comment"];
                assert_eq!(comment["authorId"], sub, "{reply}");
                assert_eq!(comment["author"], author, "{reply}");
                assert_eq!(comment["claimedAuthor"], "Vera (owner)", "{reply}");
            }
        })
    }));
    std::env::remove_var("GARDEN_PROFILE_DIR");
    let _ = std::fs::remove_dir_all(&root);
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}
