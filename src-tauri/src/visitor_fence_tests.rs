//! W6 end to end: the real router over real HTTP, real profile and graphs,
//! the real MCP surface, the real CRDT executor, the real document and
//! workspace rooms driven by a y-websocket client, and Sophia accounts signed
//! by a locally made pool key. No mocks.
//!
//! Every attack runs twice: as the visitor (refused, and nothing changes) and
//! as a member token (it goes through), so a refusal is never a broken call.

use super::*;
use crate::{
    cell_graph_boundary::CellGraphBoundary,
    local_service_host::{
        child::{fast_timing, wait_service_healthy, write_service_manifest},
        LocalServiceHost,
    },
    loopback_self_host::{test_support::TestKey, SelfHost, SelfHostSettings},
    loopback_state::{LoopbackManifest, LoopbackState},
    public_mode::PublicMode,
};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::{net::SocketAddr, sync::Arc, time::Duration};
use tokio_tungstenite::tungstenite::{client::IntoClientRequest, Message as Ws};
use yrs::{
    sync::{Message as YMessage, SyncMessage},
    updates::{decoder::Decode, encoder::Encode},
    Doc, GetString, ReadTxn, Transact, Update, XmlElementPrelim, XmlFragment, XmlTextPrelim,
};

const MASTER: &str = "visitor-fence-test-master-token";
const ISSUER: &str = "https://cognito-idp.us-west-1.amazonaws.com/us-west-1_FenceTest";
const CLIENT: &str = "fence-app-client";
const GRAPH: &str = "guide";

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
    let root = std::env::temp_dir().join(format!("garden-fence-{name}-{}", uuid::Uuid::new_v4()));
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
    async fn send(
        &self,
        method: reqwest::Method,
        path: &str,
        bearer: Option<&str>,
        body: Option<Value>,
    ) -> (u16, Value) {
        let mut request = self.http.request(method, format!("{}{path}", self.base));
        if let Some(bearer) = bearer {
            request = request.header("authorization", format!("Bearer {bearer}"));
        }
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await.unwrap();
        let status = response.status().as_u16();
        let text = response.text().await.unwrap();
        (
            status,
            serde_json::from_str(&text).unwrap_or(Value::String(text)),
        )
    }

    async fn rpc(&self, bearer: Option<&str>, method: &str, params: Value) -> Value {
        let (status, body) = self
            .send(
                reqwest::Method::POST,
                "/mcp",
                bearer,
                Some(json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params })),
            )
            .await;
        assert_eq!(status, 200, "{body}");
        body
    }

    async fn tool(&self, bearer: Option<&str>, name: &str, arguments: Value) -> Value {
        self.rpc(
            bearer,
            "tools/call",
            json!({ "name": name, "arguments": arguments }),
        )
        .await
    }

    async fn ok(&self, bearer: Option<&str>, name: &str, arguments: Value) -> Value {
        let called = self.tool(bearer, name, arguments.clone()).await;
        assert!(
            called.get("error").is_none(),
            "{name} {arguments}: {called}"
        );
        called["result"]["structuredContent"].clone()
    }
}

struct Fixture {
    state: Arc<LoopbackState>,
    client: Client,
    alice: String,
    bob: String,
    carol: String,
    member: String,
    alice_doc: String,
    bob_doc: String,
}

fn config(visitor_folders: bool, quotas: Value) -> Value {
    let mut config = json!({
        "graphs": [GRAPH, "other"],
        // Rate limits are public mode's, tested there; these tests make many calls.
        "rateLimit": { "perMinute": 100000.0, "burst": 100000.0 },
        "accounts": { "cognito": { "issuer": ISSUER, "clientId": CLIENT } },
    });
    if visitor_folders {
        config["visitorFolders"] =
            json!([{ "graphId": GRAPH, "folderId": "moo", "quotas": quotas }]);
    }
    config
}

fn account_token(key: &TestKey, sub: &str, verified: bool) -> String {
    let now = jsonwebtoken::get_current_timestamp();
    key.sign(&json!({
        "sub": sub, "email": format!("{sub}@example.com"), "email_verified": verified,
        "token_use": "id", "aud": CLIENT, "iss": ISSUER, "exp": now + 3600, "iat": now,
    }))
}

/// A public Sophia with `guide` (folders `moo` (open to visitors) with
/// `moo-sub` under it, and `elsewhere`; the owner's `guide-doc` in
/// `elsewhere` and `owner-in-moo` in `moo`), a private `other`, accounts
/// alice and bob (verified) and carol (not), and a member token.
async fn fixture(root: &Path, profile: &Path, visitor_folders: bool, quotas: Value) -> Fixture {
    let app = crate::tauri_runtime::build_mock_app_for_tests(true);
    let self_host = SelfHost::from_settings(SelfHostSettings::default())
        .unwrap()
        .with_public(
            Some(PublicMode::for_test(
                config(visitor_folders, quotas),
                profile,
            )),
            Some("https://cow-tools.example.com"),
        );
    let state = state(app, root, self_host);
    state.app.manage(state.clone());
    let key = TestKey::generate("fence-key");
    state
        .self_host
        .public()
        .unwrap()
        .accounts()
        .unwrap()
        .install_keys(crate::loopback_self_host::test_support::jwks(&[&key]).as_bytes())
        .unwrap();
    let member = crate::loopback_client_token_service::create_client_token(
        &state.app,
        crate::loopback_client_token_types::CreateLoopbackClientTokenInput {
            label: Some("family member".into()),
            grant_profile_id: Some("member".into()),
            scopes: None,
            expires_in_days: Some(1),
        },
    )
    .unwrap()
    .token;
    let client = Client {
        base: serve(&state).await,
        http: reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap(),
    };
    let owner = Some(MASTER);
    for graph in [GRAPH, "other"] {
        client
            .ok(
                owner,
                "create_graph",
                json!({ "graphId": graph, "title": graph }),
            )
            .await;
    }
    client
        .ok(
            owner,
            "create_folder",
            json!({ "graphId": GRAPH, "folderId": "moo", "name": "Moo-vements of the Spirit" }),
        )
        .await;
    client
        .ok(
            owner,
            "create_folder",
            json!({ "graphId": GRAPH, "folderId": "moo-sub", "name": "Sub", "parentId": "moo" }),
        )
        .await;
    client
        .ok(
            owner,
            "create_folder",
            json!({ "graphId": GRAPH, "folderId": "elsewhere", "name": "Elsewhere" }),
        )
        .await;
    for (id, parent) in [("guide-doc", "elsewhere"), ("owner-in-moo", "moo")] {
        client
            .ok(
                owner,
                "create_document",
                json!({ "graphId": GRAPH, "documentId": id, "title": id, "parentId": parent }),
            )
            .await;
        client.ok(owner, "write_document", json!({ "graphId": GRAPH, "documentId": id, "content": format!("# {id}\n\nThe owner wrote this."), "format": "markdown" })).await;
    }
    client.ok(owner, "create_wires", json!({ "graphId": GRAPH, "wires": [{ "wireId": "owner-wire", "sourceDocumentId": "owner-in-moo", "targetDocumentId": "guide-doc", "predicate": "relatedTo" }] })).await;
    let alice = account_token(&key, "a11ce000-0000-4000-8000-000000000001", true);
    let bob = account_token(&key, "b0b00000-0000-4000-8000-000000000002", true);
    let carol = account_token(&key, "ca401000-0000-4000-8000-000000000003", false);
    let mut fixture = Fixture {
        state,
        client,
        alice,
        bob,
        carol,
        member,
        alice_doc: String::new(),
        bob_doc: String::new(),
    };
    if visitor_folders {
        fixture.alice_doc = fixture.make(&fixture.alice.clone(), "Alice's note").await;
        fixture.bob_doc = fixture.make(&fixture.bob.clone(), "Bob's note").await;
    }
    fixture
}

