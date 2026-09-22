//! The VIOLATION LEDGER — a Meaningful Object recording SHACL validation
//! failures as observer-relative RDF testimony (EA-6).
//!
//! When a memory write is malformed but the per-graph [`ValidationPolicy`] is
//! `FlagAndAccept`, the write still LANDS (so the agent is not blocked) but the
//! violation is RECORDED here for periodic review. Each violation is a first-class
//! RDF resource — attributed to an observer (the validator witness), timestamped,
//! and carrying the SHACL report detail (shape, focus node, offending property +
//! value, message, severity).
//!
//! This is the founder's "testimony ABOUT testimony": the ledger is itself a
//! defeasible, observer-relative Meaningful Object, projected into a reserved
//! named graph (`:projection:violations`) and SERVED through the engine like any
//! other MO (queryable via the garden SPARQL tools / conneg faces). It is a
//! BORN-RDF sink written DIRECT-ON-STORE (the same lawful path the memory
//! materializer uses), bypassing the text-gated user:rdf authority service (which
//! correctly refuses `:projection:` targets), and is never a source of truth for
//! the write decision.
//!
//! RECOVERABILITY (S3). This header once called the ledger "derived, regenerable
//! from the validation events". That was FALSE — the validation events were not
//! retained anywhere, so before S3 the oxigraph store was the SOLE copy of these
//! violation testimonies. From S3 the recovery direction is the APPLIED-PLAN
//! JOURNAL: replaying a journaled memory plan through `spine::apply_memory_plan`
//! re-runs the SHACL gate, which re-appends the FlagAndAccept violations. HONEST
//! BOUND: the journaled plan omits its desired-insert triples (they are
//! `#[serde(skip)]` on `Plan`), so a replay validates an EMPTY desired set and
//! re-emits NOTHING — the ledger is NOT yet mechanically rebuildable. Until the
//! event log carries the validated triples (S9), treat this store as authoritative
//! for the violation sink and BACK IT UP; do not claim regenerability.
//!
//! Append is IDEMPOTENT: a violation's subject IRI is content-addressed over its
//! (graph, focusNode, shape, path, value, message, observer) tuple, so re-filing
//! the SAME violation on a re-attempted write inserts nothing new (the
//! INSERT-DATA into the named graph is a set union).

use oxigraph::sparql::SparqlEvaluator;
use oxigraph::store::Store;
use sha2::{Digest, Sha256};

use crate::emporium::contract::Datatype;
use crate::emporium::shacl_validator::ViolationRecord;
use crate::emporium::terms::{term_for, Value};
use crate::rdf_authority::violations_projection_graph_iri;

/// The violation-ledger vocabulary namespace. Parallel to the memory-core `mem:`
/// namespace — these are `mem:`-adjacent governance terms (the ledger is part of
/// the memory governance surface), kept under a distinct `…/memory/violation#`
/// path so a reviewer query can scope cleanly to the ledger.
pub(crate) const VLOG_NS: &str = "http://mnemosyne.dev/memory/violation#";

const RDF_TYPE: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";
const PROV_NS: &str = "http://www.w3.org/ns/prov#";

/// The default observer (witness) attributed to a recorded violation when the
/// caller does not name one: the in-cell SHACL validator. Observer-relative —
/// every recorded fact is SOMEONE's testimony; here it is the validator's.
pub(crate) const DEFAULT_OBSERVER: &str = "urn:sophia:observer:shacl-validator";

/// Content-address a violation into a stable subject IRI under the ledger graph
/// root. Idempotent: identical violations (same focus/shape/path/value/message/
/// observer in the same graph) collapse to the same subject, so re-recording is a
/// no-op set-union INSERT.
fn violation_subject(graph_id: &str, observer: &str, v: &ViolationRecord) -> String {
    let mut hasher = Sha256::new();
    hasher.update(graph_id.as_bytes());
    hasher.update(b"\x1f");
    hasher.update(observer.as_bytes());
    hasher.update(b"\x1f");
    hasher.update(v.focus_node.as_bytes());
    hasher.update(b"\x1f");
    hasher.update(v.shape.as_deref().unwrap_or("").as_bytes());
    hasher.update(b"\x1f");
    hasher.update(v.property_path.as_deref().unwrap_or("").as_bytes());
    hasher.update(b"\x1f");
    hasher.update(v.offending_value.as_deref().unwrap_or("").as_bytes());
    hasher.update(b"\x1f");
    hasher.update(v.message.as_bytes());
    let digest = format!("{:x}", hasher.finalize());
    format!(
        "{}:projection:violations:v:{}",
        crate::rdf::graph_subject(graph_id),
        &digest[..32]
    )
}

