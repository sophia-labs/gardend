//! The fold's control graph (W7 section 1, decisions 1.4 and 1.6).
//!
//! One named graph per Sophia graph, `{graph_subject}:ledger:fold`, holds what the fold must
//! remember across a restart, in the same store as the sink it describes so that one transaction
//! can change both:
//!
//! - **Watermarks.** One triple per (vocab, origin):
//!   `<urn:sophia:ledger:{vocab}:{origin}> <urn:sophia:ledger#folded> "seq"^^xsd:long`. Every frame
//!   of that origin at or below `seq` has been folded into the sink, skipped as carrying no
//!   records, quarantined or abandoned. The fold writes it in the transaction that writes the
//!   frames' delta, so the sink and the watermark can never disagree after a crash.
//! - **Folded frame** (W7d). Beside each watermark, in the same transaction:
//!   `<urn:sophia:ledger:{vocab}:{origin}> <urn:sophia:ledger#foldedFrame> "seq:id"`, the seq and the
//!   64-hex id of the frame the watermark stands on. A seq alone cannot tell the frame the fold
//!   applied from another frame at the same seq (a power cut inside the durability window loses
//!   the newest frames and the cell re-appends at the same seqs; a ledger replaced under the same
//!   origin), so a load compares the id with the frame the ledger now holds there and rebuilds the
//!   sink when they differ ([`untrusted_watermark`]). A watermark written before W7d has no
//!   folded-frame triple and is checked by its seq alone, as it was.
//! - **Generation pointers** (a `projection:*` sink only). `oxigraph` cannot rename a named graph,
//!   so a rebuild loads a fresh generation graph and then repoints a reader-visible pointer:
//!   `<{base}> <urn:sophia:ledger#currentGeneration> <{base}:gen-N>`. No pointer, or generation 0,
//!   means the sink is `{base}` itself. A reader of a ledger-folded projection resolves the pointer
//!   with [`read_generation`] (or `tail::resolve_sink_graph`) before it queries.

#![allow(dead_code)] // the fold host is the consumer; the integrator wires the module in

use std::collections::BTreeMap;

use garden_ledger::{OriginId, VersionVector};
use oxigraph::model::{
    GraphName, GraphNameRef, Literal, NamedNode, NamedOrBlankNode, Quad, Term as OxTerm,
};
use oxigraph::store::{Store, Transaction};

use crate::rdf::graph_subject;

/// The watermark predicate.
pub(crate) const FOLDED_PRED: &str = "urn:sophia:ledger#folded";
/// The folded-frame predicate (W7d): `"seq:id"` of the frame a watermark stands on.
pub(crate) const FOLDED_FRAME_PRED: &str = "urn:sophia:ledger#foldedFrame";
/// The generation pointer predicate.
pub(crate) const GENERATION_PRED: &str = "urn:sophia:ledger#currentGeneration";
const XSD_LONG: &str = "http://www.w3.org/2001/XMLSchema#long";

/// The control graph of `graph_id`.
pub(crate) fn control_graph_iri(graph_id: &str) -> String {
    format!("{}:ledger:fold", graph_subject(graph_id))
}

/// The subject a watermark hangs on: `urn:sophia:ledger:{vocab}:{origin}`.
pub(crate) fn watermark_subject(vocab: &str, origin: &str) -> String {
    format!("urn:sophia:ledger:{vocab}:{origin}")
}

fn subject_prefix(vocab: &str) -> String {
    format!("urn:sophia:ledger:{vocab}:")
}

fn node(iri: &str) -> Result<NamedNode, String> {
    NamedNode::new(iri).map_err(|e| format!("invalid IRI {iri:?}: {e}"))
}

