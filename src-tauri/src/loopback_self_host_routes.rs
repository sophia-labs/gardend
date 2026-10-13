//! The self-host HTTP surface (see `loopback_self_host` for the knobs).
//!
//! Installed only while self-host is active, as the router's fallback, so it
//! never shadows an API route: a path the API answers is answered by the API,
//! exactly as before. What reaches the fallback:
//!
//! - `GET /self-host.json`: `{"kind":"gardend-self-host","version",
//!   "auth":"cloudflare-access"|"key","user":{"email"}|null}`, public; `user`
//!   is filled when a valid Access token for a person came with the request.
//! - the UI build (`GARDEN_UI_DIR`, or `ui/` next to the binary): a file that
//!   exists is served; a browser navigation (`Sec-Fetch-Mode: navigate`, or
//!   `Accept: text/html`) to a path with no file gets `index.html`; anything
//!   else gets the router's ordinary empty 404. Hashed build assets
//!   (`assets/<name>-<hash>.<ext>`) are `immutable`; everything else,
//!   `index.html` included, is `no-cache` with an `ETag`.
//!
//! [`access_signing_key_middleware`] runs ahead of every handler when Access
//! is configured, so the synchronous auth checks always find the signing key
//! a token names (fetched on first use and after Cloudflare rotates keys).

use crate::{
    loopback_self_host::{access_jwt, SelfHost},
    loopback_state::LoopbackState,
};
use axum::{
    body::Body,
    extract::{Request, State},
    http::{header, HeaderMap, HeaderValue, Method, StatusCode, Uri},
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::UNIX_EPOCH,
};

pub(crate) const SELF_HOST_JSON_PATH: &str = "/self-host.json";
const IMMUTABLE: &str = "public, max-age=31536000, immutable";
const NO_CACHE: &str = "no-cache";

/// Fetch the Access signing key a request's token names before any handler
/// verifies it (handlers verify synchronously against the cached keys).
pub(crate) async fn access_signing_key_middleware(
    State(state): State<Arc<LoopbackState>>,
    request: Request,
    next: Next,
) -> Response {
    if let Some(access) = state.self_host.access() {
        if let Some(jwt) = access_jwt(request.headers()).map(str::to_string) {
            access.ensure_signing_key(&jwt).await;
        }
    }
    next.run(request).await
}

/// The router fallback while self-host is active.
pub(crate) async fn self_host_fallback(
    State(state): State<Arc<LoopbackState>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
) -> Response {
    if method != Method::GET && method != Method::HEAD {
        return not_found();
    }
    if uri.path() == SELF_HOST_JSON_PATH {
        return self_host_json(&state.self_host, &headers).await;
    }
    match state.self_host.ui_dir() {
        Some(ui_dir) => serve_ui(ui_dir, uri.path(), &headers).await,
        None => not_found(),
    }
}

/// The router's own answer to an unmatched path: 404, empty body.
fn not_found() -> Response {
    StatusCode::NOT_FOUND.into_response()
}

async fn self_host_json(self_host: &SelfHost, headers: &HeaderMap) -> Response {
    let user = match (self_host.access(), access_jwt(headers)) {
        (Some(access), Some(jwt)) => {
            access.ensure_signing_key(jwt).await;
            access.verify(jwt).ok().and_then(|grant| {
                grant
                    .email()
                    .map(|email| serde_json::json!({ "email": email }))
            })
        }
        _ => None,
    };
    let mut response = Json(serde_json::json!({
        "kind": "gardend-self-host",
        "version": env!("CARGO_PKG_VERSION"),
        "auth": self_host.auth_mode(),
        "user": user,
    }))
    .into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

pub(crate) async fn serve_ui(ui_dir: &Path, path: &str, headers: &HeaderMap) -> Response {
    let Some(segments) = ui_relative_path(path) else {
        return not_found();
    };
    if !segments.is_empty() {
        let mut file = ui_dir.to_path_buf();
        file.extend(&segments);
        if let Some(response) = file_response(file, &segments.join("/"), headers).await {
            return response;
        }
        if !is_navigation(headers) {
            return not_found();
        }
    }
    file_response(ui_dir.join("index.html"), "index.html", headers)
        .await
        .unwrap_or_else(not_found)
}

fn is_navigation(headers: &HeaderMap) -> bool {
    let mode = headers
        .get("sec-fetch-mode")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    let accept = headers
        .get(header::ACCEPT)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    mode.eq_ignore_ascii_case("navigate") || accept.to_ascii_lowercase().contains("text/html")
}

/// The decoded path segments under the UI folder, or `None` for a path that
/// could leave it (`..`, dotfiles, separators or drive letters inside a
/// segment, bad percent-encoding).
fn ui_relative_path(path: &str) -> Option<Vec<String>> {
    let mut segments = Vec::new();
    for raw in path.split('/') {
        if raw.is_empty() {
            continue;
        }
        let segment = percent_decode(raw)?;
        if segment.starts_with('.') || segment.contains(['/', '\\', '\0', ':']) {
            return None;
        }
        segments.push(segment);
    }
    Some(segments)
}

fn percent_decode(input: &str) -> Option<String> {
    if !input.contains('%') {
        return Some(input.to_string());
    }
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let hex = input.get(index + 1..index + 3)?;
            if !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                return None;
            }
            out.push(u8::from_str_radix(hex, 16).ok()?);
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(out).ok()
}

