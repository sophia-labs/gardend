//! `flow_board_reconcile` — the store-facing half of the Mithras Flow board
//! kind (unit G3, after G2): reconcile a graph's reserved `:projection:flow`
//! lane from the board Y.Doc's `resource` root, with the SAME value-diff
//! primitive every other Meaningful-Object materializer uses
//! (`diff_triples` + `render_updates`, DELETE-then-INSERT, direct-on-store).
//!
//! Split from [`crate::flow_board`] deliberately: that module stays a set of
//! genuinely pure functions over a `yrs::Doc` (no store, no filesystem);
//! everything that touches Oxigraph or `document.json` lives here.
//!
//! FLOW-GS-3 (contracts/garden.md §7): the scene converges as a Y.Doc room;
//! "its RDF form is a declared projection, never an independent authority".
//! Accordingly the lane is written ONLY by materializers — this module and
//! the registered `flow` Emporium pack (unit G1) — and a user `sparql_update`
//! targeting it is refused by the standing `:projection:` prefix reservation
//! (`rdf_authority::validate_sparql_update_authority`; proven by the GS-3
//! acceptance test below).
//!
//! SUBJECT SCOPE (the brief's "subject-scoped to the board's subjects"): the
//! survey collects every triple in the lane whose subject starts with the
//! board's subject-rule prefix `{graph_subject}:projection:flow:`
//! (vocabulary-map.md: `{graph_subject}:projection:flow:{Class}:{localId}`).
//! One graph carries one board (interfaces.md §A: the board document id is
//! the constant `flow-board`), and every subject either door can mint carries
//! that prefix — so the scope is exactly the board's subjects, and a row
//! deleted in the room diffs to a DELETE (its subject still matches the
//! prefix while its triples are absent from `desired`): the delete leg comes
//! free from the value diff, no explicit reclaim pass needed. Verified by
//! `deleting_a_row_reclaims_its_subject` below and by the flush-level test in
//! `crdt_engine::flow_board_flush_tests`. A converged save is zero ops.
//!
//! Consequence, stated honestly: the ROOM is the authority for the whole
//! prefix. Triples written through the Emporium door that the board does not
//! derive (e.g. an `emporium_write` never followed by a board seed) are
//! reclaimed at the next board flush — that is GS-3's authority model working,
//! not a bug. The seed rite (interfaces.md §H) writes both sides from the
//! same validated JSON precisely so the post-seed flush converges to zero ops.

use std::path::Path;

use oxigraph::sparql::{QueryResults, SparqlEvaluator};
use oxigraph::store::Store;
use yrs::Doc;

use crate::document_meaningful_object::graph_wrap_document;
use crate::document_service::DocumentRecord;
use crate::emporium::survey::parse_term;
use crate::emporium::terms::{diff_triples, render_updates, Triple, TripleDiff};
use crate::flow_board::{
    board_desired_triples, board_doc_from_update_base64, board_doc_from_update_bytes,
    board_export_json, board_rows, FLOW_BOARD_KIND,
};
use crate::rdf::graph_subject;
use crate::rdf_authority::flow_projection_graph_iri;

/// Is this record a Mithras Flow board? The one discriminator every cold path
/// branches on (unit G3): the persisted `documentKind` flag the flush copied
/// from the workspace `documents` entry.
pub(crate) fn is_flow_board_record(document: &DocumentRecord) -> bool {
    document.document_kind.as_deref() == Some(FLOW_BOARD_KIND)
}

/// Reconcile the graph's `:projection:flow` lane from a persisted board
/// record. NO-OP (`Ok(empty diff)`) for anything that is not a flow board, so
/// the shared persistence tail can call it unconditionally beside
/// `reconcile_document_record`.
///
/// The board Y.Doc is read from the record's inline `ydoc_update_base64`,
/// falling back to the durable sidecar (`update-v1.bin`) when the inline copy
/// is empty — restore deliberately clears the inline payload of restored
/// records while the sidecar carries the authoritative bytes.
pub(crate) fn reconcile_flow_board_record(
    store: &Store,
    graph_dir: &Path,
    document: &DocumentRecord,
) -> Result<TripleDiff, String> {
    if !is_flow_board_record(document) {
        return Ok(TripleDiff::default());
    }
    let doc = board_doc_for_record(graph_dir, document)?;
    reconcile_flow_board_doc(store, &document.graph_id, &doc)
}

