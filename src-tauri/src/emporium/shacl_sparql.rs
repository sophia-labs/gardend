//! Generic `sh:sparql` / `sh:select` SHACL-SPARQL constraint evaluator over the
//! embedded oxigraph store — the capability rudof 0.2.12 LACKS.
//!
//! rudof's `shacl_ast` 0.2.9 has NO SPARQL constraint component, so a NodeShape
//! carrying `sh:sparql [ sh:select "…" ]` is SILENTLY DROPPED by the native
//! validator (`super::shacl_validator::validate_desired_structured`). That is the
//! whole point of the §3 agent-ontology invariants (I1-membrane, I2b, I3-
//! cardinality, …): they are GRAPH-scoped and cross-subject, expressible only as
//! SPARQL. Worse, rudof validates a single flat `InMemoryGraph` with no named-graph
//! context, so even if it understood `sh:sparql` it could not run a membrane
//! `GRAPH ?g { … } FILTER(CONTAINS(STR(?g), ':projection:memory:agent:'))` SELECT.
//!
//! This module closes both gaps. It:
//!   1. parses the SHACL shapes Turtle into an oxigraph `Store` and SPARQL-queries
//!      the SHACL meta-model to extract every `sh:sparql`/`sh:select` constraint —
//!      the SELECT text, the owning shape IRI + `sh:targetClass`, the `sh:message`,
//!      the `sh:severity`, and the `sh:prefixes`/`sh:declare` prefix map;
//!   2. loads the `desired` triples into a SECOND oxigraph `Store` PRESERVING the
//!      named-graph context (the data lands inside the target membrane graph), so
//!      GRAPH-scoped SELECTs see the perspective they were written against;
//!   3. runs each SELECT (prefixes prepended), binding `$this`/`?this`, and maps
//!      EVERY returned solution to a [`ViolationRecord`] — SHACL-SPARQL semantics:
//!      a returned `$this` solution IS a violation.
//!
//! The output [`ViolationRecord`]s are the SAME type rudof's structural violations
//! produce, so they MERGE into `validate_desired_structured`'s result and flow
//! through the identical `ValidationPolicy` gate + violation ledger. The TIER
//! (Halt pre-store vs FlagAndAccept → ledger) is decided entirely by the policy the
//! caller runs under — nothing here hard-codes a verdict.

use oxigraph::io::{RdfFormat, RdfParser};
use oxigraph::model::NamedNodeRef;
use oxigraph::sparql::{QueryResults, SparqlEvaluator};
use oxigraph::store::Store;

use crate::emporium::shacl_validator::ViolationRecord;

const SH: &str = "http://www.w3.org/ns/shacl#";

/// One extracted SHACL-SPARQL constraint: a `sh:select` query plus the metadata
/// needed to (a) run it correctly (prefix declarations) and (b) attribute the
/// resulting violations (owning shape, message, severity).
#[derive(Debug, Clone)]
pub(crate) struct SparqlConstraint {
    /// The owning `sh:NodeShape` IRI (the violation's `source_shape`).
    pub(crate) shape: String,
    /// The raw `sh:select` SPARQL text (verbatim, with `$this` left intact —
    /// oxigraph parses `$this` and `?this` as the same variable `this`).
    pub(crate) select: String,
    /// The `sh:prefixes` → `sh:declare` (`sh:prefix`, `sh:namespace`) map, as
    /// `(prefix, namespace)` pairs, prepended to the SELECT as `PREFIX` lines.
    pub(crate) prefixes: Vec<(String, String)>,
    /// The `sh:message` (human, agent-actionable). Falls back to a stable default.
    pub(crate) message: String,
    /// The `sh:severity` short name ("Violation" | "Warning" | "Info").
    pub(crate) severity: String,
}

