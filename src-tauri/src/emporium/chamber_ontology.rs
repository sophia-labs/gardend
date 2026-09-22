//! The CHAMBER ontology resolver (EA-3 §4) — read an agent's RUNTIME-PROPOSED
//! domain ontology back out of the chamber graph and parse it to a typed
//! [`VocabularyContract`], so the agent's INSTANCES are SHACL-validated against
//! ITS OWN proposed shapes (not an embedded vocab).
//!
//! THE SEAM. The EA-3 spine validates generic-ingest instances against EMBEDDED
//! vocabs ([`crate::emporium::contract::get_vocabulary`], resolved at build time).
//! The chamber's net-new piece is resolving the agent's IN-GRAPH proposed ontology
//! instead: the agent published a `chm:DomainOntology` record (a born-RDF snapshot
//! of a `VocabularyContract`, carried byte-faithfully as a `chm:contractJson`
//! literal) into `:projection:chamber`; this module reads that literal back and
//! deserializes it to the SAME typed contract shape the embedded vocabs parse to.
//! The parsed contract then feeds `vocab_to_shacl` + `validate_desired` exactly
//! like an embedded one — the validation engine never knows the difference.
//!
//! CORE-vs-DOMAIN (load-bearing): the resolver only ever returns the agent's
//! DOMAIN ontology. The CORE world-model meta-ontology (the embedded packs) is
//! resolved by `get_vocabulary` and is untouchable from here.
//!
//! Two-tier lookup (the A ⊂ B merge, embedded-wins): callers try `get_vocabulary`
//! FIRST and fall back to [`resolve_chamber_ontology`] only on a miss — so a
//! chamber ontology can never shadow a built-in vocab name.

use oxigraph::sparql::{QueryResults, SparqlEvaluator};
use oxigraph::store::Store;

use crate::emporium::contract::{get_vocabulary, VocabularyContract};
use crate::rdf::graph_subject;

/// The chamber namespace prefix (matches `emporium-chamber.golden.json`).
pub(crate) const CHM_NS: &str = "http://mnemosyne.dev/chamber#";

/// The `:projection:chamber` named-graph IRI for a graph — the reserved sink the
/// `emporium-chamber` pack materializes `chm:DomainOntology` records into. Parallel
/// to `memory_projection_graph_iri` / `violations_projection_graph_iri`.
pub(crate) fn chamber_projection_graph_iri(graph_id: &str) -> String {
    format!("{}:projection:chamber", graph_subject(graph_id))
}

/// A resolved chamber ontology: the parsed [`VocabularyContract`] the agent
/// proposed, plus its in-graph record subject (for instrumentation / supersession
/// bookkeeping).
#[derive(Debug, Clone)]
pub(crate) struct ResolvedOntology {
    /// The owned, parsed contract the agent proposed (the source of the SHACL
    /// shapes its instances are validated against).
    pub(crate) contract: VocabularyContract,
    /// The `chm:DomainOntology` record subject this contract was read from.
    pub(crate) subject: String,
    /// The stored version string (`chm:ontologyVersion`).
    pub(crate) version: String,
    /// The RAW stored `chm:contractJson` literal (byte-faithful) — carried so a
    /// supersession demote can re-state the prior record WITHOUT re-serializing the
    /// parsed contract (`VocabularyContract` is Deserialize-only).
    pub(crate) contract_json: String,
}

