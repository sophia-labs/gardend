use super::*;
use crate::cell_graph_boundary::{CellGraphBoundary, CellRole};
use crate::graph_service::{create_graph_service, CreateGraphInput};
use yrs::WriteTxn;

fn read_only_state(
    app: &AppHandle,
    dir: &Path,
) -> std::sync::Arc<crate::loopback_state::LoopbackState> {
    use crate::loopback_state::{LoopbackManifest, LoopbackState};
    use std::sync::Arc;
    Arc::new(LoopbackState {
        app: app.clone(),
        token: "html-synthetic-token".into(),
        manifest: LoopbackManifest {
            runtime_profile: "test",
            bind_host: "127.0.0.1",
            port: 0,
            api_url: String::new(),
            mcp_url: String::new(),
            openapi_url: String::new(),
            token: "html-synthetic-token".into(),
            pid: 0,
            started_at: String::new(),
            manifest_path: String::new(),
            auth_header: "Authorization",
            token_audience: "test",
            token_storage: "memory",
            security_warning: "synthetic only",
            cell_graph_id: None,
            cell_owner: None,
            cell_generation: None,
            cell_registry_revision: None,
            capabilities: vec![],
            token_scope_mode: "session-all",
            token_scopes: vec!["artifacts.read"],
            scope_details: vec![],
            grant_profiles: vec![],
        },
        jobs: Arc::new(
            crate::local_jobs::LocalJobRegistry::new(dir.join("html-test-jobs")).unwrap(),
        ),
        services: Arc::new(crate::local_service_host::LocalServiceHost::default()),
        lifecycle: app
            .state::<Arc<crate::cell_lifecycle::CellLifecycle>>()
            .inner()
            .clone(),
        cell_graph: Arc::new(CellGraphBoundary::for_test(None)),
    })
}

fn create(inc: &str, id: &str, op: &str, text: &str) -> TextMutation {
    TextMutation {
        graph_incarnation: inc.into(),
        artifact_id: id.into(),
        operation_id: op.into(),
        mode: "create".into(),
        expected_content_sha256: None,
        text: Some(text.into()),
        filename: Some("live.html".into()),
        mime_type: Some("text/html".into()),
        parent_id: None,
        revision_id: None,
    }
}
fn edit(inc: &str, id: &str, op: &str, expected: &str, text: &str) -> TextMutation {
    TextMutation {
        graph_incarnation: inc.into(),
        artifact_id: id.into(),
        operation_id: op.into(),
        mode: "write".into(),
        expected_content_sha256: Some(expected.into()),
        text: Some(text.into()),
        filename: None,
        mime_type: None,
        parent_id: None,
        revision_id: None,
    }
}
fn profile_test(run: impl FnOnce(AppHandle, String, String) + std::panic::UnwindSafe) {
    let _serial = crate::tauri_runtime::profile_env_serial()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let saved = std::env::var_os("GARDEN_PROFILE_DIR");
    let profile = std::env::temp_dir().join(format!("garden-html-native-{}", uuid::Uuid::new_v4()));
    std::env::set_var("GARDEN_PROFILE_DIR", &profile);
    let result = std::panic::catch_unwind(|| {
        let app = crate::tauri_runtime::build_mock_app_for_tests(true);
        let graph = "html-native".to_string();
        create_graph_service(
            &app,
            CreateGraphInput {
                graph_id: Some(graph.clone()),
                title: "Synthetic HTML".into(),
                description: None,
                operation_id: None,
            },
        )
        .unwrap();
        let inc = crate::graph_record_store::read_graph_record_no_heal(&app, &graph)
            .unwrap()
            .1
            .incarnation_id
            .unwrap();
        println!("HTML_SYNTHETIC_PROFILE={}", profile.display());
        run(app, graph, inc);
    });
    if let Some(saved) = saved {
        std::env::set_var("GARDEN_PROFILE_DIR", saved);
    } else {
        std::env::remove_var("GARDEN_PROFILE_DIR");
    }
    if let Err(e) = result {
        std::panic::resume_unwind(e)
    }
}

