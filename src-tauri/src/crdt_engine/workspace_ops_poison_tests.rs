//! A queued workspace operation that can never succeed answers its caller at
//! once and leaves the graph's queue free.
//!
//! Found 2026-10-13 (agent-core V4): `create_document` with a title over 96
//! characters never answered. The workspace entry was committed first, the
//! document body then refused the title, and a refusal after the hot commit
//! is kept "for tail retry": every 25 ms to 1 s while the caller waits, then
//! every 60 s, forever, and after a restart again from the journal. A retry
//! is also a barrier, so every later write to that graph waited behind it.
//!
//! These tests drive the real MCP handlers against a disposable profile, each
//! call bounded far below the queue's 600 s caller timeout so that a parked
//! operation shows up as a failed test, not a hung one. The last test is the
//! other half of the contract: a tail that fails for a while (a disk, a lease)
//! is still retried until it lands.
use serde_json::{json, Value};
use std::time::Duration;

/// Plenty for a create or delete that answers; a parked one never does.
const ANSWER_WITHIN: Duration = Duration::from_secs(10);

/// The question V4's mirror used as a conversation's title (107 characters).
const LONG_TITLE: &str = "Which document in this workspace describes the battery charger? \
Answer with its title, in one short sentence.";

fn with_graph<F, Fut>(graph_id: &'static str, case: F)
where
    F: FnOnce(crate::app_runtime::AppHandle) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let _serial = crate::tauri_runtime::profile_env_serial()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let previous = std::env::var_os("GARDEN_PROFILE_DIR");
    let profile = std::env::temp_dir().join(format!("garden-{graph_id}-{}", uuid::Uuid::new_v4()));
    std::env::set_var("GARDEN_PROFILE_DIR", &profile);
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        crate::app_runtime::async_runtime::block_on(async {
            let app = crate::tauri_runtime::build_mock_app_for_tests(true);
            crate::graph_service::create_graph_service(
                &app,
                crate::graph_service::CreateGraphInput {
                    graph_id: Some(graph_id.into()),
                    title: "Poison specimen".into(),
                    description: None,
                    operation_id: None,
                },
            )
            .unwrap();
            case(app).await;
        })
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

async fn create(
    app: &crate::app_runtime::AppHandle,
    graph_id: &str,
    mut arguments: Value,
) -> Result<Value, String> {
    arguments["graphId"] = json!(graph_id);
    tokio::time::timeout(
        ANSWER_WITHIN,
        crate::mcp_workspace_crdt_service::mcp_local_create_document(app.clone(), &arguments),
    )
    .await
    .unwrap_or_else(|_| {
        panic!(
            "create_document {arguments} did not answer within {ANSWER_WITHIN:?}: parked for retry"
        )
    })
}

async fn delete(
    app: &crate::app_runtime::AppHandle,
    graph_id: &str,
    document_id: &str,
) -> Result<Value, String> {
    let arguments = json!({ "graphId": graph_id, "documentId": document_id });
    tokio::time::timeout(
        ANSWER_WITHIN,
        crate::mcp_workspace_crdt_service::mcp_local_delete_document(app.clone(), &arguments),
    )
    .await
    .unwrap_or_else(|_| {
        panic!(
            "delete_document {arguments} did not answer within {ANSWER_WITHIN:?}: parked for retry"
        )
    })
}

async fn workspace_rows(app: &crate::app_runtime::AppHandle, graph_id: &str) -> Vec<Value> {
    let workspace = crate::workspace_projection_service::mcp_local_get_workspace(
        app.clone(),
        &json!({ "graphId": graph_id, "depth": 5, "limit": 200 }),
    )
    .await
    .unwrap();
    workspace["documents"]
        .as_array()
        .cloned()
        .unwrap_or_default()
}

fn row_id(row: &Value) -> &str {
    row["documentId"]
        .as_str()
        .or_else(|| row["id"].as_str())
        .unwrap_or_default()
}

/// Nothing pending in the journal, nothing in flight, no retry barrier.
fn assert_queue_free(app: &crate::app_runtime::AppHandle) {
    let pending = crate::crdt_operation_journal::recover_pending_crdt_operations(app)
        .expect("read the operation journal");
    assert!(
        pending.is_empty(),
        "operations left pending in the journal: {:?}",
        pending
            .iter()
            .map(|op| (&op.operation_id, &op.kind))
            .collect::<Vec<_>>()
    );
    let queue = app.state::<crate::crdt_queue::CrdtOperationQueue>();
    assert_eq!(
        queue.counts_for_test().unwrap(),
        (0, 0),
        "pending, in flight"
    );
    assert_eq!(
        queue.retry_wait_count_for_test().unwrap(),
        0,
        "retry barriers"
    );
}

#[test]
fn a_title_over_96_characters_is_refused_at_once_and_blocks_nothing() {
    const GRAPH: &str = "poison-title";
    assert!(LONG_TITLE.chars().count() > 96);
    with_graph(GRAPH, |app| async move {
        let refused = create(&app, GRAPH, json!({ "title": LONG_TITLE }))
            .await
            .expect_err("a 107-character title is refused");
        assert!(
            refused.contains("title must be 96 characters or fewer"),
            "{refused}"
        );
        assert_queue_free(&app);

        // The same refusal for a document named by the caller, and for an
        // existing document whose create is sent again with a long title.
        let refused = create(
            &app,
            GRAPH,
            json!({ "documentId": "doc-named", "title": LONG_TITLE }),
        )
        .await
        .expect_err("refused with an id too");
        assert!(refused.contains("96 characters"), "{refused}");
        create(
            &app,
            GRAPH,
            json!({ "documentId": "doc-kept", "title": "Kept" }),
        )
        .await
        .expect("a short title is created");
        let refused = create(
            &app,
            GRAPH,
            json!({ "documentId": "doc-kept", "title": LONG_TITLE }),
        )
        .await
        .expect_err("an upsert with a long title is refused");
        assert!(refused.contains("96 characters"), "{refused}");
        assert_queue_free(&app);

        // 96 is allowed, and the graph takes writes at once.
        let ninety_six = "x".repeat(96);
        let created = create(
            &app,
            GRAPH,
            json!({ "documentId": "doc-96", "title": ninety_six }),
        )
        .await
        .expect("96 characters is the limit, not past it");
        assert_eq!(created["documentId"], "doc-96", "{created}");
        assert_queue_free(&app);

        let rows = workspace_rows(&app, GRAPH).await;
        assert!(
            !rows
                .iter()
                .any(|row| row["title"] == LONG_TITLE || row_id(row) == "doc-named"),
            "a refused create left a row in the tree: {rows:?}"
        );
        let kept = rows
            .iter()
            .find(|row| row_id(row) == "doc-kept")
            .expect("doc-kept listed");
        assert_eq!(
            kept["title"], "Kept",
            "the refused upsert renamed it: {kept}"
        );
        assert!(rows
            .iter()
            .any(|row| row_id(row) == "doc-96" && row["title"] == ninety_six));
    });
}

#[test]
fn a_document_id_that_is_not_a_local_id_is_refused_at_once() {
    const GRAPH: &str = "poison-create-id";
    with_graph(GRAPH, |app| async move {
        let refused = create(
            &app,
            GRAPH,
            json!({ "documentId": "not a local id", "title": "Fine" }),
        )
        .await
        .expect_err("an id with spaces is refused");
        assert!(
            refused.contains("document_id contains invalid characters"),
            "{refused}"
        );
        assert_queue_free(&app);
        create(
            &app,
            GRAPH,
            json!({ "documentId": "doc-after", "title": "After" }),
        )
        .await
        .expect("the graph takes the next write");
        let rows = workspace_rows(&app, GRAPH).await;
        assert!(
            !rows.iter().any(|row| row_id(row) == "not a local id"),
            "{rows:?}"
        );
    });
}

#[test]
fn creating_over_a_deleted_document_is_refused_at_once() {
    const GRAPH: &str = "poison-tombstone";
    with_graph(GRAPH, |app| async move {
        create(
            &app,
            GRAPH,
            json!({ "documentId": "doc-gone", "title": "Gone" }),
        )
        .await
        .expect("created");
        delete(&app, GRAPH, "doc-gone").await.expect("deleted");
        let refused = create(
            &app,
            GRAPH,
            json!({ "documentId": "doc-gone", "title": "Back" }),
        )
        .await
        .expect_err("a deleted id is not created again");
        assert!(
            refused.contains("document tombstoned: doc-gone"),
            "{refused}"
        );
        assert_queue_free(&app);
        create(
            &app,
            GRAPH,
            json!({ "documentId": "doc-other", "title": "Other" }),
        )
        .await
        .expect("the graph takes the next write");
        let rows = workspace_rows(&app, GRAPH).await;
        assert!(
            !rows.iter().any(|row| row_id(row) == "doc-gone"),
            "{rows:?}"
        );
    });
}

#[test]
fn deleting_an_id_that_is_not_a_local_id_is_refused_at_once() {
    const GRAPH: &str = "poison-delete-id";
    with_graph(GRAPH, |app| async move {
        let refused = delete(&app, GRAPH, "not a local id")
            .await
            .expect_err("an id with spaces is refused");
        assert!(
            refused.contains("document_id contains invalid characters"),
            "{refused}"
        );
        assert_queue_free(&app);
        create(
            &app,
            GRAPH,
            json!({ "documentId": "doc-after", "title": "After" }),
        )
        .await
        .expect("the graph takes the next write");
    });
}

#[test]
fn a_create_whose_body_cannot_be_written_for_a_while_is_retried_until_it_lands() {
    const GRAPH: &str = "poison-transient";
    with_graph(GRAPH, |app| async move {
        // A file where the document's folder goes: the workspace entry
        // commits, the body cannot be made, and that can heal.
        let graph_dir = crate::graph_paths::existing_graph_dir(&app, GRAPH).unwrap();
        let blocker = crate::graph_paths::documents_dir(&graph_dir).join("doc-tail");
        std::fs::create_dir_all(blocker.parent().unwrap()).unwrap();
        std::fs::write(&blocker, b"not a folder").unwrap();

        let caller = {
            let app = app.clone();
            crate::app_runtime::async_runtime::spawn(async move {
                crate::mcp_workspace_crdt_service::mcp_local_create_document(
                    app,
                    &json!({ "graphId": GRAPH, "documentId": "doc-tail", "title": "Tail" }),
                )
                .await
            })
        };
        let queue = app.state::<crate::crdt_queue::CrdtOperationQueue>();
        let parked = async {
            while queue.retry_wait_count_for_test().unwrap() == 0 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        };
        tokio::time::timeout(ANSWER_WITHIN, parked)
            .await
            .expect("the failed tail is kept for retry");
        assert!(
            !caller.is_finished(),
            "the caller still waits for its document"
        );
        assert_eq!(
            crate::crdt_operation_journal::recover_pending_crdt_operations(&app)
                .unwrap()
                .len(),
            1,
            "the operation stays journaled while it retries"
        );

        std::fs::remove_file(&blocker).unwrap();
        let created = tokio::time::timeout(ANSWER_WITHIN, caller)
            .await
            .expect("the retry lands once the folder can be made")
            .expect("caller task")
            .expect("created");
        assert_eq!(created["documentId"], "doc-tail", "{created}");
        assert!(crate::document_paths::document_dir(&graph_dir, "doc-tail")
            .unwrap()
            .join("document.json")
            .is_file());
        assert_queue_free(&app);
    });
}

// ─────────────────────────────────────────────────────────────────────────────
// The same family on other paths (2026-10-14): an ingest, a workspace row that
// cannot name a document, a hard folder delete over such a row, an upload
// ingest whose staged file is not there. Each is driven through the real queue
// (`enqueue_crdt_operation`, as the REST and MCP routes do), bounded at 10 s.
// ─────────────────────────────────────────────────────────────────────────────

async fn enqueue(
    app: &crate::app_runtime::AppHandle,
    kind: &str,
    graph_id: &str,
    document_id: Option<&str>,
    payload: Value,
) -> Result<Value, String> {
    let described = format!("{kind} {payload}");
    tokio::time::timeout(
        ANSWER_WITHIN,
        crate::crdt_queue::enqueue_crdt_operation(
            app.clone(),
            crate::crdt_operation_types::EnqueueCrdtOperationInput {
                kind: kind.to_string(),
                graph_id: graph_id.to_string(),
                document_id: document_id.map(str::to_string),
                payload,
            },
        ),
    )
    .await
    .unwrap_or_else(|_| {
        panic!("{described} did not answer within {ANSWER_WITHIN:?}: parked for retry")
    })
}

fn ingest_payload(document_id: &str) -> Value {
    use base64::Engine as _;
    json!({
        "documentId": document_id,
        "markdown": "# Ingested specimen\n\nThe parsed text of a PDF.\n",
        "filename": "specimen.pdf",
        "mimeType": "application/pdf",
        "dataBase64": base64::engine::general_purpose::STANDARD.encode(b"%PDF-1.4 specimen"),
    })
}

/// A row in the workspace Y.Doc, applied the way the WebSocket host applies a
/// client's update. The workspace map takes any key; only queued operations
/// check ids.
async fn client_writes_workspace_row(
    app: &crate::app_runtime::AppHandle,
    graph_id: &str,
    row_id: &str,
    parent_id: Option<&str>,
) {
    use yrs::{Any, Doc, Map, MapPrelim, ReadTxn, StateVector, Transact, WriteTxn};
    let client = Doc::new();
    {
        let mut txn = client.transact_mut();
        let documents = txn.get_or_insert_map("documents");
        let entry = documents.insert(&mut txn, row_id, MapPrelim::default());
        entry.insert(&mut txn, "title", "Stray row");
        entry.insert(
            &mut txn,
            "parentId",
            parent_id
                .map(|parent| Any::String(parent.into()))
                .unwrap_or(Any::Null),
        );
        entry.insert(&mut txn, "section", "documents");
        entry.insert(&mut txn, "order", 7.0);
        entry.insert(&mut txn, "createdAt", 1.0);
        entry.insert(&mut txn, "updatedAt", 1.0);
        entry.insert(&mut txn, "readOnly", false);
    }
    let update = client
        .transact()
        .encode_state_as_update_v1(&StateVector::default());
    let graph_dir = crate::graph_paths::existing_graph_dir(app, graph_id).unwrap();
    let room = super::workspace_ops::workspace_room(app, graph_id, &graph_dir)
        .await
        .unwrap();
    assert!(
        room.apply_client_update(&update).await.unwrap(),
        "the row was applied"
    );
}

#[test]
fn an_ingest_over_a_deleted_document_is_refused_at_once() {
    const GRAPH: &str = "poison-ingest-tombstone";
    with_graph(GRAPH, |app| async move {
        create(
            &app,
            GRAPH,
            json!({ "documentId": "doc-gone", "title": "Gone" }),
        )
        .await
        .expect("created");
        delete(&app, GRAPH, "doc-gone").await.expect("deleted");
        let refused = enqueue(
            &app,
            "document.ingestMarkdownOriginal",
            GRAPH,
            None,
            ingest_payload("doc-gone"),
        )
        .await
        .expect_err("a deleted id is not ingested over");
        assert!(
            refused.contains("document tombstoned: doc-gone"),
            "{refused}"
        );
        assert_queue_free(&app);
        create(
            &app,
            GRAPH,
            json!({ "documentId": "doc-after", "title": "After" }),
        )
        .await
        .expect("the graph takes the next write");
        let rows = workspace_rows(&app, GRAPH).await;
        assert!(
            !rows.iter().any(|row| row_id(row) == "doc-gone"),
            "{rows:?}"
        );
    });
}

#[test]
fn an_ingest_whose_document_id_is_not_a_local_id_is_refused_at_once() {
    const GRAPH: &str = "poison-ingest-id";
    with_graph(GRAPH, |app| async move {
        let refused = enqueue(
            &app,
            "document.ingestMarkdownOriginal",
            GRAPH,
            None,
            ingest_payload("not a local id"),
        )
        .await
        .expect_err("an id with spaces is refused");
        assert!(
            refused.contains("document_id contains invalid characters"),
            "{refused}"
        );
        assert_queue_free(&app);
        create(
            &app,
            GRAPH,
            json!({ "documentId": "doc-after", "title": "After" }),
        )
        .await
        .expect("the graph takes the next write");
    });
}

#[test]
fn an_ingest_whose_body_cannot_be_written_for_a_while_is_retried_until_it_lands() {
    const GRAPH: &str = "poison-ingest-transient";
    with_graph(GRAPH, |app| async move {
        let graph_dir = crate::graph_paths::existing_graph_dir(&app, GRAPH).unwrap();
        let blocker = crate::graph_paths::documents_dir(&graph_dir).join("doc-ingest-tail");
        std::fs::create_dir_all(blocker.parent().unwrap()).unwrap();
        std::fs::write(&blocker, b"not a folder").unwrap();

        let caller = {
            let app = app.clone();
            crate::app_runtime::async_runtime::spawn(async move {
                crate::crdt_queue::enqueue_crdt_operation(
                    app,
                    crate::crdt_operation_types::EnqueueCrdtOperationInput {
                        kind: "document.ingestMarkdownOriginal".to_string(),
                        graph_id: GRAPH.to_string(),
                        document_id: None,
                        payload: ingest_payload("doc-ingest-tail"),
                    },
                )
                .await
            })
        };
        let queue = app.state::<crate::crdt_queue::CrdtOperationQueue>();
        let parked = async {
            while queue.retry_wait_count_for_test().unwrap() == 0 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        };
        tokio::time::timeout(ANSWER_WITHIN, parked)
            .await
            .expect("the failed create is kept for retry");
        assert!(
            !caller.is_finished(),
            "the caller still waits for its document"
        );

        std::fs::remove_file(&blocker).unwrap();
        let ingested = tokio::time::timeout(ANSWER_WITHIN, caller)
            .await
            .expect("the retry lands once the folder can be made")
            .expect("caller task")
            .expect("ingested");
        assert_eq!(ingested["documentId"], "doc-ingest-tail", "{ingested}");
        assert_queue_free(&app);
        let rows = workspace_rows(&app, GRAPH).await;
        assert!(
            rows.iter().any(|row| row_id(row) == "doc-ingest-tail"),
            "{rows:?}"
        );
    });
}

#[test]
fn a_workspace_row_whose_id_is_not_a_local_id_blocks_no_other_write() {
    const GRAPH: &str = "poison-stray-row";
    with_graph(GRAPH, |app| async move {
        create(
            &app,
            GRAPH,
            json!({ "documentId": "doc-before", "title": "Before" }),
        )
        .await
        .expect("created");
        client_writes_workspace_row(&app, GRAPH, "stray row", None).await;

        create(
            &app,
            GRAPH,
            json!({ "documentId": "doc-after", "title": "After" }),
        )
        .await
        .expect("a create beside the stray row lands");
        enqueue(
            &app,
            "workspace.updateDocument",
            GRAPH,
            Some("doc-before"),
            json!({ "documentId": "doc-before", "title": "Before, renamed" }),
        )
        .await
        .expect("a rename beside the stray row lands");
        assert_queue_free(&app);
        let rows = workspace_rows(&app, GRAPH).await;
        assert!(
            rows.iter().any(|row| row_id(row) == "doc-after"),
            "{rows:?}"
        );
        let renamed = rows
            .iter()
            .find(|row| row_id(row) == "doc-before")
            .expect("listed");
        assert_eq!(renamed["title"], "Before, renamed", "{renamed}");
    });
}

async fn delete_folder_hard(
    app: &crate::app_runtime::AppHandle,
    graph_id: &str,
    folder_id: &str,
) -> Result<Value, String> {
    tokio::time::timeout(
        ANSWER_WITHIN,
        crate::mcp_delete_service::mcp_local_delete(
            app.clone(),
            &json!({ "type": "folder", "graphId": graph_id, "folderId": folder_id, "cascade": true, "hard": true }),
        ),
    )
    .await
    .unwrap_or_else(|_| panic!("the folder delete did not answer within {ANSWER_WITHIN:?}: parked for retry"))
}

/// A row whose id is not a local id has no body to delete (2026-10-14, the
/// hub's call): `deleteDocument` removes just the row and says so.
#[test]
fn deleting_a_row_whose_id_is_not_a_local_id_removes_just_that_row() {
    const GRAPH: &str = "poison-stray-delete";
    with_graph(GRAPH, |app| async move {
        create(
            &app,
            GRAPH,
            json!({ "documentId": "doc-kept", "title": "Kept" }),
        )
        .await
        .expect("created");
        client_writes_workspace_row(&app, GRAPH, "stray row", None).await;
        assert!(workspace_rows(&app, GRAPH)
            .await
            .iter()
            .any(|row| row_id(row) == "stray row"));

        let removed = delete(&app, GRAPH, "stray row")
            .await
            .expect("the stray row is removed");
        assert_eq!(removed["deleted"], true, "{removed}");
        assert_eq!(
            removed["removedStrayRowIds"],
            json!(["stray row"]),
            "{removed}"
        );
        assert_queue_free(&app);
        let rows = workspace_rows(&app, GRAPH).await;
        assert!(
            !rows.iter().any(|row| row_id(row) == "stray row"),
            "{rows:?}"
        );
        assert!(rows.iter().any(|row| row_id(row) == "doc-kept"), "{rows:?}");
        let graph_dir = crate::graph_paths::existing_graph_dir(&app, GRAPH).unwrap();
        assert!(
            !crate::document_tombstone_store::document_is_tombstoned(&graph_dir, "doc-kept")
                .unwrap()
        );

        // Once it is gone, the same id names nothing and is refused, at once.
        let refused = delete(&app, GRAPH, "stray row")
            .await
            .expect_err("nothing left to remove");
        assert!(
            refused.contains("document_id contains invalid characters"),
            "{refused}"
        );
        assert_queue_free(&app);

        // The `delete` tool takes the same path.
        client_writes_workspace_row(&app, GRAPH, "stray two", None).await;
        let deleted = tokio::time::timeout(
            ANSWER_WITHIN,
            crate::mcp_delete_service::mcp_local_delete(
                app.clone(),
                &json!({ "type": "documents", "graphId": GRAPH, "documentIds": ["stray two"] }),
            ),
        )
        .await
        .expect("answered")
        .expect("the delete tool removes a stray row");
        assert_eq!(
            deleted["deleted"][0]["value"]["removedStrayRowIds"],
            json!(["stray two"]),
            "{deleted}"
        );
        assert!(!workspace_rows(&app, GRAPH)
            .await
            .iter()
            .any(|row| row_id(row) == "stray two"));
        create(
            &app,
            GRAPH,
            json!({ "documentId": "doc-after", "title": "After" }),
        )
        .await
        .expect("the graph takes the next write");
        assert_queue_free(&app);
    });
}

/// A hard folder delete removes such rows with the folder (2026-10-14, the
/// hub's call); it used to refuse the whole delete, naming the row.
#[test]
fn a_hard_folder_delete_removes_rows_whose_id_is_not_a_local_id_with_the_folder() {
    const GRAPH: &str = "poison-folder-cascade";
    with_graph(GRAPH, |app| async move {
        crate::mcp_workspace_crdt_service::mcp_local_create_folder(
            app.clone(),
            &json!({ "graphId": GRAPH, "folderId": "folder-held", "name": "Held" }),
        )
        .await
        .expect("folder created");
        crate::mcp_workspace_crdt_service::mcp_local_create_folder(
            app.clone(),
            &json!({ "graphId": GRAPH, "folderId": "folder-inner", "name": "Inner", "parentId": "folder-held" }),
        )
        .await
        .expect("inner folder created");
        create(
            &app,
            GRAPH,
            json!({ "documentId": "doc-sibling", "title": "Sibling", "parentId": "folder-held" }),
        )
        .await
        .expect("a document in the folder");
        create(
            &app,
            GRAPH,
            json!({ "documentId": "doc-outside", "title": "Outside" }),
        )
        .await
        .expect("a document outside the folder");
        client_writes_workspace_row(&app, GRAPH, "stray row", Some("folder-held")).await;
        client_writes_workspace_row(&app, GRAPH, "stray inner", Some("folder-inner")).await;
        client_writes_workspace_row(&app, GRAPH, "stray outside", None).await;

        let deleted = delete_folder_hard(&app, GRAPH, "folder-held")
            .await
            .expect("the folder goes, stray rows and all");
        assert_eq!(
            deleted["value"]["deletedDocumentIds"],
            json!(["doc-sibling"]),
            "{deleted}"
        );
        assert_eq!(
            deleted["value"]["removedStrayRowIds"],
            json!(["stray inner", "stray row"]),
            "{deleted}"
        );
        assert_eq!(
            deleted["value"]["deletedFolderIds"],
            json!(["folder-inner"]),
            "{deleted}"
        );
        assert_queue_free(&app);

        let rows = workspace_rows(&app, GRAPH).await;
        for gone in ["doc-sibling", "stray row", "stray inner"] {
            assert!(
                !rows.iter().any(|row| row_id(row) == gone),
                "{gone} still listed: {rows:?}"
            );
        }
        for kept in ["doc-outside", "stray outside"] {
            assert!(
                rows.iter().any(|row| row_id(row) == kept),
                "{kept} not listed: {rows:?}"
            );
        }
        let graph_dir = crate::graph_paths::existing_graph_dir(&app, GRAPH).unwrap();
        assert!(
            crate::document_tombstone_store::document_is_tombstoned(&graph_dir, "doc-sibling")
                .unwrap()
        );
        assert!(!crate::document_tombstone_store::document_is_tombstoned(
            &graph_dir,
            "doc-outside"
        )
        .unwrap());
        create(
            &app,
            GRAPH,
            json!({ "documentId": "doc-after", "title": "After" }),
        )
        .await
        .expect("the graph takes the next write");
        assert_queue_free(&app);
    });
}

#[test]
fn an_upload_ingest_whose_staged_file_is_not_there_is_refused_at_once() {
    const GRAPH: &str = "poison-upload-pending";
    with_graph(GRAPH, |app| async move {
        let graph_dir = crate::graph_paths::existing_graph_dir(&app, GRAPH).unwrap();
        let missing = graph_dir.join("pending-uploads").join("never-staged.md");
        let refused = enqueue(
            &app,
            "document.uploadIngest",
            GRAPH,
            None,
            json!({
                "pendingOriginalPath": missing.to_string_lossy(),
                "filename": "never-staged.md",
                "mimeType": "text/markdown",
            }),
        )
        .await
        .expect_err("a staged file that is not there is refused");
        assert!(refused.contains("pending upload"), "{refused}");
        assert_queue_free(&app);
        create(
            &app,
            GRAPH,
            json!({ "documentId": "doc-after", "title": "After" }),
        )
        .await
        .expect("the graph takes the next write");
    });
}

#[test]
fn a_write_whose_document_id_is_not_a_local_id_is_refused_at_once() {
    const GRAPH: &str = "poison-write-id";
    with_graph(GRAPH, |app| async move {
        let refused = enqueue(
            &app,
            "document.write",
            GRAPH,
            None,
            json!({ "documentId": "not a local id", "content": "# Written\n\nBody.\n", "format": "markdown" }),
        )
        .await
        .expect_err("an id with spaces is refused");
        assert!(
            refused.contains("document_id contains invalid characters"),
            "{refused}"
        );
        assert_queue_free(&app);
        create(
            &app,
            GRAPH,
            json!({ "documentId": "doc-after", "title": "After" }),
        )
        .await
        .expect("the graph takes the next write");
    });
}

// ─────────────────────────────────────────────────────────────────────────────
// document.uploadIngest and the parser pool (2026-10-14, the hub's call): an
// answer the pool will give again for the same file (a 4xx, an answer with no
// markdown) is final; a 5xx, a timeout or a connection that fails is retried.
// ─────────────────────────────────────────────────────────────────────────────

/// The parser pool's own answers, replayed from a loopback listener: the gate
/// has no Python to run the pool. Captured 2026-10-14 from
/// `platform-next/parser-pool/app.py` (sha256 93a6ac5c…, platform-next
/// `c04bfc4`) under uvicorn 0.54 / FastAPI 0.143, with curl: status line,
/// content type and body byte for byte (the `date` and `server` headers left
/// out). The receipt holds the captures and a run against the pool itself.
pub(crate) mod pool_answers {
    /// POST /parse with a file no engine can open (`not a pdf at all`).
    pub(crate) const NO_MARKDOWN: (u16, &str, &str) = (
        200,
        "application/json",
        r#"{"error":"all parse approaches failed","warnings":["page count unavailable: Failed to open stream","pdf.docling-accurate failed: No module named 'docling'","pdf.pymupdf4llm failed: No module named 'pymupdf4llm'","pdf.fast-text failed: Failed to open stream"],"approachUsed":null,"localFallback":true}"#,
    );
    /// POST under a base URL that is not the pool's (`/nope/parse`).
    pub(crate) const NOT_FOUND: (u16, &str, &str) =
        (404, "application/json", r#"{"detail":"Not Found"}"#);
    /// POST /parse whose body lacks `dataBase64`.
    pub(crate) const UNPROCESSABLE: (u16, &str, &str) = (
        422,
        "application/json",
        r#"{"detail":[{"type":"missing","loc":["body","dataBase64"],"msg":"Field required","input":{"filename":"x.pdf"}}]}"#,
    );
    /// POST /parse with `dataBase64: "abc"` (Python's b64decode raises).
    pub(crate) const SERVER_ERROR: (u16, &str, &str) =
        (500, "text/plain; charset=utf-8", "Internal Server Error");
    /// POST /parse with a one-page PDF reading "Battery charger specimen".
    pub(crate) const PARSED: (u16, &str, &str) = (
        200,
        "application/json",
        r#"{"markdown":"Battery charger specimen","stats":{"pageCount":1,"sizeBytes":844,"parseMs":11},"warnings":["pdf.docling-accurate failed: No module named 'docling'","pdf.pymupdf4llm failed: No module named 'pymupdf4llm'"],"approachUsed":"pdf.fast-text","localFallback":true}"#,
    );
}

/// A loopback listener that answers every request with the answer currently
/// set, counting the requests that reach it.
pub(crate) struct RecordedPool {
    pub(crate) url: String,
    answer: std::sync::Arc<std::sync::Mutex<(u16, &'static str, &'static str)>>,
    requests: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    address: std::net::SocketAddr,
}

impl RecordedPool {
    pub(crate) fn answering(answer: (u16, &'static str, &'static str)) -> Self {
        use std::io::{Read, Write};
        use std::sync::atomic::Ordering;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let answer = std::sync::Arc::new(std::sync::Mutex::new(answer));
        let requests = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (thread_answer, thread_requests, thread_stop) =
            (answer.clone(), requests.clone(), stop.clone());
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                if thread_stop.load(Ordering::SeqCst) {
                    break;
                }
                let Ok(mut stream) = stream else { continue };
                let mut request = Vec::new();
                let mut buffer = [0u8; 8192];
                let header_end = loop {
                    let Ok(read) = stream.read(&mut buffer) else {
                        break None;
                    };
                    if read == 0 {
                        break None;
                    }
                    request.extend_from_slice(&buffer[..read]);
                    if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                        break Some(end + 4);
                    }
                };
                let Some(header_end) = header_end else {
                    continue;
                };
                let headers = String::from_utf8_lossy(&request[..header_end]).to_ascii_lowercase();
                let length = headers
                    .lines()
                    .find_map(|line| line.strip_prefix("content-length:"))
                    .and_then(|value| value.trim().parse::<usize>().ok())
                    .unwrap_or(0);
                while request.len() < header_end + length {
                    match stream.read(&mut buffer) {
                        Ok(0) | Err(_) => break,
                        Ok(read) => request.extend_from_slice(&buffer[..read]),
                    }
                }
                thread_requests.fetch_add(1, Ordering::SeqCst);
                let (status, content_type, body) = *thread_answer.lock().unwrap();
                let reason = match status {
                    200 => "OK",
                    404 => "Not Found",
                    422 => "Unprocessable Content",
                    500 => "Internal Server Error",
                    _ => "Answer",
                };
                let _ = write!(
                    stream,
                    "HTTP/1.1 {status} {reason}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.flush();
            }
        });
        Self {
            url: format!("http://{address}"),
            answer,
            requests,
            stop,
            address,
        }
    }

    pub(crate) fn answer(&self, answer: (u16, &'static str, &'static str)) {
        *self.answer.lock().unwrap() = answer;
    }

    pub(crate) fn requests(&self) -> usize {
        self.requests.load(std::sync::atomic::Ordering::SeqCst)
    }
}

