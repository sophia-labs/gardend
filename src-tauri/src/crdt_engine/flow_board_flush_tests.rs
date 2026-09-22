//! Unit G3 flush tests — the `flow-board` kind branch at the ONE TipTap seam
//! (`materialize_room_document`), exercised through the REAL persistence
//! pipeline: in-process, temp profile, pattern from `persistence_tests.rs`.
//!
//! Proves, end to end:
//! 1. a workspace entry with `documentKind:"flow-board"` + the golden board's
//!    Y.Doc update in its room flush into `:projection:flow` (COUNT DISTINCT
//!    subjects == golden row count), with `document.json` carrying EMPTY
//!    content fields by construction plus the `documentKind` flag;
//! 2. a converged second flush issues zero lane updates and mints no revision;
//! 3. deleting one row in the room and flushing reclaims exactly its subject
//!    (the value diff's delete leg — no explicit reclaim pass needed);
//! 4. restore-point capture keeps the raw Y.Doc bytes AND a readable board
//!    (the form-C export JSON) as the content snapshot;
//! 5. the workspace lane types the board `flow:Board` beside
//!    `doc:TipTapDocument`;
//! 6. a TipTap document in the same graph still projects exactly as before.

use super::{
    document_ops::{document_write, persist_room_document},
    rooms::RoomRegistry,
    workspace_ops::{persist_workspace, write_workspace_document},
};
use crate::{
    crdt_queue::CrdtOperation,
    document_paths::document_dir,
    document_record_store::read_document_record,
    flow_board::{board_from_json, BOARD_DOCUMENT_ID, FLOW_BOARD_KIND},
    graph_paths::existing_graph_dir,
    graph_service::{create_graph_service, CreateGraphInput},
    rdf_authority::{
        document_projection_graph_iri, flow_projection_graph_iri, workspace_projection_graph_iri,
    },
    ydoc_paths::document_ydoc_state_path,
};
use oxigraph::sparql::{QueryResults, SparqlEvaluator};
use oxigraph::store::Store;
use serde_json::{json, Value};
use std::{panic::UnwindSafe, path::PathBuf};
#[cfg(feature = "desktop")]
use tauri::Manager;
use uuid::Uuid;
use yrs::updates::decoder::Decode;
use yrs::{
    Any, Array as YArray, Map as YMap, Out, ReadTxn, StateVector, Transact, Update, WriteTxn,
};

const GOLDEN_BOARD_JSON: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/flow/golden-board.json"
));

/// 17 rows across the golden board's 13 tables ⇒ 17 distinct flow subjects.
const GOLDEN_ROW_COUNT: u64 = 17;

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

fn count_distinct_flow_subjects(store: &Store, graph_id: &str) -> u64 {
    let lane = flow_projection_graph_iri(graph_id);
    let query =
        format!("SELECT (COUNT(DISTINCT ?s) AS ?n) WHERE {{ GRAPH <{lane}> {{ ?s ?p ?o }} }}");
    let QueryResults::Solutions(mut solutions) = SparqlEvaluator::new()
        .parse_query(&query)
        .expect("parse flow count query")
        .on_store(store)
        .execute()
        .expect("execute flow count query")
    else {
        panic!("flow count expected solutions");
    };
    let sol = solutions
        .next()
        .expect("one aggregate row")
        .expect("aggregate row ok");
    match sol.get("n").expect("count binding") {
        oxigraph::model::Term::Literal(lit) => lit.value().parse().expect("count parses"),
        other => panic!("count binding is not a literal: {other}"),
    }
}

fn ask(store: &Store, query: &str) -> bool {
    let QueryResults::Boolean(value) = SparqlEvaluator::new()
        .parse_query(query)
        .expect("parse ASK")
        .on_store(store)
        .execute()
        .expect("execute ASK")
    else {
        panic!("ASK expected boolean");
    };
    value
}

