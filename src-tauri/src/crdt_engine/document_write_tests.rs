//! Tests for the minimal-edit write path in `document_ops` (owner U10; DESIGN §2.10, §4.3).
//!
//! `write_document` and the markdown ingest used to delete the whole `content` fragment and append
//! a new one. They now plan against the room's Y.Doc and emit only what differs. These tests drive
//! the real operations (`document.write`, `document.ingestMarkdownOriginal`) through the real
//! queue against a disposable graph, then look at the room's Y.Doc, the broadcast updates and the
//! stored record. No mocks.
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::sync::broadcast::error::TryRecvError;
use yrs::branch::{Branch, BranchID};
use yrs::{ReadTxn, Transact, XmlFragment, XmlOut};

use super::rooms::{Room, RoomRegistry};

const GRAPH: &str = "document-write";

fn with_graph(test: impl FnOnce(crate::app_runtime::AppHandle)) {
    let _serial = crate::tauri_runtime::profile_env_serial()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let previous = std::env::var_os("GARDEN_PROFILE_DIR");
    let profile =
        std::env::temp_dir().join(format!("garden-document-write-{}", uuid::Uuid::new_v4()));
    std::env::set_var("GARDEN_PROFILE_DIR", &profile);
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let app = crate::tauri_runtime::build_mock_app_for_tests(true);
        crate::graph_service::create_graph_service(
            &app,
            crate::graph_service::CreateGraphInput {
                graph_id: Some(GRAPH.into()),
                title: "Document write specimen".into(),
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

fn run<F: std::future::Future<Output = ()>>(future: F) {
    crate::app_runtime::async_runtime::block_on(future);
}

// ---------------------------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------------------------

async fn create(app: &crate::app_runtime::AppHandle, document_id: &str) {
    crate::mcp_workspace_crdt_service::mcp_local_create_document(
        app.clone(),
        &json!({"graphId": GRAPH, "documentId": document_id, "title": document_id}),
    )
    .await
    .unwrap();
}

async fn enqueue(
    app: &crate::app_runtime::AppHandle,
    kind: &str,
    document_id: Option<&str>,
    payload: Value,
) -> Result<Value, String> {
    crate::crdt_queue::enqueue_crdt_operation(
        app.clone(),
        crate::crdt_operation_types::EnqueueCrdtOperationInput {
            kind: kind.into(),
            graph_id: GRAPH.into(),
            document_id: document_id.map(str::to_string),
            payload,
        },
    )
    .await
}

/// `document.write` of a markdown string; `expected` is the `expectedRevision`.
async fn write_md_at(
    app: &crate::app_runtime::AppHandle,
    document_id: &str,
    markdown: &str,
    expected: Option<u64>,
) -> Result<Value, String> {
    let mut payload = json!({"documentId": document_id, "content": markdown, "format": "markdown"});
    if let Some(expected) = expected {
        payload["expectedRevision"] = json!(expected);
    }
    enqueue(app, "document.write", Some(document_id), payload).await
}

async fn write_md(
    app: &crate::app_runtime::AppHandle,
    document_id: &str,
    markdown: &str,
) -> Value {
    write_md_at(app, document_id, markdown, None).await.unwrap()
}

fn stored(app: &crate::app_runtime::AppHandle, document_id: &str) -> Value {
    let read = crate::document_mcp_service::mcp_local_read_document(
        app.clone(),
        &json!({"graph_id": GRAPH, "document_id": document_id, "format": "json", "maxChars": 1_000_000}),
    )
    .unwrap();
    let parsed: Value = serde_json::from_str(read["content"].as_str().unwrap()).unwrap();
    parsed["tiptapJson"].clone()
}

fn markdown(app: &crate::app_runtime::AppHandle, document_id: &str) -> String {
    crate::document_mcp_service::mcp_local_read_document(
        app.clone(),
        &json!({"graph_id": GRAPH, "document_id": document_id, "format": "markdown", "maxChars": 1_000_000}),
    )
    .unwrap()["content"]
        .as_str()
        .unwrap()
        .to_string()
}

/// The `data-block-id` of every top-level block, in order.
fn block_ids(doc: &Value) -> Vec<String> {
    doc["content"]
        .as_array()
        .unwrap()
        .iter()
        .map(|block| {
            block["attrs"]["data-block-id"]
                .as_str()
                .unwrap_or_else(|| panic!("a block without an id: {block}"))
                .to_string()
        })
        .collect()
}

fn node_text(node: &Value) -> String {
    if node["type"] == "text" {
        return node["text"].as_str().unwrap_or("").to_string();
    }
    node["content"]
        .as_array()
        .into_iter()
        .flatten()
        .map(node_text)
        .collect()
}

fn block_texts(doc: &Value) -> Vec<String> {
    doc["content"]
        .as_array()
        .unwrap()
        .iter()
        .map(node_text)
        .collect()
}

/// The revision on the stored record.
fn revision(app: &crate::app_runtime::AppHandle, document_id: &str) -> u64 {
    let graph_dir = crate::graph_paths::existing_graph_dir(app, GRAPH).unwrap();
    let manifest = crate::document_paths::document_dir(&graph_dir, document_id)
        .unwrap()
        .join("document.json");
    let record: Value = serde_json::from_slice(&std::fs::read(manifest).unwrap()).unwrap();
    record["revision"].as_u64().unwrap()
}

async fn room(app: &crate::app_runtime::AppHandle, document_id: &str) -> Arc<Room> {
    app.state::<RoomRegistry>()
        .peek(&format!("doc:{GRAPH}:{document_id}"))
        .await
        .expect("the document's room is hosted")
}

fn branch_id<T: AsRef<Branch>>(branch: &T) -> BranchID {
    branch.as_ref().id()
}

fn collect_ids<T: ReadTxn>(txn: &T, node: &XmlOut, out: &mut Vec<BranchID>) {
    match node {
        XmlOut::Element(element) => {
            out.push(branch_id(element));
            for child in element.children(txn) {
                collect_ids(txn, &child, out);
            }
        }
        XmlOut::Text(text) => out.push(branch_id(text)),
        XmlOut::Fragment(fragment) => {
            for child in fragment.children(txn) {
                collect_ids(txn, &child, out);
            }
        }
    }
}

/// Every Y.XmlElement and Y.XmlText branch id of each top-level block, in order.
async fn branch_ids(room: &Room) -> Vec<Vec<BranchID>> {
    room.with_doc(|doc| {
        let fragment = doc.get_or_insert_xml_fragment("content");
        let txn = doc.transact();
        fragment
            .children(&txn)
            .map(|block| {
                let mut ids = Vec::new();
                collect_ids(&txn, &block, &mut ids);
                ids
            })
            .collect()
    })
    .await
}

fn no_update(receiver: &mut tokio::sync::broadcast::Receiver<Vec<u8>>) -> bool {
    matches!(receiver.try_recv(), Err(TryRecvError::Empty))
}

fn ops_are_zero(result: &Value) -> bool {
    result["crdt"]["ops"]
        .as_object()
        .unwrap_or_else(|| panic!("no crdt.ops in {result}"))
        .values()
        .all(|count| count.as_u64() == Some(0))
}

const FOUR_BLOCKS: &str =
    "# Title\n\nFirst paragraph.\n\nSecond paragraph.\n\nThird paragraph.\n";
const FOUR_BLOCKS_EDITED: &str =
    "# Title\n\nFirst paragraph.\n\nSecond paragraph, edited.\n\nThird paragraph.\n";

// ---------------------------------------------------------------------------------------------
// U10 tests
// ---------------------------------------------------------------------------------------------

#[test]
fn same_markdown_written_back_is_zero_ops_and_no_revision() {
    with_graph(|app| {
        run(async {
            create(&app, "doc-same").await;
            let first = write_md(&app, "doc-same", FOUR_BLOCKS).await;
            assert_eq!(first["crdt"]["persisted"], true, "a real write persists");
            let revision_before = revision(&app, "doc-same");
            let room = room(&app, "doc-same").await;
            let ids_before = branch_ids(&room).await;
            let mut updates = room.subscribe_updates_for_test();

            // The parser mints fresh random block ids on every write; none of that is a change.
            let again = write_md(&app, "doc-same", FOUR_BLOCKS).await;
            assert!(ops_are_zero(&again), "no Yjs operation: {again}");
            assert_eq!(again["crdt"]["persisted"], false, "{again}");
            assert_eq!(again["crdt"]["commentsWritten"], 0, "{again}");
            assert_eq!(
                revision(&app, "doc-same"),
                revision_before,
                "an unchanged write is not a new revision"
            );
            assert!(no_update(&mut updates), "nothing was broadcast");
            assert!(
                branch_ids(&room).await == ids_before,
                "every Y type kept its identity"
            );

            // What an agent reads, sent back as it read it, is the same document.
            let read_back = markdown(&app, "doc-same");
            let echoed = write_md(&app, "doc-same", &read_back).await;
            assert!(ops_are_zero(&echoed), "read-then-write is a no-op: {echoed}");
            assert_eq!(revision(&app, "doc-same"), revision_before);
            assert!(no_update(&mut updates));
        });
    });
}

#[test]
fn one_paragraph_markdown_edit_keeps_other_block_ids() {
    with_graph(|app| {
        run(async {
            create(&app, "doc-edit").await;
            write_md(&app, "doc-edit", FOUR_BLOCKS).await;
            let ids_before = block_ids(&stored(&app, "doc-edit"));
            let room = room(&app, "doc-edit").await;
            let branches_before = branch_ids(&room).await;

            let result = write_md(&app, "doc-edit", FOUR_BLOCKS_EDITED).await;
            let after = stored(&app, "doc-edit");
            assert_eq!(
                block_ids(&after),
                ids_before,
                "every block keeps its id, the edited one too: the parser's fresh ids are dropped"
            );
            assert_eq!(block_texts(&after)[2], "Second paragraph, edited.");
            assert_eq!(block_texts(&after)[1], "First paragraph.");

            let branches_after = branch_ids(&room).await;
            assert_eq!(branches_after.len(), branches_before.len());
            for (index, (before, after)) in branches_before.iter().zip(&branches_after).enumerate()
            {
                assert!(
                    before == after,
                    "block {index} keeps its Yjs identity, the edited paragraph's Y.XmlText included"
                );
            }
            let ops = &result["crdt"]["ops"];
            assert_eq!(ops["nodesInserted"], 0, "{result}");
            assert_eq!(ops["nodesRemoved"], 0, "{result}");
            assert!(
                ops["textBytesInserted"].as_u64().unwrap() < 20
                    && ops["textBytesDeleted"].as_u64().unwrap() < 20,
                "only the changed words are written: {result}"
            );
        });
    });
}

#[test]
fn preflight_replay_is_a_planner_noop() {
    with_graph(|app| {
        run(async {
            create(&app, "doc-replay").await;
            let r0 = revision(&app, "doc-replay");
            write_md_at(&app, "doc-replay", FOUR_BLOCKS, Some(r0))
                .await
                .unwrap();
            let r1 = revision(&app, "doc-replay");
            assert_eq!(r1, r0 + 1);
            write_md_at(&app, "doc-replay", FOUR_BLOCKS_EDITED, Some(r1))
                .await
                .unwrap();
            let r2 = revision(&app, "doc-replay");
            assert_eq!(r2, r1 + 1);
            let room = room(&app, "doc-replay").await;
            let mut updates = room.subscribe_updates_for_test();
            let ids_before = branch_ids(&room).await;

            // The retry of the write that already applied: its parse minted new random ids, so the
            // content is not equal byte for byte; to the planner it is a no-op.
            let replay = write_md_at(&app, "doc-replay", FOUR_BLOCKS_EDITED, Some(r1))
                .await
                .expect("a write that is already applied is a replay, not a conflict");
            assert_eq!(replay["crdt"]["replayed"], true, "{replay}");
            assert!(ops_are_zero(&replay));
            assert_eq!(revision(&app, "doc-replay"), r2, "a replay is not a revision");
            assert!(no_update(&mut updates));
            assert!(branch_ids(&room).await == ids_before);

            // The same revision with other content is a conflict.
            let conflict = write_md_at(
                &app,
                "doc-replay",
                "# Title\n\nSomething else entirely.\n",
                Some(r1),
            )
            .await
            .expect_err("different content at expected+1 is not a replay");
            assert!(conflict.contains("replay content differs"), "{conflict}");
            // And a revision further behind is refused outright.
            let stale = write_md_at(&app, "doc-replay", FOUR_BLOCKS_EDITED, Some(r0))
                .await
                .expect_err("two revisions behind");
            assert!(stale.contains("revision conflict"), "{stale}");
            assert_eq!(revision(&app, "doc-replay"), r2);
        });
    });
}

#[test]
fn expected_revision_equal_write_still_bumps_revision() {
    with_graph(|app| {
        run(async {
            create(&app, "doc-expected").await;
            write_md(&app, "doc-expected", FOUR_BLOCKS).await;
            let before = revision(&app, "doc-expected");

            // Today's contract: a write at `expected` is a new revision even when nothing differs.
            let equal = write_md_at(&app, "doc-expected", FOUR_BLOCKS, Some(before))
                .await
                .unwrap();
            assert!(ops_are_zero(&equal), "the Y.Doc is untouched: {equal}");
            assert_eq!(
                equal["crdt"]["persisted"], true,
                "with expectedRevision the write persists: {equal}"
            );
            assert_eq!(revision(&app, "doc-expected"), before + 1);
        });
    });
}

#[test]
fn whiteboard_and_flow_board_writes_are_refused() {
    with_graph(|app| {
        run(async {
            for (document_id, kind, tool) in [
                ("doc-wb", "whiteboard", "whiteboard_apply"),
                ("doc-flow", "flow-board", "flow_seed_board"),
            ] {
                create(&app, document_id).await;
                super::workspace_ops::upsert_document_entry(
                    &app,
                    GRAPH,
                    &json!({"documentId": document_id, "title": document_id,
                            "documentKind": kind, "order": 1, "updatedAt": 1}),
                    &format!("declare-{document_id}"),
                )
                .await
                .unwrap();
                let revision_before = revision(&app, document_id);

                let refused = write_md_at(&app, document_id, "# Not a board\n", None)
                    .await
                    .expect_err("a scene kind takes no content write");
                assert!(refused.contains(tool), "names the right tool: {refused}");
                let refused_json = enqueue(
                    &app,
                    "document.write",
                    Some(document_id),
                    json!({"documentId": document_id,
                           "tiptapJson": {"type": "doc", "content": [
                               {"type": "paragraph", "content": [{"type": "text", "text": "x"}]}]}}),
                )
                .await
                .expect_err("tiptapJson is refused too");
                assert!(refused_json.contains(tool), "{refused_json}");

                assert_eq!(revision(&app, document_id), revision_before, "nothing saved");
                if let Some(room) = app
                    .state::<RoomRegistry>()
                    .peek(&format!("doc:{GRAPH}:{document_id}"))
                    .await
                {
                    let has_content_root = room
                        .with_doc(|doc| doc.transact().get_xml_fragment("content").is_some())
                        .await;
                    assert!(
                        !has_content_root,
                        "{kind}: no `content` root was added to the board"
                    );
                }
            }

            // An ordinary document in the same graph still takes the write.
            create(&app, "doc-text").await;
            write_md(&app, "doc-text", "# Text\n\nStill writable.\n").await;
        });
    });
}

fn ingest_payload(document_id: &str, markdown: &str) -> Value {
    use base64::Engine as _;
    json!({
        "documentId": document_id,
        "markdown": markdown,
        "filename": "specimen.pdf",
        "mimeType": "application/pdf",
        "dataBase64": base64::engine::general_purpose::STANDARD.encode(b"%PDF-1.4 specimen"),
    })
}

#[test]
fn ingest_rerun_is_minimal() {
    with_graph(|app| {
        run(async {
            let ingest = |markdown: &'static str| {
                let app = app.clone();
                async move {
                    enqueue(
                        &app,
                        "document.ingestMarkdownOriginal",
                        None,
                        ingest_payload("doc-ingest", markdown),
                    )
                    .await
                    .unwrap()
                }
            };
            ingest(FOUR_BLOCKS).await;
            let room = room(&app, "doc-ingest").await;
            let branches_before = branch_ids(&room).await;
            let ids_before = block_ids(&stored(&app, "doc-ingest"));
            let mut updates = room.subscribe_updates_for_test();

            // The same pages again: the parser mints new ids, the room changes nothing.
            ingest(FOUR_BLOCKS).await;
            assert!(no_update(&mut updates), "a re-ingest of equal text writes nothing");
            assert!(branch_ids(&room).await == branches_before);
            assert_eq!(block_ids(&stored(&app, "doc-ingest")), ids_before);

            // One page differs: only that page changes.
            ingest(FOUR_BLOCKS_EDITED).await;
            assert!(updates.try_recv().is_ok(), "the changed page was broadcast");
            let branches_after = branch_ids(&room).await;
            assert!(
                branches_after == branches_before,
                "an edited paragraph keeps its Y types; every other block is untouched"
            );
            let after = stored(&app, "doc-ingest");
            assert_eq!(block_ids(&after), ids_before);
            assert_eq!(block_texts(&after)[2], "Second paragraph, edited.");
        });
    });
}

#[test]
fn comments_unchanged_are_not_reinserted() {
    with_graph(|app| {
        run(async {
            create(&app, "doc-comments").await;
            let write = |comment_text: &'static str| {
                let app = app.clone();
                async move {
                    enqueue(
                        &app,
                        "document.write",
                        Some("doc-comments"),
                        json!({
                            "documentId": "doc-comments",
                            "tiptapJson": {"type": "doc", "content": [{
                                "type": "paragraph",
                                "attrs": {"data-block-id": "p1"},
                                "content": [{"type": "text", "text": "Annotated text."}],
                            }]},
                            "comments": {"c1": {"text": comment_text, "createdAt": 1}},
                        }),
                    )
                    .await
                    .unwrap()
                }
            };
            let first = write("first note").await;
            assert_eq!(first["crdt"]["commentsWritten"], 1, "{first}");
            let revision_before = revision(&app, "doc-comments");
            let room = room(&app, "doc-comments").await;
            let mut updates = room.subscribe_updates_for_test();

            let same = write("first note").await;
            assert_eq!(same["crdt"]["commentsWritten"], 0, "{same}");
            assert!(ops_are_zero(&same));
            assert!(
                no_update(&mut updates),
                "an unchanged comment is not written to the Y.Map again"
            );
            assert_eq!(revision(&app, "doc-comments"), revision_before);

            let changed = write("second note").await;
            assert_eq!(changed["crdt"]["commentsWritten"], 1, "{changed}");
            assert!(updates.try_recv().is_ok(), "a changed comment is broadcast");
            let text = room
                .with_doc(|doc| {
                    use yrs::{Map as _, Out};
                    let txn = doc.transact();
                    let comments = txn.get_map("comments").expect("comments map");
                    match comments.get(&txn, "c1") {
                        Some(Out::Any(any)) => any.to_string(),
                        other => panic!("comment c1 is {other:?}"),
                    }
                })
                .await;
            assert!(text.contains("second note"), "{text}");
        });
    });
}

#[test]
fn crdt_result_field_reports_ops() {
    with_graph(|app| {
        run(async {
            create(&app, "doc-report").await;
            let first = write_md(&app, "doc-report", FOUR_BLOCKS).await;
            let crdt = &first["crdt"];
            assert!(
                crdt["ops"]["nodesInserted"].as_u64().unwrap() >= 4,
                "the first write inserts the document: {first}"
            );
            assert!(crdt["plan"]["inserted"].as_u64().unwrap() >= 4, "{first}");
            assert_eq!(crdt["persisted"], true);
            assert!(crdt["clientId"].is_u64(), "{first}");
            let clocks = crdt["clocks"].as_array().unwrap();
            assert!(
                clocks[1].as_u64().unwrap() > clocks[0].as_u64().unwrap(),
                "the writer's clock moved: {first}"
            );
            for stage in ["read", "plan", "emit"] {
                assert!(crdt["ms"][stage].is_number(), "{stage} timed: {first}");
            }
            assert_eq!(crdt["liveEdits"]["kept"], json!([]));
            assert_eq!(crdt["liveEdits"]["conflicts"], json!([]));

            let edit = write_md(&app, "doc-report", FOUR_BLOCKS_EDITED).await;
            let crdt = &edit["crdt"];
            assert_eq!(crdt["ops"]["nodesInserted"], 0, "{edit}");
            assert_eq!(crdt["ops"]["nodesRemoved"], 0, "{edit}");
            assert!(crdt["ops"]["textBytesInserted"].as_u64().unwrap() > 0);
            assert_eq!(crdt["plan"]["patched"], 1, "{edit}");
            assert_eq!(crdt["plan"]["inserted"], 0);
            assert_eq!(crdt["plan"]["removed"], 0);
            assert!(crdt["plan"]["kept"].as_u64().unwrap() >= 2);
            assert_eq!(crdt["persisted"], true);
            // The MCP-facing result is unchanged apart from the new field.
            assert!(edit["warnings"].is_array());
            assert!(edit["blockIds"].is_array());
        });
    });
}