impl Drop for RecordedPool {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
        let _ = std::net::TcpStream::connect(self.address);
    }
}

/// `GARDEN_PARSER_URL` for one case (the profile lock is held), restored
/// after it even when it panics.
pub(crate) struct ParserPoolEnv(Option<std::ffi::OsString>);

impl ParserPoolEnv {
    pub(crate) fn set(url: &str) -> Self {
        let previous = std::env::var_os(super::upload_ingest_ops::PARSER_POOL_ENV);
        std::env::set_var(super::upload_ingest_ops::PARSER_POOL_ENV, url);
        Self(previous)
    }
}

impl Drop for ParserPoolEnv {
    fn drop(&mut self) {
        match self.0.take() {
            Some(value) => std::env::set_var(super::upload_ingest_ops::PARSER_POOL_ENV, value),
            None => std::env::remove_var(super::upload_ingest_ops::PARSER_POOL_ENV),
        }
    }
}

fn pdf_upload_payload(document_id: &str, data_base64: &str) -> Value {
    json!({
        "documentId": document_id,
        "filename": "specimen.pdf",
        "mimeType": "application/pdf",
        "dataBase64": data_base64,
    })
}

fn specimen_pdf_base64() -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(b"%PDF-1.4 specimen")
}

#[test]
fn an_upload_the_parser_pool_refuses_is_refused_at_once_and_blocks_nothing() {
    const GRAPH: &str = "poison-pool-final";
    with_graph(GRAPH, |app| async move {
        let pool = RecordedPool::answering(pool_answers::NOT_FOUND);
        let _env = ParserPoolEnv::set(&pool.url);
        let cases = [
            (
                "doc-pool-404",
                pool_answers::NOT_FOUND,
                "HTTP 404",
                "Not Found",
            ),
            (
                "doc-pool-422",
                pool_answers::UNPROCESSABLE,
                "HTTP 422",
                "Field required",
            ),
            (
                "doc-pool-empty",
                pool_answers::NO_MARKDOWN,
                "all parse approaches failed",
                "Failed to open stream",
            ),
        ];
        for (document_id, answer, first, second) in cases {
            pool.answer(answer);
            let before = pool.requests();
            let refused = enqueue(
                &app,
                "document.uploadIngest",
                GRAPH,
                None,
                pdf_upload_payload(document_id, &specimen_pdf_base64()),
            )
            .await
            .expect_err("a file the pool will never parse is refused");
            assert!(refused.contains("specimen.pdf"), "{refused}");
            assert!(
                refused.contains(first) && refused.contains(second),
                "{refused}"
            );
            assert_eq!(pool.requests(), before + 1, "asked once, not retried");
            assert_queue_free(&app);
        }
        let rows = workspace_rows(&app, GRAPH).await;
        assert!(
            !rows.iter().any(|row| row_id(row).starts_with("doc-pool-")),
            "{rows:?}"
        );
        create(
            &app,
            GRAPH,
            json!({ "documentId": "doc-after", "title": "After" }),
        )
        .await
        .expect("the graph takes the next write");
    });
}

