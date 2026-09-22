use super::{
    builder,
    document_ops::{
        document_write, document_write_classified, materialize_room_document,
        persist_materialized_room_document, persist_room_document,
        reconcile_matching_document_projection,
    },
    executor::ApplyOperationError,
    rooms::{run_projection_flush_for_test, Room, RoomRegistry},
    workspace_ops::{
        materialize_workspace, persist_materialized_workspace, persist_workspace,
        update_workspace_document, write_workspace_document, write_workspace_folder,
    },
};
use crate::{
    crdt_queue::{CrdtOperation, EnqueueCrdtOperationInput},
    document_paths::document_dir,
    document_record_store::read_document_record,
    graph_paths::existing_graph_dir,
    graph_service::{create_graph_service, CreateGraphInput},
    ydoc_paths::document_ydoc_state_path,
};
use serde_json::{json, Value};
use std::{panic::UnwindSafe, path::PathBuf};
#[cfg(feature = "desktop")]
use tauri::Manager;
use uuid::Uuid;
use yrs::{Map as YMap, MapPrelim, Out, ReadTxn, Transact, WriteTxn, XmlFragment};

fn with_profile(prefix: &str, test: impl FnOnce() + UnwindSafe) {
    let _serial = crate::tauri_runtime::profile_env_serial()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let profile: PathBuf = std::env::temp_dir().join(format!("{prefix}-{}", Uuid::new_v4()));
    std::env::set_var("GARDEN_PROFILE_DIR", &profile);
    let result = std::panic::catch_unwind(test);
    std::env::remove_var("GARDEN_PROFILE_DIR");
    let _ = std::fs::remove_dir_all(&profile);
    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}

async fn replace_room_text(room: &Room, block_id: &str, text: &str) {
    let nodes = vec![json!({
        "type": "paragraph",
        "attrs": { "data-block-id": block_id },
        "content": [{ "type": "text", "text": text }],
    })];
    room.update_doc(move |_doc, txn| {
        let fragment = txn.get_or_insert_xml_fragment("content");
        let len = fragment.len(txn);
        if len > 0 {
            fragment.remove_range(txn, 0, len);
        }
        builder::append_nodes(txn, &fragment, &nodes);
        Ok(())
    })
    .await
    .expect("replace room text");
}

fn current_graph_incarnation(app: &crate::app_runtime::AppHandle, graph_id: &str) -> String {
    crate::graph_record_store::read_graph_record_no_heal(app, graph_id)
        .expect("current graph record")
        .1
        .incarnation_id
        .expect("graph incarnation")
}

fn journal_and_recover_one(
    app: &crate::app_runtime::AppHandle,
    operation: &CrdtOperation,
) -> CrdtOperation {
    crate::crdt_operation_journal::record_crdt_operation_queued(app, operation)
        .expect("journal pending operation");
    recover_one_pending(app)
}

fn recover_one_pending(app: &crate::app_runtime::AppHandle) -> CrdtOperation {
    assert_eq!(
        crate::crdt_queue::recover_crdt_operations(app.clone()).expect("recover operation"),
        1
    );
    let operations = app
        .state::<crate::crdt_queue::CrdtOperationQueue>()
        .poll(Some(8))
        .expect("poll recovered operation");
    assert_eq!(operations.len(), 1);
    let recovered = operations.into_iter().next().expect("recovered operation");
    assert_eq!(
        recovered
            .payload
            .get(crate::crdt_queue::RECOVERED_OPERATION_PAYLOAD_KEY),
        Some(&Value::Bool(true))
    );
    recovered
}

#[test]
fn legacy_graph_incarnation_is_backfilled_without_touching_content_revision() {
    with_profile("garden-incarnation-backfill", || {
        crate::app_runtime::async_runtime::block_on(async {
            let app = crate::tauri_runtime::build_mock_app_for_tests(true);
            let graph_id = "legacy-incarnation";
            create_graph_service(
                &app,
                CreateGraphInput {
                    title: "Legacy Incarnation".to_string(),
                    graph_id: Some(graph_id.to_string()),
                    description: None,
                    operation_id: None,
                },
            )
            .expect("create graph");
            let (graph_dir, mut legacy) =
                crate::graph_record_store::read_graph_record_no_heal(&app, graph_id)
                    .expect("new graph");
            legacy.incarnation_id = None;
            let updated_at = legacy.updated_at.clone();
            let content_revision = legacy.content_revision.clone();
            crate::graph_record_store::write_graph_record(&graph_dir, &legacy)
                .expect("write legacy record");

            let coordinator =
                app.state::<super::persistence_coordinator::GraphPersistenceCoordinator>();
            let _lease = coordinator
                .acquire_hot_write(graph_id)
                .await
                .expect("graph lease");
            let incarnation = crate::graph_record_store::ensure_graph_incarnation(&app, graph_id)
                .expect("backfill incarnation");
            Uuid::parse_str(&incarnation).expect("incarnation is a UUID");
            let (_, stored) = crate::graph_record_store::read_graph_record_no_heal(&app, graph_id)
                .expect("backfilled graph");
            assert_eq!(stored.incarnation_id.as_deref(), Some(incarnation.as_str()));
            assert_eq!(stored.updated_at, updated_at);
            assert_eq!(stored.content_revision, content_revision);
        });
    });
}

#[test]
fn older_document_materialization_cannot_clear_newer_projection() {
    with_profile("garden-document-projection-overlap", || {
        crate::app_runtime::async_runtime::block_on(async {
            let app = crate::tauri_runtime::build_mock_app_for_tests(true);
            let graph_id = "projection-overlap";
            let document_id = "document-overlap";
            create_graph_service(
                &app,
                CreateGraphInput {
                    title: "Projection Overlap".to_string(),
                    graph_id: Some(graph_id.to_string()),
                    description: None,
                    operation_id: None,
                },
            )
            .expect("create graph");
            let graph_dir = existing_graph_dir(&app, graph_id).expect("graph dir");
            let room = app
                .state::<RoomRegistry>()
                .get_or_create(
                    &format!("doc:{graph_id}:{document_id}"),
                    document_ydoc_state_path(&graph_dir, document_id),
                )
                .await
                .expect("document room");

            replace_room_text(&room, "old-block", "old projection").await;
            let old_guard = room.lock_projection_flush().await;
            let old = materialize_room_document(
                graph_id,
                document_id,
                "Overlap Document",
                &room,
                "old-flush",
                None,
                None,
            )
            .await
            .expect("old materialization");
            replace_room_text(&room, "new-block", "new projection").await;

            let (started_tx, started_rx) = tokio::sync::oneshot::channel();
            let app_for_new = app.clone();
            let room_for_new = room.clone();
            let newer = crate::app_runtime::async_runtime::spawn(async move {
                let _ = started_tx.send(());
                persist_room_document(
                    &app_for_new,
                    graph_id,
                    document_id,
                    "Overlap Document",
                    &room_for_new,
                    "new-flush",
                )
                .await
            });
            started_rx.await.expect("new persistence started");
            let old_value =
                persist_materialized_room_document(&app, graph_id, document_id, &room, old)
                    .expect("persist old snapshot");
            assert_eq!(old_value["revision"], 1);
            assert!(room.needs_projection_flush());
            drop(old_guard);

            let new_value = newer
                .await
                .expect("new persistence task")
                .expect("persist new snapshot");
            assert_eq!(new_value["revision"], 2);
            assert!(!room.needs_projection_flush());
            let record = crate::document_service::read_document(
                app.clone(),
                graph_id.to_string(),
                document_id.to_string(),
            )
            .expect("final document");
            assert_eq!(record.body, "new projection");
            assert_eq!(record.blocks[0].id, "new-block");
            assert_eq!(
                crate::document_history_file_store::read_document_history_store(
                    &graph_dir,
                    graph_id,
                    document_id,
                )
                .expect("history")
                .total_count,
                2
            );
        });
    });
}

#[test]
fn fresh_registry_hydration_does_not_create_a_document_revision() {
    with_profile("garden-document-restart-idempotency", || {
        crate::app_runtime::async_runtime::block_on(async {
            let app = crate::tauri_runtime::build_mock_app_for_tests(true);
            let graph_id = "restart-idempotency";
            let document_id = "restart-document";
            create_graph_service(
                &app,
                CreateGraphInput {
                    title: "Restart Idempotency".to_string(),
                    graph_id: Some(graph_id.to_string()),
                    description: None,
                    operation_id: None,
                },
            )
            .expect("create graph");

            for (operation_id, text) in [("seed-restart-1", "first"), ("seed-restart-2", "second")]
            {
                document_write(
                    &app,
                    &CrdtOperation {
                        operation_id: operation_id.to_string(),
                        kind: "document.write".to_string(),
                        graph_id: graph_id.to_string(),
                        document_id: Some(document_id.to_string()),
                        payload: json!({
                            "title": "Restart Document",
                            // Omit data-block-id and include a mark so both
                            // fallback block ids and mark ids differ between
                            // materializations despite identical TipTap state.
                            "tiptapJson": {
                                "type": "doc",
                                "content": [{
                                    "type": "paragraph",
                                    "content": [{
                                        "type": "text",
                                        "text": text,
                                        "marks": [{ "type": "bold" }]
                                    }]
                                }]
                            }
                        }),
                        enqueue_timestamp: "1".to_string(),
                    },
                )
                .await
                .expect("seed document revision");
            }

            let graph_dir = existing_graph_dir(&app, graph_id).expect("graph dir");
            let manifest = document_dir(&graph_dir, document_id)
                .expect("document dir")
                .join("document.json");
            let record_before = std::fs::read(&manifest).expect("document record before restart");
            let history_before = crate::document_history_file_store::read_document_history_store(
                &graph_dir,
                graph_id,
                document_id,
            )
            .expect("history before restart")
            .total_count;
            let content_revision_before =
                crate::graph_record_store::read_graph_record_no_heal(&app, graph_id)
                    .expect("graph before restart")
                    .1
                    .content_revision;
            assert_eq!(
                read_document_record(&graph_dir, &manifest)
                    .expect("record before restart")
                    .revision,
                2
            );

            // Simulate a process restart, not a second lookup in the same room:
            // a brand-new registry hydrates a brand-new Doc only from the
            // persisted update-v1.bin sidecar.
            let restarted_registry = RoomRegistry::default();
            let restarted_room = restarted_registry
                .get_or_create(
                    &format!("doc:{graph_id}:{document_id}"),
                    document_ydoc_state_path(&graph_dir, document_id),
                )
                .await
                .expect("hydrate restarted room");
            assert!(restarted_room.needs_projection_flush());
            let replayed = persist_room_document(
                &app,
                graph_id,
                document_id,
                "Restart Document",
                &restarted_room,
                "restart-hydration-flush",
            )
            .await
            .expect("reconcile restarted projection");

            assert_eq!(replayed["revision"], 2);
            assert!(!restarted_room.needs_projection_flush());
            assert_eq!(
                std::fs::read(&manifest).expect("document record after restart"),
                record_before,
                "hydrating an unchanged sidecar must not rewrite document.json"
            );
            assert_eq!(
                crate::document_history_file_store::read_document_history_store(
                    &graph_dir,
                    graph_id,
                    document_id,
                )
                .expect("history after restart")
                .total_count,
                history_before,
                "hydrating an unchanged sidecar must not capture history"
            );
            assert_eq!(
                crate::graph_record_store::read_graph_record_no_heal(&app, graph_id)
                    .expect("graph after restart")
                    .1
                    .content_revision,
                content_revision_before,
                "hydrating an unchanged sidecar must not bump graph content revision"
            );
        });
    });
}

#[test]
fn failed_history_tail_is_required_and_repaired_once_by_semantic_replay() {
    with_profile("garden-document-history-tail-repair", || {
        crate::app_runtime::async_runtime::block_on(async {
            let app = crate::tauri_runtime::build_mock_app_for_tests(true);
            let graph_id = "document-history-tail-repair";
            let document_id = "history-tail-document";
            create_graph_service(
                &app,
                CreateGraphInput {
                    title: "Document History Tail Repair".to_string(),
                    graph_id: Some(graph_id.to_string()),
                    description: None,
                    operation_id: None,
                },
            )
            .expect("create graph");
            let graph_dir = existing_graph_dir(&app, graph_id).expect("graph dir");
            let room = app
                .state::<RoomRegistry>()
                .get_or_create(
                    &format!("doc:{graph_id}:{document_id}"),
                    document_ydoc_state_path(&graph_dir, document_id),
                )
                .await
                .expect("document room");
            replace_room_text(&room, "history-tail-block", "history tail repair").await;

            let cold_document_dir = document_dir(&graph_dir, document_id).expect("document dir");
            std::fs::create_dir_all(&cold_document_dir).expect("create cold document dir");
            let history_blocker = cold_document_dir.join("history");
            std::fs::write(&history_blocker, b"not a directory")
                .expect("block history directory creation");

            let first_error = persist_room_document(
                &app,
                graph_id,
                document_id,
                "History Tail Document",
                &room,
                "history-tail-first-attempt",
            )
            .await
            .expect_err("history failure must fail the document tail");
            assert!(
                first_error.contains("history")
                    || first_error.contains("directory")
                    || first_error.contains("Not a directory"),
                "{first_error}"
            );
            assert!(room.needs_projection_flush());
            let manifest = cold_document_dir.join("document.json");
            let written = read_document_record(&graph_dir, &manifest)
                .expect("document record landed before history failure");
            assert_eq!(written.revision, 1);
            assert_eq!(written.body, "history tail repair");
            assert_eq!(
                crate::document_history_file_store::read_document_history_store(
                    &graph_dir,
                    graph_id,
                    document_id,
                )
                .expect("empty failed history")
                .total_count,
                0
            );
            assert!(
                crate::graph_record_store::read_graph_record_no_heal(&app, graph_id)
                    .expect("graph before repaired touch")
                    .1
                    .content_revision
                    .is_none(),
                "graph touch must remain an unfinished tail after history failure"
            );

            std::fs::remove_file(&history_blocker).expect("unblock history directory");
            let restarted_registry = RoomRegistry::default();
            let restarted_room = restarted_registry
                .get_or_create(
                    &format!("doc:{graph_id}:{document_id}"),
                    document_ydoc_state_path(&graph_dir, document_id),
                )
                .await
                .expect("restart room from durable sidecar");
            let repaired = persist_room_document(
                &app,
                graph_id,
                document_id,
                "History Tail Document",
                &restarted_room,
                "history-tail-replay",
            )
            .await
            .expect("semantic replay repairs history and graph touch");
            assert_eq!(repaired["revision"], 1);
            assert!(!restarted_room.needs_projection_flush());
            let history = crate::document_history_file_store::read_document_history_store(
                &graph_dir,
                graph_id,
                document_id,
            )
            .expect("repaired history");
            assert_eq!(history.total_count, 1);
            assert_eq!(history.latest_automatic_revision, Some(1));
            let snapshot_id = history
                .latest_automatic_snapshot_id
                .clone()
                .expect("exact automatic snapshot id");
            let snapshot = history
                .snapshots
                .iter()
                .find(|snapshot| snapshot.snapshot_id == snapshot_id)
                .expect("automatic snapshot metadata");
            assert_eq!(snapshot.document_revision, Some(1));
            let commit = crate::document_history_file_store::read_document_tail_commit(
                &graph_dir,
                document_id,
            )
            .expect("read tail commit")
            .expect("tail commit after repair");
            assert_eq!(commit.document_revision, 1);
            assert_eq!(commit.snapshot_id, snapshot_id);
            let history_before_idempotent_replay = std::fs::read(
                crate::document_history_store::document_history_store_path(&graph_dir, document_id),
            )
            .expect("history before exact replay");
            let marker_before_idempotent_replay = std::fs::read(
                crate::document_history_store::document_tail_commit_path(&graph_dir, document_id),
            )
            .expect("marker before exact replay");
            let content_revision_before_idempotent_replay =
                crate::graph_record_store::read_graph_record_no_heal(&app, graph_id)
                    .expect("graph before exact replay")
                    .1
                    .content_revision;

            // Another process hydration represents crash-after-history but
            // before operation completion. The same revision marker must make
            // that replay a no-op for history.
            let second_registry = RoomRegistry::default();
            let second_room = second_registry
                .get_or_create(
                    &format!("doc:{graph_id}:{document_id}"),
                    document_ydoc_state_path(&graph_dir, document_id),
                )
                .await
                .expect("second restart room");
            persist_room_document(
                &app,
                graph_id,
                document_id,
                "History Tail Document",
                &second_room,
                "history-tail-second-replay",
            )
            .await
            .expect("second semantic replay");
            let unchanged = crate::document_history_file_store::read_document_history_store(
                &graph_dir,
                graph_id,
                document_id,
            )
            .expect("idempotent history");
            assert_eq!(unchanged.total_count, 1);
            assert_eq!(unchanged.latest_automatic_revision, Some(1));
            assert_eq!(
                std::fs::read(crate::document_history_store::document_history_store_path(
                    &graph_dir,
                    document_id,
                ))
                .expect("history after exact replay"),
                history_before_idempotent_replay,
                "a valid exact tail marker must not rewrite history"
            );
            assert_eq!(
                std::fs::read(crate::document_history_store::document_tail_commit_path(
                    &graph_dir,
                    document_id,
                ))
                .expect("marker after exact replay"),
                marker_before_idempotent_replay,
                "a valid exact tail marker must remain byte-stable"
            );
            assert_eq!(
                crate::graph_record_store::read_graph_record_no_heal(&app, graph_id)
                    .expect("graph after exact replay")
                    .1
                    .content_revision,
                content_revision_before_idempotent_replay,
                "a valid exact tail marker must not touch graph content revision"
            );

            // A marker is never trusted after its referenced payload is
            // damaged. A later replay repairs history under the SAME document
            // revision and replaces the final marker.
            std::fs::write(
                crate::document_history_store::document_snapshot_payload_path(
                    &graph_dir,
                    document_id,
                    &snapshot_id,
                ),
                b"{corrupt",
            )
            .expect("corrupt committed payload");
            let repair_registry = RoomRegistry::default();
            let repair_room = repair_registry
                .get_or_create(
                    &format!("doc:{graph_id}:{document_id}"),
                    document_ydoc_state_path(&graph_dir, document_id),
                )
                .await
                .expect("payload repair room");
            let repaired_damage = persist_room_document(
                &app,
                graph_id,
                document_id,
                "History Tail Document",
                &repair_room,
                "history-tail-damaged-payload-replay",
            )
            .await
            .expect("damaged committed payload is repaired");
            assert_eq!(repaired_damage["revision"], 1);
            let repaired_history = crate::document_history_file_store::read_document_history_store(
                &graph_dir,
                graph_id,
                document_id,
            )
            .expect("history after payload repair");
            assert_eq!(repaired_history.total_count, 1);
            assert!(repaired_history
                .snapshots
                .iter()
                .all(|snapshot| snapshot.snapshot_id != snapshot_id));
            let repaired_commit = crate::document_history_file_store::read_document_tail_commit(
                &graph_dir,
                document_id,
            )
            .expect("read repaired marker")
            .expect("repaired marker");
            assert_eq!(repaired_commit.document_revision, 1);
            assert_ne!(repaired_commit.snapshot_id, snapshot_id);
        });
    });
}

