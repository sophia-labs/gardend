//! IDLESS-BLOCKS: a `tiptapJson` block that arrives without an id is given one at write time and
//! stored in the room, so `read_document(format: "ids_only")` hands out ids the room holds and
//! `edit_comment` can address them. Real graph, real documents, the real dispatch table.
use std::sync::Arc;

use serde_json::{json, Value};

use crate::local_jobs::LocalJobRegistry;
use crate::mcp_dispatch_registry::{lookup, McpCallCtx};

const GRAPH: &str = "idless-graph";

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
        crate::app_runtime::async_runtime::block_on((entry.handler)(&ctx, &args))
            .map_err(|error| error.to_string())
    }

    fn ids(&self, document_id: &str) -> Vec<String> {
        let read = self
            .call(
                "read_document",
                json!({"documentId": document_id, "format": "ids_only"}),
            )
            .unwrap();
        read["block_ids"]
            .as_array()
            .unwrap_or_else(|| panic!("block_ids in {read}"))
            .iter()
            .map(|id| id.as_str().unwrap().to_string())
            .collect()
    }
}

fn with_env(test: impl FnOnce(&Env)) {
    let _serial = crate::tauri_runtime::profile_env_serial()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let previous = std::env::var_os("GARDEN_PROFILE_DIR");
    let profile = std::env::temp_dir().join(format!("garden-idless-{}", uuid::Uuid::new_v4()));
    std::env::set_var("GARDEN_PROFILE_DIR", &profile);
    let jobs_dir =
        std::env::temp_dir().join(format!("garden-idless-jobs-{}", uuid::Uuid::new_v4()));
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let app = crate::tauri_runtime::build_mock_app_for_tests(true);
        crate::graph_service::create_graph_service(
            &app,
            crate::graph_service::CreateGraphInput {
                graph_id: Some(GRAPH.into()),
                title: "Idless".into(),
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

fn para(text: &str) -> Value {
    json!({"type": "paragraph", "content": [{"type": "text", "text": text}]})
}

/// The probe scenario: write id-less tiptapJson, read the ids, save again, and address the first
/// block by the id the first read gave.
#[test]
fn idless_tiptap_blocks_keep_their_ids_across_saves_and_take_comments() {
    with_env(|env| {
        env.call(
            "create_document",
            json!({"documentId": "alpha", "title": "alpha"}),
        )
        .unwrap();
        env.call(
            "write_document",
            json!({"documentId": "alpha", "tiptapJson": {"type": "doc", "content": [
                para("Dreams are the royal road.")]}}),
        )
        .unwrap();
        let first = env.ids("alpha");
        assert_eq!(
            first,
            env.ids("alpha"),
            "two reads agree: no id is invented per read"
        );

        env.call(
            "insert_blocks",
            json!({"documentId": "alpha", "content": "A second paragraph.", "format": "markdown"}),
        )
        .unwrap();
        let second = env.ids("alpha");
        assert_eq!(
            second.first(),
            first.first(),
            "the first block's id survived a save"
        );
        assert!(second.len() > first.len());

        // A second tiptapJson write of the same id-less block, plus one more.
        env.call(
            "write_document",
            json!({"documentId": "alpha", "tiptapJson": {"type": "doc", "content": [
                para("Dreams are the royal road."), para("A second paragraph."), para("Third.")]}}),
        )
        .unwrap();
        let third = env.ids("alpha");
        assert_eq!(
            third.first(),
            first.first(),
            "a rewrite keeps the matched block's id"
        );

        env.call(
            "edit_comment",
            json!({"documentId": "alpha", "action": "set", "commentId": "note-1",
                   "text": "why royal?", "blockId": first[0], "find": "royal road"}),
        )
        .expect("edit_comment addresses the id read_document gave");
    });
}

/// Caller-given ids are the caller's: kept as sent, next to an id-less sibling that is minted.
#[test]
fn caller_given_ids_are_preserved_beside_minted_ones() {
    with_env(|env| {
        env.call(
            "create_document",
            json!({"documentId": "beta", "title": "beta"}),
        )
        .unwrap();
        env.call(
            "write_document",
            json!({"documentId": "beta", "tiptapJson": {"type": "doc", "content": [
                {"type": "paragraph", "attrs": {"data-block-id": "mine-1"},
                 "content": [{"type": "text", "text": "given"}]},
                para("not given")]}}),
        )
        .unwrap();
        let ids = env.ids("beta");
        assert_eq!(ids.len(), 2, "{ids:?}");
        assert_eq!(ids[0], "mine-1");
        assert!(ids[1].starts_with("block-"), "{ids:?}");
        assert_eq!(ids, env.ids("beta"));
    });
}

/// Nested minted types (list items, quoted paragraphs) are stored with ids too: every id a read
/// hands out resolves to a block.
#[test]
fn nested_idless_blocks_get_ids_the_room_holds() {
    with_env(|env| {
        env.call(
            "create_document",
            json!({"documentId": "gamma", "title": "gamma"}),
        )
        .unwrap();
        env.call(
            "write_document",
            json!({"documentId": "gamma", "tiptapJson": {"type": "doc", "content": [
                {"type": "bulletList", "content": [
                    {"type": "listItem", "content": [para("one")]},
                    {"type": "listItem", "content": [para("two")]}]},
                {"type": "blockquote", "content": [para("quoted")]}]}}),
        )
        .unwrap();
        let ids = env.ids("gamma");
        assert!(ids.len() >= 3, "{ids:?}");
        assert_eq!(ids, env.ids("gamma"), "stable across reads");
        for id in &ids {
            env.call("get_block", json!({"documentId": "gamma", "blockId": id}))
                .unwrap_or_else(|error| panic!("{id} names no block: {error}"));
        }
    });
}
