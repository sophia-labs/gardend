use super::*;
use crate::{
    crdt_engine::{
        executor, persistence_coordinator::GraphPersistenceCoordinator, rooms::RoomRegistry,
    },
    crdt_queue::GRAPH_INCARNATION_PAYLOAD_KEY,
    graph_service::{create_graph_service, CreateGraphInput},
};
use std::sync::Arc;
use yrs::{MapPrelim, WriteTxn, XmlFragment};

const GRAPH: &str = "create-once-tests";
const DOCUMENT: &str = "starter-page";

fn fixture(test: impl FnOnce(AppHandle, PathBuf)) {
    let _serial = crate::tauri_runtime::profile_env_serial()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let profile = std::env::temp_dir().join(format!("garden-create-once-{}", uuid::Uuid::new_v4()));
    std::env::set_var("GARDEN_PROFILE_DIR", &profile);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let app = crate::tauri_runtime::build_mock_app_for_tests(true);
        create_graph_service(
            &app,
            CreateGraphInput {
                title: "Create Once".into(),
                graph_id: Some(GRAPH.into()),
                description: None,
                operation_id: None,
            },
        )
        .unwrap();
        let graph_dir = crate::graph_paths::existing_graph_dir(&app, GRAPH).unwrap();
        test(app, graph_dir);
    }));
    TEST_ACTION.lock().unwrap_or_else(|e| e.into_inner()).take();
    std::env::remove_var("GARDEN_PROFILE_DIR");
    let _ = fs::remove_dir_all(profile);
    if let Err(error) = result {
        std::panic::resume_unwind(error);
    }
}

fn arguments() -> Value {
    json!({"graph_id":GRAPH,"document_id":DOCUMENT,"title":"Starter page","order":10,"parentId":null,"awaitDurable":true,
        "tiptapJson":{"type":"doc","content":[{"type":"paragraph","attrs":{"data-block-id":"starter-block"},"content":[{"type":"text","text":"Original starter text"}]}]}})
}

fn operation(app: &AppHandle, id: &str) -> CrdtOperation {
    let (_, _, mut payload) = mcp_input(&arguments()).unwrap();
    payload[GRAPH_INCARNATION_PAYLOAD_KEY] = json!(
        crate::graph_record_store::read_graph_record_no_heal(app, GRAPH)
            .unwrap()
            .1
            .incarnation_id
            .unwrap()
    );
    CrdtOperation {
        operation_id: id.into(),
        kind: "document.createOnce".into(),
        graph_id: GRAPH.into(),
        document_id: Some(DOCUMENT.into()),
        payload,
        enqueue_timestamp: crate::clock::timestamp(),
    }
}

async fn execute(app: &AppHandle, operation: &CrdtOperation) -> ApplyOperationResult<Value> {
    executor::apply_operation_classified(app, operation).await
}

fn all_files(root: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    fn collect(root: &Path, directory: &Path, values: &mut Vec<(PathBuf, Vec<u8>)>) {
        if let Ok(entries) = fs::read_dir(directory) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    collect(root, &path, values);
                } else if path.is_file() {
                    values.push((
                        path.strip_prefix(root).unwrap().to_path_buf(),
                        fs::read(path).unwrap(),
                    ));
                }
            }
        }
    }
    let mut values = Vec::new();
    collect(root, root, &mut values);
    values.sort();
    values
}

#[test]
fn create_once_strict_input_preserves_explicit_order_and_rejects_old_flags() {
    let (_, _, payload) = mcp_input(&arguments()).unwrap();
    assert_eq!(payload["order"], 10);
    assert_eq!(payload["parentId"], Value::Null);
    let mut strict = arguments();
    strict["requireDurable"] = json!(true);
    assert!(mcp_input(&strict).is_ok());
    strict["awaitDurable"] = json!(false);
    assert!(mcp_input(&strict).is_err());
    for key in [
        "expectedRevision",
        "operationId",
        "noRecreate",
        "content",
        "graphId",
        "documentId",
    ] {
        let mut input = arguments();
        input[key] = json!(0);
        assert!(mcp_input(&input).is_err(), "accepted unknown field {key}");
    }
    for (key, value) in [
        ("order", json!(null)),
        ("parentId", json!("a-folder")),
        ("title", json!(" padded ")),
        ("awaitDurable", json!("true")),
        ("requireDurable", json!("true")),
        ("tiptapJson", json!({"type":"doc","content":[]})),
    ] {
        let mut input = arguments();
        input[key] = value;
        assert!(mcp_input(&input).is_err(), "accepted malformed {key}");
    }
}

