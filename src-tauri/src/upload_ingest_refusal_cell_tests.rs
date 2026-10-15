//! The hosted cell's side of the parser-pool cases in
//! `crdt_engine::workspace_ops_poison_tests`: through the real router, with a
//! gateway lease, a stored file the parser pool will never read is imported
//! (the files canvas's `/artifacts/{g}/{a}/import`, and MCP `ingest_artifact`).
//! The import's job fails at once with the pool's reason, the file's row says
//! `status: "error"` with that reason in `errorMessage` (navigation, and MCP
//! `get_workspace`), and a later import that parses puts it back to "ready".
//! Before the fix the job ran until the caller's timeout while the operation
//! was retried forever, and the row said nothing (2026-10-14).
use super::*;
use crate::crdt_engine::workspace_ops_poison_tests::{pool_answers, ParserPoolEnv, RecordedPool};
use axum::{body::Body, http::Request};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use hmac::{Hmac, Mac};
use serde_json::{json, Value};
use tower::ServiceExt;

const GRAPH: &str = "upload-refusal-cell-fixture-20261014";
const OWNER: &str = "user:upload-refusal-cell-owner-20261014";
const SECRET: &[u8] = b"upload-refusal-cell-test-secret-at-least-32-bytes";
const TOKEN: &str = "upload-refusal-cell-disposable-token";
const BOUNDARY: &str = "upload-refusal-boundary-5c1e";
const ANSWER_WITHIN: std::time::Duration = std::time::Duration::from_secs(10);

fn lease() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let claims = json!({"iss":"pn-gateway","aud":"gardend-cell","sub":OWNER,"owner":OWNER,
        "graphId":GRAPH,"generation":1,"cellId":bound_cell_id(OWNER,GRAPH,1),"role":"editor",
        "policyRevision":1,"registryRevision":1,"sessionId":"upload-refusal-test","iat":now,"exp":now+600});
    let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"HS256","typ":"JWT"}"#);
    let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap());
    let signed = format!("{header}.{payload}");
    let mut mac = Hmac::<Sha256>::new_from_slice(SECRET).unwrap();
    mac.update(signed.as_bytes());
    format!("{signed}.{}", URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes()))
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

/// One request through the real router, bounded.
async fn call(
    state: &Arc<LoopbackState>,
    method: &str,
    path: &str,
    content_type: Option<&str>,
    body: Vec<u8>,
) -> (StatusCode, Value) {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header("authorization", format!("Bearer {TOKEN}"))
        .header("x-sophia-cell-lease", lease());
    if let Some(content_type) = content_type {
        request = request.header("content-type", content_type);
    }
    let response = tokio::time::timeout(
        ANSWER_WITHIN,
        crate::loopback_router::loopback_router(state.clone())
            .oneshot(request.body(Body::from(body)).unwrap()),
    )
    .await
    .unwrap_or_else(|_| panic!("{method} {path} did not answer within {ANSWER_WITHIN:?}"))
    .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 16 * 1024 * 1024).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

async fn post_json(state: &Arc<LoopbackState>, path: &str, body: Value) -> (StatusCode, Value) {
    call(state, "POST", path, Some("application/json"), serde_json::to_vec(&body).unwrap()).await
}

async fn mcp(state: &Arc<LoopbackState>, tool: &str, arguments: Value) -> Value {
    let (status, reply) = post_json(
        state,
        "/mcp",
        json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": {"name": tool, "arguments": arguments}}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{reply}");
    assert!(reply["error"].is_null(), "{tool}: {reply}");
    reply["result"]["structuredContent"].clone()
}

