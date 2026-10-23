//! Real rooms, queue, journal and SDK exporters; no socket listener or collector.

use super::persistence_coordinator::GraphPersistenceCoordinator;
use super::rooms::RoomRegistry;
use crate::crdt_queue::{CrdtOperation, CrdtOperationQueue, EnqueueCrdtOperationInput};
use crate::telemetry::{test_support, TraceContext};
use opentelemetry::trace::SpanId;
use opentelemetry_sdk::trace::SpanData;
use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tracing::{instrument::WithSubscriber, Instrument};
use yrs::{Doc, ReadTxn, StateVector, Transact, WriteTxn, XmlFragment};

const GRAPH: &str = "otel-graph";
const MARKER: &str = "OTEL_PRIVATE_DOCUMENT_MARKER_72d18a";

struct Profile {
    _serial: std::sync::MutexGuard<'static, ()>,
    previous: Option<std::ffi::OsString>,
    path: PathBuf,
}

impl Profile {
    fn new() -> Self {
        let serial = crate::tauri_runtime::profile_env_serial()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let path = std::env::temp_dir().join(format!("garden-otel-test-{}", uuid::Uuid::new_v4()));
        let previous = std::env::var_os("GARDEN_PROFILE_DIR");
        std::env::set_var("GARDEN_PROFILE_DIR", &path);
        Self {
            _serial: serial,
            previous,
            path,
        }
    }

    fn reopen(&self) -> crate::app_runtime::AppHandle {
        let app = crate::app_runtime::App::new();
        let handle = app.handle().clone();
        handle.manage(GraphPersistenceCoordinator::default());
        handle.manage(RoomRegistry::default());
        handle.manage(CrdtOperationQueue::default());
        handle.manage(crate::active_documents::ActiveDocumentRegistry::default());
        handle.manage(Arc::new(crate::cell_lifecycle::CellLifecycle::new()));
        handle.manage(Arc::new(crate::restore_guard::RestoreGuardState::default()));
        handle
    }

    fn app(&self) -> crate::app_runtime::AppHandle {
        let handle = self.reopen();
        crate::graph_service::create_graph_service(
            &handle,
            crate::graph_service::CreateGraphInput {
                graph_id: Some(GRAPH.to_string()),
                title: "Trace test".to_string(),
                description: None,
                operation_id: None,
            },
        )
        .expect("create real graph");
        handle
    }
}

impl Drop for Profile {
    fn drop(&mut self) {
        let _ = crate::rdf_store_service::evict_graph_store(&self.path.join("graphs").join(GRAPH));
        match &self.previous {
            Some(previous) => std::env::set_var("GARDEN_PROFILE_DIR", previous),
            None => std::env::remove_var("GARDEN_PROFILE_DIR"),
        }
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

fn attribute<'a>(span: &'a SpanData, key: &str) -> Option<&'a opentelemetry::Value> {
    span.attributes
        .iter()
        .find(|kv| kv.key.as_str() == key)
        .map(|kv| &kv.value)
}

fn fsync_kinds(span: &SpanData) -> Vec<String> {
    span.events
        .events
        .iter()
        .filter(|event| event.name == "fsync")
        .map(|event| {
            event
                .attributes
                .iter()
                .find(|kv| kv.key.as_str() == "sophia.fsync.what")
                .map(|kv| kv.value.to_string())
                .unwrap_or_default()
        })
        .collect()
}

#[test]
fn existing_document_reads_queries_and_room_opens_do_not_fsync() {
    let profile = Profile::new();
    let app = profile.app();
    let (provider, exporter, dispatch) = test_support::capture(true);
    tracing::dispatcher::with_default(&dispatch, || {
        runtime().block_on(async {
            crate::crdt_queue::enqueue_crdt_operation(
                app.clone(),
                EnqueueCrdtOperationInput {
                    kind: "document.write".to_string(),
                    graph_id: GRAPH.to_string(),
                    document_id: Some("read-doc".to_string()),
                    payload: json!({"title": "Read test", "content": MARKER, "format": "markdown"}),
                },
            )
            .instrument(tracing::info_span!(target: "gardend_otel", "test.document.write"))
            .await
            .expect("persist through the real queue");
            let graph_dir = crate::graph_paths::existing_graph_dir(&app, GRAPH).unwrap();
            let incarnation = crate::document_incarnation_store::ensure_document_incarnation_id(
                &graph_dir, "read-doc",
            )
            .unwrap();
            crate::rdf_seed_service::ensure_graph_store_seeded(&graph_dir).unwrap();
            crate::rdf_store_service::evict_graph_store(&graph_dir).unwrap();
            // A fresh registry and evicted store exercise disk hydration on
            // the first pass, then the same reads exercise cached authorities.
            let registry = RoomRegistry::default();
            for _ in 0..2 {
                tracing::info_span!(target: "gardend_otel", "test.read_document").in_scope(|| {
                    let record = crate::document_service::read_document(
                        app.clone(),
                        GRAPH.to_string(),
                        "read-doc".to_string(),
                    )
                    .unwrap();
                    assert!(record.body.contains(MARKER));
                });
                tracing::info_span!(target: "gardend_otel", "test.list_documents").in_scope(|| {
                    let documents =
                        crate::document_service::list_documents(app.clone(), GRAPH.to_string())
                            .unwrap();
                    assert!(documents
                        .iter()
                        .any(|record| record.document_id == "read-doc"));
                });
                tracing::info_span!(target: "gardend_otel", "test.sparql_query").in_scope(|| {
                    let result = crate::rdf_service::run_sparql_query_service(
                        app.clone(),
                        crate::rdf_service::SparqlInput {
                            graph_id: GRAPH.to_string(),
                            query: "ASK { GRAPH ?g { ?s ?p ?o } }".to_string(),
                        },
                    )
                    .unwrap();
                    assert_eq!(result.boolean, Some(true));
                });
                async {
                    // The existing-graph branch of doc_room, through its
                    // existence/identity checks, hydration and initial sync;
                    // no socket listener is needed to exercise these paths.
                    let coordinator = app.state::<GraphPersistenceCoordinator>();
                    let _lease = coordinator.acquire_lifecycle_shared(GRAPH).await.unwrap();
                    let graph_dir =
                        crate::graph_paths::existing_graph_dir_no_heal(&app, GRAPH).unwrap();
                    assert!(
                        crate::loopback_hocuspocus_routes::document_exists_for_room_connection(
                            &registry, GRAPH, &graph_dir, "read-doc",
                        )
                        .await
                        .unwrap()
                    );
                    assert_eq!(
                        crate::document_incarnation_store::ensure_document_incarnation_id(
                            &graph_dir, "read-doc",
                        )
                        .unwrap(),
                        incarnation
                    );
                    let room = registry
                        .get_or_create(
                            "doc:otel-graph:read-doc",
                            crate::ydoc_paths::document_ydoc_state_path(&graph_dir, "read-doc"),
                        )
                        .await
                        .unwrap();
                    room.configure_projection_flush(
                        GRAPH.to_string(),
                        Some("read-doc".to_string()),
                    )
                    .unwrap();
                    super::rooms::initial_sync_delivery(&app, GRAPH, &room, async {
                        assert!(!room.encode_state_for_test().await.is_empty());
                        Ok(())
                    })
                    .await
                    .unwrap();
                }
                .instrument(tracing::info_span!(target: "gardend_otel", "test.room_open"))
                .await;
            }
        })
    });
    provider.force_flush().unwrap();
    let spans = exporter.get_finished_spans().unwrap();
    for name in [
        "test.read_document",
        "test.list_documents",
        "test.sparql_query",
        "test.room_open",
    ] {
        let reads: Vec<_> = spans.iter().filter(|span| span.name == name).collect();
        assert_eq!(reads.len(), 2, "cold and cached {name}");
        for read in reads {
            // Count descendants too: moving a sync into a child span must
            // not make a read appear to have stopped flushing.
            let syncs: Vec<_> = spans
                .iter()
                .filter(|span| span.span_context.trace_id() == read.span_context.trace_id())
                .flat_map(fsync_kinds)
                .collect();
            assert!(syncs.is_empty(), "{name} forced flushes: {syncs:?}");
        }
    }
    let write = spans
        .iter()
        .find(|span| span.name == "test.document.write")
        .unwrap();
    let syncs: Vec<_> = spans
        .iter()
        .filter(|span| span.span_context.trace_id() == write.span_context.trace_id())
        .flat_map(fsync_kinds)
        .collect();
    assert!(syncs.iter().any(|what| what == "atomic.file"));
    assert!(syncs.iter().any(|what| what == "dir"));
}

