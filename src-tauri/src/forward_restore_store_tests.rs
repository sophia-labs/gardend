//! Forward restore through the real store and live rooms (DESIGN §4.3 U15; invariants I5, I6, I12).
//!
//! Restore used to write the restore point's bytes over `update-v1.bin` and leave the live room's newer Doc
//! in memory. Three things then undid it: the cached room's next persist, any reconnecting socket (its
//! SyncStep2 hands back every item the server lacks), and the browser's IndexedDB cache. Restore is now a
//! forward transaction through the room: it only adds items and deletions, so the server's state vector never
//! goes back and every peer, cached or live, converges on the restored content.
//!
//! These tests run the real restore service (`restore_to_restore_point` through the dispatch table, polled
//! like a client does) or, where the report matters, the engine itself (`forward_restore_manifest`), against
//! a disposable graph with live `Room`s. A "browser" is a second Y.Doc synced to the room; a reconnect is the
//! SyncStep2 exchange `Room::apply_client_update` serves. No mocks.
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use yrs::updates::decoder::Decode;
use yrs::{Any, Doc, Map, Out, ReadTxn, StateVector, Text, Transact, Update, XmlFragment, XmlOut};

use crate::crdt_engine::forward_restore::{
    forward_restore_manifest, ForwardRestoreReport, RestoreContext,
};
use crate::crdt_engine::persistence_coordinator::GraphPersistenceCoordinator;
use crate::crdt_engine::rooms::{Room, RoomRegistry};
use crate::local_jobs::LocalJobRegistry;
use crate::mcp_dispatch_registry::{lookup, McpCallCtx};

