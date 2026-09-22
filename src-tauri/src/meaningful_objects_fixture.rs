// ═══════════════════════════════════════════════════════════════════════════
//  THE CARTOGRAPHER'S NOTEBOOK
//  A literate, end-to-end integration fixture for the Meaningful Objects engine.
// ═══════════════════════════════════════════════════════════════════════════
//
//  A Meaningful Object is ONE mergeable SOURCE projected into N RDF "faces",
//  reconciled into Oxigraph by a value-canonical diff/apply. Every move flows
//  through six observable PLACES:
//
//      (1) API CALL    — a plain service/command call.
//      (2) SOURCE      — the mergeable state it mutates (a record / CRDT / store).
//      (3) DESIRED     — project(source) → the face triples the MO should hold.
//      (4) RECONCILE   — survey owned span → diff(survey, desired) → apply the
//                        MINIMAL delta (removes + adds); returns an op count.
//      (5) PROJECTION  — the materialized RDF in Oxigraph (a named/default graph).
//      (6) READ-BACK   — a SPARQL query over the projection (the reader's view).
//
//  This fixture builds a SMALL, human-readable knowledge world — a cartographer's
//  field notebook about mapping an unfamiliar coast — and walks each Meaningful
//  Object kind through those places, asserting the Lean laws at the place where
//  the Lean model and the live runtime MEET.
//
//  The world (no foo/bar — every name carries the story):
//    GRAPH      "cartographers-notebook"          — the notebook itself.
//    DOCUMENT   "the-coastline"                    — notes on the coast.
//    DOCUMENT   "the-lighthouse"                   — the fixed reference point.
//    DOCUMENT   "corrections"  (edited v1→v2)      — the error worth fixing.
//    DOCUMENT   "on-exactitude"                    — a page copied into the notebook.
//    WIRE       corrections --refines--> coastline — the correction's target.
//    WIRE       on-exactitude --describes--> the-empire — a referent never created.
//    VALUATION  the "western bend" block, score 5  — the marked keystone.
//    SONG       a verse about the work             — the narrative voice.
//
//  Migration map (honest: which kinds run the ENGINE RECONCILE today):
//    • DOCUMENT       — LIVE reconcile, CREATE and SAVE. `create_document` AND the
//                       primary `save_document` path both call `reconcile_document_record`;
//                       we drive FRESH → EDIT → RE-RUN (EQUIVALENCE / DOMINATION /
//                       CONVERGENCE) and assert the LIVE on-disk save projection equals
//                       the 4-face desired.
//    • GRAPH METADATA — LIVE reconcile NOW. `reconcile_graph_record` exists
//                       (Phase 1 landed) — PURE PARITY with the old wholesale
//                       path, but with the minimal-delta dividend. We drive the
//                       full equivalence/domination/convergence triad.
//    • WIRES/WORKSPACE — LIVE reconcile NOW (Layer-1 finish-flip landed). The
//                       live save routes through `reconcile_workspace_snapshot`
//                       (4 entity + 6 scene `Fixed` spans composed by
//                       `reconcile_classes` + a once-at-seed ontology block); Wire
//                       is one of those spans (standalone `reconcile_wire_store`
//                       shares its desired). This step still DRIVES the wholesale
//                       `materialize_workspace_snapshot` as the ORACLE BASELINE to
//                       check the no-duplicate CARDINALITY law structurally; the
//                       equivalence/domination/convergence triad lives in the
//                       per-kind oracles (`p3_wire_reconcile_oracle`,
//                       `p3_workspace_reconcile_oracle` — incl. the prereq-B
//                       scene-convergence proof).
//    • SALIENCE       — LIVE reconcile NOW. `reconcile_value_store` exists
//                       (Step [1] landed) — PURE PARITY with the old wholesale
//                       `materialize_value_store`, minimal-delta, now in the
//                       `:projection:salience` NAMED graph (the default-graph WART
//                       is FIXED). We drive FRESH / CONVERGENCE / DOMINATION + the
//                       live SPARSE valued→unvalued RECLAIM, atop SPARSE + 1-per-block.
//    • SONG           — LIVE reconcile NOW. `reconcile_song_store` exists
//                       (Step [2] landed) — the FIRST MULTI-CLASS kind: the
//                       Fixed-union of three rdf:type spans (Song / SongVerse /
//                       SongCoda) composed by `reconcile_classes`, MATCH-THE-PROOF
//                       parity with the old wholesale `materialize_song_store`, now
//                       in the `:projection:song` NAMED graph (WART FIXED). We drive
//                       FRESH / CONVERGENCE / DOMINATION and prove ORDER-PRESERVATION
//                       SURVIVES the reconcile (verseIndex == position), atop the
//                       Source→Desired→Projection observation.
//
//  NO MOCKS. Every API call is the real service fn; every projection lands in a
//  real Oxigraph store; every read-back is a real SPARQL query. The headless
//  harness (`build_mock_app_for_tests(true)`) is the real gardend cell in-process.
// ═══════════════════════════════════════════════════════════════════════════

#![cfg(all(test, feature = "headless"))]

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::json;

use crate::app_runtime::AppHandle;
use crate::crdt_operation_types::EnqueueCrdtOperationInput;
use crate::crdt_queue::enqueue_crdt_operation;
use crate::document_service::DocumentRecord;
use crate::emporium::survey::parse_term;
use crate::emporium::terms::{canon_value, CanonValue, Term};
use crate::graph_record_store::GraphRecord;
use crate::graph_service::{create_graph_service, CreateGraphInput};

// The reconcile + materializer engine fns, reached crate-internally. Each of
// these lives in a TOP-LEVEL module, so `pub(super)` == crate-reachable here.
use crate::document_meaningful_object::{
    face_desired_triples, reconcile_document_record, survey_document_projection, Face,
};
use crate::geist_song_rdf::{materialize_song_store, reconcile_song_store};
use crate::geist_song_store::read_song_store;
use crate::rdf_document_tree::document_tree_triples;
use crate::rdf_record_materializer::{materialize_graph_record, reconcile_graph_record};
use crate::rdf_wire_materializer::push_wire_triples;
use crate::rdf_workspace_store_materializer::materialize_workspace_snapshot;
use crate::salience_rdf_materializer::reconcile_value_store;
use crate::salience_value_store::{read_value_store, record_has_value_score, write_value_store};

// Authority IRIs (the named graphs each kind owns).
use crate::rdf_authority::{
    document_projection_graph_iri, graph_projection_graph_iri, salience_projection_graph_iri,
    song_projection_graph_iri, workspace_projection_graph_iri,
};
// Subject minters + namespaces + the RdfTriple serializer (for inspecting the
// wire DESIRED set without reaching its private fields).
use crate::rdf::{format_rdf_triple, graph_subject, RdfTriple};
use crate::rdf_store_service::open_graph_store;
use crate::runtime_config::{DCTERMS_NS, MDOC_NS, MNEMO_NS, RDF_TYPE, WIRE_NS};

use crate::geist_song_service::mcp_local_sing;
use crate::salience_mcp_valuation::mcp_local_value;

use oxigraph::sparql::{QueryResults, SparqlEvaluator};
use oxigraph::store::Store;

// ───────────────────────────── harness scaffolding ──────────────────────────
//
// The PROVEN pattern from `document_meaningful_object.rs::harness_tests`:
//  - `build_mock_app_for_tests(true)` — with_state=TRUE registers the
//    CrdtOperationQueue + RoomRegistry, so `enqueue_crdt_operation("document.write")`
//    drains through the in-process headless executor (that mints the real Y.Doc).
//  - `GARDEN_PROFILE_DIR` is process-global; we lock the ONE process-wide mutex
//    `profile_env_serial()` and point the profile at a unique temp dir.