#[test]
fn document_tail_failure_is_retryable_after_hot_commit_and_replays_same_revision() {
    with_profile("garden-document-classified-tail-retry", || {
        crate::app_runtime::async_runtime::block_on(async {
            let app = crate::tauri_runtime::build_mock_app_for_tests(true);
            let graph_id = "document-classified-tail-retry";
            let document_id = "classified-tail-document";
            create_graph_service(
                &app,
                CreateGraphInput {
                    title: "Document Classified Tail Retry".to_string(),
                    graph_id: Some(graph_id.to_string()),
                    description: None,
                    operation_id: None,
                },
            )
            .expect("create graph");
            let graph_dir = existing_graph_dir(&app, graph_id).expect("graph dir");
            let cold_document_dir = document_dir(&graph_dir, document_id).expect("document dir");
            std::fs::create_dir_all(&cold_document_dir).expect("cold document dir");
            let history_blocker = cold_document_dir.join("history");
            std::fs::write(&history_blocker, b"not a directory")
                .expect("block history directory creation");
            let operation = CrdtOperation {
                operation_id: "classified-tail-operation".to_string(),
                kind: "document.write".to_string(),
                graph_id: graph_id.to_string(),
                document_id: Some(document_id.to_string()),
                payload: json!({
                    "title": "Classified Tail Document",
                    "expectedRevision": 0,
                    "tiptapJson": {
                        "type": "doc",
                        "content": [{
                            "type": "paragraph",
                            "attrs": { "data-block-id": "classified-tail-block" },
                            "content": [{ "type": "text", "text": "durable hot state" }]
                        }]
                    }
                }),
                enqueue_timestamp: "1".to_string(),
            };

            let error = document_write_classified(&app, &operation)
                .await
                .expect_err("history failure after room authority is retryable");
            assert!(matches!(
                error,
                ApplyOperationError::RetryableAfterHotCommit(_)
            ));
            let manifest = cold_document_dir.join("document.json");
            assert!(document_ydoc_state_path(&graph_dir, document_id).is_file());
            assert_eq!(
                read_document_record(&graph_dir, &manifest)
                    .expect("cold record committed before tail failure")
                    .revision,
                1
            );

            std::fs::remove_file(&history_blocker).expect("unblock history");
            let result = document_write_classified(&app, &operation)
                .await
                .expect("retry repairs tail under original operation");
            assert_eq!(result["documentId"], document_id);
            assert_eq!(
                read_document_record(&graph_dir, &manifest)
                    .expect("record after retry")
                    .revision,
                1,
                "tail retry must not invent a second document revision"
            );
            let commit = crate::document_history_file_store::read_document_tail_commit(
                &graph_dir,
                document_id,
            )
            .expect("read tail commit")
            .expect("tail commit after retry");
            assert_eq!(commit.document_revision, 1);
        });
    });
}

#[test]
fn comments_only_edits_advance_manifest_revision_and_history() {
    with_profile("garden-comments-only-projection", || {
        crate::app_runtime::async_runtime::block_on(async {
            let app = crate::tauri_runtime::build_mock_app_for_tests(true);
            let graph_id = "comments-only-projection";
            let document_id = "comments-document";
            create_graph_service(
                &app,
                CreateGraphInput {
                    title: "Comments Only Projection".to_string(),
                    graph_id: Some(graph_id.to_string()),
                    description: None,
                    operation_id: None,
                },
            )
            .expect("create graph");
            document_write(
                &app,
                &CrdtOperation {
                    operation_id: "seed-comments-document".to_string(),
                    kind: "document.write".to_string(),
                    graph_id: graph_id.to_string(),
                    document_id: Some(document_id.to_string()),
                    payload: json!({
                        "title": "Comments Document",
                        "tiptapJson": {
                            "type": "doc",
                            "content": [{
                                "type": "paragraph",
                                "attrs": { "data-block-id": "comments-block" },
                                "content": [{ "type": "text", "text": "unchanged body" }]
                            }]
                        }
                    }),
                    enqueue_timestamp: "1".to_string(),
                },
            )
            .await
            .expect("seed document");
            let graph_dir = existing_graph_dir(&app, graph_id).expect("graph dir");
            let manifest = document_dir(&graph_dir, document_id)
                .expect("document dir")
                .join("document.json");
            let before: crate::document_types::DocumentRecord =
                crate::storage::read_json(&manifest).expect("raw manifest before comment");
            assert_eq!(before.revision, 1);

            super::document_ops::edit_comment(
                &app,
                &CrdtOperation {
                    operation_id: "set-unanchored-comment".to_string(),
                    kind: "document.editComment".to_string(),
                    graph_id: graph_id.to_string(),
                    document_id: Some(document_id.to_string()),
                    payload: json!({
                        "action": "set",
                        "commentId": "comment-a",
                        "text": "A comment without a content mark",
                        "updatedAt": 2,
                    }),
                    enqueue_timestamp: "2".to_string(),
                },
            )
            .await
            .expect("persist unanchored comment");
            let after_set: crate::document_types::DocumentRecord =
                crate::storage::read_json(&manifest).expect("raw manifest after comment set");
            assert_eq!(after_set.revision, 2);
            assert_eq!(after_set.body, before.body);
            assert_eq!(after_set.tiptap_json, before.tiptap_json);
            assert_ne!(after_set.ydoc_update_base64, before.ydoc_update_base64);

            super::document_ops::edit_comment(
                &app,
                &CrdtOperation {
                    operation_id: "resolve-comment".to_string(),
                    kind: "document.editComment".to_string(),
                    graph_id: graph_id.to_string(),
                    document_id: Some(document_id.to_string()),
                    payload: json!({
                        "action": "resolve",
                        "commentId": "comment-a",
                        "resolved": true,
                        "updatedAt": 3,
                    }),
                    enqueue_timestamp: "3".to_string(),
                },
            )
            .await
            .expect("persist comment resolution");
            let after_resolve: crate::document_types::DocumentRecord =
                crate::storage::read_json(&manifest).expect("raw manifest after resolve");
            assert_eq!(after_resolve.revision, 3);
            assert_eq!(after_resolve.body, before.body);
            assert_eq!(after_resolve.tiptap_json, before.tiptap_json);
            assert_ne!(
                after_resolve.ydoc_update_base64,
                after_set.ydoc_update_base64
            );
            assert_eq!(
                crate::document_history_file_store::read_document_history_store(
                    &graph_dir,
                    graph_id,
                    document_id,
                )
                .expect("comments history")
                .total_count,
                3
            );
        });
    });
}

#[test]
fn automatic_flush_waits_past_enqueue_timeout_without_losing_dirty_state() {
    with_profile("garden-projection-enqueue-liveness", || {
        crate::app_runtime::async_runtime::block_on(async {
            let app = crate::tauri_runtime::build_mock_app_for_tests(true);
            let graph_id = "projection-enqueue-liveness";
            let document_id = "delayed-document";
            create_graph_service(
                &app,
                CreateGraphInput {
                    title: "Projection Enqueue Liveness".to_string(),
                    graph_id: Some(graph_id.to_string()),
                    description: None,
                    operation_id: None,
                },
            )
            .expect("create graph");
            crate::document_service::create_document(
                app.clone(),
                crate::document_types::CreateDocumentInput {
                    graph_id: graph_id.to_string(),
                    title: "Delayed Document".to_string(),
                    document_id: Some(document_id.to_string()),
                },
            )
            .expect("create document shell");

            let graph_dir = existing_graph_dir(&app, graph_id).expect("graph dir");
            let room = app
                .state::<RoomRegistry>()
                .get_or_create(
                    &format!("doc:{graph_id}:{document_id}"),
                    document_ydoc_state_path(&graph_dir, document_id),
                )
                .await
                .expect("document room");
            room.configure_projection_flush(graph_id.to_string(), Some(document_id.to_string()))
                .expect("configure projection target");
            replace_room_text(&room, "delayed-block", "eventually persisted").await;

            let coordinator =
                app.state::<super::persistence_coordinator::GraphPersistenceCoordinator>();
            let generation = coordinator.generation(graph_id).expect("graph generation");
            let lease = coordinator
                .acquire_hot_write(graph_id)
                .await
                .expect("hold graph lease");
            let app_for_flush = app.clone();
            let room_for_flush = room.clone();
            let flush = crate::app_runtime::async_runtime::spawn(async move {
                run_projection_flush_for_test(
                    room_for_flush,
                    app_for_flush,
                    generation,
                    std::time::Duration::from_millis(20),
                )
                .await;
            });

            // Hold the lease across several shortened enqueue chunks. None may
            // consume the completed-error retry budget or abandon the room.
            tokio::time::sleep(std::time::Duration::from_millis(90)).await;
            assert_eq!(
                app.state::<crate::crdt_queue::CrdtOperationQueue>()
                    .counts_for_test()
                    .expect("queue counts while blocked"),
                (0, 0),
                "a timed-out pre-enqueue future must not insert an operation"
            );
            assert!(room.needs_projection_flush());

            drop(lease);
            tokio::time::timeout(std::time::Duration::from_secs(5), flush)
                .await
                .expect("flush made progress after lease release")
                .expect("flush task");
            // Pre-enqueue timeout chunks may retry, but after durable queue
            // insertion the scheduler retains and observes that exact marked
            // future. Returning therefore means the executor outcome is known.
            assert!(!room.needs_projection_flush());
            let record = crate::document_service::read_document(
                app.clone(),
                graph_id.to_string(),
                document_id.to_string(),
            )
            .expect("eventually persisted document");
            assert_eq!(record.revision, 1);
            assert_eq!(record.body, "eventually persisted");

            let traces = app
                .state::<crate::crdt_queue::CrdtOperationQueue>()
                .recent_traces(Some(20), false, Some("crdt.flush"), Some(document_id), None)
                .expect("flush traces");
            assert_eq!(traces.len(), 1, "one dirty epoch queues exactly one flush");
            assert_eq!(traces[0]["ok"], true);
        });
    });
}

#[test]
fn queued_automatic_flush_keeps_observing_outcome_past_attempt_timeout() {
    with_profile("garden-projection-queued-watcher", || {
        crate::app_runtime::async_runtime::block_on(async {
            let app = crate::tauri_runtime::build_mock_app_for_tests(true);
            let graph_id = "projection-queued-watcher";
            let document_id = "queued-document";
            create_graph_service(
                &app,
                CreateGraphInput {
                    title: "Projection Queued Watcher".to_string(),
                    graph_id: Some(graph_id.to_string()),
                    description: None,
                    operation_id: None,
                },
            )
            .expect("create graph");
            crate::document_service::create_document(
                app.clone(),
                crate::document_types::CreateDocumentInput {
                    graph_id: graph_id.to_string(),
                    title: "Queued Document".to_string(),
                    document_id: Some(document_id.to_string()),
                },
            )
            .expect("create document shell");

            let graph_dir = existing_graph_dir(&app, graph_id).expect("graph dir");
            let room = app
                .state::<RoomRegistry>()
                .get_or_create(
                    &format!("doc:{graph_id}:{document_id}"),
                    document_ydoc_state_path(&graph_dir, document_id),
                )
                .await
                .expect("document room");
            room.configure_projection_flush(graph_id.to_string(), Some(document_id.to_string()))
                .expect("configure projection target");
            replace_room_text(&room, "queued-block", "queued outcome observed").await;

            let queue = app.state::<crate::crdt_queue::CrdtOperationQueue>();
            let drain_guard = queue.lock_drain().await;
            let generation = app
                .state::<super::persistence_coordinator::GraphPersistenceCoordinator>()
                .generation(graph_id)
                .expect("graph generation");
            let app_for_flush = app.clone();
            let room_for_flush = room.clone();
            let mut flush = crate::app_runtime::async_runtime::spawn(async move {
                run_projection_flush_for_test(
                    room_for_flush,
                    app_for_flush,
                    generation,
                    std::time::Duration::from_millis(20),
                )
                .await;
            });

            tokio::time::timeout(std::time::Duration::from_secs(2), async {
                while queue.counts_for_test().expect("queue counts") != (1, 0) {
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                }
            })
            .await
            .expect("flush reached durable queue insertion");
            assert!(
                tokio::time::timeout(std::time::Duration::from_millis(80), &mut flush)
                    .await
                    .is_err(),
                "scheduler stopped observing an operation that was queued but not complete"
            );

            drop(drain_guard);
            tokio::time::timeout(std::time::Duration::from_secs(5), flush)
                .await
                .expect("queued flush completed after drainer release")
                .expect("flush task");
            assert!(!room.needs_projection_flush());
            let record = crate::document_service::read_document(
                app.clone(),
                graph_id.to_string(),
                document_id.to_string(),
            )
            .expect("persisted queued document");
            assert_eq!(record.body, "queued outcome observed");
            let traces = queue
                .recent_traces(Some(20), false, Some("crdt.flush"), Some(document_id), None)
                .expect("queued flush traces");
            assert_eq!(traces.len(), 1);
            assert_eq!(traces[0]["ok"], true);
        });
    });
}

