//! A content write keeps a document where it is. Hosted Hoja (2026-09-23)
//! found that every `write_document` that did not name a parent refiled the
//! document at the workspace root with a fresh order: the write path passed
//! `parentId: None` and an enqueue-timestamp order into the workspace upsert,
//! which set both unconditionally. Every writer was affected — Hoja, agents
//! over MCP, Choreograph. These tests drive the real MCP handlers
//! (`create_folder`, `create_document`, `write_document`, `move_documents`,
//! `get_workspace`) against a disposable graph.
use serde_json::{json, Value};

const GRAPH: &str = "write-placement";

fn row(workspace: &Value, document_id: &str) -> Value {
    workspace["documents"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|row| row["documentId"] == document_id || row["id"] == document_id)
        .cloned()
        .unwrap_or_else(|| panic!("{document_id} not in workspace: {workspace}"))
}

fn with_graph(test: impl FnOnce(crate::app_runtime::AppHandle)) {
    let _serial = crate::tauri_runtime::profile_env_serial()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let previous = std::env::var_os("GARDEN_PROFILE_DIR");
    let profile =
        std::env::temp_dir().join(format!("garden-write-placement-{}", uuid::Uuid::new_v4()));
    std::env::set_var("GARDEN_PROFILE_DIR", &profile);
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let app = crate::tauri_runtime::build_mock_app_for_tests(true);
        crate::graph_service::create_graph_service(
            &app,
            crate::graph_service::CreateGraphInput {
                graph_id: Some(GRAPH.into()),
                title: "Write placement specimen".into(),
                description: None,
                operation_id: None,
            },
        )
        .unwrap();
        test(app);
    }));
    match previous {
        Some(value) => std::env::set_var("GARDEN_PROFILE_DIR", value),
        None => std::env::remove_var("GARDEN_PROFILE_DIR"),
    }
    let _ = std::fs::remove_dir_all(&profile);
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}

async fn workspace(app: &crate::app_runtime::AppHandle) -> Value {
    crate::workspace_projection_service::mcp_local_get_workspace(
        app.clone(),
        &json!({"graphId": GRAPH, "depth": 5, "limit": 200}),
    )
    .await
    .unwrap()
}

/// `write_document` over MCP: content only — the tool carries no placement.
async fn write(app: &crate::app_runtime::AppHandle) {
    crate::document_mutation_service::mcp_local_write_document(
        app.clone(),
        &json!({
            "graph_id": GRAPH, "document_id": "doc-filed",
            "content": "# Filed\n\nedited body", "format": "markdown",
        }),
    )
    .await
    .unwrap();
}

/// A `document.write` operation as the desktop frontend enqueues it, which may
/// name a placement explicitly.
async fn write_op(app: &crate::app_runtime::AppHandle, document_id: &str, payload: Value) {
    let mut payload = payload;
    payload["documentId"] = json!(document_id);
    if payload.get("content").is_none() {
        payload["content"] = json!("# Filed\n\nop body");
        payload["format"] = json!("markdown");
    }
    crate::crdt_queue::enqueue_crdt_operation(
        app.clone(),
        crate::crdt_operation_types::EnqueueCrdtOperationInput {
            kind: "document.write".into(),
            graph_id: GRAPH.into(),
            document_id: Some(document_id.into()),
            payload,
        },
    )
    .await
    .unwrap();
}

async fn seed(app: &crate::app_runtime::AppHandle) {
    crate::mcp_workspace_crdt_service::mcp_local_create_folder(
        app.clone(),
        &json!({"graphId": GRAPH, "folderId": "folder-a", "name": "A"}),
    )
    .await
    .unwrap();
    crate::mcp_workspace_crdt_service::mcp_local_create_document(
        app.clone(),
        &json!({
            "graphId": GRAPH, "documentId": "doc-filed", "title": "Filed",
            "parentId": "folder-a", "order": 7,
        }),
    )
    .await
    .unwrap();
}