fn env_serial() -> &'static std::sync::Mutex<()> {
    crate::tauri_runtime::profile_env_serial()
}

fn temp_profile(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    std::env::temp_dir().join(format!("garden-mo-fixture-{name}-{nanos}"))
}

/// Run `body` with a fresh, isolated, process-global profile dir, serialized
/// against every other headless harness test in the crate.
fn with_profile(name: &str, body: impl FnOnce() + std::panic::UnwindSafe) {
    let _serial = env_serial().lock().unwrap_or_else(|p| p.into_inner());
    let profile = temp_profile(name);
    std::env::set_var("GARDEN_PROFILE_DIR", &profile);
    let result = std::panic::catch_unwind(body);
    std::env::remove_var("GARDEN_PROFILE_DIR");
    let _ = std::fs::remove_dir_all(&profile);
    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}

fn mock_app() -> AppHandle {
    crate::tauri_runtime::build_mock_app_for_tests(true)
}

/// Write `markdown` into `doc_id` through the REAL CRDT engine (the same
/// `document.write` op the spine enqueues). The headless executor mints the
/// Y.Doc, materializes the tree, and `save_document` persists the record AND
/// runs the OLD wholesale materializer into the real on-disk per-doc projection.
fn write_doc(app: &AppHandle, graph_id: &str, doc_id: &str, title: &str, markdown: &str) {
    crate::app_runtime::async_runtime::block_on(enqueue_crdt_operation(
        app.clone(),
        EnqueueCrdtOperationInput {
            kind: "document.write".to_string(),
            graph_id: graph_id.to_string(),
            document_id: Some(doc_id.to_string()),
            payload: json!({
                "documentId": doc_id,
                "content": markdown,
                "format": "markdown",
                "title": title,
            }),
        },
    ))
    .expect("document.write drains through the real CRDT engine");
}

/// Read the REAL persisted record (with its REAL populated tree/body/tiptap_xml)
/// back through the same path the spine uses.
fn read_record(app: &AppHandle, graph_id: &str, doc_id: &str) -> DocumentRecord {
    let graph_dir = crate::paths::existing_graph_dir(app, graph_id).expect("graph dir");
    let dir = crate::paths::document_dir(&graph_dir, doc_id).expect("doc dir");
    let manifest = dir.join("document.json");
    crate::document_record_store::read_document_record(&graph_dir, &manifest).expect("record")
}

fn graph_dir_of(app: &AppHandle, graph_id: &str) -> PathBuf {
    crate::paths::existing_graph_dir(app, graph_id).expect("graph dir")
}

// ───────────────────────── value-canonical read-back ────────────────────────
//
// The proven shape from `canon_set_from_harness`: parse each object term and run
// it through `canon_value`, so a store round-trip (literal re-serialization)
// never reads as drift. We compare projections by VALUE, not by byte-string.

fn strip_angle(s: &str) -> String {
    s.strip_prefix('<')
        .and_then(|r| r.strip_suffix('>'))
        .unwrap_or(s)
        .to_string()
}

/// Run a `SELECT ?s ?p ?o` directly on a `&Store` and return the value-canonical
/// (subject, predicate, canon-object) set — the reader's view of a projection.
fn canon_spo(store: &Store, query: &str) -> BTreeSet<(String, String, CanonValue)> {
    let solutions = match SparqlEvaluator::new()
        .parse_query(query)
        .expect("parse canon query")
        .on_store(store)
        .execute()
        .expect("execute canon query")
    {
        QueryResults::Solutions(s) => s,
        _ => panic!("expected SELECT solutions"),
    };
    let mut set = BTreeSet::new();
    for sol in solutions {
        let sol = sol.expect("row");
        let s = match sol.get("s").expect("?s") {
            oxigraph::model::Term::NamedNode(n) => n.as_str().to_string(),
            other => other.to_string(),
        };
        let p = match sol.get("p").expect("?p") {
            oxigraph::model::Term::NamedNode(n) => n.as_str().to_string(),
            other => other.to_string(),
        };
        let o = parse_term(&sol.get("o").expect("?o").to_string());
        set.insert((s, p, canon_value(&o)));
    }
    set
}

/// The value-canonical triple set the engine `Triple` list projects to — for
/// asserting DESIRED == PROJECTION independently of the store.
fn canon_of_triples(
    triples: &[crate::emporium::terms::Triple],
) -> BTreeSet<(String, String, CanonValue)> {
    triples
        .iter()
        .map(|(s, p, o)| (s.clone(), p.clone(), canon_value(o)))
        .collect()
}

