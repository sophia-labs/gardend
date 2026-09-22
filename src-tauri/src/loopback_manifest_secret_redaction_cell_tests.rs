//! SPEC-A acceptance case 2: with the single-graph cell boundary ENABLED
//! (a real signed `x-sophia-cell-lease` header carrying `role: viewer`),
//! `GET /manifest` must still be secret-free, and a write attempt using a
//! named read-only client token must still be denied. Drives the REAL axum
//! router (`loopback_router::loopback_router`) via `tower::ServiceExt::oneshot`
//! — same technique as `owned_restore_tests.rs` — rather than calling the
//! handler function directly, so the cell-boundary middleware is exercised
//! too, not just the handler body.
use super::*;
use crate::cell_graph_boundary::CellGraphBoundary;
use crate::loopback_client_token_service::create_client_token;
use crate::loopback_client_token_types::CreateLoopbackClientTokenInput;
use crate::loopback_state::{LoopbackManifest, LoopbackState};
use axum::{body::Body, http::Request};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use hmac::{Hmac, Mac};
use serde_json::{json, Value};
use sha2::Sha256;
use std::sync::Arc;
use tower::ServiceExt;

const GRAPH: &str = "manifest-redaction-cell-fixture-20260922";
const OWNER: &str = "user:manifest-redaction-cell-owner-20260922";
const SECRET: &[u8] = b"manifest-redaction-cell-test-secret-32-bytes-min";
const MASTER_TOKEN: &str = "manifest-redaction-master-secret-DO-NOT-LEAK-20260922";

fn lease_for(role: &str) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let claims = json!({
        "iss": "pn-gateway", "aud": "gardend-cell", "sub": OWNER, "owner": OWNER,
        "graphId": GRAPH, "generation": 1u64, "cellId": bound_cell_id(OWNER, GRAPH, 1),
        "role": role, "policyRevision": 1u64, "registryRevision": 1u64,
        "sessionId": "manifest-redaction-cell-test", "iat": now, "exp": now + 600,
    });
    let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"HS256","typ":"JWT"}"#);
    let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap());
    let signed = format!("{header}.{payload}");
    let mut mac = Hmac::<Sha256>::new_from_slice(SECRET).unwrap();
    mac.update(signed.as_bytes());
    format!("{signed}.{}", URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes()))
}

fn state(profile: &std::path::Path) -> Arc<LoopbackState> {
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
    let manifest = LoopbackManifest {
        runtime_profile: "test",
        bind_host: "127.0.0.1",
        port: 0,
        api_url: "http://127.0.0.1:0".into(),
        mcp_url: "http://127.0.0.1:0/mcp".into(),
        openapi_url: "http://127.0.0.1:0/openapi.json".into(),
        token: MASTER_TOKEN.into(),
        pid: 4242,
        started_at: "2026-09-22T00:00:00.000Z".into(),
        manifest_path: profile.join("loopback.json").to_string_lossy().into_owned(),
        auth_header: "Authorization: Bearer <token>",
        token_audience: "tauri-runtime-compatibility",
        token_storage: "plaintext-owner-only-profile-manifest",
        security_warning: "synthetic test manifest",
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
        token: MASTER_TOKEN.into(),
        manifest,
        jobs,
        services: Arc::new(crate::local_service_host::LocalServiceHost::default()),
        lifecycle: Arc::new(crate::cell_lifecycle::CellLifecycle::new()),
        cell_graph: boundary,
    })
}

async fn call(
    state: Arc<LoopbackState>,
    method: &str,
    path: &str,
    bearer: &str,
    lease: &str,
    body: Body,
) -> (axum::http::StatusCode, Vec<u8>) {
    let request = Request::builder()
        .method(method)
        .uri(path)
        .header("authorization", format!("Bearer {bearer}"))
        .header("x-sophia-cell-lease", lease)
        .header("content-type", "application/json")
        .body(body)
        .unwrap();
    let response = crate::loopback_router::loopback_router(state)
        .oneshot(request)
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1_000_000)
        .await
        .unwrap()
        .to_vec();
    (status, bytes)
}

#[test]
fn manifest_is_secret_free_under_viewer_role_cell_boundary_and_write_still_denied() {
    let _serial = crate::tauri_runtime::profile_env_serial()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let profile = std::env::temp_dir().join(format!(
        "garden-manifest-redaction-cell-{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&profile).unwrap();
    std::env::set_var("GARDEN_PROFILE_DIR", &profile);
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        crate::app_runtime::async_runtime::block_on(async {
            let state = state(&profile);

            // Seed a REAL named read-only client token through the actual
            // production issuance path (the same function the Tauri
            // `create_loopback_client_token` command calls) — not a
            // hand-rolled fixture.
            let issued = create_client_token(
                &state.app,
                CreateLoopbackClientTokenInput {
                    label: Some("cell-boundary-viewer-fixture".into()),
                    grant_profile_id: Some("read-only".into()),
                    scopes: None,
                    expires_in_days: None,
                },
            )
            .unwrap();
            assert!(
                issued
                    .record
                    .scopes
                    .iter()
                    .any(|scope| scope == "loopback.manifest.read"),
                "read-only grant profile must include loopback.manifest.read: {:?}",
                issued.record.scopes
            );
            let read_only_token = issued.token;

            let viewer = lease_for("viewer");

            // 1. GET /manifest with the read-only token, under an enabled
            //    cell boundary with a Viewer-role signed lease: 200, and the
            //    body must be entirely secret-free.
            let (status, body) = call(
                state.clone(),
                "GET",
                "/manifest",
                &read_only_token,
                &viewer,
                Body::empty(),
            )
            .await;
            assert_eq!(status, axum::http::StatusCode::OK, "{}", String::from_utf8_lossy(&body));
            let text = String::from_utf8_lossy(&body).into_owned();
            assert!(
                !text.contains(MASTER_TOKEN),
                "viewer-role manifest response leaked the master bearer: {text}"
            );
            let value: Value = serde_json::from_slice(&body).unwrap();
            let object = value.as_object().unwrap();
            for secret_field in ["token", "pid", "manifestPath"] {
                assert!(
                    !object.contains_key(secret_field),
                    "viewer-role manifest response must not carry {secret_field}: {text}"
                );
            }
            assert_eq!(value["tokenScopeMode"], "session-all");
            assert_eq!(value["cellGraphId"], GRAPH);

            // 2. A write attempt with the SAME read-only token, still under
            //    the Viewer-role lease, must still be denied (cell-role
            //    minimum and/or loopback-scope enforcement — either way, the
            //    manifest redaction fix must not have loosened this).
            let (write_status, write_body) = call(
                state.clone(),
                "POST",
                &format!("/api/graphs/{GRAPH}/folders"),
                &read_only_token,
                &viewer,
                Body::from(
                    serde_json::to_vec(&json!({
                        "folderId": "manifest-redaction-cell-folder",
                        "name": "should not be created",
                        "parentId": Value::Null,
                        "section": "documents",
                        "order": 1,
                    }))
                    .unwrap(),
                ),
            )
            .await;
            assert_eq!(
                write_status,
                axum::http::StatusCode::FORBIDDEN,
                "read-only token under viewer lease must not be able to write: {}",
                String::from_utf8_lossy(&write_body)
            );
        });
    }));
    std::env::remove_var("GARDEN_PROFILE_DIR");
    let _ = std::fs::remove_dir_all(&profile);
    if let Err(error) = outcome {
        std::panic::resume_unwind(error);
    }
}
