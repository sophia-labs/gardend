//! Direct writers on graphs under source authority (ruled 2026-10-06 12:05
//! PDT: fix it in this release; notes kept outside this repository).
//!
//! Once `{graph}/source-sync/ledger.json` exists, every ledger write rebuilds the
//! projections from the activation checkpoint. Before this change the rebuild
//! rewound everything written since activation by writers that bypass the
//! ledger: `sparql_update`, `rdf_load`, `revaluate`, the salience user-value,
//! valuation and configuration routes. These cases drive the REAL MCP dispatch
//! table and the REAL axum router through the signed single-graph cell boundary
//! (the technique of `files_rudiments_tests.rs`), on a disposable profile. No
//! mocks.
//!
//! Written red first, against `479b14d` (the Files branch head): every case
//! below that writes after activation fails there at its first post-rebuild
//! assertion; `inactive_graph_direct_writes_are_unchanged` and
//! `rebuild_keeps_ledger_authority_over_its_own_subjects` are guards that pass
//! there and must keep passing.
use super::*;
use axum::{body::Body, http::Request};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use hmac::{Hmac, Mac};
use serde_json::{json, Value};
use tower::ServiceExt;

const GRAPH: &str = "source-authority-direct-writes-20261006";
const OWNER: &str = "user:source-authority-owner-20261006";
const SECRET: &[u8] = b"source-authority-test-secret-at-least-32-bytes";
const TOKEN: &str = "source-authority-disposable-token";

/// A re-runnable assertion over the graph, run after each ledger writer.
type Check = std::pin::Pin<Box<dyn std::future::Future<Output = ()>>>;

fn user_rdf() -> String {
    format!("urn:mnemosyne:local:graph:{GRAPH}:user:rdf")
}

fn lease(role: &str) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let claims = json!({"iss":"pn-gateway","aud":"gardend-cell","sub":OWNER,"owner":OWNER,
        "graphId":GRAPH,"generation":1,"cellId":bound_cell_id(OWNER,GRAPH,1),"role":role,
        "policyRevision":1,"registryRevision":1,"sessionId":"source-authority-test","iat":now,"exp":now+600});
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
    })
}

/// One HTTP call through the real router and the signed cell boundary.
async fn send_json(
    state: &Arc<LoopbackState>,
    method: &str,
    path: &str,
    body: Value,
) -> (StatusCode, Value) {
    let request = Request::builder()
        .method(method)
        .uri(path)
        .header("authorization", format!("Bearer {TOKEN}"))
        .header("x-sophia-cell-lease", lease("editor"))
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let response = crate::loopback_router::loopback_router(state.clone())
        .oneshot(request)
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024 * 1024)
        .await
        .unwrap();
    let value = serde_json::from_slice(&bytes)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()));
    (status, value)
}

/// One MCP `tools/call` through the real runtime dispatch table.
async fn mcp(
    state: &Arc<LoopbackState>,
    tool: &str,
    arguments: Value,
) -> crate::app_error::AppResult<Value> {
    let context = crate::mcp_dispatch_registry::McpCallCtx {
        app: state.app.clone(),
        jobs: &state.jobs,
    };
    let entry =
        crate::mcp_dispatch_registry::lookup(tool).unwrap_or_else(|| panic!("no MCP tool {tool}"));
    (entry.handler)(&context, &arguments).await
}

/// Run one case in a disposable profile with the graph created.
fn with_profile<F, Fut>(name: &str, case: F)
where
    F: FnOnce(std::path::PathBuf) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let _serial = crate::tauri_runtime::profile_env_serial()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let root = std::env::temp_dir().join(format!("garden-{name}-{}", uuid::Uuid::new_v4()));
    let profile = root.join("profile");
    std::fs::create_dir_all(&profile).unwrap();
    std::env::set_var("GARDEN_PROFILE_DIR", &profile);
    let case_profile = profile.clone();
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap()
            .block_on(async move {
                let app = crate::tauri_runtime::build_mock_cell_app_for_tests(GRAPH, OWNER, 1);
                crate::graph_service::create_graph_service(
                    &app,
                    crate::graph_service::CreateGraphInput {
                        graph_id: Some(GRAPH.into()),
                        title: "Source authority direct writes fixture".into(),
                        description: None,
                        operation_id: None,
                    },
                )
                .unwrap();
                drop(app);
                case(case_profile).await;
            })
    }));
    std::env::remove_var("GARDEN_PROFILE_DIR");
    let _ = std::fs::remove_dir_all(&root);
    if let Err(error) = outcome {
        std::panic::resume_unwind(error);
    }
}