/// Reconcile the `:projection:flow` lane from an in-memory board Doc: survey
/// the board's subject span, compute `desired` via
/// [`crate::flow_board::board_desired_triples`], ONE value-canonical
/// [`diff_triples`], then DELETE-then-INSERT direct-on-store (the
/// materializer's order). Converged board ⇒ zero ops.
pub(crate) fn reconcile_flow_board_doc(
    store: &Store,
    graph_id: &str,
    doc: &Doc,
) -> Result<TripleDiff, String> {
    let gs = graph_subject(graph_id);
    let desired = board_desired_triples(&board_rows(doc), &gs);
    let current = survey_board_span(store, graph_id)?;
    let diff = diff_triples(&current, &desired);

    let lane = flow_projection_graph_iri(graph_id);
    for body in render_updates("DELETE DATA", &diff.removes, 60) {
        run_flow_update(store, &lane, &body)?;
    }
    for body in render_updates("INSERT DATA", &diff.adds, 60) {
        run_flow_update(store, &lane, &body)?;
    }
    Ok(diff)
}

/// The readable content snapshot for a board (unit G3, brief item 1): the
/// canonical-form-C export JSON from G2, derived from the record's Y.Doc
/// (inline payload or sidecar — same fallback as the reconcile). `Ok(None)`
/// for anything that is not a flow board, so history capture can branch in
/// one line. No `sophia` sidecar key: this snapshot is not the export rite
/// (FLOW-GARDEN-P1 owns that shape; a restore point id for a snapshot taken
/// WHILE capturing a restore point does not exist yet).
pub(crate) fn flow_board_snapshot_content(
    graph_dir: &Path,
    document: &DocumentRecord,
) -> Result<Option<String>, String> {
    if !is_flow_board_record(document) {
        return Ok(None);
    }
    let doc = board_doc_for_record(graph_dir, document)?;
    Ok(Some(board_export_json(&doc, None).json))
}

/// Record → scratch board Doc: decode the inline `ydoc_update_base64` when
/// present, else read the durable sidecar file; a record with neither is a
/// valid empty board.
fn board_doc_for_record(graph_dir: &Path, document: &DocumentRecord) -> Result<Doc, String> {
    if !document.ydoc_update_base64.is_empty() {
        return board_doc_from_update_base64(&document.ydoc_update_base64);
    }
    let sidecar = crate::ydoc_paths::document_ydoc_state_path(graph_dir, &document.document_id);
    if sidecar.is_file() {
        let bytes = std::fs::read(&sidecar)
            .map_err(|error| format!("read flow board Y.Doc sidecar: {error}"))?;
        return board_doc_from_update_bytes(&bytes);
    }
    Ok(Doc::new())
}

/// Survey the board's owned span: every triple in the `:projection:flow` lane
/// whose subject carries the board subject-rule prefix (see module doc).
fn survey_board_span(store: &Store, graph_id: &str) -> Result<Vec<Triple>, String> {
    let lane = flow_projection_graph_iri(graph_id);
    let subject_prefix = format!("{lane}:");
    let query = format!(
        r#"SELECT ?s ?p ?o WHERE {{
  GRAPH <{lane}> {{
    ?s ?p ?o .
    FILTER(STRSTARTS(STR(?s), "{subject_prefix}"))
  }}
}}"#
    );
    let solutions = match SparqlEvaluator::new()
        .parse_query(&query)
        .map_err(|e| format!("parse flow board survey: {e}"))?
        .on_store(store)
        .execute()
        .map_err(|e| format!("execute flow board survey: {e}"))?
    {
        QueryResults::Solutions(s) => s,
        _ => return Err("flow board survey expected SELECT solutions".to_string()),
    };

    let mut out = Vec::new();
    for sol in solutions {
        let sol = sol.map_err(|e| format!("flow board survey row: {e}"))?;
        let s = iri_string(sol.get("s").ok_or("flow survey row missing ?s")?)?;
        let p = iri_string(sol.get("p").ok_or("flow survey row missing ?p")?)?;
        let o = parse_term(&sol.get("o").ok_or("flow survey row missing ?o")?.to_string());
        out.push((s, p, o));
    }
    Ok(out)
}

