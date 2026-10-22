//! The board lane: a whiteboard's shapes, cards, connections and clusters as RDF, in the graph's own
//! `…:projection:whiteboard` named graph (SPRINT D5, D6).
//!
//! The Y.Doc is the only authority; this lane is derived from it and reconciled per board in the `pdf_source.rs`
//! pattern: class spans scoped by `wb:documentId`, the closed `sophia-whiteboard` vocabulary pack as the SHACL gate,
//! `remove_document` on delete, and the three persistence tails plus the seed and restore (INTERFACES.md names each
//! hook). The document graph is NOT used: its TreeToRdf face owns every `{doc}#…` subject there and would reclaim them.
//!
//! Owned by G2. The constants and signatures below are FINAL; G2 replaces the stub bodies.

use super::scene::Scene;
use crate::document_service::DocumentRecord;
use crate::emporium::terms::{Triple, TripleDiff};
use oxigraph::store::Store;
use std::path::Path;

/// The vocabulary pack's name (`emporium/vocabs/sophia-whiteboard.golden.json`).
pub(crate) const PACK: &str = "sophia-whiteboard";
/// `wb:` = the whiteboard vocabulary namespace.
pub(crate) const NS: &str = "http://mnemosyne.dev/whiteboard#";
/// Bumped into the seed version (`rdf_seed_service::seed_projection_version`) so existing graphs reconcile boards once.
pub(crate) const PROJECTION_VERSION: &str = "whiteboard-lane-v1";

// Classes.
pub(crate) const BOARD: &str = "http://mnemosyne.dev/whiteboard#Board";
pub(crate) const SHAPE: &str = "http://mnemosyne.dev/whiteboard#Shape";
pub(crate) const CARD: &str = "http://mnemosyne.dev/whiteboard#Card";
pub(crate) const CONNECTION: &str = "http://mnemosyne.dev/whiteboard#Connection";
pub(crate) const CLUSTER: &str = "http://mnemosyne.dev/whiteboard#Cluster";

// Predicates.
/// Every lane subject carries `wb:documentId "<board document id>"`: the span key (the pdf-source `graph_id_conjunct`).
pub(crate) const DOCUMENT_ID: &str = "http://mnemosyne.dev/whiteboard#documentId";
pub(crate) const ON_BOARD: &str = "http://mnemosyne.dev/whiteboard#onBoard";
/// The drawn kind: `rectangle`, `ellipse`, `diamond`, `text`, `embeddable`, ... (`scene::shape_kind`).
pub(crate) const SHAPE_KIND: &str = "http://mnemosyne.dev/whiteboard#shapeKind";
/// The document subject, or the object IRI, a shape or card stands for.
pub(crate) const REFERS_TO: &str = "http://mnemosyne.dev/whiteboard#refersTo";
pub(crate) const FROM: &str = "http://mnemosyne.dev/whiteboard#from";
pub(crate) const TO: &str = "http://mnemosyne.dev/whiteboard#to";
pub(crate) const IN_CLUSTER: &str = "http://mnemosyne.dev/whiteboard#inCluster";
/// A connection's wire, once promoted (`customData.mnemosyneWire.wireId`).
pub(crate) const PROMOTED_TO: &str = "http://mnemosyne.dev/whiteboard#promotedTo";

// Borrowed terms.
pub(crate) const RDFS_LABEL: &str = "http://www.w3.org/2000/01/rdf-schema#label";
pub(crate) const PROV_WAS_ATTRIBUTED_TO: &str = "http://www.w3.org/ns/prov#wasAttributedTo";
pub(crate) const PROV_GENERATED_AT_TIME: &str = "http://www.w3.org/ns/prov#generatedAtTime";

/// Subjects of the lane for board `document_id`, as plain strings (INTERFACES.md §e has the table):
/// board = `document_subject(doc)`; shape, card and cluster = `block_ref_uri(doc, element_id)`
/// (`{doc}#block-{id}`); connection = `{doc subject}#arrow-{id}`.
pub(crate) fn connection_subject(document_id: &str, arrow_id: &str) -> String {
    format!(
        "{}#arrow-{arrow_id}",
        crate::rdf::document_subject(document_id)
    )
}

/// The triples the lane should hold for one board: a pure function of the scene (no store, no clock). `title` is the
/// board's title (the `rdfs:label` of the `wb:Board`). Sorted and deduplicated; deleted elements and arrows with a
/// dangling end contribute nothing. This is what `lane_matches_the_scene` compares to INTERFACES.md §e.
pub(crate) fn desired_triples(
    graph_id: &str,
    document_id: &str,
    title: &str,
    scene: &Scene,
) -> Result<Vec<Triple>, String> {
    let _ = (graph_id, document_id, title, scene);
    unimplemented!("G2")
}

/// Reconcile the lane for one board from its scene: validate `desired_triples` against the pack, survey the board's
/// owned span (all five classes, keyed by `wb:documentId`), diff exactly, apply the diff direct-on-store, mark the
/// store written. A converged board changes nothing.
pub(crate) fn reconcile_scene(
    store: &Store,
    graph_id: &str,
    document_id: &str,
    title: &str,
    scene: &Scene,
) -> Result<TripleDiff, String> {
    // STUB (no-op: today's behaviour; the call sites are live). G2 replaces the body.
    let _ = (store, graph_id, document_id, title, scene);
    Ok(TripleDiff::default())
}

/// Reconcile from a persisted record: a no-op (empty diff) for any record that is not a whiteboard, so the shared
/// persistence tails call it unconditionally. The scene comes from the inline `ydoc_update_base64`, else the durable
/// sidecar (`ydoc_paths::document_ydoc_state_path`); a record with neither is a valid empty board (the lane empties).
pub(crate) fn reconcile_record(
    store: &Store,
    graph_dir: &Path,
    document: &DocumentRecord,
) -> Result<TripleDiff, String> {
    // STUB (no-op: today's behaviour; the call sites are live). G2 replaces the body.
    let _ = (store, graph_dir, document);
    Ok(TripleDiff::default())
}

/// Empty a board's span (the board was deleted). Idempotent.
pub(crate) fn remove_document(
    store: &Store,
    graph_id: &str,
    document_id: &str,
) -> Result<(), String> {
    // STUB (no-op: today's behaviour; the call sites are live). G2 replaces the body.
    let _ = (store, graph_id, document_id);
    Ok(())
}
