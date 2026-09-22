//! Normalizes the gateway-forwarded `x-sophia-capture-identity` header into a
//! `CaptureIdentity`, available to every downstream loopback handler via
//! request extensions — the cell half of Phase 1 of the Observatory Capture
//! spine (see plans/observatory-capture-phase1-spine-spec-20260718.md).
//!
//! TRUST BOUNDARY (read before touching this file): the header is
//! attacker-controlled input on any request that hasn't presented the
//! cell's own single per-run MASTER credential. `resolve_forwarded_identity`
//! requires `loopback_http::loopback_master_token_presented` — deliberately
//! narrower than "any loopback token this cell recognizes" — before it will
//! even look at the header:
//!
//! - A *scoped* client token (`loopback_client_tokens`) is a valid loopback
//!   credential for its own scopes, but is a separate, more widely
//!   distributable credential than the gateway's own secret. It must never
//!   be sufficient to assert an Observatory identity on someone else's
//!   behalf — see `a_valid_scoped_client_token_cannot_assert_identity` below.
//! - The master token is recognized over BOTH the ordinary
//!   `Authorization: Bearer <token>` header AND the
//!   `Sec-WebSocket-Protocol: bearer.<token>` subprotocol used on WebSocket
//!   upgrades (`loopback_hocuspocus_routes`'s auth already accepts both — a
//!   gateway/ALB in front of this cell can strip ordinary headers on the WS
//!   upgrade, so browser-origin proxied WS traffic authenticates via the
//!   subprotocol form only). A check keyed on `Authorization` alone would
//!   silently never install identity on any proxied WS interaction — see
//!   `router_attaches_identity_on_a_websocket_upgrade_via_the_subprotocol`.
//!
//! On any other path the header is never even parsed, let alone trusted.

use crate::{
    capture_event::CaptureIdentity, loopback_http::loopback_master_token_presented,
    loopback_state::LoopbackState,
};
use axum::{
    body::Body,
    extract::{Request, State},
    http::HeaderMap,
    middleware::Next,
    response::Response,
};
use std::sync::Arc;

pub(crate) const CAPTURE_IDENTITY_HEADER: &str = "x-sophia-capture-identity";

/// Axum middleware: on the same loopback surface every route already
/// authenticates against, additionally normalize any forwarded capture
/// identity into request extensions.
///
/// This does NOT itself authorize or reject a request — each handler's own
/// `require_loopback_scope`/`authorized_loopback_scopes` call remains the
/// sole gate on whether the request proceeds at all. It only makes an
/// `Option<Extension<CaptureIdentity>>` available when a forwarded identity
/// is both present and trustworthy, so Phase 2's scattered `emit_interaction`
/// call sites read identity out of handler scope with zero per-site
/// plumbing (extract `Option<Extension<CaptureIdentity>>` like any other
/// axum extension).
pub(crate) async fn capture_identity_middleware(
    State(state): State<Arc<LoopbackState>>,
    mut request: Request<Body>,
    next: Next,
) -> Response {
    if let Some(identity) = resolve_forwarded_identity(request.headers(), &state) {
        request.extensions_mut().insert(identity);
    }
    next.run(request).await
}