fn graph_dir_of(state: &Arc<LoopbackState>) -> std::path::PathBuf {
    crate::paths::existing_graph_dir(&state.app, GRAPH).unwrap()
}

fn ledger_path(graph_dir: &std::path::Path) -> std::path::PathBuf {
    graph_dir.join("source-sync").join("ledger.json")
}

fn ledger_text(state: &Arc<LoopbackState>) -> String {
    std::fs::read_to_string(ledger_path(&graph_dir_of(state))).unwrap()
}

/// Put the graph under source authority the way the deployed SPA does: its
/// background mirror's first `source_pull`. Returns the graph incarnation.
async fn activate(state: &Arc<LoopbackState>) -> String {
    let pulled = mcp(state, "source_pull", json!({"graphId": GRAPH}))
        .await
        .expect("source_pull activates the source ledger");
    assert!(crate::source_sync::source_authority_active(&state.app, GRAPH).unwrap());
    pulled["graphIncarnation"].as_str().unwrap().to_string()
}

/// Activate, then give the ledger its first operation, so that direct writes
/// from here on are recorded in the authored overlay. (While a ledger has
/// accepted no operation its checkpoint is re-taken by the next ledger write
/// instead: see `zero_operation_ledger_records_nothing_and_keeps_direct_writes`.)
async fn activate_with_an_operation(state: &Arc<LoopbackState>) -> String {
    let incarnation = activate(state).await;
    let pushed = mcp(
        state,
        "source_push",
        json!({"graphId": GRAPH, "graphIncarnation": incarnation, "operations": [
            {"kind": "graphMetadata", "operationId": "meta-first-operation", "title": "First operation"}
        ]}),
    )
    .await
    .expect("the ledger's first operation");
    assert_eq!(pushed["ok"], true, "{pushed}");
    incarnation
}

async fn sparql_update(state: &Arc<LoopbackState>, update: &str) -> Value {
    mcp(
        state,
        "sparql_update",
        json!({"graphId": GRAPH, "update": update}),
    )
    .await
    .unwrap_or_else(|error| panic!("sparql_update {update}: {error}"))
}

async fn ask(state: &Arc<LoopbackState>, query: &str) -> bool {
    let result = mcp(
        state,
        "sparql_query",
        json!({"graphId": GRAPH, "query": query}),
    )
    .await
    .unwrap_or_else(|error| panic!("sparql_query {query}: {error}"));
    result["boolean"]
        .as_bool()
        .unwrap_or_else(|| panic!("ASK returned no boolean: {result}"))
}

async fn select_rows(state: &Arc<LoopbackState>, query: &str) -> Vec<Value> {
    let result = mcp(
        state,
        "sparql_query",
        json!({"graphId": GRAPH, "query": query}),
    )
    .await
    .unwrap_or_else(|error| panic!("sparql_query {query}: {error}"));
    result["rows"].as_array().cloned().unwrap_or_default()
}

/// The shared value store file, as JSON.
fn commons_values(state: &Arc<LoopbackState>) -> Value {
    let path = graph_dir_of(state).join("values").join("block-values.json");
    serde_json::from_slice(
        &std::fs::read(&path).unwrap_or_else(|error| panic!("read {}: {error}", path.display())),
    )
    .unwrap()
}

fn sorted_nquads(graph_dir: &std::path::Path) -> Vec<String> {
    let store = crate::rdf_service::open_graph_store(graph_dir).unwrap();
    let dump = crate::rdf_query_service::dump_rdf_from_store(&store, "nquads", None, None).unwrap();
    let mut lines = dump.data.lines().map(str::to_string).collect::<Vec<_>>();
    lines.sort();
    lines
}

/// Every value-store file of the graph, by relative path.
fn value_files(graph_dir: &std::path::Path) -> std::collections::BTreeMap<String, Vec<u8>> {
    let mut files = std::collections::BTreeMap::new();
    let root = graph_dir.join("values");
    let mut pending = vec![root.clone()];
    while let Some(directory) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries {
            let path = entry.unwrap().path();
            if path.is_dir() {
                pending.push(path);
            } else {
                let relative = path
                    .strip_prefix(&root)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned();
                files.insert(relative, std::fs::read(&path).unwrap());
            }
        }
    }
    files
}