#[test]
fn an_upload_whose_parser_pool_fails_for_a_while_is_retried_until_it_lands() {
    const GRAPH: &str = "poison-pool-transient";
    with_graph(GRAPH, |app| async move {
        // First no pool at all (a closed port: the connection is refused),
        // then the pool's 500, then its answer.
        let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let closed_url = format!("http://{}", closed.local_addr().unwrap());
        drop(closed);
        let env = ParserPoolEnv::set(&closed_url);
        let pool = RecordedPool::answering(pool_answers::SERVER_ERROR);

        let caller = {
            let app = app.clone();
            crate::app_runtime::async_runtime::spawn(async move {
                crate::crdt_queue::enqueue_crdt_operation(
                    app,
                    crate::crdt_operation_types::EnqueueCrdtOperationInput {
                        kind: "document.uploadIngest".to_string(),
                        graph_id: GRAPH.to_string(),
                        document_id: None,
                        payload: pdf_upload_payload("doc-pool-tail", &specimen_pdf_base64()),
                    },
                )
                .await
            })
        };
        let queue = app.state::<crate::crdt_queue::CrdtOperationQueue>();
        let parked = async {
            while queue.retry_wait_count_for_test().unwrap() == 0 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        };
        tokio::time::timeout(ANSWER_WITHIN, parked)
            .await
            .expect("a refused connection is kept for retry");
        assert!(!caller.is_finished(), "the caller still waits");

        drop(env);
        let _env = ParserPoolEnv::set(&pool.url);
        let asked_twice = async {
            while pool.requests() < 2 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        };
        tokio::time::timeout(ANSWER_WITHIN, asked_twice)
            .await
            .expect("a 500 is asked again");
        assert!(
            !caller.is_finished(),
            "the caller still waits through the pool's 500s"
        );
        assert_eq!(
            crate::crdt_operation_journal::recover_pending_crdt_operations(&app)
                .unwrap()
                .len(),
            1,
            "the operation stays journaled while it retries"
        );

        pool.answer(pool_answers::PARSED);
        let ingested = tokio::time::timeout(ANSWER_WITHIN, caller)
            .await
            .expect("the retry lands once the pool answers")
            .expect("caller task")
            .expect("ingested");
        assert_eq!(ingested["documentId"], "doc-pool-tail", "{ingested}");
        assert_queue_free(&app);
        let rows = workspace_rows(&app, GRAPH).await;
        assert!(
            rows.iter().any(|row| row_id(row) == "doc-pool-tail"),
            "{rows:?}"
        );
    });
}

