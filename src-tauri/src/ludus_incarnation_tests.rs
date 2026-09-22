//! Actual managed public calls, real lifecycle replacement, and queued recovery.
use crate::app_runtime::AppHandle;
use crate::crdt_engine::persistence_coordinator::GraphPersistenceCoordinator;
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    future::Future,
    path::{Path, PathBuf},
    task::Poll,
};

const TOOLS: [&str; 3] = ["emporium_write", "create_document_once", "create_wires"];

fn fixture(body: impl FnOnce(AppHandle, PathBuf)) {
    let _serial = crate::tauri_runtime::profile_env_serial()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let previous = std::env::var_os("GARDEN_PROFILE_DIR");
    let profile = std::env::temp_dir().join(format!("ludus-incarnation-{}", uuid::Uuid::new_v4()));
    std::env::set_var("GARDEN_PROFILE_DIR", &profile);
    println!("LUDUS_INCARNATION_PROFILE={}", profile.display());
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        body(
            crate::tauri_runtime::build_mock_app_for_tests(true),
            profile.clone(),
        );
    }));
    if let Some(value) = previous {
        std::env::set_var("GARDEN_PROFILE_DIR", value);
    } else {
        std::env::remove_var("GARDEN_PROFILE_DIR");
    }
    if let Err(error) = result {
        std::panic::resume_unwind(error);
    }
}

fn create(app: &AppHandle, graph: &str) -> String {
    crate::graph_service::create_graph_service(
        app,
        crate::graph_service::CreateGraphInput {
            graph_id: Some(graph.into()),
            title: "Incarnation test".into(),
            description: None,
            operation_id: None,
        },
    )
    .unwrap()
    .incarnation_id
    .unwrap()
}
fn args(tool: &str, graph: &str, id: &str) -> Value {
    match tool {
        "emporium_write" => {
            let mut record: Value =
                serde_json::from_str::<Value>(include_str!("ludus_source_fixture.json")).unwrap()
                    [0]
                .clone();
            record["localId"] = json!(id);
            json!({"graph_id":graph,"vocab":"ludus-core","operationId":format!("operation-{id}"),"atMs":1788933600000_i64,"records":[record]})
        }
        "create_document_once" => {
            json!({"graph_id":graph,"document_id":id,"title":"Fixture document","order":10,"parentId":null,"awaitDurable":false,
            "tiptapJson":{"type":"doc","content":[{"type":"paragraph","attrs":{"data-block-id":format!("block-{id}")},"content":[{"type":"text","text":"Exact fixture content"}]}]}})
        }
        "create_wires" => {
            json!({"graph_id":graph,"wire_id":id,"source_document_id":"doc-a","target_document_id":"doc-b","predicate":"isWiredTo"})
        }
        "fenced_flush" => json!({"graph_id":graph}),
        _ => panic!("unknown mutator"),
    }
}
async fn call(app: &AppHandle, tool: &str, args: &Value) -> Result<Value, String> {
    match tool {
        "emporium_write" => {
            crate::emporium_mcp_surface::mcp_local_emporium_write(app.clone(), args)
                .await
                .map_err(crate::app_error::AppError::message)
        }
        "create_document_once" => {
            crate::document_create_once_mcp::create_document_once(app.clone(), args).await
        }
        "create_wires" => {
            crate::mcp_workspace_mutation_service::mcp_local_create_wires(app.clone(), args).await
        }
        "fenced_flush" => {
            crate::crdt_projection_flush::flush_projection_incarnation(
                app.clone(),
                args["graph_id"].as_str().unwrap(),
                None,
                args["graphIncarnation"].as_str().unwrap(),
                None,
            )
            .await?;
            Ok(json!({"ok":true}))
        }
        _ => panic!("unknown mutator"),
    }
}
async fn activate(app: &AppHandle, graph: &str, incarnation: &str) -> Value {
    crate::source_sync::mcp_local_source_pull(
        app.clone(),
        &json!({"graphId":graph,"graphIncarnation":incarnation}),
    )
    .await
    .unwrap()
}
fn snapshot(app: &AppHandle, graph: &str) -> Value {
    fn collect(root: &Path, path: &Path, out: &mut BTreeMap<String, String>) {
        if !path.exists() {
            return;
        }
        if path.is_dir() {
            for entry in std::fs::read_dir(path).unwrap() {
                collect(root, &entry.unwrap().path(), out);
            }
        } else {
            use sha2::{Digest, Sha256};
            out.insert(
                path.strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned(),
                format!("{:x}", Sha256::digest(std::fs::read(path).unwrap())),
            );
        }
    }
    let root = crate::paths::existing_graph_dir(app, graph).unwrap();
    let mut files = BTreeMap::new();
    for path in [
        "graph.json",
        "source-sync",
        "document-create-once",
        "documents",
        "ydocs",
        "values",
        "history",
    ] {
        collect(&root, &root.join(path), &mut files);
    }
    let store = crate::rdf_service::open_graph_store(&root).unwrap();
    let dump = crate::rdf_query_service::dump_rdf_from_store(&store, "nquads", None, None).unwrap();
    json!({"files":files,"rdf":dump.data,"quadCount":dump.quad_count})
}
fn journal(app: &AppHandle) -> Option<Vec<u8>> {
    let path = crate::crdt_operation_journal::crdt_operation_journal_path(app).unwrap();
    match std::fs::read(path) {
        Ok(bytes) => Some(bytes),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => panic!("{e}"),
    }
}