async fn upload_pdf(state: &Arc<LoopbackState>) -> String {
    let pdf = b"%PDF-1.4\n1 0 obj << /Type /Catalog /Pages 2 0 R >> endobj\n2 0 obj << /Type /Pages /Kids [] /Count 0 >> endobj\ntrailer << /Root 1 0 R >>\n%%EOF\n";
    let mut body = Vec::new();
    body.extend_from_slice(
        format!(
            "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"specimen.pdf\"\r\nContent-Type: application/pdf\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(pdf);
    body.extend_from_slice(format!("\r\n--{BOUNDARY}--\r\n").as_bytes());
    let (status, created) = call(
        state,
        "POST",
        &format!("/artifacts/{GRAPH}/files"),
        Some(&format!("multipart/form-data; boundary={BOUNDARY}")),
        body,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    created["id"].as_str().unwrap().to_string()
}

/// Wait (bounded) for a job to finish, through the route the files canvas
/// polls.
async fn finished_job(state: &Arc<LoopbackState>, job_id: &str) -> Value {
    let poll = async {
        loop {
            let (status, job) = call(state, "GET", &format!("/graphs/jobs/{job_id}"), None, Vec::new()).await;
            assert_eq!(status, StatusCode::OK, "{job}");
            if matches!(job["status"].as_str(), Some("succeeded" | "failed" | "cancelled")) {
                return job;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    };
    tokio::time::timeout(ANSWER_WITHIN, poll)
        .await
        .unwrap_or_else(|_| panic!("job {job_id} did not finish within {ANSWER_WITHIN:?}: the ingest is parked for retry"))
}

async fn import(state: &Arc<LoopbackState>, artifact_id: &str) -> Value {
    let (status, submitted) = post_json(
        state,
        &format!("/artifacts/{GRAPH}/{artifact_id}/import"),
        json!({"readOnly": true}),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{submitted}");
    let job_id = submitted["jobId"].as_str().or_else(|| submitted["job_id"].as_str()).unwrap().to_string();
    finished_job(state, &job_id).await
}

async fn artifact_row(state: &Arc<LoopbackState>, artifact_id: &str) -> Value {
    let (status, navigation) = call(state, "GET", &format!("/navigation/{GRAPH}"), None, Vec::new()).await;
    assert_eq!(status, StatusCode::OK, "{navigation}");
    navigation["artifacts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|artifact| artifact["id"] == artifact_id)
        .cloned()
        .unwrap_or_else(|| panic!("{artifact_id} is not listed: {navigation}"))
}

#[test]
fn an_import_the_parser_pool_refuses_fails_at_once_and_the_file_says_why() {
    let _serial = crate::tauri_runtime::profile_env_serial()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let root = std::env::temp_dir().join(format!("garden-upload-refusal-{}", uuid::Uuid::new_v4()));
    let profile = root.join("profile");
    std::fs::create_dir_all(&profile).unwrap();
    std::env::set_var("GARDEN_PROFILE_DIR", &profile);
    let pool = RecordedPool::answering(pool_answers::NO_MARKDOWN);
    let env = ParserPoolEnv::set(&pool.url);
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let app = crate::tauri_runtime::build_mock_cell_app_for_tests(GRAPH, OWNER, 1);
                crate::graph_service::create_graph_service(
                    &app,
                    crate::graph_service::CreateGraphInput {
                        graph_id: Some(GRAPH.into()),
                        title: "Upload refusal fixture".into(),
                        description: None,
                        operation_id: None,
                    },
                )
                .unwrap();
                drop(app);
                let state = state_for(&profile);
                let artifact_id = upload_pdf(&state).await;

                // The files canvas's import: the job fails at once with the
                // pool's reason, and the file says so.
                let job = import(&state, &artifact_id).await;
                assert_eq!(job["status"], "failed", "{job}");
                assert!(
                    job["error"].as_str().is_some_and(|error| error.contains("all parse approaches failed")),
                    "{job}"
                );
                assert_eq!(pool.requests(), 1, "asked once, not retried");
                let row = artifact_row(&state, &artifact_id).await;
                assert_eq!(row["status"], "error", "{row}");
                assert!(
                    row["errorMessage"].as_str().is_some_and(|message| message.contains("all parse approaches failed")),
                    "{row}"
                );

                // The same through MCP: ingest_artifact's job, and the row in
                // get_workspace.
                pool.answer(pool_answers::NOT_FOUND);
                let submitted = mcp(&state, "ingest_artifact", json!({"graphId": GRAPH, "artifact_id": artifact_id})).await;
                let job_id = submitted["jobId"].as_str().unwrap().to_string();
                let job = finished_job(&state, &job_id).await;
                assert_eq!(job["status"], "failed", "{job}");
                assert!(job["error"].as_str().is_some_and(|error| error.contains("HTTP 404")), "{job}");
                let workspace = mcp(&state, "get_workspace", json!({"graphId": GRAPH})).await;
                let listed = workspace["artifacts"]
                    .as_array()
                    .and_then(|artifacts| artifacts.iter().find(|artifact| artifact["id"] == artifact_id))
                    .cloned()
                    .unwrap_or_else(|| panic!("{artifact_id} not in get_workspace: {workspace}"));
                assert_eq!(listed["status"], "error", "{listed}");
                assert!(
                    listed["errorMessage"].as_str().is_some_and(|message| message.contains("HTTP 404")),
                    "{listed}"
                );

                // Once the pool reads it, the import lands and the error is gone.
                pool.answer(pool_answers::PARSED);
                let job = import(&state, &artifact_id).await;
                assert_eq!(job["status"], "succeeded", "{job}");
                let row = artifact_row(&state, &artifact_id).await;
                assert_eq!(row["status"], "ready", "{row}");
                assert!(row["errorMessage"].is_null(), "{row}");
            })
    }));
    drop(env);
    std::env::remove_var("GARDEN_PROFILE_DIR");
    let _ = std::fs::remove_dir_all(&root);
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}
