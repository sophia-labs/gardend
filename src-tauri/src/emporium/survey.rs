//! Live-graph survey helpers — the read surface the (HELD) planner consumes.
//!
//! Port of the read paths in `app/services/emporium/ingest/survey.py`
//! (`survey_live` entity/run/node queries, `current_wf_triples`,
//! `docs_provenance`), adapted to gardend's seam.
//!
//! 🔴 PORT DELTA — the load-bearing read/write graph invariant. The Python
//! survey reads `GRAPH <{prefix}>` (the graph ROOT). gardend's validator BANS
//! the bare root for writes (`rdf_authority::validate_sparql_update_authority`),
//! forcing writes onto `<{root}:user:rdf>`. So every survey SELECT here names
//! `GRAPH <{user_rdf_graph_iri(graph_id)}>` — reads MUST follow writes or every
//! desired triple reads as missing forever and convergence is impossible. The
//! string-equality test [`tests::survey_read_graph_is_user_rdf_graph`] pins it.
//!
//! Binding parse delta: pyoxigraph hands the Python survey structured terms;
//! gardend's `SparqlQueryResult.rows` are `oxigraph term.to_string()` STRINGS
//! (`<uri>`, `"lit"`, `"lit"^^<dtype>`, `"lit"@lang`). [`parse_term`] turns them
//! back into typed [`Term`] so `canon()` compares values.

use std::collections::BTreeMap;

use oxigraph::model::{Literal, NamedNode};

use crate::app_error::AppResult;
use crate::app_runtime::AppHandle;
use crate::emporium::contract::VocabularyContract;
use crate::emporium::terms::Term;
use crate::rdf::graph_subject;
use crate::rdf_authority::user_rdf_graph_iri;
use crate::rdf_service::{run_sparql_query_service, SparqlInput};

/// The live snapshot of workflow-managed entities consumed by the planner
/// (HELD — P2). The planner will import this struct; for the DSL-agnostic slice
/// it is produced and round-tripped by the survey only.
#[derive(Debug, Default, Clone, serde::Serialize)]
pub(crate) struct Live {
    /// The graph id this survey was taken over.
    pub(crate) graph: String,
    /// The doc-URI subject prefix (`urn:mnemosyne:local:graph:{id}`).
    pub(crate) prefix: String,
    /// The named graph all reads (and future writes) target.
    pub(crate) read_graph: String,
    /// `{folderId: {label, parentId}}` — workspace folder projection. The planner
    /// uses this for `ensure_folder` discovery and phase/lib/prov folder lookup.
    /// Populated by the spine (P2-WP3) from the gardend workspace projection
    /// snapshot, NOT from SPARQL; `survey_live` leaves it empty.
    pub(crate) folders: BTreeMap<String, FolderEntry>,
    /// `{docId: {title, folderId}}` — workspace document projection. The planner
    /// uses this for `wf_folder` lookup, `cur_folder` for move, and resume-partial.
    /// Populated by the spine (P2-WP3) from the gardend workspace projection
    /// snapshot, NOT from SPARQL; `survey_live` leaves it empty.
    pub(crate) docs: BTreeMap<String, DocEntry>,
    /// `{name: {uri, docId, name, sha?}}`
    pub(crate) workflows: BTreeMap<String, EntityRow>,
    /// `{name: {uri, docId, name}}`
    pub(crate) archetypes: BTreeMap<String, EntityRow>,
    /// `{name: {uri, docId, name}}`
    pub(crate) contracts: BTreeMap<String, EntityRow>,
    /// run ids
    pub(crate) runs: Vec<String>,
    /// `{workflowUri: {label: nodeDocId}}`
    pub(crate) nodes_by_workflow: BTreeMap<String, BTreeMap<String, String>>,
}

#[derive(Debug, Default, Clone, serde::Serialize)]
pub(crate) struct EntityRow {
    pub(crate) uri: String,
    pub(crate) doc_id: Option<String>,
    pub(crate) name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) sha: Option<String>,
}

/// A workspace folder as seen by the survey projection (`{label, parentId}`).
/// Mirrors the Python `live["folders"][fid]` dict the planner reads.
#[derive(Debug, Default, Clone, serde::Serialize)]
pub(crate) struct FolderEntry {
    pub(crate) label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) parent_id: Option<String>,
}

