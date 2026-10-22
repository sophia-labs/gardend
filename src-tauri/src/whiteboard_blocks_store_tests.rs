//! G1's store tests for shapes as blocks (INTERFACES.md): a real graph and live board rooms. Owned by G1.
//!
//! A board is made with `whiteboard_create`, drawn on by writing the v2 semantic fixture's elements into its live
//! room the way a browser's y-websocket client does (one stringified element per key of `elements`), and persisted
//! through the real `persist_room_document` tail. Everything after that is read back through the real record, the
//! real search projection and the real dispatch table. No mocks.
use std::path::PathBuf;
use std::sync::Arc;

use serde_json::{json, Value};

use crate::crdt_engine::rooms::RoomRegistry;
use crate::local_jobs::LocalJobRegistry;
use crate::mcp_dispatch_registry::{lookup, McpCallCtx};
use crate::whiteboard::scene::{Element, Scene};

const GRAPH: &str = "wbs-g1";
const TITLE: &str = "Dream-work board";

const V2: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/whiteboard/whiteboard-fixture-v2-semantic.json"
));

/// The shape ids of the v2 fixture in reading order (40 px bands, then left to right): the two top shapes, the
/// frame, the card (y 290) and then the linked box (y 300).
const SHAPES: [&str; 5] = [
    "box0000000000000000001",
    "note000000000000000001",
    "frm0000000000000000001",
    "crd0000000000000000001",
    "lnk0000000000000000001",
];

struct Env {
    app: crate::app_runtime::AppHandle,
    jobs: Arc<LocalJobRegistry>,
}

impl Env {
    fn call(&self, name: &str, mut args: Value) -> Result<Value, String> {
        if args.get("graphId").is_none() {
            args["graphId"] = json!(GRAPH);
        }
        let entry = lookup(name).unwrap_or_else(|| panic!("dispatch entry for {name}"));
        let ctx = McpCallCtx {
            app: self.app.clone(),
            jobs: &self.jobs,
        };
        run((entry.handler)(&ctx, &args)).map_err(|error| error.to_string())
    }

    /// A board with the v2 fixture drawn on it, persisted. Returns its document id.
    fn draw_v2_board(&self) -> String {
        let doc = self
            .call("whiteboard_create", json!({ "title": TITLE }))
            .expect("create whiteboard")["documentId"]
            .as_str()
            .expect("document id")
            .to_string();
        let fixture: Value = serde_json::from_str(V2).expect("fixture parses");
        let mut elements = std::collections::BTreeMap::new();
        for el in fixture["elements"].as_array().expect("elements") {
            let el: Element = el.as_object().expect("element object").clone();
            elements.insert(el["id"].as_str().expect("id").to_string(), el);
        }
        let mut scene = Scene::from_elements(elements);
        scene.dirty = scene.elements.keys().cloned().collect();
        let graph_dir =
            crate::graph_paths::existing_graph_dir(&self.app, GRAPH).expect("graph dir");
        let registry = self.app.state::<RoomRegistry>();
        let room = run(registry.get_or_create(
            &format!("doc:{GRAPH}:{doc}"),
            crate::ydoc_paths::document_ydoc_state_path(&graph_dir, &doc),
        ))
        .expect("board room");
        run(room.update_doc(|_doc, txn| {
            crate::whiteboard::ops::write_dirty(txn, &scene);
            Ok(())
        }))
        .expect("draw");
        run(crate::crdt_engine::document_ops::persist_room_document(
            &self.app,
            GRAPH,
            &doc,
            TITLE,
            &room,
            "op-wbs-g1-draw",
        ))
        .expect("persist the board");
        doc
    }

    fn record(&self, doc: &str) -> crate::document_types::DocumentRecord {
        crate::document_service::read_document(self.app.clone(), GRAPH.to_string(), doc.to_string())
            .expect("stored record")
    }
}

fn run<F: std::future::Future>(future: F) -> F::Output {
    crate::app_runtime::async_runtime::block_on(future)
}