impl Fixture {
    async fn make(&self, who: &str, title: &str) -> String {
        let made = self
            .client
            .ok(
                Some(who),
                "create_document",
                json!({ "graphId": GRAPH, "title": title }),
            )
            .await;
        let id = made["documentId"].as_str().unwrap().to_string();
        self.client
            .ok(Some(who), "write_document", json!({ "graphId": GRAPH, "documentId": id, "content": format!("{title}: moo."), "format": "markdown" }))
            .await;
        id
    }

    /// What must not change when a visitor's write is refused: the tree, the
    /// owner's and the other visitor's documents, the wires, and Bob's comments.
    async fn oracle(&self) -> Value {
        let owner = Some(MASTER);
        let mut seen = json!({});
        let workspace = self
            .client
            .ok(owner, "get_workspace", json!({ "graphId": GRAPH }))
            .await;
        seen["workspace"] = strip_volatile(workspace);
        for id in [
            "guide-doc",
            "owner-in-moo",
            self.bob_doc.as_str(),
            self.alice_doc.as_str(),
        ] {
            if id.is_empty() {
                continue;
            }
            let document = self
                .client
                .tool(
                    owner,
                    "read_document",
                    json!({ "graphId": GRAPH, "documentId": id, "format": "markdown" }),
                )
                .await;
            seen[id] = strip_volatile(document["result"]["structuredContent"].clone());
            let wires = self
                .client
                .tool(
                    owner,
                    "get_wires",
                    json!({ "graphId": GRAPH, "documentId": id }),
                )
                .await;
            seen[format!("{id}:wires")] =
                strip_volatile(wires["result"]["structuredContent"].clone());
        }
        let graphs = self.client.ok(owner, "list_graphs", json!({})).await;
        seen["graphs"] = strip_volatile(graphs);
        let folders = self
            .client
            .send(
                reqwest::Method::GET,
                &format!("/navigation/{GRAPH}/folders"),
                owner,
                None,
            )
            .await
            .1;
        seen["folders"] = strip_volatile(folders);
        seen
    }

    fn account_id(&self, sub: &str) -> String {
        self.state
            .self_host
            .public()
            .unwrap()
            .visitor_folders()
            .unwrap()
            .account_id(sub)
    }
}

/// Times and counters that move with any read (caches, flush revisions).
fn strip_volatile(value: Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.into_iter()
                .filter(|(key, _)| {
                    !matches!(
                        key.as_str(),
                        "last_query_at"
                            | "lastQueryAt"
                            | "updated_at"
                            | "triple_count"
                            | "durability"
                            | "durabilityChecked"
                            | "durability_checked"
                            | "projectedAt"
                            | "generatedAt"
                            | "elapsedMs"
                            | "timing"
                            | "page"
                            | "revision"
                            | "rdfTripleCount"
                    )
                })
                .map(|(key, value)| (key, strip_volatile(value)))
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.into_iter().map(strip_volatile).collect()),
        other => other,
    }
}

fn refused(called: &Value) -> bool {
    called.get("error").is_some()
}

fn block_on<F: std::future::Future>(future: F) -> F::Output {
    crate::app_runtime::async_runtime::block_on(future)
}

/// A visitor makes, writes, renames, moves (inside), wires, comments on and
/// trashes its own documents in the visitors' folder; the server names them,
/// files them in the folder and stamps them as the account's.
#[test]
fn a_visitor_writes_its_own_documents_in_the_folder() {
    with_profile("own", |root, profile| {
        block_on(async {
            let f = fixture(root, profile, true, json!({})).await;
            let alice = Some(f.alice.as_str());
            let listed = f.client.rpc(alice, "tools/list", json!({})).await;
            let names: Vec<&str> = listed["result"]["tools"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|t| t["name"].as_str())
                .collect();
            for tool in [
                "create_document",
                "write_document",
                "move_documents",
                "create_wires",
                "edit_comment",
            ] {
                assert!(names.contains(&tool), "{tool} listed for a fenced account");
            }
            for tool in [
                "sparql_update",
                "crdt_operation",
                "create_restore_point",
                "source_push",
                "create_folder",
            ] {
                assert!(!names.contains(&tool), "{tool} never listed");
            }
            let fence = &listed["result"]["_meta"]["sophia.writeFence"][0];
            assert_eq!(fence["folderId"], "moo", "{listed}");
            let account = f.account_id("a11ce000-0000-4000-8000-000000000001");
            assert_eq!(fence["account"], json!(account));

            let id = f.alice_doc.clone();
            let workspace = f
                .client
                .ok(
                    Some(MASTER),
                    "get_workspace",
                    json!({ "graphId": GRAPH, "folderId": "moo", "depth": 4 }),
                )
                .await;
            let text = workspace.to_string();
            assert!(
                text.contains(&account),
                "the workspace entry names its account: {text}"
            );
            assert!(!text.contains("a11ce000"), "never the Cognito subject");
            assert!(!text.contains("example.com"), "never the email");

            f.client.ok(alice, "insert_blocks", json!({ "graphId": GRAPH, "documentId": id, "content": "A second paragraph.", "format": "markdown" })).await;
            f.client.ok(alice, "rename", json!({ "graphId": GRAPH, "entity_type": "document", "entity_id": id, "new_name": "Alice's cows" })).await;
            f.client
                .ok(
                    alice,
                    "move_documents",
                    json!({ "graphId": GRAPH, "documentIds": [id], "parentId": "moo-sub" }),
                )
                .await;
            f.client.ok(alice, "create_wires", json!({ "graphId": GRAPH, "wires": [{ "sourceDocumentId": id, "targetDocumentId": "guide-doc", "predicate": "supports" }] })).await;
            let comment = f.client.ok(alice, "edit_comment", json!({ "graphId": GRAPH, "documentId": id, "commentId": "c1", "action": "set", "text": "a note", "author": "Owner" })).await;
            assert_eq!(comment["comment"]["authorId"], json!(account), "{comment}");
            let name = comment["comment"]["author"].as_str().unwrap().to_string();
            assert!(name.starts_with("Visitor "), "the presence name: {comment}");
            assert_eq!(comment["comment"]["claimedAuthor"], "Owner");

            let (status, trashed) = f
                .client
                .send(
                    reqwest::Method::DELETE,
                    &format!("/navigation/{GRAPH}/documents/{id}"),
                    alice,
                    None,
                )
                .await;
            assert_eq!(status, 200, "{trashed}");
            // Sealed exam (C1): what is in the Trash is the owner's to see. The public (and the visitor, until it
            // restores it) no longer reads it by id, over HTTP, MCP, its room or SPARQL; the owner still does.
            assert_trash_is_the_owners(&f, &id, "A second paragraph").await;
            // Sealed exam (qa-mcp): nor does it wire from a document in the Trash.
            let wired = f.client.tool(alice, "create_wires", json!({ "graphId": GRAPH, "wires": [{ "sourceDocumentId": id, "targetDocumentId": "guide-doc", "predicate": "supports" }] })).await;
            assert!(
                refused(&wired) && wired.to_string().contains("Trash"),
                "{wired}"
            );
            let (status, restored) = f
                .client
                .send(
                    reqwest::Method::POST,
                    &format!("/navigation/{GRAPH}/trash/{id}/restore?kind=document"),
                    alice,
                    None,
                )
                .await;
            assert_eq!(status, 200, "{restored}");
            let read = f
                .client
                .ok(
                    alice,
                    "read_document",
                    json!({ "graphId": GRAPH, "documentId": id, "format": "markdown" }),
                )
                .await;
            assert!(read.to_string().contains("A second paragraph"), "{read}");
            let read = f
                .client
                .ok(
                    None,
                    "read_document",
                    json!({ "graphId": GRAPH, "documentId": id, "format": "markdown" }),
                )
                .await;
            assert!(
                read.to_string().contains("A second paragraph"),
                "restored, the public reads it again: {read}"
            );
            // The owner moderates a visitor's note by moving it to the Trash: gone from public reading at once.
            let bobs = f.bob_doc.clone();
            let (status, trashed) = f
                .client
                .send(
                    reqwest::Method::DELETE,
                    &format!("/navigation/{GRAPH}/documents/{bobs}"),
                    Some(MASTER),
                    None,
                )
                .await;
            assert_eq!(status, 200, "{trashed}");
            assert_trash_is_the_owners(&f, &bobs, "Bob's note: moo").await;
        });
    });
}

