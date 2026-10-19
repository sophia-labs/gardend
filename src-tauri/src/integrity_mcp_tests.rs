//! Integrity of who writes what (2026-10-14), through the real router and the
//! real MCP handlers of a self-host gardend: the master key (the owner) and a
//! client token with a label. Found by the W6 probe
//! (`plans/2026-10-14-cow-tools-demo/folder-scoped-edit.md`, §3.5 F1, F3, F5):
//! - `create_document` with an existing id took the document over;
//! - `write_document` on a deleted id brought it back with the caller's text;
//! - a comment's author was whatever the caller said.
use crate::cell_graph_boundary::CellGraphBoundary;
use crate::crdt_engine::rooms::RoomRegistry;
use crate::loopback_client_token_service::create_client_token;
use crate::loopback_client_token_types::CreateLoopbackClientTokenInput;
use crate::loopback_state::{LoopbackManifest, LoopbackState};
use axum::{body::Body, http::StatusCode};
use serde_json::{json, Value};
use std::path::Path;
use std::sync::Arc;
#[cfg(feature = "desktop")]
use tauri::Manager;
use tower::ServiceExt;
use yrs::types::ToJson;
use yrs::{Map as _, Out, ReadTxn, Transact};

const MASTER: &str = "integrity-test-master-key";
const GRAPH: &str = "integrity-notes";

fn state(app: crate::app_runtime::AppHandle, dir: &Path) -> Arc<LoopbackState> {
    let grant = crate::loopback_token_grants::session_all_token_grant();
    Arc::new(LoopbackState {
        app,
        token: MASTER.into(),
        manifest: LoopbackManifest {
            runtime_profile: "test",
            bind_host: "127.0.0.1",
            port: 0,
            api_url: "http://127.0.0.1:0".into(),
            mcp_url: "http://127.0.0.1:0/mcp".into(),
            openapi_url: "http://127.0.0.1:0/openapi.json".into(),
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
        self_host: Arc::new(Default::default()),
    })
}

/// A throwaway profile with one graph, served by the real router.
fn with_gardend(name: &str, test: impl FnOnce(Arc<LoopbackState>)) {
    let _serial = crate::tauri_runtime::profile_env_serial()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let root =
        std::env::temp_dir().join(format!("garden-integrity-{name}-{}", uuid::Uuid::new_v4()));
    let profile = root.join("profile");
    std::fs::create_dir_all(&profile).unwrap();
    let previous = std::env::var_os("GARDEN_PROFILE_DIR");
    std::env::set_var("GARDEN_PROFILE_DIR", &profile);
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let app = crate::tauri_runtime::build_mock_app_for_tests(true);
        crate::graph_service::create_graph_service(
            &app,
            crate::graph_service::CreateGraphInput {
                graph_id: Some(GRAPH.into()),
                title: "Integrity notes".into(),
                description: None,
                operation_id: None,
            },
        )
        .unwrap();
        test(state(app, &root));
    }));
    match previous {
        Some(previous) => std::env::set_var("GARDEN_PROFILE_DIR", previous),
        None => std::env::remove_var("GARDEN_PROFILE_DIR"),
    }
    let _ = std::fs::remove_dir_all(&root);
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}

async fn send(
    state: &Arc<LoopbackState>,
    method: &str,
    path: &str,
    token: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let request = axum::http::Request::builder()
        .method(method)
        .uri(path)
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/json")
        .body(match body {
            Some(body) => Body::from(serde_json::to_vec(&body).unwrap()),
            None => Body::empty(),
        })
        .unwrap();
    let response = crate::loopback_router::loopback_router(state.clone())
        .oneshot(request)
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 10_000_000)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// One MCP `tools/call`: the tool's value, or its error message.
async fn tool(
    state: &Arc<LoopbackState>,
    token: &str,
    name: &str,
    arguments: Value,
) -> Result<Value, String> {
    let body = json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": { "name": name, "arguments": arguments } });
    let (status, reply) = send(state, "POST", "/mcp", token, Some(body)).await;
    assert_eq!(status, StatusCode::OK, "{name}: {reply}");
    match reply["error"]["message"].as_str() {
        Some(message) => Err(message.to_string()),
        None => Ok(reply["result"]["structuredContent"].clone()),
    }
}

fn graph_dir(state: &Arc<LoopbackState>) -> std::path::PathBuf {
    crate::graph_paths::existing_graph_dir(&state.app, GRAPH).unwrap()
}