/// Remove every existing quad of `(subject, predicate)` in `graph` and, when `object` is given,
/// insert the one replacement. Reads inside the transaction, so it sees the transaction's own
/// writes.
fn replace_in(
    txn: &mut Transaction<'_>,
    graph: &NamedNode,
    subject: &NamedNode,
    predicate: &NamedNode,
    object: Option<OxTerm>,
) -> Result<(), String> {
    let existing = txn
        .quads_for_pattern(
            Some(subject.as_ref().into()),
            Some(predicate.as_ref()),
            None,
            Some(GraphNameRef::NamedNode(graph.as_ref())),
        )
        .collect::<Result<Vec<Quad>, _>>()
        .map_err(|e| format!("read {subject}: {e}"))?;
    for quad in &existing {
        txn.remove(quad);
    }
    if let Some(object) = object {
        let quad = Quad::new(
            subject.clone(),
            predicate.clone(),
            object,
            GraphName::NamedNode(graph.clone()),
        );
        txn.insert(&quad);
    }
    Ok(())
}

/// Stage `origin`'s watermark of `vocab` at `seq` in `txn`, replacing the old one, with the id
/// (64 hex) of the frame at `seq` beside it (W7d).
pub(crate) fn stage_watermark(
    txn: &mut Transaction<'_>,
    control: &NamedNode,
    vocab: &str,
    origin: &OriginId,
    seq: u64,
    frame: &str,
) -> Result<(), String> {
    let subject = node(&watermark_subject(vocab, &origin.0))?;
    let predicate = node(FOLDED_PRED)?;
    let value = Literal::new_typed_literal(seq.to_string(), NamedNode::new_unchecked(XSD_LONG));
    replace_in(
        txn,
        control,
        &subject,
        &predicate,
        Some(OxTerm::Literal(value)),
    )?;
    let frame_predicate = node(FOLDED_FRAME_PRED)?;
    let mark = Literal::new_simple_literal(format!("{seq}:{frame}"));
    replace_in(
        txn,
        control,
        &subject,
        &frame_predicate,
        Some(OxTerm::Literal(mark)),
    )
}

/// Every watermark of `vocab` in the control graph. An origin never folded is absent (zero).
pub(crate) fn read_watermarks(
    store: &Store,
    control: &NamedNode,
    vocab: &str,
) -> Result<VersionVector, String> {
    let prefix = subject_prefix(vocab);
    let predicate = node(FOLDED_PRED)?;
    let mut map: BTreeMap<OriginId, u64> = BTreeMap::new();
    for quad in store.quads_for_pattern(
        None,
        Some(predicate.as_ref()),
        None,
        Some(GraphNameRef::NamedNode(control.as_ref())),
    ) {
        let quad = quad.map_err(|e| format!("read watermarks: {e}"))?;
        let NamedOrBlankNode::NamedNode(subject) = &quad.subject else {
            continue;
        };
        let Some(origin) = subject.as_str().strip_prefix(&prefix) else {
            continue;
        };
        let OxTerm::Literal(value) = &quad.object else {
            continue;
        };
        if let Ok(seq) = value.value().parse::<u64>() {
            let slot = map.entry(OriginId(origin.to_string())).or_insert(0);
            *slot = (*slot).max(seq);
        }
    }
    Ok(VersionVector(map))
}

/// The folded-frame marks of `vocab` (W7d): per origin, the seq and the frame id (hex) the last
/// watermark write named. An origin whose watermark was written before W7d is absent, and so is a
/// mark that does not parse (it is then checked by its seq alone).
pub(crate) fn read_folded_frames(
    store: &Store,
    control: &NamedNode,
    vocab: &str,
) -> Result<BTreeMap<OriginId, (u64, String)>, String> {
    let prefix = subject_prefix(vocab);
    let predicate = node(FOLDED_FRAME_PRED)?;
    let mut map: BTreeMap<OriginId, (u64, String)> = BTreeMap::new();
    for quad in store.quads_for_pattern(
        None,
        Some(predicate.as_ref()),
        None,
        Some(GraphNameRef::NamedNode(control.as_ref())),
    ) {
        let quad = quad.map_err(|e| format!("read folded frames: {e}"))?;
        let NamedOrBlankNode::NamedNode(subject) = &quad.subject else {
            continue;
        };
        let Some(origin) = subject.as_str().strip_prefix(&prefix) else {
            continue;
        };
        let OxTerm::Literal(value) = &quad.object else {
            continue;
        };
        let Some((seq, id)) = value.value().split_once(':') else {
            continue;
        };
        let Ok(seq) = seq.parse::<u64>() else {
            continue;
        };
        let slot = map
            .entry(OriginId(origin.to_string()))
            .or_insert((0, String::new()));
        if seq >= slot.0 {
            *slot = (seq, id.to_string());
        }
    }
    Ok(map)
}

