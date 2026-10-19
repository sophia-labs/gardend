//! Public mode end to end: the real router served over real HTTP (per-address
//! rate limits need the peer address), a real profile with real graphs, the
//! real MCP surface, and a real supervised child service (the test binary
//! re-run as an echo server) behind the proxy. No mocks.

use super::*;
use crate::{
    cell_graph_boundary::CellGraphBoundary,
    local_service_host::{
        child::{fast_timing, wait_service_healthy, write_service_manifest},
        LocalServiceHost,
    },
    loopback_self_host::{test_support::TestKey, SelfHost, SelfHostSettings},
    loopback_state::{LoopbackManifest, LoopbackState},
};
use serde_json::{json, Value};
use std::{net::SocketAddr, sync::Arc};

const MASTER: &str = "public-mode-test-master-token";
const ISSUER: &str = "https://cognito-idp.us-west-1.amazonaws.com/us-west-1_TestPool";
const CLIENT: &str = "test-app-client";

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
            api_url: "http://127.0.0.1".into(),
            mcp_url: "http://127.0.0.1/mcp".into(),
            openapi_url: String::new(),
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
        services: Arc::new(LocalServiceHost::default()),
        lifecycle: Arc::new(crate::cell_lifecycle::CellLifecycle::new()),
        cell_graph: Arc::new(CellGraphBoundary::for_test(None)),
        self_host: Arc::new(self_host),
    })
}

fn with_profile(name: &str, test: impl FnOnce(&Path, &Path)) {
    let _serial = crate::tauri_runtime::profile_env_serial()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let root = std::env::temp_dir().join(format!("garden-public-{name}-{}", uuid::Uuid::new_v4()));
    let profile = root.join("profile");
    std::fs::create_dir_all(&profile).unwrap();
    let previous = std::env::var_os("GARDEN_PROFILE_DIR");
    std::env::set_var("GARDEN_PROFILE_DIR", &profile);
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| test(&root, &profile)));
    match previous {
        Some(previous) => std::env::set_var("GARDEN_PROFILE_DIR", previous),
        None => std::env::remove_var("GARDEN_PROFILE_DIR"),
    }
    let _ = std::fs::remove_dir_all(&root);
    if let Err(error) = outcome {
        std::panic::resume_unwind(error);
    }
}

fn graphs(state: &LoopbackState, ids: &[&str]) {
    for graph in ids {
        crate::graph_service::create_graph_service(
            &state.app,
            crate::graph_service::CreateGraphInput {
                graph_id: Some((*graph).into()),
                title: format!("{graph} graph"),
                description: None,
                operation_id: None,
            },
        )
        .unwrap();
    }
}

/// Serve `state`'s router on an ephemeral loopback port, as gardend does
/// (with the peer address).
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
    format!("http://{address}")
}

struct Client {
    base: String,
    http: reqwest::Client,
}

impl Client {
    fn new(base: String) -> Self {
        Self {
            base,
            http: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap(),
        }
    }

    async fn send(
        &self,
        method: reqwest::Method,
        path: &str,
        headers: &[(&str, &str)],
        body: Option<Value>,
    ) -> (u16, reqwest::header::HeaderMap, Value) {
        let mut request = self.http.request(method, format!("{}{path}", self.base));
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await.unwrap();
        let status = response.status().as_u16();
        let headers = response.headers().clone();
        let text = response.text().await.unwrap();
        (
            status,
            headers,
            serde_json::from_str(&text).unwrap_or(Value::String(text)),
        )
    }

    async fn get(&self, path: &str, headers: &[(&str, &str)]) -> (u16, Value) {
        let (status, _, body) = self.send(reqwest::Method::GET, path, headers, None).await;
        (status, body)
    }

    async fn post(&self, path: &str, headers: &[(&str, &str)], body: Value) -> (u16, Value) {
        let (status, _, body) = self
            .send(reqwest::Method::POST, path, headers, Some(body))
            .await;
        (status, body)
    }