async fn file_response(file: PathBuf, relative: &str, headers: &HeaderMap) -> Option<Response> {
    let (bytes, modified) = tokio::task::spawn_blocking(move || {
        let metadata = std::fs::metadata(&file).ok()?;
        if !metadata.is_file() {
            return None;
        }
        let modified = metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or_default();
        std::fs::read(&file).ok().map(|bytes| (bytes, modified))
    })
    .await
    .ok()??;
    let etag = format!("\"{:x}-{:x}\"", bytes.len(), modified);
    let cache_control = if is_hashed_asset(relative) {
        IMMUTABLE
    } else {
        NO_CACHE
    };
    let unchanged = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|tags| {
            tags.split(',')
                .map(str::trim)
                .any(|tag| tag == "*" || tag.strip_prefix("W/").unwrap_or(tag) == etag)
        });
    let mut response = if unchanged {
        StatusCode::NOT_MODIFIED.into_response()
    } else {
        let mut response = Response::new(Body::from(bytes));
        response.headers_mut().insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static(content_type_for(relative)),
        );
        response
    };
    let response_headers = response.headers_mut();
    response_headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(cache_control),
    );
    if let Ok(etag) = HeaderValue::from_str(&etag) {
        response_headers.insert(header::ETAG, etag);
    }
    response_headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    Some(response)
}

/// Vite's content-hashed output: `assets/<name>-<hash>.<ext>`, the hash at
/// least eight `[A-Za-z0-9_]` characters. A hash this misses is only served
/// `no-cache`, never stale.
fn is_hashed_asset(relative: &str) -> bool {
    let Some(rest) = relative.strip_prefix("assets/") else {
        return false;
    };
    let name = rest.rsplit('/').next().unwrap_or(rest);
    let Some((stem, _extension)) = name.rsplit_once('.') else {
        return false;
    };
    let Some((_, hash)) = stem.rsplit_once('-') else {
        return false;
    };
    hash.len() >= 8
        && hash
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

fn content_type_for(relative: &str) -> &'static str {
    let name = relative.rsplit('/').next().unwrap_or(relative);
    let extension = name
        .rsplit_once('.')
        .map(|(_, extension)| extension.to_ascii_lowercase())
        .unwrap_or_default();
    match extension.as_str() {
        "html" | "htm" => "text/html; charset=utf-8",
        "js" | "mjs" | "cjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" | "map" => "application/json",
        "webmanifest" => "application/manifest+json",
        "txt" => "text/plain; charset=utf-8",
        "xml" => "application/xml",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "ico" => "image/x-icon",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        "wasm" => "application/wasm",
        "pdf" => "application/pdf",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "ogg" => "audio/ogg",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        _ => "application/octet-stream",
    }
}

#[cfg(all(test, feature = "headless"))]
mod tests {
    use super::*;
    use crate::cell_graph_boundary::CellGraphBoundary;
    use crate::loopback_client_token_service::create_client_token;
    use crate::loopback_client_token_types::CreateLoopbackClientTokenInput;
    use crate::loopback_self_host::{test_support::*, SelfHostSettings};
    use crate::loopback_state::LoopbackManifest;
    use tower::ServiceExt;

    const MASTER: &str = "self-host-test-master-token";
    const PUBLIC: &str = "https://sophia.example.com";
    const JS: &str = "assets/index-Bx3k9aZ1.js";

