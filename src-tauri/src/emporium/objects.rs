//! GENERIC MEANINGFUL-OBJECT CRUD — the object-level surface over any
//! registered (or chamber-proposed) vocab whose contract declares a
//! `projection:*` write target.
//!
//! The ingest spine already gives every generic vocab CREATE/UPDATE semantics
//! (subject-scoped upsert since S2); what was missing is the OBJECT face:
//! list a class, read one subject's span, update one subject with an
//! address-integrity check, and retract a subject — each riding the SAME
//! machinery the spine uses (two-tier contract resolution, subject_rule
//! minting, the per-graph write gate, the applied-plan journal), never a
//! parallel path.
//!
//! DELETE is policy-gated: a vocab that declares lifecycle `status_transitions`
//! (memory, chamber) supersedes rather than deletes — deleting testimony is the
//! one thing the observer-relative doctrine forbids — so DELETE answers 409
//! with that instruction. Product vocabs (bookmark-shaped) get true retraction,
//! journaled as a `mode:"retract"` applied record.

use std::collections::BTreeMap;

use oxigraph::sparql::{QueryResults, SparqlEvaluator};
use oxigraph::store::Store;
use serde_json::{json, Value};

use crate::app_runtime::AppHandle;
use crate::emporium::applied_journal::journal_applied_plan;
use crate::emporium::applier::{ApplyReport, StepReport};
use crate::emporium::chamber_ontology::resolve_ingest_contract;
use crate::emporium::contract::VocabularyContract;
use crate::emporium::memory_applier::{open_memory_store, run_memory_update};
use crate::emporium::planner::{Plan, Step};
use crate::emporium::schemas::{IngestPayload, IngestRequest};
use crate::emporium::spine::{apply_and_assert, gather_and_plan};
use crate::emporium::subject_rule::{
    mint_subject_from_rule, parse_subject_rule, TOKEN_GRAPH_SUBJECT,
};
use crate::emporium::terms::{render_updates, Triple};
use crate::emporium::write_gate::acquire_write_gate;
use crate::rdf::graph_subject;

/// Object-surface errors, carrying the HTTP-ish status the route layer maps.
#[derive(Debug)]
pub(crate) enum ObjectError {
    BadRequest(String),
    /// "This class or vocab is misconfigured." 404 on the wire, and NEVER
    /// coded — a client must not read it as an empty object. Constructed by
    /// `contract_and_sink` (unknown vocab) and `class_rdf_type`/
    /// `resolve_subject` (class not declared by the vocab).
    NotFound(String),
    /// "This object is genuinely absent from a well-formed class." The only
    /// NotFound-family variant that carries `object_not_found` (D-C23/E24).
    /// Constructed at exactly the three absence sites:
    /// `assert_subject_is_class`'s class-mismatch/retracted case,
    /// `read_object`'s empty span, and `retract`'s empty span.
    Absent(String),
    Conflict(String),
    Internal(String),
}

impl ObjectError {
    pub(crate) fn status(&self) -> u16 {
        match self {
            ObjectError::BadRequest(_) => 400,
            // Same status as `NotFound`, deliberately: the split is about
            // machine-readability, not about what the wire says happened.
            ObjectError::NotFound(_) | ObjectError::Absent(_) => 404,
            ObjectError::Conflict(_) => 409,
            ObjectError::Internal(_) => 500,
        }
    }
    pub(crate) fn message(&self) -> &str {
        match self {
            ObjectError::BadRequest(m)
            | ObjectError::NotFound(m)
            | ObjectError::Absent(m)
            | ObjectError::Conflict(m)
            | ObjectError::Internal(m) => m,
        }
    }
}

fn internal(e: impl std::fmt::Display) -> ObjectError {
    ObjectError::Internal(e.to_string())
}

/// Resolve the vocab contract (two-tier: embedded registry, then the graph's
/// chamber-proposed ontologies) and its projection sink IRI.
fn contract_and_sink(
    store: &Store,
    graph_id: &str,
    vocab: &str,
) -> Result<(VocabularyContract, String), ObjectError> {
    let contract = resolve_ingest_contract(store, graph_id, vocab)
        .map_err(|e| ObjectError::NotFound(format!("vocab '{vocab}': {e}")))?;
    let write_target = match contract.write_target.as_deref() {
        Some(t) if t.starts_with("projection:") => t.to_string(),
        other => {
            return Err(ObjectError::BadRequest(format!(
                "vocab '{vocab}' write_target {other:?} is not a 'projection:*' sink — the \
                 object surface serves projection-backed vocabs only"
            )))
        }
    };
    let sink = format!("{}:{}", graph_subject(graph_id), write_target);
    Ok((contract, sink))
}