#[test]
fn artifact_text_strict_utf8_bounds_and_surface_closure() {
    assert_eq!(strict_text(Vec::new(), "text/html").unwrap(), "");
    assert_eq!(
        strict_text("é🌱\r\n".as_bytes().to_vec(), "text/plain").unwrap(),
        "é🌱\r\n"
    );
    assert!(strict_text(vec![0xff], "text/html").is_err());
    assert!(strict_text(vec![b'x'; MAX_TEXT_BYTES + 1], "text/html").is_err());
    assert_eq!(
        strict_text(vec![b'x'; MAX_TEXT_BYTES], "text/html")
            .unwrap()
            .len(),
        MAX_TEXT_BYTES
    );
    assert!(strict_text(b"x".to_vec(), "application/xhtml+xml").is_err());
    let cell = CellGraphBoundary::for_test(Some("html-native"));
    for tool in ["create_artifact_text", "write_artifact_text"] {
        assert!(!cell.mcp_tool_visible_for_role(tool, CellRole::Viewer));
        assert!(cell.mcp_tool_visible_for_role(tool, CellRole::Editor));
        assert!(cell.authorize_mcp_role(tool, CellRole::Viewer).is_err());
        assert!(crate::mcp_dispatch_registry::lookup(tool).is_some());
        assert_eq!(
            crate::loopback_scopes::mcp_tool_scopes(tool),
            vec!["artifacts.write", "workspace.write.crdt"]
        );
    }
    assert!(cell.mcp_tool_visible_for_role("read_artifact", CellRole::Viewer));
    assert!(cell
        .authorize_crdt_operation("artifact.mutateText", "foreign", &json!({}))
        .is_err());
    let catalog: Value = serde_json::from_str(include_str!("mcp_tool_catalog.json")).unwrap();
    assert_eq!(
        catalog["tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|t| t["name"] == "create_artifact_text")
            .count(),
        1
    );
    assert!(crate::emporium::vocabs::WORKSPACE_GOLDEN_JSON.contains("mdoc:contentHashSha256"));
}

#[test]
fn artifact_text_real_queue_history_cas_restore_and_workspace_notification() {
    profile_test(|app, graph, inc| {
        crate::app_runtime::async_runtime::block_on(async {
            let input = create(&inc, "live-artifact", "create-live", "<p>é🌱</p>\r\n");
            let first = submit(app.clone(), graph.clone(), input.clone())
                .await
                .unwrap();
            let first_revision = first["revisionId"].as_str().unwrap().to_string();
            let original = read_text(&app, &graph, "live-artifact").await.unwrap();
            assert_eq!(original["text"], "<p>é🌱</p>\r\n");
            assert_eq!(original["sizeBytes"], "<p>é🌱</p>\r\n".len());
            let first_hash = original["contentHashSha256"].as_str().unwrap().to_string();
            assert_eq!(first_hash, hash("<p>é🌱</p>\r\n".as_bytes()));
            assert_eq!(
                submit(app.clone(), graph.clone(), input.clone())
                    .await
                    .unwrap()["replayed"],
                true
            );
            let mut changed = input.clone();
            changed.text = Some("different".into());
            assert!(submit(app.clone(), graph.clone(), changed)
                .await
                .unwrap_err()
                .message_ref()
                .contains("operation_id"));
            let mut stale_life = input.clone();
            stale_life.operation_id = "stale-life".into();
            stale_life.graph_incarnation = "another-life".into();
            assert!(submit(app.clone(), graph.clone(), stale_life)
                .await
                .is_err());
            assert!(submit(app.clone(), "missing-graph".into(), input.clone())
                .await
                .is_err());
            assert!(
                crate::graph_record_store::read_graph_record_no_heal(&app, "missing-graph")
                    .is_err()
            );
            let dir = crate::paths::existing_graph_dir(&app, &graph).unwrap();
            let state = read_only_state(&app, &dir);
            let mut headers = axum::http::HeaderMap::new();
            headers.insert(
                "authorization",
                axum::http::HeaderValue::from_static("Bearer html-synthetic-token"),
            );
            let response = crate::loopback_artifact_routes::loopback_read_artifact_text(
                axum::extract::State(state.clone()),
                headers.clone(),
                axum::extract::Path((graph.clone(), "live-artifact".into())),
            )
            .await;
            assert_eq!(response.status(), axum::http::StatusCode::OK);
            assert_eq!(response.headers().get("cache-control").unwrap(), "no-store");
            let wire: Value = serde_json::from_slice(
                &axum::body::to_bytes(response.into_body(), MAX_TEXT_BYTES + 4096)
                    .await
                    .unwrap(),
            )
            .unwrap();
            assert_eq!(wire, original);
            let refused = crate::loopback_artifact_routes::loopback_write_artifact_text(
                axum::extract::State(state),
                headers,
                axum::extract::Path((graph.clone(), "live-artifact".into())),
                axum::Json(json!({})),
            )
            .await;
            assert_eq!(refused.status(), axum::http::StatusCode::FORBIDDEN);
            let room = workspace_ops::workspace_room(&app, &graph, &dir)
                .await
                .unwrap();
            room.update_doc(|_, txn| {
                let artifacts = txn.get_or_insert_map("artifacts");
                let Some(Out::YMap(map)) = artifacts.get(&*txn, "live-artifact") else {
                    panic!("artifact map")
                };
                map.insert(txn, "name", "My renamed artifact");
                map.insert(txn, "parentId", "folder-keep");
                map.insert(txn, "order", Any::Number(17.0));
                map.insert(txn, "custom", "keep");
                Ok(())
            })
            .await
            .unwrap();
            let mut observer = room.subscribe_updates_for_test();
            let before_history =
                crate::artifact_revisions::list_artifact_revisions(&app, &graph, "live-artifact")
                    .unwrap()
                    .len();
            let noop = submit(
                app.clone(),
                graph.clone(),
                edit(&inc, "live-artifact", "noop", &first_hash, "<p>é🌱</p>\r\n"),
            )
            .await
            .unwrap();
            assert_eq!(noop["noop"], true);
            assert!(observer.try_recv().is_err());
            assert_eq!(
                crate::artifact_revisions::list_artifact_revisions(&app, &graph, "live-artifact")
                    .unwrap()
                    .len(),
                before_history
            );
            let (left, right) = tokio::join!(
                submit(
                    app.clone(),
                    graph.clone(),
                    edit(&inc, "live-artifact", "left", &first_hash, "left")
                ),
                submit(
                    app.clone(),
                    graph.clone(),
                    edit(&inc, "live-artifact", "right", &first_hash, "right")
                )
            );
            assert_eq!(usize::from(left.is_ok()) + usize::from(right.is_ok()), 1);
            assert!(observer.try_recv().is_ok());
            assert!(observer.try_recv().is_err());
            let current = read_text(&app, &graph, "live-artifact").await.unwrap();
            let current_hash = current["contentHashSha256"].as_str().unwrap().to_string();
            room.with_doc(|doc| {
                let txn = doc.transact();
                let maps = txn.get_map("artifacts").unwrap();
                let Some(Out::YMap(m)) = maps.get(&txn, "live-artifact") else {
                    panic!()
                };
                assert_eq!(
                    m.get(&txn, "name").unwrap().to_string(&txn),
                    "My renamed artifact"
                );
                assert_eq!(
                    m.get(&txn, "parentId").unwrap().to_string(&txn),
                    "folder-keep"
                );
                assert_eq!(m.get(&txn, "order").unwrap().to_string(&txn), "17");
                assert_eq!(m.get(&txn, "custom").unwrap().to_string(&txn), "keep");
                assert_eq!(
                    m.get(&txn, "contentHashSha256").unwrap().to_string(&txn),
                    current_hash
                );
            })
            .await;
            let index_before = crate::storage::read_bytes(
                &dir.join("artifacts/live-artifact/revisions/index.json"),
            )
            .unwrap();
            assert!(submit(
                app.clone(),
                graph.clone(),
                edit(&inc, "live-artifact", "stale", &first_hash, "bad overwrite")
            )
            .await
            .is_err());
            assert_eq!(
                crate::storage::read_bytes(
                    &dir.join("artifacts/live-artifact/revisions/index.json")
                )
                .unwrap(),
                index_before
            );
            assert_eq!(
                read_text(&app, &graph, "live-artifact").await.unwrap(),
                current
            );
            assert!(crate::original_file_service::save_artifact_original_file(
                &app,
                &graph,
                "live-artifact",
                "x.html",
                "text/html",
                "eA=="
            )
            .is_err());
            assert!(crate::artifact_revisions::create_artifact_revision(
                &app,
                &graph,
                "live-artifact",
                "x.html",
                "text/html",
                "eA==",
                None
            )
            .is_err());
            assert!(crate::artifact_revisions::restore_artifact_revision(
                &app,
                &graph,
                "live-artifact",
                &first_revision
            )
            .is_err());
            assert_eq!(
                crate::storage::read_bytes(
                    &dir.join("artifacts/live-artifact/revisions/index.json")
                )
                .unwrap(),
                index_before
            );
            let mut restore = edit(&inc, "live-artifact", "bad-restore", &current_hash, "");
            restore.mode = "restore".into();
            restore.text = None;
            restore.revision_id = Some("missing-revision".into());
            assert!(submit(app.clone(), graph.clone(), restore.clone())
                .await
                .is_err());
            assert_eq!(
                crate::storage::read_bytes(
                    &dir.join("artifacts/live-artifact/revisions/index.json")
                )
                .unwrap(),
                index_before
            );
            restore.operation_id = "good-restore".into();
            restore.revision_id = Some(first_revision);
            let restored = submit(app.clone(), graph.clone(), restore).await.unwrap();
            assert_eq!(restored["contentHashSha256"], first_hash);
            assert_eq!(
                read_text(&app, &graph, "live-artifact").await.unwrap()["text"],
                original["text"]
            );
            let empty = submit(
                app.clone(),
                graph.clone(),
                edit(&inc, "live-artifact", "empty", &first_hash, ""),
            )
            .await
            .unwrap();
            assert_eq!(empty["contentHashSha256"], hash(b""));
            assert_eq!(
                read_text(&app, &graph, "live-artifact").await.unwrap()["text"],
                ""
            );
            let store = crate::rdf_store_service::open_graph_store(&dir).unwrap();
            let rows=crate::rdf_query_service::execute_sparql_query(&store,"SELECT ?hash WHERE { GRAPH ?g { ?a <http://mnemosyne.dev/doc#contentHashSha256> ?hash } }").unwrap();
            assert!(!rows.rows.is_empty());
            println!(
                "HTML_JOINED_WITNESS={}",
                read_text(&app, &graph, "live-artifact").await.unwrap()
            );
        })
    });
}

#[test]
fn artifact_text_real_journal_restart_completes_only_the_retained_tail() {
    profile_test(|app, graph, inc| {
        crate::app_runtime::async_runtime::block_on(async {
            submit(
                app.clone(),
                graph.clone(),
                create(&inc, "restart-artifact", "create-restart", "before"),
            )
            .await
            .unwrap();
            let input = edit(
                &inc,
                "restart-artifact",
                "restart-edit",
                &hash(b"before"),
                "after restart",
            );
            let operation = CrdtOperation {
                operation_id: "server-restart-test".into(),
                kind: "artifact.mutateText".into(),
                graph_id: graph.clone(),
                document_id: Some("restart-artifact".into()),
                payload: serde_json::to_value(&input).unwrap(),
                enqueue_timestamp: crate::clock::timestamp(),
            };
            crate::crdt_operation_journal::record_crdt_operation_queued(&app, &operation).unwrap();
            *FAIL_TAIL.lock().unwrap() = Some("restart-edit".into());
            {
                let coordinator = app.state::<GraphPersistenceCoordinator>();
                let _lease = coordinator.acquire_hot_write(&graph).await.unwrap();
                assert!(matches!(
                    apply(&app, &operation).await,
                    Err(ApplyOperationError::RetryableAfterHotCommit(_))
                ));
            }
            assert!(read_text(&app, &graph, "restart-artifact").await.is_err());
            let restarted = crate::tauri_runtime::build_mock_app_for_tests(true);
            assert_eq!(
                crate::crdt_queue::recover_crdt_operations(restarted.clone()).unwrap(),
                1
            );
            crate::crdt_engine::executor::drain_queue(restarted.clone()).await;
            let actual = read_text(&restarted, &graph, "restart-artifact")
                .await
                .unwrap();
            assert_eq!(actual["text"], "after restart");
            assert_eq!(
                crate::artifact_revisions::list_artifact_revisions(
                    &restarted,
                    &graph,
                    "restart-artifact"
                )
                .unwrap()
                .len(),
                3
            );
            assert!(
                crate::crdt_operation_journal::recover_pending_crdt_operations(&restarted)
                    .unwrap()
                    .is_empty()
            );
            assert_eq!(
                submit(restarted.clone(), graph.clone(), input)
                    .await
                    .unwrap()["replayed"],
                true
            );
            println!("HTML_RESTART_WITNESS={actual}");
        })
    });
}

#[test]
fn artifact_text_committed_tail_retry_and_pending_owner_are_recoverable() {
    profile_test(|app, graph, inc| {
        crate::app_runtime::async_runtime::block_on(async {
            submit(
                app.clone(),
                graph.clone(),
                create(&inc, "tail-artifact", "create-tail", "before"),
            )
            .await
            .unwrap();
            let before = hash(b"before");
            let input = edit(&inc, "tail-artifact", "tail-edit", &before, "after");
            *FAIL_TAIL.lock().unwrap() = Some("tail-edit".into());
            // The real executor retains and retries this injected post-byte failure.
            let result = submit(app.clone(), graph.clone(), input.clone())
                .await
                .unwrap();
            assert_eq!(result["contentHashSha256"], hash(b"after"));
            assert!(FAIL_TAIL.lock().unwrap().is_none());
            assert_eq!(
                read_text(&app, &graph, "tail-artifact").await.unwrap()["text"],
                "after"
            );
            let history =
                crate::artifact_revisions::list_artifact_revisions(&app, &graph, "tail-artifact")
                    .unwrap();
            assert_eq!(history.len(), 3);
            assert_eq!(
                history
                    .iter()
                    .map(|r| r.revision_id.clone())
                    .collect::<std::collections::BTreeSet<_>>()
                    .len(),
                3
            );
            assert_eq!(
                submit(app.clone(), graph.clone(), input.clone())
                    .await
                    .unwrap()["replayed"],
                true
            );
            assert_eq!(
                crate::artifact_revisions::list_artifact_revisions(&app, &graph, "tail-artifact")
                    .unwrap()
                    .len(),
                3
            );
            let dir = crate::paths::existing_graph_dir(&app, &graph).unwrap();
            let root = dir.join("artifacts/tail-artifact");
            let owner: Owner = crate::storage::read_json(&root.join(OWNER_FILE)).unwrap();
            assert!(owner.pending.is_none());
            let r: Receipt = crate::storage::read_json(
                &dir.join("artifact-text-operations")
                    .join(format!("{}.json", hash(b"tail-edit"))),
            )
            .unwrap();
            assert_eq!(r.stage, "completed");
            // Source-level corrupt bytes must be refused, never replacement-decoded.
            crate::storage::write_bytes(&root.join("original/live.html"), &[0xff]).unwrap();
            assert!(read_text(&app, &graph, "tail-artifact").await.is_err());
            let orphan = dir.join("artifacts/orphan");
            crate::storage::create_dir_all(&orphan).unwrap();
            assert!(submit(
                app.clone(),
                graph.clone(),
                create(&inc, "orphan", "orphan-create", "x")
            )
            .await
            .is_err());
            assert!(fs::read_dir(&orphan).unwrap().next().is_none());
        })
    });
}