    async fn mcp(&self, headers: &[(&str, &str)], method: &str, params: Value) -> Value {
        let (status, body) = self
            .post(
                "/mcp",
                headers,
                json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params }),
            )
            .await;
        assert_eq!(status, 200, "{body}");
        body
    }

    async fn tool(&self, headers: &[(&str, &str)], name: &str, arguments: Value) -> Value {
        self.mcp(
            headers,
            "tools/call",
            json!({ "name": name, "arguments": arguments }),
        )
        .await
    }
}

fn owner() -> String {
    format!("Bearer {MASTER}")
}

fn graph_ids(body: &Value) -> Vec<String> {
    body.as_array()
        .or_else(|| body.get("graphs").and_then(Value::as_array))
        .into_iter()
        .flatten()
        .filter_map(|graph| {
            graph
                .get("graph_id")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .collect()
}

fn public_state(
    app: crate::app_runtime::AppHandle,
    root: &Path,
    profile: &Path,
    config: Value,
) -> Arc<LoopbackState> {
    let self_host = SelfHost::from_settings(SelfHostSettings::default())
        .unwrap()
        .with_public(
            Some(PublicMode::for_test(config, profile)),
            Some("https://cow-tools.example.com"),
        );
    state(app, root, self_host)
}

/// Unconfigured, nothing changes: a caller with no credential is refused
/// everywhere, exactly as before public mode existed.
#[test]
fn public_mode_is_inert_unless_configured() {
    with_profile("inert", |root, profile| {
        assert!(
            PublicMode::load(profile, None).unwrap().is_none(),
            "no public.json, no GARDEN_PUBLIC_GRAPHS: off"
        );
        let app = crate::tauri_runtime::build_mock_app_for_tests(true);
        let state = state(app, root, SelfHost::default());
        graphs(&state, &["uruguay-cows"]);
        crate::app_runtime::async_runtime::block_on(async {
            let client = Client::new(serve(&state).await);
            let (status, _) = client.get("/graphs", &[]).await;
            assert_eq!(status, 401);
            let (status, _) = client.get("/navigation/uruguay-cows", &[]).await;
            assert_eq!(status, 401);
            let (status, _) = client
                .post(
                    "/mcp",
                    &[],
                    json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": {} }),
                )
                .await;
            assert_eq!(status, 401);
            let (status, _) = client.get("/self-host.json", &[]).await;
            assert_eq!(status, 404);
            // A hosted cell never turns it on, whatever its profile says.
            std::fs::write(
                profile.join(PUBLIC_CONFIG_FILE),
                r#"{"graphs":["uruguay-cows"]}"#,
            )
            .unwrap();
            assert!(PublicMode::load(profile, None).unwrap().is_some());
            assert!(
                crate::loopback_server::load_public_mode_for_test(&state.app, true)
                    .unwrap()
                    .is_none()
            );
            assert!(
                crate::loopback_server::load_public_mode_for_test(&state.app, false)
                    .unwrap()
                    .is_some()
            );
        });
    });
}

/// A visitor reads the listed graphs over HTTP and MCP, never another graph,
/// never a write; the owner's key still opens everything.
#[test]
fn a_visitor_reads_the_public_graphs_and_nothing_else() {
    with_profile("visitor", |root, profile| {
        let app = crate::tauri_runtime::build_mock_app_for_tests(true);
        let state = public_state(
            app,
            root,
            profile,
            json!({ "graphs": ["uruguay-cows"], "sparql": { "maxRows": 2 } }),
        );
        graphs(&state, &["uruguay-cows", "vera-private"]);
        crate::app_runtime::async_runtime::block_on(async {
            let client = Client::new(serve(&state).await);
            let owner = owner();
            let as_owner = [("authorization", owner.as_str())];
            let made = client
                .tool(
                    &as_owner,
                    "create_document",
                    json!({ "graphId": "uruguay-cows", "title": "Florida dairy" }),
                )
                .await;
            assert!(made.get("error").is_none(), "{made}");

            // The graph list: the public graph only, as a viewer.
            let (status, listed) = client.get("/graphs", &[]).await;
            assert_eq!(status, 200, "{listed}");
            assert_eq!(graph_ids(&listed), vec!["uruguay-cows".to_string()]);
            assert_eq!(listed[0]["role"], "viewer");
            let (_, everything) = client.get("/api/graphs", &as_owner).await;
            assert_eq!(
                graph_ids(&everything).len(),
                2,
                "the owner still sees every graph"
            );

            // HTTP reads: the public graph, not the private one (404, as if it did not exist).
            let (status, workspace) = client.get("/navigation/uruguay-cows", &[]).await;
            assert_eq!(status, 200, "{workspace}");
            let (status, hidden) = client.get("/navigation/vera-private", &[]).await;
            assert_eq!(status, 404, "{hidden}");
            let (status, _) = client.get("/graphs/uruguay-cows/summary", &[]).await;
            assert_eq!(status, 200);
            let (status, graph) = client.get("/graphs/uruguay-cows", &[]).await;
            assert_eq!(
                (status, graph["graph_id"].as_str(), graph["role"].as_str()),
                (200, Some("uruguay-cows"), Some("viewer")),
                "{graph}"
            );
            let (status, _) = client.get("/graphs/vera-private", &[]).await;
            assert_eq!(status, 404);
            let (status, _) = client.get("/navigation/uruguay-cows/trash", &[]).await;
            assert_eq!(status, 403, "the trash stays the owner's");

            // SPARQL: the public graph, capped at the public row ceiling.
            let query = "SELECT ?n WHERE { VALUES ?n { 1 2 3 4 5 } }";
            let (status, rows) = client
                .post(
                    "/api/sparql/query",
                    &[],
                    json!({ "graphId": "uruguay-cows", "sparql": query }),
                )
                .await;
            assert_eq!(status, 200, "{rows}");
            let text = rows.to_string();
            assert!(
                text.contains("truncat"),
                "the visitor's answer is cut at 2 rows: {rows}"
            );
            let (status, all_rows) = client
                .post(
                    "/api/sparql/query",
                    &as_owner,
                    json!({ "graphId": "uruguay-cows", "sparql": query }),
                )
                .await;
            assert_eq!(status, 200);
            assert!(
                !all_rows.to_string().contains("truncat"),
                "the owner's is not: {all_rows}"
            );
            let (status, _) = client
                .post(
                    "/api/sparql/query",
                    &[],
                    json!({ "graphId": "vera-private", "sparql": query }),
                )
                .await;
            assert_eq!(status, 404);
            let (status, _) = client
                .post("/api/sparql/update", &[], json!({ "graphId": "uruguay-cows", "sparql": "INSERT DATA { <a:b> <a:c> <a:d> }" }))
                .await;
            assert_eq!(status, 403);

            // Writes over HTTP: refused before any handler.
            let (status, refused) = client
                .post(
                    "/graphs",
                    &[],
                    json!({ "graph_id": "mine", "title": "Mine" }),
                )
                .await;
            assert_eq!(status, 403, "{refused}");
            assert_eq!(refused["code"], "public_read_only");
            let (status, _) = client
                .post(
                    "/api/crdt/operations",
                    &[],
                    json!({ "kind": "workspace.createDocument", "graphId": "uruguay-cows", "payload": {} }),
                )
                .await;
            assert_eq!(status, 403);
            let (status, _, _) = client
                .send(reqwest::Method::DELETE, "/graphs/uruguay-cows", &[], None)
                .await;
            assert_eq!(status, 403);
            let (status, _) = client.get("/api/services/vehicle/logs", &[]).await;
            assert_eq!(status, 403, "no owner routes");
            let (status, _) = client.get("/manifest", &[]).await;
            assert_eq!(status, 403);

            // MCP: read tools only, each naming a public graph.
            let listed = client.mcp(&[], "tools/list", json!({})).await;
            let names: Vec<&str> = listed["result"]["tools"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|tool| tool["name"].as_str())
                .collect();
            for reader in [
                "list_graphs",
                "read_document",
                "search_documents",
                "sparql_query",
                "get_workspace",
            ] {
                assert!(names.contains(&reader), "{reader} is listed: {names:?}");
            }
            for writer in [
                "create_document",
                "write_document",
                "sparql_update",
                "delete",
                "create_graph",
                "rdf_load",
            ] {
                assert!(!names.contains(&writer), "{writer} is not listed");
            }
            assert_eq!(
                listed["result"]["_meta"]["sophia.publicGraphs"],
                json!(["uruguay-cows"])
            );
            let graphs_listed = client.tool(&[], "list_graphs", json!({})).await;
            assert_eq!(
                graph_ids(&graphs_listed["result"]["structuredContent"]),
                vec!["uruguay-cows".to_string()]
            );
            let documents = client
                .tool(&[], "list_documents", json!({ "graphId": "uruguay-cows" }))
                .await;
            assert!(documents.get("error").is_none(), "{documents}");
            let hidden = client
                .tool(&[], "list_documents", json!({ "graphId": "vera-private" }))
                .await;
            assert_eq!(hidden["error"]["message"], "graph not found", "{hidden}");
            let unnamed = client.tool(&[], "list_documents", json!({})).await;
            assert!(
                hidden.get("error").is_some()
                    && unnamed["error"]["message"]
                        .as_str()
                        .unwrap_or_default()
                        .contains("uruguay-cows"),
                "{unnamed}"
            );
            let write = client
                .tool(
                    &[],
                    "create_document",
                    json!({ "graphId": "uruguay-cows", "title": "Graffiti" }),
                )
                .await;
            assert_eq!(write["error"]["code"], -32003, "{write}");
            let sneaky = client
                .tool(&[], "sparql_query", json!({ "graphId": "uruguay-cows", "query": "SELECT * WHERE { ?s ?p ?o }", "sourceGraphId": "vera-private" }))
                .await;
            assert!(
                sneaky.get("error").is_some(),
                "another graph anywhere in the arguments is refused: {sneaky}"
            );
            let rows = client
                .tool(
                    &[],
                    "sparql_query",
                    json!({ "graphId": "uruguay-cows", "query": query }),
                )
                .await;
            assert!(
                rows.to_string().contains("truncat"),
                "MCP SPARQL is capped too: {rows}"
            );

            // The owner administers as before.
            let (status, _) = client.get("/api/services/vehicle/status", &as_owner).await;
            assert_eq!(status, 200);
        });
    });
}

/// A visitor's requests are limited per address: a burst, then 429 with
/// Retry-After, then served again once the bucket refills. The owner is not
/// limited.
#[test]
fn rate_limits_trip_and_recover() {
    with_profile("limits", |root, profile| {
        let app = crate::tauri_runtime::build_mock_app_for_tests(true);
        let state = public_state(
            app,
            root,
            profile,
            json!({ "graphs": ["uruguay-cows"], "rateLimit": { "perMinute": 60, "burst": 3 }, "maxBodyBytes": 2048 }),
        );
        graphs(&state, &["uruguay-cows"]);
        crate::app_runtime::async_runtime::block_on(async {
            let client = Client::new(serve(&state).await);
            for _ in 0..3 {
                assert_eq!(client.get("/graphs", &[]).await.0, 200);
            }
            let (status, headers, body) = client
                .send(reqwest::Method::GET, "/graphs", &[], None)
                .await;
            assert_eq!(status, 429, "{body}");
            assert_eq!(body["code"], "rate_limited");
            assert_eq!(
                headers.get("retry-after").and_then(|v| v.to_str().ok()),
                Some("1")
            );
            let owner = owner();
            for _ in 0..5 {
                assert_eq!(
                    client
                        .get("/api/graphs", &[("authorization", owner.as_str())])
                        .await
                        .0,
                    200,
                    "the owner is never limited"
                );
            }
            tokio::time::sleep(std::time::Duration::from_millis(1_100)).await;
            assert_eq!(client.get("/graphs", &[]).await.0, 200, "recovered");
            assert_eq!(client.get("/graphs", &[]).await.0, 429);
            tokio::time::sleep(std::time::Duration::from_millis(3_100)).await;
            let big = "x".repeat(4096);
            let (status, refused) = client
                .post(
                    "/api/sparql/query",
                    &[],
                    json!({ "graphId": "uruguay-cows", "sparql": big }),
                )
                .await;
            assert_eq!(status, 413, "{refused}");
        });
    });
}

/// A Sophia account (a Cognito ID token, verified against the pool's keys) is
/// a viewer of the public graphs, like a visitor; a forged, expired, wrongly
/// addressed or access token is refused.
#[test]
fn a_sophia_account_is_a_viewer_with_its_id_token() {
    with_profile("accounts", |root, profile| {
        let app = crate::tauri_runtime::build_mock_app_for_tests(true);
        let state = public_state(
            app,
            root,
            profile,
            json!({
                "graphs": ["uruguay-cows", "uruguay-catalogue"],
                "services": ["vehicle"],
                "accounts": { "cognito": { "issuer": ISSUER, "clientId": CLIENT, "domain": "auth.example.com", "callbackUrl": "https://cow-tools.example.com/auth/callback" } }
            }),
        );
        graphs(
            &state,
            &["uruguay-cows", "uruguay-catalogue", "vera-private"],
        );
        let key = TestKey::generate("pool-key");
        let forger = TestKey::generate("pool-key");
        state
            .self_host
            .public()
            .unwrap()
            .accounts()
            .unwrap()
            .install_keys(crate::loopback_self_host::test_support::jwks(&[&key]).as_bytes())
            .unwrap();
        let now = jsonwebtoken::get_current_timestamp();
        let claims = |overrides: Value| {
            let mut claims = json!({
                "sub": "0f6a2c1e-5b7d-4c3a-9e8f-1a2b3c4d5e6f", "email": "Visitor@Example.com", "email_verified": true,
                "token_use": "id", "aud": CLIENT, "iss": ISSUER, "exp": now + 600, "iat": now,
            });
            for (name, value) in overrides.as_object().unwrap() {
                claims[name] = value.clone();
            }
            claims
        };
        let dir = root.join("services");
        write_service_manifest(
            &dir.join("vehicle"),
            "vehicle",
            &[],
            json!({ "memberAccess": "all", "delegateCaller": true, "acceptsModelKey": true }),
        );
        write_service_manifest(
            &dir.join("tools"),
            "tools",
            &[],
            json!({ "memberAccess": "all" }),
        );
        state.services.pin_services_dir(dir);
        state.services.set_timing(fast_timing());
        state
            .services
            .set_local_api_url("http://127.0.0.1:9".into());
        for service in ["vehicle", "tools"] {
            state
                .services
                .start(service, &state.app, &state.manifest)
                .unwrap();
            wait_service_healthy(&state.services, service);
        }
        crate::app_runtime::async_runtime::block_on(async {
            let client = Client::new(serve(&state).await);
            let token = format!("Bearer {}", key.sign(&claims(json!({}))));
            let as_account = [("authorization", token.as_str())];
            let (status, listed) = client.get("/graphs", &as_account).await;
            assert_eq!(status, 200, "{listed}");
            assert_eq!(
                graph_ids(&listed),
                vec!["uruguay-catalogue".to_string(), "uruguay-cows".to_string()]
            );
            let (status, _) = client.get("/navigation/vera-private", &as_account).await;
            assert_eq!(status, 404);
            let (status, _) = client
                .post(
                    "/graphs",
                    &as_account,
                    json!({ "graph_id": "x", "title": "x" }),
                )
                .await;
            assert_eq!(status, 403);

            for (why, bad) in [
                ("forged", forger.sign(&claims(json!({})))),
                ("expired", key.sign(&claims(json!({ "exp": now - 3600 })))),
                (
                    "another app",
                    key.sign(&claims(json!({ "aud": "someone-else" }))),
                ),
                (
                    "another pool",
                    key.sign(&claims(
                        json!({ "iss": "https://cognito-idp.us-east-1.amazonaws.com/other" }),
                    )),
                ),
                (
                    "an access token",
                    key.sign(&claims(json!({ "token_use": "access" }))),
                ),
            ] {
                let bearer = format!("Bearer {bad}");
                let (status, body) = client
                    .get("/graphs", &[("authorization", bearer.as_str())])
                    .await;
                assert_eq!(status, 401, "{why}: {body}");
            }

            // Through the proxy: the account, its verified email, and its own
            // model key, to the service that takes one; a visitor as anon:.
            let (status, echoed) = client
                .get(
                    "/services/vehicle/g/uruguay-cows/sessions",
                    &[
                        ("authorization", token.as_str()),
                        ("x-model-key", "sk-or-v1-the-callers-own"),
                    ],
                )
                .await;
            assert_eq!(status, 200, "{echoed}");
            assert_eq!(
                echoed["userId"],
                "cognito:0f6a2c1e-5b7d-4c3a-9e8f-1a2b3c4d5e6f"
            );
            assert_eq!(echoed["callerKind"], "account");
            assert_eq!(echoed["accountEmail"], "visitor@example.com");
            assert_eq!(echoed["accountEmailVerified"], "true");
            assert_eq!(echoed["graphRole"], "viewer");
            assert_eq!(echoed["modelKey"], "sk-or-v1-the-callers-own");
            assert!(echoed["delegatedToken"]
                .as_str()
                .unwrap_or_default()
                .starts_with("sophia-dlg-"));
            assert!(
                echoed["authorization"].is_null(),
                "the ID token never passes"
            );
            assert_eq!(echoed["publicGraphs"], "uruguay-catalogue,uruguay-cows");

            let visitor = [
                ("x-visitor-id", "a-visitor-of-the-cows-0001"),
                ("x-model-key", "sk-or-v1-the-callers-own"),
            ];
            let (status, echoed) = client
                .get("/services/vehicle/g/uruguay-cows/sessions", &visitor)
                .await;
            assert_eq!(status, 200, "{echoed}");
            let anon = echoed["userId"].as_str().unwrap_or_default().to_string();
            assert!(anon.starts_with("anon:") && anon.len() == 29, "{echoed}");
            assert_eq!(echoed["callerKind"], "anonymous");
            assert_eq!(echoed["callerRole"], "viewer");
            assert!(
                echoed["visitorId"].is_null(),
                "the raw visitor id never passes"
            );
            let (_, again) = client.get("/services/vehicle/sessions", &visitor).await;
            assert_eq!(
                again["userId"],
                anon.as_str(),
                "the same visitor from one request to the next"
            );

            // The key reaches no service that did not ask for it; private
            // graphs and unlisted services are closed to visitors.
            let owner = owner();
            let (_, echoed) = client
                .get(
                    "/services/tools/x",
                    &[
                        ("authorization", owner.as_str()),
                        ("x-model-key", "sk-or-v1-the-callers-own"),
                    ],
                )
                .await;
            assert!(echoed["modelKey"].is_null(), "{echoed}");
            let (status, _) = client
                .get("/services/vehicle/g/vera-private/sessions", &visitor)
                .await;
            assert_eq!(status, 404);
            let (status, _) = client.get("/services/tools/x", &visitor).await;
            assert_eq!(status, 403);

            // The visitor's delegation reads every public graph (its own when a
            // call names none), never a private one, and writes nothing: one
            // conversation over all the public data.
            let (_, echoed) = client
                .get("/services/vehicle/g/uruguay-cows/sessions", &visitor)
                .await;
            let delegated = format!("Bearer {}", echoed["delegatedToken"].as_str().unwrap());
            let as_service = [("authorization", delegated.as_str())];
            let listed = client.mcp(&as_service, "tools/list", json!({})).await;
            assert_eq!(
                listed["result"]["_meta"]["sophia.publicGraphs"],
                json!(["uruguay-catalogue", "uruguay-cows"])
            );
            let read = client.tool(&as_service, "list_documents", json!({})).await;
            assert!(
                read.get("error").is_none(),
                "its own graph when none is named: {read}"
            );
            let other = client
                .tool(
                    &as_service,
                    "list_documents",
                    json!({ "graphId": "uruguay-catalogue" }),
                )
                .await;
            assert!(
                other.get("error").is_none(),
                "another public graph: {other}"
            );
            let private = client
                .tool(
                    &as_service,
                    "list_documents",
                    json!({ "graphId": "vera-private" }),
                )
                .await;
            assert_eq!(private["error"]["message"], "graph not found", "{private}");
            let write = client
                .tool(
                    &as_service,
                    "create_document",
                    json!({ "title": "Graffiti" }),
                )
                .await;
            assert!(
                write.get("error").is_some() || write["result"]["isError"] == true,
                "{write}"
            );
            // The owner's delegation stays bound to its one graph.
            let (_, echoed) = client
                .get(
                    "/services/vehicle/g/uruguay-cows/sessions",
                    &[("authorization", owner.as_str())],
                )
                .await;
            let owners = format!("Bearer {}", echoed["delegatedToken"].as_str().unwrap());
            let bound = client
                .tool(
                    &[("authorization", owners.as_str())],
                    "list_documents",
                    json!({ "graphId": "uruguay-catalogue" }),
                )
                .await;
            assert!(bound.get("error").is_some(), "{bound}");
            assert!(
                echoed["publicGraphs"].is_null(),
                "only a public caller hears of the public graphs"
            );

            // What the page and agents read to find all this.
            let (status, described) = client.get("/self-host.json", &[]).await;
            assert_eq!(status, 200);
            assert_eq!(
                described["public"]["graphs"],
                json!(["uruguay-catalogue", "uruguay-cows"])
            );
            assert_eq!(described["public"]["accounts"]["clientId"], CLIENT);
            assert_eq!(described["public"]["mcp"], "/mcp");
        });
        state.services.shutdown();
    });
}

/// With a site: the site at `/` (its pages, `/.well-known/`, its 404), the
/// app under `/app/` with its own SPA fallback, and the app's assets still
/// found at `/assets/...` (a bundle built for `/`).
#[test]
fn the_site_is_at_the_root_and_the_app_under_app() {
    with_profile("site", |root, profile| {
        let site = profile.join("site");
        std::fs::create_dir_all(site.join(".well-known")).unwrap();
        std::fs::create_dir_all(site.join("datasets")).unwrap();
        std::fs::write(site.join("index.html"), "<title>cow-tools</title>").unwrap();
        std::fs::write(site.join("llms.txt"), "# cow-tools\n").unwrap();
        std::fs::write(site.join("404.html"), "<title>no such cow</title>").unwrap();
        std::fs::write(site.join("datasets/index.html"), "<title>datasets</title>").unwrap();
        std::fs::write(site.join(".well-known/ai-catalog.json"), "{}").unwrap();
        std::fs::write(site.join("index.md"), "# cow-tools\n").unwrap();
        std::fs::write(site.join("datasets.jsonld"), "{}").unwrap();
        std::fs::write(
            site.join("mcp-server-card.json"),
            r#"{"name":"com.sophia-labs/cow-tools","version":"0.1.0","title":"cow-tools","remotes":[{"type":"streamable-http","url":"https://cow-tools.sophia-labs.com/mcp"}]}"#,
        )
        .unwrap();
        let ui = root.join("ui");
        std::fs::create_dir_all(ui.join("assets")).unwrap();
        std::fs::write(ui.join("index.html"), "<title>Sophia</title>").unwrap();
        std::fs::write(
            ui.join("assets/index-Bx3k9aZ1.js"),
            "console.log('organism')",
        )
        .unwrap();
        let app = crate::tauri_runtime::build_mock_app_for_tests(true);
        let self_host = SelfHost::from_settings(SelfHostSettings {
            ui_dir: Some(ui.display().to_string()),
            ..SelfHostSettings::default()
        })
        .unwrap()
        .with_public(
            Some(PublicMode::for_test(
                json!({ "graphs": ["uruguay-cows"], "siteDir": "site" }),
                profile,
            )),
            None,
        );
        let state = state(app, root, self_host);
        crate::app_runtime::async_runtime::block_on(async {
            let client = Client::new(serve(&state).await);
            let page = [("accept", "text/html")];
            assert_eq!(client.get("/", &page).await.1, "<title>cow-tools</title>");
            assert_eq!(client.get("/llms.txt", &[]).await.1, "# cow-tools\n");
            assert_eq!(
                client.get("/datasets/", &page).await.1,
                "<title>datasets</title>"
            );
            assert_eq!(client.get("/.well-known/ai-catalog.json", &[]).await.0, 200);
            assert_eq!(
                client.get("/.well-known/../index.html", &[]).await.0,
                200,
                "normalized by the client"
            );
            let (status, _) = client.get("/.git/config", &[]).await;
            assert_eq!(status, 404, "no other dot folder");
            // Content types an agent reads by.
            for (path, kind) in [
                ("/index.md", "text/markdown; charset=utf-8"),
                ("/datasets.jsonld", "application/ld+json"),
                ("/llms.txt", "text/plain; charset=utf-8"),
                ("/mcp-server-card.json", "application/json"),
            ] {
                let (status, headers, _) = client.send(reqwest::Method::GET, path, &[], None).await;
                assert_eq!(status, 200, "{path}");
                assert_eq!(
                    headers.get("content-type").and_then(|v| v.to_str().ok()),
                    Some(kind),
                    "{path}"
                );
            }
            // `/`: the page for a browser, its markdown twin for an agent's bare curl.
            let (_, headers, body) = client
                .send(
                    reqwest::Method::GET,
                    "/",
                    &[("accept", "text/html,*/*;q=0.8")],
                    None,
                )
                .await;
            assert_eq!(body, "<title>cow-tools</title>");
            assert_eq!(
                headers.get("vary").and_then(|v| v.to_str().ok()),
                Some("Accept")
            );
            let (_, headers, body) = client
                .send(reqwest::Method::GET, "/", &[("accept", "*/*")], None)
                .await;
            assert_eq!(body, "# cow-tools\n");
            assert_eq!(
                headers.get("content-type").and_then(|v| v.to_str().ok()),
                Some("text/markdown; charset=utf-8")
            );
            let (_, _, body) = client
                .send(
                    reqwest::Method::GET,
                    "/",
                    &[("accept", "text/markdown, text/html;q=0.5")],
                    None,
                )
                .await;
            assert_eq!(body, "# cow-tools\n");
            let (_, _, body) = client
                .send(
                    reqwest::Method::GET,
                    "/datasets/",
                    &[("accept", "*/*")],
                    None,
                )
                .await;
            assert_eq!(
                body, "<title>datasets</title>",
                "a page with no twin is the page"
            );
            // The MCP server card where SEP-2127 puts it, and /mcp named by it.
            let (status, card) = client.get("/mcp/server-card", &[]).await;
            assert_eq!(status, 200);
            assert_eq!(card["name"], "com.sophia-labs/cow-tools");
            let initialized = client.mcp(&[], "initialize", json!({})).await;
            assert_eq!(
                initialized["result"]["serverInfo"],
                json!({ "name": "com.sophia-labs/cow-tools", "version": "0.1.0", "title": "cow-tools" })
            );
            assert_eq!(client.get("/app/", &page).await.1, "<title>Sophia</title>");
            assert_eq!(
                client.get("/app/g/uruguay-cows/doc/x", &page).await.1,
                "<title>Sophia</title>",
                "the app's own routes"
            );
            assert_eq!(
                client.get("/app/assets/index-Bx3k9aZ1.js", &[]).await.0,
                200
            );
            assert_eq!(
                client.get("/assets/index-Bx3k9aZ1.js", &[]).await.0,
                200,
                "a bundle built for / still finds its assets"
            );
            let (status, missing) = client.get("/no-such-page", &page).await;
            assert_eq!(
                (status, missing.as_str().unwrap_or_default()),
                (404, "<title>no such cow</title>")
            );
            let (status, headers, _) = client.send(reqwest::Method::GET, "/app", &[], None).await;
            assert_eq!(status, 308);
            assert_eq!(
                headers.get("location").and_then(|v| v.to_str().ok()),
                Some("/app/")
            );
            assert_eq!(client.get("/../secret", &[]).await.0, 404);
        });
    });
}