/// The class-discriminating rdf:type (the primary-namespace one — the same
/// selection the apply partitioner and shacl_emit use). `pub(crate)` so the
/// query-face engine ([`crate::emporium::query_engine`]) shares this exact
/// resolution instead of re-deriving it.
pub(crate) fn class_rdf_type(
    contract: &VocabularyContract,
    class: &str,
) -> Result<String, ObjectError> {
    let spec = contract.classes.get(class).ok_or_else(|| {
        ObjectError::NotFound(format!(
            "class '{class}' is not declared by vocab '{}'",
            contract.name
        ))
    })?;
    let primary_ns = contract.primary_namespace();
    spec.rdf_types
        .iter()
        .filter_map(|t| contract.expand(t).ok())
        .find(|uri| uri.starts_with(&primary_ns))
        .ok_or_else(|| {
            ObjectError::BadRequest(format!(
                "class '{class}' has no primary-namespace rdf:type — it is never reconciled \
                 as a class span and has no object surface"
            ))
        })
}

/// Resolve an object address: a full subject IRI (contains `:`… i.e. looks like
/// a URI) is taken verbatim; otherwise it is a `localId` minted through the
/// class subject_rule with the standard `{graph_subject}`/`{localId}` tokens.
/// Rules needing MORE tokens must be addressed by full subject. `pub(crate)`
/// so [`crate::emporium::write`]'s generic write lane predicts a record's
/// minted subject BEFORE any apply, sharing this EXACT mint rather than
/// re-deriving it.
pub(crate) fn resolve_subject(
    contract: &VocabularyContract,
    graph_id: &str,
    class: &str,
    address: &str,
) -> Result<String, ObjectError> {
    if address.contains("://") || address.starts_with("urn:") {
        return Ok(address.to_string());
    }
    let spec = contract.classes.get(class).ok_or_else(|| {
        ObjectError::NotFound(format!(
            "class '{class}' is not declared by vocab '{}'",
            contract.name
        ))
    })?;
    let rule_str = spec.subject_rule.as_deref().ok_or_else(|| {
        ObjectError::BadRequest(format!(
            "class '{class}' has no subject_rule — address it by full subject IRI"
        ))
    })?;
    let rule = parse_subject_rule(rule_str).map_err(ObjectError::BadRequest)?;
    let mut tokens: BTreeMap<String, String> = BTreeMap::new();
    tokens.insert(TOKEN_GRAPH_SUBJECT.to_string(), graph_subject(graph_id));
    tokens.insert("localId".to_string(), address.to_string());
    mint_subject_from_rule(&rule, &tokens).map_err(|e| {
        ObjectError::BadRequest(format!(
            "cannot address '{address}' via subject_rule '{rule_str}': {e} — use the full \
             subject IRI"
        ))
    })
}

/// Confirm the subject carries the class's discriminating rdf:type in the sink.
/// A full-IRI address is otherwise unconstrained, so without this a read/update/
/// delete routed under class `A` could touch a subject that is actually a `B` in
/// the same multi-class projection graph. Returns NotFound (not a leak) when the
/// subject is absent or of the wrong class.
///
/// T-W Law 4: ALSO returns NotFound for a RETRACTED subject — "faces hide it,
/// the log keeps it". Every caller of this shared gate (`read_object`,
/// `update_object`, `delete_object`) inherits the same honest blindness a
/// retraction is supposed to produce; `pub(crate)` so [`crate::emporium::write`]
/// shares this exact check for its generic-lane pre-apply existence probe.
pub(crate) fn assert_subject_is_class(
    store: &Store,
    sink: &str,
    subject: &str,
    rdf_type: &str,
) -> Result<(), ObjectError> {
    let q = format!(
        "ASK {{ GRAPH <{sink}> {{ <{subject}> a <{rdf_type}> . \
         FILTER NOT EXISTS {{ <{subject}> <{}> ?emporiumRetractedAt }} }} }}",
        crate::emporium::terms::RETRACTED_AT_PRED,
    );
    let is = match SparqlEvaluator::new()
        .parse_query(&q)
        .map_err(internal)?
        .on_store(store)
        .execute()
        .map_err(internal)?
    {
        QueryResults::Boolean(b) => b,
        _ => return Err(internal("class check expected ASK boolean")),
    };
    if is {
        Ok(())
    } else {
        Err(ObjectError::Absent(format!(
            "no object <{subject}> of the requested class in {sink}"
        )))
    }
}

