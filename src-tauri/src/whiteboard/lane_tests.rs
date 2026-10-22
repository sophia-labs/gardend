//! G2's pure lane tests (INTERFACES.md): the scene in, the triples out, against the shared fixtures. Owned by G2.
//!
//! Real yrs (the cold-record tests encode a real Y.Doc), a real in-memory oxigraph `Store`, the real pack and the
//! real SHACL engine, and the two shared fixtures. No mocks. The store tests (graph, persistence tails, delete,
//! restore, cold seed) are in `whiteboard_lane_store_tests.rs`.

use super::lane::{
    connection_subject, current_triples, desired_triples, reconcile_desired, reconcile_record,
    reconcile_scene, remove_document, BOARD, CARD, CLUSTER, CONNECTION, DOCUMENT_ID, FROM,
    IN_CLUSTER, NS, ON_BOARD, PROMOTED_TO, PROV_GENERATED_AT_TIME, PROV_WAS_ATTRIBUTED_TO,
    RDFS_LABEL, REFERS_TO, SHAPE, SHAPE_KIND, TO,
};
use super::ops;
use super::scene::{Element, Scene};
use crate::document_service::DocumentRecord;
use crate::emporium::terms::Triple;
use base64::Engine as _;
use oxigraph::store::Store;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use yrs::{ReadTxn, Transact};

const V1: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/whiteboard/whiteboard-fixture-v1.json"
));
const V2: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/whiteboard/whiteboard-fixture-v2-semantic.json"
));

const G: &str = "wb-graph";
const DOC: &str = "wb-board";
const TITLE: &str = "Dream-work board";
const D: &str = "urn:mnemosyne:local:document:wb-board";
const RDF_TYPE: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";
const XSD_DATE_TIME: &str = "http://www.w3.org/2001/XMLSchema#dateTime";

const BOX: &str = "box0000000000000000001";
const NOTE: &str = "note000000000000000001";
const LNK: &str = "lnk0000000000000000001";
const CRD: &str = "crd0000000000000000001";
const FRM: &str = "frm0000000000000000001";
const ARW1: &str = "arw0000000000000000001";
const ARW2: &str = "arw0000000000000000002";
const NOTE_TEXT: &str = "txt0000000000000000002";

// ------------------------------------------------------------------ fixtures and spelling

fn scene_of(fixture: &str) -> Scene {
    let parsed: Value = serde_json::from_str(fixture).expect("fixture parses");
    let mut elements = BTreeMap::new();
    for el in parsed["elements"].as_array().expect("elements") {
        let el: Element = el.as_object().expect("element object").clone();
        elements.insert(el["id"].as_str().expect("id").to_string(), el);
    }
    Scene::from_elements(elements)
}

fn memory_store() -> Store {
    Store::new().expect("in-memory store")
}

/// Every triple as one N-Triples-shaped line, so a set of lines compares by value.
fn lines(triples: &[Triple]) -> BTreeSet<String> {
    triples
        .iter()
        .map(|(s, p, o)| format!("<{s}> <{p}> {}", o.as_nt()))
        .collect()
}

fn wb(local: &str) -> String {
    format!("{NS}{local}")
}

fn b(id: &str) -> String {
    format!("{D}#block-{id}")
}

fn a(id: &str) -> String {
    format!("{D}#arrow-{id}")
}

fn iri_line(s: &str, p: &str, o: &str) -> String {
    format!("<{s}> <{p}> <{o}>")
}

fn text_line(s: &str, p: &str, o: &str) -> String {
    format!("<{s}> <{p}> \"{o}\"")
}

fn time_line(s: &str, at: &str) -> String {
    format!("<{s}> <{PROV_GENERATED_AT_TIME}> \"{at}\"^^<{XSD_DATE_TIME}>")
}

/// A shape, card or cluster's lines from INTERFACES.md section 5, spelled out.
#[allow(clippy::too_many_arguments)]
fn node_lines(
    subject: &str,
    class: &str,
    kind: Option<&str>,
    label: &str,
    refers_to: Option<&str>,
    cluster: Option<&str>,
    by: &str,
    at: &str,
) -> Vec<String> {
    let mut out = vec![
        iri_line(subject, RDF_TYPE, class),
        text_line(subject, DOCUMENT_ID, "wb-board"),
        iri_line(subject, ON_BOARD, D),
        text_line(subject, RDFS_LABEL, label),
        iri_line(
            subject,
            PROV_WAS_ATTRIBUTED_TO,
            &format!("urn:sophia:agent:{by}"),
        ),
        time_line(subject, at),
    ];
    if let Some(kind) = kind {
        out.push(text_line(subject, SHAPE_KIND, kind));
    }
    if let Some(target) = refers_to {
        out.push(iri_line(subject, REFERS_TO, target));
    }
    if let Some(frame) = cluster {
        out.push(iri_line(subject, IN_CLUSTER, frame));
    }
    out
}

