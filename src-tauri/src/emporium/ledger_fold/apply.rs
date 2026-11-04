//! Writing a [`Delta`] into an oxigraph store transaction (the only store API the fold touches).

use oxigraph::model::{GraphName, NamedNode, Quad, Term as OxTerm};
use oxigraph::store::{StorageError, Store, Transaction};

use super::Delta;
use crate::emporium::terms::{Term, Triple, PLACEHOLDER_NS};

/// The oxigraph term of a fold term. A placeholder (a deferred IRI) becomes its reserved IRI.
pub(crate) fn ox_term(t: &Term) -> Option<OxTerm> {
    match t {
        Term::Uri(n) => Some(OxTerm::NamedNode(n.clone())),
        Term::Lit(l) => Some(OxTerm::Literal(l.clone())),
        Term::Placeholder(name) => NamedNode::new(format!("{PLACEHOLDER_NS}{name}"))
            .ok()
            .map(OxTerm::NamedNode),
    }
}

/// The quad of `triple` in `graph`. `None` when the subject or predicate is not a valid IRI (the
/// fold only produces valid ones; a hand-built triple that is not is skipped, never a panic).
pub(crate) fn quad_of(triple: &Triple, graph: &GraphName) -> Option<Quad> {
    let (s, p, o) = triple;
    Some(Quad::new(
        NamedNode::new(s.as_str()).ok()?,
        NamedNode::new(p.as_str()).ok()?,
        ox_term(o)?,
        graph.clone(),
    ))
}

/// Remove the Delta's `remove` triples from `sink`, then insert its `insert` triples, in `txn`.
///
/// Remove-then-insert, so a triple that is both (it never is, a Delta is netted) ends up present.
/// The caller commits.
pub(crate) fn apply_delta(txn: &mut Transaction<'_>, sink: &NamedNode, delta: &Delta) {
    let graph = GraphName::NamedNode(sink.clone());
    for t in &delta.remove {
        if let Some(q) = quad_of(t, &graph) {
            txn.remove(&q);
        }
    }
    for t in &delta.insert {
        if let Some(q) = quad_of(t, &graph) {
            txn.insert(&q);
        }
    }
}

/// [`apply_delta`] in a transaction of its own, committed.
pub(crate) fn apply_delta_commit(
    store: &Store,
    sink: &NamedNode,
    delta: &Delta,
) -> Result<(), StorageError> {
    let mut txn = store.start_transaction()?;
    apply_delta(&mut txn, sink, delta);
    txn.commit()
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxigraph::model::{GraphNameRef, Literal};

    fn sink() -> NamedNode {
        NamedNode::new("urn:test:graph:user:rdf").unwrap()
    }

    fn lit(s: &str) -> Term {
        Term::Lit(Literal::new_simple_literal(s))
    }

    fn count(store: &Store, g: &NamedNode) -> usize {
        store
            .quads_for_pattern(None, None, None, Some(GraphNameRef::NamedNode(g.as_ref())))
            .filter_map(Result::ok)
            .count()
    }

    #[test]
    fn insert_then_remove_round_trips_through_a_transaction() {
        let store = Store::new().unwrap();
        let g = sink();
        let t1: Triple = ("urn:s:1".into(), "urn:p:a".into(), lit("one"));
        let t2: Triple = (
            "urn:s:1".into(),
            "urn:p:b".into(),
            Term::Uri(NamedNode::new("urn:o:1").unwrap()),
        );
        let add = Delta {
            insert: vec![t1.clone(), t2.clone()],
            ..Delta::default()
        };
        apply_delta_commit(&store, &g, &add).unwrap();
        assert_eq!(count(&store, &g), 2);
        // the graph is the sink, not the default graph
        assert_eq!(store.len().unwrap(), 2);
        let swap = Delta {
            remove: vec![t1],
            insert: vec![("urn:s:1".into(), "urn:p:a".into(), lit("two"))],
            ..Delta::default()
        };
        apply_delta_commit(&store, &g, &swap).unwrap();
        assert_eq!(count(&store, &g), 2);
        let gone = Delta {
            remove: vec![t2],
            ..Delta::default()
        };
        apply_delta_commit(&store, &g, &gone).unwrap();
        assert_eq!(count(&store, &g), 1);
    }

    #[test]
    fn removing_an_absent_triple_and_an_invalid_one_do_nothing() {
        let store = Store::new().unwrap();
        let g = sink();
        let bad: Triple = ("not an iri".into(), "urn:p".into(), lit("x"));
        let absent: Triple = ("urn:s:9".into(), "urn:p".into(), lit("x"));
        let d = Delta {
            remove: vec![absent],
            insert: vec![bad],
            ..Delta::default()
        };
        apply_delta_commit(&store, &g, &d).unwrap();
        assert_eq!(store.len().unwrap(), 0);
    }

    #[test]
    fn a_placeholder_object_becomes_its_reserved_iri() {
        let store = Store::new().unwrap();
        let g = sink();
        let d = Delta {
            insert: vec![(
                "urn:s:1".into(),
                "urn:p".into(),
                Term::Placeholder("NAME".into()),
            )],
            ..Delta::default()
        };
        apply_delta_commit(&store, &g, &d).unwrap();
        let o = store
            .quads_for_pattern(None, None, None, None)
            .next()
            .unwrap()
            .unwrap()
            .object;
        assert_eq!(o.to_string(), format!("<{PLACEHOLDER_NS}NAME>"));
    }
}