/// Bare IRI string for a subject/predicate binding (the span only ever holds
/// NamedNode subjects/predicates — the subject rule mints IRIs).
fn iri_string(term: &oxigraph::model::Term) -> Result<String, String> {
    match term {
        oxigraph::model::Term::NamedNode(n) => Ok(n.as_str().to_string()),
        other => Err(format!(
            "expected a NamedNode subject/predicate in the flow lane, got {other}"
        )),
    }
}

/// Run one rendered `INSERT DATA`/`DELETE DATA` body against the flow lane,
/// GRAPH-wrapped. DIRECT-ON-STORE, bypassing the authority gate exactly like
/// `run_document_update` / the memory sink — materializers are the sanctioned
/// writers of `:projection:*`. Loud-halt on the first error.
fn run_flow_update(store: &Store, lane_iri: &str, body: &str) -> Result<(), String> {
    let wrapped = graph_wrap_document(body, lane_iri)?;
    SparqlEvaluator::new()
        .parse_update(&wrapped)
        .map_err(|e| format!("parse flow lane update: {e}"))?
        .on_store(store)
        .execute()
        .map_err(|e| format!("execute flow lane update: {e}"))
}

#[cfg(all(test, feature = "headless"))]
mod tests {
    use super::*;
    use crate::flow_board::board_from_json;
    use oxigraph::sparql::QueryResults;