/// The 58 triples of INTERFACES.md section 5, as lines.
fn expected_v2() -> BTreeSet<String> {
    let mut out: Vec<String> = vec![
        iri_line(D, RDF_TYPE, BOARD),
        text_line(D, DOCUMENT_ID, "wb-board"),
        text_line(D, RDFS_LABEL, "Dream-work board"),
    ];
    out.extend(node_lines(
        &b(BOX),
        &wb("Shape"),
        Some("box"),
        "Freud: the dream-work",
        None,
        None,
        "person",
        "2025-10-09T08:53:20Z",
    ));
    out.extend(node_lines(
        &b(NOTE),
        &wb("Shape"),
        Some("note"),
        "condensation",
        None,
        None,
        "owner",
        "2025-10-09T08:53:20Z",
    ));
    out.extend(node_lines(
        &b(LNK),
        &wb("Shape"),
        Some("box"),
        "The Interpretation of Dreams",
        Some("urn:mnemosyne:local:document:doc-traumdeutung"),
        Some(&b(FRM)),
        "person",
        "2025-10-09T08:53:21Z",
    ));
    out.extend(node_lines(
        &b(CRD),
        &wb("Card"),
        Some("card"),
        "The Wolf Man dream",
        Some("urn:mnemosyne:local:graph:wb-graph:dream:wolf-man"),
        Some(&b(FRM)),
        "demi-1",
        "2025-10-09T08:53:22Z",
    ));
    out.extend(node_lines(
        &b(FRM),
        &wb("Cluster"),
        None,
        "Dream-work",
        None,
        None,
        "person",
        "2025-10-09T08:53:20Z",
    ));
    for (arrow, from, to, label, wire, by, at) in [
        (
            ARW1,
            b(BOX),
            b(NOTE),
            "condenses",
            None,
            "owner",
            "2025-10-09T08:53:20Z",
        ),
        (
            ARW2,
            b(LNK),
            b(CRD),
            "supports",
            Some("urn:mnemosyne:local:graph:wb-graph:wire:wire-wb-0001"),
            "person",
            "2025-10-09T08:53:21Z",
        ),
    ] {
        let s = a(arrow);
        out.extend([
            iri_line(&s, RDF_TYPE, &wb("Connection")),
            text_line(&s, DOCUMENT_ID, "wb-board"),
            iri_line(&s, ON_BOARD, D),
            iri_line(&s, FROM, &from),
            iri_line(&s, TO, &to),
            text_line(&s, RDFS_LABEL, label),
            iri_line(
                &s,
                PROV_WAS_ATTRIBUTED_TO,
                &format!("urn:sophia:agent:{by}"),
            ),
            time_line(&s, at),
        ]);
        if let Some(wire) = wire {
            out.push(iri_line(&s, PROMOTED_TO, wire));
        }
    }
    assert_eq!(out.len(), 58, "the expected set itself is the spec's 58");
    out.into_iter().collect()
}

fn subjects_of(triples: &[Triple]) -> BTreeSet<String> {
    triples.iter().map(|(s, _, _)| s.clone()).collect()
}

fn temp_dir() -> PathBuf {
    std::env::temp_dir().join(format!("wb-lane-{}", uuid::Uuid::new_v4()))
}

/// A real Y.Doc holding the scene's elements the way the browser writes them, as `update-v1` bytes.
fn ydoc_bytes(scene: &Scene) -> Vec<u8> {
    let doc = yrs::Doc::new();
    {
        let mut writing = scene.clone();
        writing.dirty = writing.elements.keys().cloned().collect();
        let mut txn = doc.transact_mut();
        ops::write_dirty(&mut txn, &writing);
    }
    let update = doc
        .transact()
        .encode_state_as_update_v1(&yrs::StateVector::default());
    update
}

fn record(kind: Option<&str>, inline_base64: &str) -> DocumentRecord {
    serde_json::from_value(json!({
        "documentId": DOC, "graphId": G, "title": TITLE, "body": "", "origin": "local",
        "providerId": "local", "localPath": "", "rdfSubject": D,
        "createdAt": "2026-10-21T00:00:00Z", "updatedAt": "2026-10-21T00:00:00Z",
        "capabilities": [], "ydocUpdateBase64": inline_base64, "documentKind": kind,
    }))
    .expect("a document record")
}

