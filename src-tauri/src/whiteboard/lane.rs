//! The board lane: a whiteboard's shapes, cards, connections and clusters as RDF, in the graph's own
//! `…:projection:whiteboard` named graph (SPRINT D5, D6).
//!
//! The Y.Doc is the only authority; this lane is derived from it and reconciled per board in the `pdf_source.rs`
//! pattern: class spans scoped by `wb:documentId`, the closed `sophia-whiteboard` vocabulary pack as the SHACL gate,
//! `remove_document` on delete, and the three persistence tails plus the seed and restore (INTERFACES.md names each
//! hook). The document graph is NOT used: its TreeToRdf face owns every `{doc}#…` subject there and would reclaim them.
//!
//! Owned by G2. The constants and signatures below are FINAL.

use super::links::{self, LinkTarget};
use super::outline;
use super::scene::{el_type, is_bound_text, is_deleted, s, shape_kind, Element, Scene};
use crate::document_service::DocumentRecord;
use crate::emporium::contract::get_vocabulary;
use crate::emporium::reconcile::{apply_diff, survey_class, ClassScope, Placement, SpanKey};
use crate::emporium::terms::{Term, Triple, TripleDiff};
use chrono::Datelike;
use oxigraph::model::{Literal, NamedNode};
use oxigraph::store::Store;
use serde_json::Value;
use std::collections::BTreeMap;
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

/// The five classes whose spans a board owns, in survey order.
const CLASSES: [&str; 5] = [BOARD, SHAPE, CARD, CONNECTION, CLUSTER];

/// Subjects of the lane for board `document_id`, as plain strings (INTERFACES.md §e has the table):
/// board = `document_subject(doc)`; shape, card and cluster = `block_ref_uri(doc, element_id)`
/// (`{doc}#block-{id}`); connection = `{doc subject}#arrow-{id}`.
pub(crate) fn connection_subject(document_id: &str, arrow_id: &str) -> String {
    format!(
        "{}#arrow-{arrow_id}",
        crate::rdf::document_subject(document_id)
    )
}

/// The lane graph a board's triples live in.
fn lane_graph(graph_id: &str) -> String {
    crate::rdf_authority::whiteboard_projection_graph_iri(graph_id)
}

// ------------------------------------------------------------------ the desired set

/// A subject, predicate or object IRI the lane may write: well formed for SPARQL (the `validate_sparql_iri` the
/// pdf lane uses) and accepted by oxigraph. A peer's malformed element id leaves nothing, rather than breaking the
/// board's whole reconcile.
fn safe_iri(iri: &str) -> bool {
    crate::emporium::query_engine::validate_sparql_iri("whiteboard lane IRI", iri).is_ok()
        && NamedNode::new(iri).is_ok()
}

#[derive(Default)]
struct Emit {
    out: Vec<Triple>,
}

impl Emit {
    fn class(&mut self, subject: &str, class: &str) {
        self.iri(subject, crate::runtime_config::RDF_TYPE, class);
    }

    fn iri(&mut self, subject: &str, predicate: &str, object: &str) {
        if let Ok(node) = NamedNode::new(object) {
            if safe_iri(object) {
                self.out
                    .push((subject.to_string(), predicate.to_string(), Term::Uri(node)));
            }
        }
    }

    fn text(&mut self, subject: &str, predicate: &str, value: &str) {
        self.out.push((
            subject.to_string(),
            predicate.to_string(),
            Term::Lit(Literal::new_simple_literal(value)),
        ));
    }

    fn time(&mut self, subject: &str, predicate: &str, value: &str) {
        self.out.push((
            subject.to_string(),
            predicate.to_string(),
            Term::Lit(Literal::new_typed_literal(value, oxigraph::model::vocab::xsd::DATE_TIME)),
        ));
    }
}

