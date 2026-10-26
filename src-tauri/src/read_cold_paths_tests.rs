//! R3/R4: the orientation and block readers do not hydrate Y.Doc history, and
//! `read_document` still hands an agent the comments (from the live room or the
//! sidecar's bytes). Real graph, real documents, the real dispatch table.
use std::sync::Arc;

use serde_json::{json, Value};

use crate::local_jobs::LocalJobRegistry;
use crate::mcp_dispatch_registry::{lookup, McpCallCtx};

const GRAPH: &str = "cold-graph";

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
}

fn run<F: std::future::Future>(future: F) -> F::Output {
    crate::app_runtime::async_runtime::block_on(future)
}

fn with_env(test: impl FnOnce(&Env)) {
    let _serial = crate::tauri_runtime::profile_env_serial()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let previous = std::env::var_os("GARDEN_PROFILE_DIR");
    let profile = std::env::temp_dir().join(format!("garden-cold-{}", uuid::Uuid::new_v4()));
    std::env::set_var("GARDEN_PROFILE_DIR", &profile);
    let jobs_dir = std::env::temp_dir().join(format!("garden-cold-jobs-{}", uuid::Uuid::new_v4()));
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let app = crate::tauri_runtime::build_mock_app_for_tests(true);
        crate::graph_service::create_graph_service(
            &app,
            crate::graph_service::CreateGraphInput {
                graph_id: Some(GRAPH.into()),
                title: "Cold".into(),
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

/// The paragraph carries its own `data-block-id`: a `tiptapJson` write owns its
/// ids, and gardend mints none for it, so an id-less block would be given a
/// fresh random id by every projection and could never be addressed in the room
/// (a pre-existing engine gap, filed separately; not what these tests are about).
fn make_document(env: &Env, document_id: &str, text: &str) {
    run(
        crate::mcp_workspace_crdt_service::mcp_local_create_document(
            env.app.clone(),
            &json!({"graphId": GRAPH, "documentId": document_id, "title": document_id}),
        ),
    )
    .unwrap();
    run(crate::document_mutation_service::mcp_local_write_document(
        env.app.clone(),
        &json!({"graph_id": GRAPH, "document_id": document_id,
        "tiptapJson": {"type": "doc", "content": [
            {"type": "paragraph", "attrs": {"data-block-id": format!("block-{document_id}")},
             "content": [{"type": "text", "text": text}]}
        ]}}),
    ))
    .unwrap();
}

/// quick_orient, context_bundle, read_blocks, get_block, query_blocks and
/// document_digest read records cold: no sidecar read, no base64 of the whole
/// history, on the handler's own thread.
#[test]
fn orientation_and_block_readers_do_not_hydrate_history() {
    with_env(|env| {
        make_document(env, "alpha", "Dreams are the royal road.");
        make_document(env, "beta", "A second document.");
        let ids = env
            .call(
                "read_document",
                json!({"documentId": "alpha", "format": "ids_only"}),
            )
            .unwrap();
        let block = ids["block_ids"][0].as_str().expect("a block").to_string();

        crate::document_record_store::reset_record_read_counts_for_test();
        let orient = env.call("quick_orient", json!({})).unwrap();
        assert_eq!(orient["workspace"]["counts"]["documents"], 2, "{orient}");
        env.call("context_bundle", json!({})).unwrap();
        env.call("read_blocks", json!({"documentId": "alpha"}))
            .unwrap();
        env.call(
            "get_block",
            json!({"documentId": "alpha", "blockId": block}),
        )
        .unwrap();
        env.call(
            "query_blocks",
            json!({"documentId": "alpha", "query": "royal"}),
        )
        .unwrap();
        env.call("document_digest", json!({"documentId": "alpha"}))
            .unwrap();
        let (cold, hydrating) = crate::document_record_store::record_read_counts_for_test();
        assert_eq!(hydrating, 0, "no hydrating record read ({cold} cold reads)");
        assert!(cold > 0, "the cold reader served them");
    });
}

/// `read_document` keeps its comments, from the live room (the tool) and from
/// the sidecar's raw bytes (the direct call), without hydrating the record.
#[test]
fn read_document_comments_come_from_the_room_or_the_sidecar_bytes() {
    with_env(|env| {
        make_document(env, "alpha", "Dreams are the royal road.");
        let ids = env
            .call(
                "read_document",
                json!({"documentId": "alpha", "format": "ids_only"}),
            )
            .unwrap();
        let block = ids["block_ids"][0].as_str().expect("a block").to_string();
        env.call(
            "edit_comment",
            json!({"documentId": "alpha", "action": "set", "commentId": "note-1",
                   "text": "why royal?", "blockId": block, "find": "royal road"}),
        )
        .unwrap();

        // The tool: barrier, then the live room when it is resident.
        let via_tool = env
            .call("read_document", json!({"documentId": "alpha"}))
            .unwrap();
        assert_eq!(via_tool["commentCount"], 1, "{via_tool}");
        assert_eq!(via_tool["comments"][0]["id"], "note-1", "{via_tool}");
        assert_eq!(via_tool["comments"][0]["text"], "why royal?");

        // The direct call reads the sidecar the barrier just brought current.
        let graph_dir = crate::paths::existing_graph_dir(&env.app, GRAPH).unwrap();
        assert!(
            crate::paths::document_ydoc_state_path(&graph_dir, "alpha").is_file(),
            "the barrier left a sidecar"
        );
        crate::document_record_store::reset_record_read_counts_for_test();
        let via_sidecar = crate::document_mcp_service::mcp_local_read_document(
            env.app.clone(),
            &json!({"graphId": GRAPH, "documentId": "alpha"}),
        )
        .unwrap();
        let (_, hydrating) = crate::document_record_store::record_read_counts_for_test();
        assert_eq!(hydrating, 0, "a document with a sidecar is read cold");
        assert_eq!(via_sidecar["comments"], via_tool["comments"]);
        assert_eq!(via_sidecar["commentCount"], 1);

        // includeComments:false still leaves both fields out.
        let left_out = env
            .call(
                "read_document",
                json!({"documentId": "alpha", "includeComments": false}),
            )
            .unwrap();
        assert!(left_out.get("comments").is_none() && left_out.get("commentCount").is_none());
    });
}
