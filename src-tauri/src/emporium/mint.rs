//! `render_class_triples` — the contract-enforced mint primitive.
//!
//! Port of `app/services/emporium/ingest/planner.py::render_class_triples`
//! (lines 112-133), the line that kills vocab drift. Given a class name and a
//! flat predicate-CURIE → value(s) map, it emits the canonical `wf:` triples:
//! `rdf:type` per the class `rdf_types`, then each declared predicate coerced
//! via [`term_for`]. Two mechanical guarantees, both ported verbatim:
//!
//! 1. a required predicate that is absent (or `None`) → `PlanError`;
//! 2. ANY value key not declared in the class spec → `PlanError` ("predicates
//!    not in vocab") — minting cannot drift past the frozen vocab.
//!
//! This is DSL-AGNOSTIC: it does NOT read a workflow definition's shape. The
//! per-class minting (Workflow/Phase/AgentNode/Archetype/Run/AgentRun/Variant
//! *from a def*) now lives in [`crate::emporium::planner`] (P2), which calls this
//! primitive for every class. Still held downstream: the CRDT applier write path
//! (P3) and the assertion suite (P4).

use std::collections::BTreeMap;

use crate::emporium::contract::VocabularyContract;
use crate::emporium::terms::{term_for, PlanError, Triple, Value};

/// A predicate's value(s): a single value or a multi-valued list (the contract
/// `multi` flag is descriptive; the caller passes a list for multi predicates,
/// matching Python's `isinstance(v, list)` branch).
#[derive(Debug, Clone)]
pub(crate) enum FieldValue {
    One(Value),
    Many(Vec<Value>),
}

impl FieldValue {
    fn values(&self) -> Vec<&Value> {
        match self {
            FieldValue::One(v) => vec![v],
            FieldValue::Many(vs) => vs.iter().collect(),
        }
    }
}