/// The workspace row of `document_id`, as the workspace Y.Doc holds it.
async fn workspace_row(state: &Arc<LoopbackState>, document_id: &str) -> Option<Value> {
    let room =
        crate::crdt_engine::workspace_ops::workspace_room(&state.app, GRAPH, &graph_dir(state))
            .await
            .unwrap();
    let id = document_id.to_string();
    room.with_doc(move |doc| {
        let txn = doc.transact();
        match txn.get_map("documents")?.get(&txn, &id)? {
            Out::YMap(map) => Some(map.to_json(&txn)),
            _ => None,
        }
    })
    .await
    .map(|any| serde_json::to_value(any).unwrap())
}

/// The stored comments of `document_id`, as its document Y.Doc holds them.
async fn stored_comments(state: &Arc<LoopbackState>, document_id: &str) -> Value {
    let path = crate::ydoc_paths::checked_document_ydoc_state_path(&graph_dir(state), document_id)
        .unwrap();
    let room = state
        .app
        .state::<RoomRegistry>()
        .get_or_create(&format!("doc:{GRAPH}:{document_id}"), path)
        .await
        .unwrap();
    room.with_doc(|doc| {
        let txn = doc.transact();
        txn.get_map("comments")
            .map(|comments| serde_json::to_value(comments.to_json(&txn)).unwrap())
            .unwrap_or(Value::Null)
    })
    .await
}

/// F1: a create that names an existing document is refused and changes
/// nothing; the document stays where and what it was. Asking for the old
/// upsert by name (`ifExists: "update"`) or for the existing document as it
/// is (`ifExists: "keep"`) still works; a document in the Trash is never
/// rewritten.
#[test]
fn create_document_never_takes_over_an_existing_document() {
    with_gardend("hijack", |state| {
        crate::app_runtime::async_runtime::block_on(async {
            tool(
                &state,
                MASTER,
                "create_document",
                json!({ "graphId": GRAPH, "documentId": "the-guide", "title": "The Guide" }),
            )
            .await
            .unwrap();
            tool(
                &state,
                MASTER,
                "create_folder",
                json!({ "graphId": GRAPH, "folderId": "visitors", "name": "Visitors" }),
            )
            .await
            .unwrap();

            let refused = tool(&state, MASTER, "create_document", json!({
                "graphId": GRAPH, "documentId": "the-guide", "title": "HIJACKED", "parentId": "visitors",
            }))
            .await
            .expect_err("a create over an existing document is refused");
            assert!(refused.contains("already exists"), "{refused}");
            assert!(
                refused.contains("write_document"),
                "the refusal points to the right tools: {refused}"
            );
            let row = workspace_row(&state, "the-guide").await.unwrap();
            assert_eq!(row["title"], "The Guide", "{row}");
            assert!(row["parentId"].is_null(), "not moved into visitors: {row}");

            // The same through the REST CRDT route and the raw crdt_operation tool.
            let (status, body) = send(&state, "POST", "/api/crdt/operations", MASTER, Some(json!({
                "kind": "workspace.createDocument", "graphId": GRAPH,
                "payload": { "documentId": "the-guide", "title": "HIJACKED", "parentId": "visitors" },
            })))
            .await;
            assert!(
                body.to_string().contains("already exists"),
                "{status} {body}"
            );
            let raw = tool(&state, MASTER, "crdt_operation", json!({
                "kind": "workspace.createDocument", "graphId": GRAPH,
                "payload": { "documentId": "the-guide", "title": "HIJACKED", "parentId": "visitors" },
            }))
            .await;
            assert!(
                raw.as_ref()
                    .is_err_and(|error| error.contains("already exists")),
                "{raw:?}"
            );
            assert_eq!(
                workspace_row(&state, "the-guide").await.unwrap()["title"],
                "The Guide"
            );

            // Opt-ins.
            let kept = tool(&state, MASTER, "create_document", json!({
                "graphId": GRAPH, "documentId": "the-guide", "title": "Ignored", "parentId": "visitors",
                "ifExists": "keep",
            }))
            .await
            .unwrap();
            assert_eq!(kept["existed"], true, "{kept}");
            assert_eq!(kept["title"], "The Guide", "{kept}");
            let row = workspace_row(&state, "the-guide").await.unwrap();
            assert_eq!(row["title"], "The Guide");
            assert!(row["parentId"].is_null(), "{row}");
            let updated = tool(
                &state,
                MASTER,
                "create_document",
                json!({
                    "graphId": GRAPH, "documentId": "the-guide", "title": "The Guide, revised",
                    "ifExists": "update",
                }),
            )
            .await
            .unwrap();
            assert_eq!(updated["existed"], true, "{updated}");
            assert_eq!(
                workspace_row(&state, "the-guide").await.unwrap()["title"],
                "The Guide, revised"
            );
            let unknown = tool(&state, MASTER, "create_document", json!({
                "graphId": GRAPH, "documentId": "the-guide", "title": "x", "ifExists": "replace",
            }))
            .await;
            assert!(
                unknown
                    .as_ref()
                    .is_err_and(|error| error.contains("ifExists")),
                "{unknown:?}"
            );

            // A document in the Trash.
            let (status, body) = send(
                &state,
                "DELETE",
                &format!("/navigation/{GRAPH}/documents/the-guide"),
                MASTER,
                None,
            )
            .await;
            assert!(status.is_success(), "trash: {status} {body}");
            for if_exists in [Value::Null, json!("update")] {
                let refused = tool(&state, MASTER, "create_document", json!({
                    "graphId": GRAPH, "documentId": "the-guide", "title": "HIJACKED", "ifExists": if_exists,
                }))
                .await
                .expect_err("a document in the Trash is not rewritten");
                assert!(refused.contains("in the Trash"), "{refused}");
            }
            let kept = tool(
                &state,
                MASTER,
                "create_document",
                json!({
                    "graphId": GRAPH, "documentId": "the-guide", "title": "x", "ifExists": "keep",
                }),
            )
            .await
            .unwrap();
            assert_eq!(kept["trashed"], true, "{kept}");
            let row = workspace_row(&state, "the-guide").await.unwrap();
            assert_eq!(row["title"], "The Guide, revised", "{row}");
            assert_eq!(row["trashed"], true, "still in the Trash: {row}");

            // Creating still creates: a new id, and no id at all.
            let fresh = tool(&state, MASTER, "create_document",
                json!({ "graphId": GRAPH, "documentId": "a-new-note", "title": "A new note", "parentId": "visitors" }))
                .await
                .unwrap();
            assert!(fresh.get("existed").is_none(), "{fresh}");
            assert_eq!(
                workspace_row(&state, "a-new-note").await.unwrap()["parentId"],
                "visitors"
            );
            let minted = tool(
                &state,
                MASTER,
                "create_document",
                json!({ "graphId": GRAPH, "title": "Minted" }),
            )
            .await
            .unwrap();
            assert!(
                minted["documentId"]
                    .as_str()
                    .is_some_and(|id| id.starts_with("doc-")),
                "{minted}"
            );
            // A write to an existing document is not a create: it still works.
            tool(&state, MASTER, "write_document",
                json!({ "graphId": GRAPH, "documentId": "a-new-note", "content": "Still writable." }))
                .await
                .unwrap();
        });
    });
}

