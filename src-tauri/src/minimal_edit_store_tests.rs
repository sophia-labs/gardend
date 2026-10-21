//! Minimal CRDT edits through the real store (DESIGN §4.3 U14; invariants I1-I4, I7, I11, I15).
//!
//! `write_document` used to delete every Yjs item of a document and append new ones, so each agent write
//! destroyed item identity, dropped comment marks, and lost any edit a person typed at the same time. These
//! tests drive the real MCP handlers (`mcp_local_write_document`, `mcp_local_read_document`, `insert_blocks`,
//! `edit_comment` through the dispatch table) and the real `Room` against a disposable graph, and observe the
//! only things a collaborator can observe: the room's Y.Doc, the frames it broadcasts, the saved record's
//! revision and the receipts. A "person" is a real second Y.Doc synced to the room and delivering its updates
//! through `Room::apply_client_update`, the same call the websocket host makes. No mocks.
use std::sync::Arc;

use serde_json::{json, Value};
use yrs::sync::{Message, SyncMessage};
use yrs::updates::decoder::Decode;
use yrs::{Doc, Map, ReadTxn, Text, Transact, Update, XmlFragment, XmlOut};

use crate::crdt_engine::rooms::{Room, RoomRegistry};
use crate::local_jobs::LocalJobRegistry;
use crate::mcp_dispatch_registry::{lookup, McpCallCtx};

const GRAPH: &str = "minimal-edit";

const FRESH_BOOK: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/comment_anchoring/freud-ch1-blocks.json"
));
const GOLDEN_BOARD: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/flow/golden-board.json"
));

// ---------------------------------------------------------------------------
// The disposable graph
// ---------------------------------------------------------------------------

struct Env {
    app: crate::app_runtime::AppHandle,
    jobs: Arc<LocalJobRegistry>,
}

impl Env {
    /// Dispatch through the real registry entry (the path a live `tools/call` takes).
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
}

fn run<F: std::future::Future>(future: F) -> F::Output {
    crate::app_runtime::async_runtime::block_on(future)
}