/// The three ledger writers other than a raw push, plus a raw push and an
/// explicit rebuild: each one restores the activation checkpoint and replays.
async fn run_every_ledger_writer(
    state: &Arc<LoopbackState>,
    incarnation: &str,
    tag: &str,
    check: impl Fn() -> Check,
) {
    let valued = mcp(
        state,
        "value",
        json!({"graphId": GRAPH, "document_id": "doc-direct", "block_id": format!("block-{tag}"), "importance": 3}),
    )
    .await
    .expect("value on a graph under source authority");
    assert!(
        valued.get("sourceSync").is_some(),
        "value did not route through the ledger: {valued}"
    );
    check().await;

    let written = mcp(
        state,
        "emporium_write",
        json!({"graphId": GRAPH, "vocab": "emporium-bookmark", "records": [{
            "kind": "Bookmark", "localId": format!("bm-{tag}"),
            "url": "https://example.test/direct", "title": format!("bookmark {tag}")
        }]}),
    )
    .await
    .expect("emporium_write on a graph under source authority");
    assert_eq!(written["ok"], true, "{written}");
    check().await;

    let pushed = mcp(
        state,
        "source_push",
        json!({"graphId": GRAPH, "graphIncarnation": incarnation, "operations": [
            {"kind": "graphMetadata", "operationId": format!("meta-{tag}"), "title": format!("Renamed {tag}")}
        ]}),
    )
    .await
    .expect("source_push");
    assert_eq!(pushed["ok"], true, "{pushed}");
    check().await;

    let rebuilt = mcp(
        state,
        "source_rebuild",
        json!({"graphId": GRAPH, "graphIncarnation": incarnation}),
    )
    .await
    .expect("source_rebuild");
    assert_eq!(
        rebuilt["projectionSetEqual"],
        true,
        "a rebuild did not reproduce the live projection: {}",
        json!({"before": rebuilt["projectionBefore"], "after": rebuilt["projectionAfter"],
               "second": rebuilt["projectionAfterSecondReplay"]})
    );
    check().await;
}

/// `sparql_update` after activation: inserts into `user:rdf` (the graph-less
/// DATA rewrite), into a named graph and into the default graph, and a delete
/// of a quad that predates activation, all survive `value`, `emporium_write`,
/// `source_push` and `source_rebuild`.
#[test]
fn sparql_update_after_activation_survives_every_ledger_writer() {
    with_profile("sa-sparql", |profile| async move {
        let state = state_for(&profile);
        let user = user_rdf();
        sparql_update(
            &state,
            "INSERT DATA { <urn:sa:floor> <urn:sa:p> \"in the checkpoint\" }",
        )
        .await;
        let incarnation = activate_with_an_operation(&state).await;

        sparql_update(
            &state,
            "INSERT DATA { <urn:sa:s> <urn:sa:p> \"after activation\" }",
        )
        .await;
        sparql_update(
            &state,
            "INSERT DATA { GRAPH <urn:sa:named> { <urn:sa:s2> <urn:sa:p> \"named\" } }",
        )
        .await;
        sparql_update(
            &state,
            "INSERT { <urn:sa:d> <urn:sa:p> \"default graph\" } WHERE {}",
        )
        .await;
        sparql_update(
            &state,
            "DELETE DATA { <urn:sa:floor> <urn:sa:p> \"in the checkpoint\" }",
        )
        .await;

        let probe = state.clone();
        let user_graph = user.clone();
        let check = move || {
            let state = probe.clone();
            let user = user_graph.clone();
            Box::pin(async move {
                assert!(
                    ask(&state, &format!("ASK {{ GRAPH <{user}> {{ <urn:sa:s> <urn:sa:p> \"after activation\" }} }}")).await,
                    "a sparql_update into user:rdf made after activation was rewound"
                );
                assert!(
                    ask(
                        &state,
                        "ASK { GRAPH <urn:sa:named> { <urn:sa:s2> <urn:sa:p> \"named\" } }"
                    )
                    .await,
                    "a sparql_update into a named graph made after activation was rewound"
                );
                assert!(
                    ask(&state, "ASK { <urn:sa:d> <urn:sa:p> \"default graph\" }").await,
                    "a sparql_update into the default graph made after activation was rewound"
                );
                assert!(
                    !ask(&state, &format!("ASK {{ GRAPH <{user}> {{ <urn:sa:floor> <urn:sa:p> \"in the checkpoint\" }} }}")).await,
                    "a quad deleted after activation came back from the checkpoint"
                );
            }) as Check
        };
        check().await;
        run_every_ledger_writer(&state, &incarnation, "sparql", check).await;
    });
}