/// Build the N-Triples body (no GRAPH wrapper) for one violation resource. The
/// caller GRAPH-wraps it into `:projection:violations`. Every triple is a typed
/// term via [`term_for`] so the serialization is byte-faithful (same path the
/// memory materializer uses).
fn violation_triples_body(
    graph_id: &str,
    observer: &str,
    observed_at_ms: i64,
    v: &ViolationRecord,
) -> String {
    let subject = violation_subject(graph_id, observer, v);
    let mut lines: Vec<String> = Vec::new();

    let lit = |val: Value, dt: Datatype| term_for(&val, dt).as_nt();
    let uri = |s: &str| term_for(&Value::Uri(s.to_string()), Datatype::uri).as_nt();

    // rdf:type vlog:Violation + prov:Entity (it is a recorded entity = testimony).
    lines.push(format!("<{subject}> <{RDF_TYPE}> <{VLOG_NS}Violation>"));
    lines.push(format!("<{subject}> <{RDF_TYPE}> <{PROV_NS}Entity>"));
    // The observer (witness) — prov:wasAttributedTo (observer-relative).
    lines.push(format!(
        "<{subject}> <{PROV_NS}wasAttributedTo> {}",
        uri(observer)
    ));
    // observedAt — when the witness recorded this (epoch-ms → xsd:dateTime).
    lines.push(format!(
        "<{subject}> <{VLOG_NS}observedAt> {}",
        lit(Value::Int(observed_at_ms), Datatype::dateTime)
    ));
    // The graph this violation was observed in (scopes review queries).
    lines.push(format!(
        "<{subject}> <{VLOG_NS}graphId> {}",
        lit(Value::Str(graph_id.to_string()), Datatype::string)
    ));
    // The focus node (the malformed subject the agent tried to write).
    lines.push(format!(
        "<{subject}> <{VLOG_NS}focusNode> {}",
        lit(Value::Str(v.focus_node.clone()), Datatype::string)
    ));
    // message + severity — always present.
    lines.push(format!(
        "<{subject}> <{VLOG_NS}message> {}",
        lit(Value::Str(v.message.clone()), Datatype::string)
    ));
    lines.push(format!(
        "<{subject}> <{VLOG_NS}severity> {}",
        lit(Value::Str(v.severity.clone()), Datatype::string)
    ));
    // Optional structural detail.
    if let Some(shape) = &v.shape {
        lines.push(format!(
            "<{subject}> <{VLOG_NS}shape> {}",
            lit(Value::Str(shape.clone()), Datatype::string)
        ));
    }
    if let Some(path) = &v.property_path {
        lines.push(format!(
            "<{subject}> <{VLOG_NS}path> {}",
            lit(Value::Str(path.clone()), Datatype::string)
        ));
    }
    if let Some(value) = &v.offending_value {
        lines.push(format!(
            "<{subject}> <{VLOG_NS}offendingValue> {}",
            lit(Value::Str(value.clone()), Datatype::string)
        ));
    }

    lines.join(" .\n") + " ."
}