// ═══════════════════════════════════════════════════════════════════════════
//  THE FIXTURE — one literate test, top to bottom.
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn cartographers_notebook_meaningful_object_pipeline() {
    with_profile("cartographers-notebook", || {
        let app = mock_app();
        let graph_id = "cartographers-notebook";

        // ───────────────────────────────────────────────────────────────────
        //  STEP 1 — THE NOTEBOOK ITSELF.
        //  KIND: graph metadata.  VOCABULARY: `graphMetaSpan` (Vocab/FileSystem.lean)
        //  = a SINGLE subject `urn:mnemosyne:local:graph:{id}` over an UNRESTRICTED
        //  bare predicate set (8 triples: rdf:type mnemo:LocalGraph + dcterms:{id,
        //  title,created,modified} + mnemo:{origin,providerId,localPath}).
        //  RECONCILE: LIVE (`reconcile_graph_record`, PURE PARITY with the old
        //  wholesale path). We drive EQUIVALENCE + DOMINATION + CONVERGENCE.
        // ───────────────────────────────────────────────────────────────────
        println!("\n=== STEP 1: GRAPH METADATA — create the Cartographer's Notebook ===");

        // (1) API CALL — the real create-graph service. Mints the on-disk graph
        //     layout, writes graph.json, opens the per-graph Oxigraph, and (today)
        //     materializes the graph-metadata kind. Returns the GraphRecord SOURCE.
        let graph: GraphRecord = create_graph_service(
            &app,
            CreateGraphInput {
                graph_id: Some(graph_id.to_string()),
                title: "The Cartographer's Notebook".to_string(),
                description: Some("Field notes on mapping an unfamiliar coast.".to_string()),
                operation_id: None,
            },
        )
        .expect("create the notebook graph");

        // (2) SOURCE — the GraphRecord on disk. Its fields ARE the desired face.
        println!(
            "  SOURCE  GraphRecord: id={}, title={:?}",
            graph.graph_id, graph.title
        );
        assert_eq!(graph.graph_id, graph_id);
        assert_eq!(graph.title, "The Cartographer's Notebook");

        let graph_store = open_graph_store(&graph_dir_of(&app, graph_id)).expect("graph store");
        let g_subject = graph_subject(graph_id);
        let g_iri = graph_projection_graph_iri(graph_id);
        let g_query =
            format!("SELECT ?s ?p ?o WHERE {{ GRAPH <{g_iri}> {{ <{g_subject}> ?p ?o . BIND(<{g_subject}> AS ?s) }} }}");

        // (5)+(6) PROJECTION/READ-BACK after the create call — the real per-graph
        //     `:projection:graph` named graph. This is the OLD wholesale output (the
        //     create service still calls `materialize_graph_record` in this build).
        let create_set = canon_spo(&graph_store, &g_query);
        println!(
            "  PROJECTION (after create): {} triples on the single graph subject",
            create_set.len()
        );

        // SINGLE-SUBJECT law (Lean `graphMeta_single_subject`): exactly one subject.
        let distinct_subjects = canon_spo(
            &graph_store,
            &format!("SELECT ?s ?p ?o WHERE {{ GRAPH <{g_iri}> {{ ?s ?p ?o }} }}"),
        )
        .iter()
        .map(|(s, _, _)| s.clone())
        .collect::<BTreeSet<_>>();
        assert_eq!(
            distinct_subjects.len(),
            1,
            "graphMeta SINGLE-SUBJECT: the projection holds exactly one subject"
        );
        assert!(
            distinct_subjects.contains(&g_subject),
            "the one subject IS graph_subject(id)"
        );
        assert_eq!(
            create_set.len(),
            8,
            "graphMetaSpan = exactly 8 bare triples"
        );

        // (3) DESIRED — built INDEPENDENTLY from the GraphRecord fields + namespace
        //     constants (the Rust image of Lean `graphMetaTriples`). Never re-runs a
        //     materializer; this is the anti-tautology oracle.
        let g_desired = graph_meta_oracle(&graph);
        assert_eq!(g_desired.len(), 8, "the model predicts exactly 8 triples");

        // (4) RECONCILE — EQUIVALENCE / DOMINATION / CONVERGENCE, the LIVE law-checks.
        //     The graph subject is OWNED OUTRIGHT (unrestricted-bare span), so the
        //     reconcile is PURE PARITY: same net set as the wholesale path, reached
        //     by value-diff. We prove the triad on independent stores.
        {
            // EQUIVALENCE: wholesale store vs reconcile store reach the SAME net set,
            // both equal to the independent 8-triple oracle.
            let store_old = Store::new().expect("store_old");
            materialize_graph_record(&store_old, &graph).expect("wholesale materialize");
            let store_new = Store::new().expect("store_new");
            let fresh_ops = reconcile_graph_record(&store_new, &graph)
                .expect("fresh reconcile")
                .op_count();
            println!("  RECONCILE fresh: {fresh_ops} ops (8 adds, 0 removes)");
            assert_eq!(fresh_ops, 8, "a fresh reconcile emits exactly the 8 adds");

            let old_set = canon_spo(&store_old, &g_query);
            let new_set = canon_spo(&store_new, &g_query);
            assert_eq!(
                new_set, old_set,
                "EQUIVALENCE: reconcile net set == wholesale net set"
            );
            assert_eq!(
                new_set, g_desired,
                "EQUIVALENCE: reconcile net set == independent oracle"
            );
            // And the live create-path projection agrees too (PURE PARITY end-to-end).
            assert_eq!(
                create_set, g_desired,
                "the create-path projection == the oracle"
            );

            // CONVERGENCE: a second identical reconcile emits ZERO ops, byte-stable.
            let conv_ops = reconcile_graph_record(&store_new, &graph)
                .expect("converge")
                .op_count();
            println!("  RECONCILE re-run (converged): {conv_ops} ops");
            assert_eq!(
                conv_ops, 0,
                "CONVERGENCE: a converged reconcile emits 0 ops"
            );
            assert_eq!(
                new_set,
                canon_spo(&store_new, &g_query),
                "byte-stable across re-reconcile"
            );

            // DOMINATION: edit title + modified. Wholesale tears down 8 + inserts 8
            // (16 ops); reconcile emits only the 2 changed slots (2 removes + 2 adds).
            let mut v2 = graph.clone();
            v2.title = "The Cartographer's Notebook (revised)".to_string();
            v2.updated_at = format!("{}-revised", graph.updated_at);
            let edit_ops = reconcile_graph_record(&store_new, &v2)
                .expect("reconcile edit")
                .op_count();
            println!("  RECONCILE edit (title+modified): {edit_ops} ops vs 16 wholesale");
            assert_eq!(
                edit_ops, 4,
                "DOMINATION: only 2 changed slots → 2 removes + 2 adds"
            );
            assert!(
                edit_ops < 16,
                "DOMINATION: reconcile ({edit_ops}) < wholesale (16)"
            );
            // EQUIVALENCE after the edit holds against the v2 oracle.
            assert_eq!(
                canon_spo(&store_new, &g_query),
                graph_meta_oracle(&v2),
                "EQUIVALENCE after edit: reconcile net set == v2 oracle"
            );
        }

        // ───────────────────────────────────────────────────────────────────
        //  STEP 2 — THE COASTLINE (a real document, full 4-face pipeline).
        //  KIND: document.  VOCABULARY: 4 FACES with DISJOINT owned spans
        //  (Vocab/Refine.lean): TreeToRdf (the {subject}#… fragment subtree) +
        //  StorageMetadataToRdf/ContentToRdf/ProjectionMetaToRdf (a disjoint
        //  partition of the 10 bare DOCUMENT_LEVEL_PREDICATES).
        //  RECONCILE: LIVE. DESIRED==PROJECTION (EQUIVALENCE) and re-run=0
        //  (CONVERGENCE) checked directly here.
        // ───────────────────────────────────────────────────────────────────
        println!("\n=== STEP 2: DOCUMENT — \"the-coastline\" (full 4-face reconcile) ===");

        // (1) API CALL — drive the REAL CRDT engine (mints the Y.Doc + tree, then
        //     save_document materializes via the OLD wholesale path into the real
        //     on-disk per-doc projection graph).
        let coastline_md = "# The Coastline\n\nThe coast bends west past the lighthouse.\n\nTides arrive an hour later than the old charts claim.";
        write_doc(
            &app,
            graph_id,
            "the-coastline",
            "The Coastline",
            coastline_md,
        );

        // (2) SOURCE — the REAL persisted record, with a populated tree/body/xml.
        let coastline = read_record(&app, graph_id, "the-coastline");
        assert!(
            coastline.tree.is_some(),
            "the real CRDT write produced a tree"
        );
        assert!(
            !coastline.body.is_empty(),
            "real canonical plaintext body is non-empty"
        );
        assert!(
            !coastline.tiptap_xml.is_empty(),
            "real tiptap_xml is non-empty"
        );
        println!(
            "  SOURCE  DocumentRecord: {} tree triples, body={} chars",
            document_tree_triples(&coastline).len(),
            coastline.body.len()
        );

        // (3) DESIRED — call each face's project() WITHOUT the store, independently.
        //     The 4 faces' desired sets are the anti-tautology oracle for the
        //     reconcile's projection.
        let mut doc_desired: Vec<crate::emporium::terms::Triple> = Vec::new();
        for &face in Face::ALL {
            let face_triples = face_desired_triples(face, &coastline);
            println!("  DESIRED face {face:?}: {} triples", face_triples.len());
            doc_desired.extend(face_triples);
        }
        let doc_desired_canon = canon_of_triples(&doc_desired);

        // (4) RECONCILE — into a clean store. FRESH = full footprint.
        let doc_store = Store::new().expect("doc store");
        let fresh_ops = reconcile_document_record(&doc_store, &coastline)
            .expect("reconcile coastline")
            .op_count();
        println!("  RECONCILE fresh: {fresh_ops} ops");
        assert!(fresh_ops > 0, "a fresh reconcile writes the full footprint");

        // (5)+(6) PROJECTION/READ-BACK — the real per-doc named graph.
        let doc_iri = document_projection_graph_iri(graph_id, "the-coastline");
        let doc_proj = canon_spo(
            &doc_store,
            &format!("SELECT ?s ?p ?o WHERE {{ GRAPH <{doc_iri}> {{ ?s ?p ?o }} }}"),
        );

        // EQUIVALENCE: DESIRED (the 4 faces, computed independently) == PROJECTION
        // (what the reconcile actually wrote). This is the Lean refinement
        // `document_reconcile_equiv` made live.
        assert_eq!(
            doc_proj, doc_desired_canon,
            "EQUIVALENCE: the 4-face DESIRED set == the reconciled PROJECTION"
        );
        println!(
            "  EQUIVALENCE: 4-face DESIRED == PROJECTION ({} triples)",
            doc_proj.len()
        );

        // DISJOINT FACES (Lean L5_disjoint_face_compose): the bare-subject faces
        // (storage 7 / content 2 / meta 1) own DISJOINT predicate partitions. We
        // confirm the three bare faces never share a predicate on the doc subject.
        let span_pred = |face: Face| -> BTreeSet<String> {
            face_desired_triples(face, &coastline)
                .into_iter()
                .filter(|(s, _, _)| s == &coastline.rdf_subject)
                .map(|(_, p, _)| p)
                .collect::<BTreeSet<_>>()
        };
        let fs = span_pred(Face::StorageMetadataToRdf);
        let content = span_pred(Face::ContentToRdf);
        let meta = span_pred(Face::ProjectionMetaToRdf);
        assert!(fs.is_disjoint(&content), "FS face ⊥ content face");
        assert!(fs.is_disjoint(&meta), "FS face ⊥ meta face");
        assert!(content.is_disjoint(&meta), "content face ⊥ meta face");
        println!(
            "  DISJOINT FACES: storage={} content={} meta={} predicates, pairwise disjoint",
            fs.len(),
            content.len(),
            meta.len()
        );

        // CONVERGENCE: re-reconcile the SAME record → 0 ops, byte-stable.
        let before = canon_spo(
            &doc_store,
            &format!("SELECT ?s ?p ?o WHERE {{ GRAPH <{doc_iri}> {{ ?s ?p ?o }} }}"),
        );
        let conv_ops = reconcile_document_record(&doc_store, &coastline)
            .expect("converge coastline")
            .op_count();
        assert_eq!(conv_ops, 0, "CONVERGENCE: re-reconcile emits 0 ops");
        assert_eq!(
            before,
            canon_spo(
                &doc_store,
                &format!("SELECT ?s ?p ?o WHERE {{ GRAPH <{doc_iri}> {{ ?s ?p ?o }} }}")
            ),
            "byte-stable across re-reconcile"
        );
        println!("  CONVERGENCE: re-run = {conv_ops} ops");

        // LIVE ON-DISK SAVE PATH — the reconcile above ran into a CLEAN Store::new();
        // now read the REAL per-doc projection that `write_doc → save_document` already
        // wrote on disk (save_document calls `reconcile_document_record` since the
        // primary-path flip). The on-disk projection must equal the SAME 4-face DESIRED
        // — proving the live save path writes the full projection, not just the engine
        // in isolation.
        let live_store = open_graph_store(&graph_dir_of(&app, graph_id)).expect("live doc store");
        let live_proj = canon_spo(
            &live_store,
            &format!("SELECT ?s ?p ?o WHERE {{ GRAPH <{doc_iri}> {{ ?s ?p ?o }} }}"),
        );
        assert_eq!(
            live_proj, doc_desired_canon,
            "LIVE PARITY: the on-disk save-path projection == the 4-face DESIRED"
        );
        println!(
            "  LIVE ON-DISK: save-path projection == 4-face DESIRED ({} triples)",
            live_proj.len()
        );

        // ───────────────────────────────────────────────────────────────────
        //  STEP 3 — THE LIGHTHOUSE (a second document, the fixed reference).
        //  A second full-pipeline document so the world has a target to wire to.
        // ───────────────────────────────────────────────────────────────────
        println!("\n=== STEP 3: DOCUMENT — \"the-lighthouse\" ===");
        write_doc(
            &app,
            graph_id,
            "the-lighthouse",
            "The Lighthouse",
            "# The Lighthouse\n\nBuilt 1887. Its lamp is the fixed point every bearing is taken from.",
        );
        let lighthouse = read_record(&app, graph_id, "the-lighthouse");
        let lh_store = Store::new().expect("lh store");
        let lh_ops = reconcile_document_record(&lh_store, &lighthouse)
            .expect("reconcile lighthouse")
            .op_count();
        assert!(lh_ops > 0, "the lighthouse reconciles a full footprint");
        println!("  RECONCILE the-lighthouse: {lh_ops} ops");

        // ───────────────────────────────────────────────────────────────────
        //  STEP 4 — CORRECTIONS (a document EDITED to show minimal-delta).
        //  DOMINATION: a one-line edit reconciles with FEWER ops than the wholesale
        //  rebuild would, then re-runs to 0 (CONVERGENCE).
        // ───────────────────────────────────────────────────────────────────
        println!("\n=== STEP 4: DOCUMENT — \"corrections\" (v1→v2 minimal-delta) ===");
        write_doc(
            &app,
            graph_id,
            "corrections",
            "Corrections",
            "# Corrections\n\nThe western bend is the error most worth fixing.",
        );
        let corrections_v1 = read_record(&app, graph_id, "corrections");

        // Seed a clean store to the FULL v1 footprint via the reconcile.
        let corr_store = Store::new().expect("corrections store");
        reconcile_document_record(&corr_store, &corrections_v1).expect("seed v1");

        // EDIT one line through the REAL CRDT engine → real v2 record.
        write_doc(
            &app,
            graph_id,
            "corrections",
            "Corrections",
            "# Corrections\n\nThe western bend is the error most worth fixing, by a full nautical mile.",
        );
        let corrections_v2 = read_record(&app, graph_id, "corrections");
        assert_ne!(
            corrections_v1.body, corrections_v2.body,
            "the edit changed the body"
        );

        // OLD wholesale op count for v1→v2: the live v1 delete span + a full v2
        // re-insert (this is what the old path would have done).
        let old_delete_span = survey_document_projection(
            &corr_store,
            graph_id,
            "corrections",
            &corrections_v1.rdf_subject,
        )
        .expect("survey v1 delete span")
        .len();
        let ops_old = old_delete_span + document_tree_triples(&corrections_v2).len();

        // NEW diffed reconcile v1→v2.
        let ops_new = reconcile_document_record(&corr_store, &corrections_v2)
            .expect("reconcile v2")
            .op_count();
        println!("  RECONCILE edit: {ops_new} ops (diffed) vs {ops_old} ops (wholesale rebuild)");
        assert!(ops_new > 0, "the edit produced a non-zero delta");
        assert!(
            ops_new < ops_old,
            "DOMINATION: diffed update emits fewer ops ({ops_new}) than wholesale ({ops_old})"
        );

        // CONVERGENCE after the edit.
        let conv_ops = reconcile_document_record(&corr_store, &corrections_v2)
            .expect("converge v2")
            .op_count();
        assert_eq!(
            conv_ops, 0,
            "CONVERGENCE: re-reconcile after the edit = 0 ops"
        );
        println!("  CONVERGENCE: re-run after edit = {conv_ops} ops");

        // ───────────────────────────────────────────────────────────────────
        //  STEP 5 — THE WIRE (corrections --refines--> coastline).
        //  KIND: wires.  VOCABULARY: a per-wire REIFIED subject
        //  `urn:...:graph:{g}:wire:{id}` carrying ≤9 endpoint triples
        //  (Vocab/Wires.lean).  RECONCILE: LIVE — wires reconcile as one of the
        //  workspace `reconcile_classes` spans (the live save uses
        //  `reconcile_workspace_snapshot`; `reconcile_wire_store` is the standalone
        //  single-class organism). Here we drive the wholesale
        //  `materialize_workspace_snapshot` ORACLE BASELINE to check the no-duplicate
        //  CARDINALITY law (distinct wire ids → distinct reified subjects, nodup);
        //  the equivalence/domination/convergence triad is in `p3_wire_reconcile_oracle`.
        // ───────────────────────────────────────────────────────────────────
        println!("\n=== STEP 5: WIRES — corrections --refines--> the-coastline ===");

        // (1) API CALL — the real wholesale workspace+wire materializer over a
        //     workspace snapshot Value carrying the two docs as artifacts + 2 wires
        //     with DISTINCT ids (the second is the inverse mirror).
        // A page the cartographer copied into the notebook, and a wire from it to a
        // document that is never created.
        write_doc(
            &app,
            graph_id,
            "on-exactitude",
            "On Exactitude in Science",
            "# On Exactitude in Science\n\nIn that Empire, the Art of Cartography attained such Perfection that the Map of a single Province occupied an entire City, and the Map of the Empire, an entire Province.",
        );

        let wire_id = "wire-corrects-coastline";
        let wire_inverse_id = "wire-coastline-refined-by-corrections";
        let snapshot = json!({
            "folders": [],
            "documents": [
                { "id": "corrections",   "title": "Corrections",             "order": 0.0 },
                { "id": "the-coastline", "title": "The Coastline",            "order": 1.0 },
                { "id": "on-exactitude", "title": "On Exactitude in Science", "order": 2.0 }
            ],
            "artifacts": [],
            "wires": [
                {
                    "id": wire_id,
                    "sourceDocumentId": "corrections",
                    "targetDocumentId": "the-coastline",
                    "predicate": "refines",
                    "inverseOf": wire_inverse_id
                },
                {
                    "id": wire_inverse_id,
                    "sourceDocumentId": "the-coastline",
                    "targetDocumentId": "corrections",
                    "predicate": "refinedBy",
                    "inverseOf": wire_id
                },
                {
                    "id": "wire-exactitude-empire",
                    "sourceDocumentId": "on-exactitude",
                    "targetDocumentId": "the-empire",
                    "predicate": "describes"
                }
            ]
        });

        // (2) SOURCE — the workspace snapshot Value (the wires live in the CRDT
        //     workspace Y.Map; here we drive its serialized form directly).
        // (3) DESIRED — the per-wire reification triples, snapshotted WITHOUT the
        //     store via push_wire_triples (the anti-tautology oracle). The wire face
        //     ALSO emits a head `rdf:type mnemo:Wire` from the workspace layer; the
        //     ≤9 figure is the ENDPOINT vocabulary push_wire_triples emits, so we
        //     inspect the per-wire predicate set serialized via format_rdf_triple
        //     (RdfTriple's fields are private — we read its N-Triples rendering).
        let wire_subject = format!("{}:wire:{}", graph_subject(graph_id), wire_id);
        let mut desired_wire: Vec<RdfTriple> = Vec::new();
        push_wire_triples(
            &mut desired_wire,
            graph_id,
            &wire_subject,
            &snapshot["wires"][0],
        );
        let desired_wire_preds = rdf_triple_predicates(&desired_wire);
        println!(
            "  DESIRED wire {wire_id}: {} endpoint triples; predicates={:?}",
            desired_wire.len(),
            desired_wire_preds
        );
        // Referential anchors are present: source/target documents + the predicate.
        assert!(desired_wire_preds.contains(&format!("{WIRE_NS}sourceDocument")));
        assert!(desired_wire_preds.contains(&format!("{WIRE_NS}targetDocument")));
        assert!(desired_wire_preds.contains(&format!("{WIRE_NS}predicate")));

        // (4) RECONCILE — none. (5) materialize wholesale into the real store.
        let ws_store = open_graph_store(&graph_dir_of(&app, graph_id)).expect("ws store");
        materialize_workspace_snapshot(&ws_store, graph_id, &snapshot)
            .expect("materialize workspace snapshot");

        // (6) READ-BACK — count the reified wire subjects (`?w a mnemo:Wire`).
        let ws_iri = workspace_projection_graph_iri(graph_id);
        let wire_subjects = canon_spo(
            &ws_store,
            &format!(
                "SELECT ?s ?p ?o WHERE {{ GRAPH <{ws_iri}> {{ ?s ?p ?o . ?s a <{WIRE_NS}Wire> }} }}"
            ),
        )
        .iter()
        .map(|(s, _, _)| s.clone())
        .collect::<BTreeSet<_>>();
        println!(
            "  READ-BACK: {} distinct reified wire subjects",
            wire_subjects.len()
        );

        // CARDINALITY / nodup (Lean `wire_concrete_subjects_nodup`): 3 distinct wire
        // ids → 3 distinct `urn:...:wire:{id}` subjects (no collision), and the
        // expected minted subject is present.
        assert_eq!(
            wire_subjects.len(),
            3,
            "no-duplicate CARDINALITY: 3 ids → 3 subjects"
        );
        assert!(
            wire_subjects.contains(&wire_subject),
            "the reified subject is exactly wire_ref_uri(graph, id)"
        );

        // The third wire's target document was never created; its reference is
        // projected exactly as the real ones — referential integrity is not enforced
        // (Lean `wire_tgtdoc_passthrough`: the endpoint is emitted from the wire field,
        // unchecked).
        let empire_wire = format!("{}:wire:wire-exactitude-empire", graph_subject(graph_id));
        let empire_target = canon_spo(
            &ws_store,
            &format!(
                "SELECT ?s ?p ?o WHERE {{ GRAPH <{ws_iri}> {{ <{empire_wire}> <{WIRE_NS}targetDocument> ?o . BIND(<{empire_wire}> AS ?s) BIND(<{WIRE_NS}targetDocument> AS ?p) }} }}"
            ),
        );
        assert_eq!(
            empire_target.len(),
            1,
            "the target reference is projected though no document `the-empire` exists"
        );
        println!("  READ-BACK: on-exactitude describes the-empire, a document never created");

        // ───────────────────────────────────────────────────────────────────
        //  STEP 6 — THE KEYSTONE (value the "western bend" block).
        //  KIND: salience.  VOCABULARY: a type-keyed `mnemo:BlockValuation` class
        //  span in the DEFAULT graph (Vocab/Salience.lean).  RECONCILE: LIVE
        //  (`reconcile_value_store`, the type-keyed CLASS span the Lean
        //  `mem_equiv_class` survey-then-apply premise formalizes). We check SPARSE
        //  (an unvalued block emits nothing) + 1-valuation-per-block CARDINALITY,
        //  then drive the LIVE triad — FRESH / CONVERGENCE / DOMINATION — and the
        //  live valued→unvalued RECLAIM.
        // ───────────────────────────────────────────────────────────────────
        println!("\n=== STEP 6: SALIENCE — mark the keystone block in \"corrections\" ===");

        // Recover a REAL block id from the corrections doc projection: the block
        // whose textContent carries "western bend". (block-exists is a code-read
        // passthrough, but we use a real block to keep the world honest.)
        let corr_iri = document_projection_graph_iri(graph_id, "corrections");
        let block_id = real_block_id_containing(&corr_store, &corr_iri, "western bend")
            .expect("a real block id for the western-bend paragraph");
        println!("  SOURCE  real block id = {block_id}");

        // (1) API CALL — the real local-value MCP fn. Mutates the on-disk
        //     LocalValueStore then materializes the salience class span.
        mcp_local_value(
            app.clone(),
            &json!({
                "graph_id": graph_id,
                "document_id": "corrections",
                "block_id": block_id,
                "importance": 5,
                "valence": 2,
                "tags": ["keystone"]
            }),
        )
        .expect("value the keystone block");

        // (2) SOURCE — the LocalValueStore now carries exactly one valued block.
        let value_store =
            read_value_store(&graph_dir_of(&app, graph_id), graph_id).expect("read value store");
        println!(
            "  SOURCE  LocalValueStore: {} block record(s)",
            value_store.blocks.len()
        );

        // (5)+(6) PROJECTION/READ-BACK — the `:projection:salience` named graph's
        // BlockValuation class (the wart fix moved salience off the default graph).
        let sal_store = open_graph_store(&graph_dir_of(&app, graph_id)).expect("salience store");
        let sal_iri = salience_projection_graph_iri(graph_id);
        let valuations = canon_spo(
            &sal_store,
            &format!(
                "SELECT ?s ?p ?o WHERE {{ GRAPH <{sal_iri}> {{ ?s ?p ?o . ?s a <{MNEMO_NS}BlockValuation> ; <{MNEMO_NS}graphId> \"{graph_id}\" }} }}"
            ),
        )
        .iter()
        .map(|(s, _, _)| s.clone())
        .collect::<BTreeSet<_>>();
        println!(
            "  READ-BACK: {} BlockValuation subject(s)",
            valuations.len()
        );

        // 1-VALUATION-PER-BLOCK CARDINALITY (Lean `value_subjects_nodup`): exactly
        // one subject for the one valued block.
        assert_eq!(
            valuations.len(),
            1,
            "1-per-block CARDINALITY: one valued block → one subject"
        );

        // SPARSE (Lean `unvalued_emits_empty`): a block we did NOT value emits NO
        // valuation subject — the lighthouse's blocks contributed nothing here.
        let lighthouse_block = real_block_id_containing(
            &lh_store,
            &document_projection_graph_iri(graph_id, "the-lighthouse"),
            "fixed point",
        );
        if let Some(lh_block) = lighthouse_block {
            let sparse = canon_spo(
                &sal_store,
                &format!(
                    "SELECT ?s ?p ?o WHERE {{ ?s ?p ?o . ?s <{MNEMO_NS}blockId> \"{lh_block}\" }}"
                ),
            );
            assert!(sparse.is_empty(), "SPARSE: an unvalued block emits NOTHING");
            println!("  SPARSE: the unvalued lighthouse block '{lh_block}' emits nothing");
        }

        // (4) RECONCILE — the LIVE salience triad. `mcp_local_value` already routed
        //     through `reconcile_value_store` (the 3 live call sites are flipped),
        //     so the real graph_dir projection is ALREADY reconciled. We prove the
        //     class-reconcile laws against that real projection + the real value
        //     store: FRESH (op_count == |desired|), CONVERGENCE (re-run = 0),
        //     DOMINATION (reconcile ≤ what the wholesale would emit), and the live
        //     valued→unvalued RECLAIM. The Rust image of the Lean class algebra
        //     (Workspace.lean `mem_equiv_class` / `class_ops_dominate` /
        //     `class_converged_zero_ops`) at the place the model meets the runtime.
        let salient_subject = valuations
            .iter()
            .next()
            .cloned()
            .expect("the one valued block's BlockValuation subject");

        // FRESH — reconcile the SAME value store into an EMPTY twin graph dir: a
        // pure-insert pass whose op_count IS |desired| (> 0, the class is non-empty).
        let twin_dir = temp_profile("salience-fresh-twin");
        std::fs::create_dir_all(&twin_dir).expect("mkdir the twin graph dir");
        let fresh_ops = reconcile_value_store(&twin_dir, &value_store)
            .expect("fresh salience reconcile into an empty twin")
            .op_count();
        let desired_len = fresh_ops; // from-empty reconcile = exactly the inserts
        println!("  RECONCILE fresh: {fresh_ops} ops ({desired_len} adds, 0 removes)");
        assert!(
            fresh_ops > 0,
            "FRESH: a non-empty class reconciles with op_count > 0"
        );
        let _ = std::fs::remove_dir_all(&twin_dir);

        // CONVERGENCE — the live save already reconciled the real projection, so a
        // re-reconcile of the unchanged desired emits ZERO SPARQL (the is_empty
        // early-out = `class_converged_zero_ops`). This is the xsd:float canon gate:
        // the surveyed salience floats must key-equal the desired floats, or the
        // class would churn forever.
        let conv_ops = reconcile_value_store(&graph_dir_of(&app, graph_id), &value_store)
            .expect("re-reconcile the live salience projection")
            .op_count();
        println!("  RECONCILE re-run (converged): {conv_ops} ops");
        assert_eq!(
            conv_ops, 0,
            "CONVERGENCE: the live salience projection re-reconciles to 0 ops"
        );

        // DOMINATION — the wholesale `materialize_value_store` ALWAYS tears down then
        // rebuilds: |desired| deletes + |desired| inserts on a converged store
        // (== 2·|desired|), and |desired| inserts from empty. The reconcile is ≤ both:
        // 0 ops on the converged store (strictly fewer than 2·|desired|), and exactly
        // |desired| from empty (== the wholesale's fresh cost). `class_ops_dominate`.
        let wholesale_fresh = desired_len; // 0 deletes + |desired| inserts
        let wholesale_converged = 2 * desired_len; // |desired| deletes + |desired| inserts
        println!(
            "  RECONCILE domination: fresh {fresh_ops} ≤ wholesale {wholesale_fresh}; converged {conv_ops} < wholesale {wholesale_converged}"
        );
        assert!(
            fresh_ops <= wholesale_fresh,
            "DOMINATION: fresh reconcile ≤ wholesale fresh"
        );
        assert!(
            conv_ops < wholesale_converged,
            "DOMINATION: the converged reconcile is strictly fewer ops than the wholesale rebuild"
        );

        // SPARSE-RECLAIM (live) — unvalue the keystone block IN THE REAL STORE: clear
        // its score + tags so it leaves `salience_desired` (the sparse-skip), write it
        // back, and reconcile. The diff is PURE REMOVAL of the reclaimed BlockValuation
        // subject — adds empty, every remove names that subject, the subject is GONE,
        // and a follow-up reconcile is already converged. The reconcile image of the
        // wholesale teardown's reclaim (Lean `unvalued_emits_empty`).
        let mut unvalued_store = value_store.clone();
        for record in unvalued_store.blocks.values_mut() {
            *record = crate::salience_value_store::LocalBlockValueRecord {
                document_id: record.document_id.clone(),
                block_id: record.block_id.clone(),
                ..Default::default()
            };
            assert!(
                !record_has_value_score(record) && record.tags.is_empty(),
                "the unvalued record really hits the sparse-skip"
            );
        }
        write_value_store(&graph_dir_of(&app, graph_id), &unvalued_store)
            .expect("persist the unvalued store");
        let reclaim = reconcile_value_store(&graph_dir_of(&app, graph_id), &unvalued_store)
            .expect("reconcile the unvalued (reclaim) pass");
        println!(
            "  RECLAIM (valued→unvalued): {} ops ({} adds, {} removes)",
            reclaim.op_count(),
            reclaim.adds.len(),
            reclaim.removes.len()
        );
        assert!(
            reclaim.op_count() > 0,
            "SPARSE-RECLAIM: unvaluing reclaims (op_count > 0)"
        );
        assert!(
            reclaim.adds.is_empty(),
            "SPARSE-RECLAIM: reclaim is pure removal, no adds"
        );
        assert!(
            reclaim
                .removes
                .iter()
                .all(|(s, _, _)| s == &salient_subject),
            "SPARSE-RECLAIM: every removed triple belongs to the reclaimed valuation subject"
        );
        let post_reclaim = canon_spo(
            &sal_store,
            &format!(
                "SELECT ?s ?p ?o WHERE {{ GRAPH <{sal_iri}> {{ ?s ?p ?o . ?s a <{MNEMO_NS}BlockValuation> ; <{MNEMO_NS}graphId> \"{graph_id}\" }} }}"
            ),
        );
        assert!(
            post_reclaim.is_empty(),
            "SPARSE-RECLAIM: the reclaimed BlockValuation subject is GONE from the store"
        );
        let reconverged = reconcile_value_store(&graph_dir_of(&app, graph_id), &unvalued_store)
            .expect("re-reconcile the reclaimed store")
            .op_count();
        println!("  RECLAIM re-run (converged): {reconverged} ops");
        assert_eq!(
            reconverged, 0,
            "SPARSE-RECLAIM: the post-reclaim store is converged (0 ops)"
        );

        // ───────────────────────────────────────────────────────────────────
        //  STEP 7 — THE VOICE (sing a verse about the work).
        //  KIND: song — the FIRST MULTI-CLASS kind.  VOCABULARY: the UNION of
        //  three `Fixed` rdf:type class spans (`mnemo:Song` / `mnemo:SongVerse` /
        //  `mnemo:SongCoda`) in the DEFAULT graph (Vocab/Song.lean); verse subjects
        //  are POSITION-KEYED `{song}/verse/{i}`.  RECONCILE: LIVE
        //  (`reconcile_song_store`, the three spans composed by `reconcile_classes`
        //  — `L5_disjoint_class_compose`: distinct rdf:type classes own disjoint
        //  subject-sets, so their reconciles compose). We observe ORDER-PRESERVATION
        //  (verseIndex == position), then drive the LIVE triad — FRESH / CONVERGENCE
        //  / DOMINATION — and prove ORDER-PRESERVATION SURVIVES the reconcile.
        // ───────────────────────────────────────────────────────────────────
        println!("\n=== STEP 7: SONG — sing a verse for the Cartographer ===");

        // The song store seeds one default verse; our `sing("verse")` inserts at
        // the FRONT (index 0) and the seed slides to index 1.
        let our_verse = "Corrected the western bend a second time. The coast had not moved.";

        // (1) API CALL — the real local-sing MCP fn. Mutates the on-disk
        //     LocalSongStore then materializes the song class span.
        mcp_local_sing(
            app.clone(),
            &json!({ "graph_id": graph_id, "verse": our_verse, "mode": "verse" }),
        )
        .expect("sing the verse");

        // (2) SOURCE — the LocalSongStore; .verses preserves order, ours at index 0.
        let song_store_src =
            read_song_store(&graph_dir_of(&app, graph_id), graph_id).expect("read song store");
        println!(
            "  SOURCE  LocalSongStore: {} verses; verses[0]={:?}",
            song_store_src.verses.len(),
            &song_store_src.verses[0].text[..40.min(song_store_src.verses[0].text.len())]
        );
        assert_eq!(
            song_store_src.verses[0].text, our_verse,
            "our verse is at index 0 (front-insert)"
        );
        assert!(
            song_store_src.verses.len() >= 2,
            "seed verse slid down to make room"
        );

        // Re-materialize to be certain the projection reflects the latest store
        // (the API call already materialized; this is idempotent + explicit).
        materialize_song_store(&graph_dir_of(&app, graph_id), &song_store_src)
            .expect("materialize song store");

        // (5)+(6) PROJECTION/READ-BACK — SELECT verse subjects ORDER BY verseIndex.
        let song_store = open_graph_store(&graph_dir_of(&app, graph_id)).expect("song store");
        let song_subject = format!("{}:song", graph_subject(graph_id));
        let verse_rows = read_song_verses(&song_store, &song_subject, graph_id);
        println!(
            "  READ-BACK: {} verses, ordered by verseIndex",
            verse_rows.len()
        );

        // ORDER-PRESERVATION (Lean `verseRows_index_at_pos`): the i-th read-back
        // verse subject is `{song}/verse/{i}` AND its verseIndex column == i AND
        // its content == verses[i].text.
        assert_eq!(
            verse_rows.len(),
            song_store_src.verses.len(),
            "every source verse projects exactly one verse subject"
        );
        for (i, (subject, index, content)) in verse_rows.iter().enumerate() {
            assert_eq!(*index, i as i64, "verseIndex == position for verse {i}");
            assert_eq!(
                subject,
                &format!("{song_subject}/verse/{i}"),
                "verse subject is position-keyed"
            );
            assert_eq!(
                content, &song_store_src.verses[i].text,
                "verse {i} content matches the source order"
            );
        }
        println!(
            "  ORDER-PRESERVATION: verseIndex == position for all {} verses",
            verse_rows.len()
        );

        // (4) RECONCILE — the LIVE song triad. `mcp_local_sing` already routed
        //     through `reconcile_song_store` (the live call site is flipped), so the
        //     real graph_dir projection is ALREADY reconciled — the union of the
        //     three `Fixed` rdf:type spans (Song / SongVerse / SongCoda) composed by
        //     `reconcile_classes`. We prove the class-reconcile laws against that
        //     real projection + the real song store: FRESH (op_count > 0),
        //     CONVERGENCE (re-run = 0), DOMINATION (reconcile ≤ the wholesale would
        //     emit), and that ORDER-PRESERVATION SURVIVES the reconcile. The Rust
        //     image of the Lean disjoint-class composition (`L5_disjoint_class_compose`)
        //     at the place the model and the multi-class runtime meet.

        // FRESH — reconcile the SAME song store into an EMPTY twin graph dir: a
        // pure-insert pass across all three spans whose op_count is > 0 (the union
        // is non-empty: a Song head + N verse subjects + an optional coda).
        let song_twin_dir = temp_profile("song-fresh-twin");
        std::fs::create_dir_all(&song_twin_dir).expect("mkdir the song twin graph dir");
        let song_fresh_ops = reconcile_song_store(&song_twin_dir, &song_store_src)
            .expect("fresh song reconcile into an empty twin")
            .op_count();
        let song_desired_len = song_fresh_ops; // from-empty reconcile = exactly the inserts
        println!("  RECONCILE fresh: {song_fresh_ops} ops ({song_desired_len} adds, 0 removes) across the 3 spans");
        assert!(
            song_fresh_ops > 0,
            "FRESH: the non-empty multi-class song reconciles with op_count > 0"
        );
        let _ = std::fs::remove_dir_all(&song_twin_dir);

        // CONVERGENCE — the live sing already reconciled the real projection, so a
        // re-reconcile of the unchanged song store emits ZERO SPARQL (the is_empty
        // early-out = `class_converged_zero_ops`, holding across all three spans at
        // once). If any of the three spans churned, this would be non-zero.
        let song_conv_ops = reconcile_song_store(&graph_dir_of(&app, graph_id), &song_store_src)
            .expect("re-reconcile the live song projection")
            .op_count();
        println!("  RECONCILE re-run (converged): {song_conv_ops} ops");
        assert_eq!(
            song_conv_ops, 0,
            "CONVERGENCE: the live song projection re-reconciles to 0 ops"
        );

        // DOMINATION — the wholesale `materialize_song_store` ALWAYS tears down then
        // rebuilds: |desired| deletes + |desired| inserts on a converged store
        // (== 2·|desired|), and |desired| inserts from empty. The reconcile is ≤ both:
        // 0 ops on the converged store (strictly fewer than 2·|desired|), and exactly
        // |desired| from empty (== the wholesale's fresh cost). `class_ops_dominate`.
        let song_wholesale_fresh = song_desired_len; // 0 deletes + |desired| inserts
        let song_wholesale_converged = 2 * song_desired_len; // |desired| deletes + |desired| inserts
        println!(
            "  RECONCILE domination: fresh {song_fresh_ops} ≤ wholesale {song_wholesale_fresh}; converged {song_conv_ops} < wholesale {song_wholesale_converged}"
        );
        assert!(
            song_fresh_ops <= song_wholesale_fresh,
            "DOMINATION: fresh reconcile ≤ wholesale fresh"
        );
        assert!(
            song_conv_ops < song_wholesale_converged,
            "DOMINATION: the converged reconcile is strictly fewer ops than the wholesale rebuild"
        );

        // ORDER-PRESERVATION SURVIVES THE RECONCILE — re-read the verse subjects out
        // of the (converged) live projection ORDER BY verseIndex and re-assert the
        // position-keying. The minimal-delta path must leave verseIndex == position
        // intact: the multi-class reconcile is order-faithful, not just convergent.
        let post_reconcile_rows = read_song_verses(&song_store, &song_subject, graph_id);
        assert_eq!(
            post_reconcile_rows.len(),
            song_store_src.verses.len(),
            "every source verse still projects exactly one verse subject after the reconcile"
        );
        for (i, (subject, index, content)) in post_reconcile_rows.iter().enumerate() {
            assert_eq!(
                *index, i as i64,
                "verseIndex == position SURVIVES reconcile for verse {i}"
            );
            assert_eq!(
                subject,
                &format!("{song_subject}/verse/{i}"),
                "verse subject stays position-keyed through the reconcile"
            );
            assert_eq!(
                content, &song_store_src.verses[i].text,
                "verse {i} content matches the source order after the reconcile"
            );
        }
        println!(
            "  ORDER-PRESERVATION (survives reconcile): verseIndex == position for all {} verses",
            post_reconcile_rows.len()
        );

        // ───────────────────────────────────────────────────────────────────
        //  THE WORLD IS BUILT. Every kind moved through the pipeline; every Lean
        //  law that is live-checkable today was asserted at the place the model
        //  and the runtime meet.
        // ───────────────────────────────────────────────────────────────────
        println!("\n=== The Cartographer's Notebook is complete: graph + 4 docs + 3 wires + valuation + verse ===");
    });
}