fn with_env(test: impl FnOnce(&Env)) {
    let _serial = crate::tauri_runtime::profile_env_serial()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let previous = std::env::var_os("GARDEN_PROFILE_DIR");
    let profile: PathBuf =
        std::env::temp_dir().join(format!("garden-wbs-g1-{}", uuid::Uuid::new_v4()));
    std::env::set_var("GARDEN_PROFILE_DIR", &profile);
    let jobs_dir =
        std::env::temp_dir().join(format!("garden-wbs-g1-jobs-{}", uuid::Uuid::new_v4()));
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let app = crate::tauri_runtime::build_mock_app_for_tests(true);
        crate::graph_service::create_graph_service(
            &app,
            crate::graph_service::CreateGraphInput {
                graph_id: Some(GRAPH.into()),
                title: "Whiteboard blocks specimen".into(),
                description: None,
                operation_id: None,
            },
        )
        .unwrap();
        let jobs = Arc::new(LocalJobRegistry::new(jobs_dir.clone()).expect("job registry"));
        test(&Env { app, jobs });
    }));
    match previous {
        Some(value) => std::env::set_var("GARDEN_PROFILE_DIR", value),
        None => std::env::remove_var("GARDEN_PROFILE_DIR"),
    }
    let _ = std::fs::remove_dir_all(&profile);
    let _ = std::fs::remove_dir_all(&jobs_dir);
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}

#[test]
fn whiteboard_record_lists_a_block_per_live_shape() {
    with_env(|env| {
        let doc = env.draw_v2_board();
        let record = env.record(&doc);
        assert_eq!(record.document_kind.as_deref(), Some("whiteboard"));
        let ids: Vec<&str> = record.blocks.iter().map(|b| b.id.as_str()).collect();
        assert_eq!(
            ids,
            SHAPES.to_vec(),
            "one block per live shape, in reading order: none for the two arrows, the bound text or the deleted rectangle"
        );
        let kinds: Vec<&str> = record
            .blocks
            .iter()
            .map(|b| b.block_type.as_str())
            .collect();
        assert_eq!(kinds, vec!["box", "note", "frame", "card", "box"]);
        let parent = |id: &str| {
            record
                .blocks
                .iter()
                .find(|b| b.id == id)
                .and_then(|b| b.parent_id.clone())
        };
        assert_eq!(
            parent("lnk0000000000000000001").as_deref(),
            Some("frm0000000000000000001")
        );
        assert_eq!(
            parent("crd0000000000000000001").as_deref(),
            Some("frm0000000000000000001")
        );
        assert_eq!(parent("box0000000000000000001"), None);
        // Re-persisting the same board changes no block.
        let again = env.record(&doc);
        assert_eq!(
            serde_json::to_string(&again.blocks).unwrap(),
            serde_json::to_string(&record.blocks).unwrap()
        );
    });
}

#[test]
fn semantic_sources_are_per_shape() {
    with_env(|env| {
        let doc = env.draw_v2_board();
        let record = env.record(&doc);
        let sources = crate::semantic_search_projection::semantic_block_sources(&[record.clone()]);
        assert!(
            sources.iter().filter(|s| s.block_id == "body").count() == 1,
            "a board keeps one body passage, so short notes, arrow labels and the title stay searchable"
        );
        assert!(
            sources
                .iter()
                .filter(|s| s.block_id != "body")
                .all(|s| s.kind == "block"),
            "and its indexable shapes are passages of their own"
        );
        // The index keeps passages of three words or more: the box, the card and the linked box. The note
        // ("condensation") and the frame ("Dream-work") are blocks but too short to index.
        let mut ids: Vec<&str> = sources.iter().map(|s| s.block_id.as_str()).collect();
        ids.sort();
        assert_eq!(
            ids,
            vec![
                "body",
                "box0000000000000000001",
                "crd0000000000000000001",
                "lnk0000000000000000001"
            ]
        );
        for source in sources.iter().filter(|s| s.block_id != "body") {
            assert_eq!(
                source.iri,
                format!("{}#block-{}", record.rdf_subject, source.block_id),
                "the passage's IRI is the board's subject plus the element id"
            );
            assert_eq!(source.document_id, doc);
        }
        let card = sources
            .iter()
            .find(|s| s.block_id == "crd0000000000000000001")
            .unwrap();
        assert_eq!(card.content, "DreamReport: The Wolf Man dream");
        assert_eq!(card.block_type, "card");
    });
}

#[test]
fn search_blocks_finds_a_shape() {
    with_env(|env| {
        let doc = env.draw_v2_board();
        let hits = env
            .call(
                "search_blocks",
                json!({"query": "Interpretation", "mode": "lexical"}),
            )
            .expect("search_blocks");
        let results = hits["results"].as_array().expect("results");
        let hit = results
            .iter()
            .find(|r| r["blockId"] == "lnk0000000000000000001")
            .unwrap_or_else(|| panic!("the linked box is a hit: {hits}"));
        assert_eq!(hit["documentId"], doc.as_str());
        assert_eq!(hit["blockType"], "box");
        assert!(hit["content"].as_str().unwrap().contains("Interpretation"));
    });
}