/// The trust-boundary decision, factored out of the middleware so it is
/// exercised directly in tests without needing axum's internal `Next`
/// machinery.
///
/// Order matters for cost, not correctness: the overwhelming majority of
/// requests (anything not proxied through the gateway) carry no forwarded
/// identity header at all, so the cheap header-presence check runs first and
/// the master-token check — which reads request headers/subprotocol but no
/// per-request allocation — only runs when there's actually something to
/// trust or reject.
fn resolve_forwarded_identity(
    headers: &HeaderMap,
    state: &LoopbackState,
) -> Option<CaptureIdentity> {
    let raw = headers.get(CAPTURE_IDENTITY_HEADER)?.to_str().ok()?;
    if !loopback_master_token_presented(headers, state) {
        return None;
    }
    match CaptureIdentity::from_forwarded_header(raw) {
        Ok(identity) => Some(identity),
        Err(error) => {
            // A malformed forwarded identity is a gateway-side bug, not a
            // reason to fail the request: ignore it exactly like "absent" —
            // Phase 2 handlers already treat missing identity as a no-emit
            // condition. Only visible on stderr (the ordinary `log` lane),
            // never in the positive-gated NDJSON testimony.
            log::debug!("ignoring malformed x-sophia-capture-identity: {error}");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        cell_graph_boundary::CellGraphBoundary,
        cell_lifecycle::CellLifecycle,
        local_jobs::LocalJobRegistry,
        local_service_host::LocalServiceHost,
        loopback_client_token_service::{create_client_token, resolve_loopback_client_token},
        loopback_client_token_types::CreateLoopbackClientTokenInput,
        loopback_state::LoopbackManifest,
        loopback_token_grants::session_all_token_grant,
    };
    use axum::{
        body::Body,
        extract::ws::WebSocketUpgrade,
        http::{header, HeaderValue, Request as HttpRequest, StatusCode},
        middleware,
        response::IntoResponse,
        routing::get,
        Extension, Router,
    };
    use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _};
    use tower::ServiceExt;
    use uuid::Uuid;

    const TEST_TOKEN: &str = "test-master-loopback-token";

    fn test_state() -> Arc<LoopbackState> {
        let app = crate::tauri_runtime::build_mock_app_for_tests(false);
        let jobs_dir =
            std::env::temp_dir().join(format!("capture-identity-jobs-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&jobs_dir).expect("create test jobs dir");
        let jobs = Arc::new(LocalJobRegistry::new(jobs_dir).expect("build test job registry"));
        let grant = session_all_token_grant();
        let manifest = LoopbackManifest {
            runtime_profile: "test",
            bind_host: "127.0.0.1",
            port: 0,
            api_url: "http://127.0.0.1:0".to_string(),
            mcp_url: "http://127.0.0.1:0/mcp".to_string(),
            openapi_url: "http://127.0.0.1:0/openapi.json".to_string(),
            token: TEST_TOKEN.to_string(),
            pid: 0,
            started_at: "1970-01-01T00:00:00Z".to_string(),
            manifest_path: "test".to_string(),
            auth_header: "Authorization: Bearer <token>",
            token_audience: "test",
            token_storage: "test",
            security_warning: "test",
            cell_graph_id: None,
            cell_owner: None,
            cell_generation: None,
            cell_registry_revision: None,
            capabilities: grant.scopes.clone(),
            token_scope_mode: grant.scope_mode,
            token_scopes: grant.scopes,
            scope_details: grant.scope_details,
            grant_profiles: grant.grant_profiles,
        };
        Arc::new(LoopbackState {
            app,
            token: TEST_TOKEN.to_string(),
            manifest,
            jobs,
            services: Arc::new(LocalServiceHost::default()),
            lifecycle: Arc::new(CellLifecycle::new()),
            cell_graph: Arc::new(CellGraphBoundary::for_test(None)),
        })
    }

    fn forwarded_identity_header(
        principal: &str,
        client_class: &str,
        on_behalf_of: Option<&str>,
    ) -> String {
        let mut json = serde_json::json!({
            "principal": principal,
            "client_class": client_class,
        });
        if let Some(subject) = on_behalf_of {
            json["on_behalf_of"] = serde_json::Value::String(subject.to_string());
        }
        BASE64_STANDARD.encode(serde_json::to_vec(&json).unwrap())
    }

    #[test]
    fn trusted_path_normalizes_the_forwarded_identity() {
        let state = test_state();
        let header_value =
            forwarded_identity_header("service:choreograph", "agent", Some("user-42"));
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            format!("Bearer {TEST_TOKEN}").parse().unwrap(),
        );
        headers.insert(CAPTURE_IDENTITY_HEADER, header_value.parse().unwrap());

        let identity = resolve_forwarded_identity(&headers, &state)
            .expect("identity normalized on trusted path");
        assert_eq!(identity.principal(), "service:choreograph");
        assert_eq!(identity.client_class().as_str(), "agent");
        assert_eq!(identity.on_behalf_of(), Some("user-42"));
    }

    #[test]
    fn capture_identity_is_ignored_without_a_valid_loopback_token() {
        let state = test_state();
        // A spoofed identity header, but NO bearer token at all — the
        // untrusted path. If this ever normalized, an attacker on the
        // loopback surface could impersonate any principal in Observatory
        // testimony.
        let header_value = forwarded_identity_header("service:attacker", "service", None);
        let mut headers = HeaderMap::new();
        headers.insert(CAPTURE_IDENTITY_HEADER, header_value.parse().unwrap());

        assert!(resolve_forwarded_identity(&headers, &state).is_none());
    }

    #[test]
    fn capture_identity_is_ignored_with_a_wrong_bearer_token() {
        let state = test_state();
        let header_value = forwarded_identity_header("service:attacker", "service", None);
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            "Bearer not-the-real-token".parse().unwrap(),
        );
        headers.insert(CAPTURE_IDENTITY_HEADER, header_value.parse().unwrap());

        assert!(resolve_forwarded_identity(&headers, &state).is_none());
    }

    /// `GARDEN_PROFILE_DIR` is process-global; serialize with every other
    /// headless test module that touches it (established pattern — see
    /// `document_meaningful_object`'s `env_serial`/`temp_profile`).
    fn env_serial() -> &'static std::sync::Mutex<()> {
        crate::tauri_runtime::profile_env_serial()
    }

    fn temp_profile_dir() -> std::path::PathBuf {
        std::env::temp_dir().join(format!("capture-identity-profile-{}", Uuid::new_v4()))
    }

    #[test]
    fn a_valid_scoped_client_token_cannot_assert_identity_only_the_master_token_can() {
        let _guard = env_serial()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let profile_dir = temp_profile_dir();
        std::env::set_var("GARDEN_PROFILE_DIR", &profile_dir);

        let state = test_state();
        let scoped_token = create_client_token(
            &state.app,
            CreateLoopbackClientTokenInput {
                label: None,
                grant_profile_id: None,
                scopes: Some(vec!["rdf.query".to_string()]),
                expires_in_days: None,
            },
        )
        .expect("create scoped client token")
        .token;

        // Sanity check this is a REAL, resolvable loopback credential — not
        // a broken token that would fail every check regardless. The point
        // of this test is that a genuinely valid, narrower-scoped
        // credential still must not be sufficient to assert identity.
        assert!(
            resolve_loopback_client_token(&state.app, &scoped_token)
                .expect("resolve scoped token")
                .is_some(),
            "the scoped token must be a valid loopback credential in general"
        );

        let header_value =
            forwarded_identity_header("service:attacker-via-scoped-token", "service", None);
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            format!("Bearer {scoped_token}").parse().unwrap(),
        );
        headers.insert(CAPTURE_IDENTITY_HEADER, header_value.parse().unwrap());
        assert!(
            resolve_forwarded_identity(&headers, &state).is_none(),
            "a scoped client token must never be able to assert an Observatory identity"
        );

        // The master token, by contrast, IS trusted — same header, same
        // state, only the credential differs.
        let mut master_headers = HeaderMap::new();
        master_headers.insert(
            header::AUTHORIZATION,
            format!("Bearer {TEST_TOKEN}").parse().unwrap(),
        );
        master_headers.insert(CAPTURE_IDENTITY_HEADER, header_value.parse().unwrap());
        assert!(resolve_forwarded_identity(&master_headers, &state).is_some());

        std::env::remove_var("GARDEN_PROFILE_DIR");
    }

    #[test]
    fn trusted_path_without_the_header_yields_no_identity() {
        let state = test_state();
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            format!("Bearer {TEST_TOKEN}").parse().unwrap(),
        );
        assert!(resolve_forwarded_identity(&headers, &state).is_none());
    }

    #[test]
    fn trusted_path_with_a_malformed_header_yields_no_identity_not_an_error() {
        let state = test_state();
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            format!("Bearer {TEST_TOKEN}").parse().unwrap(),
        );
        headers.insert(
            CAPTURE_IDENTITY_HEADER,
            "not-base64-json!!".parse().unwrap(),
        );
        assert!(resolve_forwarded_identity(&headers, &state).is_none());
    }

    /// End-to-end proof through a real axum `Router` (not just the pure
    /// function): the middleware is wired so identity actually reaches
    /// handler scope on the trusted path, and never reaches it otherwise —
    /// via a real HTTP request/response round trip, no hand-built `Next`.
    async fn probe_router(state: Arc<LoopbackState>) -> Router {
        // `Option<Extension<T>>` is axum's built-in "present or not" extractor
        // — it returns `None` when the middleware never inserted `T`, rather
        // than erroring. This mirrors exactly what a Phase 2 handler will do:
        // no separate default-extension layer is needed.
        Router::new()
            .route(
                "/probe",
                get(|identity: Option<Extension<CaptureIdentity>>| async move {
                    match identity {
                        Some(Extension(identity)) => {
                            identity.principal().to_string().into_response()
                        }
                        None => StatusCode::NO_CONTENT.into_response(),
                    }
                }),
            )
            .layer(middleware::from_fn_with_state(
                state.clone(),
                capture_identity_middleware,
            ))
            .with_state(state)
    }

    #[tokio::test]
    async fn router_attaches_identity_only_on_the_trusted_path() {
        let state = test_state();
        let header_value = forwarded_identity_header("service:choreograph", "agent", None);

        let trusted = HttpRequest::builder()
            .uri("/probe")
            .header(header::AUTHORIZATION, format!("Bearer {TEST_TOKEN}"))
            .header(CAPTURE_IDENTITY_HEADER, &header_value)
            .body(Body::empty())
            .unwrap();
        let response = probe_router(state.clone())
            .await
            .oneshot(trusted)
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(&body[..], b"service:choreograph");

        let untrusted = HttpRequest::builder()
            .uri("/probe")
            .header(CAPTURE_IDENTITY_HEADER, &header_value)
            .body(Body::empty())
            .unwrap();
        let response = probe_router(state).await.oneshot(untrusted).await.unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
    }

    /// A WS route wired exactly like `loopback_hocuspocus_routes`'s real
    /// handlers: `Option<Extension<CaptureIdentity>>` alongside
    /// `WebSocketUpgrade`. Stashes the resolved principal (or the sentinel
    /// `"MISSING"`) in a response header the test can read — the upgrade
    /// response is generated synchronously by `on_upgrade`, so this doesn't
    /// need a real byte-level WS handshake to prove whether identity was
    /// installed before the handler ran.
    async fn ws_probe(
        identity: Option<Extension<CaptureIdentity>>,
        ws: WebSocketUpgrade,
    ) -> Response {
        let principal = identity
            .as_ref()
            .map(|Extension(identity)| identity.principal().to_string())
            .unwrap_or_else(|| "MISSING".to_string());
        let mut response = ws.on_upgrade(|_socket| async {});
        response.headers_mut().insert(
            "x-test-principal",
            HeaderValue::from_str(&principal).unwrap(),
        );
        response
    }

    async fn ws_probe_router(state: Arc<LoopbackState>) -> Router {
        Router::new()
            .route("/ws-probe", get(ws_probe))
            .layer(middleware::from_fn_with_state(
                state.clone(),
                capture_identity_middleware,
            ))
            .with_state(state)
    }

    /// Minimal parsed HTTP/1.1 response head (status line + headers, no
    /// body) read off a raw socket.
    struct RawResponse {
        status_line: String,
        headers: Vec<(String, String)>,
    }

    impl RawResponse {
        fn header(&self, name: &str) -> Option<&str> {
            self.headers
                .iter()
                .find(|(key, _)| key.eq_ignore_ascii_case(name))
                .map(|(_, value)| value.as_str())
        }
    }

    /// Sends a real WS upgrade request over a real TCP socket and reads the
    /// real response head back.
    ///
    /// `axum::extract::ws::WebSocketUpgrade` requires a genuine
    /// `hyper::upgrade::OnUpgrade` in the request's extensions — populated
    /// only by hyper's real connection-serving machinery — so a
    /// `Router::oneshot()` call on a hand-built `http::Request` (fine for
    /// every other test in this file) rejects with 426 before the handler
    /// even runs. This is the reason a real listener is unavoidable for
    /// this one test; axum's own extractor test suite does the same
    /// (`extract::ws::tests`, `spawn_service` + a real `TcpStream`).
    async fn ws_upgrade_over_real_socket(
        addr: std::net::SocketAddr,
        subprotocol: &str,
        identity_header: &str,
    ) -> RawResponse {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut stream = tokio::net::TcpStream::connect(addr)
            .await
            .expect("connect to test WS server");
        let request = format!(
            "GET /ws-probe HTTP/1.1\r\n\
             Host: 127.0.0.1\r\n\
             Connection: Upgrade\r\n\
             Upgrade: websocket\r\n\
             Sec-WebSocket-Version: 13\r\n\
             Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
             Sec-WebSocket-Protocol: {subprotocol}\r\n\
             x-sophia-capture-identity: {identity_header}\r\n\
             \r\n"
        );
        stream
            .write_all(request.as_bytes())
            .await
            .expect("write WS upgrade request");

        let mut buf = Vec::new();
        let mut chunk = [0u8; 1024];
        loop {
            let n = stream.read(&mut chunk).await.expect("read WS response");
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
            if buf.windows(4).any(|window| window == b"\r\n\r\n") {
                break;
            }
        }
        let text = String::from_utf8_lossy(&buf);
        let mut lines = text.split("\r\n");
        let status_line = lines.next().unwrap_or_default().to_string();
        let headers = lines
            .take_while(|line| !line.is_empty())
            .filter_map(|line| {
                let (name, value) = line.split_once(':')?;
                Some((name.trim().to_string(), value.trim().to_string()))
            })
            .collect();
        RawResponse {
            status_line,
            headers,
        }
    }

    /// Reproduces the exact defect the joint refute flagged: the gateway
    /// authenticates a proxied WS upgrade via the `Sec-WebSocket-Protocol:
    /// bearer.<token>` subprotocol (no `Authorization` header at all — ALBs
    /// strip it on upgrade requests), yet the identity gate used to check
    /// only `Authorization`, so the identity was silently never installed
    /// even though `loopback_hocuspocus_routes::authorize_ws` accepts this
    /// exact form and the WS connection proceeds unattributed.
    #[tokio::test]
    async fn router_attaches_identity_on_a_websocket_upgrade_via_the_subprotocol() {
        let state = test_state();
        let header_value = forwarded_identity_header("service:choreograph", "agent", None);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test WS listener");
        let addr = listener.local_addr().expect("test listener addr");
        let router = ws_probe_router(state).await;
        tokio::spawn(async move {
            axum::serve(listener, router)
                .await
                .expect("serve test router");
        });

        // Trusted: the subprotocol carries the real master token, and
        // deliberately NO `Authorization` header is sent at all.
        let response =
            ws_upgrade_over_real_socket(addr, &format!("bearer.{TEST_TOKEN}"), &header_value).await;
        assert_eq!(response.status_line, "HTTP/1.1 101 Switching Protocols");
        assert_eq!(
            response.header("x-test-principal"),
            Some("service:choreograph")
        );

        // Same well-formed WS upgrade shape, but the subprotocol token is
        // wrong — identity must not be installed even though the upgrade
        // itself still succeeds (auth for the WS connection itself is a
        // separate concern from this middleware, mirroring how
        // `authorize_ws` and `capture_identity_middleware` are independent
        // gates in production).
        let response =
            ws_upgrade_over_real_socket(addr, "bearer.not-the-real-token", &header_value).await;
        assert_eq!(response.status_line, "HTTP/1.1 101 Switching Protocols");
        assert_eq!(response.header("x-test-principal"), Some("MISSING"));
    }
}
