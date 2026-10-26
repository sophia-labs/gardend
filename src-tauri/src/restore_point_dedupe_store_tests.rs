//! Restore points are O(changed documents): content reuse, exactness, link-counted retention.
//!
//! A capture reuses the newest point's snapshot and hard-links its Y.Doc bytes for every document that has
//! not changed, instead of re-snapshotting and rewriting it. These tests run the real service on a
//! disposable graph (documents written through the real engine, restores run through the real restore
//! service) and compare what a point holds against an ORACLE: the state of the live graph at the moment the
//! point was created, read independently of the capture code, plus a fresh full snapshot taken by the old
//! per-document path (`current_document_snapshot_with_lease`) at that same moment. No mocks.
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::local_jobs::LocalJobRegistry;
use crate::mcp_dispatch_registry::{lookup, McpCallCtx};

const GRAPH: &str = "restore-dedupe";

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
        std::env::temp_dir().join(format!("garden-restore-dedupe-{}", uuid::Uuid::new_v4()));
    std::env::set_var("GARDEN_PROFILE_DIR", &profile);
    let jobs_dir = std::env::temp_dir().join(format!(
        "garden-restore-dedupe-jobs-{}",
        uuid::Uuid::new_v4()
    ));
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let app = crate::tauri_runtime::build_mock_app_for_tests(true);
        crate::graph_service::create_graph_service(
            &app,
            crate::graph_service::CreateGraphInput {
                graph_id: Some(GRAPH.into()),
                title: "Restore dedupe specimen".into(),
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

fn write(env: &Env, document_id: &str, text: &str) {
    run(crate::document_mutation_service::mcp_local_write_document(
        env.app.clone(),
        &json!({"graph_id": GRAPH, "document_id": document_id,
                "tiptapJson": {"type": "doc", "content": [paragraph("a", text)]}}),
    ))
    .unwrap();
}

fn markdown(env: &Env, document_id: &str) -> String {
    crate::document_mcp_service::mcp_local_read_document(
        env.app.clone(),
        &json!({"graph_id": GRAPH, "document_id": document_id, "format": "markdown", "maxChars": 1_000_000}),
    )
    .unwrap()["content"]
        .as_str()
        .unwrap()
        .to_string()
}

fn sidecar(env: &Env, document_id: &str) -> Vec<u8> {
    std::fs::read(crate::ydoc_paths::document_ydoc_state_path(
        &env.graph_dir(),
        document_id,
    ))
    .unwrap_or_else(|error| panic!("sidecar of {document_id}: {error}"))
}

/// Points are ordered by millisecond timestamp; keep them apart so "newest" is unambiguous.
fn tick() {
    std::thread::sleep(Duration::from_millis(5));
}

/// What the old implementation stored for one document at one moment: its Y.Doc bytes and a FRESH snapshot's
/// payload (title, blocks, XML), plus the markdown a reader sees.
#[derive(Debug, PartialEq)]
struct Oracle {
    ydoc: Vec<u8>,
    title: String,
    blocks: Value,
    xml: String,
    markdown: String,
}

fn oracle(env: &Env, ids: &[&str]) -> BTreeMap<String, Oracle> {
    ids.iter()
        .map(|id| {
            let meta = crate::document_history_service::current_document_snapshot_with_lease(
                &env.app,
                GRAPH,
                id,
                true,
                Some("oracle".to_string()),
            )
            .expect("fresh snapshot");
            let payload = crate::document_history_file_store::read_document_snapshot_payload(
                &env.graph_dir(),
                id,
                &meta.snapshot_id,
            )
            .expect("fresh payload");
            (
                id.to_string(),
                Oracle {
                    ydoc: sidecar(env, id),
                    title: payload.title,
                    blocks: serde_json::to_value(&payload.blocks).unwrap(),
                    xml: payload.tiptap_xml,
                    markdown: markdown(env, id),
                },
            )
        })
        .collect()
}

fn point(env: &Env, label: &str) -> String {
    // Settle acknowledged writes first, so a record is not rewritten by a pending flush between two points.
    let _ = env.call("flush_crdt", json!({}));
    env.call("create_restore_point", json!({"label": label}))
        .unwrap()["restorePointId"]
        .as_str()
        .expect("restore point id")
        .to_string()
}

fn manifest(env: &Env, restore_point_id: &str) -> crate::time_travel_types::RestorePointManifest {
    crate::time_travel_store::read_manifest(&env.graph_dir(), restore_point_id).expect("manifest")
}

fn doc_ref(
    manifest: &crate::time_travel_types::RestorePointManifest,
    id: &str,
) -> crate::time_travel_types::DocumentSnapshotRef {
    manifest
        .documents
        .iter()
        .find(|doc| doc.document_id == id)
        .unwrap_or_else(|| panic!("{id} missing from {}", manifest.restore_point_id))
        .clone()
}

fn bytes_path(env: &Env, restore_point_id: &str, id: &str) -> std::path::PathBuf {
    crate::time_travel_paths::restore_point_document_bytes_path(
        &env.graph_dir(),
        restore_point_id,
        id,
    )
}

#[cfg(unix)]
fn links(path: &std::path::Path) -> u64 {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(path)
        .unwrap_or_else(|error| panic!("stat {path:?}: {error}"))
        .nlink()
}

/// A point holds, for every document, what the oracle saw when it was made: the same Y.Doc bytes and the same
/// snapshot content, whether the entry was captured fresh or reused.
fn assert_point_matches_oracle(
    env: &Env,
    restore_point_id: &str,
    expected: &BTreeMap<String, Oracle>,
) {
    let manifest = manifest(env, restore_point_id);
    assert_eq!(manifest.documents.len(), expected.len());
    for (id, want) in expected {
        let entry = doc_ref(&manifest, id);
        let bytes =
            crate::time_travel_store::read_document_bytes(&env.graph_dir(), restore_point_id, id)
                .unwrap();
        assert_eq!(bytes, want.ydoc, "{restore_point_id}/{id}: Y.Doc bytes");
        let payload = crate::document_history_file_store::read_document_snapshot_payload(
            &env.graph_dir(),
            id,
            &entry.snapshot_id,
        )
        .unwrap_or_else(|error| panic!("{restore_point_id}/{id}: snapshot payload: {error}"));
        assert_eq!(payload.title, want.title, "{restore_point_id}/{id}: title");
        assert_eq!(
            serde_json::to_value(&payload.blocks).unwrap(),
            want.blocks,
            "{restore_point_id}/{id}: blocks"
        );
        assert_eq!(payload.tiptap_xml, want.xml, "{restore_point_id}/{id}: xml");
    }
}

fn restore(env: &Env, restore_point_id: &str) {
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
            "succeeded" => return,
            "failed" | "rolled_back" => {
                panic!("restore of {restore_point_id} did not succeed: {status}")
            }
            _ => {}
        }
        assert!(Instant::now() < deadline, "restore timed out: {status}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// A, B, C over documents that change in different places. Every point matches its oracle, unchanged
/// documents are reused, and restoring each point (in an order that makes each restore cross the others) gives
/// back exactly the documents the oracle saw.
#[test]
fn points_reuse_unchanged_documents_and_restore_exactly() {
    with_env(|env| {
        let ids = ["d1", "d2", "d3"];
        for id in ids {
            create(env, id);
            write(env, id, &format!("{id} first"));
        }
        let a = point(env, "A");
        let oracle_a = oracle(env, &ids);
        assert_point_matches_oracle(env, &a, &oracle_a);

        tick();
        write(env, "d2", "d2 second");
        let b = point(env, "B");
        let oracle_b = oracle(env, &ids);
        assert_point_matches_oracle(env, &b, &oracle_b);

        tick();
        write(env, "d3", "d3 third");
        write(env, "d2", "d2 third");
        let c = point(env, "C");
        let oracle_c = oracle(env, &ids);
        assert_point_matches_oracle(env, &c, &oracle_c);

        let (ma, mb, mc) = (manifest(env, &a), manifest(env, &b), manifest(env, &c));
        // d1 never changed: one snapshot, one set of bytes, in all three points.
        assert_eq!(
            doc_ref(&ma, "d1").snapshot_id,
            doc_ref(&mb, "d1").snapshot_id
        );
        assert_eq!(
            doc_ref(&mb, "d1").snapshot_id,
            doc_ref(&mc, "d1").snapshot_id
        );
        // d3 changed between B and C only; d2 changed in both gaps.
        assert_eq!(
            doc_ref(&ma, "d3").snapshot_id,
            doc_ref(&mb, "d3").snapshot_id
        );
        assert_ne!(
            doc_ref(&mb, "d3").snapshot_id,
            doc_ref(&mc, "d3").snapshot_id
        );
        assert_ne!(
            doc_ref(&ma, "d2").snapshot_id,
            doc_ref(&mb, "d2").snapshot_id
        );
        assert_ne!(
            doc_ref(&mb, "d2").snapshot_id,
            doc_ref(&mc, "d2").snapshot_id
        );
        #[cfg(unix)]
        {
            assert_eq!(
                links(&bytes_path(env, &a, "d1")),
                3,
                "d1 is one inode in A, B, C"
            );
            assert_eq!(
                links(&bytes_path(env, &a, "d3")),
                2,
                "d3 is shared by A and B"
            );
            assert_eq!(links(&bytes_path(env, &c, "d3")), 1);
            assert_eq!(links(&bytes_path(env, &c, "d2")), 1);
        }

        // Restore crosses the points: A, then C, then B. Each lands on the oracle's markdown.
        for (id, oracle_point) in [(&a, &oracle_a), (&c, &oracle_c), (&b, &oracle_b)] {
            restore(env, id);
            for doc in ids {
                assert_eq!(
                    markdown(env, doc),
                    oracle_point[doc].markdown,
                    "restoring {id} did not restore {doc}"
                );
            }
        }
        // The pre-restore backups were captured against the same shared content and are points like any
        // other: all of the originals still match their oracles.
        assert_point_matches_oracle(env, &a, &oracle_a);
        assert_point_matches_oracle(env, &b, &oracle_b);
        assert_point_matches_oracle(env, &c, &oracle_c);
    });
}

/// `diff_restore_points` between two points reports the documents that really changed (the old capture made
/// a fresh snapshot per document every time, so every document read as modified).
#[test]
fn diff_counts_only_the_documents_that_changed() {
    with_env(|env| {
        for id in ["d1", "d2", "d3"] {
            create(env, id);
            write(env, id, &format!("{id} first"));
        }
        let a = point(env, "A");
        tick();
        write(env, "d2", "d2 changed and longer");
        create(env, "d4");
        write(env, "d4", "d4 is new");
        let b = point(env, "B");

        let diff = env
            .call(
                "diff_restore_points",
                json!({"restorePointId": b, "against": a}),
            )
            .unwrap();
        let aggregate = &diff["aggregate"];
        assert_eq!(aggregate["docsModified"], 1, "{diff}");
        assert_eq!(aggregate["docsAdded"], 1, "{diff}");
        assert_eq!(aggregate["docsRemoved"], 0, "{diff}");
        let (ma, mb) = (manifest(env, &a), manifest(env, &b));
        let expected_delta = doc_ref(&mb, "d2").char_count as i64
            - doc_ref(&ma, "d2").char_count as i64
            + doc_ref(&mb, "d4").char_count as i64;
        assert_eq!(aggregate["totalCharDelta"], expected_delta, "{diff}");
        // An unchanged graph diffs clean.
        tick();
        let c = point(env, "C");
        let same = env
            .call(
                "diff_restore_points",
                json!({"restorePointId": c, "against": b}),
            )
            .unwrap();
        assert_eq!(same["aggregate"]["docsModified"], 0, "{same}");
        assert_eq!(same["aggregate"]["totalCharDelta"], 0, "{same}");
    });
}

/// Deleting a point never frees content another point still names: the link count is the reference count.
/// Deleting the newest point, then capturing again, dedupes against what is left.
#[test]
fn deleting_points_never_frees_content_another_point_names() {
    with_env(|env| {
        let ids = ["d1", "d2"];
        for id in ids {
            create(env, id);
            write(env, id, &format!("{id} first"));
        }
        let a = point(env, "A");
        tick();
        let b = point(env, "B");
        tick();
        let c = point(env, "C");
        let want = oracle(env, &ids);
        #[cfg(unix)]
        assert_eq!(links(&bytes_path(env, &a, "d1")), 3);

        env.call("delete_restore_point", json!({"restorePointId": a}))
            .unwrap();
        assert!(!bytes_path(env, &a, "d1").exists());
        #[cfg(unix)]
        assert_eq!(
            links(&bytes_path(env, &b, "d1")),
            2,
            "A's name is gone, the content stays"
        );
        assert_point_matches_oracle(env, &b, &want);
        assert_point_matches_oracle(env, &c, &want);

        env.call("delete_restore_point", json!({"restorePointId": b}))
            .unwrap();
        #[cfg(unix)]
        assert_eq!(links(&bytes_path(env, &c, "d1")), 1);
        assert_point_matches_oracle(env, &c, &want);

        // Restore from the only survivor still works, and a new capture dedupes against it.
        write(env, "d1", "d1 after the deletions");
        restore(env, &c);
        assert_eq!(markdown(env, "d1"), want["d1"].markdown);
        tick();
        let d = point(env, "D");
        let (mc, md) = (manifest(env, &c), manifest(env, &d));
        assert_eq!(
            doc_ref(&mc, "d2").snapshot_id,
            doc_ref(&md, "d2").snapshot_id
        );

        // Deleting the last name frees the content.
        let c_file = bytes_path(env, &c, "d2");
        env.call("delete_restore_point", json!({"restorePointId": c}))
            .unwrap();
        assert!(!c_file.exists());
        assert!(bytes_path(env, &d, "d2").is_file(), "D still holds d2");
        assert_point_matches_oracle(env, &d, &oracle(env, &ids));
    });
}

/// Reuse is only ever of byte-identical, still-resolvable content: a damaged old file, or an old snapshot
/// the user deleted, is recaptured rather than propagated.
#[test]
fn damaged_or_missing_old_content_is_recaptured_not_reused() {
    with_env(|env| {
        let ids = ["d1", "d2"];
        for id in ids {
            create(env, id);
            write(env, id, &format!("{id} first"));
        }
        let a = point(env, "A");
        let ma = manifest(env, &a);

        // d1: the old point's bytes are damaged in place (same length, same inode).
        let damaged = bytes_path(env, &a, "d1");
        let mut bytes = std::fs::read(&damaged).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 0xff;
        std::fs::write(&damaged, &bytes).unwrap();
        // d2: the old point's snapshot payload is gone.
        std::fs::remove_file(
            crate::document_history_store::document_snapshot_payload_path(
                &env.graph_dir(),
                "d2",
                &doc_ref(&ma, "d2").snapshot_id,
            ),
        )
        .unwrap();

        tick();
        let b = point(env, "B");
        let mb = manifest(env, &b);
        assert_ne!(
            doc_ref(&ma, "d1").snapshot_id,
            doc_ref(&mb, "d1").snapshot_id
        );
        assert_ne!(
            doc_ref(&ma, "d2").snapshot_id,
            doc_ref(&mb, "d2").snapshot_id
        );
        assert_eq!(
            crate::time_travel_store::read_document_bytes(&env.graph_dir(), &b, "d1").unwrap(),
            sidecar(env, "d1"),
            "B holds the live bytes, not A's damaged ones"
        );
        #[cfg(unix)]
        {
            assert_eq!(links(&bytes_path(env, &b, "d1")), 1);
            assert_eq!(links(&bytes_path(env, &b, "d2")), 1);
        }
        assert_point_matches_oracle(env, &b, &oracle(env, &ids));
    });
}

/// The cost of a capture follows the changed documents, not all of them: an untouched graph writes no
/// document bytes and pays a fixed handful of fsyncs however many documents it holds, and the spans say so.
#[test]
fn unchanged_graph_capture_costs_a_fixed_number_of_fsyncs() {
    with_env(|env| {
        let ids: Vec<String> = (0..12).map(|n| format!("doc-{n:02}")).collect();
        for id in &ids {
            create(env, id);
            write(env, id, &format!("{id} text"));
        }
        let fsyncs_and_span = |label: &str| {
            let (provider, exporter, dispatch) = crate::telemetry::test_support::capture(true);
            let id = tracing::dispatcher::with_default(&dispatch, || point(env, label));
            provider.force_flush().unwrap();
            let spans = exporter.get_finished_spans().unwrap();
            let fsyncs = spans
                .iter()
                .flat_map(|span| span.events.events.iter())
                .filter(|event| event.name == "fsync")
                .count();
            let attr = |span: &str, key: &str| -> String {
                spans
                    .iter()
                    .find(|s| s.name == span)
                    .and_then(|s| s.attributes.iter().find(|kv| kv.key.as_str() == key))
                    .map(|kv| kv.value.to_string())
                    .unwrap_or_else(|| panic!("{span} lacks {key}"))
            };
            let per_document = spans
                .iter()
                .filter(|s| s.name == "restore_point.document")
                .count();
            (
                id,
                fsyncs,
                attr(
                    "restore_point.capture",
                    "sophia.restore_point.documents_reused",
                ),
                attr("restore_point.capture", "sophia.restore_point.documents"),
                per_document,
            )
        };

        let (first, first_fsyncs, first_reused, _, _) = fsyncs_and_span("first");
        assert_eq!(first_reused, "0");
        assert!(
            first_fsyncs >= ids.len(),
            "a first capture writes every document: {first_fsyncs} fsyncs"
        );
        tick();
        let (second, second_fsyncs, second_reused, second_documents, per_document) =
            fsyncs_and_span("second");
        assert_eq!(second_reused, "12");
        assert_eq!(second_documents, "12");
        assert_eq!(per_document, 12, "one span per document");
        assert!(
            second_fsyncs <= 14,
            "an unchanged capture is a fixed handful of fsyncs, not one per document: {second_fsyncs}"
        );
        assert_point_matches_oracle(
            env,
            &first,
            &oracle(env, &ids.iter().map(String::as_str).collect::<Vec<_>>()),
        );
        assert_point_matches_oracle(
            env,
            &second,
            &oracle(env, &ids.iter().map(String::as_str).collect::<Vec<_>>()),
        );

        // One changed document: one document's worth more, still independent of the other eleven.
        tick();
        write(env, "doc-03", "changed");
        let (_, third_fsyncs, third_reused, _, _) = fsyncs_and_span("third");
        assert_eq!(third_reused, "11");
        assert!(
            third_fsyncs < first_fsyncs,
            "{third_fsyncs} vs {first_fsyncs}"
        );
    });
}

fn decodes_to_text(bytes: &[u8]) -> String {
    use yrs::updates::decoder::Decode;
    use yrs::{Doc, Transact, Update};
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
    let doc = Doc::new();
    doc.transact_mut()
        .apply_update(Update::decode_v1(bytes).expect("a point's bytes are a Yjs update"))
        .expect("apply");
    node_text(&crate::crdt_engine::projection::ydoc_to_tiptap_json(&doc))
}

/// The capture no longer holds the graph lease across the walk: writers keep landing while points are being
/// taken, no point fails, every point lists every document with bytes that decode, and a later quiet point
/// matches its oracle exactly (so reuse across points that raced writers never reused stale content).
#[test]
fn points_taken_while_writers_run_are_complete_and_later_points_are_exact() {
    with_env(|env| {
        let ids: Vec<String> = (0..16).map(|n| format!("doc-{n:02}")).collect();
        for id in &ids {
            create(env, id);
            write(env, id, &format!("{id} v0"));
        }
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let writes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let writer = {
            let (app, ids, stop, writes) =
                (env.app.clone(), ids.clone(), stop.clone(), writes.clone());
            std::thread::spawn(move || {
                let mut round = 1;
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    for id in &ids {
                        run(crate::document_mutation_service::mcp_local_write_document(
                            app.clone(),
                            &json!({"graph_id": GRAPH, "document_id": id,
                                    "tiptapJson": {"type": "doc", "content": [paragraph("a", &format!("{id} v{round}"))]}}),
                        ))
                        .unwrap();
                        writes.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        if stop.load(std::sync::atomic::Ordering::Relaxed) {
                            break;
                        }
                    }
                    round += 1;
                }
            })
        };

        let mut points = Vec::new();
        for n in 0..3 {
            points.push(point(env, &format!("racing-{n}")));
        }
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        writer.join().expect("writer thread");
        assert!(
            writes.load(std::sync::atomic::Ordering::Relaxed) > 0,
            "the writer ran"
        );

        for id in &points {
            let manifest = manifest(env, id);
            assert_eq!(
                manifest.documents.len(),
                ids.len(),
                "{id} lists every document"
            );
            for doc in &ids {
                let bytes =
                    crate::time_travel_store::read_document_bytes(&env.graph_dir(), id, doc)
                        .unwrap();
                assert!(
                    decodes_to_text(&bytes).starts_with(doc.as_str()),
                    "{id}/{doc}: the point holds a whole version of the document"
                );
            }
        }

        // Quiet again: a point is exact, and the next one reuses it all.
        tick();
        let quiet = point(env, "quiet");
        let want = oracle(env, &ids.iter().map(String::as_str).collect::<Vec<_>>());
        assert_point_matches_oracle(env, &quiet, &want);
        tick();
        let again = point(env, "again");
        assert_point_matches_oracle(env, &again, &want);
        let diff = env
            .call(
                "diff_restore_points",
                json!({"restorePointId": again, "against": quiet}),
            )
            .unwrap();
        assert_eq!(diff["aggregate"]["docsModified"], 0, "{diff}");
    });
}