#[test]
fn block_tools_refuse_a_whiteboard() {
    with_env(|env| {
        let doc = env.draw_v2_board();
        let refusal = |tool: &str| {
            format!("{tool}: a whiteboard is drawn, not written: use whiteboard_apply")
        };
        let attempts = [
            (
                "insert_blocks",
                json!({"documentId": doc, "content": "A paragraph", "format": "markdown", "index": 0}),
            ),
            (
                "update_blocks",
                json!({"documentId": doc, "blockId": "box0000000000000000001", "attrs": {"level": 2}}),
            ),
            (
                "edit_block_text",
                json!({"documentId": doc, "blockId": "box0000000000000000001",
                       "operations": [{"op": "replace", "find": "Freud", "text": "Sigmund"}]}),
            ),
            (
                "delete_blocks",
                json!({"documentId": doc, "blockIds": ["box0000000000000000001"]}),
            ),
        ];
        for (tool, args) in attempts {
            let error = env.call(tool, args).expect_err(tool);
            assert!(
                error.contains(&refusal(tool)),
                "{tool} names itself and the tool to use: {error}"
            );
        }
        // Nothing was written: the board still has its five blocks.
        assert_eq!(env.record(&doc).blocks.len(), SHAPES.len());

        // The same tools still work on a normal document in the same graph.
        let text_doc = run(
            crate::mcp_workspace_crdt_service::mcp_local_create_document(
                env.app.clone(),
                &json!({"graphId": GRAPH, "documentId": "plain", "title": "Plain"}),
            ),
        )
        .expect("create a text document");
        let text_doc = text_doc["documentId"]
            .as_str()
            .unwrap_or("plain")
            .to_string();
        env.call(
            "insert_blocks",
            json!({"documentId": text_doc, "content": "A plain paragraph", "format": "markdown", "index": 0}),
        )
        .expect("insert_blocks works on a text document");
        let blocks = env
            .call("read_blocks", json!({"documentId": text_doc}))
            .expect("read_blocks");
        let block_id = blocks["blocks"][0]["blockId"]
            .as_str()
            .unwrap_or("body")
            .to_string();
        for (tool, args) in [
            (
                "update_blocks",
                json!({"documentId": text_doc, "blockId": block_id, "attrs": {}}),
            ),
            (
                "edit_block_text",
                json!({"documentId": text_doc, "blockId": block_id,
                       "operations": [{"op": "replace", "find": "plain", "text": "small"}]}),
            ),
        ] {
            // Whatever these say about their own arguments, they are not refused as a board.
            if let Err(error) = env.call(tool, args) {
                assert!(!error.contains("is drawn, not written"), "{tool}: {error}");
            }
        }
        if let Err(error) = env.call(
            "delete_blocks",
            json!({"documentId": text_doc, "blockIds": [block_id]}),
        ) {
            assert!(
                !error.contains("is drawn, not written"),
                "delete_blocks: {error}"
            );
        }
    });
}

#[test]
fn read_blocks_reads_shapes() {
    with_env(|env| {
        let doc = env.draw_v2_board();
        let read = env
            .call("read_blocks", json!({"documentId": doc}))
            .expect("read_blocks");
        assert_eq!(read["total_blocks"], SHAPES.len());
        let rows = read["blocks"].as_array().expect("blocks");
        let ids: Vec<&str> = rows
            .iter()
            .map(|r| r["blockId"].as_str().unwrap())
            .collect();
        assert_eq!(ids, SHAPES.to_vec());
        let content = |id: &str| {
            rows.iter()
                .find(|r| r["blockId"] == id)
                .unwrap_or_else(|| panic!("row {id} in {read}"))["content"]
                .as_str()
                .unwrap()
                .to_string()
        };
        assert_eq!(content("box0000000000000000001"), "Freud: the dream-work");
        assert_eq!(content("note000000000000000001"), "condensation");
        assert_eq!(content("frm0000000000000000001"), "Dream-work");
        assert_eq!(
            content("crd0000000000000000001"),
            "DreamReport: The Wolf Man dream"
        );
        assert_eq!(
            content("lnk0000000000000000001"),
            "The Interpretation of Dreams"
        );
        let card = rows
            .iter()
            .find(|r| r["blockId"] == "crd0000000000000000001")
            .unwrap();
        assert_eq!(card["blockType"], "card");
    });
}