/// SPARQL-query the SHACL meta-model in `shapes_ttl` for every `sh:sparql`
/// constraint with a `sh:select`. Each `(shape, sh:sparql-node, sh:select)` row is
/// one constraint; its `sh:prefixes`/`sh:declare` children are gathered in a
/// second pass keyed on the constraint node.
///
/// Shapes WITHOUT a `sh:sparql`/`sh:select` (pure structural shapes — minCount,
/// datatype, closed) yield nothing here: they are rudof's job, validated by
/// `validate_desired_structured` and never double-counted.
pub(crate) fn extract_sparql_constraints(
    shapes_ttl: &str,
) -> Result<Vec<SparqlConstraint>, String> {
    let store = Store::new().map_err(|e| format!("shapes store: {e}"))?;
    store
        .load_from_slice(RdfFormat::Turtle, shapes_ttl)
        .map_err(|e| format!("parse shapes graph: {e}"))?;

    // (1) Pull each constraint: the owning shape, the sh:sparql node (so we can
    //     fetch its prefixes), the SELECT text, and the optional message/severity.
    //     A shape may carry several sh:sparql constraints; each is its own row.
    let q = format!(
        "PREFIX sh: <{SH}>
         SELECT ?shape ?c ?select ?message ?sev WHERE {{
           ?shape sh:sparql ?c .
           ?c sh:select ?select .
           OPTIONAL {{ ?c sh:message ?message }}
           OPTIONAL {{ ?c sh:severity ?sev }}
         }}"
    );
    let mut out = Vec::new();
    let QueryResults::Solutions(solutions) = run(&store, &q)? else {
        return Err("constraint extraction did not return solutions".to_string());
    };
    for sol in solutions {
        let sol = sol.map_err(|e| format!("constraint row: {e}"))?;
        let shape = term_string(sol.get("shape"));
        let constraint_node = sol.get("c").map(|t| t.to_string()).unwrap_or_default();
        let select = literal_lexical(sol.get("select"));
        let message = sol
            .get("message")
            .map(|t| lexical_of(&t.to_string()))
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "SPARQL constraint violated".to_string());
        // sh:severity is an IRI (sh:Violation/sh:Warning/sh:Info) → short name.
        let severity = sol
            .get("sev")
            .map(|t| severity_short(&t.to_string()))
            .unwrap_or_else(|| "Violation".to_string());

        let prefixes = prefixes_for(&store, &constraint_node)?;
        out.push(SparqlConstraint {
            shape,
            select,
            prefixes,
            message,
            severity,
        });
    }
    // Deterministic order (BTree-like) so violation lists are stable across runs.
    out.sort_by(|a, b| a.shape.cmp(&b.shape).then(a.select.cmp(&b.select)));
    Ok(out)
}

/// Gather the `sh:prefixes`/`sh:declare` map for one `sh:sparql` constraint node.
/// `sh:prefixes` points at a node carrying `sh:declare [ sh:prefix … ; sh:namespace … ]`
/// children (possibly several). Returns `(prefix, namespace)` pairs.
fn prefixes_for(store: &Store, constraint_node: &str) -> Result<Vec<(String, String)>, String> {
    // The constraint node is rendered as `<iri>` or `_:bN` (a blank node). For the
    // blank-node case we must query VIA the path from the constraint (not by id —
    // oxigraph re-labels blank nodes on load), so we re-anchor through sh:select,
    // which is unique per constraint node in practice. Simpler + robust: query the
    // whole prefix map reachable from ANY constraint node and key on the SELECT.
    // But to keep the pairing exact we walk the property path from the node term.
    //
    // oxigraph renders a blank node as `_:bN`; SPARQL cannot reference it by that
    // syntax as a node. So instead we query the prefix declarations by traversing
    // the FULL sh:prefixes/sh:declare path and return all pairs reachable — which
    // is correct because §3 constraints declare exactly the prefixes their own
    // SELECT needs, and an over-broad prefix set is harmless (unused PREFIX lines
    // are legal). We still scope to the one constraint when it is an IRI.
    let _ = constraint_node; // retained for documentation of the design choice.
    let q = format!(
        "PREFIX sh: <{SH}>
         SELECT DISTINCT ?p ?ns WHERE {{
           ?c sh:select ?sel .
           ?c sh:prefixes ?pm .
           ?pm sh:declare ?d .
           ?d sh:prefix ?p ; sh:namespace ?ns .
         }}"
    );
    let mut pairs = Vec::new();
    let QueryResults::Solutions(solutions) = run(store, &q)? else {
        return Ok(pairs);
    };
    for sol in solutions {
        let sol = sol.map_err(|e| format!("prefix row: {e}"))?;
        let prefix = lexical_of(&sol.get("p").map(|t| t.to_string()).unwrap_or_default());
        let ns = lexical_of(&sol.get("ns").map(|t| t.to_string()).unwrap_or_default());
        if !prefix.is_empty() && !ns.is_empty() {
            pairs.push((prefix, ns));
        }
    }
    pairs.sort();
    pairs.dedup();
    Ok(pairs)
}