const GRAPH: &str = "forward-restore";

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

    fn graph_dir(&self) -> std::path::PathBuf {
        crate::graph_paths::existing_graph_dir(&self.app, GRAPH).unwrap()
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
    let profile =
        std::env::temp_dir().join(format!("garden-forward-restore-{}", uuid::Uuid::new_v4()));
    std::env::set_var("GARDEN_PROFILE_DIR", &profile);
    let jobs_dir = std::env::temp_dir().join(format!(
        "garden-forward-restore-jobs-{}",
        uuid::Uuid::new_v4()
    ));
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let app = crate::tauri_runtime::build_mock_app_for_tests(true);
        crate::graph_service::create_graph_service(
            &app,
            crate::graph_service::CreateGraphInput {
                graph_id: Some(GRAPH.into()),
                title: "Forward restore specimen".into(),
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
// Documents, rooms, browsers
// ---------------------------------------------------------------------------

fn paragraph(id: &str, text: &str) -> Value {
    json!({"type": "paragraph", "attrs": {"data-block-id": id},
           "content": [{"type": "text", "text": text}]})
}

fn create(env: &Env, document_id: &str) {
    run(
        crate::mcp_workspace_crdt_service::mcp_local_create_document(
            env.app.clone(),
            &json!({"graphId": GRAPH, "documentId": document_id, "title": document_id}),
        ),
    )
    .unwrap();
}

fn write_tiptap(env: &Env, document_id: &str, content: Vec<Value>) -> Value {
    run(crate::document_mutation_service::mcp_local_write_document(
        env.app.clone(),
        &json!({"graph_id": GRAPH, "document_id": document_id,
                "tiptapJson": {"type": "doc", "content": content}}),
    ))
    .unwrap()
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

fn room_key(document_id: &str) -> String {
    format!("doc:{GRAPH}:{document_id}")
}

fn room_of(env: &Env, document_id: &str) -> Arc<Room> {
    let registry = env.app.state::<RoomRegistry>();
    run(registry.get_or_create(
        &room_key(document_id),
        crate::ydoc_paths::document_ydoc_state_path(&env.graph_dir(), document_id),
    ))
    .unwrap()
}

fn live_room(env: &Env, document_id: &str) -> Option<Arc<Room>> {
    let registry = env.app.state::<RoomRegistry>();
    run(registry.peek(&room_key(document_id)))
}

fn room_json(room: &Arc<Room>) -> Value {
    run(room.with_doc(crate::crdt_engine::projection::ydoc_to_tiptap_json))
}

fn room_state_vector(room: &Arc<Room>) -> StateVector {
    run(room.with_doc(|doc| doc.transact().state_vector()))
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

fn texts(list: &[&str]) -> Vec<String> {
    list.iter().map(|t| t.to_string()).collect()
}

fn apply(doc: &Doc, update: &[u8]) {
    let update = Update::decode_v1(update).expect("decode update");
    doc.transact_mut()
        .apply_update(update)
        .expect("apply update");
}

/// A browser: a second Y.Doc holding what the room holds now.
fn browser_synced(room: &Arc<Room>) -> Doc {
    let browser = Doc::new();
    apply(&browser, &run(room.encode_state_for_test()));
    browser
}

/// The browser types `text` at the end of top-level block `block`; the update it would send.
fn browser_types(browser: &Doc, block: u32, text: &str) -> Vec<u8> {
    let before = browser.transact().state_vector();
    let fragment = browser.get_or_insert_xml_fragment("content");
    {
        let mut txn = browser.transact_mut();
        let element = fragment
            .get(&txn, block)
            .and_then(XmlOut::into_xml_element)
            .expect("a paragraph at that index");
        let leaf = element
            .get(&txn, 0)
            .and_then(XmlOut::into_xml_text)
            .expect("the paragraph's first text");
        let at = leaf.len(&txn);
        leaf.insert(&mut txn, at, text);
    }
    let update = browser.transact().encode_state_as_update_v1(&before);
    update
}

fn browser_texts(browser: &Doc) -> Vec<String> {
    block_texts(&crate::crdt_engine::projection::ydoc_to_tiptap_json(
        browser,
    ))
}

/// The update bytes of every frame broadcast since the last drain (awareness frames are not updates).
fn drain_update_bytes(rx: &mut tokio::sync::broadcast::Receiver<Vec<u8>>) -> Vec<Vec<u8>> {
    use yrs::sync::{Message, SyncMessage};
    let mut out = Vec::new();
    while let Ok(frame) = rx.try_recv() {
        if let Ok(Message::Sync(SyncMessage::Update(bytes))) = Message::decode_v1(&frame) {
            out.push(bytes);
        }
    }
    out
}

/// The document's hot sidecar (`update-v1.bin`) as TipTap JSON: what a cold open would find.
fn sidecar_json(env: &Env, document_id: &str) -> Value {
    let path = crate::ydoc_paths::document_ydoc_state_path(&env.graph_dir(), document_id);
    let bytes = std::fs::read(&path).unwrap_or_else(|error| panic!("read {path:?}: {error}"));
    let doc = Doc::new();
    apply(&doc, &bytes);
    crate::crdt_engine::projection::ydoc_to_tiptap_json(&doc)
}

/// The saved record (`document.json`) as the store reads it from disk.
fn disk_record(env: &Env, document_id: &str) -> Value {
    let path = crate::paths::documents_dir(&env.graph_dir())
        .join(document_id)
        .join("document.json");
    serde_json::from_slice(
        &std::fs::read(&path).unwrap_or_else(|error| panic!("read {path:?}: {error}")),
    )
    .unwrap()
}

fn record_revision(env: &Env, document_id: &str) -> u64 {
    disk_record(env, document_id)["revision"]
        .as_u64()
        .expect("record revision")
}

fn in_tree(env: &Env, document_id: &str) -> bool {
    let ws = run(crate::crdt_engine::workspace_ops::workspace_room(
        &env.app,
        GRAPH,
        &env.graph_dir(),
    ))
    .unwrap();
    let id = document_id.to_string();
    run(ws.with_doc(move |doc| {
        crate::crdt_engine::workspace_ops::document_title_in_workspace(doc, &id)
    }))
    .is_some()
}

// ---------------------------------------------------------------------------
// Restore points and the two ways to run a restore
// ---------------------------------------------------------------------------

fn restore_point(env: &Env, label: &str) -> String {
    env.call("create_restore_point", json!({"label": label}))
        .unwrap()["restorePointId"]
        .as_str()
        .expect("restore point id")
        .to_string()
}

/// The real service, started and polled the way a client does. Returns the finished operation.
fn restore_via_service(env: &Env, restore_point_id: &str) -> Value {
    let started = env
        .call(
            "restore_to_restore_point",
            json!({"restorePointId": restore_point_id, "dryRun": false}),
        )
        .expect("start restore");
    let operation_id = started["operationId"]
        .as_str()
        .unwrap_or_else(|| panic!("no operation id: {started}"))
        .to_string();
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let status = env
            .call(
                "get_restore_operation",
                json!({"operationId": operation_id}),
            )
            .expect("restore status");
        match status["state"].as_str().unwrap_or_default() {
            "succeeded" | "failed" | "rolled_back" => return status,
            _ => {}
        }
        assert!(
            Instant::now() < deadline,
            "restore did not finish in time: {status}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn assert_succeeded(operation: &Value) {
    assert_eq!(
        operation["state"], "succeeded",
        "restore failed: {operation}"
    );
}

/// The engine itself, under the graph lease the service would hold, so the report is observable.
fn restore_engine(env: &Env, restore_point_id: &str) -> ForwardRestoreReport {
    let manifest = crate::time_travel_store::read_manifest(&env.graph_dir(), restore_point_id)
        .expect("manifest");
    let ctx = RestoreContext {
        restore_point_id: restore_point_id.to_string(),
        operation_id: "restore-engine-test".to_string(),
        now_ms: 1_792_000_000_000.0,
    };
    run(async {
        let lease = match env.app.try_state::<GraphPersistenceCoordinator>() {
            Some(coordinator) => Some(
                coordinator
                    .acquire_hot_write(GRAPH)
                    .await
                    .expect("graph lease"),
            ),
            None => None,
        };
        let report = forward_restore_manifest(&env.app, GRAPH, &manifest, &ctx).await;
        drop(lease);
        report
    })
    .expect("forward restore")
}

fn insert_block(env: &Env, document_id: &str, content: &str, index: u32) {
    env.call(
        "insert_blocks",
        json!({"documentId": document_id, "content": content, "format": "markdown", "index": index}),
    )
    .unwrap();
}

// ---------------------------------------------------------------------------
// U15 tests
// ---------------------------------------------------------------------------

/// I6: with a live room, the room's own Doc (not just the disk) becomes the restore point's, a browser that
/// stayed open converges from the broadcast, and the next frame from that browser brings nothing back.
#[test]
fn restore_with_a_live_room_converges_the_room() {
    with_env(|env| {
        create(env, "live");
        write_tiptap(
            env,
            "live",
            vec![
                paragraph("a", "Captured alpha"),
                paragraph("b", "Captured beta"),
            ],
        );
        let room = room_of(env, "live");
        let captured = restore_point(env, "live-room");

        // After the capture: an agent adds a block and a person types.
        insert_block(env, "live", "AGENT-AFTER-CAPTURE", 2);
        let browser = browser_synced(&room);
        let typed = browser_types(&browser, 0, " PERSON-AFTER");
        assert!(run(room.apply_client_update(&typed)).unwrap());
        assert_eq!(
            block_texts(&room_json(&room)),
            texts(&[
                "Captured alpha PERSON-AFTER",
                "Captured beta",
                "AGENT-AFTER-CAPTURE"
            ]),
            "the post-capture edits did not land"
        );

        let mut rx = room.subscribe_updates_for_test();
        assert_succeeded(&restore_via_service(env, &captured));

        // The room that was live is still the room, and its Doc is the snapshot's.
        let still = live_room(env, "live").expect("a live room stays cached");
        assert!(
            Arc::ptr_eq(&still, &room),
            "the restore replaced the live room"
        );
        let restored = texts(&["Captured alpha", "Captured beta"]);
        assert_eq!(
            block_texts(&room_json(&room)),
            restored,
            "the room's Y.Doc was not restored"
        );

        // The disk agrees with the room, and the saved record is the restored one.
        assert_eq!(block_texts(&sidecar_json(env, "live")), restored);
        let body = disk_record(env, "live")["body"]
            .as_str()
            .unwrap_or("")
            .to_string();
        assert!(
            body.contains("Captured alpha") && !body.contains("AFTER"),
            "{body}"
        );
        let markdown = read_markdown(env, "live");
        assert!(
            markdown.contains("Captured beta") && !markdown.contains("AGENT-AFTER-CAPTURE"),
            "{markdown}"
        );

        // The open browser converges from what the room broadcast, without a reload.
        let updates = drain_update_bytes(&mut rx);
        assert!(
            !updates.is_empty(),
            "the restore broadcast nothing to the open browser"
        );
        for update in &updates {
            apply(&browser, update);
        }
        assert_eq!(
            browser_texts(&browser),
            restored,
            "the open browser did not converge"
        );

        // Its next sync frame changes nothing on the server.
        let server = room_state_vector(&room);
        let frame = browser.transact().encode_state_as_update_v1(&server);
        assert!(
            !run(room.apply_client_update(&frame)).unwrap(),
            "the converged browser brought something back"
        );
        assert_eq!(block_texts(&room_json(&room)), restored);
    });
}

/// I5: a browser that missed the restore (closed tab, dropped socket) and holds the newer state reconnects.
/// Its SyncStep2 changes nothing on the server, and it converges to the restored content.
#[test]
fn reconnecting_client_after_restore_brings_nothing_back() {
    with_env(|env| {
        create(env, "reconnect");
        write_tiptap(
            env,
            "reconnect",
            vec![
                paragraph("a", "Captured alpha"),
                paragraph("b", "Captured beta"),
            ],
        );
        let room = room_of(env, "reconnect");
        let captured = restore_point(env, "reconnect");

        insert_block(env, "reconnect", "AFTER-CAPTURE-BLOCK", 2);
        let browser = browser_synced(&room);
        assert_eq!(
            browser_texts(&browser).len(),
            3,
            "the browser holds the newer state"
        );

        // The browser goes away while the restore runs; it never sees the broadcast.
        assert_succeeded(&restore_via_service(env, &captured));
        let restored = texts(&["Captured alpha", "Captured beta"]);
        assert_eq!(block_texts(&room_json(&room)), restored);

        // Reconnect: it sends everything the server lacks...
        let server = room_state_vector(&room);
        let step2 = browser.transact().encode_state_as_update_v1(&server);
        let changed = run(room.apply_client_update(&step2)).unwrap();
        assert!(
            !changed,
            "a reconnecting client with the newer state changed the restored room"
        );
        assert_eq!(
            block_texts(&room_json(&room)),
            restored,
            "the reconnect brought the post-capture content back"
        );
        assert_eq!(block_texts(&sidecar_json(env, "reconnect")), restored);

        // ...and receives what it lacks.
        let browser_sv = browser.transact().state_vector();
        let pull =
            run(room.with_doc(move |doc| doc.transact().encode_state_as_update_v1(&browser_sv)));
        apply(&browser, &pull);
        assert_eq!(
            browser_texts(&browser),
            restored,
            "the reconnected browser did not converge on the restore"
        );
    });
}

/// Characterization of the old path, which pins why it is fenced: restore point bytes written over
/// `update-v1.bin` give a restored room, and the first reconnecting client that holds the newer state hands
/// every post-capture item back. (A CRDT cannot delete what a peer still holds.)
#[test]
fn cold_byte_restore_is_undone_by_a_reconnecting_client() {
    with_env(|env| {
        create(env, "cold");
        write_tiptap(
            env,
            "cold",
            vec![
                paragraph("a", "Captured alpha"),
                paragraph("b", "Captured beta"),
            ],
        );
        let room = room_of(env, "cold");
        let captured = restore_point(env, "cold");
        insert_block(env, "cold", "AFTER-CAPTURE-BLOCK", 2);
        let browser = browser_synced(&room);

        // The old restore: the snapshot's bytes over the sidecar, no room involved. A separate registry
        // opens the restored file the way a fresh gardend would.
        let bytes =
            crate::time_travel_store::read_document_bytes(&env.graph_dir(), &captured, "cold")
                .expect("restore point bytes");
        assert!(
            !bytes.is_empty(),
            "the restore point holds the document's bytes"
        );
        let dir =
            std::env::temp_dir().join(format!("garden-cold-restore-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("update-v1.bin");
        std::fs::write(&path, &bytes).unwrap();
        let registry = RoomRegistry::default();
        let cold = run(registry.get_or_create("doc:cold-characterization:cold", path)).unwrap();
        let restored = texts(&["Captured alpha", "Captured beta"]);
        assert_eq!(
            block_texts(&room_json(&cold)),
            restored,
            "the byte swap restored the file"
        );

        // The browser that kept the newer state reconnects.
        let server = room_state_vector(&cold);
        let step2 = browser.transact().encode_state_as_update_v1(&server);
        let changed = run(cold.apply_client_update(&step2)).unwrap();
        assert!(
            changed,
            "the reconnecting client had nothing the byte-restored server lacked"
        );
        assert!(
            block_texts(&room_json(&cold))
                .iter()
                .any(|t| t == "AFTER-CAPTURE-BLOCK"),
            "the byte restore survived a reconnecting client; the fence would be unnecessary"
        );
        let _ = std::fs::remove_dir_all(&dir);
    });
}

/// With no room open for the document the restore still lands on disk, in the saved record and in the RDF
/// projection, and it does not leave a room resident.
#[test]
fn restore_without_rooms_rewrites_disk_and_rdf() {
    with_env(|env| {
        create(env, "bare");
        write_tiptap(
            env,
            "bare",
            vec![paragraph(
                "a",
                "RestoreSentinelOne is the captured content.",
            )],
        );
        let captured = restore_point(env, "bare");
        write_tiptap(
            env,
            "bare",
            vec![paragraph(
                "a",
                "RestoreSentinelTwo is the post-capture content.",
            )],
        );

        // No room for the document.
        let room = room_of(env, "bare");
        let registry = env.app.state::<RoomRegistry>();
        assert!(registry.evict_room_if_identity(&room_key("bare"), &room));
        drop(room);
        assert!(live_room(env, "bare").is_none(), "the room is still cached");

        assert_succeeded(&restore_via_service(env, &captured));

        assert_eq!(
            block_texts(&sidecar_json(env, "bare")),
            texts(&["RestoreSentinelOne is the captured content."]),
            "the sidecar was not restored"
        );
        let body = disk_record(env, "bare")["body"]
            .as_str()
            .unwrap_or("")
            .to_string();
        assert!(
            body.contains("RestoreSentinelOne") && !body.contains("RestoreSentinelTwo"),
            "{body}"
        );
        let markdown = read_markdown(env, "bare");
        assert!(
            markdown.contains("RestoreSentinelOne") && !markdown.contains("RestoreSentinelTwo")
        );

        let store =
            crate::rdf_store_service::open_graph_store(&env.graph_dir()).expect("open store");
        let projection = crate::rdf_authority::document_projection_graph_iri(GRAPH, "bare");
        let ask = |needle: &str| {
            crate::rdf_query_service::execute_sparql_query(
                &store,
                &format!(
                    r#"ASK {{ GRAPH <{projection}> {{ ?s ?p ?o . FILTER(CONTAINS(STR(?o), "{needle}")) }} }}"#
                ),
            )
            .expect("sparql ask")
            .boolean
        };
        assert_eq!(
            ask("RestoreSentinelTwo"),
            Some(false),
            "the post-capture text is still in the RDF"
        );
        assert_eq!(
            ask("RestoreSentinelOne"),
            Some(true),
            "the captured text is not in the RDF"
        );

        assert!(
            live_room(env, "bare").is_none(),
            "the restore left a room it opened resident"
        );
    });
}

/// A document the restore does not change gets no new revision and no write at all; one it changes gets a
/// new revision through the normal save path.
#[test]
fn unchanged_documents_get_no_revision() {
    with_env(|env| {
        create(env, "keep");
        create(env, "change");
        write_tiptap(
            env,
            "keep",
            vec![paragraph("k", "Untouched since the capture")],
        );
        write_tiptap(env, "change", vec![paragraph("c", "Captured text")]);
        let captured = restore_point(env, "unchanged");

        write_tiptap(env, "change", vec![paragraph("c", "Post-capture text")]);
        let keep_revision = record_revision(env, "keep");
        let change_revision = record_revision(env, "change");
        let keep_bytes = std::fs::read(crate::ydoc_paths::document_ydoc_state_path(
            &env.graph_dir(),
            "keep",
        ))
        .unwrap();

        let report = restore_engine(env, &captured);

        assert!(
            report.documents_unchanged.contains(&"keep".to_string()),
            "{report:?}"
        );
        assert!(
            report.documents_changed.contains(&"change".to_string()),
            "{report:?}"
        );
        assert!(
            !report.documents_changed.contains(&"keep".to_string()),
            "{report:?}"
        );
        assert_eq!(
            record_revision(env, "keep"),
            keep_revision,
            "an unchanged document got a new revision"
        );
        assert_eq!(
            std::fs::read(crate::ydoc_paths::document_ydoc_state_path(
                &env.graph_dir(),
                "keep"
            ))
            .unwrap(),
            keep_bytes,
            "an unchanged document's sidecar was rewritten"
        );
        assert!(
            record_revision(env, "change") > change_revision,
            "a restored document did not get a new revision through the save path"
        );
        assert_eq!(
            block_texts(&sidecar_json(env, "change")),
            texts(&["Captured text"])
        );
    });
}

/// Q3: a document deleted after the capture stays deleted. The restore skips it and says so.
#[test]
fn deleted_after_capture_is_skipped_and_reported() {
    with_env(|env| {
        create(env, "gone");
        create(env, "stay");
        write_tiptap(
            env,
            "gone",
            vec![paragraph("g", "Written before the capture")],
        );
        write_tiptap(env, "stay", vec![paragraph("s", "Captured text")]);
        let captured = restore_point(env, "deleted");

        env.call("delete_document", json!({"documentId": "gone"}))
            .unwrap();
        write_tiptap(env, "stay", vec![paragraph("s", "Post-capture text")]);
        assert!(
            crate::document_tombstone_store::document_is_tombstoned(&env.graph_dir(), "gone")
                .unwrap(),
            "the delete must tombstone the document"
        );

        let report = restore_engine(env, &captured);

        assert!(
            report
                .documents_skipped_deleted
                .contains(&"gone".to_string()),
            "the skipped document is not reported: {report:?}"
        );
        assert!(
            crate::document_tombstone_store::document_is_tombstoned(&env.graph_dir(), "gone")
                .unwrap(),
            "the restore brought a deleted document back"
        );
        assert!(
            !in_tree(env, "gone"),
            "a deleted document reappeared in the tree"
        );
        assert!(
            live_room(env, "gone").is_none(),
            "the restore opened a room for a deleted document"
        );
        assert_eq!(
            block_texts(&sidecar_json(env, "stay")),
            texts(&["Captured text"]),
            "the rest of the graph was not restored"
        );
    });
}

/// Q4: a document created after the capture leaves the tree (as before), its files stay, and restoring the
/// automatic pre-restore backup brings it back.
#[test]
fn post_capture_document_leaves_the_tree_files_stay() {
    with_env(|env| {
        create(env, "old");
        write_tiptap(env, "old", vec![paragraph("o", "Captured text")]);
        let captured = restore_point(env, "post-capture-doc");

        create(env, "late");
        write_tiptap(
            env,
            "late",
            vec![paragraph("l", "Written after the capture")],
        );
        assert!(in_tree(env, "late"));

        assert_succeeded(&restore_via_service(env, &captured));

        assert!(in_tree(env, "old"), "a captured document left the tree");
        assert!(
            !in_tree(env, "late"),
            "a post-capture document stayed in the tree"
        );
        let late_sidecar = crate::ydoc_paths::document_ydoc_state_path(&env.graph_dir(), "late");
        let late_record = crate::paths::documents_dir(&env.graph_dir())
            .join("late")
            .join("document.json");
        assert!(
            late_sidecar.is_file(),
            "the post-capture document's Y.Doc was deleted"
        );
        assert!(
            late_record.is_file(),
            "the post-capture document's record was deleted"
        );

        // The automatic backup taken at the start of the restore has it.
        let listed = env.call("list_restore_points", json!({})).unwrap();
        let backup = listed["restorePoints"]
            .as_array()
            .unwrap()
            .iter()
            .find(|point| {
                point["label"]
                    .as_str()
                    .is_some_and(|label| label.starts_with("auto-backup-before-restore/"))
            })
            .unwrap_or_else(|| panic!("no automatic backup among {listed}"))["restorePointId"]
            .as_str()
            .unwrap()
            .to_string();
        assert_succeeded(&restore_via_service(env, &backup));
        assert!(
            in_tree(env, "late"),
            "restoring the backup did not bring the document back"
        );
        assert_eq!(
            block_texts(&sidecar_json(env, "late")),
            texts(&["Written after the capture"])
        );
    });
}

/// Rollback is the same engine pointed at the automatic backup: a restore that fails after restoring one
/// document puts it back, as a forward step (the state vector never goes back).
#[test]
fn rollback_is_a_forward_restore_to_the_backup() {
    with_env(|env| {
        create(env, "r1");
        create(env, "r2");
        write_tiptap(env, "r1", vec![paragraph("a", "R1 captured")]);
        write_tiptap(env, "r2", vec![paragraph("b", "R2 captured")]);
        let captured = restore_point(env, "rollback");
        write_tiptap(env, "r1", vec![paragraph("a", "R1 post-capture")]);
        write_tiptap(env, "r2", vec![paragraph("b", "R2 post-capture")]);

        // Documents restore in id order: r1 restores, then r2 fails on bytes that are not a Yjs update.
        let bytes_path = crate::time_travel_paths::restore_point_document_bytes_path(
            &env.graph_dir(),
            &captured,
            "r2",
        );
        std::fs::write(&bytes_path, b"this is not a yjs update").unwrap();

        let room_r1 = room_of(env, "r1");
        let before = room_state_vector(&room_r1);

        let operation = restore_via_service(env, &captured);
        assert!(
            operation["state"] == "rolled_back"
                || operation["error"].to_string().contains("rolled_back"),
            "the failed restore was not rolled back: {operation}"
        );

        // Both documents are as they were when the restore began.
        assert_eq!(
            block_texts(&room_json(&room_r1)),
            texts(&["R1 post-capture"]),
            "the rollback did not put the first document back"
        );
        assert_eq!(
            block_texts(&sidecar_json(env, "r1")),
            texts(&["R1 post-capture"])
        );
        assert_eq!(
            block_texts(&sidecar_json(env, "r2")),
            texts(&["R2 post-capture"]),
            "the failed document changed"
        );
        let room_r2 = room_of(env, "r2");
        assert_eq!(
            block_texts(&room_json(&room_r2)),
            texts(&["R2 post-capture"])
        );

        // Forward only: nothing the first document's room knew was forgotten.
        let after = room_state_vector(&room_r1);
        for (client, clock) in before.iter() {
            assert!(
                after.get(client) >= *clock,
                "the state vector went back for client {client}: {} < {clock}",
                after.get(client)
            );
        }
    });
}

/// I12: restore never touches `_operationLog`. A block operation applied after the capture keeps its ledger
/// entry through the restore, so a retry of that operation cannot apply twice.
#[test]
fn restore_keeps_the_operation_ledger() {
    with_env(|env| {
        create(env, "ledger");
        write_tiptap(env, "ledger", vec![paragraph("a", "Captured")]);
        let captured = restore_point(env, "ledger");

        let insert = json!({"operationId": "ledger-op-1", "content": "LEDGER-BLOCK",
                            "format": "markdown", "index": 1});
        let enqueue = || {
            run(crate::crdt_queue::enqueue_crdt_operation(
                env.app.clone(),
                crate::crdt_queue::EnqueueCrdtOperationInput {
                    kind: "block.insert".to_string(),
                    graph_id: GRAPH.to_string(),
                    document_id: Some("ledger".to_string()),
                    payload: insert.clone(),
                },
            ))
            .unwrap()
        };
        enqueue();
        let room = room_of(env, "ledger");
        let ledger_keys = |room: &Arc<Room>| -> Vec<String> {
            run(room.with_doc(|doc| {
                let txn = doc.transact();
                let keys: Vec<String> = match txn.get_map("_operationLog") {
                    Some(ledger) => ledger.keys(&txn).map(|key| key.to_string()).collect(),
                    None => Vec::new(),
                };
                keys
            }))
        };
        assert!(
            ledger_keys(&room).contains(&"ledger-op-1".to_string()),
            "the block operation left no ledger entry"
        );
        assert_eq!(
            block_texts(&room_json(&room)),
            texts(&["Captured", "LEDGER-BLOCK"])
        );

        assert_succeeded(&restore_via_service(env, &captured));

        assert_eq!(block_texts(&room_json(&room)), texts(&["Captured"]));
        assert!(
            ledger_keys(&room).contains(&"ledger-op-1".to_string()),
            "the restore rewound the operation ledger"
        );

        // The retry of the same operation is a replay, not a second insert.
        enqueue();
        assert_eq!(
            block_texts(&room_json(&room)),
            texts(&["Captured"]),
            "a retried operation applied twice after the restore"
        );
    });
}

/// A Flow board restores through the same engine (its `resource` and `scene` roots), with the board live.
#[test]
fn flow_board_restore_end_to_end() {
    with_env(|env| {
        let seeded = env
            .call("flow_seed_board", json!({"json": GOLDEN_BOARD}))
            .unwrap();
        let board = seeded["documentId"].as_str().unwrap().to_string();
        let export = |env: &Env| -> String {
            env.call("flow_export_board", json!({})).unwrap()["json"]
                .as_str()
                .unwrap()
                .to_string()
        };
        let captured_export = export(env);
        assert!(captured_export.contains("task-2"));
        let captured = restore_point(env, "flow");

        // Replace the board with the golden board minus task-2.
        let mut modified: Value = serde_json::from_str(GOLDEN_BOARD).unwrap();
        modified["tasks"]
            .as_array_mut()
            .unwrap()
            .retain(|row| row["id"] != "task-2");
        env.call(
            "flow_seed_board",
            json!({"json": serde_json::to_string(&modified).unwrap(), "replace": true}),
        )
        .unwrap();
        let replaced = export(env);
        assert!(
            !replaced.contains("task-2"),
            "the replace did not drop task-2"
        );
        let room = room_of(env, &board);
        let state_before = run(room.encode_state_for_test());

        assert_succeeded(&restore_via_service(env, &captured));

        let restored = export(env);
        assert_eq!(
            restored, captured_export,
            "the restored Flow board is not the captured one"
        );
        assert!(restored.contains("task-2"));
        assert_ne!(
            run(room.encode_state_for_test()),
            state_before,
            "the live Flow board room was not changed by the restore"
        );
        let still = live_room(env, &board).expect("the board's room stays cached");
        assert!(Arc::ptr_eq(&still, &room));
    });
}

/// A whiteboard restores element by element with versions bumped above both sides, so an open canvas (which
/// keeps the higher `version`) accepts the restore: a moved element returns, a post-capture one is tombstoned.
#[test]
fn whiteboard_restore_end_to_end() {
    with_env(|env| {
        let board = env
            .call("whiteboard_create", json!({"title": "Restorable"}))
            .unwrap()["documentId"]
            .as_str()
            .unwrap()
            .to_string();
        let created = env
            .call(
                "whiteboard_apply",
                json!({"documentId": board,
                       "moves": [{"op": "add", "ref": "keep", "shape": "note", "text": "before the restore point"}]}),
            )
            .unwrap();
        let keep = created["created"]["keep"].as_str().unwrap().to_string();

        let element = |env: &Env, id: &str| -> Value {
            let read = env
                .call(
                    "whiteboard_read",
                    json!({"documentId": board, "format": "elements"}),
                )
                .unwrap();
            read["elements"]
                .as_array()
                .unwrap()
                .iter()
                .find(|el| el["id"] == id)
                .cloned()
                .unwrap_or(Value::Null)
        };
        let captured_element = element(env, &keep);
        assert!(
            captured_element.is_object(),
            "the element exists at capture"
        );
        let captured = restore_point(env, "whiteboard");

        // After the capture: move the element (its version rises) and add another.
        let moved = env
            .call(
                "whiteboard_apply",
                json!({"documentId": board,
                       "moves": [{"op": "move", "id": keep, "at": {"x": 900, "y": 50}},
                                 {"op": "add", "ref": "extra", "shape": "note", "text": "after the restore point"}]}),
            )
            .unwrap();
        let extra = moved["created"]["extra"].as_str().unwrap().to_string();
        let moved_element = element(env, &keep);
        let moved_version = moved_element["version"].as_u64().unwrap();
        assert!(
            moved_version > captured_element["version"].as_u64().unwrap(),
            "the move did not raise the element's version"
        );

        // An open canvas: a second Y.Doc synced to the board's room.
        let room = room_of(env, &board);
        let canvas = browser_synced(&room);
        let mut rx = room.subscribe_updates_for_test();

        assert_succeeded(&restore_via_service(env, &captured));

        let restored_element = element(env, &keep);
        assert_eq!(
            (
                restored_element["x"].as_f64(),
                restored_element["y"].as_f64()
            ),
            (
                captured_element["x"].as_f64(),
                captured_element["y"].as_f64()
            ),
            "the moved element did not return to its captured place"
        );
        assert!(
            restored_element["version"].as_u64().unwrap() > moved_version,
            "the restored element does not outrank the version it replaced: {restored_element}"
        );
        let extra_element = element(env, &extra);
        assert!(
            extra_element.is_null() || extra_element["isDeleted"] == true,
            "the post-capture element survived the restore: {extra_element}"
        );
        let outline = env
            .call("whiteboard_read", json!({"documentId": board}))
            .unwrap();
        let markdown = outline["markdown"].as_str().unwrap_or_default();
        assert!(
            markdown.contains("before the restore point")
                && !markdown.contains("after the restore point"),
            "{markdown}"
        );
        let body = disk_record(env, &board)["body"]
            .as_str()
            .unwrap_or("")
            .to_string();
        assert!(
            body.contains("before the restore point") && !body.contains("after the restore point"),
            "{body}"
        );
        assert_eq!(disk_record(env, &board)["documentKind"], "whiteboard");

        // The open canvas receives the restore and holds the element at the restored version.
        for update in drain_update_bytes(&mut rx) {
            apply(&canvas, &update);
        }
        let elements = canvas.get_or_insert_map("elements");
        let txn = canvas.transact();
        let held = match elements.get(&txn, keep.as_str()) {
            Some(Out::Any(Any::String(text))) => text.to_string(),
            other => panic!("the canvas holds no element {keep}: {other:?}"),
        };
        let held: Value = serde_json::from_str(&held).unwrap();
        assert_eq!(
            held["version"], restored_element["version"],
            "the open canvas did not receive the restored version"
        );
        assert_eq!(held["x"].as_f64(), captured_element["x"].as_f64());
    });
}
