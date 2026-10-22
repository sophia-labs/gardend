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
use tracing::Instrument;
use yrs::{Doc, ReadTxn, StateVector, Transact, WriteTxn};

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

    fn app(&self) -> crate::app_runtime::AppHandle {
        let app = crate::app_runtime::App::new();
        let handle = app.handle().clone();
        handle.manage(GraphPersistenceCoordinator::default());
        handle.manage(RoomRegistry::default());
        handle.manage(CrdtOperationQueue::default());
        handle.manage(Arc::new(crate::cell_lifecycle::CellLifecycle::new()));
        handle.manage(Arc::new(crate::restore_guard::RestoreGuardState::default()));
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
    let what: Vec<String> = write
        .events
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
        .collect();
    // The file, then its directory: two full-drive flushes for one small record on macOS.
    assert_eq!(what, ["atomic.file", "dir"]);
    assert!(!test_support::json(&spans).contains(MARKER));
    let _ = std::fs::remove_dir_all(&dir);
}