/// The pool answers base64 it cannot decode with a 500 (its `b64decode`
/// raises), which is retried: such a payload is refused before it is sent.
#[test]
fn an_upload_whose_base64_does_not_decode_is_refused_before_the_parser_pool() {
    const GRAPH: &str = "poison-pool-base64";
    with_graph(GRAPH, |app| async move {
        let pool = RecordedPool::answering(pool_answers::SERVER_ERROR);
        let _env = ParserPoolEnv::set(&pool.url);
        let refused = enqueue(
            &app,
            "document.uploadIngest",
            GRAPH,
            None,
            pdf_upload_payload("doc-pool-b64", "abc"),
        )
        .await
        .expect_err("base64 that does not decode is refused");
        assert!(refused.contains("invalid base64"), "{refused}");
        assert_eq!(pool.requests(), 0, "never sent to the pool");
        assert_queue_free(&app);
        create(
            &app,
            GRAPH,
            json!({ "documentId": "doc-after", "title": "After" }),
        )
        .await
        .expect("the graph takes the next write");
    });
}

// ─────────────────────────────────────────────────────────────────────────────
// Folder, artifact and wire keys that make no IRI (2026-10-14, the hub's call):
// left out of the projection with a warning, and refused where an op creates
// them.
// ─────────────────────────────────────────────────────────────────────────────