/// Why a stored watermark cannot be trusted against the ledger a load now reads, or `None` when it
/// can (W7d). `last_seen` is the ledger's newest seq per origin; `at_marks` the id (hex) of the frame
/// the ledger holds at each folded-frame mark's seq (absent when it holds none there). Untrusted: a
/// watermark above the ledger (frames lost inside the durability window, or a ledger replaced), and
/// a folded-frame mark whose id is not the frame now at its seq (frames re-appended at reused seqs).
/// An origin with no mark (a watermark written before W7d) is checked by its seq alone, as before.
pub(crate) fn untrusted_watermark(
    stored: &VersionVector,
    marks: &BTreeMap<OriginId, (u64, String)>,
    last_seen: &BTreeMap<OriginId, u64>,
    at_marks: &BTreeMap<OriginId, String>,
) -> Option<String> {
    let short = |id: &str| id.chars().take(12).collect::<String>();
    for (origin, seq) in &stored.0 {
        let held = last_seen.get(origin).copied().unwrap_or(0);
        if *seq > held {
            return Some(format!(
                "watermark {} at {seq} is ahead of the ledger (which holds {held})",
                origin.0
            ));
        }
    }
    for (origin, (seq, id)) in marks {
        match at_marks.get(origin) {
            Some(found) if found == id => {}
            Some(found) => {
                return Some(format!(
                    "watermark {} at {seq} names frame {}, the ledger holds {} there",
                    origin.0,
                    short(id.as_str()),
                    short(found.as_str())
                ))
            }
            None => {
                return Some(format!(
                    "watermark {} at {seq} names frame {}, the ledger holds no frame there",
                    origin.0,
                    short(id.as_str())
                ))
            }
        }
    }
    None
}

/// The IRI of generation `n` of the sink `base`. Generation 0 is `base` itself.
pub(crate) fn generation_graph_iri(base: &str, n: u64) -> String {
    if n == 0 {
        base.to_string()
    } else {
        format!("{base}:gen-{n}")
    }
}

/// The current generation number of the sink `base` (0 when no pointer was ever written).
pub(crate) fn read_generation(
    store: &Store,
    control: &NamedNode,
    base: &str,
) -> Result<u64, String> {
    let subject = node(base)?;
    let predicate = node(GENERATION_PRED)?;
    let prefix = format!("{base}:gen-");
    let mut current = 0u64;
    for quad in store.quads_for_pattern(
        Some(subject.as_ref().into()),
        Some(predicate.as_ref()),
        None,
        Some(GraphNameRef::NamedNode(control.as_ref())),
    ) {
        let quad = quad.map_err(|e| format!("read generation pointer: {e}"))?;
        if let OxTerm::NamedNode(object) = &quad.object {
            if let Some(n) = object
                .as_str()
                .strip_prefix(&prefix)
                .and_then(|n| n.parse::<u64>().ok())
            {
                current = current.max(n);
            }
        }
    }
    Ok(current)
}

/// Stage the pointer of `base` at generation `n` in `txn`, replacing the old one.
pub(crate) fn stage_generation(
    txn: &mut Transaction<'_>,
    control: &NamedNode,
    base: &str,
    n: u64,
) -> Result<(), String> {
    let subject = node(base)?;
    let predicate = node(GENERATION_PRED)?;
    let target = node(&generation_graph_iri(base, n))?;
    replace_in(
        txn,
        control,
        &subject,
        &predicate,
        Some(OxTerm::NamedNode(target)),
    )
}