#[test]
fn ludus_incarnation_schema_strict_aliases_and_public_capability() {
    use crate::graph_incarnation_admission::expected_incarnation;
    assert_eq!(expected_incarnation(&json!({})).unwrap(), None);
    assert_eq!(
        expected_incarnation(&json!({"graphIncarnation":"same","graph_incarnation":"same"}))
            .unwrap(),
        Some("same".into())
    );
    for value in [
        Value::Null,
        json!(1),
        json!(false),
        json!(""),
        json!(" padded "),
        json!([]),
        json!("x".repeat(257)),
    ] {
        assert!(expected_incarnation(&json!({"graphIncarnation":value})).is_err());
    }
    assert!(
        expected_incarnation(&json!({"graphIncarnation":"a","graph_incarnation":"b"})).is_err()
    );
    let catalog = serde_json::to_value(crate::mcp_tool_registry::mcp_tools_list_result()).unwrap();
    for tool in TOOLS {
        let rows = catalog["tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|row| row["name"] == tool)
            .collect::<Vec<_>>();
        assert_eq!(rows.len(), 1);
        for name in ["graphIncarnation", "graph_incarnation"] {
            assert_eq!(rows[0]["inputSchema"]["properties"][name]["type"], "string");
            assert!(!rows[0]["inputSchema"]["required"]
                .as_array()
                .is_some_and(|r| r.contains(&json!(name))));
        }
    }
}

#[test]
fn ludus_incarnation_actual_current_missing_and_invalid_calls() {
    fixture(|app, _| {
        crate::app_runtime::async_runtime::block_on(async {
            let graph = "ludus-fence-current";
            let incarnation = create(&app, graph);
            let mut inactive = args("emporium_write", graph, "inactive");
            inactive["graphIncarnation"] = json!(incarnation);
            let before = snapshot(&app, graph);
            assert!(call(&app, "emporium_write", &inactive)
                .await
                .unwrap_err()
                .contains("requires active source authority"));
            assert_eq!(snapshot(&app, graph), before);
            let bundle = activate(&app, graph, &incarnation).await;
            let capability = &bundle["mutationCapabilities"]["expectedGraphIncarnation"];
            assert_eq!(capability["version"], 1);
            assert_eq!(capability["tools"], json!(TOOLS));
            assert_eq!(capability["multiOperationAtomic"], false);
            for (index, id) in ["doc-a", "doc-b"].iter().enumerate() {
                let mut input = args("create_document_once", graph, id);
                if index == 0 {
                    input["graphIncarnation"] = json!(incarnation);
                }
                assert_eq!(
                    call(&app, "create_document_once", &input).await.unwrap()["success"],
                    true
                );
            }
            let mut successes = Vec::new();
            for tool in ["emporium_write", "create_wires"] {
                for fenced in [true, false] {
                    let mut input = args(tool, graph, &format!("valid-{tool}-{fenced}"));
                    if fenced {
                        input["graph_incarnation"] = json!(incarnation);
                    }
                    let out = call(&app, tool, &input).await.unwrap();
                    assert_eq!(
                        out[if tool == "emporium_write" {
                            "ok"
                        } else {
                            "success"
                        }],
                        true,
                        "{out}"
                    );
                    successes.push(out);
                }
            }
            for tool in TOOLS {
                for fence in [
                    json!("stale-token"),
                    Value::Null,
                    json!(4),
                    json!(""),
                    json!(" padded "),
                ] {
                    let mut input = args(tool, graph, "must-not-admit");
                    input["graphIncarnation"] = fence;
                    let before = snapshot(&app, graph);
                    let prior_journal = journal(&app);
                    assert!(
                        call(&app, tool, &input).await.is_err(),
                        "{tool} accepted {input}"
                    );
                    assert_eq!(snapshot(&app, graph), before);
                    assert_eq!(journal(&app), prior_journal);
                }
                let mut input = args(tool, graph, "unmanaged-must-refuse");
                input["graphIncarnation"] = json!(incarnation);
                let unmanaged = crate::tauri_runtime::build_mock_app_for_tests(false);
                assert!(call(&unmanaged, tool, &input)
                    .await
                    .unwrap_err()
                    .contains("coordinator"));
            }
            let mut bad_batch = args("create_wires", graph, "top-wire");
            bad_batch["graphIncarnation"] = json!(incarnation);
            bad_batch["wires"] = json!([{"wire_id":"first-must-not-admit"},{"wire_id":"bad","graphIncarnation":"different"}]);
            let before = snapshot(&app, graph);
            let prior_journal = journal(&app);
            assert!(call(&app, "create_wires", &bad_batch).await.is_err());
            assert_eq!(snapshot(&app, graph), before);
            assert_eq!(journal(&app), prior_journal);
            println!(
                "LUDUS_FENCED_ACTUAL_SUCCESS={}",
                json!({"graph":graph,"incarnation":incarnation,"capability":capability,"responses":successes})
            );
        })
    });
}

#[test]
fn ludus_incarnation_waiting_public_calls_refuse_real_replacement_without_effects() {
    fixture(|app, _| {
        crate::app_runtime::async_runtime::block_on(async {
            for tool in TOOLS {
                let graph = format!("ludus-race-{tool}");
                let old = create(&app, &graph);
                activate(&app, &graph, &old).await;
                let coordinator = app.state::<GraphPersistenceCoordinator>();
                let lease = coordinator
                    .acquire_lifecycle_exclusive(&graph)
                    .await
                    .unwrap();
                let mut input = args(tool, &graph, "old-lifetime-request");
                input["graphIncarnation"] = json!(old);
                let mut waiting = Box::pin(call(&app, tool, &input));
                assert!(
                    std::future::poll_fn(|cx| Poll::Ready(waiting.as_mut().poll(cx).is_pending()))
                        .await,
                    "must wait behind the real lifecycle lease"
                );
                let next =
                    crate::graph_service::replace_graph_under_test_lease(&app, &graph, &lease)
                        .unwrap();
                assert_ne!(next.incarnation_id.as_deref(), Some(old.as_str()));
                let before = snapshot(&app, &graph);
                let prior_journal = journal(&app);
                drop(lease);
                let error = tokio::time::timeout(std::time::Duration::from_secs(3), waiting)
                    .await
                    .unwrap()
                    .unwrap_err();
                assert!(error.contains("stale graph incarnation"), "{tool}: {error}");
                assert_eq!(
                    snapshot(&app, &graph),
                    before,
                    "successor data changed for {tool}"
                );
                assert_eq!(
                    journal(&app),
                    prior_journal,
                    "stale admission journaled for {tool}"
                );
                let error = crate::crdt_projection_flush::flush_projection_incarnation(
                    app.clone(),
                    &graph,
                    None,
                    &old,
                    None,
                )
                .await
                .unwrap_err();
                assert!(error.contains("stale graph incarnation"));
                assert_eq!(snapshot(&app, &graph), before);
                assert_eq!(journal(&app), prior_journal);
                println!(
                    "LUDUS_REPLACEMENT_REFUSAL={}",
                    json!({"tool":tool,"old":old,"new":next.incarnation_id,"error":error,"successor":before})
                );
            }
        })
    });
}

#[test]
fn ludus_incarnation_already_queued_calls_and_saved_executor_payload_refuse_successor() {
    fixture(|app, _| {
        crate::app_runtime::async_runtime::block_on(async {
            for tool in ["create_document_once", "create_wires", "fenced_flush"] {
                let graph = format!("ludus-queued-{tool}");
                let old = create(&app, &graph);
                let queue = app.state::<crate::crdt_queue::CrdtOperationQueue>();
                let drain = queue.lock_drain().await;
                let mut input = args(tool, &graph, "queued-old-lifetime");
                input["graphIncarnation"] = json!(old);
                let mut waiting = Box::pin(call(&app, tool, &input));
                assert!(
                    std::future::poll_fn(|cx| Poll::Ready(waiting.as_mut().poll(cx).is_pending()))
                        .await
                );
                let pending =
                    crate::crdt_operation_journal::recover_pending_crdt_operations(&app).unwrap();
                let saved = pending
                    .into_iter()
                    .find(|operation| operation.graph_id == graph)
                    .expect("actual journaled operation");
                assert_eq!(saved.payload["graphIncarnation"], old);
                let coordinator = app.state::<GraphPersistenceCoordinator>();
                let lease = coordinator
                    .acquire_lifecycle_exclusive(&graph)
                    .await
                    .unwrap();
                let next =
                    crate::graph_service::replace_graph_under_test_lease(&app, &graph, &lease)
                        .unwrap();
                let before = snapshot(&app, &graph);
                drop(lease);
                drop(drain);
                assert!(
                    tokio::time::timeout(std::time::Duration::from_secs(3), waiting)
                        .await
                        .unwrap()
                        .is_err()
                );
                let error = crate::crdt_engine::executor::apply_operation(&app, &saved)
                    .await
                    .unwrap_err();
                assert!(error.contains("stale graph incarnation"), "{error}");
                assert_eq!(snapshot(&app, &graph), before);
                println!(
                    "LUDUS_QUEUED_REFUSAL={}",
                    json!({"tool":tool,"operationId":saved.operation_id,"old":old,"new":next.incarnation_id,"error":error})
                );
            }
        })
    });
}