#[cfg(feature = "headless")]
#[test]
fn create_once_strict_durability_without_plane_keeps_admission_and_refuses_replay() {
    fixture(|app, graph_dir| {
        crate::app_runtime::async_runtime::block_on(async {
            let mut input = arguments();
            input["requireDurable"] = json!(true);
            let error = crate::document_create_once_mcp::create_document_once(app.clone(), &input)
                .await
                .unwrap_err();
            assert!(error.contains("durable plane is not configured"), "{error}");
            assert!(admission_path(&graph_dir, DOCUMENT).unwrap().is_file());
            let retry = crate::document_create_once_mcp::create_document_once(app, &input)
                .await
                .unwrap_err();
            assert!(retry.contains("prior admission"), "{retry}");
        })
    });
}

#[test]
fn create_once_managed_mcp_success_then_same_template_refuses_without_changes() {
    fixture(|app, graph_dir| {
        crate::app_runtime::async_runtime::block_on(async {
            let response =
                crate::document_create_once_mcp::create_document_once(app.clone(), &arguments())
                    .await
                    .unwrap();
            assert_eq!(response["success"], true);
            assert_eq!(response["outcome"], "created");
            assert_eq!(response["order"], 10);
            assert_eq!(response["parentId"], Value::Null);
            assert_eq!(response["revision"], 1);
            assert!(response["durability"].is_string());
            let admission = fs::read(admission_path(&graph_dir, DOCUMENT).unwrap()).unwrap();
            let record =
                crate::document_service::read_document(app.clone(), GRAPH.into(), DOCUMENT.into())
                    .unwrap();
            assert_eq!(record.body, "Original starter text");
            let snapshot: Value =
                crate::storage::read_json(&crate::ydoc_paths::workspace_snapshot_path(&graph_dir))
                    .unwrap();
            assert_eq!(snapshot["documents"][0]["order"].as_f64(), Some(10.0));
            let before = all_files(&graph_dir.join("documents"));
            let sidecar = fs::read(crate::ydoc_paths::document_ydoc_state_path(
                &graph_dir, DOCUMENT,
            ))
            .unwrap();
            let error =
                crate::document_create_once_mcp::create_document_once(app.clone(), &arguments())
                    .await
                    .unwrap_err();
            assert!(error.contains("prior admission"), "{error}");
            assert_eq!(before, all_files(&graph_dir.join("documents")));
            assert_eq!(
                sidecar,
                fs::read(crate::ydoc_paths::document_ydoc_state_path(
                    &graph_dir, DOCUMENT
                ))
                .unwrap()
            );
            assert_eq!(
                admission,
                fs::read(admission_path(&graph_dir, DOCUMENT).unwrap()).unwrap()
            );
            assert!(
                crate::crdt_operation_journal::recover_pending_crdt_operations(&app)
                    .unwrap()
                    .is_empty()
            );
        })
    });
}

#[test]
fn create_once_nine_root_pages_preserve_order_through_fresh_room_registry() {
    fixture(|app, graph_dir| {
        crate::app_runtime::async_runtime::block_on(async {
            for index in 1..=9 {
                let mut input = arguments();
                input["document_id"] = json!(format!("starter-page-{index}"));
                input["order"] = json!(index * 10);
                input["tiptapJson"]["content"][0]["type"] = json!("heading");
                input["tiptapJson"]["content"][0]["attrs"]["level"] = json!(2);
                crate::document_create_once_mcp::create_document_once(app.clone(), &input)
                    .await
                    .unwrap();
                app.state::<RoomRegistry>().evict_graph(GRAPH);
            }
            let snapshot: Value =
                crate::storage::read_json(&crate::ydoc_paths::workspace_snapshot_path(&graph_dir))
                    .unwrap();
            let documents = snapshot["documents"].as_array().unwrap();
            assert_eq!(documents.len(), 9);
            for index in 1..=9 {
                let document = documents
                    .iter()
                    .find(|d| d["id"] == format!("starter-page-{index}"))
                    .unwrap();
                assert_eq!(document["order"].as_f64(), Some((index * 10) as f64));
            }
        })
    });
}