#[test]
fn recovered_graph_flush_hydrates_empty_registry_sidecars() {
    with_profile("garden-recovered-flush-hydration", || {
        crate::app_runtime::async_runtime::block_on(async {
            let app = crate::tauri_runtime::build_mock_app_for_tests(true);
            let graph_id = "recovered-flush-hydration";
            let document_id = "persisted-hot-document";
            let title = "Persisted Hot Document";
            let body = "recovered sidecar materialization sentinel";
            let graph = create_graph_service(
                &app,
                CreateGraphInput {
                    title: "Recovered Flush Hydration".to_string(),
                    graph_id: Some(graph_id.to_string()),
                    description: None,
                    operation_id: None,
                },
            )
            .expect("create graph");
            let graph_dir = existing_graph_dir(&app, graph_id).expect("graph dir");

            // Persist hot authority through a registry that is deliberately not
            // the app's managed registry, modeling sidecars left by a prior
            // process while the restarted process has hosted no rooms yet.
            let prior_registry = RoomRegistry::default();
            let workspace_room = prior_registry
                .get_or_create(
                    &format!("workspace:{graph_id}"),
                    crate::ydoc_paths::workspace_ydoc_state_path(&graph_dir),
                )
                .await
                .expect("prior workspace room");
            workspace_room
                .update_doc(|_doc, txn| {
                    write_workspace_document(
                        txn,
                        &json!({
                            "documentId": document_id,
                            "title": title,
                            "order": 1,
                            "updatedAt": 1,
                        }),
                    )
                })
                .await
                .expect("persist prior workspace sidecar");
            let document_room = prior_registry
                .get_or_create(
                    &format!("doc:{graph_id}:{document_id}"),
                    document_ydoc_state_path(&graph_dir, document_id),
                )
                .await
                .expect("prior document room");
            replace_room_text(&document_room, "recovered-block", body).await;
            drop(document_room);
            drop(workspace_room);
            drop(prior_registry);

            let restarted_registry = app.state::<RoomRegistry>();
            assert!(restarted_registry
                .peek(&format!("workspace:{graph_id}"))
                .await
                .is_none());
            assert!(restarted_registry
                .peek(&format!("doc:{graph_id}:{document_id}"))
                .await
                .is_none());
            assert!(crate::document_service::read_document(
                app.clone(),
                graph_id.to_string(),
                document_id.to_string(),
            )
            .is_err());
            assert!(!crate::ydoc_paths::workspace_snapshot_path(&graph_dir).exists());

            let recovered_flush = CrdtOperation {
                operation_id: "recovered-empty-registry-flush".to_string(),
                kind: "crdt.flush".to_string(),
                graph_id: graph_id.to_string(),
                document_id: None,
                payload: json!({
                    "includeMaterialization": true,
                    "graphIncarnation": graph.incarnation_id.expect("graph incarnation"),
                }),
                enqueue_timestamp: "1".to_string(),
            };
            crate::crdt_operation_journal::record_crdt_operation_queued(&app, &recovered_flush)
                .expect("journal recovered flush");
            assert_eq!(
                crate::crdt_queue::recover_crdt_operations(app.clone())
                    .expect("recover durable flush"),
                1
            );
            super::executor::drain_queue(app.clone()).await;
            assert!(
                crate::crdt_operation_journal::recover_pending_crdt_operations(&app)
                    .expect("completed recovery journal")
                    .is_empty()
            );

            let record = crate::document_service::read_document(
                app.clone(),
                graph_id.to_string(),
                document_id.to_string(),
            )
            .expect("cold document materialized from recovered sidecar");
            assert_eq!(record.title, title);
            assert_eq!(record.body, body);
            assert_eq!(record.blocks[0].id, "recovered-block");
            let workspace: Value =
                crate::storage::read_json(&crate::ydoc_paths::workspace_snapshot_path(&graph_dir))
                    .expect("cold workspace materialized from recovered sidecar");
            assert_eq!(workspace["documents"][0]["id"], document_id);

            let document_projection =
                crate::rdf_authority::document_projection_graph_iri(graph_id, document_id);
            let workspace_projection =
                crate::rdf_authority::workspace_projection_graph_iri(graph_id);
            let store = crate::rdf_service::open_graph_store(&graph_dir).expect("graph RDF");
            let projected = oxigraph::sparql::SparqlEvaluator::new()
                .parse_query(&format!(
                    "ASK WHERE {{ GRAPH <{document_projection}> {{ ?s ?p ?o }} \
                     GRAPH <{workspace_projection}> {{ ?ws ?wp ?wo }} }}"
                ))
                .expect("parse recovered projection ASK")
                .on_store(&store)
                .execute()
                .expect("query recovered projections");
            assert!(matches!(
                projected,
                oxigraph::sparql::QueryResults::Boolean(true)
            ));
            let traces = app
                .state::<crate::crdt_queue::CrdtOperationQueue>()
                .recent_traces(
                    Some(20),
                    false,
                    Some("crdt.flush"),
                    None,
                    Some("recovered-empty-registry-flush"),
                )
                .expect("recovered flush trace");
            assert_eq!(traces.len(), 1);
            assert_eq!(traces[0]["ok"], true);
        });
    });
}

#[test]
fn recovered_automatic_workspace_flush_does_not_hydrate_unrelated_documents() {
    with_profile("garden-recovered-workspace-flush-scope", || {
        crate::app_runtime::async_runtime::block_on(async {
            let app = crate::tauri_runtime::build_mock_app_for_tests(true);
            let graph_id = "recovered-workspace-flush-scope";
            let document_id = "unrelated-persisted-document";
            let graph = create_graph_service(
                &app,
                CreateGraphInput {
                    title: "Recovered Workspace Flush Scope".to_string(),
                    graph_id: Some(graph_id.to_string()),
                    description: None,
                    operation_id: None,
                },
            )
            .expect("create graph");
            let graph_dir = existing_graph_dir(&app, graph_id).expect("graph dir");

            let prior_registry = RoomRegistry::default();
            let workspace_room = prior_registry
                .get_or_create(
                    &format!("workspace:{graph_id}"),
                    crate::ydoc_paths::workspace_ydoc_state_path(&graph_dir),
                )
                .await
                .expect("prior workspace room");
            workspace_room
                .update_doc(|_doc, txn| {
                    write_workspace_folder(
                        txn,
                        &json!({
                            "folderId": "recovered-folder",
                            "title": "Recovered Folder",
                            "order": 1,
                            "updatedAt": 1,
                        }),
                    )
                })
                .await
                .expect("persist prior workspace sidecar");
            let document_room = prior_registry
                .get_or_create(
                    &format!("doc:{graph_id}:{document_id}"),
                    document_ydoc_state_path(&graph_dir, document_id),
                )
                .await
                .expect("prior unrelated document room");
            replace_room_text(&document_room, "unrelated-block", "must stay unhosted").await;
            drop(document_room);
            drop(workspace_room);
            drop(prior_registry);

            let recovered_flush = CrdtOperation {
                operation_id: "recovered-workspace-generation-flush".to_string(),
                kind: "crdt.flush".to_string(),
                graph_id: graph_id.to_string(),
                document_id: None,
                payload: json!({
                    "includeMaterialization": true,
                    "graphGeneration": 7,
                    "graphIncarnation": graph.incarnation_id.expect("graph incarnation"),
                }),
                enqueue_timestamp: "1".to_string(),
            };
            crate::crdt_operation_journal::record_crdt_operation_queued(&app, &recovered_flush)
                .expect("journal recovered workspace flush");
            assert_eq!(
                crate::crdt_queue::recover_crdt_operations(app.clone())
                    .expect("recover workspace flush"),
                1
            );
            super::executor::drain_queue(app.clone()).await;

            let registry = app.state::<RoomRegistry>();
            assert!(registry
                .peek(&format!("workspace:{graph_id}"))
                .await
                .is_some());
            assert!(
                registry
                    .peek(&format!("doc:{graph_id}:{document_id}"))
                    .await
                    .is_none(),
                "automatic workspace recovery must not scan all document sidecars"
            );
            assert!(crate::document_service::read_document(
                app.clone(),
                graph_id.to_string(),
                document_id.to_string(),
            )
            .is_err());
            let workspace: Value =
                crate::storage::read_json(&crate::ydoc_paths::workspace_snapshot_path(&graph_dir))
                    .expect("workspace recovery materialized cold snapshot");
            assert_eq!(workspace["folders"][0]["id"], "recovered-folder");
        });
    });
}

#[test]
fn ordinary_graph_flush_does_not_scan_persisted_sidecars() {
    with_profile("garden-ordinary-flush-no-sidecar-scan", || {
        crate::app_runtime::async_runtime::block_on(async {
            let app = crate::tauri_runtime::build_mock_app_for_tests(true);
            let graph_id = "ordinary-flush-no-sidecar-scan";
            let document_id = "persisted-but-not-live";
            create_graph_service(
                &app,
                CreateGraphInput {
                    title: "Ordinary Flush No Sidecar Scan".to_string(),
                    graph_id: Some(graph_id.to_string()),
                    description: None,
                    operation_id: None,
                },
            )
            .expect("create graph");
            let graph_dir = existing_graph_dir(&app, graph_id).expect("graph dir");
            let prior_registry = RoomRegistry::default();
            let prior_room = prior_registry
                .get_or_create(
                    &format!("doc:{graph_id}:{document_id}"),
                    document_ydoc_state_path(&graph_dir, document_id),
                )
                .await
                .expect("prior document room");
            replace_room_text(&prior_room, "persisted-block", "persisted but not live").await;
            drop(prior_room);
            drop(prior_registry);

            let outcome = super::flush_ops::apply(
                &app,
                &CrdtOperation {
                    operation_id: "ordinary-fresh-graph-flush".to_string(),
                    kind: "crdt.flush".to_string(),
                    graph_id: graph_id.to_string(),
                    document_id: None,
                    payload: json!({ "includeMaterialization": true }),
                    enqueue_timestamp: "1".to_string(),
                },
            )
            .await
            .expect("ordinary graph flush");
            assert_eq!(outcome["documentsFlushed"], json!([]));
            assert!(app
                .state::<RoomRegistry>()
                .peek(&format!("doc:{graph_id}:{document_id}"))
                .await
                .is_none());
            assert!(crate::document_service::read_document(
                app.clone(),
                graph_id.to_string(),
                document_id.to_string(),
            )
            .is_err());
        });
    });
}

#[test]
fn expected_revision_mismatch_changes_neither_hot_nor_cold_state() {
    with_profile("garden-revision-preflight", || {
        crate::app_runtime::async_runtime::block_on(async {
            let app = crate::tauri_runtime::build_mock_app_for_tests(true);
            let graph_id = "revision-preflight";
            let document_id = "revision-document";
            create_graph_service(
                &app,
                CreateGraphInput {
                    title: "Revision Preflight".to_string(),
                    graph_id: Some(graph_id.to_string()),
                    description: None,
                    operation_id: None,
                },
            )
            .expect("create graph");
            let operation =
                |id: &str, title: &str, block: &str, body: &str, comment: &str| CrdtOperation {
                    operation_id: id.to_string(),
                    kind: "document.write".to_string(),
                    graph_id: graph_id.to_string(),
                    document_id: Some(document_id.to_string()),
                    payload: json!({
                        "title": title,
                        "expectedRevision": 0,
                        "tiptapJson": {
                            "type": "doc",
                            "content": [{
                                "type": "paragraph",
                                "attrs": { "data-block-id": block },
                                "content": [{ "type": "text", "text": body }]
                            }]
                        },
                        "comments": {
                            "comment-stable": { "id": "comment-stable", "text": comment }
                        }
                    }),
                    enqueue_timestamp: "1".to_string(),
                };
            document_write(
                &app,
                &operation(
                    "seed-revision-document",
                    "Stable title",
                    "stable-block",
                    "stable body",
                    "stable",
                ),
            )
            .await
            .expect("seed document");

            let graph_dir = existing_graph_dir(&app, graph_id).expect("graph dir");
            let room = app
                .state::<RoomRegistry>()
                .peek(&format!("doc:{graph_id}:{document_id}"))
                .await
                .expect("seeded room");
            let mut updates = room.subscribe_updates_for_test();
            let hot_before = room.encode_state_for_test().await;
            let state_path = document_ydoc_state_path(&graph_dir, document_id);
            let sidecar_before = std::fs::read(&state_path).expect("sidecar");
            let manifest = document_dir(&graph_dir, document_id)
                .expect("document dir")
                .join("document.json");
            let record_before = std::fs::read(&manifest).expect("record");
            let history_before = crate::document_history_file_store::read_document_history_store(
                &graph_dir,
                graph_id,
                document_id,
            )
            .expect("history")
            .total_count;

            let error = document_write(
                &app,
                &operation(
                    "mismatched-replay",
                    "Conflicting title",
                    "conflicting-block",
                    "conflicting body",
                    "changed",
                ),
            )
            .await
            .expect_err("mismatched expected+1 replay conflicts");
            assert!(error.contains("revision conflict"), "{error}");
            assert_eq!(room.encode_state_for_test().await, hot_before);
            assert_eq!(
                std::fs::read(&state_path).expect("sidecar after"),
                sidecar_before
            );
            assert_eq!(
                std::fs::read(&manifest).expect("record after"),
                record_before
            );
            assert!(matches!(
                updates.try_recv(),
                Err(tokio::sync::broadcast::error::TryRecvError::Empty)
            ));
            assert_eq!(
                crate::document_history_file_store::read_document_history_store(
                    &graph_dir,
                    graph_id,
                    document_id,
                )
                .expect("history after")
                .total_count,
                history_before
            );

            let record = read_document_record(&graph_dir, &manifest).expect("document record");
            let mut candidate = serde_json::to_value(record).expect("candidate");
            candidate["title"] = json!("Cold mismatch");
            candidate["expectedRevision"] = json!(0);
            let cold_error =
                reconcile_matching_document_projection(&app, graph_id, document_id, &candidate)
                    .expect_err("cold mismatch conflicts");
            assert!(cold_error.contains("revision conflict"), "{cold_error}");
            assert_eq!(
                std::fs::read(&manifest).expect("final record"),
                record_before
            );
        });
    });
}