fn write_sidecar(graph_dir: &std::path::Path, scene: &Scene) {
    let path = crate::ydoc_paths::document_ydoc_state_path(graph_dir, DOC);
    std::fs::create_dir_all(path.parent().expect("sidecar parent")).expect("sidecar dir");
    std::fs::write(path, ydoc_bytes(scene)).expect("sidecar written");
}

// ------------------------------------------------------------------ tests

#[test]
fn lane_matches_the_scene() {
    let v2 = scene_of(V2);
    let desired = desired_triples(G, DOC, TITLE, &v2).expect("v2 lane");
    assert_eq!(desired.len(), 58, "INTERFACES.md section 5 counts 58");
    assert_eq!(lines(&desired), expected_v2());
    assert_eq!(
        lines(&desired_triples(G, DOC, TITLE, &v2).expect("again")),
        lines(&desired),
        "a pure function of the scene: same scene, same triples"
    );
    let mut sorted = desired.clone();
    sorted.sort_by_cached_key(|(s, p, o)| (s.clone(), p.clone(), o.as_nt()));
    assert_eq!(
        sorted.iter().map(|t| (&t.0, &t.1)).collect::<Vec<_>>(),
        desired.iter().map(|t| (&t.0, &t.1)).collect::<Vec<_>>(),
        "the desired set comes sorted"
    );
    // The card is a Card and only a Card; the frame is the only Cluster; the dangling arrow and the tombstone are not here.
    let typed = |class: &str| -> BTreeSet<String> {
        lines(&desired)
            .into_iter()
            .filter(|l| l.contains(&format!("<{RDF_TYPE}> <{class}>")))
            .collect()
    };
    assert_eq!(typed(CARD).len(), 1);
    assert_eq!(typed(CLUSTER).len(), 1);
    assert_eq!(typed(CONNECTION).len(), 2);
    assert_eq!(typed(SHAPE).len(), 3, "box, note and the linked box");
    let all = lines(&desired).into_iter().collect::<Vec<_>>().join("\n");
    assert!(!all.contains("del0000000000000000001"), "the tombstone");
    assert!(
        !all.contains("arw0000000000000000003"),
        "the dangling arrow"
    );
    assert!(
        !all.contains("txt000000000000000"),
        "bound text is no subject"
    );

    // v1: the board, the box, the note and the `condenses` connection; nothing for the deleted rectangle.
    let v1 = desired_triples(G, DOC, TITLE, &scene_of(V1)).expect("v1 lane");
    assert_eq!(
        subjects_of(&v1),
        BTreeSet::from([D.to_string(), b(BOX), b(NOTE), a(ARW1)])
    );
    assert_eq!(v1.len(), 25, "3 + 7 + 7 + 8");
    assert!(!lines(&v1)
        .iter()
        .any(|l| l.contains("del0000000000000000001")));

    // And a real store converges on exactly that, and stays there.
    let store = memory_store();
    let diff = reconcile_scene(&store, G, DOC, TITLE, &v2).expect("reconcile");
    assert_eq!(diff.adds.len(), 58);
    assert!(diff.removes.is_empty());
    assert_eq!(
        lines(&current_triples(&store, G, DOC).expect("survey")),
        expected_v2()
    );
    let again = reconcile_scene(&store, G, DOC, TITLE, &v2).expect("reconcile again");
    assert!(again.is_empty(), "a converged board changes nothing");
}

#[test]
fn edit_then_reconcile_removes_stale_triples() {
    let store = memory_store();
    let mut scene = scene_of(V2);
    reconcile_scene(&store, G, DOC, TITLE, &scene).expect("first reconcile");
    let before = lines(&current_triples(&store, G, DOC).expect("survey"));
    assert!(before.iter().any(|l| l.contains(CRD)));
    assert!(before.contains(&text_line(&b(NOTE), RDFS_LABEL, "condensation")));

    // The card is deleted and the note is relabelled.
    scene
        .elements
        .get_mut(CRD)
        .expect("card")
        .insert("isDeleted".into(), json!(true));
    for key in ["originalText", "text"] {
        scene
            .elements
            .get_mut(NOTE_TEXT)
            .expect("note label")
            .insert(key.into(), json!("condensation (Verdichtung)"));
    }
    let diff = reconcile_scene(&store, G, DOC, TITLE, &scene).expect("second reconcile");
    assert!(!diff.removes.is_empty() && !diff.adds.is_empty());

    let after = lines(&current_triples(&store, G, DOC).expect("survey"));
    assert!(
        !after.iter().any(|l| l.contains(CRD)),
        "the card, its inCluster and the connection that ended on it are gone: {after:#?}"
    );
    assert!(
        !after.iter().any(|l| l.contains(ARW2)),
        "the connection to the deleted card leaves nothing"
    );
    assert!(after.contains(&text_line(
        &b(NOTE),
        RDFS_LABEL,
        "condensation (Verdichtung)"
    )));
    assert!(
        !after.contains(&text_line(&b(NOTE), RDFS_LABEL, "condensation")),
        "the old label is not left beside the new one"
    );
    assert!(
        after.iter().any(|l| l.contains(&b(LNK))) && after.iter().any(|l| l.contains(&a(ARW1))),
        "what did not change stays"
    );
    assert_eq!(
        after,
        lines(&desired_triples(G, DOC, TITLE, &scene).expect("lane")),
        "the store is exactly the edited scene's lane"
    );
}