/// `rdf_load` after activation (Turtle with an anonymous blank node, into
/// `user:rdf` and into a named target) survives every ledger writer, and the
/// blank node is neither lost nor duplicated by replay.
#[test]
fn rdf_load_after_activation_survives_every_ledger_writer() {
    with_profile("sa-rdf-load", |profile| async move {
        let state = state_for(&profile);
        let incarnation = activate_with_an_operation(&state).await;
        let loaded = mcp(
            &state,
            "rdf_load",
            json!({"graphId": GRAPH, "format": "turtle", "data":
                "@prefix sa: <urn:sa:> .\nsa:loaded sa:p \"loaded\" ; sa:q [ sa:r \"inner\" ] .\n"}),
        )
        .await
        .expect("rdf_load into user:rdf");
        assert_eq!(loaded["ok"], true, "{loaded}");
        mcp(
            &state,
            "rdf_load",
            json!({"graphId": GRAPH, "format": "turtle", "targetGraphIri": "urn:sa:loaded-graph",
                   "data": "<urn:sa:t> <urn:sa:p> \"target\" .\n"}),
        )
        .await
        .expect("rdf_load into a named target");

        let probe = state.clone();
        let check = move || {
            let state = probe.clone();
            Box::pin(async move {
                let user = user_rdf();
                assert!(
                    ask(
                        &state,
                        &format!(
                            "ASK {{ GRAPH <{user}> {{ <urn:sa:loaded> <urn:sa:p> \"loaded\" }} }}"
                        )
                    )
                    .await,
                    "an rdf_load made after activation was rewound"
                );
                let inner = select_rows(
                    &state,
                    &format!("SELECT ?b WHERE {{ GRAPH <{user}> {{ <urn:sa:loaded> <urn:sa:q> ?b . ?b <urn:sa:r> \"inner\" }} }}"),
                )
                .await;
                assert_eq!(
                    inner.len(),
                    1,
                    "the loaded blank node was lost or duplicated: {inner:?}"
                );
                assert!(
                    ask(
                        &state,
                        "ASK { GRAPH <urn:sa:loaded-graph> { <urn:sa:t> <urn:sa:p> \"target\" } }"
                    )
                    .await,
                    "an rdf_load into a named target made after activation was rewound"
                );
            }) as Check
        };
        check().await;
        run_every_ledger_writer(&state, &incarnation, "load", check).await;
    });
}

/// `revaluate` after activation (prompt, weights, and the configuration
/// history entry it archives) survives every ledger writer.
#[test]
fn revaluate_after_activation_survives_every_ledger_writer() {
    with_profile("sa-revaluate", |profile| async move {
        let state = state_for(&profile);
        let incarnation = activate_with_an_operation(&state).await;
        let revalued = mcp(
            &state,
            "revaluate",
            json!({"graphId": GRAPH, "importance_prompt": "direct-write importance prompt",
                   "weights": "importance_weight: 0.55"}),
        )
        .await
        .expect("revaluate");
        assert_eq!(revalued["success"], true, "{revalued}");

        let probe = state.clone();
        let check = move || {
            let state = probe.clone();
            Box::pin(async move {
                let values = commons_values(&state);
                assert_eq!(
                    values["config"]["importancePrompt"], "direct-write importance prompt",
                    "a revaluate made after activation was rewound: {}",
                    values["config"]
                );
                assert_eq!(
                    values["config"]["weights"]["importance_weight"], 0.55,
                    "{}",
                    values["config"]
                );
                assert_eq!(
                    values["configHistory"].as_array().map(Vec::len),
                    Some(1),
                    "the archived configuration was lost or duplicated: {}",
                    values["configHistory"]
                );
            }) as Check
        };
        check().await;
        run_every_ledger_writer(&state, &incarnation, "revaluate", check).await;
    });
}