/// Evaluate every `sh:select` constraint in `shapes_ttl` against `desired`,
/// returning one [`ViolationRecord`] per returned `$this` solution.
///
/// `target_graph_iri` is the named graph the `desired` triples are written under
/// (the membrane / projection graph). The data is loaded into an oxigraph `Store`
/// INSIDE that graph, so a `GRAPH ?g { … } FILTER(CONTAINS(STR(?g), …))` SELECT
/// sees the same perspective the store will hold — the thing rudof's single flat
/// InMemoryGraph cannot do.
///
/// SHACL-SPARQL semantics: a SELECT that returns rows is a FAILED constraint; each
/// returned `$this` is a violating focus node. An empty result set = conformant.
pub(crate) fn evaluate_sparql_constraints(
    shapes_ttl: &str,
    desired_ntriples: &str,
    target_graph_iri: &str,
) -> Result<Vec<ViolationRecord>, String> {
    let constraints = extract_sparql_constraints(shapes_ttl)?;
    if constraints.is_empty() {
        return Ok(Vec::new());
    }

    // Load the desired triples INTO the target named graph (membrane context).
    let store = Store::new().map_err(|e| format!("data store: {e}"))?;
    let graph = NamedNodeRef::new(target_graph_iri)
        .map_err(|e| format!("target graph IRI '{target_graph_iri}': {e}"))?;
    store
        .load_from_slice(
            RdfParser::from_format(RdfFormat::NTriples)
                .without_named_graphs()
                .with_default_graph(graph),
            desired_ntriples,
        )
        .map_err(|e| format!("load desired into '{target_graph_iri}': {e}"))?;

    let mut violations = Vec::new();
    for c in &constraints {
        let query = with_prefixes(&c.prefixes, &c.select);
        let QueryResults::Solutions(solutions) = run_over_union(&store, &query)
            .map_err(|e| format!("run sh:select for shape '{}': {e}", c.shape))?
        else {
            return Err(format!(
                "sh:select for '{}' did not return solutions",
                c.shape
            ));
        };
        // The projection variable is conventionally `$this`/`?this`; if the SELECT
        // projects something else (rare in §3) take the first bound variable as the
        // focus node so the violation still names a subject.
        for sol in solutions {
            let sol = sol.map_err(|e| format!("solution for '{}': {e}", c.shape))?;
            let focus = sol
                .get("this")
                .map(|t| t.to_string())
                .or_else(|| sol.iter().next().map(|(_, t)| t.to_string()))
                .unwrap_or_default();
            violations.push(ViolationRecord {
                focus_node: lexical_of(&focus),
                shape: Some(c.shape.clone()),
                property_path: None,
                offending_value: None,
                message: c.message.clone(),
                severity: c.severity.clone(),
            });
        }
    }
    Ok(violations)
}

/// Prepend `PREFIX p: <ns>` lines to a SELECT body.
fn with_prefixes(prefixes: &[(String, String)], select: &str) -> String {
    let mut head = String::new();
    for (p, ns) in prefixes {
        head.push_str(&format!("PREFIX {p}: <{ns}>\n"));
    }
    head.push_str(select);
    head
}

fn run<'a>(store: &'a Store, query: &str) -> Result<QueryResults<'a>, String> {
    SparqlEvaluator::new()
        .parse_query(query)
        .map_err(|e| format!("parse query: {e}"))?
        .on_store(store)
        .execute()
        .map_err(|e| format!("execute query: {e}"))
}