#[test]
fn flow_board_flush_projects_reconverges_reclaims_and_snapshots() {
    with_profile("garden-flow-board-flush", || {
        crate::app_runtime::async_runtime::block_on(async {
            let app = crate::tauri_runtime::build_mock_app_for_tests(true);
            let graph_id = "flow-board-flush";
            create_graph_service(
                &app,
                CreateGraphInput {
                    title: "Flow Board Flush".to_string(),
                    graph_id: Some(graph_id.to_string()),
                    description: None,
                    operation_id: None,
                },
            )
            .expect("create graph");
            let graph_dir = existing_graph_dir(&app, graph_id).expect("graph dir");

            // Workspace entry with the kind discriminator. The production
            // doc-level mutator writes the known fields; `documentKind` is a
            // generic extra field the client writes over raw CRDT sync — the
            // test writes it the same way, directly on the entry Y.Map.
            let workspace_room = app
                .state::<RoomRegistry>()
                .get_or_create(
                    &format!("workspace:{graph_id}"),
                    crate::ydoc_paths::workspace_ydoc_state_path(&graph_dir),
                )
                .await
                .expect("workspace room");
            workspace_room
                .update_doc(|_doc, txn| {
                    write_workspace_document(
                        txn,
                        &json!({
                            "documentId": BOARD_DOCUMENT_ID,
                            "title": "Mission Board",
                            "order": 1,
                            "updatedAt": 1,
                        }),
                    )?;
                    let documents = txn.get_or_insert_map("documents");
                    let Some(Out::YMap(entry)) = documents.get(&*txn, BOARD_DOCUMENT_ID) else {
                        return Err("board workspace entry missing".to_string());
                    };
                    entry.insert(txn, "documentKind", FLOW_BOARD_KIND);
                    Ok(())
                })
                .await
                .expect("write board workspace entry");
            // Flush the workspace projection now (as the debounced cold flush
            // continuously does in production): restore-point capture walks
            // the COLD workspace snapshot for its document roster, and the
            // workspace lane assertions below need the projection materialized.
            persist_workspace(&app, graph_id, &graph_dir, &workspace_room, "ws-flush-initial")
                .await
                .expect("initial workspace flush");

            // Board room: apply the golden board's Y.Doc update.
            let board_room = app
                .state::<RoomRegistry>()
                .get_or_create(
                    &format!("doc:{graph_id}:{BOARD_DOCUMENT_ID}"),
                    document_ydoc_state_path(&graph_dir, BOARD_DOCUMENT_ID),
                )
                .await
                .expect("board room");
            let golden_doc = board_from_json(GOLDEN_BOARD_JSON).expect("golden board doc");
            let golden_update = {
                let txn = golden_doc.transact();
                txn.encode_state_as_update_v1(&StateVector::default())
            };
            board_room
                .update_doc(move |_doc, txn| {
                    let update = Update::decode_v1(&golden_update)
                        .map_err(|error| format!("decode golden update: {error}"))?;
                    txn.apply_update(update)
                        .map_err(|error| format!("apply golden update: {error}"))
                })
                .await
                .expect("seed board room");

            // ── 1. First flush: board branch, flow lane populated ──────────
            let flushed = persist_room_document(
                &app,
                graph_id,
                BOARD_DOCUMENT_ID,
                "Mission Board",
                &board_room,
                "flow-board-flush-1",
            )
            .await
            .expect("first board flush");
            assert_eq!(flushed["revision"], 1);

            let manifest = document_dir(&graph_dir, BOARD_DOCUMENT_ID)
                .expect("board document dir")
                .join("document.json");
            let raw: Value = crate::storage::read_json(&manifest).expect("board document.json");
            assert_eq!(raw["documentKind"], FLOW_BOARD_KIND, "kind flag persisted");
            assert_eq!(raw["body"], "", "board body empty by construction");
            assert_eq!(raw["tiptapXml"], "", "board tiptapXml empty by construction");
            assert_eq!(raw["blocks"], json!([]), "board blocks empty by construction");
            assert_eq!(raw["tree"], Value::Null, "board tree empty by construction");
            assert!(
                !raw["ydocUpdateBase64"]
                    .as_str()
                    .expect("inline payload is a string")
                    .is_empty(),
                "board Y.Doc bytes saved verbatim"
            );

            let store = crate::rdf_service::open_graph_store(&graph_dir).expect("graph store");
            assert_eq!(
                count_distinct_flow_subjects(&store, graph_id),
                GOLDEN_ROW_COUNT,
                "flow lane subjects == golden row count"
            );
            // The board's per-doc TipTap projection lane carries no tree
            // triples (bare document-level predicates only, all empty-valued).
            let doc_lane = document_projection_graph_iri(graph_id, BOARD_DOCUMENT_ID);
            assert!(
                !ask(
                    &store,
                    &format!(
                        "ASK WHERE {{ GRAPH <{doc_lane}> {{ ?s ?p ?o . \
                         FILTER(CONTAINS(STR(?s), \"#\")) }} }}"
                    )
                ),
                "no TipTap tree/block subjects for a board"
            );

            // ── 2. Converged second flush: zero ops, no new revision ───────
            let reflushed = persist_room_document(
                &app,
                graph_id,
                BOARD_DOCUMENT_ID,
                "Mission Board",
                &board_room,
                "flow-board-flush-noop",
            )
            .await
            .expect("converged board flush");
            assert_eq!(reflushed["revision"], 1, "converged flush mints no revision");
            let record = read_document_record(&graph_dir, &manifest).expect("board record");
            let converged =
                crate::flow_board_reconcile::reconcile_flow_board_record(&store, &graph_dir, &record)
                    .expect("converged reconcile");
            assert_eq!(
                converged.op_count(),
                0,
                "a converged board reconcile issues zero updates"
            );

            // ── 3. Delete one row in the room; flush reclaims its subject ──
            board_room
                .update_doc(|_doc, txn| {
                    let resource = txn.get_or_insert_map("resource");
                    let Some(Out::YMap(tasks)) = resource.get(&*txn, "tasks") else {
                        return Err("tasks table missing".to_string());
                    };
                    let Some(Out::YMap(rows)) = tasks.get(&*txn, "rows") else {
                        return Err("tasks rows missing".to_string());
                    };
                    rows.remove(txn, "task-2")
                        .ok_or_else(|| "task-2 missing".to_string())?;
                    let Some(Out::YArray(order)) = tasks.get(&*txn, "order") else {
                        return Err("tasks order missing".to_string());
                    };
                    let idx = (0..order.len(&*txn))
                        .find(|&i| {
                            matches!(
                                order.get(&*txn, i),
                                Some(Out::Any(Any::String(ref s))) if s.as_ref() == "task-2"
                            )
                        })
                        .ok_or_else(|| "task-2 not in order".to_string())?;
                    order.remove(txn, idx);
                    Ok(())
                })
                .await
                .expect("delete task-2 in room");
            let after_delete = persist_room_document(
                &app,
                graph_id,
                BOARD_DOCUMENT_ID,
                "Mission Board",
                &board_room,
                "flow-board-flush-2",
            )
            .await
            .expect("post-delete board flush");
            assert_eq!(after_delete["revision"], 2, "a board edit is a real revision");
            let flow_lane = flow_projection_graph_iri(graph_id);
            let task2_subject = format!(
                "{}:projection:flow:task:task-2",
                crate::rdf::graph_subject(graph_id)
            );
            assert!(
                !ask(
                    &store,
                    &format!("ASK WHERE {{ GRAPH <{flow_lane}> {{ <{task2_subject}> ?p ?o }} }}")
                ),
                "deleted row's subject reclaimed from the flow lane"
            );
            assert_eq!(
                count_distinct_flow_subjects(&store, graph_id),
                GOLDEN_ROW_COUNT - 1
            );

            // ── 4. Restore point: raw bytes kept + readable board snapshot ─
            let summary = crate::time_travel_service::capture_restore_point(
                &app,
                graph_id,
                crate::time_travel_types::RestorePointTrigger::Manual,
                Some("g3-flow-board".to_string()),
            )
            .expect("capture restore point");
            let restore_point_id = summary["restorePointId"]
                .as_str()
                .expect("restore point id")
                .to_string();
            let rp_manifest = crate::time_travel_store::read_manifest(&graph_dir, &restore_point_id)
                .expect("restore point manifest");
            let board_ref = rp_manifest
                .documents
                .iter()
                .find(|doc_ref| doc_ref.document_id == BOARD_DOCUMENT_ID)
                .expect("board in restore point");
            assert!(
                board_ref.ydoc_bytes_path.is_some(),
                "raw Y.Doc bytes captured for the board"
            );
            let rp_bytes = crate::time_travel_store::read_document_bytes(
                &graph_dir,
                &restore_point_id,
                BOARD_DOCUMENT_ID,
            )
            .expect("restore point board bytes");
            assert!(!rp_bytes.is_empty());
            let payload = crate::document_history_file_store::read_document_snapshot_payload(
                &graph_dir,
                BOARD_DOCUMENT_ID,
                &board_ref.snapshot_id,
            )
            .expect("board snapshot payload");
            let record_after = read_document_record(&graph_dir, &manifest).expect("board record");
            let expected_export = crate::flow_board::board_export_json(
                &crate::flow_board::board_doc_from_update_base64(&record_after.ydoc_update_base64)
                    .expect("decode flushed board"),
                None,
            )
            .json;
            assert_eq!(
                payload.tiptap_xml, expected_export,
                "restore-point content snapshot is the readable form-C export"
            );
            serde_json::from_str::<Value>(&payload.tiptap_xml)
                .expect("board snapshot content parses as JSON");
            // `document_blocks_for_read` synthesizes ONE title-content block
            // for every content-empty record (pre-existing universal
            // behavior, shared by capture and the currency matcher) — a board
            // record is content-empty by construction, so its snapshot
            // carries exactly that synthetic block and no real ones.
            assert_eq!(payload.blocks.len(), 1, "only the synthetic empty-record block");
            assert_eq!(payload.blocks[0].id, "body");
            assert_eq!(payload.blocks[0].content, "Mission Board");

            // ── 5. Workspace lane types the board flow:Board (beside mdoc) ─
            persist_workspace(&app, graph_id, &graph_dir, &workspace_room, "ws-flush")
                .await
                .expect("workspace flush");
            let ws_lane = workspace_projection_graph_iri(graph_id);
            let board_subject = crate::rdf::document_subject(BOARD_DOCUMENT_ID);
            let rdf_type = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";
            assert!(
                ask(
                    &store,
                    &format!(
                        "ASK WHERE {{ GRAPH <{ws_lane}> {{ <{board_subject}> <{rdf_type}> \
                         <urn:sophia:flow:vocab:Board> }} }}"
                    )
                ),
                "workspace lane types the board flow:Board"
            );
            assert!(
                ask(
                    &store,
                    &format!(
                        "ASK WHERE {{ GRAPH <{ws_lane}> {{ <{board_subject}> \
                         <urn:sophia:flow:vocab:documentKind> \"flow-board\" }} }}"
                    )
                ),
                "workspace lane carries flow:documentKind"
            );
            assert!(
                ask(
                    &store,
                    &format!(
                        "ASK WHERE {{ GRAPH <{ws_lane}> {{ <{board_subject}> <{rdf_type}> \
                         <http://mnemosyne.dev/doc#TipTapDocument> }} }}"
                    )
                ),
                "mdoc type stays beside flow:Board (class-span routing)"
            );
        });
    });
}