/// Render the canonical triples for one instance of a vocabulary class.
///
/// `values` maps predicate CURIEs to their value(s). Absent (or empty `Many`)
/// is treated as "not provided". The class's `rdf:type`(s) are emitted first.
///
/// Errors (mirroring the Python `PlanError`):
/// - a required predicate is missing → `"<class> <subject> missing required <curie>"`
/// - a value key is not in the class predicate set → `"predicates not in vocab for <class>: …"`
pub(crate) fn render_class_triples(
    contract: &VocabularyContract,
    subject: &str,
    class: &str,
    values: &BTreeMap<String, FieldValue>,
) -> Result<Vec<Triple>, PlanError> {
    let spec = contract
        .classes
        .get(class)
        .ok_or_else(|| PlanError(format!("unknown class {class}")))?;

    let mut out: Vec<Triple> = Vec::new();

    let rdf_type = contract
        .namespaces
        .get("rdf")
        .map(|ns| format!("{ns}type"))
        .ok_or_else(|| PlanError("contract has no rdf namespace".to_string()))?;

    // rdf:type per the class rdf_types (Protocol has none → no type triple).
    for t in &spec.rdf_types {
        let object_uri = contract.expand(t).map_err(PlanError)?;
        out.push((
            subject.to_string(),
            rdf_type.clone(),
            term_for(
                &Value::Uri(object_uri),
                crate::emporium::contract::Datatype::uri,
            ),
        ));
    }

    // Each declared predicate, in contract order (BTreeMap → deterministic).
    for (curie, pspec) in &spec.predicates {
        let provided = values.get(curie).filter(|fv| !fv.values().is_empty());
        let Some(fv) = provided else {
            if pspec.required {
                return Err(PlanError(format!(
                    "{class} <{subject}> missing required {curie}"
                )));
            }
            continue;
        };
        let predicate_uri = contract.expand(curie).map_err(PlanError)?;
        for v in fv.values() {
            // Ludus source excerpts and submission snapshots promise exact
            // lexical text. Preserve CR/CRLF here for that new pack only;
            // legacy term_for parity (including CR stripping) is unchanged.
            let term = match (contract.name.as_str(), pspec.datatype, v) {
                ("ludus-core", crate::emporium::contract::Datatype::string, Value::Str(text)) => {
                    crate::emporium::terms::Term::Lit(
                        oxigraph::model::Literal::new_simple_literal(text.clone()),
                    )
                }
                _ => term_for(v, pspec.datatype),
            };
            out.push((
                subject.to_string(),
                predicate_uri.clone(),
                term,
            ));
        }
    }

    // The drift-killer: any value key not declared on the class is rejected.
    let mut extraneous: Vec<&String> = values
        .keys()
        .filter(|k| !spec.predicates.contains_key(*k))
        .collect();
    if !extraneous.is_empty() {
        extraneous.sort();
        return Err(PlanError(format!(
            "predicates not in vocab for {class}: {extraneous:?}"
        )));
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::emporium::contract::workflow_vocabulary;
    use crate::emporium::terms::{canon, Term};

    fn fv(v: Value) -> FieldValue {
        FieldValue::One(v)
    }

    fn object_of<'a>(triples: &'a [Triple], predicate_suffix: &str) -> Vec<&'a Term> {
        triples
            .iter()
            .filter(|(_, p, _)| p.ends_with(predicate_suffix))
            .map(|(_, _, o)| o)
            .collect()
    }

    #[test]
    fn phase_emits_rdf_type_plus_two_predicates() {
        let c = workflow_vocabulary();
        let mut values = BTreeMap::new();
        values.insert("wf:order".to_string(), fv(Value::Int(1)));
        values.insert(
            "dcterms:title".to_string(),
            fv(Value::Str("Go".to_string())),
        );
        let triples =
            render_class_triples(c, "urn:sophia:wf:demo:phase:1", "Phase", &values).unwrap();
        // rdf:type wf:Phase + wf:order + dcterms:title = 3 triples.
        assert_eq!(triples.len(), 3);
        assert_eq!(object_of(&triples, "#type").len(), 1);
        assert_eq!(
            object_of(&triples, "/dc/terms/title").len(),
            1,
            "dcterms:title must be emitted"
        );
        // The rdf:type object expands to wf:Phase.
        let type_obj = object_of(&triples, "#type")[0];
        assert_eq!(type_obj.as_nt(), "<http://mnemosyne.dev/workflow#Phase>");
    }

    #[test]
    fn missing_required_predicate_errors() {
        let c = workflow_vocabulary();
        let mut values = BTreeMap::new();
        values.insert("wf:order".to_string(), fv(Value::Int(1)));
        // dcterms:title is required and omitted.
        let err =
            render_class_triples(c, "urn:sophia:wf:demo:phase:1", "Phase", &values).unwrap_err();
        assert!(
            err.0.contains("missing required dcterms:title"),
            "{}",
            err.0
        );
    }

    #[test]
    fn extraneous_predicate_errors_predicates_not_in_vocab() {
        let c = workflow_vocabulary();
        let mut values = BTreeMap::new();
        values.insert("wf:order".to_string(), fv(Value::Int(1)));
        values.insert(
            "dcterms:title".to_string(),
            fv(Value::Str("Go".to_string())),
        );
        values.insert("wf:bogus".to_string(), fv(Value::Str("x".to_string())));
        let err =
            render_class_triples(c, "urn:sophia:wf:demo:phase:1", "Phase", &values).unwrap_err();
        assert!(
            err.0.contains("predicates not in vocab for Phase"),
            "{}",
            err.0
        );
        assert!(err.0.contains("wf:bogus"), "{}", err.0);
    }

    #[test]
    fn multi_valued_predicate_emits_one_triple_per_value() {
        let c = workflow_vocabulary();
        // Workflow.wf:phase is multi=true, uri. Provide a complete required set.
        let mut values = BTreeMap::new();
        values.insert("wf:name".to_string(), fv(Value::Str("demo".into())));
        values.insert("wf:description".to_string(), fv(Value::Str("d".into())));
        values.insert(
            "wf:phase".to_string(),
            FieldValue::Many(vec![
                Value::Uri("urn:sophia:wf:demo:phase:1".into()),
                Value::Uri("urn:sophia:wf:demo:phase:2".into()),
            ]),
        );
        values.insert(
            "wf:scriptBlock".to_string(),
            fv(Value::Placeholder("SCRIPT_BLOCK".into())),
        );
        values.insert("wf:scriptSha256".to_string(), fv(Value::Str("abc".into())));
        let triples = render_class_triples(
            c,
            "urn:mnemosyne:local:graph:lab:doc:wf-demo",
            "Workflow",
            &values,
        )
        .unwrap();
        // Two wf:phase triples.
        assert_eq!(object_of(&triples, "workflow#phase").len(), 2);
        // The scriptBlock placeholder renders as a placeholder URI.
        let sb = object_of(&triples, "workflow#scriptBlock")[0];
        assert_eq!(sb.as_nt(), "<urn:wf-emit:placeholder:SCRIPT_BLOCK>");
    }

    #[test]
    fn rerendering_same_class_diffs_to_zero_ops() {
        // Idempotency at the mint level: rendering twice and diffing the two
        // sets by canon key yields zero add/zero remove.
        let c = workflow_vocabulary();
        let mut values = BTreeMap::new();
        values.insert("wf:order".to_string(), fv(Value::Int(2)));
        values.insert("dcterms:title".to_string(), fv(Value::Str("Second".into())));
        let a = render_class_triples(c, "urn:sophia:wf:demo:phase:2", "Phase", &values).unwrap();
        let b = render_class_triples(c, "urn:sophia:wf:demo:phase:2", "Phase", &values).unwrap();
        let diff = crate::emporium::terms::diff_triples(&a, &b);
        assert!(diff.is_empty());
        // canon keys are equal pairwise.
        let ka: Vec<_> = a.iter().map(canon).collect();
        let kb: Vec<_> = b.iter().map(canon).collect();
        assert_eq!(ka, kb);
    }

    #[test]
    fn protocol_emits_no_rdf_type() {
        // Protocol has empty rdf_types; rendering its required wf:name emits no
        // type triple. (Protocol is registry-only / never minted per-workflow,
        // but render_class_triples itself does not special-case it.)
        let c = workflow_vocabulary();
        let mut values = BTreeMap::new();
        values.insert("wf:name".to_string(), fv(Value::Str("p".into())));
        values.insert(
            "wf:protocolVersion".to_string(),
            fv(Value::Str("0.2.0".into())),
        );
        let triples =
            render_class_triples(c, "urn:sophia:wf:protocol", "Protocol", &values).unwrap();
        assert!(object_of(&triples, "#type").is_empty());
    }
}