/// `prov:wasAttributedTo` / `prov:generatedAtTime` from `customData.sophia {by, at}` (FORMAT.md): who last wrote the
/// element and when, `at` in milliseconds, written as whole UTC seconds. Either is omitted when absent.
fn provenance(emit: &mut Emit, subject: &str, element: &Element) {
    let sophia = element.get("customData").and_then(|c| c.get("sophia"));
    if let Some(by) = sophia.and_then(|c| c.get("by")).and_then(Value::as_str) {
        if let Some(iri) = crate::rdf_authority::observer_iri(by) {
            emit.iri(subject, PROV_WAS_ATTRIBUTED_TO, &iri);
        }
    }
    let at = sophia
        .and_then(|c| c.get("at"))
        .and_then(|v| v.as_i64().or_else(|| v.as_f64().map(|f| f as i64)));
    if let Some(ms) = at {
        if let Some(moment) = chrono::DateTime::from_timestamp(ms.div_euclid(1000), 0)
            .filter(|moment| (1..=9999).contains(&moment.year()))
        {
            emit.time(
                subject,
                PROV_GENERATED_AT_TIME,
                &moment.format("%Y-%m-%dT%H:%M:%SZ").to_string(),
            );
        }
    }
}

/// What a shape's or card's link means to the lane: a document of THIS graph is its document subject, an object link
/// is the IRI as given, and everything else (a web address, another graph's document, an artifact) says nothing.
fn refers_to(graph_id: &str, element: &Element) -> Option<String> {
    match links::parse_link(s(element, "link")?)? {
        LinkTarget::Document {
            graph_id: linked,
            document_id,
        } if linked == graph_id => Some(crate::rdf::document_subject(&document_id)),
        LinkTarget::Object { iri, .. } => Some(iri),
        _ => None,
    }
}

/// A card is an `embeddable` whose link is a document or object link.
fn is_card(element: &Element) -> bool {
    el_type(element) == "embeddable"
        && matches!(
            s(element, "link").and_then(links::parse_link),
            Some(LinkTarget::Document { .. }) | Some(LinkTarget::Object { .. })
        )
}

/// A card's name as the board snapshot keeps it (`customData.mnemosyne`): the object's label, else the document title.
fn card_label(element: &Element) -> String {
    let snapshot = element.get("customData").and_then(|c| c.get("mnemosyne"));
    ["label", "title"]
        .iter()
        .filter_map(|key| snapshot.and_then(|m| m.get(*key)).and_then(Value::as_str))
        .map(str::trim)
        .find(|value| !value.is_empty())
        .unwrap_or("")
        .to_string()
}