/// The `(p, o-as-nt)` rows of one subject in the sink (empty = no such object).
fn subject_span(
    store: &Store,
    sink: &str,
    subject: &str,
) -> Result<Vec<(String, String)>, ObjectError> {
    let q =
        format!("SELECT ?p ?o WHERE {{ GRAPH <{sink}> {{ <{subject}> ?p ?o }} }} ORDER BY ?p ?o");
    let solutions = match SparqlEvaluator::new()
        .parse_query(&q)
        .map_err(internal)?
        .on_store(store)
        .execute()
        .map_err(internal)?
    {
        QueryResults::Solutions(s) => s,
        _ => return Err(internal("subject span expected SELECT solutions")),
    };
    let mut out = Vec::new();
    for sol in solutions {
        let sol = sol.map_err(internal)?;
        let p = match sol.get("p") {
            Some(oxigraph::model::Term::NamedNode(n)) => n.as_str().to_string(),
            _ => continue,
        };
        let o = sol.get("o").map(|t| t.to_string()).unwrap_or_default();
        out.push((p, o));
    }
    Ok(out)
}

/// LIST the subjects of one class in the vocab's sink, paginated.
pub(crate) fn list_objects(
    app: &AppHandle,
    graph_id: &str,
    vocab: &str,
    class: &str,
    limit: usize,
    offset: usize,
) -> Result<Value, ObjectError> {
    let store = open_memory_store(app, graph_id).map_err(internal)?;
    let (contract, sink) = contract_and_sink(&store, graph_id, vocab)?;
    let rdf_type = class_rdf_type(&contract, class)?;
    let limit = limit.clamp(1, 500);
    // T-W Law 4: a retracted subject is excluded — "faces hide it".
    let q = format!(
        "SELECT ?s WHERE {{ GRAPH <{sink}> {{ ?s a <{rdf_type}> . \
         FILTER NOT EXISTS {{ ?s <{}> ?emporiumRetractedAt }} }} }} ORDER BY ?s \
         LIMIT {limit} OFFSET {offset}",
        crate::emporium::terms::RETRACTED_AT_PRED,
    );
    let solutions = match SparqlEvaluator::new()
        .parse_query(&q)
        .map_err(internal)?
        .on_store(&store)
        .execute()
        .map_err(internal)?
    {
        QueryResults::Solutions(s) => s,
        _ => return Err(internal("list expected SELECT solutions")),
    };
    let mut subjects = Vec::new();
    for sol in solutions {
        let sol = sol.map_err(internal)?;
        if let Some(oxigraph::model::Term::NamedNode(n)) = sol.get("s") {
            subjects.push(n.as_str().to_string());
        }
    }
    Ok(json!({
        "graphId": graph_id,
        "vocab": contract.name,
        "class": class,
        "sink": sink,
        "limit": limit,
        "offset": offset,
        "subjects": subjects,
    }))
}

/// READ one object: its full predicate span, rendered as `{predicate: [values]}`
/// (N-Triples value forms — the honest, face-neutral rendering).
pub(crate) fn read_object(
    app: &AppHandle,
    graph_id: &str,
    vocab: &str,
    class: &str,
    address: &str,
) -> Result<Value, ObjectError> {
    let store = open_memory_store(app, graph_id).map_err(internal)?;
    let (contract, sink) = contract_and_sink(&store, graph_id, vocab)?;
    let rdf_type = class_rdf_type(&contract, class)?;
    let subject = resolve_subject(&contract, graph_id, class, address)?;
    assert_subject_is_class(&store, &sink, &subject, &rdf_type)?;
    let span = subject_span(&store, &sink, &subject)?;
    if span.is_empty() {
        return Err(ObjectError::Absent(format!(
            "no object <{subject}> in {sink}"
        )));
    }
    let mut predicates: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (p, o) in span {
        predicates.entry(p).or_default().push(o);
    }
    Ok(json!({
        "graphId": graph_id,
        "vocab": contract.name,
        "class": class,
        "subject": subject,
        "predicates": predicates,
    }))
}