fn with_env(test: impl FnOnce(&Env)) {
    let _serial = crate::tauri_runtime::profile_env_serial()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let previous = std::env::var_os("GARDEN_PROFILE_DIR");
    let profile = std::env::temp_dir().join(format!("garden-minimal-edit-{}", uuid::Uuid::new_v4()));
    std::env::set_var("GARDEN_PROFILE_DIR", &profile);
    let jobs_dir = std::env::temp_dir().join(format!("garden-minimal-edit-jobs-{}", uuid::Uuid::new_v4()));
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let app = crate::tauri_runtime::build_mock_app_for_tests(true);
        crate::graph_service::create_graph_service(
            &app,
            crate::graph_service::CreateGraphInput {
                graph_id: Some(GRAPH.into()),
                title: "Minimal edit specimen".into(),
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

// ---------------------------------------------------------------------------
// Documents: create, write, read
// ---------------------------------------------------------------------------

fn create(env: &Env, document_id: &str) {
    run(crate::mcp_workspace_crdt_service::mcp_local_create_document(
        env.app.clone(),
        &json!({"graphId": GRAPH, "documentId": document_id, "title": document_id}),
    ))
    .unwrap();
}

fn write(env: &Env, document_id: &str, mut args: Value) -> Result<Value, String> {
    args["graph_id"] = json!(GRAPH);
    args["document_id"] = json!(document_id);
    run(crate::document_mutation_service::mcp_local_write_document(
        env.app.clone(),
        &args,
    ))
}

fn write_tiptap(env: &Env, document_id: &str, content: Vec<Value>) -> Value {
    write(
        env,
        document_id,
        json!({"tiptapJson": {"type": "doc", "content": content}}),
    )
    .unwrap()
}

fn write_markdown(env: &Env, document_id: &str, markdown: &str) -> Value {
    write(
        env,
        document_id,
        json!({"content": markdown, "format": "markdown"}),
    )
    .unwrap()
}

/// A write at the executor level, whose result carries the `crdt` report (the MCP response does not).
fn write_outcome(env: &Env, document_id: &str, args: Value) -> Result<Value, String> {
    let payload = crate::document_mcp_write_payloads::mcp_write_document_payload(&args, document_id)?;
    run(crate::crdt_queue::enqueue_crdt_operation(
        env.app.clone(),
        crate::crdt_queue::EnqueueCrdtOperationInput {
            kind: "document.write".to_string(),
            graph_id: GRAPH.to_string(),
            document_id: Some(document_id.to_string()),
            payload,
        },
    ))
}

fn read_markdown(env: &Env, document_id: &str) -> String {
    crate::document_mcp_service::mcp_local_read_document(
        env.app.clone(),
        &json!({"graph_id": GRAPH, "document_id": document_id, "format": "markdown", "maxChars": 1_000_000}),
    )
    .unwrap()["content"]
        .as_str()
        .unwrap()
        .to_string()
}

/// The saved record's TipTap JSON (the cold side: what `read_document` serves).
fn stored(env: &Env, document_id: &str) -> Value {
    let read = crate::document_mcp_service::mcp_local_read_document(
        env.app.clone(),
        &json!({"graph_id": GRAPH, "document_id": document_id, "format": "json", "maxChars": 1_000_000}),
    )
    .unwrap();
    let parsed: Value = serde_json::from_str(read["content"].as_str().unwrap()).unwrap();
    parsed["tiptapJson"].clone()
}

fn warnings(result: &Value) -> Vec<String> {
    result["warnings"]
        .as_array()
        .unwrap_or_else(|| panic!("write result carries no warnings array: {result}"))
        .iter()
        .map(|w| w.as_str().unwrap_or_default().to_string())
        .collect()
}

fn revision(result: &Value) -> u64 {
    result["revision"]
        .as_u64()
        .unwrap_or_else(|| panic!("write result carries no revision: {result}"))
}

// ---------------------------------------------------------------------------
// The live room and a person typing into it
// ---------------------------------------------------------------------------

fn room_of(env: &Env, document_id: &str) -> Arc<Room> {
    let graph_dir = crate::graph_paths::existing_graph_dir(&env.app, GRAPH).unwrap();
    let registry = env.app.state::<RoomRegistry>();
    run(registry.get_or_create(
        &format!("doc:{GRAPH}:{document_id}"),
        crate::ydoc_paths::document_ydoc_state_path(&graph_dir, document_id),
    ))
    .unwrap()
}

fn room_json(room: &Arc<Room>) -> Value {
    run(room.with_doc(crate::crdt_engine::projection::ydoc_to_tiptap_json))
}

fn room_state(room: &Arc<Room>) -> Vec<u8> {
    run(room.encode_state_for_test())
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
        .into_iter()
        .flatten()
        .map(node_text)
        .collect()
}

fn block_ids(doc: &Value) -> Vec<String> {
    doc["content"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|block| {
            block["attrs"]["data-block-id"]
                .as_str()
                .unwrap_or_default()
                .to_string()
        })
        .collect()
}

fn paragraph(id: &str, text: &str) -> Value {
    json!({"type": "paragraph", "attrs": {"data-block-id": id},
           "content": [{"type": "text", "text": text}]})
}

fn apply(doc: &Doc, update: &[u8]) {
    let update = Update::decode_v1(update).expect("decode update");
    doc.transact_mut().apply_update(update).expect("apply update");
}

/// A browser: a second Y.Doc holding what the room holds now.
fn person_synced(room: &Arc<Room>) -> Doc {
    let person = Doc::new();
    apply(&person, &room_state(room));
    person
}

/// The person types `text` into top-level block `block`, at its start or its end, and the returned bytes are
/// the update her browser would send.
fn person_types(person: &Doc, block: u32, text: &str, at_start: bool) -> Vec<u8> {
    let before = person.transact().state_vector();
    let fragment = person.get_or_insert_xml_fragment("content");
    {
        let mut txn = person.transact_mut();
        let element = fragment
            .get(&txn, block)
            .and_then(XmlOut::into_xml_element)
            .expect("a paragraph at that index");
        let leaf = element
            .get(&txn, 0)
            .and_then(XmlOut::into_xml_text)
            .expect("the paragraph's first text");
        let at = if at_start { 0 } else { leaf.len(&txn) };
        leaf.insert(&mut txn, at, text);
    }
    let update = person.transact().encode_state_as_update_v1(&before);
    update
}

/// Everything broadcast since the last drain, as the Yjs updates the frames carry.
fn drain_updates(rx: &mut tokio::sync::broadcast::Receiver<Vec<u8>>) -> Vec<Update> {
    let mut out = Vec::new();
    while let Ok(frame) = rx.try_recv() {
        match Message::decode_v1(&frame).expect("decode frame") {
            Message::Sync(SyncMessage::Update(bytes)) => {
                out.push(Update::decode_v1(&bytes).expect("decode broadcast update"))
            }
            other => panic!("unexpected broadcast frame: {other:?}"),
        }
    }
    out
}

fn live_edits_kept(outcome: &Value) -> Vec<String> {
    outcome["crdt"]["liveEdits"]["kept"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|id| id.as_str().map(str::to_string))
        .collect()
}

fn live_edits_conflicts(outcome: &Value) -> Vec<String> {
    outcome["crdt"]["liveEdits"]["conflicts"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|id| id.as_str().map(str::to_string))
        .collect()
}

// ---------------------------------------------------------------------------
// The block-contract specimen (a reduced copy of block_contract_store_tests' specimen)
// ---------------------------------------------------------------------------

fn text_node(t: &str) -> Value {
    json!({"type": "text", "text": t})
}

fn specimen() -> Vec<Value> {
    let item = |children: Vec<Value>| json!({"type": "listItem", "content": children});
    let para = |t: &str| json!({"type": "paragraph", "content": [text_node(t)]});
    vec![
        paragraph("anchor", "Anchor paragraph stays."),
        json!({"type": "bulletList", "attrs": {"data-block-id": "bl"}, "content": [
            item(vec![para("BULLET-A"), json!({"type": "bulletList", "content": [
                item(vec![para("BULLET-A1")]),
            ]})]),
            item(vec![para("BULLET-B")]),
        ]}),
        json!({"type": "orderedList", "attrs": {"data-block-id": "ol"}, "content": [
            item(vec![para("ORDERED-A")]),
            item(vec![para("ORDERED-B")]),
        ]}),
        json!({"type": "paragraph", "attrs": {"data-block-id": "pimg"}, "content": [
            text_node("IMG-BEFORE "),
            {"type": "image", "attrs": {"src": "https://example.org/a.png", "alt": "ALT-A"}},
            text_node(" IMG-AFTER"),
        ]}),
        json!({"type": "callout", "attrs": {"data-block-id": "co", "tone": "warn"},
               "content": [para("CALLOUT-TEXT")]}),
        json!({"type": "paragraph", "attrs": {"data-block-id": "pm"}, "content": [
            {"type": "text", "text": "KBD-TEXT", "marks": [{"type": "kbd"}]},
            text_node(" and "),
            {"type": "text", "text": "SUP-TEXT", "marks": [{"type": "superscript"}, {"type": "bold"}]},
        ]}),
    ]
}

/// Drop every minted-or-given block id: two writes of one specimen mint different ids for the blocks the
/// normaliser creates (the seed is the operation id), and nothing else may differ.
fn without_ids(value: &Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.iter()
                .filter(|(key, _)| key.as_str() != "data-block-id")
                .map(|(key, value)| (key.clone(), without_ids(value)))
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(without_ids).collect()),
        Value::Number(n) => match n.as_f64() {
            Some(f) if f.fract() == 0.0 => json!(f as i64),
            _ => value.clone(),
        },
        other => other.clone(),
    }
}

fn is_rewrite(warning: &str) -> bool {
    warning.starts_with("block-contract known-equivalent")
        || warning.starts_with("block-contract unknown")
}

// ---------------------------------------------------------------------------
// U14 tests
// ---------------------------------------------------------------------------

/// I7 and I1: the receipts a specimen write reports do not depend on what the document held, and writing
/// the canonical result back changes nothing at all (no Yjs operation, no broadcast, no new revision).
#[test]
fn specimen_receipts_are_byte_identical_to_a_fresh_write() {
    with_env(|env| {
        create(env, "spec-a");
        let first = write_tiptap(env, "spec-a", specimen());
        let first_warnings = warnings(&first);
        assert!(
            first_warnings.iter().any(|w| is_rewrite(w)),
            "the specimen must report rewrites: {first_warnings:?}"
        );
        let first_doc = stored(env, "spec-a");

        // A fresh document gets the same receipts.
        create(env, "spec-b");
        let fresh = write_tiptap(env, "spec-b", specimen());
        assert_eq!(
            warnings(&fresh),
            first_warnings,
            "a fresh write reports other receipts"
        );

        // The same specimen into the document that already holds it: same receipts, same content. (Blocks the
        // normaliser mints get ids from the operation id, so only ids may differ.)
        let again = write_tiptap(env, "spec-a", specimen());
        assert_eq!(
            warnings(&again),
            first_warnings,
            "writing into an existing document changed the receipts"
        );
        let again_doc = stored(env, "spec-a");
        assert_eq!(
            without_ids(&again_doc),
            without_ids(&first_doc),
            "the second specimen write changed content beyond block ids"
        );
        let ids_after = block_ids(&again_doc);
        for given in ["anchor", "bl", "ol", "pimg", "co", "pm"] {
            assert!(
                ids_after.iter().any(|id| id == given),
                "the id {given} the caller gave was lost: {ids_after:?}"
            );
        }

        // The canonical content written back is a no-op.
        let room = room_of(env, "spec-a");
        let state_before = room_state(&room);
        let mut rx = room.subscribe_updates_for_test();
        let canonical = again_doc["content"].as_array().unwrap().clone();
        let back = write_tiptap(env, "spec-a", canonical);
        assert!(
            warnings(&back).iter().all(|w| !is_rewrite(w)),
            "canonical content was rewritten: {:?}",
            warnings(&back)
        );
        assert_eq!(
            room_state(&room),
            state_before,
            "writing the canonical content back changed the room's Y.Doc"
        );
        assert!(
            drain_updates(&mut rx).is_empty(),
            "a no-op write broadcast an update"
        );
        assert_eq!(
            revision(&back),
            revision(&again),
            "a no-op write minted a revision"
        );
    });
}

/// I1 on real text: Freud chapter I read as markdown and sent back is zero operations.
#[test]
fn markdown_round_trip_of_freud_chapter_is_zero_ops() {
    with_env(|env| {
        let fixture: Value = serde_json::from_str(FRESH_BOOK).unwrap();
        let blocks = fixture["tiptapJson"]["content"].as_array().unwrap().clone();
        create(env, "freud");
        let first = write_tiptap(env, "freud", blocks);
        let markdown = read_markdown(env, "freud");
        assert!(markdown.contains("Strümpell"), "the chapter reads back");

        let room = room_of(env, "freud");
        let state_before = room_state(&room);
        let json_before = room_json(&room);
        let mut rx = room.subscribe_updates_for_test();

        let back = write_markdown(env, "freud", &markdown);

        assert_eq!(
            room_state(&room),
            state_before,
            "the markdown round trip changed the room's Y.Doc"
        );
        assert_eq!(room_json(&room), json_before);
        assert!(
            drain_updates(&mut rx).is_empty(),
            "the markdown round trip broadcast an update"
        );
        assert_eq!(
            revision(&back),
            revision(&first),
            "the markdown round trip minted a revision"
        );
        assert_eq!(block_ids(&room_json(&room)), block_ids(&json_before));
    });
}

/// I2 at the wire: one word changed in one paragraph of a 6 KB chapter is a broadcast of a few dozen bytes,
/// not the chapter.
#[test]
fn one_paragraph_edit_broadcasts_a_small_update() {
    with_env(|env| {
        let fixture: Value = serde_json::from_str(FRESH_BOOK).unwrap();
        let blocks = fixture["tiptapJson"]["content"].as_array().unwrap().clone();
        create(env, "freud-small");
        write_tiptap(env, "freud-small", blocks);
        let markdown = read_markdown(env, "freud-small");
        let edited = markdown.replacen("patient", "subject", 1);
        assert_ne!(edited, markdown, "the chapter mentions a patient");

        let room = room_of(env, "freud-small");
        let ids_before = block_ids(&room_json(&room));
        let mut rx = room.subscribe_updates_for_test();
        let mut frames: Vec<Vec<u8>> = Vec::new();

        write_markdown(env, "freud-small", &edited);

        while let Ok(frame) = rx.try_recv() {
            frames.push(frame);
        }
        assert!(!frames.is_empty(), "the edit broadcast nothing");
        let total: usize = frames.iter().map(Vec::len).sum();
        assert!(
            total < 2048,
            "one edited word broadcast {total} bytes in {} frame(s); the chapter is {} bytes",
            frames.len(),
            markdown.len()
        );
        let after = room_json(&room);
        assert!(block_texts(&after).iter().any(|t| t.contains("subject")));
        assert_eq!(
            block_ids(&after),
            ids_before,
            "an in-place edit changed block ids"
        );
    });
}

/// I3: a person typing into blocks the agent did not rewrite, and into one it did, loses nothing when the
/// agent's write lands first and her update arrives after.
#[test]
fn concurrent_person_insert_survives_agent_write() {
    with_env(|env| {
        create(env, "concurrent");
        write_tiptap(
            env,
            "concurrent",
            vec![
                paragraph("a", "Alpha one"),
                paragraph("b", "Beta two"),
                paragraph("c", "Gamma three"),
            ],
        );
        let room = room_of(env, "concurrent");
        let person = person_synced(&room);

        // She types while the agent is still composing: into B (which the agent leaves alone) and at the
        // start of C (which it extends at the end).
        let into_b = person_types(&person, 1, " PERSON-B", false);
        let into_c = person_types(&person, 2, "PERSON-C ", true);

        // The agent read before she typed, and writes first.
        let markdown = read_markdown(env, "concurrent");
        let edited = markdown.replace("Gamma three", "Gamma three!");
        assert_ne!(edited, markdown);
        write_markdown(env, "concurrent", &edited);

        assert!(run(room.apply_client_update(&into_b)).unwrap());
        assert!(run(room.apply_client_update(&into_c)).unwrap());

        let after = room_json(&room);
        assert_eq!(
            block_texts(&after),
            vec![
                "Alpha one".to_string(),
                "Beta two PERSON-B".to_string(),
                "PERSON-C Gamma three!".to_string(),
            ],
            "the person's typing or the agent's edit was lost"
        );
        assert_eq!(
            block_ids(&after),
            vec!["a".to_string(), "b".to_string(), "c".to_string()],
            "the agent's markdown write re-minted block ids"
        );
    });
}

/// I4: the person's edit has reached the room but not the saved record. The agent read the record at
/// revision R and writes with `expectedRevision: R`. Her edit stays and is reported as kept.
#[test]
fn unflushed_person_edit_survives_expected_revision_write() {
    with_env(|env| {
        create(env, "rebase");
        let first = write_tiptap(
            env,
            "rebase",
            vec![
                paragraph("p1", "First paragraph"),
                paragraph("p2", "Second paragraph"),
            ],
        );
        let expected = revision(&first);
        let read_by_agent = read_markdown(env, "rebase");

        let room = room_of(env, "rebase");
        let person = person_synced(&room);
        let typed = person_types(&person, 0, " PERSON", false);
        assert!(run(room.apply_client_update(&typed)).unwrap());
        assert!(
            room.needs_projection_flush(),
            "the person's edit must still be unflushed for this test to mean anything"
        );

        let edited = read_by_agent.replace("Second paragraph", "Second paragraph edited");
        assert_ne!(edited, read_by_agent);
        let outcome = write_outcome(
            env,
            "rebase",
            json!({"content": edited, "format": "markdown", "expectedRevision": expected}),
        )
        .unwrap();

        assert_eq!(
            block_texts(&room_json(&room)),
            vec![
                "First paragraph PERSON".to_string(),
                "Second paragraph edited".to_string(),
            ],
            "the unflushed edit was overwritten, or the agent's edit was not applied"
        );
        assert!(
            live_edits_kept(&outcome).contains(&"p1".to_string()),
            "liveEdits.kept does not name the block the person edited: {outcome}"
        );
        assert!(
            live_edits_conflicts(&outcome).is_empty(),
            "nothing conflicted: {outcome}"
        );
    });
}

/// The other half of I4's contract, so the semantics are written down: without `expectedRevision` the write
/// is two-way, "the document becomes what I sent", and the agent's stale copy of the paragraph wins.
#[test]
fn two_way_write_without_expected_revision_wins() {
    with_env(|env| {
        create(env, "two-way");
        write_tiptap(
            env,
            "two-way",
            vec![
                paragraph("p1", "First paragraph"),
                paragraph("p2", "Second paragraph"),
            ],
        );
        let read_by_agent = read_markdown(env, "two-way");
        let room = room_of(env, "two-way");
        let person = person_synced(&room);
        let typed = person_types(&person, 0, " PERSON", false);
        assert!(run(room.apply_client_update(&typed)).unwrap());

        let edited = read_by_agent.replace("Second paragraph", "Second paragraph edited");
        let outcome = write_outcome(env, "two-way", json!({"content": edited, "format": "markdown"})).unwrap();

        assert_eq!(
            block_texts(&room_json(&room)),
            vec![
                "First paragraph".to_string(),
                "Second paragraph edited".to_string(),
            ],
            "a two-way write must make the document what the agent sent"
        );
        assert!(
            live_edits_kept(&outcome).is_empty(),
            "a two-way write keeps no live edit: {outcome}"
        );
    });
}

/// I15: markdown cannot say "comment", so a markdown rewrite that leaves the commented word alone must
/// leave its comment alone, whether the edit is after the word or before it.
#[test]
fn comment_marks_survive_markdown_rewrite() {
    with_env(|env| {
        create(env, "marks");
        write_tiptap(
            env,
            "marks",
            vec![
                paragraph("m1", "Hello world, plain tail."),
                paragraph("m2", "Second block stays."),
            ],
        );
        env.call(
            "edit_comment",
            json!({"documentId": "marks", "commentId": "c1", "action": "set",
                   "text": "a note", "find": "world", "blockId": "m1"}),
        )
        .unwrap();

        let commented = |doc: &Value| -> String {
            let mut out = String::new();
            fn walk(node: &Value, out: &mut String) {
                if node["type"] == "text" {
                    let marked = node["marks"].as_array().into_iter().flatten().any(|mark| {
                        mark["type"] == "commentMark" && mark["attrs"]["commentId"] == "c1"
                    });
                    if marked {
                        out.push_str(node["text"].as_str().unwrap_or(""));
                    }
                }
                for child in node["content"].as_array().into_iter().flatten() {
                    walk(child, out);
                }
            }
            walk(doc, &mut out);
            out
        };
        assert_eq!(commented(&stored(env, "marks")), "world", "the comment landed");

        // An edit after the commented word.
        let markdown = read_markdown(env, "marks");
        assert!(!markdown.contains("c1"), "markdown cannot carry the comment");
        let after = markdown.replace("plain tail.", "plain tail, extended.");
        assert_ne!(after, markdown);
        write_markdown(env, "marks", &after);
        let doc = stored(env, "marks");
        assert_eq!(commented(&doc), "world", "an edit after the word dropped the comment");
        assert_eq!(block_texts(&doc)[0], "Hello world, plain tail, extended.");

        // An edit before it.
        let before = after.replace("Hello", "Greetings");
        write_markdown(env, "marks", &before);
        let doc = stored(env, "marks");
        assert_eq!(commented(&doc), "world", "an edit before the word dropped the comment");
        assert_eq!(block_texts(&doc)[0], "Greetings world, plain tail, extended.");

        // The comment's entry in the document's comments map survived too.
        let room = room_of(env, "marks");
        let has_entry = run(room.with_doc(|doc| {
            let txn = doc.transact();
            let found = match txn.get_map("comments") {
                Some(comments) => comments.contains_key(&txn, "c1"),
                None => false,
            };
            found
        }));
        assert!(has_entry, "the comment's entry left the comments map");
    });
}

/// The conversation mirror rewrites the whole transcript after every turn. With minimal edits, a turn is an
/// append: the broadcast holds insertions only, no deletion, and the earlier blocks keep their ids.
#[test]
fn vehicle_mirror_append_is_an_append() {
    with_env(|env| {
        let turn_one = "# Transcript\n\n**You:** first question\n\n**Sophia:** first answer";
        let turn_two = format!(
            "{turn_one}\n\n**You:** second question\n\n**Sophia:** second answer"
        );
        create(env, "mirror");
        write_markdown(env, "mirror", turn_one);
        let room = room_of(env, "mirror");
        let ids_before = block_ids(&room_json(&room));
        assert_eq!(ids_before.len(), 3);
        let mut rx = room.subscribe_updates_for_test();

        write_markdown(env, "mirror", &turn_two);

        let updates = drain_updates(&mut rx);
        assert!(!updates.is_empty(), "the second turn broadcast nothing");
        for update in &updates {
            assert!(
                update.delete_set().is_empty(),
                "appending a turn deleted something"
            );
            assert!(
                !update.insertions(true).is_empty(),
                "the broadcast inserted nothing"
            );
        }
        let after = room_json(&room);
        assert_eq!(
            &block_ids(&after)[..3],
            &ids_before[..],
            "the earlier blocks lost their ids"
        );
        assert_eq!(block_ids(&after).len(), 5);
        assert!(block_texts(&after)[4].contains("second answer"));
    });
}

/// I11: a whiteboard or a Flow board is not a TipTap document. `write_document` refuses, names the right tool,
/// and writes nothing: the room's Y.Doc is byte for byte what it was.
#[test]
fn scene_kind_write_refused_end_to_end() {
    with_env(|env| {
        // A whiteboard.
        let board = env
            .call("whiteboard_create", json!({"title": "Sketch"}))
            .unwrap()["documentId"]
            .as_str()
            .unwrap()
            .to_string();
        let room = room_of(env, &board);
        let state_before = room_state(&room);
        let mut rx = room.subscribe_updates_for_test();
        let error = write(env, &board, json!({"content": "# Not a document", "format": "markdown"}))
            .expect_err("a content write to a whiteboard must be refused");
        assert!(
            error.contains("whiteboard_apply"),
            "the refusal must name the right tool: {error}"
        );
        let error = write(
            env,
            &board,
            json!({"tiptapJson": {"type": "doc", "content": [paragraph("x", "nope")]}}),
        )
        .expect_err("a tiptapJson write to a whiteboard must be refused");
        assert!(error.contains("whiteboard_apply"), "{error}");
        assert_eq!(
            room_state(&room),
            state_before,
            "a refused write changed the whiteboard's Y.Doc"
        );
        assert!(drain_updates(&mut rx).is_empty());
        let read = env
            .call("whiteboard_read", json!({"documentId": board}))
            .unwrap();
        assert_eq!(read["elementCount"], 0, "the board is still empty: {read}");

        // A Flow board.
        let seeded = env
            .call("flow_seed_board", json!({"json": GOLDEN_BOARD}))
            .unwrap();
        let flow = seeded["documentId"].as_str().unwrap().to_string();
        let export_before = env.call("flow_export_board", json!({})).unwrap()["json"]
            .as_str()
            .unwrap()
            .to_string();
        let flow_room = room_of(env, &flow);
        let flow_state = room_state(&flow_room);
        let error = write(env, &flow, json!({"content": "# Not a document", "format": "markdown"}))
            .expect_err("a content write to a Flow board must be refused");
        assert!(
            error.contains("flow_seed_board"),
            "the refusal must name the right tool: {error}"
        );
        assert_eq!(
            room_state(&flow_room),
            flow_state,
            "a refused write changed the Flow board's Y.Doc"
        );
        let export_after = env.call("flow_export_board", json!({})).unwrap()["json"]
            .as_str()
            .unwrap()
            .to_string();
        assert_eq!(export_after, export_before, "the Flow board changed");
    });
}