    fn state(
        app: crate::app_runtime::AppHandle,
        dir: &Path,
        self_host: SelfHost,
    ) -> Arc<LoopbackState> {
        let grant = crate::loopback_token_grants::session_all_token_grant();
        Arc::new(LoopbackState {
            app,
            token: MASTER.into(),
            manifest: LoopbackManifest {
                runtime_profile: "test",
                bind_host: "127.0.0.1",
                port: 0,
                api_url: PUBLIC.into(),
                mcp_url: format!("{PUBLIC}/mcp"),
                openapi_url: format!("{PUBLIC}/openapi.json"),
                token: MASTER.into(),
                pid: 0,
                started_at: String::new(),
                manifest_path: String::new(),
                auth_header: "Authorization: Bearer <token>",
                token_audience: "test",
                token_storage: "memory",
                security_warning: "synthetic test manifest",
                cell_graph_id: None,
                cell_owner: None,
                cell_generation: None,
                cell_registry_revision: None,
                capabilities: grant.scopes.clone(),
                token_scope_mode: grant.scope_mode,
                token_scopes: grant.scopes,
                scope_details: grant.scope_details,
                grant_profiles: grant.grant_profiles,
            },
            jobs: Arc::new(crate::local_jobs::LocalJobRegistry::new(dir.join("jobs")).unwrap()),
            services: Arc::new(crate::local_service_host::LocalServiceHost::default()),
            lifecycle: Arc::new(crate::cell_lifecycle::CellLifecycle::new()),
            cell_graph: Arc::new(CellGraphBoundary::for_test(None)),
            self_host: Arc::new(self_host),
        })
    }

    fn ui_dir(root: &Path) -> PathBuf {
        let ui = root.join("ui");
        std::fs::create_dir_all(ui.join("assets")).unwrap();
        std::fs::write(
            ui.join("index.html"),
            "<!doctype html><title>Sophia</title>",
        )
        .unwrap();
        std::fs::write(ui.join(JS), "console.log('organism')").unwrap();
        std::fs::write(ui.join("favicon.svg"), "<svg/>").unwrap();
        std::fs::write(root.join("secret.txt"), "outside the UI").unwrap();
        ui
    }

    async fn send(
        state: &Arc<LoopbackState>,
        method: &str,
        path: &str,
        headers: &[(&str, String)],
    ) -> (StatusCode, HeaderMap, Vec<u8>) {
        send_with(state, method, path, headers, Body::empty()).await
    }