/// CREATE (or upsert) a batch of records — sugar over the generic ingest
/// (subject-scoped by default, SHACL-gated, journaled, event-consistent), run
/// inside the per-graph write gate exactly like the ingest route.
///
/// DEPRECATED (T-W, 2026-07-06): [`crate::emporium::write::emporium_write`] is
/// the honest MCP write door over the SAME machinery (plus the five laws:
/// observer/membrane routing, the subject-in-record rejection, per-record
/// converged/applied outcomes, dry_run-as-validation-preview). This HTTP-only
/// function is NOT removed this wave (still routed at
/// `POST /emporium/objects/{graph_id}/{vocab}`) — new callers should prefer
/// `emporium_write`.
pub(crate) async fn create_objects(
    app: &AppHandle,
    graph_id: &str,
    vocab: &str,
    records: Value,
) -> Result<Value, ObjectError> {
    let request: IngestRequest = serde_json::from_value(json!({
        "vocab": vocab,
        "dry_run": false,
        "payload": { "kind": "generic", "records": records }
    }))
    .map_err(|e| ObjectError::BadRequest(format!("records are malformed: {e}")))?;
    let _gate = acquire_write_gate(graph_id).await;
    let planned = gather_and_plan(app, graph_id, &request)
        .map_err(|e| ObjectError::BadRequest(e.message_ref().to_string()))?;
    let subjects: Vec<String> = planned
        .plan
        .desired_inserts
        .iter()
        .map(|(s, _, _)| s.clone())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    let report = apply_and_assert(app, graph_id, &planned.plan, planned.contract, &request).await;
    if !report.ok {
        return Err(ObjectError::Conflict(format!(
            "apply halted: {}",
            report
                .steps
                .iter()
                .rev()
                .find_map(|s| s.extra.get("error").and_then(Value::as_str))
                .unwrap_or("see report")
        )));
    }
    Ok(json!({ "ok": true, "subjects": subjects, "warnings": report.warnings }))
}

/// UPDATE one object: a single record whose minted subject MUST equal the
/// addressed subject (the address-integrity check), then the same gated
/// subject-scoped apply as create.
///
/// DEPRECATED (T-W, 2026-07-06): [`crate::emporium::write::emporium_write`]
/// absorbs this — a re-write of the SAME `localId`/`client_ref` on a
/// materialize/current-state ("Replace"-strategy) class IS the in-place
/// reappearance emporium_write routes through `class_dispatch`, with no
/// separate "update" verb needed. NOT removed this wave (still routed at
/// `PUT /emporium/objects/{graph_id}/{vocab}/{class}/{address}`).
pub(crate) async fn update_object(
    app: &AppHandle,
    graph_id: &str,
    vocab: &str,
    class: &str,
    address: &str,
    record: Value,
) -> Result<Value, ObjectError> {
    let store = open_memory_store(app, graph_id).map_err(internal)?;
    let (contract, sink) = contract_and_sink(&store, graph_id, vocab)?;
    let subject = resolve_subject(&contract, graph_id, class, address)?;
    let request: IngestRequest = serde_json::from_value(json!({
        "vocab": vocab,
        "dry_run": false,
        "payload": { "kind": "generic", "records": [record] }
    }))
    .map_err(|e| ObjectError::BadRequest(format!("record is malformed: {e}")))?;

    let rdf_type = class_rdf_type(&contract, class)?;
    let _gate = acquire_write_gate(graph_id).await;
    // The object must already exist AND be of the addressed class (a full-IRI
    // address could otherwise name a sibling class's subject).
    assert_subject_is_class(&store, &sink, &subject, &rdf_type)?;
    let planned = gather_and_plan(app, graph_id, &request)
        .map_err(|e| ObjectError::BadRequest(e.message_ref().to_string()))?;
    // Address integrity: the record must mint EXACTLY the addressed subject.
    let minted: std::collections::BTreeSet<&str> = planned
        .plan
        .desired_inserts
        .iter()
        .map(|(s, _, _)| s.as_str())
        .collect();
    if !minted.contains(subject.as_str()) || minted.len() != 1 {
        return Err(ObjectError::BadRequest(format!(
            "the record does not address this object: minted {minted:?}, addressed <{subject}>"
        )));
    }
    let report = apply_and_assert(app, graph_id, &planned.plan, planned.contract, &request).await;
    if !report.ok {
        return Err(ObjectError::Conflict("apply halted".to_string()));
    }
    Ok(json!({ "ok": true, "subject": subject, "warnings": report.warnings }))
}