/// A document in the Trash, read by id as the public (anonymous and signed in)
/// over every read surface: refused or empty; the owner still reads it.
async fn assert_trash_is_the_owners(f: &Fixture, id: &str, marker: &str) {
    for who in [None, Some(f.alice.as_str()), Some(f.bob.as_str())] {
        for path in [
            format!("/documents/{GRAPH}/{id}"),
            format!("/documents/{GRAPH}/{id}/blocks"),
            format!("/documents/{GRAPH}/{id}/export?format=markdown"),
        ] {
            let (status, body) = f.client.send(reqwest::Method::GET, &path, who, None).await;
            assert_eq!(status, 404, "{path} as {}: {body}", who.is_some());
            assert!(!body.to_string().contains(marker), "{path}: {body}");
        }
        for (tool, arguments) in [
            (
                "read_document",
                json!({ "graphId": GRAPH, "documentId": id, "format": "markdown" }),
            ),
            ("read_blocks", json!({ "graphId": GRAPH, "documentId": id })),
            (
                "document_digest",
                json!({ "graphId": GRAPH, "documentId": id }),
            ),
            (
                "get_document_history",
                json!({ "graph_id": GRAPH, "document_id": id }),
            ),
        ] {
            let called = f.client.tool(who, tool, arguments).await;
            assert!(
                refused(&called) && !called.to_string().contains(marker),
                "{tool}: {called}"
            );
        }
        let query = format!(
            "SELECT ?text WHERE {{ GRAPH ?g {{ ?block <http://mnemosyne.dev/doc#textContent> ?text FILTER(CONTAINS(STR(?text), \"{}\")) }} }}",
            marker.split(':').next().unwrap()
        );
        let rows = f
            .client
            .tool(
                who,
                "sparql_query",
                json!({ "graphId": GRAPH, "query": query }),
            )
            .await;
        assert!(!rows.to_string().contains(marker), "sparql_query: {rows}");
        let protocol = who.map(|token| format!("bearer.{token}"));
        assert!(
            connect(
                &f.client.base,
                &format!("/hocuspocus/docs/{GRAPH}/{id}"),
                protocol.as_deref()
            )
            .await
            .is_err(),
            "its room stays closed to the public"
        );
    }
    let read = f
        .client
        .ok(
            Some(MASTER),
            "read_document",
            json!({ "graphId": GRAPH, "documentId": id, "format": "markdown" }),
        )
        .await;
    assert!(
        read.to_string().contains(marker),
        "the owner reads the Trash: {read}"
    );
}