fn wire_id_of(element: &Element) -> Option<&str> {
    element
        .get("customData")
        .and_then(|c| c.get("mnemosyneWire"))
        .and_then(|w| w.get("wireId"))
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Node {
    Shape,
    Card,
    Cluster,
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
    let board = crate::rdf::document_subject(document_id);
    if !safe_iri(&board) {
        return Err(format!(
            "whiteboard lane: document id '{document_id}' cannot be part of an IRI"
        ));
    }
    let mut emit = Emit::default();
    emit.class(&board, BOARD);
    emit.text(&board, DOCUMENT_ID, document_id);
    if !title.trim().is_empty() {
        emit.text(&board, RDFS_LABEL, title);
    }

    // The words come from the outline's reading of the scene, so the lane says what `whiteboard_read` says.
    let shapes: BTreeMap<String, outline::Shape> = outline::shapes(scene)
        .into_iter()
        .map(|shape| (shape.id.clone(), shape))
        .collect();

    // Pass one: which live elements become subjects (a shape, a card or a cluster).
    let elements = shapes_elements(scene, &shapes);
    let mut nodes: BTreeMap<String, (String, Node)> = BTreeMap::new();
    for (id, element) in &elements {
        let subject = format!("{board}#block-{id}");
        if !safe_iri(&subject) {
            continue;
        }
        let node = if shape_kind(element) == "frame" {
            Node::Cluster
        } else if is_card(element) {
            Node::Card
        } else {
            Node::Shape
        };
        nodes.insert(id.clone(), (subject, node));
    }

    // Pass two: what each subject says.
    for (id, element) in &elements {
        let Some((subject, node)) = nodes.get(id) else {
            continue;
        };
        let reading = &shapes[id];
        let class = match node {
            Node::Shape => SHAPE,
            Node::Card => CARD,
            Node::Cluster => CLUSTER,
        };
        emit.class(subject, class);
        emit.text(subject, DOCUMENT_ID, document_id);
        emit.iri(subject, ON_BOARD, &board);
        let label = match node {
            Node::Card => card_label(element),
            _ => reading.text.trim().to_string(),
        };
        match node {
            Node::Card => emit.text(subject, SHAPE_KIND, "card"),
            Node::Shape => emit.text(subject, SHAPE_KIND, shape_kind(element)),
            Node::Cluster => {}
        }
        if !label.is_empty() {
            emit.text(subject, RDFS_LABEL, &label);
        }
        if *node != Node::Cluster {
            if let Some(target) = refers_to(graph_id, element) {
                emit.iri(subject, REFERS_TO, &target);
            }
            if let Some((frame_subject, Node::Cluster)) = s(element, "frameId").and_then(|f| nodes.get(f)) {
                emit.iri(subject, IN_CLUSTER, frame_subject);
            }
        }
        provenance(&mut emit, subject, element);
    }

    // Connections: a live arrow with both ends on subjects. A binding to a label resolves to the label's container.
    let arrow_labels: BTreeMap<String, Option<String>> = outline::arrows(scene)
        .into_iter()
        .map(|arrow| (arrow.id, arrow.label))
        .collect();
    for (id, element) in scene.live() {
        if el_type(element) != "arrow" {
            continue;
        }
        let end = |key: &str| -> Option<String> {
            let target_id = element.get(key)?.get("elementId")?.as_str()?;
            let target = scene.elements.get(target_id)?;
            if is_deleted(target) {
                return None;
            }
            let resolved = if is_bound_text(target) {
                s(target, "containerId")?
            } else {
                target_id
            };
            nodes.get(resolved).map(|(subject, _)| subject.clone())
        };
        let (Some(from), Some(to)) = (end("startBinding"), end("endBinding")) else {
            continue;
        };
        let subject = connection_subject(document_id, id);
        if !safe_iri(&subject) {
            continue;
        }
        emit.class(&subject, CONNECTION);
        emit.text(&subject, DOCUMENT_ID, document_id);
        emit.iri(&subject, ON_BOARD, &board);
        emit.iri(&subject, FROM, &from);
        emit.iri(&subject, TO, &to);
        if let Some(Some(label)) = arrow_labels.get(id) {
            if !label.trim().is_empty() {
                emit.text(&subject, RDFS_LABEL, label.trim());
            }
        }
        if let Some(wire_id) = wire_id_of(element) {
            emit.iri(
                &subject,
                PROMOTED_TO,
                &format!("{}:wire:{wire_id}", crate::rdf::graph_subject(graph_id)),
            );
        }
        provenance(&mut emit, &subject, element);
    }

    let mut triples = emit.out;
    triples.sort_by_cached_key(key_of);
    triples.dedup_by_key(|triple| key_of(triple));
    Ok(triples)
}

/// The live elements the outline reads as shapes (non-arrow, non-bound-text), with their raw element.
fn shapes_elements<'a>(
    scene: &'a Scene,
    shapes: &BTreeMap<String, outline::Shape>,
) -> Vec<(String, &'a Element)> {
    shapes
        .keys()
        .filter_map(|id| scene.elements.get(id).map(|element| (id.clone(), element)))
        .collect()
}

// ------------------------------------------------------------------ reconcile

/// A triple's exact identity: subject, predicate and the N-Triples object. The lane holds only strings, IRIs and
/// whole-second dateTimes, so exact text is exact value (no numeric equivalence to hide a drift).
fn key_of(triple: &Triple) -> (String, String, String) {
    (triple.0.clone(), triple.1.clone(), triple.2.as_nt())
}