#[test]
fn create_once_refuses_prior_revision_zero_deleted_partial_and_unreadable_authority() {
    for mode in [
        "revision-zero",
        "deleted",
        "interrupted-delete",
        "partial-document-directory",
        "sidecar",
        "malformed-workspace-sidecar",
        "room",
        "workspace-hot",
        "workspace-sidecar",
        "workspace-snapshot",
        "malformed-workspace",
        "malformed-fence",
    ] {
        fixture(|app, graph_dir| {
            crate::app_runtime::async_runtime::block_on(async {
                let op = operation(&app, mode);
                match mode {
                    "revision-zero" | "deleted" | "interrupted-delete" => {
                        crate::document_service::create_document(
                            app.clone(),
                            crate::document_types::CreateDocumentInput {
                                graph_id: GRAPH.into(),
                                document_id: Some(DOCUMENT.into()),
                                title: "Prior author".into(),
                            },
                        )
                        .unwrap();
                        if mode == "deleted" {
                            crate::document_delete_service::delete_document(
                                app.clone(),
                                GRAPH.into(),
                                DOCUMENT.into(),
                            )
                            .unwrap();
                        } else if mode == "interrupted-delete" {
                            crate::document_tombstone_store::write_document_tombstone_for_operation(&graph_dir,DOCUMENT,Some("prior-delete")).unwrap();
                        }
                    }
                    "sidecar" => {
                        let directory = crate::ydoc_paths::document_ydoc_dir(&graph_dir, DOCUMENT);
                        fs::create_dir_all(&directory).unwrap();
                        crate::storage::write_bytes(&directory.join("update-v1.bin"), b"partial")
                            .unwrap();
                    }
                    "partial-document-directory" => {
                        fs::create_dir_all(
                            crate::document_paths::document_dir(&graph_dir, DOCUMENT).unwrap(),
                        )
                        .unwrap();
                    }
                    "malformed-workspace-sidecar" => {
                        crate::storage::write_bytes(
                            &crate::ydoc_paths::workspace_ydoc_state_path(&graph_dir),
                            b"not a CRDT update",
                        )
                        .unwrap();
                    }
                    "room" => {
                        app.state::<RoomRegistry>()
                            .get_or_create(
                                &format!("doc:{GRAPH}:{DOCUMENT}"),
                                crate::ydoc_paths::document_ydoc_state_path(&graph_dir, DOCUMENT),
                            )
                            .await
                            .unwrap();
                    }
                    "workspace-hot" | "workspace-sidecar" => {
                        let registry = app.state::<RoomRegistry>();
                        let room = registry
                            .get_or_create(
                                &format!("workspace:{GRAPH}"),
                                crate::ydoc_paths::workspace_ydoc_state_path(&graph_dir),
                            )
                            .await
                            .unwrap();
                        room.update_doc(|_, txn| {
                            txn.get_or_insert_map("documents").insert(
                                txn,
                                DOCUMENT,
                                MapPrelim::default(),
                            );
                            Ok(())
                        })
                        .await
                        .unwrap();
                        if mode == "workspace-sidecar" {
                            registry.evict_graph(GRAPH);
                        }
                    }
                    "workspace-snapshot" | "malformed-workspace" => {
                        let path = crate::ydoc_paths::workspace_snapshot_path(&graph_dir);
                        crate::storage::write_json(
                            &path,
                            &if mode == "workspace-snapshot" {
                                json!({"schemaVersion":1,"graphId":GRAPH,"documents":[{"id":DOCUMENT}]})
                            } else {
                                json!({"schemaVersion":1,"graphId":GRAPH,"documents":"invalid"})
                            },
                        )
                        .unwrap();
                    }
                    "malformed-fence" => {
                        let path = admission_path(&graph_dir, DOCUMENT).unwrap();
                        fs::create_dir_all(path.parent().unwrap()).unwrap();
                        crate::storage::write_bytes(&path, b"{").unwrap();
                    }
                    _ => unreachable!(),
                }
                let before_documents = all_files(&graph_dir.join("documents"));
                let before_ydocs = all_files(&graph_dir.join("ydocs"));
                let error = execute(&app, &op).await.unwrap_err();
                assert!(
                    matches!(error, ApplyOperationError::Terminal(_)),
                    "{mode}: {error}"
                );
                assert!(
                    error.message().starts_with("create_document_once refused:"),
                    "{mode}: {error}"
                );
                assert_eq!(
                    before_documents,
                    all_files(&graph_dir.join("documents")),
                    "{mode}"
                );
                assert_eq!(before_ydocs, all_files(&graph_dir.join("ydocs")), "{mode}");
                if mode != "malformed-fence" {
                    assert!(
                        !admission_path(&graph_dir, DOCUMENT).unwrap().exists(),
                        "{mode}"
                    );
                }
            })
        });
    }
}