/// Every attack is refused for the visitor and changes nothing; the same call
/// with a member token goes through (so the attack is real).
#[test]
fn the_fence_refuses_what_a_member_may_do() {
    with_profile("attacks", |root, profile| {
        block_on(async {
            let f = fixture(root, profile, true, json!({})).await;
            let (alice, member) = (f.alice.clone(), f.member.clone());
            let (mine, bobs) = (f.alice_doc.clone(), f.bob_doc.clone());
            let image = json!({"type":"doc","content":[{"type":"image","attrs":{"src":"https://tracker.example/p.gif"}}]});
            let query = json!({"type":"doc","content":[{"type":"queryBlock","attrs":{"query":"SELECT * WHERE { ?s ?p ?o }"}}]});
            let script = json!({"type":"doc","content":[{"type":"paragraph","content":[{"type":"text","text":"x","marks":[{"type":"link","attrs":{"href":"javascript:alert(1)"}}]}]}]});
            // (why, tool, arguments, a member may do it too)
            let attacks: Vec<(&str, &str, Value, bool)> = vec![
                (
                    "write the owner's document elsewhere",
                    "write_document",
                    json!({"graphId": GRAPH, "documentId": "guide-doc", "content": "defaced", "format": "markdown"}),
                    true,
                ),
                (
                    "write the owner's document in the folder",
                    "write_document",
                    json!({"graphId": GRAPH, "documentId": "owner-in-moo", "content": "defaced", "format": "markdown"}),
                    true,
                ),
                (
                    "write another visitor's document",
                    "write_document",
                    json!({"graphId": GRAPH, "documentId": bobs, "content": "defaced", "format": "markdown"}),
                    true,
                ),
                (
                    "insert into the owner's document",
                    "insert_blocks",
                    json!({"graphId": GRAPH, "documentId": "guide-doc", "content": "spam", "format": "markdown"}),
                    true,
                ),
                (
                    "comment on the owner's document",
                    "edit_comment",
                    json!({"graphId": GRAPH, "documentId": "owner-in-moo", "commentId": "x", "action": "set", "text": "spam"}),
                    true,
                ),
                (
                    "create with a chosen id",
                    "create_document",
                    json!({"graphId": GRAPH, "documentId": "chosen-id", "title": "x", "parentId": "moo"}),
                    true,
                ),
                (
                    "create outside the folder",
                    "create_document",
                    json!({"graphId": GRAPH, "title": "x", "parentId": "elsewhere"}),
                    true,
                ),
                (
                    "create in another graph",
                    "create_document",
                    json!({"graphId": "other", "title": "x"}),
                    true,
                ),
                (
                    "move my document out",
                    "move_documents",
                    json!({"graphId": GRAPH, "documentIds": [mine], "parentId": "elsewhere"}),
                    true,
                ),
                (
                    "move another visitor's document",
                    "move_documents",
                    json!({"graphId": GRAPH, "documentIds": [bobs], "parentId": "moo-sub"}),
                    true,
                ),
                (
                    "move the owner's document in",
                    "move_documents",
                    json!({"graphId": GRAPH, "documentIds": ["guide-doc"], "parentId": "moo"}),
                    true,
                ),
                (
                    "rename the graph",
                    "rename",
                    json!({"graphId": GRAPH, "entity_type": "graph", "new_name": "pwned"}),
                    true,
                ),
                (
                    "rename the folder",
                    "rename",
                    json!({"graphId": GRAPH, "entity_type": "folder", "entity_id": "moo", "new_name": "pwned"}),
                    true,
                ),
                (
                    "rename another visitor's document",
                    "rename",
                    json!({"graphId": GRAPH, "entity_type": "document", "entity_id": bobs, "new_name": "pwned"}),
                    true,
                ),
                (
                    "write my document out of the folder",
                    "write_document",
                    json!({"graphId": GRAPH, "documentId": mine, "content": "x", "format": "markdown", "parentId": "elsewhere"}),
                    true,
                ),
                (
                    "an image in my document",
                    "write_document",
                    json!({"graphId": GRAPH, "documentId": mine, "tiptapJson": image}),
                    true,
                ),
                (
                    "a query block in my document",
                    "write_document",
                    json!({"graphId": GRAPH, "documentId": mine, "tiptapJson": query}),
                    true,
                ),
                (
                    "a javascript link in my document",
                    "write_document",
                    json!({"graphId": GRAPH, "documentId": mine, "tiptapJson": script}),
                    true,
                ),
                (
                    "a wire from another visitor's document",
                    "create_wires",
                    json!({"graphId": GRAPH, "wires": [{"sourceDocumentId": bobs, "targetDocumentId": "guide-doc"}]}),
                    true,
                ),
                (
                    "take over the owner's wire by its id",
                    "create_wires",
                    json!({"graphId": GRAPH, "wires": [{"wireId": "owner-wire", "sourceDocumentId": mine, "targetDocumentId": "owner-in-moo"}]}),
                    true,
                ),
                (
                    "a wire with a foreign predicate",
                    "create_wires",
                    json!({"graphId": GRAPH, "wires": [{"sourceDocumentId": mine, "targetDocumentId": "guide-doc", "predicate": "http://www.w3.org/2002/07/owl#sameAs"}]}),
                    true,
                ),
                (
                    "delete a document",
                    "delete",
                    json!({"graphId": GRAPH, "type": "document", "document_id": mine}),
                    false,
                ),
                (
                    "delete the folder",
                    "delete",
                    json!({"graphId": GRAPH, "type": "folder", "folder_id": "moo"}),
                    false,
                ),
                (
                    "a raw workspace operation",
                    "crdt_operation",
                    json!({"graphId": GRAPH, "kind": "workspace.updateDocument", "documentId": "guide-doc", "payload": {"documentId": "guide-doc", "title": "pwned"}}),
                    true,
                ),
                (
                    "a raw trash flag",
                    "crdt_operation",
                    json!({"graphId": GRAPH, "kind": "workspace.updateDocument", "documentId": mine, "payload": {"documentId": mine, "trashed": true}}),
                    true,
                ),
                (
                    "make a folder",
                    "create_folder",
                    json!({"graphId": GRAPH, "name": "spam", "parentId": "moo"}),
                    true,
                ),
                (
                    "move the folder",
                    "move_folder",
                    json!({"graphId": GRAPH, "folderId": "moo-sub", "newParentId": "elsewhere"}),
                    true,
                ),
                (
                    "sparql update",
                    "sparql_update",
                    json!({"graphId": GRAPH, "sparql": "INSERT DATA { <urn:x> <urn:y> <urn:z> }"}),
                    false,
                ),
                (
                    "a restore point",
                    "create_restore_point",
                    json!({"graphId": GRAPH, "label": "x"}),
                    false,
                ),
                (
                    "source push",
                    "source_push",
                    json!({"graphId": GRAPH, "operations": []}),
                    false,
                ),
                ("flush", "flush_crdt", json!({"graphId": GRAPH}), true),
                (
                    "make editable",
                    "make_document_editable",
                    json!({"graphId": GRAPH, "documentId": "guide-doc"}),
                    true,
                ),
            ];
            let before = f.oracle().await;
            for (why, tool, arguments, _) in &attacks {
                let called = f.client.tool(Some(&alice), tool, arguments.clone()).await;
                assert!(
                    refused(&called),
                    "the visitor was not refused: {why}: {called}"
                );
                let after = f.oracle().await;
                assert_eq!(
                    before, after,
                    "the visitor's refused attack changed state: {why}"
                );
            }
            // The unverified account, and anonymous callers, write nothing at all.
            for who in [Some(f.carol.as_str()), None] {
                let called = f
                    .client
                    .tool(
                        who,
                        "create_document",
                        json!({"graphId": GRAPH, "title": "x"}),
                    )
                    .await;
                assert!(refused(&called), "{called}");
            }
            // REST: the owner's trash, a purge, a raw operation, a document PUT.
            for (method, path, body) in [
                (
                    reqwest::Method::DELETE,
                    format!("/navigation/{GRAPH}/documents/guide-doc"),
                    None,
                ),
                (
                    reqwest::Method::DELETE,
                    format!("/navigation/{GRAPH}/documents/{bobs}"),
                    None,
                ),
                (
                    reqwest::Method::POST,
                    format!("/navigation/{GRAPH}/trash/{mine}/restore"),
                    None,
                ),
                (
                    reqwest::Method::DELETE,
                    format!("/navigation/{GRAPH}/trash/{mine}?kind=document"),
                    None,
                ),
                (
                    reqwest::Method::POST,
                    "/api/crdt/operations".to_string(),
                    Some(
                        json!({"kind": "workspace.updateDocument", "graphId": GRAPH, "documentId": mine, "payload": {"documentId": mine, "title": "raw"}}),
                    ),
                ),
                (
                    reqwest::Method::PUT,
                    format!("/documents/{GRAPH}/{mine}"),
                    Some(json!({"title": "x", "blocks": []})),
                ),
                (
                    reqwest::Method::POST,
                    format!("/api/graphs/{GRAPH}/documents"),
                    Some(json!({"title": "x", "parentId": "moo"})),
                ),
            ] {
                let (status, body) = f
                    .client
                    .send(method.clone(), &path, Some(&alice), body)
                    .await;
                assert!(status >= 400, "{method} {path}: {status} {body}");
            }
            assert_eq!(
                before,
                f.oracle().await,
                "a refused REST call changed state"
            );
            // The same calls as a member go through: the attacks are real.
            for (why, tool, arguments, member_may) in &attacks {
                if !member_may {
                    continue;
                }
                let called = f.client.tool(Some(&member), tool, arguments.clone()).await;
                assert!(
                    !refused(&called),
                    "the member was refused, so the attack proves nothing: {why}: {called}"
                );
            }
        });
    });
}