#[test]
fn content_write_keeps_parent_and_order() {
    with_graph(|app| {
        crate::app_runtime::async_runtime::block_on(async {
            seed(&app).await;
            let before = row(&workspace(&app).await, "doc-filed");
            assert_eq!(before["parentId"], "folder-a", "seeded in folder: {before}");
            assert_eq!(
                before["order"].as_f64(),
                Some(7.0),
                "seeded order: {before}"
            );

            write(&app).await;
            let after = row(&workspace(&app).await, "doc-filed");
            assert_eq!(
                after["parentId"], "folder-a",
                "a content write refiled it: {after}"
            );
            assert_eq!(
                after["order"].as_f64(),
                Some(7.0),
                "a content write reordered it: {after}"
            );
            assert_eq!(after["title"], "Filed");

            // tiptapJson writes take the same path.
            crate::document_mutation_service::mcp_local_write_document(
                app.clone(),
                &json!({
                    "graph_id": GRAPH, "document_id": "doc-filed",
                    "tiptapJson": {"type": "doc", "content": [
                        {"type": "heading", "attrs": {"level": 1}, "content": [{"type": "text", "text": "Filed"}]},
                        {"type": "paragraph", "content": [{"type": "text", "text": "json body"}]},
                    ]},
                }),
            )
            .await
            .unwrap();
            let after_json = row(&workspace(&app).await, "doc-filed");
            assert_eq!(after_json["parentId"], "folder-a", "{after_json}");
            assert_eq!(after_json["order"].as_f64(), Some(7.0), "{after_json}");
        });
    });
}

#[test]
fn explicit_placement_on_a_write_still_applies_and_moves_still_work() {
    with_graph(|app| {
        crate::app_runtime::async_runtime::block_on(async {
            seed(&app).await;
            crate::mcp_workspace_crdt_service::mcp_local_create_folder(
                app.clone(),
                &json!({"graphId": GRAPH, "folderId": "folder-b", "name": "B"}),
            )
            .await
            .unwrap();

            // An explicit parentId/order in the write payload is honoured.
            write_op(
                &app,
                "doc-filed",
                json!({"parentId": "folder-b", "order": 3}),
            )
            .await;
            let moved = row(&workspace(&app).await, "doc-filed");
            assert_eq!(moved["parentId"], "folder-b", "{moved}");
            assert_eq!(moved["order"].as_f64(), Some(3.0), "{moved}");

            // An explicit null parent means "file at the root".
            write_op(&app, "doc-filed", json!({"parentId": null})).await;
            let rooted = row(&workspace(&app).await, "doc-filed");
            assert!(rooted["parentId"].is_null(), "{rooted}");
            assert_eq!(
                rooted["order"].as_f64(),
                Some(3.0),
                "order untouched: {rooted}"
            );

            // move_documents still moves, and a later content write keeps it there.
            crate::mcp_workspace_crdt_service::mcp_local_move_documents(
                app.clone(),
                &json!({"graphId": GRAPH, "documentIds": ["doc-filed"], "parentId": "folder-a", "order": 11}),
            )
            .await
            .unwrap();
            write(&app).await;
            let back = row(&workspace(&app).await, "doc-filed");
            assert_eq!(back["parentId"], "folder-a", "{back}");
            assert_eq!(back["order"].as_f64(), Some(11.0), "{back}");
        });
    });
}

#[test]
fn a_write_that_creates_a_document_files_it_as_before() {
    with_graph(|app| {
        crate::app_runtime::async_runtime::block_on(async {
            seed(&app).await;
            // No workspace entry yet: the write creates one at the root (today's
            // behaviour), or in the folder it names.
            crate::document_mutation_service::mcp_local_write_document(
                app.clone(),
                &json!({"graph_id": GRAPH, "document_id": "doc-new-root", "content": "# New root"}),
            )
            .await
            .unwrap();
            write_op(
                &app,
                "doc-new-filed",
                json!({"content": "# New filed", "parentId": "folder-a"}),
            )
            .await;
            let ws = workspace(&app).await;
            let root = row(&ws, "doc-new-root");
            assert!(root["parentId"].is_null(), "{root}");
            assert!(root["order"].as_f64().is_some_and(|o| o > 0.0), "{root}");
            assert_eq!(row(&ws, "doc-new-filed")["parentId"], "folder-a");
        });
    });
}