#[test]
fn corrupt_existing_manifest_fails_expected_revision_before_hot_mutation() {
    with_profile("garden-revision-corrupt-manifest", || {
        crate::app_runtime::async_runtime::block_on(async {
            let app = crate::tauri_runtime::build_mock_app_for_tests(true);
            let graph_id = "revision-corrupt-manifest";
            let document_id = "corrupt-document";
            create_graph_service(
                &app,
                CreateGraphInput {
                    title: "Revision Corrupt Manifest".to_string(),
                    graph_id: Some(graph_id.to_string()),
                    description: None,
                    operation_id: None,
                },
            )
            .expect("create graph");
            crate::document_service::create_document(
                app.clone(),
                crate::document_types::CreateDocumentInput {
                    graph_id: graph_id.to_string(),
                    title: "Corrupt Document".to_string(),
                    document_id: Some(document_id.to_string()),
                },
            )
            .expect("create document shell");

            let graph_dir = existing_graph_dir(&app, graph_id).expect("graph dir");
            let state_path = document_ydoc_state_path(&graph_dir, document_id);
            let room = app
                .state::<RoomRegistry>()
                .get_or_create(&format!("doc:{graph_id}:{document_id}"), state_path.clone())
                .await
                .expect("document room");
            replace_room_text(&room, "stable-hot-block", "stable hot state").await;
            let hot_before = room.encode_state_for_test().await;
            let sidecar_before = std::fs::read(&state_path).expect("stable sidecar");
            let mut updates = room.subscribe_updates_for_test();
            let manifest = document_dir(&graph_dir, document_id)
                .expect("document dir")
                .join("document.json");
            std::fs::write(&manifest, b"{ definitely-not-json").expect("corrupt existing manifest");

            let error = document_write(
                &app,
                &CrdtOperation {
                    operation_id: "corrupt-manifest-write".to_string(),
                    kind: "document.write".to_string(),
                    graph_id: graph_id.to_string(),
                    document_id: Some(document_id.to_string()),
                    payload: json!({
                        "title": "Must Not Apply",
                        "expectedRevision": 0,
                        "tiptapJson": {
                            "type": "doc",
                            "content": [{
                                "type": "paragraph",
                                "attrs": { "data-block-id": "must-not-apply" },
                                "content": [{ "type": "text", "text": "must not apply" }]
                            }]
                        }
                    }),
                    enqueue_timestamp: "1".to_string(),
                },
            )
            .await
            .expect_err("corrupt manifest fails before revision preflight");
            assert!(
                error.contains("read") || error.contains("parse") || error.contains("JSON"),
                "{error}"
            );
            assert_eq!(room.encode_state_for_test().await, hot_before);
            assert_eq!(
                std::fs::read(&state_path).expect("sidecar after rejected write"),
                sidecar_before
            );
            assert!(matches!(
                updates.try_recv(),
                Err(tokio::sync::broadcast::error::TryRecvError::Empty)
            ));
            assert_eq!(
                std::fs::read(&manifest).expect("corrupt manifest remains"),
                b"{ definitely-not-json"
            );
        });
    });
}

#[test]
fn recovered_block_ledger_hit_repairs_unfinished_cold_projection_tail() {
    with_profile("garden-block-ledger-cold-tail", || {
        crate::app_runtime::async_runtime::block_on(async {
            let app = crate::tauri_runtime::build_mock_app_for_tests(true);
            let graph_id = "block-ledger-cold-tail";
            let document_id = "ledger-recovery-document";
            create_graph_service(
                &app,
                CreateGraphInput {
                    title: "Block Ledger Cold Tail".to_string(),
                    graph_id: Some(graph_id.to_string()),
                    description: None,
                    operation_id: None,
                },
            )
            .expect("create graph");
            let graph_dir = existing_graph_dir(&app, graph_id).expect("graph dir");
            let blocked_manifest = document_dir(&graph_dir, document_id)
                .expect("document dir")
                .join("document.json");
            std::fs::create_dir_all(&blocked_manifest).expect("block only the cold manifest write");
            let operation = CrdtOperation {
                operation_id: "recovered-block-ledger-operation".to_string(),
                kind: "block.insert".to_string(),
                graph_id: graph_id.to_string(),
                document_id: Some(document_id.to_string()),
                payload: json!({
                    "graphIncarnation": current_graph_incarnation(&app, graph_id),
                    "tiptapJson": {
                        "type": "doc",
                        "content": [{
                            "type": "paragraph",
                            "attrs": { "data-block-id": "recovered-ledger-block" },
                            "content": [{ "type": "text", "text": "recovered cold tail" }]
                        }]
                    }
                }),
                enqueue_timestamp: crate::clock::timestamp(),
            };

            crate::crdt_operation_journal::record_crdt_operation_queued(&app, &operation)
                .expect("journal block operation");
            app.state::<crate::crdt_queue::CrdtOperationQueue>()
                .enqueue_detached(operation.clone())
                .expect("enqueue block operation");
            super::executor::drain_queue(app.clone()).await;
            let pending = crate::crdt_operation_journal::recover_pending_crdt_operations(&app)
                .expect("block operation remains pending");
            assert_eq!(pending.len(), 1);
            assert_eq!(pending[0].operation_id, operation.operation_id);
            assert_eq!(
                app.state::<crate::crdt_queue::CrdtOperationQueue>()
                    .counts_for_test()
                    .expect("parked queue counts"),
                (1, 0)
            );
            let registry = app.state::<RoomRegistry>();
            let room_key = format!("doc:{graph_id}:{document_id}");
            let first_room = registry.peek(&room_key).await.expect("hot ledger room");
            assert!(first_room.needs_projection_flush());
            assert!(crate::document_service::read_document(
                app.clone(),
                graph_id.to_string(),
                document_id.to_string(),
            )
            .is_err());

            // Model process recovery: a new queue and registry hydrate the
            // original mutation+ledger from its authoritative sidecar.
            std::fs::remove_dir_all(&blocked_manifest).expect("unblock cold manifest write");
            let restarted = crate::tauri_runtime::build_mock_app_for_tests(true);
            assert_eq!(
                crate::crdt_queue::recover_crdt_operations(restarted.clone())
                    .expect("recover original block operation"),
                1
            );
            super::executor::drain_queue(restarted.clone()).await;
            assert!(
                crate::crdt_operation_journal::recover_pending_crdt_operations(&restarted)
                    .expect("completed block journal")
                    .is_empty()
            );
            let record = crate::document_service::read_document(
                restarted.clone(),
                graph_id.to_string(),
                document_id.to_string(),
            )
            .expect("recovered cold record");
            assert_eq!(record.revision, 1);
            assert_eq!(record.body, "recovered cold tail");
            assert_eq!(record.blocks[0].id, "recovered-ledger-block");
            assert_eq!(
                crate::document_history_file_store::read_document_history_store(
                    &graph_dir,
                    graph_id,
                    document_id,
                )
                .expect("recovered history")
                .total_count,
                1
            );
            let projection =
                crate::rdf_authority::document_projection_graph_iri(graph_id, document_id);
            let store = crate::rdf_service::open_graph_store(&graph_dir).expect("graph RDF");
            let result = oxigraph::sparql::SparqlEvaluator::new()
                .parse_query(&format!(
                    "ASK WHERE {{ GRAPH <{projection}> {{ ?s \
                     <http://mnemosyne.dev/doc#textContent> \"recovered cold tail\" }} }}"
                ))
                .expect("parse recovered RDF ASK")
                .on_store(&store)
                .execute()
                .expect("query recovered RDF");
            assert!(matches!(
                result,
                oxigraph::sparql::QueryResults::Boolean(true)
            ));
        });
    });
}

#[test]
fn invalid_document_ids_cannot_create_room_sidecars_outside_document_root() {
    with_profile("garden-room-document-path-safety", || {
        crate::app_runtime::async_runtime::block_on(async {
            let app = crate::tauri_runtime::build_mock_app_for_tests(true);
            let graph_id = "room-document-path-safety";
            create_graph_service(
                &app,
                CreateGraphInput {
                    title: "Room Document Path Safety".to_string(),
                    graph_id: Some(graph_id.to_string()),
                    description: None,
                    operation_id: None,
                },
            )
            .expect("create graph");
            let graph_dir = existing_graph_dir(&app, graph_id).expect("graph dir");

            for (index, document_id) in ["..", "../escape", "nested/escape"].into_iter().enumerate()
            {
                let operation = CrdtOperation {
                    operation_id: format!("invalid-document-path-{index}"),
                    kind: "block.insert".to_string(),
                    graph_id: graph_id.to_string(),
                    document_id: Some(document_id.to_string()),
                    payload: json!({
                        "tiptapJson": {
                            "type": "doc",
                            "content": [{
                                "type": "paragraph",
                                "attrs": { "data-block-id": "never-written" },
                                "content": [{ "type": "text", "text": "never written" }]
                            }]
                        }
                    }),
                    enqueue_timestamp: "1".to_string(),
                };
                let error = super::block_ops::apply(&app, &operation)
                    .await
                    .expect_err("invalid document id is rejected before room construction");
                assert!(error.contains("invalid characters"), "{error}");
                assert!(app
                    .state::<RoomRegistry>()
                    .peek(&format!("doc:{graph_id}:{document_id}"))
                    .await
                    .is_none());
            }

            assert!(!graph_dir.join("ydocs/update-v1.bin").exists());
            assert!(!graph_dir.join("ydocs/escape/update-v1.bin").exists());
            assert!(!graph_dir
                .join("ydocs/documents/nested/escape/update-v1.bin")
                .exists());
        });
    });
}

#[test]
fn document_room_connection_requires_workspace_membership_or_valid_cold_record() {
    with_profile("garden-document-room-membership-guard", || {
        crate::app_runtime::async_runtime::block_on(async {
            let app = crate::tauri_runtime::build_mock_app_for_tests(true);
            let graph_id = "document-room-membership-guard";
            let workspace_only_id = "workspace-only-document";
            let cold_only_id = "cold-only-document";
            create_graph_service(
                &app,
                CreateGraphInput {
                    title: "Document Room Membership Guard".to_string(),
                    graph_id: Some(graph_id.to_string()),
                    description: None,
                    operation_id: None,
                },
            )
            .expect("create graph");
            let graph_dir = existing_graph_dir(&app, graph_id).expect("graph dir");
            let registry = RoomRegistry::default();

            assert!(
                !crate::loopback_hocuspocus_routes::document_exists_for_room_connection(
                    &registry,
                    graph_id,
                    &graph_dir,
                    workspace_only_id,
                )
                .await
                .expect("missing document check")
            );
            assert!(registry
                .peek(&format!("doc:{graph_id}:{workspace_only_id}"))
                .await
                .is_none());

            // Write an entry with no title at all through another registry. The
            // guard must check map membership, then hydrate that persisted
            // workspace sidecar in the fresh registry above.
            let writer_registry = RoomRegistry::default();
            let writer_workspace = writer_registry
                .get_or_create(
                    &format!("workspace:{graph_id}"),
                    crate::ydoc_paths::workspace_ydoc_state_path(&graph_dir),
                )
                .await
                .expect("writer workspace room");
            writer_workspace
                .update_doc(|_doc, txn| {
                    txn.get_or_insert_map("documents").insert(
                        txn,
                        workspace_only_id,
                        MapPrelim::default(),
                    );
                    Ok(())
                })
                .await
                .expect("persist titleless workspace member");
            drop(writer_workspace);
            drop(writer_registry);
            assert!(
                crate::loopback_hocuspocus_routes::document_exists_for_room_connection(
                    &registry,
                    graph_id,
                    &graph_dir,
                    workspace_only_id,
                )
                .await
                .expect("hydrated workspace membership")
            );
            assert!(registry
                .peek(&format!("doc:{graph_id}:{workspace_only_id}"))
                .await
                .is_none());

            let hydrated_workspace = registry
                .peek(&format!("workspace:{graph_id}"))
                .await
                .expect("workspace was hydrated");
            hydrated_workspace
                .update_doc(|_doc, txn| {
                    txn.get_or_insert_map("documents")
                        .remove(txn, workspace_only_id);
                    Ok(())
                })
                .await
                .expect("remove workspace identity");
            assert!(
                !crate::loopback_hocuspocus_routes::document_exists_for_room_connection(
                    &registry,
                    graph_id,
                    &graph_dir,
                    workspace_only_id,
                )
                .await
                .expect("stale reconnect is rejected")
            );

            let snapshot_only_id = "snapshot-only-document";
            let workspace_state_path = crate::ydoc_paths::workspace_ydoc_state_path(&graph_dir);
            std::fs::remove_file(&workspace_state_path)
                .expect("remove workspace sidecar for legacy snapshot case");
            crate::storage::write_json(
                &crate::ydoc_paths::workspace_snapshot_path(&graph_dir),
                &json!({
                    "documents": [{ "id": snapshot_only_id, "title": "" }],
                    "folders": [],
                }),
            )
            .expect("write legacy workspace snapshot");
            let snapshot_registry = RoomRegistry::default();
            assert!(
                crate::loopback_hocuspocus_routes::document_exists_for_room_connection(
                    &snapshot_registry,
                    graph_id,
                    &graph_dir,
                    snapshot_only_id,
                )
                .await
                .expect("snapshot-only workspace membership")
            );
            assert!(snapshot_registry
                .peek(&format!("doc:{graph_id}:{snapshot_only_id}"))
                .await
                .is_none());

            crate::document_service::create_document(
                app.clone(),
                crate::document_types::CreateDocumentInput {
                    graph_id: graph_id.to_string(),
                    title: "Cold Only Document".to_string(),
                    document_id: Some(cold_only_id.to_string()),
                },
            )
            .expect("create valid cold record");
            assert!(
                crate::loopback_hocuspocus_routes::document_exists_for_room_connection(
                    &registry,
                    graph_id,
                    &graph_dir,
                    cold_only_id,
                )
                .await
                .expect("valid cold manifest admits MCP create-before-open")
            );

            let mismatched_id = "mismatched-cold-document";
            crate::document_service::create_document(
                app.clone(),
                crate::document_types::CreateDocumentInput {
                    graph_id: graph_id.to_string(),
                    title: "Mismatched Cold Document".to_string(),
                    document_id: Some(mismatched_id.to_string()),
                },
            )
            .expect("create mismatched cold record");
            let mismatched_manifest = document_dir(&graph_dir, mismatched_id)
                .expect("mismatched document dir")
                .join("document.json");
            let mut mismatched_record: Value =
                crate::storage::read_json(&mismatched_manifest).expect("read mismatched record");
            mismatched_record["documentId"] = json!("different-document-id");
            mismatched_record["ydocUpdateBase64"] = json!("bXVzdC1ub3Qtd3JpdGU=");
            crate::storage::write_json(&mismatched_manifest, &mismatched_record)
                .expect("write mismatched cold record");
            let foreign_sidecar = document_ydoc_state_path(&graph_dir, "different-document-id");
            assert!(!foreign_sidecar.exists());
            assert!(
                !crate::loopback_hocuspocus_routes::document_exists_for_room_connection(
                    &registry,
                    graph_id,
                    &graph_dir,
                    mismatched_id,
                )
                .await
                .expect("mismatched manifest is not authoritative")
            );
            assert!(
                !foreign_sidecar.exists(),
                "rejected manifest wrote through its mismatched embedded id"
            );

            let traversal_id = "traversal-cold-document";
            crate::document_service::create_document(
                app.clone(),
                crate::document_types::CreateDocumentInput {
                    graph_id: graph_id.to_string(),
                    title: "Traversal Cold Document".to_string(),
                    document_id: Some(traversal_id.to_string()),
                },
            )
            .expect("create traversal cold record");
            let traversal_manifest = document_dir(&graph_dir, traversal_id)
                .expect("traversal document dir")
                .join("document.json");
            let mut traversal_record: Value =
                crate::storage::read_json(&traversal_manifest).expect("read traversal record");
            traversal_record["documentId"] = json!("../../escape");
            traversal_record["ydocUpdateBase64"] = json!("bXVzdC1ub3Qtd3JpdGU=");
            crate::storage::write_json(&traversal_manifest, &traversal_record)
                .expect("write traversal record");
            let escaped_sidecar = graph_dir.join("escape/update-v1.bin");
            assert!(
                crate::loopback_hocuspocus_routes::document_exists_for_room_connection(
                    &registry,
                    graph_id,
                    &graph_dir,
                    traversal_id,
                )
                .await
                .is_err()
            );
            assert!(
                !escaped_sidecar.exists(),
                "rejected manifest escaped the document sidecar root"
            );

            let wrong_graph_id = "wrong-graph-cold-document";
            crate::document_service::create_document(
                app.clone(),
                crate::document_types::CreateDocumentInput {
                    graph_id: graph_id.to_string(),
                    title: "Wrong Graph Cold Document".to_string(),
                    document_id: Some(wrong_graph_id.to_string()),
                },
            )
            .expect("create wrong-graph cold record");
            let wrong_graph_manifest = document_dir(&graph_dir, wrong_graph_id)
                .expect("wrong-graph document dir")
                .join("document.json");
            let mut wrong_graph_record: Value =
                crate::storage::read_json(&wrong_graph_manifest).expect("read wrong-graph record");
            wrong_graph_record["graphId"] = json!("different-graph-id");
            wrong_graph_record["ydocUpdateBase64"] = json!("bXVzdC1ub3Qtd3JpdGU=");
            crate::storage::write_json(&wrong_graph_manifest, &wrong_graph_record)
                .expect("write wrong-graph record");
            let wrong_graph_sidecar = document_ydoc_state_path(&graph_dir, wrong_graph_id);
            if wrong_graph_sidecar.is_file() {
                std::fs::remove_file(&wrong_graph_sidecar)
                    .expect("remove preexisting wrong-graph sidecar");
            }
            assert!(
                !crate::loopback_hocuspocus_routes::document_exists_for_room_connection(
                    &registry,
                    graph_id,
                    &graph_dir,
                    wrong_graph_id,
                )
                .await
                .expect("wrong graph identity is rejected")
            );
            assert!(
                !wrong_graph_sidecar.exists(),
                "rejected graph identity backfilled a sidecar"
            );

            let corrupt_id = "corrupt-cold-document";
            crate::document_service::create_document(
                app.clone(),
                crate::document_types::CreateDocumentInput {
                    graph_id: graph_id.to_string(),
                    title: "Corrupt Cold Document".to_string(),
                    document_id: Some(corrupt_id.to_string()),
                },
            )
            .expect("create corruptable cold record");
            let corrupt_manifest = document_dir(&graph_dir, corrupt_id)
                .expect("corrupt document dir")
                .join("document.json");
            std::fs::write(&corrupt_manifest, b"{ not-readable-json")
                .expect("corrupt cold manifest");
            assert!(
                crate::loopback_hocuspocus_routes::document_exists_for_room_connection(
                    &registry, graph_id, &graph_dir, corrupt_id,
                )
                .await
                .is_err()
            );
            assert!(registry
                .peek(&format!("doc:{graph_id}:{corrupt_id}"))
                .await
                .is_none());
            for document_id in [
                workspace_only_id,
                cold_only_id,
                mismatched_id,
                traversal_id,
                wrong_graph_id,
                corrupt_id,
            ] {
                assert!(registry
                    .peek(&format!("doc:{graph_id}:{document_id}"))
                    .await
                    .is_none());
            }
        });
    });
}

