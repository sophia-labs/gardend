//! The graph-wide read fence (`flush_ops::await_document_projections`, taken by
//! `sparql_query` and `search_blocks`): read-after-write through every write
//! path, and the cost of a read measured by COUNTERS (documents hydrated,
//! skipped, projected), never by time. Real graph, real rooms, real sidecars,
//! the real dispatch table. No mocks.
//!
//! Existing tests this work touches and that must stay green:
//! `persistence_tests::recovered_automatic_workspace_flush_does_not_hydrate_unrelated_documents`,
//! `persistence_tests::comments_only_edits_advance_manifest_revision_and_history`
//! (the fence after a comment write), the recovery test that evicts a hot room
//! and repairs "on reopen" through the fence, `otel_tests` (the fence under a
//! held projection gate), `wire_iri_store_tests::document_target_validation_sees_acknowledged_blocks_before_projection`,
//! `block_contract_store_tests` (every `stored()`/`markdown()` goes through the
//! fence), and the `rdf_seed_service` marker tests.
use std::sync::Arc;

use serde_json::{json, Value};
use yrs::updates::decoder::Decode;
use yrs::{Transact, Update, WriteTxn};

use crate::crdt_engine::flush_ops::{
    await_document_projections, last_fence_counts_for_test, FenceCounts,
};
use crate::crdt_engine::rooms::RoomRegistry;
use crate::local_jobs::LocalJobRegistry;
use crate::mcp_dispatch_registry::{lookup, McpCallCtx};

const GRAPH: &str = "fence-graph";