/// Brief item: "A TipTap document in the same graph still projects as before"
/// — one explicit test. The graph hosts a board-kind workspace entry AND a
/// plain TipTap document; the TipTap flush must take the TipTap branch
/// (workspace entry present, `documentKind` absent) and its flush must leave
/// the flow lane untouched.
#[test]
fn tiptap_document_in_same_graph_still_projects_as_before() {
    with_profile("garden-flow-tiptap-regression", || {
        crate::app_runtime::async_runtime::block_on(async {
            let app = crate::tauri_runtime::build_mock_app_for_tests(true);
            let graph_id = "flow-tiptap-regression";
            let document_id = "plain-note";
            create_graph_service(
                &app,
                CreateGraphInput {
                    title: "Flow TipTap Regression".to_string(),
                    graph_id: Some(graph_id.to_string()),
                    description: None,
                    operation_id: None,
                },
            )
            .expect("create graph");
            let graph_dir = existing_graph_dir(&app, graph_id).expect("graph dir");

            // Hosted workspace room with a kindless entry for the TipTap doc,
            // exercising the live-room kind resolution's None path.
            let workspace_room = app
                .state::<RoomRegistry>()
                .get_or_create(
                    &format!("workspace:{graph_id}"),
                    crate::ydoc_paths::workspace_ydoc_state_path(&graph_dir),
                )
                .await
                .expect("workspace room");
            workspace_room
                .update_doc(|_doc, txn| {
                    write_workspace_document(
                        txn,
                        &json!({
                            "documentId": document_id,
                            "title": "Plain Note",
                            "order": 1,
                            "updatedAt": 1,
                        }),
                    )
                    .map(|_| ())
                })
                .await
                .expect("write tiptap workspace entry");

            document_write(
                &app,
                &CrdtOperation {
                    operation_id: "tiptap-regression-write".to_string(),
                    kind: "document.write".to_string(),
                    graph_id: graph_id.to_string(),
                    document_id: Some(document_id.to_string()),
                    payload: json!({
                        "title": "Plain Note",
                        "tiptapJson": {
                            "type": "doc",
                            "content": [{
                                "type": "paragraph",
                                "attrs": { "data-block-id": "note-block" },
                                "content": [{ "type": "text", "text": "still a tiptap doc" }]
                            }]
                        }
                    }),
                    enqueue_timestamp: "1".to_string(),
                },
            )
            .await
            .expect("tiptap document write");

            let manifest = document_dir(&graph_dir, document_id)
                .expect("document dir")
                .join("document.json");
            let record = read_document_record(&graph_dir, &manifest).expect("tiptap record");
            assert_eq!(record.body, "still a tiptap doc");
            assert_eq!(record.blocks[0].id, "note-block");
            assert!(!record.tiptap_xml.is_empty(), "TipTap XML still projected");
            assert!(
                record.document_kind.is_none(),
                "no kind flag on a TipTap record"
            );

            let store = crate::rdf_service::open_graph_store(&graph_dir).expect("graph store");
            let doc_lane = document_projection_graph_iri(graph_id, document_id);
            assert!(
                ask(
                    &store,
                    &format!("ASK WHERE {{ GRAPH <{doc_lane}> {{ ?s ?p ?o }} }}")
                ),
                "TipTap per-document projection lane still populated"
            );
            assert_eq!(
                count_distinct_flow_subjects(&store, graph_id),
                0,
                "a TipTap flush never touches the flow lane"
            );
        });
    });
}