#[test]
fn artifact_post_hot_tail_failure_retries_original_journaled_operation_after_restart() {
    with_profile("garden-artifact-post-hot-retry", || {
        crate::app_runtime::async_runtime::block_on(async {
            let app = crate::tauri_runtime::build_mock_app_for_tests(true);
            let graph_id = "artifact-post-hot-retry";
            let artifact_id = "artifact-post-hot";
            create_graph_service(
                &app,
                CreateGraphInput {
                    title: "Artifact Post-hot Retry".to_string(),
                    graph_id: Some(graph_id.to_string()),
                    description: None,
                    operation_id: None,
                },
            )
            .expect("create graph");
            super::workspace_ops::apply(
                &app,
                &CrdtOperation {
                    operation_id: "seed-post-hot-artifact".to_string(),
                    kind: "workspace.putArtifact".to_string(),
                    graph_id: graph_id.to_string(),
                    document_id: Some(artifact_id.to_string()),
                    payload: json!({
                        "artifactId": artifact_id,
                        "label": "Post-hot.pdf",
                        "status": "ready",
                        "order": 1,
                        "updatedAt": 1,
                    }),
                    enqueue_timestamp: "1".to_string(),
                },
            )
            .await
            .expect("seed artifact");
            let graph_dir = existing_graph_dir(&app, graph_id).expect("graph dir");
            let snapshot_path = crate::ydoc_paths::workspace_snapshot_path(&graph_dir);
            std::fs::remove_file(&snapshot_path).expect("remove workspace snapshot");
            std::fs::create_dir(&snapshot_path).expect("block workspace tail write");

            let operation = CrdtOperation {
                operation_id: "delete-post-hot-artifact".to_string(),
                kind: "workspace.deleteArtifact".to_string(),
                graph_id: graph_id.to_string(),
                document_id: Some(artifact_id.to_string()),
                payload: json!({
                    "artifactId": artifact_id,
                    "graphIncarnation": current_graph_incarnation(&app, graph_id),
                }),
                enqueue_timestamp: "2".to_string(),
            };
            crate::crdt_operation_journal::record_crdt_operation_queued(&app, &operation)
                .expect("journal artifact delete");
            app.state::<crate::crdt_queue::CrdtOperationQueue>()
                .enqueue_detached(operation.clone())
                .expect("enqueue artifact delete");
            super::executor::drain_queue(app.clone()).await;
            let pending = crate::crdt_operation_journal::recover_pending_crdt_operations(&app)
                .expect("artifact operation remains pending");
            assert_eq!(pending.len(), 1);
            assert_eq!(pending[0].operation_id, operation.operation_id);
            assert_eq!(
                app.state::<crate::crdt_queue::CrdtOperationQueue>()
                    .counts_for_test()
                    .expect("parked artifact queue"),
                (1, 0)
            );

            std::fs::remove_dir(&snapshot_path).expect("unblock workspace tail");
            let restarted = crate::tauri_runtime::build_mock_app_for_tests(true);
            assert_eq!(
                crate::crdt_queue::recover_crdt_operations(restarted.clone())
                    .expect("recover artifact delete"),
                1
            );
            super::executor::drain_queue(restarted.clone()).await;
            assert!(
                crate::crdt_operation_journal::recover_pending_crdt_operations(&restarted)
                    .expect("completed artifact journal")
                    .is_empty()
            );
            let snapshot: Value = crate::storage::read_json(&snapshot_path)
                .expect("repaired artifact workspace snapshot");
            assert!(snapshot["artifacts"].as_array().unwrap().is_empty());
        });
    });
}

#[test]
fn recovered_artifact_delete_finishes_workspace_snapshot_and_rdf_tail() {
    with_profile("garden-recovered-artifact-delete", || {
        crate::app_runtime::async_runtime::block_on(async {
            let app = crate::tauri_runtime::build_mock_app_for_tests(true);
            let graph_id = "recovered-artifact-delete";
            let artifact_id = "artifact-recovery";
            create_graph_service(
                &app,
                CreateGraphInput {
                    title: "Recovered Artifact Delete".to_string(),
                    graph_id: Some(graph_id.to_string()),
                    description: None,
                    operation_id: None,
                },
            )
            .expect("create graph");
            super::workspace_ops::apply(
                &app,
                &CrdtOperation {
                    operation_id: "seed-recovered-artifact".to_string(),
                    kind: "workspace.putArtifact".to_string(),
                    graph_id: graph_id.to_string(),
                    document_id: Some(artifact_id.to_string()),
                    payload: json!({
                        "label": "Recovery.pdf",
                        "originalFilename": "Recovery.pdf",
                        "mimeType": "application/pdf",
                        "status": "ready",
                        "order": 1,
                        "updatedAt": 1,
                    }),
                    enqueue_timestamp: "1".to_string(),
                },
            )
            .await
            .expect("seed artifact projection");
            let graph_dir = existing_graph_dir(&app, graph_id).expect("graph dir");
            let workspace_key = format!("workspace:{graph_id}");
            let registry = app.state::<RoomRegistry>();
            let room = registry.peek(&workspace_key).await.expect("workspace room");
            room.update_doc(|_doc, txn| {
                txn.get_or_insert_map("artifacts").remove(txn, artifact_id);
                Ok(())
            })
            .await
            .expect("persist hot-only artifact removal");
            assert!(room.needs_projection_flush());
            assert_eq!(
                crate::storage::read_json::<Value>(&crate::ydoc_paths::workspace_snapshot_path(
                    &graph_dir
                ))
                .expect("stale cold snapshot")["artifacts"][0]["id"],
                artifact_id
            );

            let pending = CrdtOperation {
                operation_id: "recover-artifact-delete".to_string(),
                kind: "workspace.deleteArtifact".to_string(),
                graph_id: graph_id.to_string(),
                document_id: Some(artifact_id.to_string()),
                payload: json!({
                    "artifactId": artifact_id,
                    "graphIncarnation": current_graph_incarnation(&app, graph_id),
                }),
                enqueue_timestamp: "2".to_string(),
            };
            assert!(registry.evict_room(&workspace_key));
            let recovered = journal_and_recover_one(&app, &pending);
            let response = super::executor::apply_operation(&app, &recovered)
                .await
                .expect("recovered artifact delete");
            assert_eq!(response["status"], "deleted");
            let snapshot: Value =
                crate::storage::read_json(&crate::ydoc_paths::workspace_snapshot_path(&graph_dir))
                    .expect("recovered workspace snapshot");
            assert!(snapshot["artifacts"].as_array().unwrap().is_empty());
            let projection = crate::rdf_authority::workspace_projection_graph_iri(graph_id);
            let subject = crate::rdf_workspace_terms::workspace_entity_subject(
                graph_id,
                "artifact",
                artifact_id,
            );
            let store = crate::rdf_service::open_graph_store(&graph_dir).expect("graph RDF");
            let result = oxigraph::sparql::SparqlEvaluator::new()
                .parse_query(&format!(
                    "ASK WHERE {{ GRAPH <{projection}> {{ <{subject}> ?p ?o }} }}"
                ))
                .expect("parse artifact ASK")
                .on_store(&store)
                .execute()
                .expect("query artifact RDF");
            assert!(matches!(
                result,
                oxigraph::sparql::QueryResults::Boolean(false)
            ));

            let fresh_error = super::workspace_ops::apply(
                &app,
                &CrdtOperation {
                    operation_id: "fresh-missing-artifact-delete".to_string(),
                    kind: "workspace.deleteArtifact".to_string(),
                    graph_id: graph_id.to_string(),
                    document_id: Some(artifact_id.to_string()),
                    payload: json!({ "artifactId": artifact_id }),
                    enqueue_timestamp: "3".to_string(),
                },
            )
            .await
            .expect_err("fresh missing artifact remains an error");
            assert!(fresh_error.contains("artifact not found"), "{fresh_error}");
        });
    });
}

#[test]
fn wire_post_hot_tail_failure_retries_original_journaled_operation_after_restart() {
    with_profile("garden-wire-post-hot-retry", || {
        crate::app_runtime::async_runtime::block_on(async {
            let app = crate::tauri_runtime::build_mock_app_for_tests(true);
            let graph_id = "wire-post-hot-retry";
            let wire_id = "wire-post-hot";
            create_graph_service(
                &app,
                CreateGraphInput {
                    title: "Wire Post-hot Retry".to_string(),
                    graph_id: Some(graph_id.to_string()),
                    description: None,
                    operation_id: None,
                },
            )
            .expect("create graph");
            super::workspace_ops::apply(
                &app,
                &CrdtOperation {
                    operation_id: "seed-post-hot-wire-source".to_string(),
                    kind: "workspace.createDocument".to_string(),
                    graph_id: graph_id.to_string(),
                    document_id: Some("wire-post-hot-source".to_string()),
                    payload: json!({
                        "documentId": "wire-post-hot-source",
                        "title": "Wire source",
                        "order": 1,
                        "updatedAt": 1,
                    }),
                    enqueue_timestamp: "1".to_string(),
                },
            )
            .await
            .expect("seed wire source");
            super::workspace_ops::apply(
                &app,
                &CrdtOperation {
                    operation_id: "seed-post-hot-wire".to_string(),
                    kind: "workspace.createWire".to_string(),
                    graph_id: graph_id.to_string(),
                    document_id: Some("wire-post-hot-source".to_string()),
                    payload: json!({
                        "wireId": wire_id,
                        "sourceDocumentId": "wire-post-hot-source",
                        "targetDocumentId": "wire-post-hot-target",
                        "predicate": "supports",
                        "updatedAt": 2,
                    }),
                    enqueue_timestamp: "2".to_string(),
                },
            )
            .await
            .expect("seed wire");
            let graph_dir = existing_graph_dir(&app, graph_id).expect("graph dir");
            let snapshot_path = crate::ydoc_paths::workspace_snapshot_path(&graph_dir);
            std::fs::remove_file(&snapshot_path).expect("remove workspace snapshot");
            std::fs::create_dir(&snapshot_path).expect("block workspace tail write");

            let operation = CrdtOperation {
                operation_id: "delete-post-hot-wire".to_string(),
                kind: "workspace.deleteWire".to_string(),
                graph_id: graph_id.to_string(),
                document_id: None,
                payload: json!({
                    "wireId": wire_id,
                    "graphIncarnation": current_graph_incarnation(&app, graph_id),
                }),
                enqueue_timestamp: "3".to_string(),
            };
            crate::crdt_operation_journal::record_crdt_operation_queued(&app, &operation)
                .expect("journal wire delete");
            app.state::<crate::crdt_queue::CrdtOperationQueue>()
                .enqueue_detached(operation.clone())
                .expect("enqueue wire delete");
            super::executor::drain_queue(app.clone()).await;
            let pending = crate::crdt_operation_journal::recover_pending_crdt_operations(&app)
                .expect("wire operation remains pending");
            assert_eq!(pending.len(), 1);
            assert_eq!(pending[0].operation_id, operation.operation_id);
            assert_eq!(
                app.state::<crate::crdt_queue::CrdtOperationQueue>()
                    .counts_for_test()
                    .expect("parked wire queue"),
                (1, 0)
            );

            std::fs::remove_dir(&snapshot_path).expect("unblock workspace tail");
            let restarted = crate::tauri_runtime::build_mock_app_for_tests(true);
            assert_eq!(
                crate::crdt_queue::recover_crdt_operations(restarted.clone())
                    .expect("recover wire delete"),
                1
            );
            super::executor::drain_queue(restarted.clone()).await;
            assert!(
                crate::crdt_operation_journal::recover_pending_crdt_operations(&restarted)
                    .expect("completed wire journal")
                    .is_empty()
            );
            let snapshot: Value = crate::storage::read_json(&snapshot_path)
                .expect("repaired wire workspace snapshot");
            assert!(snapshot["wires"].as_array().unwrap().is_empty());
        });
    });
}