/// An entry in one of the workspace Y.Doc's maps, applied the way the
/// WebSocket host applies a client's update.
async fn client_writes_workspace_entry(
    app: &crate::app_runtime::AppHandle,
    graph_id: &str,
    map: &str,
    key: &str,
    fields: Vec<(&str, yrs::Any)>,
) {
    use yrs::{Doc, Map, MapPrelim, ReadTxn, StateVector, Transact, WriteTxn};
    let client = Doc::new();
    {
        let mut txn = client.transact_mut();
        let entries = txn.get_or_insert_map(map);
        let entry = entries.insert(&mut txn, key, MapPrelim::default());
        for (field, value) in fields {
            entry.insert(&mut txn, field, value);
        }
    }
    let update = client
        .transact()
        .encode_state_as_update_v1(&StateVector::default());
    let graph_dir = crate::graph_paths::existing_graph_dir(app, graph_id).unwrap();
    let room = super::workspace_ops::workspace_room(app, graph_id, &graph_dir)
        .await
        .unwrap();
    assert!(
        room.apply_client_update(&update).await.unwrap(),
        "the entry was applied"
    );
}

fn projected_subjects(app: &crate::app_runtime::AppHandle, graph_id: &str) -> Vec<String> {
    let graph_dir = crate::graph_paths::existing_graph_dir(app, graph_id).unwrap();
    let store = crate::rdf_store_service::open_graph_store(&graph_dir).unwrap();
    let projection = crate::rdf_authority::workspace_projection_graph_iri(graph_id);
    crate::rdf_query_service::execute_sparql_query(
        &store,
        &format!("SELECT DISTINCT ?s WHERE {{ GRAPH <{projection}> {{ ?s ?p ?o }} }}"),
    )
    .unwrap()
    .rows
    .iter()
    .map(|row| row["s"].clone())
    .collect()
}