#[test]
fn create_once_partial_failures_are_terminal_and_recovered_operation_never_retries_content() {
    for (name, action) in [
        ("admission", TestAction::AdmissionFailure),
        ("hot", TestAction::HotFailure),
        ("document-tail", TestAction::DocumentTailFailure),
        ("workspace-tail", TestAction::WorkspaceTailFailure),
    ] {
        fixture(|app, graph_dir| {
            crate::app_runtime::async_runtime::block_on(async {
                let op = operation(&app, name);
                *TEST_ACTION.lock().unwrap() = Some((name.into(), action));
                crate::crdt_operation_journal::record_crdt_operation_queued(&app, &op).unwrap();
                let error = execute(&app, &op).await.unwrap_err();
                assert!(
                    matches!(error, ApplyOperationError::Terminal(_)),
                    "{name}: {error}"
                );
                assert!(error.message().contains("uncertain"), "{name}: {error}");
                assert!(admission_path(&graph_dir, DOCUMENT).unwrap().is_file());
                let documents = all_files(&graph_dir.join("documents"));
                let ydocs = all_files(&graph_dir.join("ydocs"));
                if name == "admission" {
                    assert!(!crate::document_paths::document_dir(&graph_dir, DOCUMENT)
                        .unwrap()
                        .exists());
                }
                if name == "document-tail" {
                    let raw: Value = crate::storage::read_json(
                        &crate::document_paths::document_dir(&graph_dir, DOCUMENT)
                            .unwrap()
                            .join("document.json"),
                    )
                    .unwrap();
                    assert_eq!(raw["revision"], 1);
                }
                // Simulate crash before terminal journaling: use a new managed app,
                // recover the original journal and exercise the real worker.
                app.state::<RoomRegistry>().evict_graph(GRAPH);
                let replacement = crate::tauri_runtime::build_mock_app_for_tests(true);
                assert_eq!(
                    crate::crdt_queue::recover_crdt_operations(replacement.clone()).unwrap(),
                    1
                );
                executor::drain_queue(replacement.clone()).await;
                assert!(
                    crate::crdt_operation_journal::recover_pending_crdt_operations(&replacement)
                        .unwrap()
                        .is_empty()
                );
                assert_eq!(documents, all_files(&graph_dir.join("documents")), "{name}");
                assert_eq!(ydocs, all_files(&graph_dir.join("ydocs")), "{name}");
            })
        });
    }
}

#[test]
fn create_once_fence_survives_ordinary_delete_and_recreation() {
    fixture(|app, graph_dir| {
        crate::app_runtime::async_runtime::block_on(async {
            let op = operation(&app, "first");
            execute(&app, &op).await.unwrap();
            let fence = fs::read(admission_path(&graph_dir, DOCUMENT).unwrap()).unwrap();
            crate::document_delete_service::delete_document(
                app.clone(),
                GRAPH.into(),
                DOCUMENT.into(),
            )
            .unwrap();
            assert!(execute(&app, &operation(&app, "after-delete"))
                .await
                .unwrap_err()
                .message()
                .contains("prior admission"));
            let mut ordinary = operation(&app, "ordinary-recreate");
            ordinary.kind = "document.write".into();
            ordinary.payload["expectedRevision"] = json!(0);
            ordinary.payload["tiptapJson"]["content"][0]["content"][0]["text"] =
                json!("User recreation");
            execute(&app, &ordinary).await.unwrap();
            assert_eq!(
                crate::document_service::read_document(app.clone(), GRAPH.into(), DOCUMENT.into())
                    .unwrap()
                    .body,
                "User recreation"
            );
            assert_eq!(
                fence,
                fs::read(admission_path(&graph_dir, DOCUMENT).unwrap()).unwrap()
            );
            assert!(execute(&app, &operation(&app, "after-recreate"))
                .await
                .is_err());
        })
    });
}