#[test]
fn cold_record_reads_the_sidecar() {
    let v2 = scene_of(V2);
    let expected = lines(&desired_triples(G, DOC, TITLE, &v2).expect("lane"));
    assert_eq!(expected.len(), 58);

    // A cold record (no inline Y.Doc) reconciles from the durable sidecar to the same triples.
    let dir = temp_dir();
    write_sidecar(&dir, &v2);
    let store = memory_store();
    let diff = reconcile_record(&store, &dir, &record(Some("whiteboard"), "")).expect("cold");
    assert_eq!(diff.adds.len(), 58);
    assert_eq!(
        lines(&current_triples(&store, G, DOC).expect("survey")),
        expected
    );

    // The inline copy wins when there is one: a v1 record over a v2 sidecar gives v1's 25.
    let inline = base64::engine::general_purpose::STANDARD.encode(ydoc_bytes(&scene_of(V1)));
    let inline_store = memory_store();
    reconcile_record(&inline_store, &dir, &record(Some("whiteboard"), &inline)).expect("inline");
    assert_eq!(
        current_triples(&inline_store, G, DOC)
            .expect("survey")
            .len(),
        25
    );

    // A record with neither is a valid empty board: the lane empties.
    let empty_dir = temp_dir();
    let emptied = reconcile_record(&store, &empty_dir, &record(Some("whiteboard"), ""))
        .expect("a missing sidecar is an empty board");
    // An empty board is still a board: its type, id and title stay (3 triples); the 55 triples of shapes, arrows and
    // authorship go.
    assert_eq!(emptied.removes.len(), 55);
    assert!(emptied.adds.is_empty());
    assert_eq!(current_triples(&store, G, DOC).expect("survey").len(), 3);

    // Any record that is not a whiteboard is a no-op, whatever its sidecar says.
    let filled = memory_store();
    reconcile_scene(&filled, G, DOC, TITLE, &v2).expect("fill");
    let untouched = reconcile_record(&filled, &empty_dir, &record(None, "")).expect("not a board");
    assert!(untouched.is_empty());
    let note = reconcile_record(&filled, &empty_dir, &record(Some("note"), "")).expect("a note");
    assert!(note.is_empty());
    assert_eq!(
        current_triples(&filled, G, DOC).expect("survey").len(),
        58,
        "a non-whiteboard record never empties a board's lane"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn remove_document_empties_only_that_board() {
    let store = memory_store();
    let v2 = scene_of(V2);
    reconcile_scene(&store, G, DOC, TITLE, &v2).expect("first board");
    reconcile_scene(&store, G, "wb-other", "Another board", &v2).expect("second board");
    remove_document(&store, G, DOC).expect("remove");
    assert!(current_triples(&store, G, DOC).expect("survey").is_empty());
    assert_eq!(
        current_triples(&store, G, "wb-other")
            .expect("survey")
            .len(),
        58,
        "the other board keeps its triples"
    );
    remove_document(&store, G, DOC).expect("idempotent");
}

#[test]
fn shacl_refuses_a_rogue_predicate() {
    let store = memory_store();
    let v2 = scene_of(V2);
    let good = desired_triples(G, DOC, TITLE, &v2).expect("lane");
    reconcile_desired(&store, G, DOC, &good).expect("the honest set is accepted");

    // A predicate outside the closed pack: refused, and the lane is left exactly as it was.
    let mut rogue = good.clone();
    rogue.push((
        b(BOX),
        wb("rogue"),
        crate::emporium::terms::Term::Lit(oxigraph::model::Literal::new_simple_literal("x")),
    ));
    let refused = reconcile_desired(&store, G, DOC, &rogue).expect_err("wb:rogue is refused");
    assert!(
        refused.contains("SHACL") && refused.contains("sophia-whiteboard"),
        "{refused}"
    );
    assert_eq!(
        lines(&current_triples(&store, G, DOC).expect("survey")),
        expected_v2(),
        "a refused set writes nothing"
    );

    // A shape without its required board is refused too.
    let mut orphan = good.clone();
    orphan.retain(|(s, p, _)| !(s == &b(BOX) && p == ON_BOARD));
    assert!(reconcile_desired(&store, G, DOC, &orphan).is_err());

    // And the same refusal reaches reconcile_scene's own path: a scene can only say honest things.
    assert!(reconcile_scene(&store, G, DOC, TITLE, &v2)
        .expect("honest scene")
        .is_empty());

    // The lane graph is reserved by its `:projection:` prefix: a user write into it is refused by the authority
    // gate, with no gate edits.
    let lane = crate::rdf_authority::whiteboard_projection_graph_iri(G);
    let update = format!(
        "INSERT DATA {{ GRAPH <{lane}> {{ <{}> <{}> \"forged\" }} }}",
        b(BOX),
        RDFS_LABEL
    );
    assert!(crate::rdf_authority::validate_sparql_update_authority(G, &update).is_err());
    assert!(crate::rdf_authority::user_rdf_target_graph_iri(G, Some(&lane)).is_err());
}

fn rect(id: &str, by: Option<&str>, at: Option<Value>) -> Element {
    let mut sophia = serde_json::Map::new();
    if let Some(by) = by {
        sophia.insert("by".into(), json!(by));
    }
    if let Some(at) = at {
        sophia.insert("at".into(), at);
    }
    json!({
        "id": id, "type": "rectangle", "isDeleted": false,
        "x": 0.0, "y": 0.0, "width": 10.0, "height": 10.0, "boundElements": [],
        "customData": { "sophia": Value::Object(sophia) },
    })
    .as_object()
    .expect("object")
    .clone()
}

#[test]
fn authorship_becomes_prov() {
    let mut elements = BTreeMap::new();
    for (id, by) in [("p1", "person"), ("o1", "owner"), ("d1", "demi-1")] {
        elements.insert(
            id.to_string(),
            rect(id, Some(by), Some(json!(1_760_000_001_000_i64))),
        );
    }
    // Whole seconds: a millisecond tail does not round up.
    elements.insert(
        "t1".into(),
        rect("t1", Some("person"), Some(json!(1_760_000_001_999_i64))),
    );
    // No `by` and no `at`: no prov triples at all.
    elements.insert("n1".into(), rect("n1", None, None));
    // An actor that is already an IRI is kept as given.
    elements.insert(
        "u1".into(),
        rect("u1", Some("urn:sophia:agent:geist-7"), Some(json!(0))),
    );
    let triples = desired_triples(G, DOC, TITLE, &Scene::from_elements(elements)).expect("lane");
    let all = lines(&triples);

    for (id, by) in [("p1", "person"), ("o1", "owner"), ("d1", "demi-1")] {
        assert!(
            all.contains(&iri_line(
                &b(id),
                PROV_WAS_ATTRIBUTED_TO,
                &format!("urn:sophia:agent:{by}")
            )),
            "{id} is attributed to urn:sophia:agent:{by}: {all:#?}"
        );
        assert!(
            all.contains(&time_line(&b(id), "2025-10-09T08:53:21Z")),
            "{id} was generated at 1760000001000 ms = 2025-10-09T08:53:21Z"
        );
    }
    assert!(
        all.contains(&time_line(&b("t1"), "2025-10-09T08:53:21Z")),
        "1760000001999 ms is still second :21"
    );
    assert!(
        !all.iter()
            .any(|l| l.starts_with(&format!("<{}>", b("n1"))) && l.contains("/prov#")),
        "an element nobody signed has no prov triples"
    );
    assert!(all.contains(&iri_line(
        &b("u1"),
        PROV_WAS_ATTRIBUTED_TO,
        "urn:sophia:agent:geist-7"
    )));
    assert!(
        all.contains(&time_line(&b("u1"), "1970-01-01T00:00:00Z")),
        "the epoch is a time, not an absence"
    );

    // The pack accepts the attribution it asks for: the dateTime literal passes the closed shape.
    let store = memory_store();
    reconcile_desired(&store, G, DOC, &triples).expect("the pack accepts prov:generatedAtTime");
    assert_eq!(connection_subject(DOC, "x"), format!("{D}#arrow-x"));
}