/// Resolve the ACTIVE `chm:DomainOntology` for `ontology_name` in this chamber
/// graph and parse its stored contract.
///
/// Reads `:projection:chamber` for a `chm:DomainOntology` record whose
/// `chm:ontologyName` equals `ontology_name` and whose `chm:status` is `"active"`,
/// extracts its `chm:contractJson` literal, and deserializes it to a typed
/// [`VocabularyContract`] — the SAME parse the embedded vocabs use, so a malformed
/// in-graph proposal is caught HERE (before it reaches the SHACL gate) with a loud
/// error.
///
/// Returns `Ok(None)` when no active ontology of that name exists (the caller then
/// has no chamber contract to validate against — a loud "unknown vocab" upstream).
/// Returns `Err` only on a genuine fault: a store/SPARQL error, an ontology record
/// with no/empty `chm:contractJson`, or a `chm:contractJson` that does not parse to
/// a `VocabularyContract` (the immutable-testimony contract: a broken proposal must
/// be re-published as a new version, never silently skipped).
pub(crate) fn resolve_chamber_ontology(
    store: &Store,
    graph_id: &str,
    ontology_name: &str,
) -> Result<Option<ResolvedOntology>, String> {
    let chamber = chamber_projection_graph_iri(graph_id);
    // The name is a string literal in the record; escape any quote/backslash so the
    // SPARQL FILTER cannot be perturbed by an exotic name.
    let name_lit = sparql_quote(ontology_name);
    let query = format!(
        "SELECT ?s ?json ?version WHERE {{ GRAPH <{chamber}> {{ \
           ?s a <{CHM_NS}DomainOntology> ; \
              <{CHM_NS}ontologyName> {name_lit} ; \
              <{CHM_NS}status> \"active\" ; \
              <{CHM_NS}contractJson> ?json ; \
              <{CHM_NS}ontologyVersion> ?version . \
         }} }}"
    );

    let solutions = match SparqlEvaluator::new()
        .parse_query(&query)
        .map_err(|e| format!("parse chamber ontology query: {e}"))?
        .on_store(store)
        .execute()
        .map_err(|e| format!("execute chamber ontology query: {e}"))?
    {
        QueryResults::Solutions(s) => s,
        _ => return Err("chamber ontology query expected SELECT solutions".to_string()),
    };

    // Take the first active record (the lifecycle invariant: at most one active
    // ontology per name — a v2 demotes v1 to "superseded" before activating).
    let mut solutions = solutions;
    if let Some(sol) = solutions.next() {
        let sol = sol.map_err(|e| format!("chamber ontology row: {e}"))?;
        let subject = match sol.get("s").ok_or("chamber ontology row missing ?s")? {
            oxigraph::model::Term::NamedNode(n) => n.as_str().to_string(),
            other => return Err(format!("chamber ontology subject not a NamedNode: {other}")),
        };
        let json = literal_value(
            sol.get("json")
                .ok_or("chamber ontology row missing ?json")?,
        )?;
        if json.trim().is_empty() {
            return Err(format!(
                "chamber ontology <{subject}> has an empty chm:contractJson (re-propose a valid contract)"
            ));
        }
        let version = literal_value(
            sol.get("version")
                .ok_or("chamber ontology row missing ?version")?,
        )?;
        let contract: VocabularyContract = serde_json::from_str(&json).map_err(|e| {
            format!(
                "chamber ontology <{subject}> chm:contractJson does not parse to a VocabularyContract: {e} \
                 (immutable testimony — re-publish a corrected version)"
            )
        })?;
        return Ok(Some(ResolvedOntology {
            contract,
            subject,
            version,
            contract_json: json,
        }));
    }
    Ok(None)
}

/// Read the stored `chm:contractJson` literal for the record at EXACTLY `subject`
/// (ANY lifecycle status — active OR superseded), if one exists. The
/// immutable-published-version check ([`crate::emporium::chamber::propose_domain_ontology`])
/// uses this to detect an in-place OVERWRITE of a published `(name, version)` subject:
/// `propose_domain_ontology` mints the subject deterministically from `name`+`version`
/// (a versioned logical id), so re-proposing the SAME `(name, version)` with DIFFERENT
/// content would converge to this subject and silently overwrite published testimony.
///
/// Returns `Ok(None)` when no `chm:DomainOntology` record exists at that subject, the
/// raw stored literal otherwise. `Err` only on a store/SPARQL fault or a non-literal
/// `chm:contractJson`. The subject is a code-minted IRI (`sanitize_local_id`ed), safe
/// to interpolate — same trust basis as [`resolve_chamber_ontology`]'s `<{chamber}>`.
pub(crate) fn stored_contract_json_for_subject(
    store: &Store,
    subject: &str,
) -> Result<Option<String>, String> {
    let query = format!(
        "SELECT ?json WHERE {{ GRAPH ?g {{ \
           <{subject}> a <{CHM_NS}DomainOntology> ; \
                       <{CHM_NS}contractJson> ?json . \
         }} }}"
    );
    let solutions = match SparqlEvaluator::new()
        .parse_query(&query)
        .map_err(|e| format!("parse chamber subject query: {e}"))?
        .on_store(store)
        .execute()
        .map_err(|e| format!("execute chamber subject query: {e}"))?
    {
        QueryResults::Solutions(s) => s,
        _ => return Err("chamber subject query expected SELECT solutions".to_string()),
    };
    let mut solutions = solutions;
    if let Some(sol) = solutions.next() {
        let sol = sol.map_err(|e| format!("chamber subject row: {e}"))?;
        let json = literal_value(sol.get("json").ok_or("chamber subject row missing ?json")?)?;
        return Ok(Some(json));
    }
    Ok(None)
}