#[test]
fn folder_artifact_and_wire_keys_that_make_no_iri_block_no_other_write() {
    const GRAPH: &str = "poison-stray-keys";
    use yrs::Any;
    with_graph(GRAPH, |app| async move {
        create(
            &app,
            GRAPH,
            json!({ "documentId": "doc-before", "title": "Before" }),
        )
        .await
        .expect("created");
        client_writes_workspace_entry(
            &app,
            GRAPH,
            "folders",
            "stray folder",
            vec![
                ("name", Any::from("Stray folder")),
                ("parentId", Any::Null),
                ("order", Any::Number(1.0)),
            ],
        )
        .await;
        client_writes_workspace_entry(
            &app,
            GRAPH,
            "artifacts",
            "stray artifact",
            vec![
                ("name", Any::from("Stray artifact")),
                ("parentId", Any::Null),
                ("order", Any::Number(2.0)),
                ("fileType", Any::from("pdf")),
                ("status", Any::from("ready")),
                ("mimeType", Any::from("application/pdf")),
            ],
        )
        .await;
        client_writes_workspace_entry(
            &app,
            GRAPH,
            "wires",
            "stray wire",
            vec![
                ("sourceDocumentId", Any::from("doc-before")),
                ("targetDocumentId", Any::from("doc-before")),
                ("targetGraphId", Any::from(GRAPH)),
                ("predicate", Any::from("isWiredTo")),
            ],
        )
        .await;
        // A row with a good id inside the stray folder: its parent makes no IRI.
        client_writes_workspace_entry(
            &app,
            GRAPH,
            "documents",
            "doc-in-stray",
            vec![
                ("title", Any::from("In the stray folder")),
                ("parentId", Any::from("stray folder")),
                ("section", Any::from("documents")),
                ("order", Any::Number(3.0)),
                ("createdAt", Any::Number(1.0)),
                ("updatedAt", Any::Number(1.0)),
                ("readOnly", Any::Bool(false)),
            ],
        )
        .await;

        create(
            &app,
            GRAPH,
            json!({ "documentId": "doc-after", "title": "After" }),
        )
        .await
        .expect("a create beside the stray keys lands");
        enqueue(
            &app,
            "workspace.updateDocument",
            GRAPH,
            Some("doc-before"),
            json!({ "documentId": "doc-before", "title": "Before, renamed" }),
        )
        .await
        .expect("a rename beside the stray keys lands");
        assert_queue_free(&app);

        let subjects = projected_subjects(&app, GRAPH);
        assert!(
            subjects
                .iter()
                .any(|subject| subject.ends_with(":document:doc-after>")),
            "{subjects:?}"
        );
        assert!(
            subjects
                .iter()
                .any(|subject| subject.ends_with(":document:doc-in-stray>")),
            "the row in the stray folder is projected, without its parent: {subjects:?}"
        );
        assert!(
            !subjects.iter().any(|subject| subject.contains("stray ")),
            "{subjects:?}"
        );
        let rows = workspace_rows(&app, GRAPH).await;
        let renamed = rows
            .iter()
            .find(|row| row_id(row) == "doc-before")
            .expect("listed");
        assert_eq!(renamed["title"], "Before, renamed", "{renamed}");
    });
}