    async fn send_with(
        state: &Arc<LoopbackState>,
        method: &str,
        path: &str,
        headers: &[(&str, String)],
        body: Body,
    ) -> (StatusCode, HeaderMap, Vec<u8>) {
        let mut request = axum::http::Request::builder().method(method).uri(path);
        for (name, value) in headers {
            request = request.header(*name, value.as_str());
        }
        let response = crate::loopback_router::loopback_router(state.clone())
            .oneshot(request.body(body).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let body = axum::body::to_bytes(response.into_body(), 10_000_000)
            .await
            .unwrap()
            .to_vec();
        (status, headers, body)
    }

    fn with_profile(name: &str, test: impl FnOnce(&Path)) {
        let _serial = crate::tauri_runtime::profile_env_serial()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let root = std::env::temp_dir().join(format!("garden-{name}-{}", uuid::Uuid::new_v4()));
        let profile = root.join("profile");
        std::fs::create_dir_all(&profile).unwrap();
        let previous = std::env::var_os("GARDEN_PROFILE_DIR");
        std::env::set_var("GARDEN_PROFILE_DIR", &profile);
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| test(&root)));
        match previous {
            Some(previous) => std::env::set_var("GARDEN_PROFILE_DIR", previous),
            None => std::env::remove_var("GARDEN_PROFILE_DIR"),
        }
        let _ = std::fs::remove_dir_all(&root);
        if let Err(error) = outcome {
            std::panic::resume_unwind(error);
        }
    }

    fn header(headers: &HeaderMap, name: &str) -> String {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_string()
    }

    /// With no knob set: no UI, no `/self-host.json`, an Access token is
    /// ignored, and CORS still mirrors any origin, exactly as before.
    #[test]
    fn with_no_knob_the_server_is_unchanged() {
        with_profile("selfhost-off", |root| {
            crate::app_runtime::async_runtime::block_on(async {
                let key = TestKey::generate("k1");
                let app = crate::tauri_runtime::build_mock_app_for_tests(true);
                let state = state(app, root, SelfHost::default());

                let (status, headers, body) =
                    send(&state, "GET", "/", &[("accept", "text/html".into())]).await;
                assert_eq!(status, StatusCode::NOT_FOUND);
                assert!(body.is_empty());
                assert!(headers.get(header::CONTENT_TYPE).is_none());
                let (status, _, body) = send(&state, "GET", SELF_HOST_JSON_PATH, &[]).await;
                assert_eq!((status, body.is_empty()), (StatusCode::NOT_FOUND, true));

                let jwt = key.sign(&person_claims("vera@example.com"));
                let (status, _, body) = send(
                    &state,
                    "GET",
                    "/manifest",
                    &[("cf-access-jwt-assertion", jwt.clone())],
                )
                .await;
                assert_eq!(status, StatusCode::UNAUTHORIZED);
                assert!(String::from_utf8_lossy(&body).contains("missing bearer token"));

                let (status, headers, _) = send(
                    &state,
                    "GET",
                    "/manifest",
                    &[
                        ("authorization", format!("Bearer {MASTER}")),
                        ("origin", "https://evil.example".into()),
                    ],
                )
                .await;
                assert_eq!(
                    status,
                    StatusCode::FORBIDDEN,
                    "foreign origin still refused"
                );
                assert_eq!(
                    header(&headers, "access-control-allow-origin"),
                    "https://evil.example",
                    "very_permissive CORS is unchanged"
                );
            });
        });
    }

    #[test]
    fn ui_is_served_with_an_spa_fallback_and_the_api_is_untouched() {
        with_profile("selfhost-ui", |root| {
            crate::app_runtime::async_runtime::block_on(async {
                let ui = ui_dir(root);
                let self_host = SelfHost::from_settings(SelfHostSettings {
                    ui_dir: Some(ui.display().to_string()),
                    ..SelfHostSettings::default()
                })
                .unwrap();
                let app = crate::tauri_runtime::build_mock_app_for_tests(true);
                let state = state(app, root, self_host);
                let navigate = || {
                    vec![
                        ("sec-fetch-mode", "navigate".to_string()),
                        ("accept", "text/html".to_string()),
                    ]
                };

                // `/` is the app.
                let (status, headers, body) = send(&state, "GET", "/", &navigate()).await;
                assert_eq!(status, StatusCode::OK);
                assert_eq!(header(&headers, "content-type"), "text/html; charset=utf-8");
                assert_eq!(header(&headers, "cache-control"), "no-cache");
                assert!(String::from_utf8_lossy(&body).contains("<title>Sophia</title>"));
                let etag = header(&headers, "etag");
                assert!(!etag.is_empty());
                let (status, _, body) =
                    send(&state, "GET", "/", &[("if-none-match", etag.clone())]).await;
                assert_eq!((status, body.is_empty()), (StatusCode::NOT_MODIFIED, true));

                // Hashed assets are immutable; other files revalidate.
                let (status, headers, body) = send(&state, "GET", &format!("/{JS}"), &[]).await;
                assert_eq!(status, StatusCode::OK);
                assert_eq!(
                    header(&headers, "content-type"),
                    "text/javascript; charset=utf-8"
                );
                assert_eq!(header(&headers, "cache-control"), IMMUTABLE);
                assert_eq!(body, b"console.log('organism')");
                let (_, headers, _) = send(&state, "GET", "/favicon.svg", &[]).await;
                assert_eq!(header(&headers, "content-type"), "image/svg+xml");
                assert_eq!(header(&headers, "cache-control"), "no-cache");

                // A navigation to a client-side path gets the app; a fetch of a
                // missing file gets the ordinary empty 404.
                for route in [
                    "/g/sophia-cluster",
                    "/home",
                    "/settings",
                    "/settings/appearance",
                    "/ops-health",
                ] {
                    let (status, headers, body) = send(&state, "GET", route, &navigate()).await;
                    assert_eq!(status, StatusCode::OK, "{route}");
                    assert_eq!(header(&headers, "content-type"), "text/html; charset=utf-8");
                    assert!(String::from_utf8_lossy(&body).contains("<title>Sophia</title>"));
                }
                let (_, headers, body) =
                    send(&state, "GET", SELF_HOST_JSON_PATH, &navigate()).await;
                assert_eq!(header(&headers, "content-type"), "application/json");
                assert!(!String::from_utf8_lossy(&body).contains("<title>"));
                let (status, _, body) = send(
                    &state,
                    "GET",
                    "/assets/missing-Zz9Zz9Zz.js",
                    &[("accept", "*/*".into())],
                )
                .await;
                assert_eq!((status, body.is_empty()), (StatusCode::NOT_FOUND, true));

                // Nothing outside the UI folder, however it is spelled.
                for path in [
                    "/../secret.txt",
                    "/%2e%2e/secret.txt",
                    "/assets/..%2f..%2fsecret.txt",
                    "/.env",
                ] {
                    let (status, _, body) = send(&state, "GET", path, &[]).await;
                    assert_eq!(status, StatusCode::NOT_FOUND, "{path}");
                    assert!(
                        !String::from_utf8_lossy(&body).contains("outside the UI"),
                        "{path}"
                    );
                }

                // API routes answer exactly as before, browser navigation or not.
                let (status, headers, _) =
                    send(&state, "GET", "/emporium/vocabs", &navigate()).await;
                assert_eq!(status, StatusCode::OK);
                assert_eq!(header(&headers, "content-type"), "application/json");
                let (status, headers, body) = send(&state, "GET", "/health", &navigate()).await;
                assert_eq!(status, StatusCode::OK);
                assert_eq!(header(&headers, "content-type"), "application/json");
                let health: serde_json::Value = serde_json::from_slice(&body).unwrap();
                assert_eq!(health["status"], "ok");
                let (status, _, body) = send(&state, "GET", "/manifest", &navigate()).await;
                assert_eq!(status, StatusCode::UNAUTHORIZED);
                assert!(String::from_utf8_lossy(&body).contains("missing bearer token"));
                let (status, _, _) = send(
                    &state,
                    "GET",
                    "/manifest",
                    &[("authorization", format!("Bearer {MASTER}"))],
                )
                .await;
                assert_eq!(status, StatusCode::OK);

                // Key mode: the contract says so, with no user.
                let (status, headers, body) = send(&state, "GET", SELF_HOST_JSON_PATH, &[]).await;
                assert_eq!(status, StatusCode::OK);
                assert_eq!(header(&headers, "cache-control"), "no-store");
                let described: serde_json::Value = serde_json::from_slice(&body).unwrap();
                assert_eq!(
                    described,
                    serde_json::json!({
                        "kind": "gardend-self-host",
                        "version": env!("CARGO_PKG_VERSION"),
                        "auth": "key",
                        "user": null,
                    })
                );

                // CORS is narrowed: loopback still answered, a stranger not.
                let (_, headers, _) = send(
                    &state,
                    "OPTIONS",
                    "/manifest",
                    &[
                        ("origin", "http://localhost:5173".into()),
                        ("access-control-request-method", "GET".into()),
                    ],
                )
                .await;
                assert_eq!(
                    header(&headers, "access-control-allow-origin"),
                    "http://localhost:5173"
                );
                let (_, headers, _) = send(
                    &state,
                    "OPTIONS",
                    "/manifest",
                    &[
                        ("origin", "https://evil.example".into()),
                        ("access-control-request-method", "GET".into()),
                    ],
                )
                .await;
                assert!(headers.get("access-control-allow-origin").is_none());
            });
        });
    }

    #[test]
    fn cloudflare_access_logs_people_and_service_tokens_in() {
        with_profile("selfhost-access", |root| {
            crate::app_runtime::async_runtime::block_on(async {
                let key = TestKey::generate("k1");
                let self_host = SelfHost::from_settings(SelfHostSettings {
                    ui_dir: Some(ui_dir(root).display().to_string()),
                    cf_access_team_domain: Some(TEAM.into()),
                    cf_access_aud: Some(AUD.into()),
                    cf_access_admin_emails: Some("vera@example.com".into()),
                    public_base_url: Some(format!("{PUBLIC}/")),
                    ..SelfHostSettings::default()
                })
                .unwrap();
                self_host
                    .access()
                    .unwrap()
                    .install_keys(jwks(&[&key]).as_bytes())
                    .unwrap();
                let app = crate::tauri_runtime::build_mock_app_for_tests(true);
                let state = state(app, root, self_host);
                let vera = key.sign(&person_claims("Vera@example.com"));
                let cousin = key.sign(&person_claims("cousin@example.com"));
                let agent = key.sign(&service_claims("6fe0d2a1.access"));
                let jwt = |token: &str| ("cf-access-jwt-assertion", token.to_string());

                // Everyone admitted reads; only listed admins administer. The
                // identity is the token's alone: an X-User-ID naming an admin
                // changes nothing.
                for token in [&vera, &cousin, &agent] {
                    let (status, _, body) = send(&state, "GET", "/manifest", &[jwt(token)]).await;
                    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
                }
                let mcp = |token: &str, call: serde_json::Value| {
                    let headers = vec![
                        jwt(token),
                        ("content-type", "application/json".to_string()),
                        ("x-user-id", "vera@example.com".to_string()),
                    ];
                    let state = state.clone();
                    async move {
                        let (status, _, body) = send_with(
                            &state,
                            "POST",
                            "/mcp",
                            &headers,
                            Body::from(serde_json::to_vec(&call).unwrap()),
                        )
                        .await;
                        assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
                        serde_json::from_slice::<serde_json::Value>(&body).unwrap()
                    }
                };
                let listed = mcp(
                    cousin.as_str(),
                    serde_json::json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }),
                )
                .await;
                assert!(listed["result"]["tools"]
                    .as_array()
                    .is_some_and(|tools| !tools.is_empty()));
                let update = serde_json::json!({
                    "jsonrpc": "2.0", "id": 2, "method": "tools/call",
                    "params": { "name": "sparql_update", "arguments": { "graph_id": "g", "query": "INSERT DATA {}" } },
                });
                let refused = mcp(cousin.as_str(), update.clone()).await;
                assert!(
                    refused["error"]["message"].as_str().is_some_and(
                        |message| message.contains("missing required scope rdf.update")
                    ),
                    "{refused}"
                );
                let refused = mcp(agent.as_str(), update).await;
                assert!(refused["error"]["message"]
                    .as_str()
                    .is_some_and(|message| message.contains("rdf.update")));

                // The public origin is allowed; a stranger, `null` and a
                // browser-marked cross-site request are not.
                let (status, headers, _) = send(
                    &state,
                    "GET",
                    "/manifest",
                    &[jwt(&vera), ("origin", PUBLIC.into())],
                )
                .await;
                assert_eq!(status, StatusCode::OK);
                assert_eq!(header(&headers, "access-control-allow-origin"), PUBLIC);
                for (name, value) in [
                    ("origin", "https://evil.example"),
                    ("origin", "null"),
                    ("sec-fetch-site", "cross-site"),
                ] {
                    let (status, headers, _) = send(
                        &state,
                        "GET",
                        "/manifest",
                        &[jwt(&vera), (name, value.into())],
                    )
                    .await;
                    assert_eq!(status, StatusCode::FORBIDDEN, "{name}: {value}");
                    assert!(
                        headers.get("access-control-allow-origin").is_none(),
                        "{name}: {value}"
                    );
                }

                // A forged or foreign token is refused; bearer tokens work as before.
                let forged = TestKey::generate("k1").sign(&person_claims("vera@example.com"));
                let (status, _, body) = send(&state, "GET", "/manifest", &[jwt(&forged)]).await;
                assert_eq!(status, StatusCode::UNAUTHORIZED);
                assert!(String::from_utf8_lossy(&body).contains("invalid Cloudflare Access token"));
                let (status, _, _) = send(
                    &state,
                    "GET",
                    "/manifest",
                    &[("authorization", format!("Bearer {MASTER}"))],
                )
                .await;
                assert_eq!(status, StatusCode::OK);
                let (status, _, _) = send(
                    &state,
                    "GET",
                    "/manifest",
                    &[("authorization", "Bearer not-a-token".into()), jwt(&vera)],
                )
                .await;
                assert_eq!(
                    status,
                    StatusCode::OK,
                    "a stale bearer does not hide a valid Access login"
                );

                // /self-host.json names the person; a service token is no person.
                let describe = |token: Option<&str>| {
                    let headers = token.map(|token| vec![jwt(token)]).unwrap_or_default();
                    let state = state.clone();
                    async move {
                        let (status, _, body) =
                            send(&state, "GET", SELF_HOST_JSON_PATH, &headers).await;
                        assert_eq!(status, StatusCode::OK);
                        serde_json::from_slice::<serde_json::Value>(&body).unwrap()
                    }
                };
                let described = describe(Some(vera.as_str())).await;
                assert_eq!(described["auth"], "cloudflare-access");
                assert_eq!(
                    described["user"],
                    serde_json::json!({ "email": "vera@example.com" })
                );
                assert_eq!(described.as_object().unwrap().len(), 4);
                assert_eq!(
                    describe(Some(agent.as_str())).await["user"],
                    serde_json::Value::Null
                );
                assert_eq!(
                    describe(Some(forged.as_str())).await["user"],
                    serde_json::Value::Null
                );
                assert_eq!(describe(None).await["user"], serde_json::Value::Null);

                // WebSocket rooms: master, scoped client tokens, and Access.
                let ws = |pairs: &[(&'static str, String)]| {
                    let mut headers = HeaderMap::new();
                    for (name, value) in pairs {
                        headers.insert(*name, value.parse().unwrap());
                    }
                    headers
                };
                let authorize = |headers: &HeaderMap, scope: &str| {
                    crate::loopback_hocuspocus_routes::authorize_ws(headers, &state, scope)
                };
                let doc = "documents.write.crdt";
                assert_eq!(
                    authorize(&ws(&[("authorization", format!("Bearer {MASTER}"))]), doc).ok(),
                    Some(None)
                );
                let issue = |profile: &str| {
                    create_client_token(
                        &state.app,
                        CreateLoopbackClientTokenInput {
                            label: Some(format!("self-host {profile}")),
                            grant_profile_id: Some(profile.into()),
                            scopes: None,
                            expires_in_days: None,
                        },
                    )
                    .unwrap()
                    .token
                };
                let authoring = issue("authoring");
                let read_only = issue("read-only");
                let offered = format!("bearer.{authoring}");
                assert_eq!(
                    authorize(&ws(&[("sec-websocket-protocol", offered.clone())]), doc).ok(),
                    Some(Some(offered))
                );
                assert_eq!(
                    authorize(
                        &ws(&[("authorization", format!("Bearer {authoring}"))]),
                        "workspace.write.crdt"
                    )
                    .ok(),
                    Some(None)
                );
                let refused = authorize(
                    &ws(&[("authorization", format!("Bearer {read_only}"))]),
                    doc,
                )
                .unwrap_err();
                assert_eq!(refused.status(), StatusCode::FORBIDDEN);
                let refused = authorize(
                    &ws(&[("sec-websocket-protocol", "bearer.nope".into())]),
                    doc,
                )
                .unwrap_err();
                assert_eq!(refused.status(), StatusCode::UNAUTHORIZED);

                assert_eq!(
                    authorize(&ws(&[jwt(&cousin), ("origin", PUBLIC.into())]), doc).ok(),
                    Some(None)
                );
                assert_eq!(
                    authorize(
                        &ws(&[
                            jwt(&cousin),
                            ("origin", PUBLIC.into()),
                            ("sec-websocket-protocol", "sophia".into())
                        ]),
                        doc
                    )
                    .ok(),
                    Some(Some("sophia".to_string())),
                    "an offered subprotocol is echoed so the browser completes the handshake"
                );
                let hijack = authorize(
                    &ws(&[jwt(&vera), ("origin", "https://evil.example".into())]),
                    doc,
                )
                .unwrap_err();
                assert_eq!(hijack.status(), StatusCode::FORBIDDEN);
                let forged =
                    authorize(&ws(&[jwt(&forged), ("origin", PUBLIC.into())]), doc).unwrap_err();
                assert_eq!(forged.status(), StatusCode::UNAUTHORIZED);
            });
        });
    }

    #[test]
    fn hashed_asset_and_path_rules() {
        assert!(is_hashed_asset("assets/index-Bx3k9aZ1.js"));
        assert!(is_hashed_asset("assets/pdf.worker.min-DlJ2xP0q.mjs"));
        assert!(!is_hashed_asset("index.html"));
        assert!(
            !is_hashed_asset("favicon-abcdefgh.svg"),
            "only Vite's assets/ folder is hashed"
        );
        assert!(!is_hashed_asset("assets/logo.svg"));
        assert!(!is_hashed_asset("assets/index-Bx3.js"));
        assert_eq!(ui_relative_path("/"), Some(vec![]));
        assert_eq!(
            ui_relative_path("/a//b/"),
            Some(vec!["a".to_string(), "b".to_string()])
        );
        assert_eq!(
            ui_relative_path("/caf%C3%A9.txt"),
            Some(vec!["café.txt".to_string()])
        );
        assert_eq!(ui_relative_path("/%zz"), None);
        assert_eq!(ui_relative_path("/a/%5c..%5csecret"), None);
        assert_eq!(ui_relative_path("/C:/secret"), None);
    }
}