struct Env {
    app: crate::app_runtime::AppHandle,
    jobs: Arc<LocalJobRegistry>,
    #[allow(dead_code)]
    profile: std::path::PathBuf,
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
    let profile = std::env::temp_dir().join(format!("garden-fence-{}", uuid::Uuid::new_v4()));
    std::env::set_var("GARDEN_PROFILE_DIR", &profile);
    let jobs_dir = std::env::temp_dir().join(format!("garden-fence-jobs-{}", uuid::Uuid::new_v4()));
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let app = crate::tauri_runtime::build_mock_app_for_tests(true);
        crate::graph_service::create_graph_service(
            &app,
            crate::graph_service::CreateGraphInput {
                graph_id: Some(GRAPH.into()),
                title: "Read fence".into(),
                description: None,
                operation_id: None,
            },
        )
        .unwrap();
        let jobs = Arc::new(LocalJobRegistry::new(jobs_dir.clone()).expect("job registry"));
        test(&Env {
            app,
            jobs,
            profile: profile.clone(),
        });
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

fn create(env: &Env, document_id: &str) {
    run(
        crate::mcp_workspace_crdt_service::mcp_local_create_document(
            env.app.clone(),
            &json!({"graphId": GRAPH, "documentId": document_id, "title": document_id}),
        ),
    )
    .unwrap();
}

fn paragraph(block_id: &str, text: &str) -> Value {
    json!({
        "type": "paragraph",
        "attrs": { "data-block-id": block_id },
        "content": [{ "type": "text", "text": text }],
    })
}

/// `write_document` through the dispatch table: a whole-document write.
fn write_document(env: &Env, document_id: &str, text: &str) {
    env.call(
        "write_document",
        json!({"graph_id": GRAPH, "document_id": document_id,
               "tiptapJson": {"type": "doc", "content": [paragraph(&format!("{document_id}-b"), text)]}}),
    )
    .unwrap();
}

/// `insert_blocks` through the dispatch table: an MCP block insert.
fn insert_block(env: &Env, document_id: &str, text: &str) {
    env.call(
        "insert_blocks",
        json!({"documentId": document_id, "content": text, "format": "markdown", "index": 0}),
    )
    .unwrap();
}

/// A browser's frame on a resident room: a second client Doc syncs the room's
/// state, appends a paragraph, and sends only its update to the room, exactly
/// what the WebSocket handler does with a binary sync frame.
fn websocket_frame(env: &Env, document_id: &str, text: &str) {
    run(async {
        let registry = env.app.state::<RoomRegistry>();
        let graph_dir = crate::paths::existing_graph_dir(&env.app, GRAPH).unwrap();
        let room = registry
            .get_or_create(
                &format!("doc:{GRAPH}:{document_id}"),
                crate::ydoc_paths::document_ydoc_state_path(&graph_dir, document_id),
            )
            .await
            .unwrap();
        let client = yrs::Doc::new();
        {
            let state = room.encode_state_for_test().await;
            let mut txn = client.transact_mut();
            txn.apply_update(Update::decode_v1(&state).unwrap())
                .unwrap();
        }
        let update = {
            let mut txn = client.transact_mut();
            let fragment = txn.get_or_insert_xml_fragment("content");
            crate::crdt_engine::builder::append_nodes(
                &mut txn,
                &fragment,
                &[paragraph(&format!("{document_id}-ws"), text)],
            );
            txn.encode_update_v1()
        };
        let changed = room
            .apply_client_update_and_schedule(&update, env.app.clone())
            .await
            .unwrap();
        assert!(changed, "the room accepted the frame");
    });
}

/// Is a block with exactly this text in the graph's RDF, asked the way an agent
/// asks: `sparql_query` through the dispatch table (fence, seed check, query).
fn ask(env: &Env, text: &str) -> bool {
    let result = env
        .call(
            "sparql_query",
            json!({"query": format!(
                "ASK WHERE {{ GRAPH ?g {{ ?s <http://mnemosyne.dev/doc#textContent> \"{text}\" }} }}"
            )}),
        )
        .unwrap_or_else(|error| panic!("sparql_query for {text}: {error}"));
    result["boolean"]
        .as_bool()
        .unwrap_or_else(|| panic!("ASK returned no boolean: {result}"))
}

fn fence(app: &crate::app_runtime::AppHandle) -> FenceCounts {
    run(await_document_projections(
        app.clone(),
        GRAPH.to_string(),
        None,
    ))
    .unwrap();
    last_fence_counts_for_test(GRAPH)
}

fn document_rooms(env: &Env) -> Vec<String> {
    run(async {
        env.app
            .state::<RoomRegistry>()
            .rooms_with_prefix(&format!("doc:{GRAPH}:"))
            .await
            .into_iter()
            .map(|(key, _)| key)
            .collect()
    })
}

/// Drop every document room, as an idle eviction or a restart does. The
/// workspace room stays: it is not what the fence walks.
fn evict_document_rooms(env: &Env) {
    let registry = env.app.state::<RoomRegistry>();
    for key in document_rooms(env) {
        registry.evict_room(&key);
    }
}

/// `count` documents, each with its own text, projected and then cold. The
/// first fence projects whatever the writes left dirty; evicting and fencing
/// again lets any document that no flush has stamped yet be stamped (the old
/// slow path, once); evicting again leaves them cold AND current.
fn cold_and_current(env: &Env, count: usize) -> Vec<String> {
    let ids: Vec<String> = (0..count).map(|index| format!("cold-{index}")).collect();
    for id in &ids {
        create(env, id);
        write_document(env, id, &format!("text of {id}"));
    }
    fence(&env.app);
    evict_document_rooms(env);
    fence(&env.app);
    evict_document_rooms(env);
    assert!(document_rooms(env).is_empty(), "every document is cold");
    ids
}

#[test]
fn a_query_right_after_an_acknowledged_write_sees_it_through_every_write_path() {
    with_env(|env| {
        for id in ["via-mcp", "via-write", "via-socket"] {
            create(env, id);
        }
        // Seed the store, so each write below is a write AFTER a seeded read.
        assert!(!ask(env, "never written"));

        insert_block(env, "via-mcp", "mcp insert marker");
        assert!(ask(env, "mcp insert marker"), "MCP insert_blocks");

        write_document(env, "via-write", "write document marker");
        assert!(ask(env, "write document marker"), "write_document");

        websocket_frame(env, "via-socket", "websocket frame marker");
        assert!(
            ask(env, "websocket frame marker"),
            "a frame on a resident room"
        );

        // Earlier writes are still there, and a marker nobody wrote is not.
        assert!(ask(env, "mcp insert marker"));
        assert!(ask(env, "write document marker"));
        assert!(!ask(env, "a marker nobody wrote"));
    });
}

#[test]
fn a_query_on_cold_current_documents_hydrates_none_of_them() {
    with_env(|env| {
        let ids = cold_and_current(env, 12);

        let counts = fence(&env.app);
        assert_eq!(counts.hydrated, 0, "{counts:?}");
        assert_eq!(counts.projected, 0, "{counts:?}");
        assert_eq!(counts.skipped_current, ids.len() as u64, "{counts:?}");
        assert!(
            document_rooms(env).is_empty(),
            "the fence opened no room: {:?}",
            document_rooms(env)
        );

        // The same through the real tool: the answer is right AND still free.
        for id in &ids {
            assert!(ask(env, &format!("text of {id}")), "{id}");
        }
        let counts = last_fence_counts_for_test(GRAPH);
        assert_eq!((counts.hydrated, counts.projected), (0, 0), "{counts:?}");
        assert!(document_rooms(env).is_empty());
    });
}

#[test]
fn a_restarted_process_finds_the_stamps_and_hydrates_none() {
    with_env(|env| {
        let ids = cold_and_current(env, 6);
        // A new process: a new registry over the same profile. This is
        // `boot.first_sparql`: before the stamp every document was decoded.
        let restarted = crate::tauri_runtime::build_mock_app_for_tests(true);
        let counts = fence(&restarted);
        assert_eq!(counts.hydrated, 0, "{counts:?}");
        assert_eq!(counts.projected, 0, "{counts:?}");
        assert_eq!(counts.skipped_current, ids.len() as u64, "{counts:?}");
    });
}

#[test]
fn a_query_after_k_documents_change_touches_only_those_k() {
    with_env(|env| {
        let ids = cold_and_current(env, 8);
        let changed = &ids[..3];
        for id in changed {
            insert_block(env, id, &format!("changed {id}"));
        }

        let counts = fence(&env.app);
        assert_eq!(
            counts.hydrated, 0,
            "no cold document was opened: {counts:?}"
        );
        assert_eq!(
            counts.skipped_current,
            (ids.len() - changed.len()) as u64,
            "the five untouched documents were skipped: {counts:?}"
        );
        assert_eq!(
            counts.projected + counts.resident_current,
            changed.len() as u64,
            "exactly the K changed documents were judged by their rooms: {counts:?}"
        );

        for id in changed {
            assert!(ask(env, &format!("changed {id}")), "{id}");
        }
        for id in &ids[3..] {
            assert!(!ask(env, &format!("changed {id}")), "{id} did not change");
            assert!(ask(env, &format!("text of {id}")), "{id} is still there");
        }
    });
}

/// The negative control for the stamp: a write that reached the sidecar but
/// not the projection (the process died between the hot commit and the flush)
/// leaves a stamp behind that no longer matches. The fence must open that one
/// document, project it, and still skip its neighbours.
#[test]
fn a_write_that_never_reached_its_projection_is_not_hidden_by_an_old_stamp() {
    with_env(|env| {
        let ids = cold_and_current(env, 4);
        let victim = &ids[0];
        run(async {
            let registry = env.app.state::<RoomRegistry>();
            let graph_dir = crate::paths::existing_graph_dir(&env.app, GRAPH).unwrap();
            let key = format!("doc:{GRAPH}:{victim}");
            let room = registry
                .get_or_create(
                    &key,
                    crate::ydoc_paths::document_ydoc_state_path(&graph_dir, victim),
                )
                .await
                .unwrap();
            // Hold the projection gate: the write replies, its flush cannot run.
            let gate = room.lock_projection_flush().await;
            tokio::time::timeout(
                std::time::Duration::from_secs(5),
                crate::crdt_queue::enqueue_crdt_operation(
                    env.app.clone(),
                    crate::crdt_queue::EnqueueCrdtOperationInput {
                        kind: "block.insert".into(),
                        graph_id: GRAPH.into(),
                        document_id: Some(victim.clone()),
                        payload: json!({"blocks": [{"id": "orphan-block", "content": "orphan marker"}]}),
                    },
                ),
            )
            .await
            .expect("the write replies while its projection is held")
            .unwrap();
            // The process "dies": the room and its debounce are gone, the
            // sidecar keeps the write, the record and the stamp do not.
            registry.evict_room(&key);
            drop(gate);
        });

        let counts = fence(&env.app);
        assert_eq!(counts.hydrated, 1, "only the victim was opened: {counts:?}");
        assert_eq!(counts.projected, 1, "and projected: {counts:?}");
        assert_eq!(
            counts.skipped_current, 3,
            "its neighbours were skipped: {counts:?}"
        );
        assert!(ask(env, "orphan marker"), "the orphan write is visible");

        // And now it is current again.
        evict_document_rooms(env);
        let counts = fence(&env.app);
        assert_eq!((counts.hydrated, counts.projected), (0, 0), "{counts:?}");
        assert_eq!(counts.skipped_current, 4, "{counts:?}");
    });
}

/// A resident room is judged by its committed epoch: once projected it is
/// current, and a second fence neither projects it again nor opens anything.
#[test]
fn a_resident_room_that_is_clean_is_not_projected_again() {
    with_env(|env| {
        let ids = cold_and_current(env, 2);
        let id = &ids[0];
        let room = run(async {
            let registry = env.app.state::<RoomRegistry>();
            let graph_dir = crate::paths::existing_graph_dir(&env.app, GRAPH).unwrap();
            registry
                .get_or_create(
                    &format!("doc:{GRAPH}:{id}"),
                    crate::ydoc_paths::document_ydoc_state_path(&graph_dir, id),
                )
                .await
                .unwrap()
        });
        // Reopened by the test, so it starts dirty: the first fence projects it.
        let first = fence(&env.app);
        assert_eq!(first.hydrated, 0, "{first:?}");
        assert_eq!(first.projected, 1, "{first:?}");
        assert_eq!(first.skipped_current, 1, "{first:?}");
        assert!(!room.needs_projection_flush());
        // Now it is resident and clean.
        let second = fence(&env.app);
        assert_eq!(second.hydrated, 0, "{second:?}");
        assert_eq!(second.projected, 0, "{second:?}");
        assert_eq!(second.resident_current, 1, "{second:?}");
        assert_eq!(second.skipped_current, 1, "{second:?}");
    });
}

/// O2: a save carries the seed marker with its bump, so a read after a save
/// neither walks the workspace and documents again nor marks the store dirty
/// for the next checkpoint. Before, every read after a save did both.
#[test]
fn a_read_after_a_save_neither_walks_nor_dirties_the_store() {
    with_env(|env| {
        let graph_dir = crate::paths::existing_graph_dir(&env.app, GRAPH).unwrap();
        let store_path = graph_dir.join("store.oxigraph");
        let durable = env.profile.with_file_name(format!(
            "{}-durable",
            env.profile.file_name().unwrap().to_string_lossy()
        ));
        std::fs::create_dir_all(&durable).unwrap();

        create(env, "seeded");
        write_document(env, "seeded", "first text");
        assert!(ask(env, "first text"), "the first query seeds the store");

        // A clean baseline: everything written so far is checkpointed.
        crate::cell_durability::flush(&env.profile, &durable).expect("baseline checkpoint");
        let walks = crate::rdf_seed_service::reseed_count(&store_path);

        // A save: the block lands in the room, the fence projects it (record,
        // RDF, history tail, content_revision bump).
        insert_block(env, "seeded", "second text");
        fence(&env.app);
        // What the save itself wrote is checkpointed...
        crate::cell_durability::flush(&env.profile, &durable).expect("checkpoint after the save");

        // ...and a pure read after it finds the marker current.
        assert!(ask(env, "second text"), "the saved text is visible");
        assert_eq!(
            crate::rdf_seed_service::reseed_count(&store_path),
            walks,
            "no seed walk for a read after a save"
        );
        let after_read =
            crate::cell_durability::flush(&env.profile, &durable).expect("checkpoint after read");
        assert!(
            !after_read.published,
            "a pure read left the store clean: nothing to publish"
        );
        let _ = std::fs::remove_dir_all(&durable);
    });
}