/// The salience routes the web app uses, after activation: the user value
/// (`PUT .../user-value`), the configuration patch, and a valuation through
/// `POST .../blocks/value` (which used to skip the ledger) all survive every
/// ledger writer, together with a `value` event on the same block.
#[test]
fn salience_routes_after_activation_survive_every_ledger_writer() {
    with_profile("sa-salience-routes", |profile| async move {
        let state = state_for(&profile);
        let incarnation = activate_with_an_operation(&state).await;
        let (status, body) = send_json(
            &state,
            "PUT",
            &format!("/salience/{GRAPH}/blocks/user-value"),
            json!({"documentId": "doc-direct", "blockId": "block-user", "importance": 5, "valence": 4}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let (status, body) = send_json(
            &state,
            "PATCH",
            &format!("/salience/{GRAPH}/config"),
            json!({"valenceWeight": 0.33}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let (status, body) = send_json(
            &state,
            "POST",
            &format!("/salience/{GRAPH}/blocks/value"),
            json!({"valuations": [{"document_id": "doc-direct", "block_id": "block-user", "importance": 2}]}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");

        let probe = state.clone();
        let check = move || {
            let state = probe.clone();
            Box::pin(async move {
                let values = commons_values(&state);
                let block = &values["blocks"]["doc-direct:block-user"];
                assert_eq!(
                    block["userImportance"], 5.0,
                    "a user value set after activation was rewound: {block}"
                );
                assert_eq!(
                    block["userValence"], 4.0,
                    "a user value set after activation was rewound: {block}"
                );
                assert!(
                    block["rawImportanceSum"].as_f64().unwrap_or(0.0) >= 2.0,
                    "a valuation through POST /blocks/value made after activation was rewound: {block}"
                );
                assert_eq!(
                    values["config"]["weights"]["valence_weight"], 0.33,
                    "a configuration patch made after activation was rewound: {}",
                    values["config"]
                );
            }) as Check
        };
        check().await;
        run_every_ledger_writer(&state, &incarnation, "salience", check).await;

        // The `value` event of the ledger and the user value meet on one block:
        // both are kept, neither is applied twice.
        mcp(
            &state,
            "value",
            json!({"graphId": GRAPH, "document_id": "doc-direct", "block_id": "block-user", "importance": 3}),
        )
        .await
        .unwrap();
        mcp(
            &state,
            "source_rebuild",
            json!({"graphId": GRAPH, "graphIncarnation": incarnation}),
        )
        .await
        .unwrap();
        let values = commons_values(&state);
        let block = &values["blocks"]["doc-direct:block-user"];
        assert_eq!(block["userImportance"], 5.0, "{block}");
        assert_eq!(
            block["rawImportanceSum"], 5.0,
            "valuations replayed other than exactly once: {block}"
        );
    });
}

/// Replay is deterministic: two rebuilds in a row leave the store, the value
/// stores and the ledger byte-identical. A value rewritten many times costs
/// the ledger its last state only: superseded intermediate states leave
/// nothing behind.
#[test]
fn replay_is_deterministic_and_a_second_rebuild_changes_nothing() {
    with_profile("sa-determinism", |profile| async move {
        let state = state_for(&profile);
        let graph_dir = graph_dir_of(&state);
        let incarnation = activate_with_an_operation(&state).await;
        let user = user_rdf();
        for round in 0..30 {
            sparql_update(
                &state,
                &format!(
                    "DELETE {{ GRAPH <{user}> {{ <urn:sa:layout> <urn:sa:json> ?old }} }} \
                     INSERT {{ GRAPH <{user}> {{ <urn:sa:layout> <urn:sa:json> \"rewrite-{round}\" }} }} \
                     WHERE {{ OPTIONAL {{ GRAPH <{user}> {{ <urn:sa:layout> <urn:sa:json> ?old }} }} }}"
                ),
            )
            .await;
        }
        sparql_update(
            &state,
            "INSERT DATA { <urn:sa:keep> <urn:sa:p> _:b1 . _:b1 <urn:sa:r> \"blank\" }",
        )
        .await;
        sparql_update(
            &state,
            "INSERT DATA { <urn:sa:gone> <urn:sa:p> \"inserted then deleted\" }",
        )
        .await;
        sparql_update(
            &state,
            "DELETE DATA { <urn:sa:gone> <urn:sa:p> \"inserted then deleted\" }",
        )
        .await;
        mcp(
            &state,
            "revaluate",
            json!({"graphId": GRAPH, "valence_prompt": "determinism"}),
        )
        .await
        .unwrap();
        send_json(
            &state,
            "PUT",
            &format!("/salience/{GRAPH}/blocks/user-value"),
            json!({"documentId": "doc-direct", "blockId": "block-det", "importance": 3, "valence": 0}),
        )
        .await;

        let first = mcp(
            &state,
            "source_rebuild",
            json!({"graphId": GRAPH, "graphIncarnation": incarnation}),
        )
        .await
        .unwrap();
        assert_eq!(first["ok"], true, "{first}");
        let nquads = sorted_nquads(&graph_dir);
        let values = value_files(&graph_dir);
        let ledger = ledger_text(&state);
        assert!(
            nquads.iter().any(|line| line.contains("rewrite-29")),
            "the last rewrite did not survive the rebuild"
        );
        assert!(
            !nquads
                .iter()
                .any(|line| line.contains("rewrite-28") || line.contains("inserted then deleted")),
            "a superseded state came back"
        );

        let second = mcp(
            &state,
            "source_rebuild",
            json!({"graphId": GRAPH, "graphIncarnation": incarnation}),
        )
        .await
        .unwrap();
        assert_eq!(second["ok"], true, "{second}");
        assert_eq!(
            sorted_nquads(&graph_dir),
            nquads,
            "a second rebuild changed the store"
        );
        assert_eq!(
            value_files(&graph_dir),
            values,
            "a second rebuild changed the value stores"
        );
        assert_eq!(
            ledger_text(&state),
            ledger,
            "a second rebuild changed the ledger"
        );

        assert!(
            ledger.contains("rewrite-29"),
            "the ledger does not record the direct write"
        );
        for round in 0..29 {
            assert!(
                !ledger.contains(&format!("\\\"rewrite-{round}\\\"")),
                "the ledger kept the superseded state rewrite-{round}"
            );
        }
        assert!(
            !ledger.contains("inserted then deleted"),
            "the ledger kept a write that was undone"
        );
    });
}

/// A graph that is not under source authority behaves exactly as before: the
/// same writers write, nothing creates a ledger, and nothing is recorded.
#[test]
fn inactive_graph_direct_writes_are_unchanged() {
    with_profile("sa-inactive", |profile| async move {
        let state = state_for(&profile);
        let graph_dir = graph_dir_of(&state);
        let user = user_rdf();
        let updated =
            sparql_update(&state, "INSERT DATA { <urn:sa:i> <urn:sa:p> \"inactive\" }").await;
        assert_eq!(updated["ok"], true, "{updated}");
        assert!(updated["quadCount"].as_u64().unwrap_or(0) >= 1, "{updated}");
        let loaded = mcp(
            &state,
            "rdf_load",
            json!({"graphId": GRAPH, "format": "turtle", "data": "<urn:sa:il> <urn:sa:p> \"loaded\" .\n"}),
        )
        .await
        .unwrap();
        assert_eq!(loaded["ok"], true, "{loaded}");
        let revalued = mcp(
            &state,
            "revaluate",
            json!({"graphId": GRAPH, "importance_prompt": "inactive prompt"}),
        )
        .await
        .unwrap();
        assert_eq!(
            revalued["updated"],
            json!(["importance_prompt"]),
            "{revalued}"
        );
        let (status, user_value) = send_json(
            &state,
            "PUT",
            &format!("/salience/{GRAPH}/blocks/user-value"),
            json!({"documentId": "doc-i", "blockId": "block-i", "importance": 3, "valence": -4}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{user_value}");
        let (status, refused) = send_json(
            &state,
            "PUT",
            &format!("/salience/{GRAPH}/blocks/user-value"),
            json!({"documentId": "doc-i", "blockId": "block-i", "importance": 4}),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{refused}");
        let (status, body) = send_json(
            &state,
            "PATCH",
            &format!("/salience/{GRAPH}/config"),
            json!({"importanceWeight": 0.4}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let (status, valued) = send_json(
            &state,
            "POST",
            &format!("/salience/{GRAPH}/blocks/value"),
            json!({"valuations": [{"document_id": "doc-i", "block_id": "block-i", "importance": 2}]}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{valued}");
        assert!(
            valued.get("sourceSync").is_none(),
            "an inactive graph routed a valuation through the ledger: {valued}"
        );

        assert!(!crate::source_sync::source_authority_active(&state.app, GRAPH).unwrap());
        assert!(
            !graph_dir.join("source-sync").exists(),
            "a direct write created source-sync/ on an inactive graph"
        );
        assert!(
            ask(
                &state,
                &format!("ASK {{ GRAPH <{user}> {{ <urn:sa:i> <urn:sa:p> \"inactive\" }} }}")
            )
            .await
        );
        assert!(
            ask(
                &state,
                &format!("ASK {{ GRAPH <{user}> {{ <urn:sa:il> <urn:sa:p> \"loaded\" }} }}")
            )
            .await
        );
        let values = commons_values(&state);
        assert_eq!(values["config"]["importancePrompt"], "inactive prompt");
        assert_eq!(values["config"]["weights"]["importance_weight"], 0.4);
        assert_eq!(values["blocks"]["doc-i:block-i"]["userImportance"], 3.0);
        assert_eq!(values["blocks"]["doc-i:block-i"]["userValence"], -4.0);
        assert_eq!(values["blocks"]["doc-i:block-i"]["rawImportanceSum"], 2.0);
    });
}

/// The last line of defence: a write the fix does not route (here the
/// in-process SPARQL service and the user-value function, called without
/// their entry points), made on a graph already under source authority, is
/// kept by the next rebuild and recorded in the ledger, so a second rebuild
/// keeps it without any further adoption. This is also what keeps the direct
/// writes made between activation and the roll of this fix.
#[test]
fn rebuild_adopts_direct_writes_it_did_not_see() {
    with_profile("sa-adoption", |profile| async move {
        let state = state_for(&profile);
        let incarnation = activate_with_an_operation(&state).await;
        crate::rdf_service::run_sparql_update_service(
            state.app.clone(),
            crate::rdf_service::SparqlUpdateInput {
                graph_id: GRAPH.into(),
                update: "INSERT DATA { GRAPH <urn:sa:unrouted> { <urn:sa:u> <urn:sa:p> \"adopted-direct\" } }".into(),
            },
        )
        .unwrap();
        crate::salience_route_service::set_local_user_valuation(
            state.app.clone(),
            GRAPH,
            serde_json::from_value(
                json!({"documentId": "doc-u", "blockId": "block-u", "importance": 5}),
            )
            .unwrap(),
        )
        .unwrap();

        let pushed = mcp(
            &state,
            "source_push",
            json!({"graphId": GRAPH, "graphIncarnation": incarnation, "operations": [
                {"kind": "graphMetadata", "operationId": "meta-adoption", "title": "Adoption"}
            ]}),
        )
        .await
        .unwrap();
        assert_eq!(pushed["ok"], true, "{pushed}");
        assert!(
            ask(
                &state,
                "ASK { GRAPH <urn:sa:unrouted> { <urn:sa:u> <urn:sa:p> \"adopted-direct\" } }"
            )
            .await,
            "a rebuild erased a direct write it did not see"
        );
        assert_eq!(
            commons_values(&state)["blocks"]["doc-u:block-u"]["userImportance"],
            5.0
        );
        assert!(
            ledger_text(&state).contains("adopted-direct"),
            "the adopted write is not in the ledger"
        );

        let rebuilt = mcp(
            &state,
            "source_rebuild",
            json!({"graphId": GRAPH, "graphIncarnation": incarnation}),
        )
        .await
        .unwrap();
        assert_eq!(
            rebuilt["ok"], true,
            "a second rebuild still had to adopt: {rebuilt}"
        );
        assert!(
            ask(
                &state,
                "ASK { GRAPH <urn:sa:unrouted> { <urn:sa:u> <urn:sa:p> \"adopted-direct\" } }"
            )
            .await
        );
        assert_eq!(
            commons_values(&state)["blocks"]["doc-u:block-u"]["userImportance"],
            5.0
        );
    });
}

/// Adoption never overrides the ledger on the subjects the ledger itself
/// materializes: an object written with `emporium_write` and then altered
/// behind the ledger's back comes back as the ledger says.
#[test]
fn rebuild_keeps_ledger_authority_over_its_own_subjects() {
    with_profile("sa-owned", |profile| async move {
        let state = state_for(&profile);
        let incarnation = activate(&state).await;
        let written = mcp(
            &state,
            "emporium_write",
            json!({"graphId": GRAPH, "vocab": "emporium-bookmark", "records": [{
                "kind": "Bookmark", "localId": "bm-owned", "url": "https://example.test/owned", "title": "ledger title"
            }]}),
        )
        .await
        .unwrap();
        assert_eq!(written["ok"], true, "{written}");
        let rows = select_rows(
            &state,
            "SELECT ?g ?s ?p WHERE { GRAPH ?g { ?s ?p \"ledger title\" } }",
        )
        .await;
        assert!(!rows.is_empty(), "the bookmark title was not materialized");
        let term = |value: &Value| value.as_str().unwrap().to_string();
        let faces = rows
            .iter()
            .map(|row| (term(&row["g"]), term(&row["s"]), term(&row["p"])))
            .collect::<Vec<_>>();
        // Behind the ledger's back: straight on the store, past the SPARQL
        // authority gate (the bookmark sink is an engine projection graph).
        let store = crate::rdf_service::open_graph_store(&graph_dir_of(&state)).unwrap();
        for (g, s, p) in &faces {
            crate::rdf_query_service::execute_sparql_update(
                &store,
                &format!(
                    "DELETE DATA {{ GRAPH {g} {{ {s} {p} \"ledger title\" }} }} ; \
                     INSERT DATA {{ GRAPH {g} {{ {s} {p} \"tampered\" }} }}"
                ),
            )
            .unwrap();
        }
        drop(store);
        mcp(
            &state,
            "source_rebuild",
            json!({"graphId": GRAPH, "graphIncarnation": incarnation}),
        )
        .await
        .unwrap();
        for (g, s, p) in &faces {
            assert!(
                ask(
                    &state,
                    &format!("ASK {{ GRAPH {g} {{ {s} {p} \"ledger title\" }} }}")
                )
                .await,
                "the ledger's own object did not come back as the ledger says"
            );
        }
        assert!(
            select_rows(
                &state,
                "SELECT ?s WHERE { GRAPH ?g { ?s ?p \"tampered\" } }"
            )
            .await
            .is_empty(),
            "a rebuild adopted a change to a subject the ledger materializes"
        );
    });
}

/// The Phanes shape (`phanes-social`, `phanes-discord-source`: under source
/// authority since 09-10 with 0 ledger operations, written only through
/// `/api/sparql/update` in batches). A ledger that has accepted no operation
/// has replayed nothing over its checkpoint, so direct writes are not recorded
/// at all: the ledger stays byte-identical however many batches land, and
/// nothing is scanned. The next ledger write re-takes the checkpoint from the
/// live store before replaying, so every batch survives it, and the superseded
/// checkpoint files are removed.
#[test]
fn zero_operation_ledger_records_nothing_and_keeps_direct_writes() {
    with_profile("sa-zero-operations", |profile| async move {
        let state = state_for(&profile);
        let graph_dir = graph_dir_of(&state);
        let incarnation = activate(&state).await;
        let ledger_before = ledger_text(&state);
        let checkpoint_before: Value =
            serde_json::from_str::<Value>(&ledger_before).unwrap()["checkpoint"].clone();
        let old_rdf = graph_dir
            .join("source-sync")
            .join("checkpoints")
            .join(format!(
                "{}.nq",
                checkpoint_before["rdfDigest"].as_str().unwrap()
            ));
        assert!(old_rdf.is_file());

        // A batch rewrite: insert, then rewrite every message, batch by batch.
        for batch in 0..12 {
            let body = (0..20)
                .map(|message| format!("<urn:sa:msg:{batch}:{message}> <urn:sa:text> \"first {batch}-{message}\" ."))
                .collect::<Vec<_>>()
                .join("\n");
            sparql_update(
                &state,
                &format!("INSERT DATA {{ GRAPH <urn:sa:social> {{ {body} }} }}"),
            )
            .await;
        }
        for batch in 0..12 {
            sparql_update(
                &state,
                &format!(
                    "DELETE {{ GRAPH <urn:sa:social> {{ ?m <urn:sa:text> ?old }} }} \
                     INSERT {{ GRAPH <urn:sa:social> {{ ?m <urn:sa:text> ?new }} }} \
                     WHERE {{ GRAPH <urn:sa:social> {{ ?m <urn:sa:text> ?old }} \
                       FILTER(STRSTARTS(STR(?m), \"urn:sa:msg:{batch}:\")) \
                       BIND(CONCAT(\"redacted \", STR(?old)) AS ?new) }}"
                ),
            )
            .await;
        }
        assert_eq!(
            ledger_text(&state),
            ledger_before,
            "a direct write on a ledger with no operation was recorded"
        );

        // The first ledger operation re-takes the checkpoint and keeps every batch.
        let valued = mcp(
            &state,
            "value",
            json!({"graphId": GRAPH, "document_id": "doc-zero", "block_id": "block-zero", "importance": 3}),
        )
        .await
        .unwrap();
        assert!(valued.get("sourceSync").is_some(), "{valued}");
        let rows = select_rows(&state, "SELECT ?m WHERE { GRAPH <urn:sa:social> { ?m <urn:sa:text> ?t FILTER(STRSTARTS(?t, \"redacted first \")) } }").await;
        assert_eq!(
            rows.len(),
            240,
            "a batch written before the first ledger operation was rewound"
        );
        assert!(
            !ask(&state, "ASK { GRAPH <urn:sa:social> { ?m <urn:sa:text> ?t FILTER(STRSTARTS(?t, \"first \")) } }").await,
            "a rewritten message came back"
        );
        let ledger: Value = serde_json::from_str(&ledger_text(&state)).unwrap();
        assert!(
            ledger.get("authored").is_none(),
            "nothing should have been recorded: {}",
            ledger["authored"]
        );
        assert_ne!(
            ledger["checkpoint"]["rdfDigest"], checkpoint_before["rdfDigest"],
            "the checkpoint was not re-taken"
        );
        assert!(
            !old_rdf.exists(),
            "the superseded checkpoint was left behind"
        );

        // From the first operation on, direct writes are recorded, and a
        // rebuild reproduces everything without adopting.
        sparql_update(&state, "INSERT DATA { GRAPH <urn:sa:social> { <urn:sa:after> <urn:sa:text> \"after the first operation\" } }").await;
        assert!(ledger_text(&state).contains("after the first operation"));
        let rebuilt = mcp(
            &state,
            "source_rebuild",
            json!({"graphId": GRAPH, "graphIncarnation": incarnation}),
        )
        .await
        .unwrap();
        assert_eq!(rebuilt["ok"], true, "{rebuilt}");
        assert_eq!(
            select_rows(
                &state,
                "SELECT ?m WHERE { GRAPH <urn:sa:social> { ?m <urn:sa:text> ?t } }"
            )
            .await
            .len(),
            241
        );
    });
}