/// DELETE (retract) one object's span from the sink. Lifecycle vocabs are
/// refused with the supersession instruction; product vocabs get a true,
/// journaled retraction. Gated, direct-on-store (the lawful materializer path).
///
/// DEPRECATED (T-W, 2026-07-06): this is a HARD delete (the triples are gone —
/// `render_updates("DELETE DATA", …)` below), which the T-W ruling's Law 4
/// deliberately does NOT build for the new write path (hard-delete stays
/// DEFERRED). [`crate::emporium::write::emporium_retract`] is the honest door
/// for retraction going forward: it mints a retraction EVENT on the subject
/// (who/when/rationale) and leaves the triples in place for the log — prefer
/// it over this route for anything that is testimony, not scratch. NOT
/// removed or changed this wave (still routed at
/// `DELETE /emporium/objects/{graph_id}/{vocab}/{class}/{address}`); it now
/// additionally refuses (404, via [`assert_subject_is_class`]) a subject that
/// `emporium_retract` already marked retracted, since that check is shared.
pub(crate) async fn delete_object(
    app: &AppHandle,
    graph_id: &str,
    vocab: &str,
    class: &str,
    address: &str,
) -> Result<Value, ObjectError> {
    let store = open_memory_store(app, graph_id).map_err(internal)?;
    let (contract, sink) = contract_and_sink(&store, graph_id, vocab)?;
    // The policy gate, declaration-driven: a vocab with lifecycle transitions
    // manages history by supersession — deleting testimony is forbidden.
    if !contract.status_transitions.is_empty() {
        return Err(ObjectError::Conflict(format!(
            "vocab '{}' declares lifecycle status_transitions — objects are superseded, \
             never deleted (file a new record with supersedesRef; see the reconciliation \
             policy). DELETE is for product vocabs without lifecycle.",
            contract.name
        )));
    }
    let rdf_type = class_rdf_type(&contract, class)?;
    let subject = resolve_subject(&contract, graph_id, class, address)?;

    let _gate = acquire_write_gate(graph_id).await;
    // Retract only a subject that IS of the addressed class (a full-IRI address
    // could otherwise delete a sibling class's subject in the same sink).
    assert_subject_is_class(&store, &sink, &subject, &rdf_type)?;
    let span = subject_span(&store, &sink, &subject)?;
    if span.is_empty() {
        return Err(ObjectError::Absent(format!(
            "no object <{subject}> in {sink}"
        )));
    }
    // Render the retraction from the surveyed span (value-faithful: parse each
    // rendered object term back through the survey round-trip).
    let triples: Vec<Triple> = span
        .iter()
        .map(|(p, o)| {
            (
                subject.clone(),
                p.clone(),
                crate::emporium::survey::parse_term(o),
            )
        })
        .collect();
    let bodies = render_updates("DELETE DATA", &triples, 60);
    let steps: Vec<Step> = bodies
        .iter()
        .map(|update| Step::SparqlUpdate {
            update: update.clone(),
        })
        .collect();
    for body in &bodies {
        run_memory_update(&store, &sink, body).map_err(internal)?;
    }

    // Journal the retraction as an applied record (mode "retract"), so the
    // audit trail covers destructive ops with the same machinery as applies.
    let plan = Plan {
        graph: graph_id.to_string(),
        workflow: format!("retract:{class}:{address}"),
        vocab: contract.name.clone(),
        mode: "retract".to_string(),
        short_id: String::new(),
        workflow_doc_id: String::new(),
        steps,
        summary: crate::emporium::planner::PlanSummary {
            folders: 0,
            doc_writes: 0,
            moves: 0,
            wires_create: 0,
            wires_delete: 0,
            rdf_delete: triples.len(),
            rdf_insert: 0,
        },
        warnings: Vec::new(),
        desired_inserts: Vec::new(),
        observer: String::new(),
        planned_at_ms: 0,
    };
    let report = ApplyReport {
        graph: graph_id.to_string(),
        workflow: plan.workflow.clone(),
        mode: "retract".to_string(),
        summary: json!({"rdfDelete": triples.len()}),
        warnings: Vec::new(),
        steps: vec![StepReport {
            op: "retract_subject".to_string(),
            extra: serde_json::Map::from_iter([
                ("ok".to_string(), json!(true)),
                ("subject".to_string(), json!(subject)),
                ("rdfDelete".to_string(), json!(triples.len())),
            ]),
        }],
        ok: true,
        halted_at: None,
        script_block: None,
        assertion: None,
    };
    if let Err(error) = journal_applied_plan(app, graph_id, &plan, &contract, false, &report) {
        log::error!("retract journal lagged graph={graph_id} subject={subject} error={error}");
    }
    Ok(json!({
        "ok": true,
        "subject": subject,
        "retractedTriples": triples.len(),
    }))
}