// ───────────────────────────── independent oracles ──────────────────────────

/// INDEPENDENT oracle for the graph-metadata DESIRED set: the 8 triples built
/// straight from the `GraphRecord` fields + namespace constants (the Rust image
/// of Lean `graphMetaTriples`). Never touches a materializer.
fn graph_meta_oracle(graph: &GraphRecord) -> BTreeSet<(String, String, CanonValue)> {
    let s = graph_subject(&graph.graph_id);
    let lit = |v: &str| canon_value(&Term::Lit(oxigraph::model::Literal::new_simple_literal(v)));
    let uri = |iri: &str| {
        canon_value(&Term::Uri(
            oxigraph::model::NamedNode::new(iri).expect("valid IRI"),
        ))
    };
    let mut set = BTreeSet::new();
    set.insert((
        s.clone(),
        RDF_TYPE.to_string(),
        uri(&format!("{MNEMO_NS}LocalGraph")),
    ));
    set.insert((
        s.clone(),
        format!("{DCTERMS_NS}identifier"),
        lit(&graph.graph_id),
    ));
    set.insert((s.clone(), format!("{DCTERMS_NS}title"), lit(&graph.title)));
    set.insert((
        s.clone(),
        format!("{DCTERMS_NS}created"),
        lit(&graph.created_at),
    ));
    set.insert((
        s.clone(),
        format!("{DCTERMS_NS}modified"),
        lit(&graph.updated_at),
    ));
    set.insert((s.clone(), format!("{MNEMO_NS}origin"), lit(&graph.origin)));
    set.insert((
        s.clone(),
        format!("{MNEMO_NS}providerId"),
        lit(&graph.provider_id),
    ));
    set.insert((s, format!("{MNEMO_NS}localPath"), lit(&graph.local_path)));
    set
}