/// Resolve the [`VocabularyContract`] an ingest's `vocab` names — the TWO-TIER
/// (A ⊂ B) resolver the CHAMBER instance path uses. Tries the EMBEDDED registry
/// FIRST ([`get_vocabulary`]); on a miss, falls back to the agent's RUNTIME
/// in-graph ontology of that name ([`resolve_chamber_ontology`]). Returns an
/// OWNED contract (embedded contracts are cloned; chamber contracts are parsed
/// from the in-graph literal) so the same call site serves both tiers uniformly.
///
/// `Err` on a genuinely-unknown vocab (neither embedded nor an active chamber
/// ontology) or a malformed in-graph proposal. This is the seam that makes the
/// agent's instances validate against ITS OWN proposed shapes: the embedded vocabs
/// can never be shadowed (embedded wins), and a chamber vocab resolves to the live
/// proposed contract.
pub(crate) fn resolve_ingest_contract(
    store: &Store,
    graph_id: &str,
    vocab: &str,
) -> Result<VocabularyContract, String> {
    if let Some(embedded) = get_vocabulary(vocab) {
        return Ok(embedded.clone());
    }
    match resolve_chamber_ontology(store, graph_id, vocab)? {
        Some(resolved) => Ok(resolved.contract),
        None => Err(format!(
            "unknown vocab '{vocab}': not an embedded pack and no ACTIVE chamber ontology of that \
             name in this graph (propose it via propose_domain_ontology first)"
        )),
    }
}

/// Extract the lexical value of a literal object term (strip the `"…"^^<dt>` or
/// `"…"@lang` decorations oxigraph renders). A non-literal is a loud error.
fn literal_value(term: &oxigraph::model::Term) -> Result<String, String> {
    match term {
        oxigraph::model::Term::Literal(l) => Ok(l.value().to_string()),
        other => Err(format!("expected a literal, got {other}")),
    }
}