/// Remove every `{base}:gen-*` graph the pointer does not name (a crash between loading a
/// generation and swapping the pointer leaves one). Returns how many were removed.
pub(crate) fn sweep_orphan_generations(
    store: &Store,
    base: &str,
    keep: u64,
) -> Result<usize, String> {
    let prefix = format!("{base}:gen-");
    let keep_iri = generation_graph_iri(base, keep);
    let mut orphans = Vec::new();
    for graph in store.named_graphs() {
        let graph = graph.map_err(|e| format!("list graphs: {e}"))?;
        if let NamedOrBlankNode::NamedNode(name) = graph {
            if name.as_str().starts_with(&prefix) && name.as_str() != keep_iri {
                orphans.push(name);
            }
        }
    }
    for name in &orphans {
        store
            .remove_named_graph(name)
            .map_err(|e| format!("remove orphan generation {name}: {e}"))?;
    }
    if !orphans.is_empty() {
        crate::cell_durability::mark_rdf_store_written(store);
    }
    Ok(orphans.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn control() -> NamedNode {
        NamedNode::new(control_graph_iri("g1")).unwrap()
    }

    /// A stand-in frame id: 64 hex, distinct per seq.
    fn frame_hex(seq: u64) -> String {
        format!("{seq:064x}")
    }

    #[test]
    fn a_watermark_is_replaced_not_accumulated() {
        let store = Store::new().unwrap();
        let origin = OriginId("cell-aa".into());
        for seq in [3u64, 9, 12] {
            let mut txn = store.start_transaction().unwrap();
            stage_watermark(&mut txn, &control(), "ao", &origin, seq, &frame_hex(seq)).unwrap();
            txn.commit().unwrap();
        }
        let read = read_watermarks(&store, &control(), "ao").unwrap();
        assert_eq!(read.get(&origin), 12);
        // one seq triple and one folded-frame triple: the old ones were replaced
        assert_eq!(store.len().unwrap(), 2, "old watermark triples stayed");
        let marks = read_folded_frames(&store, &control(), "ao").unwrap();
        assert_eq!(marks.get(&origin), Some(&(12, frame_hex(12))));
        // Another vocab's watermarks are not read.
        assert!(read_watermarks(&store, &control(), "other")
            .unwrap()
            .0
            .is_empty());
    }

    #[test]
    fn the_generation_pointer_swaps_and_orphans_are_swept() {
        let store = Store::new().unwrap();
        let base = "urn:mnemosyne:local:graph:g1:projection:x";
        assert_eq!(read_generation(&store, &control(), base).unwrap(), 0);
        assert_eq!(generation_graph_iri(base, 0), base);
        let mut txn = store.start_transaction().unwrap();
        stage_generation(&mut txn, &control(), base, 2).unwrap();
        txn.commit().unwrap();
        assert_eq!(read_generation(&store, &control(), base).unwrap(), 2);
        let mut txn = store.start_transaction().unwrap();
        stage_generation(&mut txn, &control(), base, 3).unwrap();
        txn.commit().unwrap();
        assert_eq!(read_generation(&store, &control(), base).unwrap(), 3);

        // gen-3 is kept, gen-1 and gen-2 are orphans.
        for n in 1..=3 {
            let g = NamedNode::new(generation_graph_iri(base, n)).unwrap();
            let q = Quad::new(
                NamedNode::new("urn:s").unwrap(),
                NamedNode::new("urn:p").unwrap(),
                NamedNode::new("urn:o").unwrap(),
                GraphName::NamedNode(g),
            );
            store.insert(&q).unwrap();
        }
        assert_eq!(sweep_orphan_generations(&store, base, 3).unwrap(), 2);
        assert!(store
            .contains_named_graph(&NamedNode::new(generation_graph_iri(base, 3)).unwrap())
            .unwrap());
        assert!(!store
            .contains_named_graph(&NamedNode::new(generation_graph_iri(base, 1)).unwrap())
            .unwrap());
    }
}

#[cfg(test)]
mod folded_frame_tests {
    use super::*;

    fn o(s: &str) -> OriginId {
        OriginId(s.to_string())
    }

    fn vv(entries: &[(&str, u64)]) -> VersionVector {
        VersionVector(entries.iter().map(|(k, v)| (o(k), *v)).collect())
    }

    fn seen(entries: &[(&str, u64)]) -> BTreeMap<OriginId, u64> {
        entries.iter().map(|(k, v)| (o(k), *v)).collect()
    }

    fn ids(entries: &[(&str, &str)]) -> BTreeMap<OriginId, String> {
        entries.iter().map(|(k, v)| (o(k), v.to_string())).collect()
    }

    fn marks(entries: &[(&str, u64, &str)]) -> BTreeMap<OriginId, (u64, String)> {
        entries
            .iter()
            .map(|(k, seq, id)| (o(k), (*seq, id.to_string())))
            .collect()
    }

    /// W7d item 2, the decision a load makes. A mark whose id is the frame the ledger holds at its
    /// seq is trusted; another id there, or no frame there, is not; a watermark above the ledger is
    /// not; a watermark with no mark (written before W7d) is judged by its seq alone. Mutation:
    /// compare seqs only (drop the id loop) and the mismatch case is trusted.
    #[test]
    fn a_watermark_is_trusted_only_when_its_frame_is_still_at_its_seq() {
        let stored = vv(&[("cell-a", 5), ("cell-b", 2)]);
        let last = seen(&[("cell-a", 7), ("cell-b", 2)]);
        // matching ids
        let m = marks(&[("cell-a", 5, "aa"), ("cell-b", 2, "bb")]);
        let found = ids(&[("cell-a", "aa"), ("cell-b", "bb")]);
        assert_eq!(untrusted_watermark(&stored, &m, &last, &found), None);
        // another frame at cell-a's seq 5
        let other = ids(&[("cell-a", "a2"), ("cell-b", "bb")]);
        let why = untrusted_watermark(&stored, &m, &last, &other).expect("a mismatch is untrusted");
        assert!(
            why.contains("cell-a") && why.contains("names frame aa"),
            "{why}"
        );
        // no frame at the mark's seq
        let missing = ids(&[("cell-b", "bb")]);
        assert!(untrusted_watermark(&stored, &m, &last, &missing).is_some());
        // a watermark above the ledger, mark or not
        let short_ledger = seen(&[("cell-a", 4), ("cell-b", 2)]);
        assert!(
            untrusted_watermark(&stored, &BTreeMap::new(), &short_ledger, &found)
                .expect("ahead")
                .contains("ahead")
        );
        // no marks at all (a pre-W7d watermark): the seq check alone, as before
        assert_eq!(
            untrusted_watermark(&stored, &BTreeMap::new(), &last, &BTreeMap::new()),
            None
        );
    }

    /// A watermark written before W7d (the seq triple only) reads back with no folded-frame mark,
    /// so a load judges it by its seq alone; a mark that does not parse is ignored the same way.
    #[test]
    fn an_id_less_watermark_reads_back_with_no_mark() {
        let store = Store::new().unwrap();
        let control = NamedNode::new(control_graph_iri("g1")).unwrap();
        let subject = NamedNode::new(watermark_subject("ao", "cell-aa")).unwrap();
        let seq_quad = Quad::new(
            subject.clone(),
            NamedNode::new(FOLDED_PRED).unwrap(),
            Literal::new_typed_literal("7", NamedNode::new_unchecked(XSD_LONG)),
            GraphName::NamedNode(control.clone()),
        );
        store.insert(&seq_quad).unwrap();
        assert_eq!(
            read_watermarks(&store, &control, "ao")
                .unwrap()
                .get(&o("cell-aa")),
            7
        );
        assert!(read_folded_frames(&store, &control, "ao")
            .unwrap()
            .is_empty());
        let garbled = Quad::new(
            subject,
            NamedNode::new(FOLDED_FRAME_PRED).unwrap(),
            Literal::new_simple_literal("not a mark"),
            GraphName::NamedNode(control.clone()),
        );
        store.insert(&garbled).unwrap();
        assert!(read_folded_frames(&store, &control, "ao")
            .unwrap()
            .is_empty());
    }
}