#[test]
fn create_once_execution_lease_excludes_authoring_between_admission_and_write() {
    fixture(|app, graph_dir| {
        crate::app_runtime::async_runtime::block_on(async {
            let op = operation(&app, "paused");
            let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
            let resume = Arc::new(tokio::sync::Notify::new());
            *TEST_ACTION.lock().unwrap() = Some((
                "paused".into(),
                TestAction::Pause {
                    entered: entered_tx,
                    resume: resume.clone(),
                },
            ));
            let writing_app = app.clone();
            let writer = tokio::spawn(async move { execute(&writing_app, &op).await });
            entered_rx.await.unwrap();
            assert!(admission_path(&graph_dir, DOCUMENT).unwrap().exists());
            let (author_entered_tx, mut author_entered_rx) = tokio::sync::oneshot::channel();
            let author_app = app.clone();
            let author_dir = graph_dir.clone();
            let author = tokio::spawn(async move {
                // Same root gate used by inbound WebSocket mutations. The queue
                // mutex is not the exclusion mechanism being exercised here.
                let coordinator = author_app.state::<GraphPersistenceCoordinator>();
                let _lease = coordinator.acquire_hot_write(GRAPH).await.unwrap();
                author_entered_tx.send(()).unwrap();
                assert!(crate::document_paths::document_dir(&author_dir, DOCUMENT)
                    .unwrap()
                    .join("document.json")
                    .exists());
                let room = author_app
                    .state::<RoomRegistry>()
                    .existing_room(&format!("doc:{GRAPH}:{DOCUMENT}"))
                    .unwrap()
                    .unwrap();
                room.update_doc(|_, txn| {
                    let fragment = txn.get_or_insert_xml_fragment("content");
                    fragment.remove_range(txn, 0, fragment.len(txn));
                    super::super::builder::append_nodes(
                        txn,
                        &fragment,
                        &[json!({
                            "type":"paragraph", "attrs":{"data-block-id":"starter-block"},
                            "content":[{"type":"text","text":"Later author edit"}]
                        })],
                    );
                    Ok(())
                })
                .await
                .unwrap();
            });
            tokio::task::yield_now().await;
            assert!(tokio::time::timeout(
                std::time::Duration::from_millis(30),
                &mut author_entered_rx
            )
            .await
            .is_err());
            resume.notify_one();
            writer.await.unwrap().unwrap();
            author.await.unwrap();
            author_entered_rx.await.unwrap();
            let room = app
                .state::<RoomRegistry>()
                .existing_room(&format!("doc:{GRAPH}:{DOCUMENT}"))
                .unwrap()
                .unwrap();
            assert_eq!(
                room.with_doc(|doc| super::super::projection::materialize_ydoc(doc, DOCUMENT).body)
                    .await,
                "Later author edit"
            );
            assert!(execute(&app, &operation(&app, "later")).await.is_err());
        })
    });
}

#[test]
fn create_once_waiting_first_write_refuses_when_prior_author_wins_the_execution_lease() {
    fixture(|app, graph_dir| {
        crate::app_runtime::async_runtime::block_on(async {
            let coordinator = app.state::<GraphPersistenceCoordinator>();
            let author_lease = coordinator.acquire_hot_write(GRAPH).await.unwrap();
            let op = operation(&app, "queued-behind-author");
            let pending_app = app.clone();
            let mut pending = tokio::spawn(async move { execute(&pending_app, &op).await });
            assert!(
                tokio::time::timeout(std::time::Duration::from_millis(30), &mut pending)
                    .await
                    .is_err()
            );
            crate::document_service::create_document_with_lease(
                app.clone(),
                crate::document_types::CreateDocumentInput {
                    graph_id: GRAPH.into(),
                    document_id: Some(DOCUMENT.into()),
                    title: "Prior author wins".into(),
                },
            )
            .unwrap();
            drop(author_lease);
            let error = pending.await.unwrap().unwrap_err();
            assert!(error.message().contains("document authority"), "{error}");
            assert!(!admission_path(&graph_dir, DOCUMENT).unwrap().exists());
            assert_eq!(
                crate::document_service::read_document(app.clone(), GRAPH.into(), DOCUMENT.into())
                    .unwrap()
                    .title,
                "Prior author wins"
            );
        })
    });
}