/// A workspace document as seen by the survey projection (`{title, folderId}`).
/// Mirrors the Python `live["docs"][did]` dict the planner reads.
#[derive(Debug, Default, Clone, serde::Serialize)]
pub(crate) struct DocEntry {
    pub(crate) title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) folder_id: Option<String>,
}

fn doc_id_from_uri(uri: &str) -> Option<String> {
    // mirrors _DOC_ID_RE = r":doc:([^#]+)$"
    let idx = uri.rfind(":doc:")?;
    let tail = &uri[idx + ":doc:".len()..];
    if tail.is_empty() || tail.contains('#') {
        // a trailing fragment is excluded by the [^#]+$ anchor
        let before_hash = tail.split('#').next().unwrap_or("");
        if before_hash.is_empty() {
            return None;
        }
        // The Python regex anchors to end-of-string, so a fragment means no match.
        return None;
    }
    Some(tail.to_string())
}

/// Parse an oxigraph `term.to_string()` rendering back into a typed [`Term`].
/// Handles `<uri>`, `"lit"`, `"lit"^^<dtype>`, `"lit"@lang`, and our placeholder
/// scheme. The round-trip is exact for the cases `canon()` keys on
/// (numeric/dateTime/boolean), which is what the value-diff needs.
pub(crate) fn parse_term(s: &str) -> Term {
    let s = s.trim();
    if let Some(inner) = s.strip_prefix('<').and_then(|x| x.strip_suffix('>')) {
        if let Some(name) = inner.strip_prefix(crate::emporium::terms::PLACEHOLDER_NS) {
            return Term::Placeholder(name.to_string());
        }
        if let Ok(n) = NamedNode::new(inner) {
            return Term::Uri(n);
        }
        return Term::Placeholder(inner.to_string());
    }
    // Literal forms. Find the closing quote of the lexical value.
    if s.starts_with('"') {
        if let Some(close) = find_closing_quote(s) {
            let lexical = unescape_nt(&s[1..close]);
            let rest = &s[close + 1..];
            if let Some(dt) = rest.strip_prefix("^^<").and_then(|x| x.strip_suffix('>')) {
                let datatype = NamedNode::new(dt).unwrap_or_else(|_| {
                    NamedNode::new("http://www.w3.org/2001/XMLSchema#string").unwrap()
                });
                return Term::Lit(Literal::new_typed_literal(lexical, datatype));
            }
            if let Some(lang) = rest.strip_prefix('@') {
                return Term::Lit(
                    Literal::new_language_tagged_literal(lexical, lang)
                        .unwrap_or_else(|_| Literal::new_simple_literal("")),
                );
            }
            return Term::Lit(Literal::new_simple_literal(lexical));
        }
    }
    // Fallback: treat as a simple literal of the raw string.
    Term::Lit(Literal::new_simple_literal(s.to_string()))
}

fn find_closing_quote(s: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    let mut i = 1;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2,
            b'"' => return Some(i),
            _ => i += 1,
        }
    }
    None
}