/// Append a batch of violations to the ledger projection graph for `graph_id`,
/// DIRECT-ON-STORE. Idempotent (content-addressed subjects + INSERT DATA set
/// union). Returns the count of violation RESOURCES written (one per record).
///
/// This is the FlagAndAccept sink: the memory write has already landed; this
/// records the testimony about it for periodic review. A failure to record is
/// returned to the caller (the apply fork decides whether that is loud) — the
/// ledger never silently drops a violation.
pub(crate) fn append_violations(
    store: &Store,
    graph_id: &str,
    observer: &str,
    observed_at_ms: i64,
    violations: &[ViolationRecord],
) -> Result<usize, String> {
    if violations.is_empty() {
        return Ok(0);
    }
    let ledger_graph = violations_projection_graph_iri(graph_id);
    let mut bodies: Vec<String> = Vec::new();
    for v in violations {
        bodies.push(violation_triples_body(
            graph_id,
            observer,
            observed_at_ms,
            v,
        ));
    }
    let update = format!(
        "INSERT DATA {{ GRAPH <{ledger_graph}> {{\n{}\n}} }}",
        bodies.join("\n")
    );
    SparqlEvaluator::new()
        .parse_update(&update)
        .map_err(|e| format!("parse violation-ledger update: {e}"))?
        .on_store(store)
        .execute()
        .map_err(|e| format!("execute violation-ledger update: {e}"))?;
    // DURABILITY AUDIT FINDING (2026-07-18): direct-on-store write, no narrow
    // mark. The `memory_applier::validate_memory_write` FlagAndAccept caller
    // runs under Emporium's write-gate (now itself a marking choke point —
    // see `write_gate::WriteGateGuard`), but `sweep::sweep_memory_conformance`
    // — reachable from the `emporium_sweep` MCP tool and the
    // `POST /emporium/sweep/{graph_id}` HTTP route — calls this with NO
    // covering lease/gate at all: a real, acknowledged ledger write that was
    // invisible to the durable flush. Mark here, in the one function every
    // caller funnels through, rather than at each caller (safe from the
    // `violations.is_empty()` early-return above: reaching here means a real
    // write just happened).
    crate::cell_durability::mark_rdf_store_written(store);
    Ok(violations.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxigraph::sparql::QueryResults;

    fn store() -> Store {
        Store::new().expect("in-memory store")
    }

    fn sample(focus: &str) -> ViolationRecord {
        ViolationRecord {
            focus_node: focus.to_string(),
            shape: Some("urn:sophia:shacl:sophia-memory-core#SourceReferenceShape".to_string()),
            property_path: Some("http://mnemosyne.dev/memory#sourceKind".to_string()),
            offending_value: None,
            message: "Less than 1 values".to_string(),
            severity: "Violation".to_string(),
        }
    }

    fn count(store: &Store, query: &str) -> usize {
        match SparqlEvaluator::new()
            .parse_query(query)
            .expect("parse")
            .on_store(store)
            .execute()
            .expect("execute")
        {
            QueryResults::Solutions(s) => s.count(),
            _ => panic!("expected solutions"),
        }
    }

    /// A recorded violation IS a queryable Meaningful Object: it lands in
    /// :projection:violations with its observer + observedAt + focus + message,
    /// and a review query reads it back.
    #[test]
    fn violation_is_a_queryable_meaningful_object() {
        let s = store();
        let graph = "lab";
        let g = violations_projection_graph_iri(graph);
        let n = append_violations(
            &s,
            graph,
            DEFAULT_OBSERVER,
            1_718_700_000_000,
            &[sample(
                "urn:mnemosyne:local:graph:lab:projection:memory:src:bad",
            )],
        )
        .expect("record one violation");
        assert_eq!(n, 1);

        // The review query: every vlog:Violation in the ledger, with its detail.
        let rows = count(
            &s,
            &format!(
                "SELECT ?v ?focus ?msg WHERE {{ GRAPH <{g}> {{ \
                 ?v a <{VLOG_NS}Violation> ; \
                    <{VLOG_NS}focusNode> ?focus ; \
                    <{VLOG_NS}message> ?msg }} }}"
            ),
        );
        assert_eq!(rows, 1, "the violation is queryable back as an MO");

        // Observer attribution is present (observer-relative testimony).
        let attributed = count(
            &s,
            &format!(
                "SELECT ?v WHERE {{ GRAPH <{g}> {{ \
                 ?v <{PROV_NS}wasAttributedTo> <{DEFAULT_OBSERVER}> }} }}"
            ),
        );
        assert_eq!(attributed, 1, "the violation is attributed to the observer");
    }

    /// Append is IDEMPOTENT: recording the SAME violation twice yields ONE
    /// resource (content-addressed subject + INSERT-DATA set union).
    #[test]
    fn appending_same_violation_twice_is_idempotent() {
        let s = store();
        let graph = "lab";
        let g = violations_projection_graph_iri(graph);
        let v = sample("urn:mnemosyne:local:graph:lab:projection:memory:src:dup");
        append_violations(&s, graph, DEFAULT_OBSERVER, 1_718_700_000_000, &[v.clone()]).unwrap();
        append_violations(&s, graph, DEFAULT_OBSERVER, 1_718_700_000_000, &[v]).unwrap();
        let resources = count(
            &s,
            &format!("SELECT ?v WHERE {{ GRAPH <{g}> {{ ?v a <{VLOG_NS}Violation> }} }}"),
        );
        assert_eq!(resources, 1, "the same violation collapses to one resource");
    }

    /// The ledger lands ONLY in :projection:violations — never the default graph
    /// nor any other (it is a reserved, isolated projection).
    #[test]
    fn ledger_is_isolated_to_its_projection_graph() {
        let s = store();
        let graph = "lab";
        let g = violations_projection_graph_iri(graph);
        append_violations(
            &s,
            graph,
            DEFAULT_OBSERVER,
            1_718_700_000_000,
            &[sample(
                "urn:mnemosyne:local:graph:lab:projection:memory:src:iso",
            )],
        )
        .unwrap();
        let elsewhere = count(
            &s,
            &format!("SELECT ?x WHERE {{ GRAPH ?g {{ ?x a <{VLOG_NS}Violation> }} FILTER(?g != <{g}>) }}"),
        );
        assert_eq!(
            elsewhere, 0,
            "ledger triples are isolated to :projection:violations"
        );
        let in_default = count(
            &s,
            &format!("SELECT ?x WHERE {{ ?x a <{VLOG_NS}Violation> }}"),
        );
        assert_eq!(in_default, 0, "nothing leaked to the default graph");
    }

    /// Distinct focus nodes record as distinct violation resources (no collision).
    #[test]
    fn distinct_violations_record_distinctly() {
        let s = store();
        let graph = "lab";
        let g = violations_projection_graph_iri(graph);
        append_violations(
            &s,
            graph,
            DEFAULT_OBSERVER,
            1_718_700_000_000,
            &[sample("urn:a"), sample("urn:b")],
        )
        .unwrap();
        let resources = count(
            &s,
            &format!("SELECT ?v WHERE {{ GRAPH <{g}> {{ ?v a <{VLOG_NS}Violation> }} }}"),
        );
        assert_eq!(resources, 2, "two distinct focus nodes → two resources");
    }
}