/// SPARQL-quote a string literal value (escape `\\` and `"`), wrapped in quotes.
fn sparql_quote(value: &str) -> String {
    let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxigraph::model::GraphName;
    use oxigraph::model::{Literal, NamedNode, Quad, Term as OxTerm};

    const GRAPH: &str = "lab";

    fn ttl_quad(s: &str, p: &str, o: OxTerm, g: &str) -> Quad {
        Quad::new(
            NamedNode::new(s).unwrap(),
            NamedNode::new(p).unwrap(),
            o,
            GraphName::NamedNode(NamedNode::new(g).unwrap()),
        )
    }

    /// A minimal valid VocabularyContract JSON for a one-class domain ontology.
    fn sample_contract_json() -> String {
        serde_json::json!({
            "name": "bench-domain",
            "version": "1.0.0",
            "title": "Benchmark Domain",
            "description": "agent-proposed",
            "namespaces": {
                "bench": "http://bench.ai/domain#",
                "rdf": "http://www.w3.org/1999/02/22-rdf-syntax-ns#",
                "xsd": "http://www.w3.org/2001/XMLSchema#"
            },
            "primary_prefix": "bench",
            "write_target": "projection:bench",
            "classes": {
                "Trial": {
                    "rdf_types": ["bench:Trial"],
                    "subject_rule": "{graph_subject}:projection:bench:trial:{localId}",
                    "source_kind": "current-state",
                    "identity_kind": "urn-template",
                    "store_mode": "materialize",
                    "store_target": "projection:bench",
                    "enforcement": "halt",
                    "reconciliation_strategy": "codeBacked",
                    "dispatch_mode": "current-state-materialize",
                    "predicates": {
                        "bench:label": {"datatype": "string", "required": true, "multi": false}
                    }
                }
            }
        })
        .to_string()
    }

    /// Seed an ACTIVE chm:DomainOntology record into :projection:chamber, then
    /// resolve it back to a typed contract.
    #[test]
    fn resolves_an_active_in_graph_ontology() {
        let store = Store::new().unwrap();
        let chamber = chamber_projection_graph_iri(GRAPH);
        let subj = format!("{chamber}:ontology:bench-domain-v1");
        let json = sample_contract_json();
        for (p, o) in [
            (
                "http://www.w3.org/1999/02/22-rdf-syntax-ns#type",
                OxTerm::NamedNode(NamedNode::new(format!("{CHM_NS}DomainOntology")).unwrap()),
            ),
            (
                &format!("{CHM_NS}ontologyName"),
                OxTerm::Literal(Literal::new_simple_literal("bench-domain")),
            ),
            (
                &format!("{CHM_NS}ontologyVersion"),
                OxTerm::Literal(Literal::new_simple_literal("1.0.0")),
            ),
            (
                &format!("{CHM_NS}status"),
                OxTerm::Literal(Literal::new_simple_literal("active")),
            ),
            (
                &format!("{CHM_NS}contractJson"),
                OxTerm::Literal(Literal::new_simple_literal(json.clone())),
            ),
        ] {
            store.insert(&ttl_quad(&subj, p, o, &chamber)).unwrap();
        }

        let resolved = resolve_chamber_ontology(&store, GRAPH, "bench-domain")
            .expect("resolve must not fault")
            .expect("an active ontology of that name exists");
        assert_eq!(resolved.contract.name, "bench-domain");
        assert_eq!(resolved.version, "1.0.0");
        assert_eq!(resolved.subject, subj);
        assert!(resolved.contract.classes.contains_key("Trial"));
        assert_eq!(
            resolved.contract.write_target.as_deref(),
            Some("projection:bench")
        );
    }

    /// No active ontology of that name → Ok(None) (the caller surfaces an
    /// "unknown vocab" upstream; this is not a fault).
    #[test]
    fn missing_ontology_resolves_to_none() {
        let store = Store::new().unwrap();
        let got = resolve_chamber_ontology(&store, GRAPH, "absent").expect("no fault");
        assert!(got.is_none());
    }

    /// A SUPERSEDED record of the same name is NOT resolved (only active counts) —
    /// the lifecycle gate that makes v2 the one whose shapes validate new instances.
    #[test]
    fn superseded_record_is_not_resolved() {
        let store = Store::new().unwrap();
        let chamber = chamber_projection_graph_iri(GRAPH);
        let subj = format!("{chamber}:ontology:bench-domain-v0");
        for (p, o) in [
            (
                "http://www.w3.org/1999/02/22-rdf-syntax-ns#type",
                OxTerm::NamedNode(NamedNode::new(format!("{CHM_NS}DomainOntology")).unwrap()),
            ),
            (
                &format!("{CHM_NS}ontologyName"),
                OxTerm::Literal(Literal::new_simple_literal("bench-domain")),
            ),
            (
                &format!("{CHM_NS}ontologyVersion"),
                OxTerm::Literal(Literal::new_simple_literal("0.1.0")),
            ),
            (
                &format!("{CHM_NS}status"),
                OxTerm::Literal(Literal::new_simple_literal("superseded")),
            ),
            (
                &format!("{CHM_NS}contractJson"),
                OxTerm::Literal(Literal::new_simple_literal(sample_contract_json())),
            ),
        ] {
            store.insert(&ttl_quad(&subj, p, o, &chamber)).unwrap();
        }
        let got = resolve_chamber_ontology(&store, GRAPH, "bench-domain").expect("no fault");
        assert!(
            got.is_none(),
            "a superseded ontology must not resolve as active"
        );
    }

    /// TEETH: a stored contractJson that does NOT parse to a VocabularyContract is a
    /// LOUD error here — the malformed proposal is caught before it can reach the
    /// SHACL gate (immutable testimony: re-publish a corrected version).
    #[test]
    fn malformed_contract_json_is_a_loud_error() {
        let store = Store::new().unwrap();
        let chamber = chamber_projection_graph_iri(GRAPH);
        let subj = format!("{chamber}:ontology:broken");
        for (p, o) in [
            (
                "http://www.w3.org/1999/02/22-rdf-syntax-ns#type",
                OxTerm::NamedNode(NamedNode::new(format!("{CHM_NS}DomainOntology")).unwrap()),
            ),
            (
                &format!("{CHM_NS}ontologyName"),
                OxTerm::Literal(Literal::new_simple_literal("broken")),
            ),
            (
                &format!("{CHM_NS}ontologyVersion"),
                OxTerm::Literal(Literal::new_simple_literal("1.0.0")),
            ),
            (
                &format!("{CHM_NS}status"),
                OxTerm::Literal(Literal::new_simple_literal("active")),
            ),
            (
                &format!("{CHM_NS}contractJson"),
                OxTerm::Literal(Literal::new_simple_literal("{ not valid json")),
            ),
        ] {
            store.insert(&ttl_quad(&subj, p, o, &chamber)).unwrap();
        }
        let err = resolve_chamber_ontology(&store, GRAPH, "broken")
            .expect_err("a malformed contractJson must be a loud error");
        assert!(err.contains("does not parse"), "{err}");
    }
}