/// The five class scopes of one board's owned span, keyed by `wb:documentId` (the pdf-source pattern).
fn scopes(graph_id: &str, document_id: &str) -> Vec<ClassScope> {
    CLASSES
        .iter()
        .map(|class| ClassScope {
            placement: Placement::Named(lane_graph(graph_id)),
            key: SpanKey::Fixed {
                rdf_type: (*class).to_string(),
            },
            graph_id_conjunct: Some((DOCUMENT_ID.to_string(), document_id.to_string())),
            subjects: None,
        })
        .collect()
}

/// What the store holds for one board's span right now (every triple of every subject of the five classes).
pub(crate) fn current_triples(
    store: &Store,
    graph_id: &str,
    document_id: &str,
) -> Result<Vec<Triple>, String> {
    let mut current = Vec::new();
    for scope in scopes(graph_id, document_id) {
        current.extend(survey_class(store, &scope)?);
    }
    current.sort_by_cached_key(key_of);
    current.dedup_by_key(|triple| key_of(triple));
    Ok(current)
}

/// Validate `desired` against the pack, diff it against the board's owned span exactly and apply the diff
/// direct-on-store. A set the closed pack refuses (a rogue predicate, a missing required one) changes nothing.
pub(crate) fn reconcile_desired(
    store: &Store,
    graph_id: &str,
    document_id: &str,
    desired: &[Triple],
) -> Result<TripleDiff, String> {
    let contract = get_vocabulary(PACK).ok_or("whiteboard vocabulary is not registered")?;
    crate::emporium::shacl_validator::validate_desired(desired, contract)?;
    let before: BTreeMap<_, _> = current_triples(store, graph_id, document_id)?
        .into_iter()
        .map(|triple| (key_of(&triple), triple))
        .collect();
    let after: BTreeMap<_, _> = desired
        .iter()
        .cloned()
        .map(|triple| (key_of(&triple), triple))
        .collect();
    let diff = TripleDiff {
        removes: before
            .iter()
            .filter(|(key, _)| !after.contains_key(*key))
            .map(|(_, triple)| triple.clone())
            .collect(),
        adds: after
            .iter()
            .filter(|(key, _)| !before.contains_key(*key))
            .map(|(_, triple)| triple.clone())
            .collect(),
    };
    if !diff.is_empty() {
        apply_diff(store, &Placement::Named(lane_graph(graph_id)), &diff)?;
        crate::cell_durability::mark_rdf_store_written(store);
    }
    Ok(diff)
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
    let desired = desired_triples(graph_id, document_id, title, scene)?;
    reconcile_desired(store, graph_id, document_id, &desired)
}

/// Reconcile from a persisted record: a no-op (empty diff) for any record that is not a whiteboard, so the shared
/// persistence tails call it unconditionally. The scene comes from the inline `ydoc_update_base64`, else the durable
/// sidecar (`ydoc_paths::document_ydoc_state_path`); a record with neither is a valid empty board (the lane empties).
pub(crate) fn reconcile_record(
    store: &Store,
    graph_dir: &Path,
    document: &DocumentRecord,
) -> Result<TripleDiff, String> {
    if !super::is_whiteboard(document.document_kind.as_deref()) {
        return Ok(TripleDiff::default());
    }
    let scene = if !document.ydoc_update_base64.is_empty() {
        super::ops::scene_from_update_base64(&document.ydoc_update_base64)?
    } else {
        let path = crate::ydoc_paths::document_ydoc_state_path(graph_dir, &document.document_id);
        match std::fs::read(&path) {
            Ok(bytes) => super::ops::scene_from_update_bytes(&bytes)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Scene::default(),
            Err(error) => {
                return Err(format!(
                    "read whiteboard Y.Doc sidecar {}: {error}",
                    path.display()
                ))
            }
        }
    };
    reconcile_scene(
        store,
        &document.graph_id,
        &document.document_id,
        &document.title,
        &scene,
    )
}

/// Empty a board's span (the board was deleted). Idempotent.
pub(crate) fn remove_document(
    store: &Store,
    graph_id: &str,
    document_id: &str,
) -> Result<(), String> {
    reconcile_desired(store, graph_id, document_id, &[])?;
    Ok(())
}