#[test]
fn missing_directories_are_synced_once_and_graph_layout_still_heals() {
    let profile = Profile::new();
    let app = profile.app();
    let graph_dir = crate::graph_paths::existing_graph_dir(&app, GRAPH).unwrap();
    let artifacts = crate::graph_paths::artifacts_dir(&graph_dir);
    std::fs::remove_dir(&artifacts).unwrap();
    let nested = profile.path.join("new-parent/nested/leaf");
    let (provider, exporter, dispatch) = test_support::capture(true);
    tracing::dispatcher::with_default(&dispatch, || {
        tracing::info_span!(target: "gardend_otel", "test.mkdir").in_scope(|| {
            crate::storage::create_dir_all(&nested).unwrap();
        });
        tracing::info_span!(target: "gardend_otel", "test.heal").in_scope(|| {
            crate::graph_paths::existing_graph_dir(&app, GRAPH).unwrap();
        });
        tracing::info_span!(target: "gardend_otel", "test.existing_dirs").in_scope(|| {
            crate::storage::create_dir_all(&nested).unwrap();
            crate::graph_paths::existing_graph_dir(&app, GRAPH).unwrap();
        });
    });
    assert!(nested.is_dir());
    assert!(artifacts.is_dir());
    provider.force_flush().unwrap();
    let spans = exporter.get_finished_spans().unwrap();
    for (name, expected) in [
        ("test.mkdir", 3),
        ("test.heal", 1),
        ("test.existing_dirs", 0),
    ] {
        let span = spans.iter().find(|span| span.name == name).unwrap();
        assert_eq!(fsync_kinds(span), vec!["dir"; expected], "{name}");
    }
}

fn milliseconds(span: &SpanData, key: &str) -> f64 {
    match attribute(span, key) {
        Some(opentelemetry::Value::F64(value)) => *value,
        value => panic!("missing numeric {key}: {value:?}"),
    }
}

fn create_document(app: &crate::app_runtime::AppHandle, document_id: &str) {
    let coordinator = app.state::<GraphPersistenceCoordinator>();
    let _lease = coordinator.acquire_hot_write_blocking(GRAPH).unwrap();
    crate::document_service::create_document_with_lease(
        app.clone(),
        crate::document_types::CreateDocumentInput {
            graph_id: GRAPH.to_string(),
            document_id: Some(document_id.to_string()),
            title: document_id.to_string(),
        },
    )
    .unwrap();
}