#[test]
fn recovered_wire_delete_finishes_workspace_snapshot_and_rdf_tail() {
    with_profile("garden-recovered-wire-delete", || {
        crate::app_runtime::async_runtime::block_on(async {
            let app = crate::tauri_runtime::build_mock_app_for_tests(true);
            let graph_id = "recovered-wire-delete";
            let wire_id = "wire-recovery";
            create_graph_service(
                &app,
                CreateGraphInput {
                    title: "Recovered Wire Delete".to_string(),
                    graph_id: Some(graph_id.to_string()),
                    description: None,
                    operation_id: None,
                },
            )
            .expect("create graph");
            super::workspace_ops::apply(
                &app,
                &CrdtOperation {
                    operation_id: "seed-wire-source".to_string(),
                    kind: "workspace.createDocument".to_string(),
                    graph_id: graph_id.to_string(),
                    document_id: Some("wire-source".to_string()),
                    payload: json!({
                        "documentId": "wire-source",
                        "title": "Wire Source",
                        "order": 1,
                        "updatedAt": 1,
                    }),
                    enqueue_timestamp: "1".to_string(),
                },
            )
            .await
            .expect("seed wire source");
            super::workspace_ops::apply(
                &app,
                &CrdtOperation {
                    operation_id: "seed-recovered-wire".to_string(),
                    kind: "workspace.createWire".to_string(),
                    graph_id: graph_id.to_string(),
                    document_id: Some("wire-source".to_string()),
                    payload: json!({
                        "wireId": wire_id,
                        "sourceDocumentId": "wire-source",
                        "targetDocumentId": "wire-target",
                        "predicate": "supports",
                        "updatedAt": 2,
                    }),
                    enqueue_timestamp: "2".to_string(),
                },
            )
            .await
            .expect("seed wire projection");
            let graph_dir = existing_graph_dir(&app, graph_id).expect("graph dir");
            let workspace_key = format!("workspace:{graph_id}");
            let registry = app.state::<RoomRegistry>();
            let room = registry.peek(&workspace_key).await.expect("workspace room");
            room.update_doc(|_doc, txn| {
                txn.get_or_insert_map("wires").remove(txn, wire_id);
                Ok(())
            })
            .await
            .expect("persist hot-only wire removal");
            assert!(room.needs_projection_flush());
            assert_eq!(
                crate::storage::read_json::<Value>(&crate::ydoc_paths::workspace_snapshot_path(
                    &graph_dir
                ))
                .expect("stale wire snapshot")["wires"][0]["id"],
                wire_id
            );

            let pending = CrdtOperation {
                operation_id: "recover-wire-delete".to_string(),
                kind: "workspace.deleteWire".to_string(),
                graph_id: graph_id.to_string(),
                document_id: None,
                payload: json!({
                    "wireId": wire_id,
                    "graphIncarnation": current_graph_incarnation(&app, graph_id),
                }),
                enqueue_timestamp: "3".to_string(),
            };
            assert!(registry.evict_room(&workspace_key));
            let recovered = journal_and_recover_one(&app, &pending);
            let response = super::executor::apply_operation(&app, &recovered)
                .await
                .expect("recovered wire delete");
            assert_eq!(response["deleted"], true);
            let snapshot: Value =
                crate::storage::read_json(&crate::ydoc_paths::workspace_snapshot_path(&graph_dir))
                    .expect("recovered wire snapshot");
            assert!(snapshot["wires"].as_array().unwrap().is_empty());
            let projection = crate::rdf_authority::workspace_projection_graph_iri(graph_id);
            let subject =
                crate::rdf_workspace_terms::workspace_entity_subject(graph_id, "wire", wire_id);
            let store = crate::rdf_service::open_graph_store(&graph_dir).expect("graph RDF");
            let result = oxigraph::sparql::SparqlEvaluator::new()
                .parse_query(&format!(
                    "ASK WHERE {{ GRAPH <{projection}> {{ <{subject}> ?p ?o }} }}"
                ))
                .expect("parse wire ASK")
                .on_store(&store)
                .execute()
                .expect("query wire RDF");
            assert!(matches!(
                result,
                oxigraph::sparql::QueryResults::Boolean(false)
            ));

            let fresh_error = super::workspace_ops::apply(
                &app,
                &CrdtOperation {
                    operation_id: "fresh-missing-wire-delete".to_string(),
                    kind: "workspace.deleteWire".to_string(),
                    graph_id: graph_id.to_string(),
                    document_id: None,
                    payload: json!({ "wireId": wire_id }),
                    enqueue_timestamp: "4".to_string(),
                },
            )
            .await
            .expect_err("fresh missing wire remains an error");
            assert!(fresh_error.contains("wire not found"), "{fresh_error}");
        });
    });
}

#[test]
fn fresh_enqueue_strips_spoofed_recovery_marker_before_delete_dispatch() {
    with_profile("garden-recovery-marker-spoof", || {
        crate::app_runtime::async_runtime::block_on(async {
            let app = crate::tauri_runtime::build_mock_app_for_tests(true);
            let graph_id = "recovery-marker-spoof";
            create_graph_service(
                &app,
                CreateGraphInput {
                    title: "Recovery Marker Spoof".to_string(),
                    graph_id: Some(graph_id.to_string()),
                    description: None,
                    operation_id: None,
                },
            )
            .expect("create graph");
            let error = crate::crdt_queue::enqueue_crdt_operation(
                app.clone(),
                EnqueueCrdtOperationInput {
                    kind: "workspace.deleteArtifact".to_string(),
                    graph_id: graph_id.to_string(),
                    document_id: Some("never-existed".to_string()),
                    payload: json!({
                        "artifactId": "never-existed",
                        "__gardenRecoveredOperation": true,
                    }),
                },
            )
            .await
            .expect_err("fresh caller cannot spoof recovery idempotence");
            assert!(error.contains("artifact not found"), "{error}");
        });
    });
}

#[test]
fn recovered_folder_cascade_rederives_and_finishes_hard_delete_tail() {
    with_profile("garden-recovered-folder-cascade", || {
        crate::app_runtime::async_runtime::block_on(async {
            let app = crate::tauri_runtime::build_mock_app_for_tests(true);
            let graph_id = "recovered-folder-cascade";
            let root_folder = "folder-root";
            let nested_folder = "folder-nested";
            let nested_document = "document-nested";
            let root_document = "document-root";
            let hot_only_document = "document-hot-only";
            let artifact_id = "artifact-nested";
            create_graph_service(
                &app,
                CreateGraphInput {
                    title: "Recovered Folder Cascade".to_string(),
                    graph_id: Some(graph_id.to_string()),
                    description: None,
                    operation_id: None,
                },
            )
            .expect("create graph");
            for (folder_id, parent_id, order) in [
                (root_folder, Value::Null, 1),
                (nested_folder, json!(root_folder), 2),
            ] {
                super::workspace_ops::apply(
                    &app,
                    &CrdtOperation {
                        operation_id: format!("seed-{folder_id}"),
                        kind: "workspace.createFolder".to_string(),
                        graph_id: graph_id.to_string(),
                        document_id: Some(folder_id.to_string()),
                        payload: json!({
                            "folderId": folder_id,
                            "name": folder_id,
                            "parentId": parent_id,
                            "order": order,
                            "updatedAt": order,
                        }),
                        enqueue_timestamp: order.to_string(),
                    },
                )
                .await
                .expect("seed folder");
            }
            for (document_id, parent_id, text, order) in [
                (nested_document, nested_folder, "nested body", 3),
                (root_document, root_folder, "root body", 4),
            ] {
                document_write(
                    &app,
                    &CrdtOperation {
                        operation_id: format!("seed-{document_id}"),
                        kind: "document.write".to_string(),
                        graph_id: graph_id.to_string(),
                        document_id: Some(document_id.to_string()),
                        payload: json!({
                            "title": document_id,
                            "parentId": parent_id,
                            "order": order,
                            "tiptapJson": {
                                "type": "doc",
                                "content": [{
                                    "type": "paragraph",
                                    "attrs": { "data-block-id": format!("block-{document_id}") },
                                    "content": [{ "type": "text", "text": text }]
                                }]
                            }
                        }),
                        enqueue_timestamp: order.to_string(),
                    },
                )
                .await
                .expect("seed child document");
            }
            super::workspace_ops::apply(
                &app,
                &CrdtOperation {
                    operation_id: "seed-nested-artifact".to_string(),
                    kind: "workspace.putArtifact".to_string(),
                    graph_id: graph_id.to_string(),
                    document_id: Some(artifact_id.to_string()),
                    payload: json!({
                        "label": "Nested.pdf",
                        "parentId": nested_folder,
                        "originalFilename": "Nested.pdf",
                        "mimeType": "application/pdf",
                        "status": "ready",
                        "order": 5,
                        "updatedAt": 5,
                    }),
                    enqueue_timestamp: "5".to_string(),
                },
            )
            .await
            .expect("seed nested artifact");

            let graph_dir = existing_graph_dir(&app, graph_id).expect("graph dir");
            crate::document_service::create_document(
                app.clone(),
                crate::document_types::CreateDocumentInput {
                    graph_id: graph_id.to_string(),
                    title: "Hot Only Document".to_string(),
                    document_id: Some(hot_only_document.to_string()),
                },
            )
            .expect("create cold record for hot-only workspace child");
            let workspace_key = format!("workspace:{graph_id}");
            let registry = app.state::<RoomRegistry>();
            let room = registry.peek(&workspace_key).await.expect("workspace room");
            room.update_doc(|_doc, txn| {
                write_workspace_document(
                    txn,
                    &json!({
                        "documentId": hot_only_document,
                        "title": "Hot Only Document",
                        "parentId": root_folder,
                        "order": 6,
                        "updatedAt": 6,
                    }),
                )
            })
            .await
            .expect("add hot-only workspace child");
            let pending = CrdtOperation {
                operation_id: "recover-folder-cascade".to_string(),
                kind: "workspace.deleteFolder".to_string(),
                graph_id: graph_id.to_string(),
                document_id: Some(root_folder.to_string()),
                payload: json!({
                    "folderId": root_folder,
                    "cascade": true,
                    "hard": true,
                    "graphIncarnation": current_graph_incarnation(&app, graph_id),
                }),
                enqueue_timestamp: "6".to_string(),
            };
            crate::crdt_operation_journal::record_crdt_operation_queued(&app, &pending)
                .expect("journal folder cascade before apply");
            let semantic_index = crate::semantic_index_paths::semantic_index_path(&graph_dir);
            std::fs::write(&semantic_index, b"{ corrupt semantic index")
                .expect("inject hard-delete tail failure");
            let first_error = super::workspace_ops::apply(&app, &pending)
                .await
                .expect_err("fresh hard cascade fails after hot mutation");
            assert!(
                first_error.contains("semantic")
                    || first_error.contains("JSON")
                    || first_error.contains("parse"),
                "{first_error}"
            );
            assert!(room.needs_projection_flush());
            // Production's fresh hard+cascade preflush must have captured the
            // hot-only child before the failed destructive tail.
            let predelete_snapshot: Value =
                crate::storage::read_json(&crate::ydoc_paths::workspace_snapshot_path(&graph_dir))
                    .expect("predelete recovery snapshot");
            assert!(predelete_snapshot["documents"]
                .as_array()
                .unwrap()
                .iter()
                .any(|document| document["id"] == hot_only_document));
            assert!(
                !document_dir(&graph_dir, nested_document)
                    .expect("first partial-tail document")
                    .exists(),
                "the injected failure must occur after one cold child delete"
            );
            assert!(document_dir(&graph_dir, root_document)
                .expect("remaining cold document")
                .is_dir());
            std::fs::remove_file(&semantic_index).expect("repair semantic index tail");
            assert!(registry.evict_room(&workspace_key));
            let recovered = recover_one_pending(&app);
            let response = super::executor::apply_operation(&app, &recovered)
                .await
                .expect("recovered hard folder cascade");
            assert_eq!(
                response["deletedDocumentIds"],
                json!([nested_document, hot_only_document, root_document])
            );
            assert_eq!(response["deletedArtifactIds"], json!([artifact_id]));
            assert_eq!(response["deletedFolderIds"], json!([nested_folder]));

            let snapshot: Value =
                crate::storage::read_json(&crate::ydoc_paths::workspace_snapshot_path(&graph_dir))
                    .expect("recovered cascade snapshot");
            for field in ["folders", "documents", "artifacts"] {
                assert!(snapshot[field].as_array().unwrap().is_empty(), "{field}");
            }
            let store = crate::rdf_service::open_graph_store(&graph_dir).expect("graph RDF");
            for document_id in [nested_document, root_document, hot_only_document] {
                assert!(!document_dir(&graph_dir, document_id)
                    .expect("deleted document dir")
                    .exists());
                assert!(!document_ydoc_state_path(&graph_dir, document_id).exists());
                let projection =
                    crate::rdf_authority::document_projection_graph_iri(graph_id, document_id);
                let result = oxigraph::sparql::SparqlEvaluator::new()
                    .parse_query(&format!(
                        "ASK WHERE {{ GRAPH <{projection}> {{ ?s ?p ?o }} }}"
                    ))
                    .expect("parse deleted document ASK")
                    .on_store(&store)
                    .execute()
                    .expect("query deleted document projection");
                assert!(matches!(
                    result,
                    oxigraph::sparql::QueryResults::Boolean(false)
                ));
            }
            let workspace_projection =
                crate::rdf_authority::workspace_projection_graph_iri(graph_id);
            for (entity_type, entity_id) in [
                ("folder", root_folder),
                ("folder", nested_folder),
                ("artifact", artifact_id),
            ] {
                let subject = crate::rdf_workspace_terms::workspace_entity_subject(
                    graph_id,
                    entity_type,
                    entity_id,
                );
                let result = oxigraph::sparql::SparqlEvaluator::new()
                    .parse_query(&format!(
                        "ASK WHERE {{ GRAPH <{workspace_projection}> {{ <{subject}> ?p ?o }} }}"
                    ))
                    .expect("parse deleted workspace entity ASK")
                    .on_store(&store)
                    .execute()
                    .expect("query deleted workspace entity");
                assert!(matches!(
                    result,
                    oxigraph::sparql::QueryResults::Boolean(false)
                ));
            }

            // Crash-after-success can leave the journal pending even though
            // the valid cold snapshot is already post-delete. Folder absence
            // in that present snapshot is idempotent success, not a lost plan.
            let postdelete_replay = super::workspace_ops::apply(
                &app,
                &CrdtOperation {
                    operation_id: "recovered-folder-without-plan".to_string(),
                    kind: "workspace.deleteFolder".to_string(),
                    graph_id: graph_id.to_string(),
                    document_id: Some(root_folder.to_string()),
                    payload: json!({
                        "folderId": root_folder,
                        "cascade": true,
                        "hard": true,
                        "__gardenRecoveredOperation": true,
                    }),
                    enqueue_timestamp: "7".to_string(),
                },
            )
            .await
            .expect("post-delete recovered cascade is idempotent");
            assert_eq!(postdelete_replay["deletedDocumentIds"], json!([]));
            assert_eq!(postdelete_replay["deletedArtifactIds"], json!([]));
            assert_eq!(postdelete_replay["deletedFolderIds"], json!([]));
        });
    });
}

#[test]
fn unrelated_workspace_cycle_rejects_valid_subtree_delete_before_hot_commit() {
    with_profile("garden-unrelated-workspace-cycle", || {
        crate::app_runtime::async_runtime::block_on(async {
            let app = crate::tauri_runtime::build_mock_app_for_tests(true);
            let graph_id = "unrelated-workspace-cycle";
            let valid_root = "valid-root";
            let valid_child = "valid-child";
            let cycle_a = "unrelated-cycle-a";
            let cycle_b = "unrelated-cycle-b";
            create_graph_service(
                &app,
                CreateGraphInput {
                    title: "Unrelated Workspace Cycle".to_string(),
                    graph_id: Some(graph_id.to_string()),
                    description: None,
                    operation_id: None,
                },
            )
            .expect("create graph");
            for (index, (folder_id, parent_id)) in [
                (valid_root, None),
                (valid_child, Some(valid_root)),
                (cycle_a, None),
                (cycle_b, Some(cycle_a)),
            ]
            .into_iter()
            .enumerate()
            {
                super::workspace_ops::apply(
                    &app,
                    &CrdtOperation {
                        operation_id: format!("seed-unrelated-cycle-{index}"),
                        kind: "workspace.createFolder".to_string(),
                        graph_id: graph_id.to_string(),
                        document_id: Some(folder_id.to_string()),
                        payload: json!({
                            "folderId": folder_id,
                            "name": folder_id,
                            "parentId": parent_id,
                            "order": index + 1,
                            "updatedAt": index + 1,
                        }),
                        enqueue_timestamp: (index + 1).to_string(),
                    },
                )
                .await
                .expect("seed valid folder graph");
            }

            let graph_dir = existing_graph_dir(&app, graph_id).expect("graph dir");
            let snapshot_path = crate::ydoc_paths::workspace_snapshot_path(&graph_dir);
            let state_path = crate::ydoc_paths::workspace_ydoc_state_path(&graph_dir);
            let cold_before = std::fs::read(&snapshot_path).expect("valid cold snapshot");
            let room = app
                .state::<RoomRegistry>()
                .peek(&format!("workspace:{graph_id}"))
                .await
                .expect("workspace room");

            // A raw CRDT peer can bypass the structured move guard. Close a
            // cycle in a different branch while deliberately leaving the last
            // materialized snapshot as the valid recovery boundary.
            room.update_doc(|_doc, txn| {
                let folders = txn.get_or_insert_map("folders");
                let folder = match folders.get(&*txn, cycle_a) {
                    Some(Out::YMap(folder)) => folder,
                    _ => return Err("cycle folder A missing".to_string()),
                };
                folder.insert(txn, "parentId", cycle_b);
                Ok(())
            })
            .await
            .expect("persist unrelated raw cycle");
            let hot_before = room.encode_state_for_test().await;
            let sidecar_before = std::fs::read(&state_path).expect("cyclic hot sidecar");

            let error = super::workspace_ops::apply_classified(
                &app,
                &CrdtOperation {
                    operation_id: "delete-valid-subtree-beside-cycle".to_string(),
                    kind: "workspace.deleteFolder".to_string(),
                    graph_id: graph_id.to_string(),
                    document_id: Some(valid_root.to_string()),
                    payload: json!({
                        "folderId": valid_root,
                        "cascade": true,
                        "hard": false,
                    }),
                    enqueue_timestamp: "10".to_string(),
                },
            )
            .await
            .expect_err("global cycle must reject an unrelated delete");
            match error {
                ApplyOperationError::Terminal(message) => {
                    assert!(message.contains("cycle"), "unexpected error: {message}");
                }
                ApplyOperationError::RetryableAfterHotCommit(message) => {
                    panic!("cycle validation ran after hot commit: {message}");
                }
            }

            assert_eq!(room.encode_state_for_test().await, hot_before);
            assert_eq!(
                std::fs::read(&state_path).expect("hot sidecar after rejection"),
                sidecar_before
            );
            assert_eq!(
                std::fs::read(&snapshot_path).expect("cold snapshot after rejection"),
                cold_before
            );

            let verification_registry = RoomRegistry::default();
            let verification_room = verification_registry
                .get_or_create(
                    &format!("workspace:{graph_id}"),
                    crate::ydoc_paths::workspace_ydoc_state_path(&graph_dir),
                )
                .await
                .expect("reload cyclic room");
            verification_room
                .with_doc(|doc| {
                    let txn = doc.transact();
                    let folders = txn.get_map("folders").expect("folders remain");
                    for folder_id in [valid_root, valid_child, cycle_a, cycle_b] {
                        assert!(folders.contains_key(&txn, folder_id));
                    }
                })
                .await;
        });
    });
}