/// The kill switch, a ban and the per-account quota stop a visitor's writes
/// at the next call; the switch's file and a ban are read without a restart.
#[test]
fn the_switch_a_ban_and_the_quota_stop_a_visitor() {
    with_profile("switch", |root, profile| {
        block_on(async {
            let f = fixture(root, profile, true, json!({ "documentsPerAccount": 2 })).await;
            let alice = Some(f.alice.as_str());
            let folders = f
                .state
                .self_host
                .public()
                .unwrap()
                .visitor_folders()
                .unwrap();
            let id = f.alice_doc.clone();
            let write = json!({"graphId": GRAPH, "documentId": id, "content": "still here", "format": "markdown"});
            f.client.ok(alice, "write_document", write.clone()).await;

            std::fs::create_dir_all(folders.state_dir()).unwrap();
            std::fs::write(folders.state_dir().join(KILL_SWITCH_FILE), "test").unwrap();
            folders.reread_switch();
            let called = f.client.tool(alice, "write_document", write.clone()).await;
            assert!(
                called["error"]["message"]
                    .as_str()
                    .unwrap()
                    .contains("paused"),
                "{called}"
            );
            std::fs::remove_file(folders.state_dir().join(KILL_SWITCH_FILE)).unwrap();
            folders.reread_switch();
            f.client.ok(alice, "write_document", write.clone()).await;

            let account = f.account_id("a11ce000-0000-4000-8000-000000000001");
            set_banned(folders.state_dir(), &account, true).unwrap();
            folders.reread_switch();
            let called = f.client.tool(alice, "write_document", write.clone()).await;
            assert!(refused(&called), "{called}");
            set_banned(folders.state_dir(), &account, false).unwrap();
            folders.reread_switch();

            // Alice has one document; the quota is two.
            f.client
                .ok(
                    alice,
                    "create_document",
                    json!({"graphId": GRAPH, "title": "second"}),
                )
                .await;
            let called = f
                .client
                .tool(
                    alice,
                    "create_document",
                    json!({"graphId": GRAPH, "title": "third"}),
                )
                .await;
            let message = called["error"]["message"].as_str().unwrap();
            assert!(message.contains("most an account"), "{called}");
            // Sealed exam (C2): documents in the Trash still count (a restore must not overflow the quota), so the
            // refusal does not send the visitor there.
            assert!(!message.contains("move one to the Trash"), "{called}");
            assert!(message.contains("Trash count"), "{called}");
        });
    });
}

/// A journaled operation that claims a fence is checked when it runs, not
/// trusted: one stamped for Alice that names Bob's document is refused by the
/// executor, and a caller's own copy of the server-only key is stripped.
#[test]
fn the_executor_checks_the_fence_and_strips_a_forged_one() {
    with_profile("executor", |root, profile| {
        block_on(async {
            let f = fixture(root, profile, true, json!({})).await;
            let folders = f
                .state
                .self_host
                .public()
                .unwrap()
                .visitor_folders()
                .unwrap();
            let account = folders.account_id("a11ce000-0000-4000-8000-000000000001");
            let fence = WriteFence {
                graph_id: GRAPH.into(),
                root_folder_id: "moo".into(),
                principal: "cognito:a11ce000-0000-4000-8000-000000000001".into(),
                account: account.clone(),
                name: "Visitor".into(),
                color: "#16a34a".into(),
                quotas: Quotas::default(),
            };
            let operation = crate::crdt_operation_types::CrdtOperation {
                operation_id: "op-forged".into(),
                kind: "workspace.updateDocument".into(),
                graph_id: GRAPH.into(),
                document_id: Some(f.bob_doc.clone()),
                payload: json!({ "documentId": f.bob_doc, "title": "pwned", "updatedAt": 1, VISITOR_FENCE_PAYLOAD_KEY: fence }),
                enqueue_timestamp: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_millis()
                    .to_string(),
            };
            let refused =
                crate::crdt_engine::executor::apply_operation(&f.state.app, &operation).await;
            assert!(refused.unwrap_err().contains("not yours"));

            // A member's raw create that carries a fence of its own: stripped,
            // so the document is the member's, not stamped as Alice's.
            let made = f.client.ok(Some(&f.member), "crdt_operation", json!({
                "graphId": GRAPH, "kind": "workspace.createDocument",
                "payload": { "title": "member doc", "parentId": "moo", VISITOR_FENCE_PAYLOAD_KEY: fence }
            })).await;
            let workspace = f
                .client
                .ok(
                    Some(MASTER),
                    "get_workspace",
                    json!({ "graphId": GRAPH, "folderId": "moo", "depth": 4 }),
                )
                .await;
            let id = made["documentId"].as_str().unwrap();
            let row = find_row(&workspace, id).expect("the member's document is in the folder");
            assert!(
                row.get(CREATED_BY_FIELD).is_none(),
                "a forged fence never stamps: {row}"
            );
            assert!(
                row.get("extra")
                    .and_then(|extra| extra.get(CREATED_BY_FIELD))
                    .is_none(),
                "a forged fence never stamps: {row}"
            );
        });
    });
}

fn find_row(value: &Value, id: &str) -> Option<Value> {
    match value {
        Value::Object(map) => {
            if map.get("id").and_then(Value::as_str) == Some(id)
                || map.get("documentId").and_then(Value::as_str) == Some(id)
            {
                return Some(value.clone());
            }
            map.values().find_map(|child| find_row(child, id))
        }
        Value::Array(items) => items.iter().find_map(|child| find_row(child, id)),
        _ => None,
    }
}

// ─── Rooms ───────────────────────────────────────────────────────────────────