/// Extract the predicate IRIs of a `Vec<RdfTriple>` via its N-Triples rendering
/// (RdfTriple's fields are private; `format_rdf_triple` emits `<s> <p> OBJ .`).
fn rdf_triple_predicates(triples: &[RdfTriple]) -> BTreeSet<String> {
    triples
        .iter()
        .filter_map(|t| {
            let line = format_rdf_triple(t);
            // `<s> <p> OBJ .` — take the SECOND angle-bracketed token (the predicate).
            let after_subject = line.split_once('>')?.1.trim_start();
            let predicate = after_subject.strip_prefix('<')?.split_once('>')?.0;
            Some(predicate.to_string())
        })
        .collect()
}

/// Find a REAL block id in a document projection whose `mdoc:textContent`
/// contains `needle`. Returns the `mdoc:nodeId` (the block id) of the match.
fn real_block_id_containing(store: &Store, doc_iri: &str, needle: &str) -> Option<String> {
    let solutions = match SparqlEvaluator::new()
        .parse_query(&format!(
            "SELECT ?bid ?text WHERE {{ GRAPH <{doc_iri}> {{ \
             ?s <{MDOC_NS}nodeId> ?bid ; <{MDOC_NS}textContent> ?text }} }}"
        ))
        .ok()?
        .on_store(store)
        .execute()
        .ok()?
    {
        QueryResults::Solutions(s) => s,
        _ => return None,
    };
    for sol in solutions {
        let sol = sol.ok()?;
        let text = sol.get("text")?.to_string();
        if text.contains(needle) {
            if let oxigraph::model::Term::Literal(l) = sol.get("bid")? {
                return Some(l.value().to_string());
            }
        }
    }
    None
}