/// F3: a write to a deleted document is refused, says where restoring lives,
/// and brings nothing back; a caller cannot claim the server's recreation
/// marker.
#[test]
fn write_document_never_resurrects_a_deleted_document() {
    with_gardend("resurrect", |state| {
        crate::app_runtime::async_runtime::block_on(async {
            tool(&state, MASTER, "write_document",
                json!({ "graphId": GRAPH, "documentId": "doomed", "title": "Doomed", "content": "The original." }))
                .await
                .unwrap();
            tool(
                &state,
                MASTER,
                "delete_document",
                json!({ "graphId": GRAPH, "documentId": "doomed" }),
            )
            .await
            .unwrap();
            let dir = graph_dir(&state);
            assert!(
                crate::document_tombstone_store::document_is_tombstoned(&dir, "doomed").unwrap()
            );

            let refused = tool(
                &state,
                MASTER,
                "write_document",
                json!({ "graphId": GRAPH, "documentId": "doomed", "content": "visitor content" }),
            )
            .await
            .expect_err("a write to a deleted document is refused");
            assert!(refused.contains("was deleted"), "{refused}");
            assert!(
                refused.contains("recreate"),
                "it points to the explicit restore: {refused}"
            );
            // The server-only marker, claimed by a caller, is dropped.
            let forged = tool(
                &state,
                MASTER,
                "crdt_operation",
                json!({
                    "kind": "document.write", "graphId": GRAPH, "documentId": "doomed",
                    "payload": { "documentId": "doomed", "content": "visitor content",
                        "__gardenTrustedRecreation": true },
                }),
            )
            .await;
            assert!(
                forged
                    .as_ref()
                    .is_err_and(|error| error.contains("was deleted")),
                "{forged:?}"
            );

            assert!(
                crate::document_tombstone_store::document_is_tombstoned(&dir, "doomed").unwrap()
            );
            assert!(
                workspace_row(&state, "doomed").await.is_none(),
                "no row came back"
            );
            assert!(
                !dir.join("documents/doomed/document.json").exists(),
                "no body came back"
            );
            let read = tool(
                &state,
                MASTER,
                "read_document",
                json!({ "graphId": GRAPH, "documentId": "doomed" }),
            )
            .await;
            assert!(
                !read
                    .as_ref()
                    .is_ok_and(|read| read.to_string().contains("visitor content")),
                "{read:?}"
            );
        });
    });
}