#[test]
fn document_projection_releases_hot_write_but_keeps_the_deletion_fence() {
    for (kind, payload) in [
        (
            "block.insert",
            json!({"blocks": [{"id": "inserted", "content": "inserted text"}]}),
        ),
        (
            "document.editComment",
            json!({"action": "set", "commentId": "note", "text": "a note"}),
        ),
        (
            "block.editText",
            json!({"blockId": "large-0", "operations": [{"type": "insert", "offset": 0, "text": "edited "}]}),
        ),
    ] {
        let profile = Profile::new();
        let app = profile.app();
        create_document(&app, "large");
        create_document(&app, "other");
        let (provider, exporter, dispatch) = test_support::capture(true);
        let work = async {
            let registry = app.state::<RoomRegistry>();
            let graph_dir = crate::graph_paths::existing_graph_dir(&app, GRAPH).unwrap();
            let state_path = crate::ydoc_paths::document_ydoc_state_path(&graph_dir, "large");
            let room = registry
                .get_or_create("doc:otel-graph:large", state_path.clone())
                .await
                .unwrap();
            // A real book-sized room. Hold only its projection gate to
            // make the concurrency assertion deterministic on any disk.
            room.update_doc(|_doc, txn| {
                let fragment = txn.get_or_insert_xml_fragment("content");
                let nodes: Vec<_> = (0..2470)
                    .map(|index| json!({
                        "type": "paragraph", "attrs": {"data-block-id": format!("large-{index}")},
                        "content": [{"type": "text", "text": "A paragraph in the large document."}],
                    }))
                    .collect();
                super::builder::append_nodes(txn, &fragment, &nodes);
                Ok(())
            })
            .await
            .unwrap();
            let before = std::fs::read(&state_path).unwrap();
            let projection_guard = room.lock_projection_flush().await;
            let writer_app = app.clone();
            let writer = tokio::spawn(
                async move {
                    crate::crdt_queue::enqueue_crdt_operation(
                        writer_app,
                        EnqueueCrdtOperationInput {
                            kind: kind.to_string(),
                            graph_id: GRAPH.to_string(),
                            document_id: Some("large".to_string()),
                            payload,
                        },
                    )
                    .await
                }
                .with_current_subscriber(),
            );
            tokio::time::timeout(Duration::from_secs(10), async {
                while std::fs::read(&state_path).unwrap() == before {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .expect("queued mutation commits its authority before projection");

            let other = registry
                .get_or_create(
                    "doc:otel-graph:other",
                    crate::ydoc_paths::document_ydoc_state_path(&graph_dir, "other"),
                )
                .await
                .unwrap();
            let coordinator = app.state::<GraphPersistenceCoordinator>();
            let other_write = tracing::info_span!(target: "gardend_otel", "test.other_document");
            let update_other = async {
                let lease = tokio::time::timeout(
                    Duration::from_secs(10),
                    coordinator.acquire_hot_write(GRAPH),
                )
                .await
                .expect("another document must not wait for the blocked projection")
                .unwrap();
                lease.declare_rdf_read_only();
                other.update_doc(|_doc, txn| {
                    let fragment = txn.get_or_insert_xml_fragment("content");
                    super::builder::append_nodes(txn, &fragment, &[json!({
                        "type": "paragraph", "content": [{"type": "text", "text": "other document"}],
                    })]);
                    Ok(())
                }).await.unwrap();
            };
            update_other.instrument(other_write).await;
            tokio::time::timeout(Duration::from_secs(2), writer)
                .await
                .expect("write replies while its projection is fenced")
                .unwrap()
                .unwrap();
            // The background projector has its own lifecycle fence.
            tokio::time::sleep(Duration::from_millis(700)).await;
            assert!(
                tokio::time::timeout(
                    Duration::from_millis(25),
                    coordinator.acquire_lifecycle_exclusive(GRAPH)
                )
                .await
                .is_err(),
                "deletion still waits for the projection's lifecycle reader"
            );
            // Leave an observable gap between the hot commit and projection.
            tokio::time::sleep(Duration::from_millis(100)).await;
            drop(projection_guard);
            super::flush_ops::await_document_projections(
                app.clone(),
                GRAPH.to_string(),
                Some("large".to_string()),
            )
            .await
            .unwrap();
            assert!(!room.needs_projection_flush());
            assert_eq!(
                std::fs::read(&state_path).unwrap(),
                room.encode_state_for_test().await
            );
        };
        tracing::dispatcher::with_default(&dispatch, || runtime().block_on(work));
        provider.force_flush().unwrap();
        let spans = exporter.get_finished_spans().unwrap();
        let apply = spans
            .iter()
            .find(|span| {
                span.name == "crdt.op.apply"
                    && attribute(span, "sophia.crdt.kind")
                        == Some(&opentelemetry::Value::from(kind))
            })
            .unwrap();
        let hot = spans
            .iter()
            .find(|span| {
                span.name == "graph.lease.hot_write"
                    && span.parent_span_id == apply.span_context.span_id()
            })
            .unwrap();
        let materialize = spans
            .iter()
            .find(|span| span.name == "document.materialize")
            .unwrap();
        // Lease span durations include acquisition; held_ms is the actual hold.
        let released_at = hot.start_time
            + Duration::from_secs_f64(
                (milliseconds(hot, "sophia.lease.wait_ms")
                    + milliseconds(hot, "sophia.lease.held_ms"))
                    / 1000.0,
            );
        assert!(
            released_at + Duration::from_millis(50) < materialize.start_time,
            "{kind} held the hot lease into projection"
        );
        assert!(apply.end_time < materialize.start_time);
        let other = spans
            .iter()
            .find(|span| span.name == "test.other_document")
            .unwrap();
        let other_hot = spans
            .iter()
            .find(|span| {
                span.name == "graph.lease.hot_write"
                    && span.parent_span_id == other.span_context.span_id()
            })
            .unwrap();
        assert!(milliseconds(other_hot, "sophia.lease.wait_ms") < 100.0);
        assert!(milliseconds(other_hot, "sophia.lease.held_ms") >= 0.0);
        assert!(other.end_time < materialize.start_time);
        for name in [
            "document.save",
            "document.record.write",
            "document.rdf.reconcile",
            "document.history.snapshot",
        ] {
            assert!(
                spans
                    .iter()
                    .any(|span| span.name == name && span.start_time >= materialize.end_time),
                "missing {name}"
            );
        }
    }
}

async fn call_mcp(
    app: &crate::app_runtime::AppHandle,
    name: &str,
    args: serde_json::Value,
) -> serde_json::Value {
    let jobs_path = PathBuf::from(std::env::var_os("GARDEN_PROFILE_DIR").unwrap()).join("jobs");
    let jobs = Arc::new(crate::local_jobs::LocalJobRegistry::new(jobs_path).unwrap());
    let ctx = crate::mcp_dispatch_registry::McpCallCtx {
        app: app.clone(),
        jobs: &jobs,
    };
    (crate::mcp_dispatch_registry::lookup(name).unwrap().handler)(&ctx, &args)
        .await
        .unwrap()
}

#[test]
fn each_mcp_read_fences_its_acknowledged_write() {
    let profile = Profile::new();
    let app = profile.app();
    runtime().block_on(async {
        for tool in [
            "read_document", "read_blocks", "get_block", "query_blocks",
            "search_blocks", "document_digest", "sparql_query", "get_document_history",
        ] {
            create_document(&app, tool);
            let graph_dir = crate::graph_paths::existing_graph_dir(&app, GRAPH).unwrap();
            let room = app.state::<RoomRegistry>().get_or_create(
                &format!("doc:{GRAPH}:{tool}"),
                crate::ydoc_paths::document_ydoc_state_path(&graph_dir, tool),
            ).await.unwrap();
            let projection_guard = room.lock_projection_flush().await;
            crate::mcp_block_mutation_service::mcp_local_insert_blocks(
                app.clone(),
                &json!({
                    "graph_id": GRAPH, "document_id": tool,
                    "blocks": [{"id": "new-block", "content": "immediate block"}],
                }),
            ).await.unwrap();
            let graph = crate::rdf_authority::document_projection_graph_iri(GRAPH, tool);
            let mut args = json!({
                "graph_id": GRAPH, "graphId": GRAPH, "document_id": tool,
                "block_id": "new-block", "format": "xml", "topValued": 0,
                "text_contains": "immediate block", "mode": "lexical",
                "queries": ["immediate"],
                "query": format!("ASK WHERE {{ GRAPH <{graph}> {{ ?s ?p ?o FILTER(CONTAINS(STR(?o), \"immediate block\")) }} }}"),
            });
            if tool == "query_blocks" {
                args.as_object_mut().unwrap().remove("queries");
            }
            let read_app = app.clone();
            let mut read = tokio::spawn(async move { call_mcp(&read_app, tool, args).await });
            assert!(tokio::time::timeout(Duration::from_millis(50), &mut read).await.is_err(),
                "{tool} must not return the stale projection");
            let coordinator = app.state::<GraphPersistenceCoordinator>();
            let hot = tokio::time::timeout(Duration::from_secs(1), coordinator.acquire_hot_write(GRAPH))
                .await.expect("read barrier does not hold the hot-write lease").unwrap();
            hot.declare_rdf_read_only();
            drop(hot);
            drop(projection_guard);
            let result = read.await.unwrap();
            match tool {
                "read_document" => assert!(result["content"].as_str().unwrap().contains("immediate block")),
                "read_blocks" => assert_eq!(result["blocks"][0]["block_id"], "new-block"),
                "get_block" => assert_eq!(result["block"]["block_id"], "new-block"),
                "query_blocks" => assert_eq!(result["total_matches"], 1),
                "document_digest" => assert_eq!(result["size"]["block_count"], 1),
                "get_document_history" => assert_eq!(result["snapshot_count"], 2),
                "sparql_query" => assert_eq!(result["boolean"], true),
                "search_blocks" => assert!(result.to_string().contains("immediate block")),
                _ => unreachable!(),
            }
        }
    });
}

#[test]
fn quick_edits_coalesce_without_changing_queue_order() {
    let profile = Profile::new();
    let app = profile.app();
    create_document(&app, "ordered");
    let (provider, exporter, dispatch) = test_support::capture(true);
    let work = async {
        let graph_dir = crate::graph_paths::existing_graph_dir(&app, GRAPH).unwrap();
        let room = app
            .state::<RoomRegistry>()
            .get_or_create(
                "doc:otel-graph:ordered",
                crate::ydoc_paths::document_ydoc_state_path(&graph_dir, "ordered"),
            )
            .await
            .unwrap();
        let projection_guard = room.lock_projection_flush().await;
        crate::mcp_block_mutation_service::mcp_local_insert_blocks(
            app.clone(),
            &json!({
                "graph_id": GRAPH, "document_id": "ordered",
                "blocks": [{"id": "ordered-block", "content": "base"}],
            }),
        )
        .await
        .unwrap();
        let edit = |text: &str| EnqueueCrdtOperationInput {
            kind: "block.editText".to_string(),
            graph_id: GRAPH.to_string(),
            document_id: Some("ordered".to_string()),
            payload: json!({"blockId": "ordered-block", "operations": [{"type": "insert", "offset": 0, "text": text}]}),
        };
        let (first, second) = tokio::join!(biased;
            crate::crdt_queue::enqueue_crdt_operation(app.clone(), edit("first ")),
            crate::crdt_queue::enqueue_crdt_operation(app.clone(), edit("second ")),
        );
        first.unwrap();
        second.unwrap();
        drop(projection_guard);
        super::flush_ops::await_document_projections(
            app.clone(),
            GRAPH.to_string(),
            Some("ordered".to_string()),
        )
        .await
        .unwrap();
        let document = crate::document_service::read_document(
            app.clone(),
            GRAPH.to_string(),
            "ordered".to_string(),
        )
        .unwrap();
        assert_eq!(document.body, "second first base");
        assert_eq!(document.revision, 1);
        let graph_dir = crate::graph_paths::existing_graph_dir(&app, GRAPH).unwrap();
        assert_eq!(
            crate::document_history_file_store::read_document_history_store(
                &graph_dir, GRAPH, "ordered"
            )
            .unwrap()
            .total_count,
            2
        );
    };
    tracing::dispatcher::with_default(&dispatch, || runtime().block_on(work));
    provider.force_flush().unwrap();
    let spans = exporter.get_finished_spans().unwrap();
    let mut records: Vec<_> = spans
        .iter()
        .filter(|span| span.name == "document.record.write")
        .collect();
    records.sort_by_key(|span| span.start_time);
    assert_eq!(records.len(), 1);
    for (index, record) in records.iter().enumerate() {
        assert_eq!(
            attribute(record, "sophia.document.revision")
                .unwrap()
                .to_string(),
            (index + 1).to_string()
        );
    }
    assert!(records
        .windows(2)
        .all(|pair| pair[0].end_time <= pair[1].start_time));
}

#[test]
fn ten_inserts_reply_on_durable_authority_and_share_one_projection() {
    let profile = Profile::new();
    let app = profile.app();
    create_document(&app, "burst");
    let (provider, exporter, dispatch) = test_support::capture(true);
    tracing::dispatcher::with_default(&dispatch, || {
        runtime().block_on(async {
        let graph_dir = crate::graph_paths::existing_graph_dir(&app, GRAPH).unwrap();
        let state_path = crate::ydoc_paths::document_ydoc_state_path(&graph_dir, "burst");
        let room = app.state::<RoomRegistry>().get_or_create(
            "doc:otel-graph:burst", state_path.clone(),
        ).await.unwrap();
        let projection_guard = room.lock_projection_flush().await;
        for index in 0..10 {
            let outcome = tokio::time::timeout(Duration::from_secs(2),
                crate::crdt_queue::enqueue_crdt_operation_outcome(app.clone(), EnqueueCrdtOperationInput {
                    kind: "block.insert".to_string(), graph_id: GRAPH.to_string(),
                    document_id: Some("burst".to_string()),
                    payload: json!({"blocks": [{"id": format!("burst-{index}"), "content": format!("burst text {index}")}]}),
                }),
            ).await.expect("each caller replies while projection is blocked").unwrap();
            // Decode the actual persisted authority after EACH reply. Its
            // replay receipt and the new block must already be on disk.
            let reopened = RoomRegistry::default().get_or_create("durable", state_path.clone()).await.unwrap();
            reopened.with_doc(|doc| {
                let mut txn = doc.transact_mut();
                assert!(super::block_ops::read_ledger_entry(&txn, &outcome.operation_id).is_some());
                assert_eq!(txn.get_or_insert_xml_fragment("content").len(&txn), index + 1);
            }).await;
        }
        assert!(crate::crdt_operation_journal::recover_pending_crdt_operations(&app).unwrap().is_empty());
        assert!(crate::document_service::read_document(app.clone(), GRAPH.into(), "burst".into()).unwrap().blocks.is_empty());
        let generation = app.state::<GraphPersistenceCoordinator>().generation(GRAPH).unwrap();
        let flush_app = app.clone();
        let flush_room = room.clone();
        let flush = tokio::spawn(async move {
            super::rooms::run_projection_flush_for_test(flush_room, flush_app, generation, Duration::from_millis(20)).await;
        }.with_current_subscriber());
        // Make the projection span observably longer than the authority work.
        tokio::time::sleep(Duration::from_millis(800)).await;
        drop(projection_guard);
        flush.await.unwrap();
        super::flush_ops::await_document_projections(app.clone(), GRAPH.into(), Some("burst".into())).await.unwrap();
        let record = crate::document_service::read_document(app.clone(), GRAPH.into(), "burst".into()).unwrap();
        assert_eq!(record.blocks.len(), 10);
        for index in 0..10 {
            assert!(record.body.contains(&format!("burst text {index}")));
        }
        let graph = crate::rdf_authority::document_projection_graph_iri(GRAPH, "burst");
        let store = crate::rdf_service::open_graph_store(&graph_dir).unwrap();
        let result = oxigraph::sparql::SparqlEvaluator::new().parse_query(&format!(
            "SELECT ?text WHERE {{ GRAPH <{graph}> {{ ?s <http://mnemosyne.dev/doc#textContent> ?text }} }}",
        )).unwrap().on_store(&store).execute().unwrap();
        let oxigraph::sparql::QueryResults::Solutions(rows) = result else { panic!("solutions") };
        assert_eq!(rows.map(Result::unwrap).count(), 10);
    })
    });
    provider.force_flush().unwrap();
    let spans = exporter.get_finished_spans().unwrap();
    let writes: Vec<_> = spans
        .iter()
        .filter(|span| span.name == "crdt.op.apply")
        .collect();
    assert_eq!(writes.len(), 10);
    let projection = spans
        .iter()
        .find(|span| span.name == "projection.flush")
        .unwrap();
    for write in writes {
        assert!(
            write.end_time.duration_since(write.start_time).unwrap() * 4
                < projection
                    .end_time
                    .duration_since(projection.start_time)
                    .unwrap()
        );
        assert!(
            spans.iter().any(|span| span.name == "ydoc.state.write"
                && span.start_time >= write.start_time
                && span.end_time <= write.end_time
                && fsync_kinds(span).iter().any(|kind| kind == "atomic.file")),
            "each apply includes its authority fsync"
        );
    }
    assert_eq!(
        spans
            .iter()
            .filter(|span| span.name == "document.save")
            .count(),
        1
    );
    assert_eq!(
        spans
            .iter()
            .filter(|span| span.name == "document.history.snapshot")
            .count(),
        1
    );
}

#[test]
fn update_and_comment_replay_receipts_preserve_later_writes() {
    let profile = Profile::new();
    let app = profile.app();
    runtime().block_on(async {
        for (kind, first, second) in [
            (
                "block.update",
                json!({"blockId": "base", "attrs": {"customFlag": "first"}}),
                json!({"blockId": "base", "attrs": {"customFlag": "second"}}),
            ),
            (
                "document.editComment",
                json!({"action": "set", "commentId": "note", "text": "first", "updatedAt": 1}),
                json!({"action": "set", "commentId": "note", "text": "second", "updatedAt": 2}),
            ),
        ] {
            let document_id = if kind == "block.update" {
                "update-replay"
            } else {
                "comment-replay"
            };
            create_document(&app, document_id);
            let graph_dir = crate::graph_paths::existing_graph_dir(&app, GRAPH).unwrap();
            let state_path = crate::ydoc_paths::document_ydoc_state_path(&graph_dir, document_id);
            let room = app
                .state::<RoomRegistry>()
                .get_or_create(&format!("doc:{GRAPH}:{document_id}"), state_path.clone())
                .await
                .unwrap();
            let guard = room.lock_projection_flush().await;
            crate::mcp_block_mutation_service::mcp_local_insert_blocks(
                app.clone(),
                &json!({
                    "graph_id": GRAPH, "document_id": document_id,
                    "blocks": [{"id": "base", "content": "base"}],
                }),
            )
            .await
            .unwrap();
            let input = |payload| EnqueueCrdtOperationInput {
                kind: kind.into(),
                graph_id: GRAPH.into(),
                document_id: Some(document_id.into()),
                payload,
            };
            let receipt = crate::crdt_queue::enqueue_crdt_operation_outcome(
                app.clone(),
                input(first.clone()),
            )
            .await
            .unwrap();
            crate::crdt_queue::enqueue_crdt_operation(app.clone(), input(second))
                .await
                .unwrap();
            let committed = std::fs::read(&state_path).unwrap();
            let replay = CrdtOperation {
                operation_id: receipt.operation_id.clone(),
                kind: kind.into(),
                graph_id: GRAPH.into(),
                document_id: Some(document_id.into()),
                payload: first,
                enqueue_timestamp: crate::clock::timestamp(),
                trace_context: Default::default(),
            };
            super::executor::apply_operation(&app, &replay)
                .await
                .unwrap();
            assert_eq!(
                std::fs::read(&state_path).unwrap(),
                committed,
                "replaying {kind} must not overwrite the later edit"
            );
            let reopened = RoomRegistry::default()
                .get_or_create("receipt", state_path)
                .await
                .unwrap();
            reopened
                .with_doc(|doc| {
                    assert!(super::block_ops::read_ledger_entry(
                        &doc.transact_mut(),
                        &receipt.operation_id
                    )
                    .is_some());
                })
                .await;
            drop(guard);
            super::flush_ops::await_document_projections(
                app.clone(),
                GRAPH.into(),
                Some(document_id.into()),
            )
            .await
            .unwrap();
        }
    });
}

#[test]
fn replied_write_with_dropped_projection_is_repaired_on_reopen() {
    let profile = Profile::new();
    let app = profile.app();
    create_document(&app, "interrupted");
    runtime().block_on(async {
        let graph_dir = crate::graph_paths::existing_graph_dir(&app, GRAPH).unwrap();
        let room = app.state::<RoomRegistry>().get_or_create(
            "doc:otel-graph:interrupted",
            crate::ydoc_paths::document_ydoc_state_path(&graph_dir, "interrupted"),
        ).await.unwrap();
        let projection_guard = room.lock_projection_flush().await;
        crate::mcp_block_mutation_service::mcp_local_insert_blocks(app.clone(), &json!({
            "graph_id": GRAPH, "document_id": "interrupted",
            "blocks": [{"id": "survivor", "content": "acknowledged before crash"}],
        })).await.unwrap();
        let registry = app.state::<RoomRegistry>();
        let room = registry.peek("doc:otel-graph:interrupted").await.unwrap();
        // Drop the projection after snapshot capture, before its record/RDF/
        // history writes. Eviction also cancels the pending debounce.
        let candidate = super::document_ops::materialize_room_document(
            GRAPH, "interrupted", "interrupted", &room, "interrupted-projection", None, None,
        ).await.unwrap();
        drop(candidate);
        registry.evict_room("doc:otel-graph:interrupted");
        drop(projection_guard);
        assert!(crate::crdt_operation_journal::recover_pending_crdt_operations(&app).unwrap().is_empty());
        assert!(crate::document_service::read_document(app.clone(), GRAPH.into(), "interrupted".into()).unwrap().blocks.is_empty());
        // No pending operation is needed: hydration marks authority dirty and
        // the read barrier uses the same projection repair as connect-time flush.
        let reopened = profile.reopen();
        let result = call_mcp(&reopened, "read_document", json!({
            "graph_id": GRAPH, "document_id": "interrupted",
        })).await;
        assert!(result["content"].as_str().unwrap().contains("acknowledged before crash"));
        let graph_dir = crate::graph_paths::existing_graph_dir(&reopened, GRAPH).unwrap();
        let store = crate::rdf_service::open_graph_store(&graph_dir).unwrap();
        let graph = crate::rdf_authority::document_projection_graph_iri(GRAPH, "interrupted");
        let result = oxigraph::sparql::SparqlEvaluator::new().parse_query(&format!(
            "ASK WHERE {{ GRAPH <{graph}> {{ ?s <http://mnemosyne.dev/doc#textContent> \"acknowledged before crash\" }} }}",
        )).unwrap().on_store(&store).execute().unwrap();
        assert!(matches!(result, oxigraph::sparql::QueryResults::Boolean(true)));
    });
}

#[test]
fn room_updates_debounce_into_a_linked_flush_without_content() {
    let profile = Profile::new();
    let app = profile.app();
    let (provider, exporter, dispatch) = test_support::capture(true);
    tracing::dispatcher::with_default(&dispatch, || {
        runtime().block_on(async {
            let registry = app.state::<RoomRegistry>();
            let graph_dir = crate::graph_paths::existing_graph_dir(&app, GRAPH).unwrap();
            let room = registry
                .get_or_create(
                    "doc:otel-graph:room-doc",
                    crate::ydoc_paths::document_ydoc_state_path(&graph_dir, "room-doc"),
                )
                .await
                .unwrap();
            room.configure_projection_flush(GRAPH.to_string(), Some("room-doc".to_string()))
                .unwrap();
            let remote = Doc::new();
            let connection = tracing::info_span!(target: "gardend_otel", "room.connection");
            async {
            for index in 0..2 {
                let update = {
                    let mut txn = remote.transact_mut();
                    let fragment = txn.get_or_insert_xml_fragment("content");
                    super::builder::append_nodes(&mut txn, &fragment, &[json!({
                        "type": "paragraph", "attrs": {"data-block-id": format!("block-{index}")},
                        "content": [{"type": "text", "text": MARKER}],
                    })]);
                    drop(txn);
                    remote.transact().encode_state_as_update_v1(&StateVector::default())
                };
                let coordinator = app.state::<GraphPersistenceCoordinator>();
                let lease = coordinator.acquire_hot_write(GRAPH).await.unwrap();
                lease.declare_rdf_read_only();
                assert!(room.apply_client_update_and_schedule(&update, app.clone()).await.unwrap());
            }
        }.instrument(connection).await;
            tokio::time::timeout(Duration::from_secs(15), async {
                loop {
                    let spans = exporter.get_finished_spans().unwrap();
                    if spans.iter().any(|span| span.name == "projection.flush")
                        && spans.iter().any(|span| span.name == "room.connection")
                    {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("debounced flush completes");
            assert!(!room.needs_projection_flush());
            let document = crate::document_record_store::read_document_record(
                &graph_dir,
                &crate::document_paths::document_dir(&graph_dir, "room-doc")
                    .unwrap()
                    .join("document.json"),
            )
            .unwrap();
            assert!(document.body.contains(MARKER));
        })
    });
    provider.force_flush().unwrap();
    let spans = exporter.get_finished_spans().unwrap();
    let connection = spans
        .iter()
        .find(|span| span.name == "room.connection")
        .unwrap();
    let updates: Vec<_> = spans
        .iter()
        .filter(|span| span.name == "ydoc.update.apply")
        .collect();
    assert_eq!(updates.len(), 2);
    for update in &updates {
        assert_eq!(update.parent_span_id, connection.span_context.span_id());
        assert!(attribute(update, "sophia.ydoc.update_bytes").is_some());
        assert!(attribute(update, "sophia.ydoc.mutex_wait_ms").is_some());
    }
    let leases: Vec<_> = spans
        .iter()
        .filter(|span| span.name == "graph.lease.hot_write")
        .collect();
    assert!(!leases.is_empty());
    assert!(leases
        .iter()
        .all(|span| attribute(span, "sophia.lease.wait_ms").is_some()));
    assert!(leases
        .iter()
        .all(|span| attribute(span, "sophia.lease.held_ms").is_some()));
    let flush = spans
        .iter()
        .find(|span| span.name == "projection.flush")
        .unwrap();
    assert_eq!(flush.parent_span_id, SpanId::INVALID);
    assert_eq!(flush.links.links.len(), 2);
    for update in updates {
        assert!(flush
            .links
            .links
            .iter()
            .any(
                |link| link.span_context.trace_id() == update.span_context.trace_id()
                    && link.span_context.span_id() == update.span_context.span_id()
            ));
    }
    for name in [
        "ydoc.state.persist",
        "ydoc.state.encode",
        "ydoc.state.write",
        "crdt.op.apply",
        "document.materialize",
        "document.save",
        "document.record.write",
        "document.rdf.reconcile",
        "document.history.snapshot",
    ] {
        assert!(spans.iter().any(|span| span.name == name), "missing {name}");
    }
    assert!(!test_support::json(&spans).contains(MARKER));
}

#[test]
fn enqueue_context_parents_the_real_drainer_and_recovery_links_it() {
    let profile = Profile::new();
    let app = profile.app();
    let (provider, exporter, dispatch) = test_support::capture(true);
    let mut recovered_id = None;
    tracing::dispatcher::with_default(&dispatch, || {
        runtime().block_on(async {
            let enqueue = tracing::info_span!(target: "gardend_otel", "test.enqueue");
            crate::crdt_queue::enqueue_crdt_operation(
                app.clone(),
                EnqueueCrdtOperationInput {
                    kind: "document.write".to_string(),
                    graph_id: GRAPH.to_string(),
                    document_id: Some("queued-doc".to_string()),
                    payload: json!({"title": MARKER, "content": MARKER, "format": "markdown"}),
                },
            )
            .instrument(enqueue)
            .await
            .expect("real queued write");

            // Persist a second operation, reconstruct it from the journal through
            // the production recovery path, then drain under an unrelated span.
            let origin = tracing::info_span!(target: "gardend_otel", "test.before_restart");
            let context = origin.in_scope(TraceContext::capture);
            let operation = CrdtOperation {
                operation_id: "otel-recovered".to_string(),
                kind: "crdt.flush".to_string(),
                graph_id: GRAPH.to_string(),
                document_id: None,
                payload: json!({"includeMaterialization": true}),
                enqueue_timestamp: crate::clock::timestamp(),
                trace_context: context,
            };
            recovered_id = Some(operation.operation_id.clone());
            crate::crdt_operation_journal::record_crdt_operation_queued(&app, &operation).unwrap();
            drop(origin);
            assert_eq!(
                crate::crdt_queue::recover_crdt_operations(app.clone()).unwrap(),
                1
            );
            super::executor::drain_queue(app.clone())
                .instrument(tracing::info_span!(target: "gardend_otel", "test.unrelated_drainer"))
                .await;
        })
    });
    provider.force_flush().unwrap();
    let spans = exporter.get_finished_spans().unwrap();
    let enqueue = spans
        .iter()
        .find(|span| span.name == "test.enqueue")
        .unwrap();
    let apply = spans
        .iter()
        .find(|span| {
            span.name == "crdt.op.apply"
                && attribute(span, "sophia.crdt.kind")
                    == Some(&opentelemetry::Value::from("document.write"))
        })
        .unwrap();
    assert_eq!(apply.parent_span_id, enqueue.span_context.span_id());
    assert_eq!(
        apply.span_context.trace_id(),
        enqueue.span_context.trace_id()
    );
    let recovered = spans
        .iter()
        .find(|span| {
            span.name == "crdt.op.apply"
                && attribute(span, "sophia.operation.id")
                    == Some(&opentelemetry::Value::from(recovered_id.clone().unwrap()))
        })
        .unwrap();
    let origin = spans
        .iter()
        .find(|span| span.name == "test.before_restart")
        .unwrap();
    assert_eq!(recovered.parent_span_id, SpanId::INVALID);
    assert_eq!(recovered.links.links.len(), 1);
    assert_eq!(
        recovered.links.links[0].span_context.trace_id(),
        origin.span_context.trace_id()
    );
    assert_eq!(
        recovered.links.links[0].span_context.span_id(),
        origin.span_context.span_id()
    );
    assert_ne!(
        recovered.span_context.trace_id(),
        origin.span_context.trace_id()
    );
    assert!(!test_support::json(&spans).contains(MARKER));
}

#[test]
fn a_slow_cancelled_lease_wait_reports_the_current_holder() {
    let (provider, exporter, dispatch) = test_support::capture(true);
    tracing::dispatcher::with_default(&dispatch, || {
        runtime().block_on(async {
            let coordinator = GraphPersistenceCoordinator::default();
            let holder = coordinator.acquire_hot_write(GRAPH).await.unwrap();
            holder.declare_rdf_read_only();
            holder.set_crdt_kind("crdt.flush");
            assert!(tokio::time::timeout(
                Duration::from_millis(1100),
                coordinator.acquire_hot_write(GRAPH)
            )
            .await
            .is_err());
            drop(holder);
        })
    });
    provider.force_flush().unwrap();
    let spans = exporter.get_finished_spans().unwrap();
    let waiting = spans
        .iter()
        .find(|span| {
            span.name == "graph.lease.hot_write"
                && attribute(span, "sophia.lease.outcome")
                    == Some(&opentelemetry::Value::from("cancelled"))
        })
        .unwrap();
    assert_eq!(
        attribute(waiting, "sophia.lease.blocker.kind"),
        Some(&opentelemetry::Value::from("crdt.flush"))
    );
    assert!(
        matches!(attribute(waiting, "sophia.lease.blocker.held_ms"), Some(opentelemetry::Value::F64(ms)) if *ms >= 1000.0)
    );
    assert!(
        matches!(attribute(waiting, "sophia.lease.wait_ms"), Some(opentelemetry::Value::F64(ms)) if *ms >= 1000.0)
    );
}

#[test]
fn disabled_room_update_records_nothing() {
    let profile = Profile::new();
    let app = profile.app();
    let (provider, exporter, dispatch) = test_support::capture(false);
    tracing::dispatcher::with_default(&dispatch, || {
        runtime().block_on(async {
            let registry = app.state::<RoomRegistry>();
            let room = registry
                .get_or_create(
                    "doc:otel-graph:disabled",
                    profile
                        .path
                        .join("graphs/otel-graph/ydocs/documents/disabled/update-v1.bin"),
                )
                .await
                .unwrap();
            let update = {
                let doc = Doc::new();
                let mut txn = doc.transact_mut();
                let fragment = txn.get_or_insert_xml_fragment("content");
                super::builder::append_nodes(
                    &mut txn,
                    &fragment,
                    &[json!({
                        "type": "paragraph", "content": [{"type": "text", "text": MARKER}],
                    })],
                );
                drop(txn);
                let update = doc
                    .transact()
                    .encode_state_as_update_v1(&StateVector::default());
                update
            };
            let coordinator = app.state::<GraphPersistenceCoordinator>();
            let lease = coordinator.acquire_hot_write(GRAPH).await.unwrap();
            lease.declare_rdf_read_only();
            assert!(room.apply_client_update(&update).await.unwrap());
        })
    });
    provider.force_flush().unwrap();
    assert!(exporter.get_finished_spans().unwrap().is_empty());
}

#[test]
fn initial_sync_classifies_delivery_and_lease_timeouts_without_error_text() {
    let profile = Profile::new();
    let app = profile.app();
    let (provider, exporter, dispatch) = test_support::capture(true);
    tracing::dispatcher::with_default(&dispatch, || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .start_paused(true)
            .build()
            .unwrap()
            .block_on(async {
                let registry = app.state::<RoomRegistry>();
                let room = registry
                    .get_or_create(
                        "doc:otel-graph:initial",
                        profile
                            .path
                            .join("graphs/otel-graph/ydocs/documents/initial/update-v1.bin"),
                    )
                    .await
                    .unwrap();
                super::rooms::initial_sync_delivery(&app, GRAPH, &room, async { Ok(()) })
                    .await
                    .unwrap();
                assert!(
                    super::rooms::initial_sync_delivery(&app, GRAPH, &room, async {
                        Err(MARKER.to_string())
                    })
                    .await
                    .is_err()
                );
                assert!(super::rooms::initial_sync_delivery(
                    &app,
                    GRAPH,
                    &room,
                    std::future::pending::<Result<(), String>>()
                )
                .await
                .is_err());
                let coordinator = app.state::<GraphPersistenceCoordinator>();
                let holder = coordinator
                    .acquire_lifecycle_exclusive(GRAPH)
                    .await
                    .unwrap();
                holder.declare_rdf_read_only();
                assert!(
                    super::rooms::initial_sync_delivery(&app, GRAPH, &room, async { Ok(()) })
                        .await
                        .is_err()
                );
            });
    });
    provider.force_flush().unwrap();
    let spans = exporter.get_finished_spans().unwrap();
    let syncs: Vec<_> = spans
        .iter()
        .filter(|span| span.name == "room.initial_sync")
        .collect();
    assert_eq!(syncs.len(), 4);
    for outcome in ["sent", "peer_gone", "delivery_timeout", "lease_timeout"] {
        assert!(
            syncs
                .iter()
                .any(|span| attribute(span, "sophia.delivery.outcome")
                    == Some(&opentelemetry::Value::from(outcome))),
            "missing {outcome}"
        );
    }
    assert!(syncs
        .iter()
        .all(|span| attribute(span, "sophia.lease.wait_ms").is_some()));
    assert!(!test_support::json(&spans).contains(MARKER));
}

#[test]
fn an_atomic_write_counts_its_fsyncs_on_the_open_span() {
    let (provider, exporter, dispatch) = test_support::capture(true);
    let dir = std::env::temp_dir().join(format!("otel-fsync-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let target = dir.join("record.json");
    tracing::dispatcher::with_default(&dispatch, || {
        tracing::info_span!(target: "gardend_otel", "test.write").in_scope(|| {
            crate::storage_atomic::write_bytes_atomic(
                &target,
                &crate::storage_atomic::atomic_temp_path(&target),
                MARKER.as_bytes(),
                None,
            )
            .unwrap();
        });
    });
    provider.force_flush().unwrap();
    let spans = exporter.get_finished_spans().unwrap();
    let write = spans.iter().find(|span| span.name == "test.write").unwrap();
    // The file, then its directory: two full-drive flushes for one small record on macOS.
    assert_eq!(fsync_kinds(write), ["atomic.file", "dir"]);
    assert!(!test_support::json(&spans).contains(MARKER));
    let _ = std::fs::remove_dir_all(&dir);
}