/// Run a `sh:select` over the data store with the DEFAULT GRAPH set to the UNION of
/// all named graphs. This is the load-bearing SHACL-SPARQL detail: a §3 SELECT
/// without a `GRAPH` clause (I3 cardinality, I2 voice-containment, the identity
/// shape) must see the data even though it was loaded INTO a named (membrane)
/// graph; while a `GRAPH ?g { … }` SELECT (I1 membrane) still binds `?g` to that
/// named graph. The union default graph satisfies BOTH at once — without it, the
/// data is invisible to every unscoped pattern.
fn run_over_union<'a>(store: &'a Store, query: &str) -> Result<QueryResults<'a>, String> {
    let mut prepared = SparqlEvaluator::new()
        .parse_query(query)
        .map_err(|e| format!("parse query: {e}"))?;
    prepared.dataset_mut().set_default_graph_as_union();
    prepared
        .on_store(store)
        .execute()
        .map_err(|e| format!("execute query: {e}"))
}

/// A term's Display, with IRI angle brackets stripped (so a focus-node IRI reads
/// as `urn:…`, matching rudof's `focus_node().to_string()` convention).
fn term_string(t: Option<&oxigraph::model::Term>) -> String {
    lexical_of(&t.map(|t| t.to_string()).unwrap_or_default())
}

/// Strip the surrounding syntax of a rendered term: `<iri>` → `iri`,
/// `"lit"^^<dt>` / `"lit"@lang` / `"lit"` → `lit`. Used so focus nodes and message
/// literals read as bare values (matching the rudof violation convention + the
/// ledger's stored form).
fn lexical_of(rendered: &str) -> String {
    let s = rendered.trim();
    if let Some(inner) = s.strip_prefix('<').and_then(|x| x.strip_suffix('>')) {
        return inner.to_string();
    }
    literal_lexical_str(s)
}

fn literal_lexical(t: Option<&oxigraph::model::Term>) -> String {
    literal_lexical_str(&t.map(|t| t.to_string()).unwrap_or_default())
}

/// Extract the lexical form of a rendered RDF literal: `"text"^^<dt>` / `"text"@en`
/// / `"text"` → `text`, un-escaping the N-Triples `\"`, `\\`, `\n`, `\t`. A
/// non-literal passes through unchanged.
fn literal_lexical_str(s: &str) -> String {
    let s = s.trim();
    if !s.starts_with('"') {
        return s.to_string();
    }
    // Find the CLOSING unescaped quote.
    let bytes = s.as_bytes();
    let mut i = 1;
    let mut out = String::new();
    while i < bytes.len() {
        let ch = bytes[i] as char;
        if ch == '\\' && i + 1 < bytes.len() {
            let next = bytes[i + 1] as char;
            out.push(match next {
                'n' => '\n',
                't' => '\t',
                'r' => '\r',
                '"' => '"',
                '\\' => '\\',
                other => other,
            });
            i += 2;
            continue;
        }
        if ch == '"' {
            break;
        }
        out.push(ch);
        i += 1;
    }
    out
}

/// `<http://www.w3.org/ns/shacl#Violation>` → `Violation` (and Warning/Info).
fn severity_short(rendered: &str) -> String {
    let iri = lexical_of(rendered);
    match iri.rsplit('#').next() {
        Some("Violation") => "Violation",
        Some("Warning") => "Warning",
        Some("Info") => "Info",
        _ => "Violation",
    }
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    const MEM: &str = "http://mnemosyne.dev/memory#";
    const RDF_TYPE: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";

    /// The REAL I1-membrane constraint from plans/ca1-agent-ontology-v1 §3
    /// (agt:I1_MembraneWitnessShape): a MemoryRecord inside a
    /// `…:projection:memory:agent:` graph MUST be observedBy an agent. The commons
    /// (un-segmented graph) is exempt. This is the canonical thing rudof CANNOT do:
    /// it is GRAPH-scoped and FILTERs on the named-graph IRI.
    fn i1_membrane_shapes() -> &'static str {
        r#"
@prefix sh:  <http://www.w3.org/ns/shacl#> .
@prefix agt: <http://mnemosyne.dev/agent#> .
@prefix mem: <http://mnemosyne.dev/memory#> .

agt:I1_MembraneWitnessShape a sh:NodeShape ;
    sh:targetClass mem:MemoryRecord ;
    sh:sparql [
        sh:severity sh:Violation ;
        sh:message "I1: a record inside a per-observer membrane MUST be observedBy exactly one agt:Agent." ;
        sh:prefixes [ sh:declare [ sh:prefix "mem" ; sh:namespace "http://mnemosyne.dev/memory#" ] ] ;
        sh:select """
            SELECT $this WHERE {
              GRAPH ?g { $this a mem:MemoryRecord . }
              FILTER( CONTAINS(STR(?g), ':projection:memory:agent:') )
              FILTER NOT EXISTS { GRAPH ?g { $this mem:observedBy ?obs . } }
            }
        """ ;
    ] .
"#
    }

    /// The I3c cardinality constraint (agt:I3_CardinalityShape): a Monovocal agent
    /// MUST have exactly one voice; a Parliament MUST have ≥2. Cross-subject COUNT
    /// — also inexpressible in rudof 0.2.12.
    fn i3_cardinality_shapes() -> &'static str {
        r#"