fn unescape_nt(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') => out.push('\n'),
                Some('r') => out.push('\r'),
                Some('t') => out.push('\t'),
                Some('"') => out.push('"'),
                Some('\\') => out.push('\\'),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn run_query(
    app: &AppHandle,
    graph_id: &str,
    query: String,
) -> AppResult<Vec<BTreeMap<String, String>>> {
    let result = run_sparql_query_service(
        app.clone(),
        SparqlInput {
            graph_id: graph_id.to_string(),
            query,
        },
    )?;
    Ok(result.rows)
}

/// 🔴 READ-GRAPH == WRITE-GRAPH (risk #1): the ONE place the survey resolves the
/// graph it reads RDF from. It is exactly [`user_rdf_graph_iri`] — the SAME graph
/// the applier writes (`applier::write_graph_iri`) — so a re-survey sees exactly
/// what was minted. Every survey read site (`survey_live`, `current_wf_triples`,
/// `docs_provenance`) names `GRAPH <{read_graph_iri}>`. The cross-WP invariant
/// test (`fixtures::tests::cross_wp_read_eq_write_eq_user_rdf`) pins
/// `survey read graph == applier write graph == user_rdf_graph_iri` as a standing
/// regression guard.
pub(crate) fn read_graph_iri(graph_id: &str) -> String {
    user_rdf_graph_iri(graph_id)
}

/// Survey the live workflow-managed entities. Reads from
/// `GRAPH <{read_graph_iri(graph_id)}>` exclusively.
pub(crate) fn survey_live(
    app: &AppHandle,
    graph_id: &str,
    contract: &VocabularyContract,
) -> AppResult<Live> {
    let prefix = graph_subject(graph_id);
    let read_graph = read_graph_iri(graph_id);
    let graph_named = format!("<{read_graph}>");

    let wfns = contract.primary_namespace();

    // Query 1: Workflows, Archetypes, Contracts.
    let q_entities = format!(
        "SELECT ?s ?type ?name ?sha WHERE {{\n  GRAPH {graph_named} {{\n    VALUES ?type {{ <{wfns}Workflow> <{wfns}Archetype> <{wfns}Contract> }}\n    ?s a ?type ; <{wfns}name> ?name .\n    OPTIONAL {{ ?s <{wfns}scriptSha256> ?sha }}\n  }}\n}}"
    );

    let mut live = Live {
        graph: graph_id.to_string(),
        prefix: prefix.clone(),
        read_graph: read_graph.clone(),
        ..Live::default()
    };

    for row in run_query(app, graph_id, q_entities)? {
        let Some(uri) = row.get("s").map(|v| strip_uri(v)) else {
            continue;
        };
        let Some(type_uri) = row.get("type").map(|v| strip_uri(v)) else {
            continue;
        };
        let name = row
            .get("name")
            .map(|v| literal_value(v))
            .unwrap_or_default();
        let sha = row.get("sha").map(|v| literal_value(v));
        let doc_id = doc_id_from_uri(&uri);
        let row_entry = EntityRow {
            uri,
            doc_id,
            name: name.clone(),
            sha: sha.clone(),
        };
        if type_uri.ends_with("Workflow") {
            live.workflows.insert(name, row_entry);
        } else if type_uri.ends_with("Archetype") {
            live.archetypes.insert(
                name,
                EntityRow {
                    sha: None,
                    ..row_entry
                },
            );
        } else {
            live.contracts.insert(
                name,
                EntityRow {
                    sha: None,
                    ..row_entry
                },
            );
        }
    }

    // Query 2: Run runIds.
    let q_runs = format!(
        "SELECT ?rid WHERE {{\n  GRAPH {graph_named} {{\n    ?r a <{wfns}Run> ; <{wfns}runId> ?rid .\n  }}\n}}"
    );
    for row in run_query(app, graph_id, q_runs)? {
        if let Some(rid) = row.get("rid").map(|v| literal_value(v)) {
            live.runs.push(rid);
        }
    }

    // Query 3: AgentNodes keyed by workflow URI.
    let q_nodes = format!(
        "SELECT ?s ?label ?wfdoc WHERE {{\n  GRAPH {graph_named} {{\n    ?s a <{wfns}AgentNode> ;\n       <{wfns}label> ?label ;\n       <{wfns}partOfWorkflow> ?wfdoc .\n  }}\n}}"
    );
    for row in run_query(app, graph_id, q_nodes)? {
        let wfdoc = row.get("wfdoc").map(|v| strip_uri(v)).unwrap_or_default();
        let node_uri = row.get("s").map(|v| strip_uri(v)).unwrap_or_default();
        let label = row
            .get("label")
            .map(|v| literal_value(v))
            .unwrap_or_default();
        if let Some(doc_id) = doc_id_from_uri(&node_uri) {
            live.nodes_by_workflow
                .entry(wfdoc)
                .or_default()
                .insert(label, doc_id);
        }
    }

    Ok(live)
}

/// Return all workflow-managed triples in the graph as typed `(s, p, Term)`.
/// Port of `survey.current_wf_triples` (predicate-in-wf/prov OR subject-starts
/// `urn:sophia:wf` OR rdf:type-with-wf-object). Reads from the user:rdf graph.
pub(crate) fn current_wf_triples(
    app: &AppHandle,
    graph_id: &str,
    contract: &VocabularyContract,
) -> AppResult<Vec<(String, String, Term)>> {
    let read_graph = read_graph_iri(graph_id);
    let graph_named = format!("<{read_graph}>");
    let wfns = contract.primary_namespace();
    let prov_ns = contract
        .namespaces
        .get("prov")
        .map(String::as_str)
        .unwrap_or("");
    let rdf_type_uri = contract
        .namespaces
        .get("rdf")
        .map(|ns| format!("{ns}type"))
        .unwrap_or_default();

    let q = format!(
        "SELECT ?s ?p ?o WHERE {{\n  GRAPH {graph_named} {{\n    ?s ?p ?o .\n    FILTER(\n      STRSTARTS(STR(?p), \"{wfns}\")\n      || STRSTARTS(STR(?p), \"{prov_ns}\")\n      || STRSTARTS(STR(?s), \"urn:sophia:wf\")\n      || (?p = <{rdf_type_uri}> && STRSTARTS(STR(?o), \"{wfns}\"))\n    )\n  }}\n}}"
    );

    let mut triples = Vec::new();
    for row in run_query(app, graph_id, q)? {
        let s = row.get("s").map(|v| strip_uri(v)).unwrap_or_default();
        let p = row.get("p").map(|v| strip_uri(v)).unwrap_or_default();
        let o = row
            .get("o")
            .map(|v| parse_term(v))
            .unwrap_or_else(|| Term::Lit(Literal::new_simple_literal("")));
        triples.push((s, p, o));
    }
    Ok(triples)
}

/// Read the current `mem:` triples from the RESERVED memory projection graph —
/// the PER-OBSERVER perspective graph `…:projection:memory:agent:{observer}` (Variant
/// B), or the shared commons `…:projection:memory` for an empty observer — NOT the
/// user:rdf graph the wf survey reads. This is the memory analog of
/// [`current_wf_triples`]; the memory planner diffs the minted desired set against
/// these for zero-ops idempotency.
///
/// 🔴 SUPERSESSION-SURVEY RE-SCOPE (fix #1, the load-bearing change): the survey
/// MUST be keyed by the SAME observer the WRITE uses. Under the commons-only survey,
/// a per-observer write would diff against an EMPTY snapshot → every supersession
/// (including an agent superseding its OWN prior head across a respawn) silently
/// degrades to "not a live record — no demote". Reading the same per-observer graph
/// makes the demote pass see the witness's own live heads.
///
/// Returns every triple in this observer's projection graph whose subject sits at
/// OR under the graph root (records, sources, evidence, AND the graph-IRI-as-subject
/// PROV attribution) — the `GRAPH` clause already isolates this observer's quads;
/// the `STRSTARTS` (no trailing `:`, so it also matches the bare root subject) is
/// belt-and-suspenders.
pub(crate) fn current_memory_triples(
    app: &AppHandle,
    graph_id: &str,
    observer: &str,
) -> AppResult<Vec<(String, String, Term)>> {
    let mem_graph = crate::rdf_authority::memory_projection_graph_iri_for(graph_id, observer);
    let graph_named = format!("<{mem_graph}>");
    // Subjects live at or under the per-observer projection root; the no-trailing-
    // colon prefix matches both the bare root (the PROV attribution subject) and
    // every `{root}:...` subject.
    let subject_prefix = mem_graph.clone();
    let q = format!(
        "SELECT ?s ?p ?o WHERE {{\n  GRAPH {graph_named} {{\n    ?s ?p ?o .\n    FILTER(STRSTARTS(STR(?s), \"{subject_prefix}\"))\n  }}\n}}"
    );

    let mut triples = Vec::new();
    for row in run_query(app, graph_id, q)? {
        let s = row.get("s").map(|v| strip_uri(v)).unwrap_or_default();
        let p = row.get("p").map(|v| strip_uri(v)).unwrap_or_default();
        let o = row
            .get("o")
            .map(|v| parse_term(v))
            .unwrap_or_else(|| Term::Lit(Literal::new_simple_literal("")));
        triples.push((s, p, o));
    }
    Ok(triples)
}

/// Recorded managed-document provenance per doc id
/// (`{doc_id: {intentSha256, renderSha256, managedBy}}`). Port of
/// `survey.docs_provenance` — reads the `emp:` namespace from the user:rdf graph.
#[allow(dead_code)]
pub(crate) fn docs_provenance(
    app: &AppHandle,
    graph_id: &str,
    doc_ids: &[String],
) -> AppResult<BTreeMap<String, BTreeMap<String, String>>> {
    let read_graph = read_graph_iri(graph_id);
    let graph_named = format!("<{read_graph}>");
    let emp_ns = crate::emporium::terms::EMPORIUM_NS;
    let wanted: std::collections::BTreeSet<&str> = doc_ids.iter().map(String::as_str).collect();

    let q = format!(
        "SELECT ?doc ?p ?o WHERE {{\n  GRAPH {graph_named} {{\n    ?doc ?p ?o .\n    FILTER(STRSTARTS(STR(?p), \"{emp_ns}\"))\n  }}\n}}"
    );

    let mut result: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    for row in run_query(app, graph_id, q)? {
        let doc = row.get("doc").map(|v| strip_uri(v)).unwrap_or_default();
        let Some(doc_id) = doc_id_from_uri(&doc) else {
            continue;
        };
        if !wanted.contains(doc_id.as_str()) {
            continue;
        }
        let p = row.get("p").map(|v| strip_uri(v)).unwrap_or_default();
        let key = p.strip_prefix(emp_ns).unwrap_or(&p).to_string();
        let value = row.get("o").map(|v| literal_value(v)).unwrap_or_default();
        result.entry(doc_id).or_default().insert(key, value);
    }
    Ok(result)
}

/// Strip the `<…>` from a rendered URI binding; returns the raw string for
/// non-URI bindings unchanged.
fn strip_uri(s: &str) -> String {
    s.strip_prefix('<')
        .and_then(|x| x.strip_suffix('>'))
        .map(str::to_string)
        .unwrap_or_else(|| s.to_string())
}

/// Extract the lexical value from a rendered literal binding (`"x"`,
/// `"x"^^<dt>`, `"x"@en`) or pass a URI/raw string through.
fn literal_value(s: &str) -> String {
    match parse_term(s) {
        Term::Lit(l) => l.value().to_string(),
        Term::Uri(n) => n.as_str().to_string(),
        Term::Placeholder(name) => format!("{}{}", crate::emporium::terms::PLACEHOLDER_NS, name),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::emporium::terms::{canon_value, CanonValue};

    #[test]
    fn survey_read_graph_is_user_rdf_graph() {
        // The #1 invariant: the survey read graph string == user_rdf_graph_iri.
        // This pins read/write graph symmetry before any mint exists.
        let g = "lab";
        assert_eq!(
            user_rdf_graph_iri(g),
            "urn:mnemosyne:local:graph:lab:user:rdf"
        );
        // And the doc-URI prefix is the bare graph subject (NOT the user graph).
        assert_eq!(graph_subject(g), "urn:mnemosyne:local:graph:lab");
    }

    #[test]
    fn parse_term_round_trips_uri() {
        let t = parse_term("<urn:sophia:wf:demo>");
        assert!(matches!(t, Term::Uri(_)));
        assert_eq!(t.as_nt(), "<urn:sophia:wf:demo>");
    }

    #[test]
    fn parse_term_round_trips_typed_literal() {
        let t = parse_term("\"5\"^^<http://www.w3.org/2001/XMLSchema#integer>");
        // canon keys as NUM(5) — equal to a long(5).
        assert_eq!(
            canon_value(&t),
            canon_value(&crate::emporium::terms::term_for(
                &crate::emporium::terms::Value::Int(5),
                crate::emporium::contract::Datatype::long
            ))
        );
    }

    #[test]
    fn parse_term_simple_literal() {
        let t = parse_term("\"hello\"");
        assert!(matches!(canon_value(&t), CanonValue::Lit(_)));
        assert_eq!(t.as_nt(), "\"hello\"");
    }

    #[test]
    fn parse_term_placeholder() {
        let t = parse_term("<urn:wf-emit:placeholder:SCRIPT_BLOCK>");
        assert!(matches!(t, Term::Placeholder(_)));
        assert_eq!(t.as_nt(), "<urn:wf-emit:placeholder:SCRIPT_BLOCK>");
    }

    #[test]
    fn doc_id_extraction_matches_anchor() {
        assert_eq!(
            doc_id_from_uri("urn:mnemosyne:local:graph:lab:doc:wf-demo"),
            Some("wf-demo".to_string())
        );
        // A fragment after :doc: means no match (the [^#]+$ anchor).
        assert_eq!(
            doc_id_from_uri("urn:mnemosyne:local:graph:lab:doc:wf-demo#block-x"),
            None
        );
        assert_eq!(doc_id_from_uri("urn:sophia:wf:demo:phase:1"), None);
    }

    #[test]
    fn literal_value_extracts_lexical() {
        assert_eq!(literal_value("\"demo\""), "demo");
        assert_eq!(
            literal_value("\"abc123\"^^<http://www.w3.org/2001/XMLSchema#string>"),
            "abc123"
        );
        assert_eq!(literal_value("<urn:x>"), "urn:x");
    }
}