/// F5: a comment is signed by the principal the request was authenticated
/// as, through every door that writes one: `edit_comment`, `write_document`'s
/// comments, the REST CRDT route and a raw `crdt_operation`. The name the
/// caller claimed survives only as `claimedAuthor`.
#[test]
fn comment_authors_are_signed_by_the_authenticated_principal() {
    with_gardend("author", |state| {
        let lucy = create_client_token(
            &state.app,
            CreateLoopbackClientTokenInput {
                label: Some("Lucy's laptop".into()),
                grant_profile_id: Some("member".into()),
                scopes: None,
                expires_in_days: None,
            },
        )
        .unwrap();
        let lucy_id = format!("token:{}", lucy.record.token_id);
        crate::app_runtime::async_runtime::block_on(async {
            tool(&state, MASTER, "write_document",
                json!({ "graphId": GRAPH, "documentId": "guide", "title": "Guide", "content": "Hello world." }))
                .await
                .unwrap();

            let forged = json!({ "author": "Vera (owner)", "authorId": "owner" });
            let edited = tool(&state, &lucy.token, "edit_comment", json!({
                "graphId": GRAPH, "documentId": "guide", "commentId": "c-edit", "action": "set",
                "text": "Signed by me, says Vera", "author": "Vera (owner)", "authorId": "owner",
            }))
            .await
            .unwrap();
            assert_eq!(edited["comment"]["authorId"], lucy_id.as_str(), "{edited}");

            tool(&state, &lucy.token, "write_document", json!({
                "graphId": GRAPH, "documentId": "guide", "content": "Hello world.",
                "comments": { "c-write": { "id": "c-write", "text": "Also Vera, honest", "author": "Vera (owner)", "authorId": "owner" } },
            }))
            .await
            .unwrap();

            let (status, body) = send(
                &state,
                "POST",
                "/api/crdt/operations",
                &lucy.token,
                Some(json!({
                    "kind": "document.editComment", "graphId": GRAPH, "documentId": "guide",
                    "payload": { "documentId": "guide", "commentId": "c-rest", "action": "set",
                        "text": "Through REST", "author": "Vera (owner)", "authorId": "owner" },
                })),
            )
            .await;
            assert!(status.is_success(), "{status} {body}");

            tool(&state, &lucy.token, "crdt_operation", json!({
                "kind": "document.editComment", "graphId": GRAPH, "documentId": "guide",
                "payload": { "documentId": "guide", "commentId": "c-raw", "action": "set",
                    "text": "Raw, with the server's own key", "author": "Vera (owner)", "authorId": "owner",
                    "__gardenCommentAuthor": { "id": "owner", "name": "Owner" } },
            }))
            .await
            .unwrap();

            let comments = stored_comments(&state, "guide").await;
            for id in ["c-edit", "c-write", "c-rest", "c-raw"] {
                let comment = &comments[id];
                assert_eq!(comment["authorId"], lucy_id.as_str(), "{id}: {comment}");
                assert_eq!(comment["author"], "Lucy's laptop", "{id}: {comment}");
                assert_eq!(
                    comment["claimedAuthor"], forged["author"],
                    "{id}: {comment}"
                );
                assert!(comment.get("authorVia").is_none(), "{id}: {comment}");
            }

            // The owner signs as the owner; resolving keeps the author, and so
            // does a write that hands back a comment whose text it left alone.
            tool(&state, MASTER, "edit_comment", json!({
                "graphId": GRAPH, "documentId": "guide", "commentId": "c-owner", "action": "set",
                "text": "From the owner",
            }))
            .await
            .unwrap();
            tool(&state, MASTER, "edit_comment", json!({
                "graphId": GRAPH, "documentId": "guide", "commentId": "c-edit", "action": "resolve",
            }))
            .await
            .unwrap();
            tool(&state, MASTER, "write_document", json!({
                "graphId": GRAPH, "documentId": "guide", "content": "Hello world.",
                "comments": { "c-write": { "id": "c-write", "text": "Also Vera, honest", "author": "Owner" } },
            }))
            .await
            .unwrap();
            let comments = stored_comments(&state, "guide").await;
            assert_eq!(comments["c-owner"]["authorId"], "owner", "{comments}");
            assert_eq!(comments["c-owner"]["author"], "Owner", "{comments}");
            assert!(
                comments["c-owner"].get("claimedAuthor").is_none(),
                "{comments}"
            );
            assert_eq!(comments["c-edit"]["resolved"], true, "{comments}");
            assert_eq!(
                comments["c-edit"]["authorId"],
                lucy_id.as_str(),
                "{comments}"
            );
            assert_eq!(
                comments["c-write"]["authorId"],
                lucy_id.as_str(),
                "{comments}"
            );

            // Rewriting another's comment makes it the writer's.
            tool(
                &state,
                MASTER,
                "edit_comment",
                json!({
                    "graphId": GRAPH, "documentId": "guide", "commentId": "c-rest", "action": "set",
                    "text": "Rewritten by the owner",
                }),
            )
            .await
            .unwrap();
            let comments = stored_comments(&state, "guide").await;
            assert_eq!(comments["c-rest"]["authorId"], "owner", "{comments}");
        });
    });
}