    const GOLDEN_BOARD_JSON: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/flow/golden-board.json"
    ));

    /// 17 rows across the golden board's 13 tables (3 systems, 2 requirements,
    /// 2 tasks, 1 of each remaining table) ⇒ 17 distinct subjects.
    const GOLDEN_ROW_COUNT: usize = 17;

    fn distinct_subject_count(store: &Store, graph_id: &str) -> usize {
        let lane = flow_projection_graph_iri(graph_id);
        let query = format!(
            "SELECT (COUNT(DISTINCT ?s) AS ?n) WHERE {{ GRAPH <{lane}> {{ ?s ?p ?o }} }}"
        );
        let QueryResults::Solutions(mut solutions) = SparqlEvaluator::new()
            .parse_query(&query)
            .expect("parse count query")
            .on_store(store)
            .execute()
            .expect("execute count query")
        else {
            panic!("count query expected solutions");
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

    #[test]
    fn reconcile_populates_reconverges_and_reclaims() {
        let store = Store::new().expect("in-memory store");
        let graph_id = "flow-reconcile-unit";
        let doc = board_from_json(GOLDEN_BOARD_JSON).expect("golden board doc");

        // Fresh lane: every desired triple is an ADD; subjects == rows.
        let first = reconcile_flow_board_doc(&store, graph_id, &doc).expect("first reconcile");
        assert!(first.removes.is_empty(), "fresh lane has nothing to remove");
        assert!(!first.adds.is_empty());
        assert_eq!(distinct_subject_count(&store, graph_id), GOLDEN_ROW_COUNT);

        // Converged board: zero ops (FLOW-GEOM-12's value-canonical invariant).
        let second = reconcile_flow_board_doc(&store, graph_id, &doc).expect("second reconcile");
        assert_eq!(
            second.op_count(),
            0,
            "a converged board must issue zero store ops"
        );

        // Delete one row (task-2) in the doc — its subject must be reclaimed
        // by the value diff's delete leg, nothing else touched.
        {
            use yrs::{Map as YMap, Out, Transact, WriteTxn};
            let mut txn = doc.transact_mut();
            let resource = txn.get_or_insert_map("resource");
            let Some(Out::YMap(tasks)) = resource.get(&txn, "tasks") else {
                panic!("golden board has a tasks table");
            };
            let Some(Out::YMap(rows)) = tasks.get(&txn, "rows") else {
                panic!("tasks table has rows");
            };
            rows.remove(&mut txn, "task-2").expect("task-2 exists");
            let Some(Out::YArray(order)) = tasks.get(&txn, "order") else {
                panic!("tasks table has order");
            };
            use yrs::Array as YArray;
            let idx = (0..order.len(&txn))
                .find(|&i| {
                    matches!(
                        order.get(&txn, i),
                        Some(Out::Any(yrs::Any::String(ref s))) if s.as_ref() == "task-2"
                    )
                })
                .expect("task-2 in order");
            order.remove(&mut txn, idx);
        }
        let third = reconcile_flow_board_doc(&store, graph_id, &doc).expect("third reconcile");
        assert!(third.adds.is_empty(), "a pure deletion adds nothing");
        assert!(!third.removes.is_empty());
        assert_eq!(distinct_subject_count(&store, graph_id), GOLDEN_ROW_COUNT - 1);

        let gs = graph_subject(graph_id);
        let subject = format!("{gs}:projection:flow:task:task-2");
        let lane = flow_projection_graph_iri(graph_id);
        let ask = format!("ASK WHERE {{ GRAPH <{lane}> {{ <{subject}> ?p ?o }} }}");
        let QueryResults::Boolean(present) = SparqlEvaluator::new()
            .parse_query(&ask)
            .expect("parse ask")
            .on_store(&store)
            .execute()
            .expect("execute ask")
        else {
            panic!("ask expected boolean");
        };
        assert!(!present, "deleted row's subject must be reclaimed");
    }

    /// FLOW-GS-3 acceptance (brief item 3): a user `sparql_update` targeting
    /// the `:projection:flow` lane is refused. Proven against the REAL
    /// production gate (`run_sparql_update_service`, the same body the MCP
    /// `sparql_update` tool and the Tauri command execute), not a lookalike.
    /// This is a characterization test of the standing `:projection:` prefix
    /// reservation — expected green from birth; it exists so the reservation
    /// can never silently regress out from under the flow lane.
    #[test]
    fn sparql_update_targeting_flow_lane_is_refused() {
        let _serial = crate::tauri_runtime::profile_env_serial()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let profile =
            std::env::temp_dir().join(format!("garden-flow-gs3-gate-{}", uuid::Uuid::new_v4()));
        std::env::set_var("GARDEN_PROFILE_DIR", &profile);
        let result = std::panic::catch_unwind(|| {
            let app = crate::tauri_runtime::build_mock_app_for_tests(true);
            let graph_id = "flow-gs3-gate";
            crate::graph_service::create_graph_service(
                &app,
                crate::graph_service::CreateGraphInput {
                    title: "Flow GS-3 Gate".to_string(),
                    graph_id: Some(graph_id.to_string()),
                    description: None,
                    operation_id: None,
                },
            )
            .expect("create graph");

            let lane = flow_projection_graph_iri(graph_id);
            for update in [
                format!(
                    "INSERT DATA {{ GRAPH <{lane}> {{ <urn:x:s> <urn:x:p> \"forged\" }} }}"
                ),
                format!("WITH <{lane}> DELETE {{ ?s ?p ?o }} WHERE {{ ?s ?p ?o }}"),
            ] {
                let error = crate::rdf_service::run_sparql_update(
                    app.clone(),
                    crate::rdf_service::SparqlUpdateInput {
                        graph_id: graph_id.to_string(),
                        update,
                    },
                )
                .expect_err("update targeting :projection:flow must be refused");
                assert!(
                    error.contains("reserved"),
                    "refusal names the reserved-graph gate: {error}"
                );
            }

            // The same change is accepted through the sanctioned door: the
            // materializer writes the lane directly (GS-3's second clause,
            // "the room accepts the same change" — here the reconcile applies
            // the room's derived triples where the user update was refused).
            let graph_dir =
                crate::graph_paths::existing_graph_dir(&app, graph_id).expect("graph dir");
            let store = crate::rdf_service::open_graph_store(&graph_dir).expect("store");
            let doc = board_from_json(GOLDEN_BOARD_JSON).expect("golden board doc");
            let diff =
                reconcile_flow_board_doc(&store, graph_id, &doc).expect("materializer write");
            assert!(!diff.adds.is_empty(), "materializer door stays open");
        });
        std::env::remove_var("GARDEN_PROFILE_DIR");
        let _ = std::fs::remove_dir_all(&profile);
        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
    }
}