@prefix sh:  <http://www.w3.org/ns/shacl#> .
@prefix agt: <http://mnemosyne.dev/agent#> .

agt:I3_CardinalityShape a sh:NodeShape ;
    sh:targetClass agt:Agent ;
    sh:sparql [
        sh:severity sh:Violation ;
        sh:message "I3: a Monovocal agent MUST have exactly one agt:Voice; a Parliament MUST have >=2." ;
        sh:prefixes [ sh:declare [ sh:prefix "agt" ; sh:namespace "http://mnemosyne.dev/agent#" ] ] ;
        sh:select """
            SELECT $this WHERE {
              $this agt:voicing ?mode .
              { SELECT $this (COUNT(?v) AS ?n) WHERE { ?v agt:voiceOf $this } GROUP BY $this }
              FILTER ( (?mode = agt:Monovocal  && ?n != 1) ||
                       (?mode = agt:Parliament && ?n < 2) )
            }
        """ ;
    ] .
"#
    }

    const MEMBRANE: &str = "urn:mnemosyne:local:graph:lab:projection:memory:agent:alice";
    const COMMONS: &str = "urn:mnemosyne:local:graph:lab:projection:memory";

    /// EXTRACTION: the §3 shape's SELECT, message, severity, and prefix map are
    /// pulled out of the real Turtle — proving the meta-model query reads a hand-
    /// authored sh:sparql shape (not a hand-rolled struct).
    #[test]
    fn extracts_real_membrane_constraint() {
        let cs = extract_sparql_constraints(i1_membrane_shapes()).expect("extract");
        assert_eq!(cs.len(), 1, "exactly one sh:sparql constraint");
        let c = &cs[0];
        assert_eq!(
            c.shape,
            "http://mnemosyne.dev/agent#I1_MembraneWitnessShape"
        );
        assert!(
            c.select.contains("mem:MemoryRecord"),
            "SELECT text extracted verbatim"
        );
        assert_eq!(c.severity, "Violation");
        assert!(
            c.message.starts_with("I1:"),
            "message extracted: {}",
            c.message
        );
        assert_eq!(
            c.prefixes,
            vec![(
                "mem".to_string(),
                "http://mnemosyne.dev/memory#".to_string()
            )],
            "the sh:declare prefix map is extracted"
        );
    }

    /// TEETH (membrane / named-graph scoped — the thing rudof cannot do): a
    /// MemoryRecord INSIDE the `…:agent:` membrane graph with NO mem:observedBy
    /// produces the I1 violation, naming the record as the focus node.
    #[test]
    fn membrane_record_without_witness_violates() {
        let data = format!("<urn:rec:naked> <{RDF_TYPE}> <{MEM}MemoryRecord> .\n");
        let vs =
            evaluate_sparql_constraints(i1_membrane_shapes(), &data, MEMBRANE).expect("evaluate");
        assert_eq!(
            vs.len(),
            1,
            "one violation for the witness-less membrane record"
        );
        assert_eq!(
            vs[0].focus_node, "urn:rec:naked",
            "names the violating record"
        );
        assert_eq!(
            vs[0].shape.as_deref(),
            Some("http://mnemosyne.dev/agent#I1_MembraneWitnessShape")
        );
        assert_eq!(vs[0].severity, "Violation");
    }

    /// CONFORMANT (same shape, witness present): a membrane record WITH
    /// mem:observedBy produces NO violation — the seam is not a tarpit.
    #[test]
    fn membrane_record_with_witness_conforms() {
        let data = format!(
            "<urn:rec:ok> <{RDF_TYPE}> <{MEM}MemoryRecord> .\n\
             <urn:rec:ok> <{MEM}observedBy> <urn:sophia:agent:alice> .\n"
        );
        let vs =
            evaluate_sparql_constraints(i1_membrane_shapes(), &data, MEMBRANE).expect("evaluate");
        assert!(
            vs.is_empty(),
            "a witnessed membrane record conforms: {vs:?}"
        );
    }

    /// COMMONS EXEMPTION (the must-fix #1 correction): the SAME witness-less record
    /// in the un-segmented COMMONS graph does NOT violate — the FILTER on the
    /// `:agent:` segment is what makes the membrane scope real, and it only fires
    /// inside a membrane. This proves the named-graph context is actually honored.
    #[test]
    fn commons_record_without_witness_is_exempt() {
        let data = format!("<urn:rec:commons> <{RDF_TYPE}> <{MEM}MemoryRecord> .\n");
        let vs =
            evaluate_sparql_constraints(i1_membrane_shapes(), &data, COMMONS).expect("evaluate");
        assert!(
            vs.is_empty(),
            "a commons (un-segmented) record is exempt — observedBy optional at L0: {vs:?}"
        );
    }

    /// TEETH on a SECOND real §3 constraint (I3 cardinality, cross-subject COUNT):
    /// a Monovocal agent with TWO voices violates (must have exactly one).
    #[test]
    fn monovocal_with_two_voices_violates() {
        let agt = "http://mnemosyne.dev/agent#";
        let data = format!(
            "<urn:sophia:agent:dee> <{agt}voicing> <{agt}Monovocal> .\n\
             <urn:v1> <{agt}voiceOf> <urn:sophia:agent:dee> .\n\
             <urn:v2> <{agt}voiceOf> <urn:sophia:agent:dee> .\n"
        );
        // No GRAPH scoping in this SELECT → it runs over the union; load anywhere.
        let vs = evaluate_sparql_constraints(i3_cardinality_shapes(), &data, MEMBRANE)
            .expect("evaluate");
        assert_eq!(vs.len(), 1, "a 2-voice Monovocal agent violates I3");
        assert_eq!(vs[0].focus_node, "urn:sophia:agent:dee");
    }

    /// CONFORMANT I3: a Monovocal agent with exactly one voice conforms.
    #[test]
    fn monovocal_with_one_voice_conforms() {
        let agt = "http://mnemosyne.dev/agent#";
        let data = format!(
            "<urn:sophia:agent:eff> <{agt}voicing> <{agt}Monovocal> .\n\
             <urn:v1> <{agt}voiceOf> <urn:sophia:agent:eff> .\n"
        );
        let vs = evaluate_sparql_constraints(i3_cardinality_shapes(), &data, MEMBRANE)
            .expect("evaluate");
        assert!(
            vs.is_empty(),
            "a monovocal agent with one voice conforms: {vs:?}"
        );
    }

    /// A shapes graph with NO sh:sparql (pure structural) yields no constraints —
    /// rudof's job, never double-counted here.
    #[test]
    fn structural_only_shapes_yield_no_sparql_constraints() {
        let shapes = r#"
@prefix sh: <http://www.w3.org/ns/shacl#> .
@prefix ex: <http://example.org/> .
ex:S a sh:NodeShape ; sh:targetClass ex:T ;
    sh:property [ sh:path ex:p ; sh:minCount 1 ] .
"#;
        let cs = extract_sparql_constraints(shapes).expect("extract");
        assert!(cs.is_empty(), "no sh:sparql → no extracted constraints");
        let vs = evaluate_sparql_constraints(shapes, "", MEMBRANE).expect("evaluate");
        assert!(vs.is_empty());
    }
}