#[test]
fn creating_a_folder_artifact_or_wire_that_makes_no_iri_is_refused_at_once() {
    const GRAPH: &str = "poison-create-keys";
    with_graph(GRAPH, |app| async move {
        create(
            &app,
            GRAPH,
            json!({ "documentId": "doc-source", "title": "Source" }),
        )
        .await
        .expect("created");
        let refusals = [
            (
                "workspace.createFolder",
                None,
                json!({ "folderId": "stray folder", "name": "Stray" }),
                "stray folder",
            ),
            (
                "workspace.createFolder",
                None,
                json!({ "folderId": "folder-ok", "name": "Ok", "parentId": "stray folder" }),
                "stray folder",
            ),
            (
                "workspace.putArtifact",
                Some("stray artifact"),
                json!({ "label": "Stray", "mimeType": "application/pdf" }),
                "stray artifact",
            ),
            (
                "workspace.createWire",
                Some("doc-source"),
                json!({ "wireId": "stray wire", "targetDocumentId": "doc-source" }),
                "stray wire",
            ),
            (
                "workspace.createWire",
                Some("doc-source"),
                json!({ "targetDocumentId": "doc-source", "predicate": "has part" }),
                "has part",
            ),
        ];
        for (kind, document_id, payload, named) in refusals {
            let refused = enqueue(&app, kind, GRAPH, document_id, payload.clone())
                .await
                .expect_err(&format!("{kind} {payload} is refused"));
            assert!(
                refused.contains("not an IRI") && refused.contains(named),
                "{kind}: {refused}"
            );
            assert_queue_free(&app);
        }
        let graph_dir = crate::graph_paths::existing_graph_dir(&app, GRAPH).unwrap();
        let snapshot: Value =
            crate::storage::read_json(&crate::ydoc_paths::workspace_snapshot_path(&graph_dir))
                .unwrap();
        for map in ["folders", "artifacts", "wires"] {
            assert!(
                snapshot[map]
                    .as_array()
                    .map_or(true, |entries| entries.is_empty()),
                "nothing was written to {map}: {}",
                snapshot[map]
            );
        }
        crate::mcp_workspace_crdt_service::mcp_local_create_folder(
            app.clone(),
            &json!({ "graphId": GRAPH, "folderId": "folder-fine", "name": "Fine" }),
        )
        .await
        .expect("the graph takes the next write");
        assert_queue_free(&app);
    });
}