#[test]
fn cyclic_workspace_delete_is_terminal_fresh_and_recovered_without_partial_state() {
    with_profile("garden-cyclic-workspace-delete", || {
        crate::app_runtime::async_runtime::block_on(async {
            let app = crate::tauri_runtime::build_mock_app_for_tests(true);
            let graph_id = "cyclic-workspace-delete";
            let folder_a = "cycle-folder-a";
            let folder_b = "cycle-folder-b";
            create_graph_service(
                &app,
                CreateGraphInput {
                    title: "Cyclic Workspace Delete".to_string(),
                    graph_id: Some(graph_id.to_string()),
                    description: None,
                    operation_id: None,
                },
            )
            .expect("create graph");
            for (operation_id, folder_id, parent_id, order) in [
                ("seed-cycle-a", folder_a, None, 1),
                ("seed-cycle-b", folder_b, Some(folder_a), 2),
            ] {
                super::workspace_ops::apply(
                    &app,
                    &CrdtOperation {
                        operation_id: operation_id.to_string(),
                        kind: "workspace.createFolder".to_string(),
                        graph_id: graph_id.to_string(),
                        document_id: Some(folder_id.to_string()),
                        payload: json!({
                            "folderId": folder_id,
                            "name": folder_id,
                            "parentId": parent_id,
                            "order": order,
                            "updatedAt": order,
                        }),
                        enqueue_timestamp: order.to_string(),
                    },
                )
                .await
                .expect("seed valid folder");
            }
            let graph_dir = existing_graph_dir(&app, graph_id).expect("graph dir");
            let snapshot_path = crate::ydoc_paths::workspace_snapshot_path(&graph_dir);
            let snapshot_before = std::fs::read(&snapshot_path).expect("valid cold snapshot");

            // Model a raw sync client bypassing the structured move guard. The
            // room sidecar is authoritative and durable, while workspace.json
            // intentionally remains the last valid acyclic projection.
            let room = app
                .state::<RoomRegistry>()
                .peek(&format!("workspace:{graph_id}"))
                .await
                .expect("workspace room");
            room.update_doc(|_doc, txn| {
                let folders = txn.get_or_insert_map("folders");
                let folder = match folders.get(&*txn, folder_a) {
                    Some(Out::YMap(folder)) => folder,
                    _ => return Err("cycle folder A missing".to_string()),
                };
                folder.insert(txn, "parentId", folder_b);
                Ok(())
            })
            .await
            .expect("persist raw cyclic Y.Doc");

            let operation = |operation_id: &str| CrdtOperation {
                operation_id: operation_id.to_string(),
                kind: "workspace.deleteFolder".to_string(),
                graph_id: graph_id.to_string(),
                document_id: Some(folder_a.to_string()),
                payload: json!({
                    "folderId": folder_a,
                    "cascade": true,
                    "hard": true,
                    "graphIncarnation": current_graph_incarnation(&app, graph_id),
                }),
                enqueue_timestamp: "3".to_string(),
            };

            // Fresh production dispatch: validation/materialization fails
            // before any delete, so the journal records a terminal failure.
            let fresh = operation("fresh-cycle-delete");
            crate::crdt_operation_journal::record_crdt_operation_queued(&app, &fresh)
                .expect("journal fresh delete");
            app.state::<crate::crdt_queue::CrdtOperationQueue>()
                .enqueue_detached(fresh)
                .expect("enqueue fresh delete");
            super::executor::drain_queue(app.clone()).await;
            assert!(
                crate::crdt_operation_journal::recover_pending_crdt_operations(&app)
                    .expect("fresh pending journal")
                    .is_empty()
            );
            assert_eq!(
                std::fs::read(&snapshot_path).expect("snapshot after fresh rejection"),
                snapshot_before
            );

            // Recovered production dispatch follows the same checked path and
            // must also be terminal without blessing or deleting the cycle.
            let recovered = operation("recovered-cycle-delete");
            crate::crdt_operation_journal::record_crdt_operation_queued(&app, &recovered)
                .expect("journal recovered delete");
            let restarted = crate::tauri_runtime::build_mock_app_for_tests(true);
            assert_eq!(
                crate::crdt_queue::recover_crdt_operations(restarted.clone())
                    .expect("recover cyclic delete"),
                1
            );
            super::executor::drain_queue(restarted.clone()).await;
            assert!(
                crate::crdt_operation_journal::recover_pending_crdt_operations(&restarted)
                    .expect("recovered pending journal")
                    .is_empty()
            );
            assert_eq!(
                std::fs::read(&snapshot_path).expect("snapshot after recovered rejection"),
                snapshot_before
            );

            let verification_registry = RoomRegistry::default();
            let verification_room = verification_registry
                .get_or_create(
                    &format!("workspace:{graph_id}"),
                    crate::ydoc_paths::workspace_ydoc_state_path(&graph_dir),
                )
                .await
                .expect("reload cyclic room");
            verification_room
                .with_doc(|doc| {
                    let txn = doc.transact();
                    let folders = txn.get_map("folders").expect("folders remain");
                    assert!(folders.contains_key(&txn, folder_a));
                    assert!(folders.contains_key(&txn, folder_b));
                })
                .await;
        });
    });
}

#[test]
fn recovered_folder_cascade_refreshes_plan_before_its_first_dispatch() {
    with_profile("garden-recovered-folder-first-dispatch", || {
        crate::app_runtime::async_runtime::block_on(async {
            let seed_app = crate::tauri_runtime::build_mock_app_for_tests(true);
            let graph_id = "recovered-folder-first-dispatch";
            let folder_id = "folder-first-dispatch";
            let nested_folder_id = "folder-hot-only-child";
            let document_id = "document-first-dispatch";
            create_graph_service(
                &seed_app,
                CreateGraphInput {
                    title: "Recovered Folder First Dispatch".to_string(),
                    graph_id: Some(graph_id.to_string()),
                    description: None,
                    operation_id: None,
                },
            )
            .expect("create graph");
            super::workspace_ops::apply(
                &seed_app,
                &CrdtOperation {
                    operation_id: "seed-first-dispatch-folder".to_string(),
                    kind: "workspace.createFolder".to_string(),
                    graph_id: graph_id.to_string(),
                    document_id: Some(folder_id.to_string()),
                    payload: json!({
                        "folderId": folder_id,
                        "name": "First Dispatch Folder",
                        "order": 1,
                        "updatedAt": 1,
                    }),
                    enqueue_timestamp: "1".to_string(),
                },
            )
            .await
            .expect("seed folder");
            document_write(
                &seed_app,
                &CrdtOperation {
                    operation_id: "seed-first-dispatch-document".to_string(),
                    kind: "document.write".to_string(),
                    graph_id: graph_id.to_string(),
                    document_id: Some(document_id.to_string()),
                    payload: json!({
                        "title": "First Dispatch Document",
                        "parentId": folder_id,
                        "order": 2,
                        "tiptapJson": {
                            "type": "doc",
                            "content": [{
                                "type": "paragraph",
                                "attrs": { "data-block-id": "first-dispatch-block" },
                                "content": [{ "type": "text", "text": "recursive recovery" }]
                            }]
                        }
                    }),
                    enqueue_timestamp: "2".to_string(),
                },
            )
            .await
            .expect("seed document");

            // Leave the cold workspace snapshot deliberately stale while the
            // authoritative room sidecar contains a recursive child and the
            // document's new parent. A recovered operation whose original
            // process died before first dispatch must refresh this evidence.
            let workspace_room = seed_app
                .state::<RoomRegistry>()
                .peek(&format!("workspace:{graph_id}"))
                .await
                .expect("seed workspace room");
            workspace_room
                .update_doc(|_doc, txn| {
                    super::workspace_ops::write_workspace_folder(
                        txn,
                        &json!({
                            "folderId": nested_folder_id,
                            "name": "Hot-only nested folder",
                            "parentId": folder_id,
                            "order": 3,
                            "updatedAt": 3,
                        }),
                    )?;
                    super::workspace_ops::update_workspace_document(
                        txn,
                        document_id,
                        &json!({
                            "parentId": nested_folder_id,
                            "updatedAt": 3,
                        }),
                    )?;
                    Ok(())
                })
                .await
                .expect("persist hot-only recursive workspace state");
            let stale_snapshot: Value =
                crate::storage::read_json(&crate::ydoc_paths::workspace_snapshot_path(
                    &existing_graph_dir(&seed_app, graph_id).expect("seed graph dir"),
                ))
                .expect("stale workspace snapshot");
            assert!(!stale_snapshot["folders"]
                .as_array()
                .unwrap()
                .iter()
                .any(|folder| folder["id"] == nested_folder_id));

            let pending = CrdtOperation {
                operation_id: "recover-before-first-dispatch".to_string(),
                kind: "workspace.deleteFolder".to_string(),
                graph_id: graph_id.to_string(),
                document_id: Some(folder_id.to_string()),
                payload: json!({
                    "folderId": folder_id,
                    "cascade": true,
                    "hard": true,
                    "graphIncarnation": current_graph_incarnation(&seed_app, graph_id),
                }),
                enqueue_timestamp: "3".to_string(),
            };
            crate::crdt_operation_journal::record_crdt_operation_queued(&seed_app, &pending)
                .expect("journal before any dispatch");
            let graph_dir = existing_graph_dir(&seed_app, graph_id).expect("graph dir");
            let semantic_index = crate::semantic_index_paths::semantic_index_path(&graph_dir);
            std::fs::write(&semantic_index, b"{ corrupt semantic index")
                .expect("inject post-hot delete failure");

            // First process recovery is also the operation's first dispatch.
            // It must persist a plan before deleting the hot folder, because
            // this injected tail failure models a second crash immediately
            // after that mutation became durable.
            let first_recovery_app = crate::tauri_runtime::build_mock_app_for_tests(true);
            assert_eq!(
                crate::crdt_queue::recover_crdt_operations(first_recovery_app.clone())
                    .expect("recover before first dispatch"),
                1
            );
            super::executor::drain_queue(first_recovery_app.clone()).await;
            let retained =
                crate::crdt_operation_journal::recover_pending_crdt_operations(&first_recovery_app)
                    .expect("post-hot folder operation remains pending");
            assert_eq!(retained.len(), 1);
            assert_eq!(retained[0].operation_id, pending.operation_id);
            assert_eq!(
                first_recovery_app
                    .state::<crate::crdt_queue::CrdtOperationQueue>()
                    .counts_for_test()
                    .expect("parked folder queue"),
                (1, 0)
            );
            let plan: Value =
                crate::storage::read_json(&crate::ydoc_paths::workspace_snapshot_path(&graph_dir))
                    .expect("recovery plan refreshed before mutation");
            assert!(plan["folders"]
                .as_array()
                .unwrap()
                .iter()
                .any(|folder| folder["id"] == folder_id));
            assert!(plan["folders"]
                .as_array()
                .unwrap()
                .iter()
                .any(|folder| folder["id"] == nested_folder_id));
            assert!(plan["documents"]
                .as_array()
                .unwrap()
                .iter()
                .any(|document| {
                    document["id"] == document_id && document["parentId"] == nested_folder_id
                }));

            std::fs::remove_file(&semantic_index).expect("repair semantic index tail");
            let second_recovery_app = crate::tauri_runtime::build_mock_app_for_tests(true);
            assert_eq!(
                crate::crdt_queue::recover_crdt_operations(second_recovery_app.clone())
                    .expect("recover retained folder operation"),
                1
            );
            super::executor::drain_queue(second_recovery_app.clone()).await;
            assert!(
                crate::crdt_operation_journal::recover_pending_crdt_operations(
                    &second_recovery_app
                )
                .expect("completed folder journal")
                .is_empty()
            );

            let final_snapshot: Value =
                crate::storage::read_json(&crate::ydoc_paths::workspace_snapshot_path(&graph_dir))
                    .expect("post-delete workspace snapshot");
            assert!(final_snapshot["folders"].as_array().unwrap().is_empty());
            assert!(final_snapshot["documents"].as_array().unwrap().is_empty());
            assert!(!document_dir(&graph_dir, document_id)
                .expect("document dir")
                .exists());
        });
    });
}

#[test]
fn queued_document_delete_evicts_live_room_and_cancels_zombie_flush() {
    with_profile("garden-document-delete-room-eviction", || {
        crate::app_runtime::async_runtime::block_on(async {
            let app = crate::tauri_runtime::build_mock_app_for_tests(true);
            let graph_id = "document-delete-room-eviction";
            let document_id = "live-delete-document";
            create_graph_service(
                &app,
                CreateGraphInput {
                    title: "Document Delete Room Eviction".to_string(),
                    graph_id: Some(graph_id.to_string()),
                    description: None,
                    operation_id: None,
                },
            )
            .expect("create graph");
            document_write(
                &app,
                &CrdtOperation {
                    operation_id: "seed-live-delete-document".to_string(),
                    kind: "document.write".to_string(),
                    graph_id: graph_id.to_string(),
                    document_id: Some(document_id.to_string()),
                    payload: json!({
                        "title": "Live Delete Document",
                        "tiptapJson": {
                            "type": "doc",
                            "content": [{
                                "type": "paragraph",
                                "attrs": { "data-block-id": "seed-delete-block" },
                                "content": [{ "type": "text", "text": "seeded for delete" }]
                            }]
                        }
                    }),
                    enqueue_timestamp: "1".to_string(),
                },
            )
            .await
            .expect("seed live and cold document");

            let graph_dir = existing_graph_dir(&app, graph_id).expect("graph dir");
            let document_dir = document_dir(&graph_dir, document_id).expect("document dir");
            let state_path = document_ydoc_state_path(&graph_dir, document_id);
            let registry = app.state::<RoomRegistry>();
            let room = registry
                .peek(&format!("doc:{graph_id}:{document_id}"))
                .await
                .expect("live document room");
            room.configure_projection_flush(graph_id.to_string(), Some(document_id.to_string()))
                .expect("configure pending debounce");
            replace_room_text(&room, "zombie-delete-block", "must never resurrect").await;
            room.schedule_projection_flush(app.clone());
            assert!(document_dir.join("document.json").is_file());
            assert!(state_path.is_file());

            crate::crdt_queue::enqueue_crdt_operation(
                app.clone(),
                EnqueueCrdtOperationInput {
                    kind: "workspace.deleteDocument".to_string(),
                    graph_id: graph_id.to_string(),
                    document_id: Some(document_id.to_string()),
                    payload: json!({ "documentId": document_id }),
                },
            )
            .await
            .expect("queued document delete");

            assert!(registry
                .peek(&format!("doc:{graph_id}:{document_id}"))
                .await
                .is_none());
            let update_error = room
                .update_doc(|_doc, _txn| Ok(()))
                .await
                .expect_err("extant room Arc rejects updates after delete");
            assert!(update_error.contains("room was evicted"), "{update_error}");
            assert!(!document_dir.exists());
            assert!(!state_path.exists());

            // Wait beyond the document debounce. Its eviction signal must stop
            // the scheduled generation without queueing a flush that recreates
            // the deleted document from the still-held room Arc.
            tokio::time::sleep(std::time::Duration::from_millis(900)).await;
            assert!(!document_dir.exists());
            assert!(!state_path.exists());
            assert!(crate::document_service::read_document(
                app.clone(),
                graph_id.to_string(),
                document_id.to_string(),
            )
            .is_err());
            let document_projection =
                crate::rdf_authority::document_projection_graph_iri(graph_id, document_id);
            let store = crate::rdf_service::open_graph_store(&graph_dir).expect("graph RDF");
            let projected = oxigraph::sparql::SparqlEvaluator::new()
                .parse_query(&format!(
                    "ASK WHERE {{ GRAPH <{document_projection}> {{ ?s ?p ?o }} }}"
                ))
                .expect("parse deleted projection ASK")
                .on_store(&store)
                .execute()
                .expect("query deleted projection");
            assert!(matches!(
                projected,
                oxigraph::sparql::QueryResults::Boolean(false)
            ));
            let zombie_flushes = app
                .state::<crate::crdt_queue::CrdtOperationQueue>()
                .recent_traces(Some(20), false, Some("crdt.flush"), Some(document_id), None)
                .expect("zombie flush traces");
            assert!(zombie_flushes.is_empty());
        });
    });
}