type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn connect(base: &str, path: &str, protocol: Option<&str>) -> Result<Socket, String> {
    let url = format!("{}{path}", base.replacen("http://", "ws://", 1));
    let mut request = url
        .into_client_request()
        .map_err(|error| error.to_string())?;
    if let Some(protocol) = protocol {
        request
            .headers_mut()
            .insert("sec-websocket-protocol", protocol.parse().unwrap());
    }
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

async fn room_text(base: &str, graph: &str, document: &str) -> String {
    let mut socket = connect(
        base,
        &format!("/hocuspocus/docs/{graph}/{document}"),
        Some(&format!("bearer.{MASTER}")),
    )
    .await
    .unwrap();
    let doc = Doc::new();
    sync(&mut socket, &doc).await;
    let fragment = doc.get_or_insert_xml_fragment("content");
    let txn = doc.transact();
    fragment.get_string(&txn)
}

/// The rooms: a visitor's frames land only in its own documents inside the
/// fence and pass the content policy; the workspace room is read-only to
/// every visitor (one raw update could rewrite the tree), and an anonymous
/// visitor reads both. The owner's same raw update goes through.
#[test]
fn the_rooms_take_a_visitors_frames_only_for_its_own_documents() {
    with_profile("rooms", |root, profile| {
        block_on(async {
            let f = fixture(root, profile, true, json!({})).await;
            let base = f.client.base.clone();
            let alice = format!("bearer.{}", f.alice);
            let graphs = f.client.ok(Some(MASTER), "list_graphs", json!({})).await;
            let graph = graphs["graphs"]
                .as_array()
                .unwrap()
                .iter()
                .find(|graph| graph["graph_id"] == GRAPH)
                .unwrap()
                .clone();
            let incarnation = graph["graph_incarnation"]
                .as_str()
                .or_else(|| graph["graphIncarnation"].as_str())
                .unwrap()
                .to_string();
            let workspace_path =
                format!("/hocuspocus/workspace/{GRAPH}?graph_incarnation={incarnation}");
            let before = f.oracle().await;

            // The workspace room: read-only for an anonymous visitor and for Alice.
            for protocol in [None, Some(alice.as_str())] {
                let mut socket = connect(&base, &workspace_path, protocol).await.unwrap();
                let doc = Doc::new();
                sync(&mut socket, &doc).await;
                assert!(
                    doc.transact().get_map("folders").is_some(),
                    "a visitor reads the tree"
                );
                let closed = send_change(&mut socket, &doc, |doc| {
                    let folders = doc.get_or_insert_map("folders");
                    let mut txn = doc.transact_mut();
                    if let Some(yrs::Out::YMap(moo)) = folders.get(&txn, "moo") {
                        moo.insert(&mut txn, "name", "RENAMED BY A VISITOR");
                    }
                })
                .await;
                assert_eq!(
                    closed,
                    Some(crate::crdt_engine::rooms::VISITOR_REFUSED_CLOSE),
                    "{protocol:?}"
                );
            }
            assert_eq!(
                before,
                f.oracle().await,
                "a refused workspace frame changed the tree"
            );

            // Alice's own document: her paragraph lands.
            let mut socket = connect(
                &base,
                &format!("/hocuspocus/docs/{GRAPH}/{}", f.alice_doc),
                Some(&alice),
            )
            .await
            .unwrap();
            let doc = Doc::new();
            sync(&mut socket, &doc).await;
            assert_eq!(
                send_change(&mut socket, &doc, |doc| paragraph(doc, "typed by alice")).await,
                None
            );
            // An image, an unknown part, a comment signed as the owner: refused.
            let closed = send_change(&mut socket, &doc, |doc| {
                let fragment = doc.get_or_insert_xml_fragment("content");
                let mut txn = doc.transact_mut();
                fragment.push_back(&mut txn, XmlElementPrelim::empty("image"));
            })
            .await;
            assert_eq!(
                closed,
                Some(crate::crdt_engine::rooms::VISITOR_REFUSED_CLOSE)
            );
            assert!(room_text(&base, GRAPH, &f.alice_doc)
                .await
                .contains("typed by alice"));
            for change in [
                Box::new(|doc: &Doc| {
                    doc.get_or_insert_map("evil-root")
                        .insert(&mut doc.transact_mut(), "x", "y");
                }) as Box<dyn FnOnce(&Doc)>,
                Box::new(|doc: &Doc| {
                    let comments = doc.get_or_insert_map("comments");
                    let mut txn = doc.transact_mut();
                    comments.insert(
                        &mut txn,
                        "forged",
                        yrs::any!({ "text": "hi", "author": "Owner", "authorId": "owner" }),
                    );
                }),
                // Sealed exam (qa-rooms): unsigned, or signed "You"/"local", is not signed by the account.
                Box::new(|doc: &Doc| {
                    let comments = doc.get_or_insert_map("comments");
                    let mut txn = doc.transact_mut();
                    comments.insert(&mut txn, "unsigned", yrs::any!({ "text": "who wrote me" }));
                }),
                Box::new(|doc: &Doc| {
                    let comments = doc.get_or_insert_map("comments");
                    let mut txn = doc.transact_mut();
                    comments.insert(
                        &mut txn,
                        "local",
                        yrs::any!({ "text": "hi", "author": "You", "authorId": "local" }),
                    );
                }),
                // Sealed exam, round 2: the document's metadata and the operations' replay ledger are the server's.
                Box::new(|doc: &Doc| {
                    doc.get_or_insert_map("metadata").insert(
                        &mut doc.transact_mut(),
                        "createdBy",
                        "owner",
                    );
                }),
                Box::new(|doc: &Doc| {
                    let ledger = doc.get_or_insert_map("_operationLog");
                    let mut txn = doc.transact_mut();
                    ledger.insert(&mut txn, "fake-server-operation", yrs::any!({ "v": 1, "op": "block.update", "payloadHash": "visitor-chosen" }));
                }),
            ] {
                let mut socket = connect(
                    &base,
                    &format!("/hocuspocus/docs/{GRAPH}/{}", f.alice_doc),
                    Some(&alice),
                )
                .await
                .unwrap();
                let doc = Doc::new();
                sync(&mut socket, &doc).await;
                assert_eq!(
                    send_change(&mut socket, &doc, change).await,
                    Some(crate::crdt_engine::rooms::VISITOR_REFUSED_CLOSE)
                );
            }

            // Signed as herself (her account and her bound name): it lands.
            let account = f.account_id("a11ce000-0000-4000-8000-000000000001");
            let name = f
                .state
                .self_host
                .public()
                .unwrap()
                .visitor_folders()
                .unwrap()
                .presence_of(&account)
                .name;
            let mut socket = connect(
                &base,
                &format!("/hocuspocus/docs/{GRAPH}/{}", f.alice_doc),
                Some(&alice),
            )
            .await
            .unwrap();
            let doc = Doc::new();
            sync(&mut socket, &doc).await;
            let signed = (account.clone(), name.clone());
            let closed = send_change(&mut socket, &doc, move |doc| {
                let comments = doc.get_or_insert_map("comments");
                let mut txn = doc.transact_mut();
                comments.insert(&mut txn, "signed", yrs::any!({ "text": "mine", "author": signed.1.as_str(), "authorId": signed.0.as_str() }));
            })
            .await;
            assert_eq!(closed, None, "a comment signed as herself lands");

            // Not hers (inside and outside the folder): refused, and unchanged.
            for document in ["guide-doc", "owner-in-moo", f.bob_doc.as_str()] {
                let mut socket = connect(
                    &base,
                    &format!("/hocuspocus/docs/{GRAPH}/{document}"),
                    Some(&alice),
                )
                .await
                .unwrap();
                let doc = Doc::new();
                sync(&mut socket, &doc).await;
                let closed =
                    send_change(&mut socket, &doc, |doc| paragraph(doc, "defaced by alice")).await;
                assert_eq!(
                    closed,
                    Some(crate::crdt_engine::rooms::VISITOR_REFUSED_CLOSE),
                    "{document}"
                );
                assert!(
                    !room_text(&base, GRAPH, document).await.contains("defaced"),
                    "{document}"
                );
            }
            // Anonymous: reads a document, cannot write it.
            let mut socket = connect(&base, &format!("/hocuspocus/docs/{GRAPH}/guide-doc"), None)
                .await
                .unwrap();
            let doc = Doc::new();
            sync(&mut socket, &doc).await;
            assert_eq!(
                send_change(&mut socket, &doc, |doc| paragraph(doc, "anon")).await,
                Some(crate::crdt_engine::rooms::VISITOR_REFUSED_CLOSE)
            );
            // A private graph's rooms stay closed to visitors; a wrong key stays 401.
            assert!(connect(&base, "/hocuspocus/docs/other/guide-doc", None)
                .await
                .is_err());
            assert!(connect(&base, &workspace_path, Some("bearer.not-the-key"))
                .await
                .is_err());

            // The owner's same raw tree update goes through (the attack is real).
            let mut socket = connect(&base, &workspace_path, Some(&format!("bearer.{MASTER}")))
                .await
                .unwrap();
            let doc = Doc::new();
            sync(&mut socket, &doc).await;
            let closed = send_change(&mut socket, &doc, |doc| {
                let folders = doc.get_or_insert_map("folders");
                let mut txn = doc.transact_mut();
                if let Some(yrs::Out::YMap(moo)) = folders.get(&txn, "elsewhere") {
                    moo.insert(&mut txn, "name", "Renamed by the owner");
                }
            })
            .await;
            assert_eq!(closed, None);
        });
    });
}

/// A visitor's presence is its account: whatever id and name it publishes,
/// the room broadcasts the account's id, a name that is not the house's, and
/// `visitor: true`; an anonymous visitor's presence is dropped.
#[test]
fn a_visitors_presence_is_stamped_with_its_account() {
    with_profile("presence", |root, profile| {
        block_on(async {
            let f = fixture(root, profile, true, json!({})).await;
            let base = f.client.base.clone();
            let path = format!("/hocuspocus/docs/{GRAPH}/{}", f.alice_doc);
            let mut owner = connect(&base, &path, Some(&format!("bearer.{MASTER}")))
                .await
                .unwrap();
            sync(&mut owner, &Doc::new()).await;
            let mut alice = connect(&base, &path, Some(&format!("bearer.{}", f.alice)))
                .await
                .unwrap();
            sync(&mut alice, &Doc::new()).await;
            let forged =
                json!({ "user": { "userId": "owner", "name": "Owner", "color": "#123456" } })
                    .to_string();
            let mut clients = std::collections::HashMap::new();
            clients.insert(
                yrs::block::ClientID::new(4242),
                yrs::sync::awareness::AwarenessUpdateEntry {
                    clock: 1,
                    json: forged.into(),
                },
            );
            let message = YMessage::Awareness(yrs::sync::AwarenessUpdate { clients }).encode_v1();
            alice.send(Ws::Binary(message.into())).await.unwrap();
            let account = f.account_id("a11ce000-0000-4000-8000-000000000001");
            let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
            let mut seen = None;
            while tokio::time::Instant::now() < deadline && seen.is_none() {
                if let Ok(Some(Ok(Ws::Binary(data)))) =
                    tokio::time::timeout(Duration::from_millis(300), owner.next()).await
                {
                    if let Ok(YMessage::Awareness(update)) = YMessage::decode_v1(&data) {
                        if let Some(entry) = update.clients.get(&yrs::block::ClientID::new(4242)) {
                            seen = Some(serde_json::from_str::<Value>(&entry.json).unwrap());
                        }
                    }
                }
            }
            let seen = seen.expect("the owner sees alice's presence");
            assert_eq!(seen["user"]["userId"], json!(account), "{seen}");
            assert_ne!(seen["user"]["name"], "Owner");
            assert_eq!(seen["user"]["visitor"], true);

            // Sealed exam (qa-rooms): Bob cannot take over Alice's live client id with a higher clock.
            let mut bob = connect(&base, &path, Some(&format!("bearer.{}", f.bob)))
                .await
                .unwrap();
            sync(&mut bob, &Doc::new()).await;
            let mut clients = std::collections::HashMap::new();
            clients.insert(
                yrs::block::ClientID::new(4242),
                yrs::sync::awareness::AwarenessUpdateEntry {
                    clock: 999,
                    json: json!({ "user": { "name": "Bob was here" } })
                        .to_string()
                        .into(),
                },
            );
            bob.send(Ws::Binary(
                YMessage::Awareness(yrs::sync::AwarenessUpdate { clients })
                    .encode_v1()
                    .into(),
            ))
            .await
            .unwrap();
            let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
            while tokio::time::Instant::now() < deadline {
                if let Ok(Some(Ok(Ws::Binary(data)))) =
                    tokio::time::timeout(Duration::from_millis(300), owner.next()).await
                {
                    if let Ok(YMessage::Awareness(update)) = YMessage::decode_v1(&data) {
                        if let Some(entry) = update.clients.get(&yrs::block::ClientID::new(4242)) {
                            panic!("bob's frame replaced alice's presence: {}", entry.json);
                        }
                    }
                }
            }
        });
    });
}

/// Without `visitorFolders` nothing changes: an account is a viewer exactly as
/// public mode made it, and the owner and a member write as before.
#[test]
fn without_a_visitor_folder_the_fence_is_inert() {
    with_profile("inert", |root, profile| {
        block_on(async {
            let f = fixture(root, profile, false, json!({})).await;
            let called = f
                .client
                .tool(
                    Some(&f.alice),
                    "create_document",
                    json!({ "graphId": GRAPH, "title": "x", "parentId": "moo" }),
                )
                .await;
            assert_eq!(called["error"]["code"], -32003, "{called}");
            assert!(called["error"]["message"]
                .as_str()
                .unwrap()
                .contains("not open to visitors"));
            let listed = f.client.rpc(Some(&f.alice), "tools/list", json!({})).await;
            assert!(listed["result"]["_meta"].get("sophia.writeFence").is_none());
            f.client.ok(Some(&f.member), "write_document", json!({ "graphId": GRAPH, "documentId": "guide-doc", "content": "member edit", "format": "markdown" })).await;
            f.client
                .ok(
                    Some(MASTER),
                    "create_document",
                    json!({ "graphId": GRAPH, "title": "owner doc", "parentId": "moo" }),
                )
                .await;
        });
    });
}

/// Every MCP tool and every writing REST route, called by a fenced visitor
/// with arguments aimed at what is not hers, leaves everything as it was: a
/// tool or route added later is walked here without anyone listing it.
#[test]
fn every_tool_and_route_leaves_what_is_not_hers_alone() {
    with_profile("walker", |root, profile| {
        block_on(async {
            let f = fixture(root, profile, true, json!({ "writesPerMinute": 100000.0 })).await;
            let alice = f.alice.clone();
            let bobs = f.bob_doc.clone();
            let catalog: Value =
                serde_json::from_str(include_str!("mcp_tool_catalog.json")).unwrap();
            let arguments = json!({
                "graphId": GRAPH, "graph_id": GRAPH, "documentId": bobs, "document_id": bobs,
                "entity_type": "document", "entity_id": bobs, "new_name": "pwned", "title": "pwned",
                "content": "pwned", "format": "markdown", "folderId": "moo", "folder_id": "moo",
                "parentId": "moo", "newParentId": "elsewhere", "documentIds": [bobs, "guide-doc"],
                "type": "document", "kind": "workspace.updateDocument",
                "payload": { "documentId": "guide-doc", "title": "pwned" },
                "sparql": "INSERT DATA { <urn:a> <urn:b> <urn:c> }", "query": "INSERT DATA { <urn:a> <urn:b> <urn:c> }",
                "wires": [{ "sourceDocumentId": bobs, "targetDocumentId": "guide-doc" }],
                "sourceDocumentId": bobs, "targetDocumentId": "guide-doc",
                "commentId": "c9", "comment_id": "c9", "action": "set", "text": "pwned",
                "blocks": [{ "type": "paragraph", "content": "pwned" }], "operations": [],
                "label": "pwned", "memories": [], "records": [], "name": "pwned",
            });
            let before = f.oracle().await;
            let mut walked = 0;
            for tool in catalog["tools"].as_array().unwrap() {
                let name = tool["name"].as_str().unwrap();
                let _ = f.client.tool(Some(&alice), name, arguments.clone()).await;
                walked += 1;
                assert_eq!(
                    before,
                    f.oracle().await,
                    "{name} changed what is not the visitor's"
                );
            }
            assert!(walked >= 100, "the whole catalog: {walked}");
            let surface: Value =
                serde_json::from_str(include_str!("../../parity/local-loopback-surface.json"))
                    .unwrap();
            for route in surface["routes"].as_array().unwrap() {
                let method = route["method"].as_str().unwrap();
                if method == "GET" {
                    continue;
                }
                let path = route["path"]
                    .as_str()
                    .unwrap()
                    .replace("{graph_id}", GRAPH)
                    .replace("{document_id}", &bobs)
                    .replace("{doc_id}", &bobs)
                    .replace("{artifact_id}", &bobs)
                    .replace("{folder_id}", "moo")
                    .replace("{entity_type}", "document")
                    .replace("{entity_id}", &bobs)
                    .replace("{wire_id}", "w-1")
                    .replace("{job_id}", "job-1")
                    .replace("{snapshot_id}", "s-1")
                    .replace("{restore_point_id}", "rp-1")
                    .replace("{operation_id}", "op-1")
                    .replace("{revision_id}", "r-1")
                    .replace("{service_id}", "vehicle")
                    .replace("{vocab}", "v")
                    .replace("{class}", "c")
                    .replace("{address}", "a")
                    .replace("{name}", "n")
                    .replace("{version}", "1");
                let method = reqwest::Method::from_bytes(method.as_bytes()).unwrap();
                let (status, body) = f
                    .client
                    .send(method.clone(), &path, Some(&alice), Some(arguments.clone()))
                    .await;
                if path == "/mcp" {
                    continue;
                }
                assert_eq!(
                    before,
                    f.oracle().await,
                    "{method} {path} changed what is not the visitor's ({status} {body})"
                );
            }
        });
    });
}

/// A visitor's agent (a service it reaches through the proxy, here the test
/// binary as an echo service) gets the visitor's fence and nothing more: its
/// delegation writes the visitor's own notes in the folder, as the visitor
/// via the service, and is refused everything else. An account with no fence
/// (an unverified email) hears of none and writes nothing.
#[test]
fn a_visitors_agent_writes_inside_its_fence_only() {
    with_profile("agent", |root, profile| {
        let app = crate::tauri_runtime::build_mock_app_for_tests(true);
        let mut config = config(true, json!({}));
        config["services"] = json!(["vehicle"]);
        let self_host = SelfHost::from_settings(SelfHostSettings::default())
            .unwrap()
            .with_public(
                Some(PublicMode::for_test(config, profile)),
                Some("https://cow-tools.example.com"),
            );
        let state = state(app, root, self_host);
        state.app.manage(state.clone());
        let key = TestKey::generate("fence-key");
        state
            .self_host
            .public()
            .unwrap()
            .accounts()
            .unwrap()
            .install_keys(crate::loopback_self_host::test_support::jwks(&[&key]).as_bytes())
            .unwrap();
        let dir = root.join("services");
        write_service_manifest(
            &dir.join("vehicle"),
            "vehicle",
            &[],
            json!({ "memberAccess": "all", "delegateCaller": true }),
        );
        state.services.pin_services_dir(dir);
        state.services.set_timing(fast_timing());
        state
            .services
            .set_local_api_url("http://127.0.0.1:9".into());
        state
            .services
            .start("vehicle", &state.app, &state.manifest)
            .unwrap();
        wait_service_healthy(&state.services, "vehicle");
        block_on(async {
            let client = Client {
                base: serve(&state).await,
                http: reqwest::Client::builder()
                    .redirect(reqwest::redirect::Policy::none())
                    .build()
                    .unwrap(),
            };
            let owner = Some(MASTER);
            client
                .ok(
                    owner,
                    "create_graph",
                    json!({ "graphId": GRAPH, "title": GRAPH }),
                )
                .await;
            client
                .ok(
                    owner,
                    "create_folder",
                    json!({ "graphId": GRAPH, "folderId": "moo", "name": "Moo" }),
                )
                .await;
            client
                .ok(
                    owner,
                    "create_document",
                    json!({ "graphId": GRAPH, "documentId": "guide-doc", "title": "Guide" }),
                )
                .await;
            let alice = account_token(&key, "a11ce000-0000-4000-8000-000000000001", true);
            let (status, echoed) = client
                .send(
                    reqwest::Method::GET,
                    &format!("/services/vehicle/g/{GRAPH}/sessions"),
                    Some(&alice),
                    None,
                )
                .await;
            assert_eq!(status, 200, "{echoed}");
            assert_eq!(echoed["writeFence"], format!("{GRAPH}/moo"));
            let account = echoed["visitorAccount"].as_str().unwrap().to_string();
            assert!(account.starts_with("acct-"), "{echoed}");
            assert_eq!(
                echoed["graphRole"], "viewer",
                "the role a viewer's service knows stays viewer"
            );
            let delegated = echoed["delegatedToken"].as_str().unwrap().to_string();
            let made = client
                .ok(
                    Some(&delegated),
                    "create_document",
                    json!({ "graphId": GRAPH, "title": "Written by alice's agent" }),
                )
                .await;
            let id = made["documentId"].as_str().unwrap().to_string();
            assert_eq!(made["parentId"], "moo", "{made}");
            client.ok(Some(&delegated), "write_document", json!({ "graphId": GRAPH, "documentId": id, "content": "moo", "format": "markdown" })).await;
            let workspace = client
                .ok(
                    owner,
                    "get_workspace",
                    json!({ "graphId": GRAPH, "folderId": "moo", "depth": 4 }),
                )
                .await;
            assert!(
                workspace.to_string().contains(&account),
                "the agent's note is alice's: {workspace}"
            );
            for (tool, arguments) in [
                (
                    "write_document",
                    json!({ "graphId": GRAPH, "documentId": "guide-doc", "content": "x", "format": "markdown" }),
                ),
                (
                    "create_document",
                    json!({ "graphId": GRAPH, "title": "x", "parentId": null, "documentId": "chosen" }),
                ),
                (
                    "sparql_update",
                    json!({ "graphId": GRAPH, "sparql": "INSERT DATA { <urn:a> <urn:b> <urn:c> }" }),
                ),
                ("create_folder", json!({ "graphId": GRAPH, "name": "x" })),
            ] {
                let called = client.tool(Some(&delegated), tool, arguments).await;
                assert!(refused(&called), "{tool}: {called}");
            }
            // The audit names alice, via the service.
            let audit = std::fs::read_to_string(
                crate::profile_paths::loopback_audit_log_path(&state.app).unwrap(),
            )
            .unwrap_or_default();
            assert!(
                audit.contains("cognito:a11ce000-0000-4000-8000-000000000001"),
                "the audit names the account"
            );

            let carol = account_token(&key, "ca401000-0000-4000-8000-000000000003", false);
            let (_, echoed) = client
                .send(
                    reqwest::Method::GET,
                    &format!("/services/vehicle/g/{GRAPH}/sessions"),
                    Some(&carol),
                    None,
                )
                .await;
            assert!(echoed["writeFence"].is_null(), "{echoed}");
            let delegated = echoed["delegatedToken"].as_str().unwrap().to_string();
            let called = client
                .tool(
                    Some(&delegated),
                    "create_document",
                    json!({ "graphId": GRAPH, "title": "x" }),
                )
                .await;
            assert!(refused(&called), "{called}");
        });
        state.services.shutdown();
    });
}