/// The projection itself (a pure function of the snapshot): what makes no IRI
/// is left out, including a `>` that used to split a serialized triple.
#[test]
fn ids_that_make_no_iri_are_left_out_of_the_workspace_projection() {
    let snapshot = json!({
        "graphId": "graph-a",
        "folders": [
            { "id": "folder-a", "name": "A", "order": 0 },
            { "id": "folder>b", "name": "B", "order": 1 },
        ],
        "documents": [
            { "id": "doc-a", "title": "A", "parentId": "folder>b", "order": 0 },
        ],
        "artifacts": [
            { "id": "artifact a", "name": "a.pdf", "mimeType": "application/pdf", "order": 0 },
            { "id": "artifact-b", "name": "b.pdf", "mimeType": "application/pdf", "order": 1, "ingestedDocId": "doc b" },
        ],
        "wires": [
            { "id": "wire-a", "sourceDocumentId": "doc-a", "targetDocumentId": "doc-a", "predicate": "has part" },
            { "id": "wire b", "sourceDocumentId": "doc-a", "targetDocumentId": "doc-a", "predicate": "isWiredTo" },
        ],
    });
    let triples = crate::rdf_workspace_materializer::workspace_entity_triples("graph-a", &snapshot);
    let lines: Vec<String> = triples.iter().map(crate::rdf::format_rdf_triple).collect();
    // Every line is N-Triples a store or a SHACL validator will read.
    let text = lines.join("\n");
    let store = oxigraph::store::Store::new().unwrap();
    store
        .load_from_slice(
            oxigraph::io::RdfParser::from_format(oxigraph::io::RdfFormat::NTriples),
            text.as_bytes(),
        )
        .unwrap_or_else(|error| panic!("{error}:\n{text}"));
    let has = |needle: &str| lines.iter().any(|line| line.contains(needle));
    assert!(has(":folder:folder-a>"));
    assert!(!has("folder>b"), "{lines:#?}");
    assert!(
        has(":document:doc-a>"),
        "the document stays, without its parent"
    );
    assert!(!has("artifact a"));
    assert!(
        has(":artifact:artifact-b>"),
        "the artifact stays, without its link"
    );
    assert!(!has("doc b"));
    assert!(
        has(":wire:wire-a>"),
        "the wire stays, without its predicate"
    );
    assert!(!has("has part"));
    assert!(!has("wire b"));
    // Every line now converts without panicking.
    for triple in &triples {
        crate::rdf_record_materializer::rdf_triple_to_term(triple);
    }
}