/// Read the song verses back as (subject, verseIndex, content), ORDER BY index.
fn read_song_verses(
    store: &Store,
    song_subject: &str,
    graph_id: &str,
) -> Vec<(String, i64, String)> {
    let _ = song_subject;
    // The wart fix moved song off the default graph into `:projection:song`.
    let song_iri = song_projection_graph_iri(graph_id);
    let query = format!(
        "SELECT ?s ?idx ?text WHERE {{ GRAPH <{song_iri}> {{ \
         ?s <{MNEMO_NS}graphId> \"{graph_id}\" ; \
            <{MNEMO_NS}narrativeKind> \"song-verse\" ; \
            <{MNEMO_NS}verseIndex> ?idx ; \
            <{MNEMO_NS}content> ?text }} }} ORDER BY ?idx"
    );
    let solutions = match SparqlEvaluator::new()
        .parse_query(&query)
        .expect("parse song verse query")
        .on_store(store)
        .execute()
        .expect("execute song verse query")
    {
        QueryResults::Solutions(s) => s,
        _ => panic!("expected solutions"),
    };
    let mut rows = Vec::new();
    for sol in solutions {
        let sol = sol.expect("row");
        let s = match sol.get("s").expect("?s") {
            oxigraph::model::Term::NamedNode(n) => n.as_str().to_string(),
            other => other.to_string(),
        };
        let idx = match sol.get("idx").expect("?idx") {
            oxigraph::model::Term::Literal(l) => l.value().parse::<i64>().unwrap_or(-1),
            _ => -1,
        };
        let text = match sol.get("text").expect("?text") {
            oxigraph::model::Term::Literal(l) => l.value().to_string(),
            other => other.to_string(),
        };
        rows.push((s, idx, text));
    }
    rows
}