/// The explicit way back for a deleted document is the source ledger's
/// `recreate` (it checks the deletion boundary in the ledger first): a plain
/// write in between is refused, the recreate brings the id back, and the
/// document then takes writes again.
#[test]
fn a_deleted_document_comes_back_only_through_the_ledger_recreate() {
    with_gardend("recreate", |state| {
        let graph_incarnation =
            crate::graph_record_store::ensure_graph_incarnation(&state.app, GRAPH).unwrap();
        crate::app_runtime::async_runtime::block_on(async {
            let push = |operation: Value| {
                let state = state.clone();
                let graph_incarnation = graph_incarnation.clone();
                async move {
                    tool(&state, MASTER, "source_push", json!({
                        "graphId": GRAPH, "graphIncarnation": graph_incarnation, "operations": [operation],
                    }))
                    .await
                }
            };
            push(json!({ "kind": "documentLifecycle", "operationId": "lc-create", "action": "create",
                "documentId": "lc-doc", "title": "Lifecycle", "newDocumentIncarnation": uuid::Uuid::new_v4().to_string() }))
                .await
                .unwrap();
            tool(&state, MASTER, "write_document",
                json!({ "graphId": GRAPH, "documentId": "lc-doc", "content": "Before the delete." }))
                .await
                .unwrap();
            let dir = graph_dir(&state);
            let first =
                crate::document_incarnation_store::ensure_document_incarnation_id(&dir, "lc-doc")
                    .unwrap();
            push(
                json!({ "kind": "documentLifecycle", "operationId": "lc-delete", "action": "delete",
                "documentId": "lc-doc", "expectedDocumentIncarnation": first }),
            )
            .await
            .unwrap();
            assert!(
                crate::document_tombstone_store::document_is_tombstoned(&dir, "lc-doc").unwrap()
            );
            let refused = tool(
                &state,
                MASTER,
                "write_document",
                json!({ "graphId": GRAPH, "documentId": "lc-doc", "content": "Sneaking back." }),
            )
            .await
            .expect_err("a plain write does not bring it back");
            assert!(refused.contains("was deleted"), "{refused}");

            push(json!({ "kind": "documentLifecycle", "operationId": "lc-recreate", "action": "recreate",
                "documentId": "lc-doc", "title": "Lifecycle, again", "expectedDocumentIncarnation": first,
                "newDocumentIncarnation": uuid::Uuid::new_v4().to_string() }))
                .await
                .unwrap();
            assert!(
                !crate::document_tombstone_store::document_is_tombstoned(&dir, "lc-doc").unwrap()
            );
            assert!(
                workspace_row(&state, "lc-doc").await.is_some(),
                "back in the tree"
            );
            tool(&state, MASTER, "write_document",
                json!({ "graphId": GRAPH, "documentId": "lc-doc", "content": "After the recreate." }))
                .await
                .unwrap();
            let second =
                crate::document_incarnation_store::ensure_document_incarnation_id(&dir, "lc-doc")
                    .unwrap();
            assert_ne!(first, second, "a new incarnation");
        });
    });
}