#[test]
fn failed_delete_cannot_resurrect_and_same_id_recreation_clears_only_after_workspace_commit() {
    with_profile("garden-document-delete-tombstone-recreation", || {
        crate::app_runtime::async_runtime::block_on(async {
            let app = crate::tauri_runtime::build_mock_app_for_tests(true);
            let graph_id = "document-delete-tombstone-recreation";
            let document_id = "recreated-document";
            create_graph_service(
                &app,
                CreateGraphInput {
                    title: "Document Tombstone Recreation".to_string(),
                    graph_id: Some(graph_id.to_string()),
                    description: None,
                    operation_id: None,
                },
            )
            .expect("create graph");

            document_write(
                &app,
                &CrdtOperation {
                    operation_id: "seed-deleted-document".to_string(),
                    kind: "document.write".to_string(),
                    graph_id: graph_id.to_string(),
                    document_id: Some(document_id.to_string()),
                    payload: json!({
                        "title": "Deleted incarnation",
                        "tiptapJson": {
                            "type": "doc",
                            "content": [{
                                "type": "paragraph",
                                "attrs": { "data-block-id": "stale-block" },
                                "content": [{ "type": "text", "text": "stale deleted sentinel" }]
                            }]
                        }
                    }),
                    enqueue_timestamp: "1".to_string(),
                },
            )
            .await
            .expect("seed document");

            let graph_dir = existing_graph_dir(&app, graph_id).expect("graph dir");
            let manifest = document_dir(&graph_dir, document_id)
                .expect("document dir")
                .join("document.json");
            let state_path = document_ydoc_state_path(&graph_dir, document_id);
            assert!(manifest.is_file());
            assert!(state_path.is_file());

            crate::document_delete_service::fail_next_document_delete_after_tombstone_for_test(
                graph_id,
                document_id,
            );
            let delete_error = super::workspace_ops::apply(
                &app,
                &CrdtOperation {
                    operation_id: "delete-with-interrupted-cleanup".to_string(),
                    kind: "workspace.deleteDocument".to_string(),
                    graph_id: graph_id.to_string(),
                    document_id: Some(document_id.to_string()),
                    payload: json!({ "documentId": document_id }),
                    enqueue_timestamp: "2".to_string(),
                },
            )
            .await
            .expect_err("injected delete cleanup failure");
            assert!(delete_error.contains("cleanup failure after tombstone"));

            let deletion =
                crate::document_tombstone_store::read_document_tombstone(&graph_dir, document_id)
                    .expect("read tombstone")
                    .expect("delete committed tombstone");
            assert!(manifest.is_file(), "injected failure leaves stale manifest");
            assert!(
                state_path.is_file(),
                "injected failure leaves stale sidecar"
            );
            let registry = app.state::<RoomRegistry>();
            assert!(registry
                .peek(&format!("doc:{graph_id}:{document_id}"))
                .await
                .is_none());
            assert!(
                !crate::loopback_hocuspocus_routes::document_exists_for_room_connection(
                    &registry,
                    graph_id,
                    &graph_dir,
                    document_id,
                )
                .await
                .expect("hocuspocus authority check")
            );
            let restarted_registry = RoomRegistry::default();
            let reconnect_error = match restarted_registry
                .get_or_create(&format!("doc:{graph_id}:{document_id}"), state_path.clone())
                .await
            {
                Ok(_) => panic!("stale sidecar must not reconnect"),
                Err(error) => error,
            };
            assert!(
                reconnect_error.contains("document tombstoned"),
                "{reconnect_error}"
            );

            let recreation = CrdtOperation {
                operation_id: "recreate-same-document-id".to_string(),
                kind: "document.write".to_string(),
                graph_id: graph_id.to_string(),
                document_id: Some(document_id.to_string()),
                payload: json!({
                    "title": "Fresh incarnation",
                    "tiptapJson": {
                        "type": "doc",
                        "content": [{
                            "type": "paragraph",
                            "attrs": { "data-block-id": "fresh-block" },
                            "content": [{ "type": "text", "text": "fresh recreated authority" }]
                        }]
                    }
                }),
                enqueue_timestamp: "3".to_string(),
            };
            super::document_ops::fail_next_document_recreation_before_workspace_for_test(
                &recreation.operation_id,
            );
            let partial = document_write_classified(&app, &recreation)
                .await
                .expect_err("inject recreation failure before workspace commit");
            assert!(matches!(
                partial,
                ApplyOperationError::RetryableAfterHotCommit(_)
            ));
            assert_eq!(
                crate::document_tombstone_store::read_document_tombstone(&graph_dir, document_id,)
                    .expect("read retained tombstone")
                    .expect("tombstone retained until workspace commit")
                    .deletion_id,
                deletion.deletion_id
            );
            let partial_record: Value =
                crate::storage::read_json(&manifest).expect("fresh document projection committed");
            assert_eq!(partial_record["body"], "fresh recreated authority");
            assert!(registry
                .peek(&format!("doc:{graph_id}:{document_id}"))
                .await
                .is_none());
            assert!(
                !crate::loopback_hocuspocus_routes::document_exists_for_room_connection(
                    &registry,
                    graph_id,
                    &graph_dir,
                    document_id,
                )
                .await
                .expect("partial recreation remains hidden")
            );

            document_write(&app, &recreation)
                .await
                .expect("retry completes same-ID recreation");
            assert!(
                crate::document_tombstone_store::read_document_tombstone(&graph_dir, document_id,)
                    .expect("read cleared tombstone")
                    .is_none(),
                "marker clears only after document and workspace authorities commit"
            );
            let record = crate::document_service::read_document(
                app.clone(),
                graph_id.to_string(),
                document_id.to_string(),
            )
            .expect("read recreated document");
            assert_eq!(record.title, "Fresh incarnation");
            assert_eq!(record.body, "fresh recreated authority");
            assert!(!record.body.contains("stale deleted sentinel"));
            assert!(
                crate::loopback_hocuspocus_routes::document_exists_for_room_connection(
                    &registry,
                    graph_id,
                    &graph_dir,
                    document_id,
                )
                .await
                .expect("recreated hocuspocus authority")
            );
        });
    });
}

#[test]
fn older_workspace_snapshot_cannot_finish_after_newer_projection() {
    with_profile("garden-workspace-projection-overlap", || {
        crate::app_runtime::async_runtime::block_on(async {
            let app = crate::tauri_runtime::build_mock_app_for_tests(true);
            let graph_id = "workspace-overlap";
            create_graph_service(
                &app,
                CreateGraphInput {
                    title: "Workspace Overlap".to_string(),
                    graph_id: Some(graph_id.to_string()),
                    description: None,
                    operation_id: None,
                },
            )
            .expect("create graph");
            let graph_dir = existing_graph_dir(&app, graph_id).expect("graph dir");
            let room = app
                .state::<RoomRegistry>()
                .get_or_create(
                    &format!("workspace:{graph_id}"),
                    crate::ydoc_paths::workspace_ydoc_state_path(&graph_dir),
                )
                .await
                .expect("workspace room");
            room.update_doc(|_doc, txn| {
                write_workspace_document(
                    txn,
                    &json!({
                        "documentId": "doc-overlap",
                        "title": "Old title",
                        "order": 1,
                        "updatedAt": 1,
                    }),
                )
            })
            .await
            .expect("old workspace");

            let old_guard = room.lock_projection_flush().await;
            let (old_epoch, old_snapshot) = materialize_workspace(&room, graph_id)
                .await
                .expect("materialize old workspace");
            room.update_doc(|_doc, txn| {
                update_workspace_document(
                    txn,
                    "doc-overlap",
                    &json!({ "title": "New title", "updatedAt": 2 }),
                )
            })
            .await
            .expect("new workspace");

            let (started_tx, started_rx) = tokio::sync::oneshot::channel();
            let app_for_new = app.clone();
            let room_for_new = room.clone();
            let graph_dir_for_new = graph_dir.clone();
            let newer = crate::app_runtime::async_runtime::spawn(async move {
                let _ = started_tx.send(());
                persist_workspace(
                    &app_for_new,
                    graph_id,
                    &graph_dir_for_new,
                    &room_for_new,
                    "new-workspace-flush",
                )
                .await
            });
            started_rx.await.expect("new workspace persistence started");
            persist_materialized_workspace(graph_id, &graph_dir, &old_snapshot)
                .expect("persist old workspace");
            room.mark_projection_persisted(old_epoch);
            assert!(room.needs_projection_flush());
            drop(old_guard);
            assert!(newer.await.expect("new task").expect("new persistence"));
            assert!(!room.needs_projection_flush());
            let snapshot: Value =
                crate::storage::read_json(&crate::ydoc_paths::workspace_snapshot_path(&graph_dir))
                    .expect("final workspace snapshot");
            assert_eq!(snapshot["documents"][0]["title"], "New title");
        });
    });
}

#[test]
fn folder_only_workspace_flush_bumps_graph_content_revision() {
    with_profile("garden-workspace-folder-revision", || {
        crate::app_runtime::async_runtime::block_on(async {
            let app = crate::tauri_runtime::build_mock_app_for_tests(true);
            let graph_id = "workspace-folder-revision";
            create_graph_service(
                &app,
                CreateGraphInput {
                    title: "Workspace Folder Revision".to_string(),
                    graph_id: Some(graph_id.to_string()),
                    description: None,
                    operation_id: None,
                },
            )
            .expect("create graph");
            let (graph_dir, before) =
                crate::graph_record_store::read_graph_record_no_heal(&app, graph_id)
                    .expect("graph before folder flush");
            assert!(before.content_revision.is_none());
            let room = app
                .state::<RoomRegistry>()
                .get_or_create(
                    &format!("workspace:{graph_id}"),
                    crate::ydoc_paths::workspace_ydoc_state_path(&graph_dir),
                )
                .await
                .expect("workspace room");
            room.update_doc(|_doc, txn| {
                write_workspace_folder(
                    txn,
                    &json!({
                        "folderId": "folder-only",
                        "title": "Folder Only",
                        "order": 1,
                        "updatedAt": 1,
                    }),
                )
            })
            .await
            .expect("folder websocket update");
            assert!(
                persist_workspace(&app, graph_id, &graph_dir, &room, "folder-only-flush")
                    .await
                    .expect("folder-only workspace flush")
            );
            let (_, after) = crate::graph_record_store::read_graph_record_no_heal(&app, graph_id)
                .expect("graph after folder flush");
            assert!(after.content_revision.is_some());
            let snapshot: Value =
                crate::storage::read_json(&crate::ydoc_paths::workspace_snapshot_path(&graph_dir))
                    .expect("folder-only snapshot");
            assert_eq!(snapshot["counts"]["folders"], 1);
            assert_eq!(snapshot["counts"]["documents"], 0);
        });
    });
}

#[test]
fn workspace_rename_updates_document_record_and_workspace_rdf() {
    with_profile("garden-workspace-title-sync", || {
        crate::app_runtime::async_runtime::block_on(async {
            let app = crate::tauri_runtime::build_mock_app_for_tests(true);
            let graph_id = "workspace-title-sync";
            let document_id = "document-title-sync";
            create_graph_service(
                &app,
                CreateGraphInput {
                    title: "Workspace Title Sync".to_string(),
                    graph_id: Some(graph_id.to_string()),
                    description: None,
                    operation_id: None,
                },
            )
            .expect("create graph");
            crate::document_service::create_document(
                app.clone(),
                crate::document_types::CreateDocumentInput {
                    graph_id: graph_id.to_string(),
                    title: "Old title".to_string(),
                    document_id: Some(document_id.to_string()),
                },
            )
            .expect("create document");
            let graph_dir = existing_graph_dir(&app, graph_id).expect("graph dir");
            let room = app
                .state::<RoomRegistry>()
                .get_or_create(
                    &format!("workspace:{graph_id}"),
                    crate::ydoc_paths::workspace_ydoc_state_path(&graph_dir),
                )
                .await
                .expect("workspace room");
            room.update_doc(|_doc, txn| {
                write_workspace_document(
                    txn,
                    &json!({
                        "documentId": document_id,
                        "title": "Old title",
                        "order": 1,
                        "updatedAt": 1,
                    }),
                )
            })
            .await
            .expect("seed workspace");
            persist_workspace(&app, graph_id, &graph_dir, &room, "seed-workspace")
                .await
                .expect("seed projection");
            room.update_doc(|_doc, txn| {
                update_workspace_document(
                    txn,
                    document_id,
                    &json!({ "title": "New title", "updatedAt": 2 }),
                )
            })
            .await
            .expect("rename workspace");
            persist_workspace(&app, graph_id, &graph_dir, &room, "rename-workspace")
                .await
                .expect("persist rename");

            let record = crate::document_service::read_document(
                app.clone(),
                graph_id.to_string(),
                document_id.to_string(),
            )
            .expect("renamed document");
            assert_eq!(record.title, "New title");
            assert_eq!(record.revision, 1);
            let store = crate::rdf_service::open_graph_store(&graph_dir).expect("graph RDF");
            // Titles are workspace-owned RDF. The document projection owns
            // content/storage predicates but deliberately not dcterms:title.
            let projection = crate::rdf_authority::workspace_projection_graph_iri(graph_id);
            let subject = crate::rdf::document_subject(document_id);
            let result = oxigraph::sparql::SparqlEvaluator::new()
                .parse_query(&format!(
                    "SELECT ?title WHERE {{ GRAPH <{projection}> {{ \
                     <{subject}> <http://purl.org/dc/terms/title> ?title }} }}"
                ))
                .expect("parse title query")
                .on_store(&store)
                .execute()
                .expect("query title");
            let oxigraph::sparql::QueryResults::Solutions(mut solutions) = result else {
                panic!("title query did not return solutions");
            };
            assert_eq!(
                solutions
                    .next()
                    .expect("title row")
                    .expect("title solution")
                    .get("title")
                    .expect("title binding")
                    .to_string(),
                "\"New title\""
            );
        });
    });
}
