//! T-W — the write path: `emporium_write` / `emporium_retract`, the honest
//! MCP doors over the existing spine (I1 gate, SHACL, minting, journal,
//! contested lineage). See `plans/emporium-what-it-wants-to-be-20260706.md`
//! §"T-W — The write path" for the five ratified laws this module implements
//! mechanically:
//!
//! 1. **Witnessed or explicitly commons** — a membrane-ed class's write lands
//!    in the writer's own membrane by default (`observer` given, `publish`
//!    absent); commons requires the explicit `publish:true` flag; an
//!    observer-less write to a membrane-ed class WITHOUT `publish:true` is a
//!    LOUD rejection. A class with no membrane semantics (everything outside
//!    the `sophia-memory-core` family, today) lands commons as it always did
//!    — `publish` there is a no-op ACCEPTED with a warning, not an error.
//! 2. **The client never mints identity** — every record is rejected if it
//!    carries a `subject` (or `vocab`) field; content-hash classes (the
//!    `MemoryRecord` shape) converge (`outcome: "converged"`) on a byte-
//!    identical resubmission; addressed (materialize/current-state) classes
//!    take a `localId`/`client_ref` and are minted through the EXISTING
//!    subject_rule grammar (never re-derived here).
//! 3. **Change is lineage, not mutation** — the memory lane's per-record
//!    `supersedes`/`contradicts` pass through to the spine's existing lineage
//!    machinery (`MemoryRecordIn::supersedes_ref`/`contradicts_ref`); the
//!    generic lane has NO lineage machinery, so those keys are a loud
//!    rejection there — a materialize class's only "reappearance" is a plain
//!    resubmission of the same `localId` (the EXISTING subject-scoped upsert
//!    `apply_simple_projection_plan` already performs; this is what the T-W
//!    memo calls "Replace-strategy classes").
//! 4. **Retraction is an event, not an erasure** — `emporium_retract` mints a
//!    retraction event (who/when/rationale, [`crate::emporium::terms::RETRACTED_AT_PRED`]
//!    et al.) directly on the subject, in whichever named graph it already
//!    lives in. Hard-delete is NOT built here (DEFERRED by ruling — see
//!    `objects::delete_object`, which already has one and is untouched).
//! 5. **The log is the authority** — `dry_run` runs the SAME structural/SHACL
//!    validation a real apply would (the memory lane's per-graph
//!    `ValidationPolicy`-tiered gate, minus its ledger side effect; the
//!    generic lane's flat validate-or-halt), returning violations and a
//!    predicted per-record outcome, with NOTHING written.
//!
//! Batches are single-vocab (structural: one `vocab` argument) and
//! ALL-OR-HALT: a SHACL/apply failure marks EVERY record's outcome
//! `"halted"` and carries the shared violations — never a partial write.

use oxigraph::model::{Literal, NamedNode};
use oxigraph::sparql::{QueryResults, SparqlEvaluator};
use oxigraph::store::Store;
use serde_json::{json, Value};

use crate::app_runtime::AppHandle;
use crate::emporium::applied_journal::read_applied_records;
use crate::emporium::chamber_ontology::resolve_ingest_contract;
use crate::emporium::class_dispatch::{self, DispatchRoute};
use crate::emporium::contract::{
    agent_memory_validation_vocabulary, memory_core_vocabulary, VocabularyContract,
};
use crate::emporium::memory_applier::{open_memory_store, run_memory_update};
use crate::emporium::objects::{resolve_subject, ObjectError};
use crate::emporium::planner::{memory_record_subject, record_observer, Plan};
use crate::emporium::query_engine::validate_sparql_iri;
use crate::emporium::schemas::{GenericRecordIn, IngestPayload, IngestRequest, MemoryRecordIn};
use crate::emporium::shacl_validator::{
    validate_desired_structured, validate_desired_structured_in_graph, ViolationRecord,
};
use crate::emporium::spine::{apply_and_assert, gather_and_plan};
use crate::emporium::terms::{
    iso_from_ms, render_updates, Term, Triple, RETRACTED_AT_PRED, RETRACTED_BY_PRED,
    RETRACTION_EVENT_PRED, RETRACTION_KIND_PRED, RETRACTION_RATIONALE_PRED,
};
use crate::emporium::write_gate::acquire_write_gate;
use crate::graph_record_store::read_graph_record;
use crate::rdf_authority::{memory_projection_graph_iri_for, observer_iri};
use crate::runtime_config::ValidationPolicy;

fn query_emit_err_to_object_err(error: crate::emporium::query_emit::QueryEmitError) -> ObjectError {
    use crate::emporium::query_emit::QueryEmitError as Q;
    match error {
        Q::BadRequest(m) => ObjectError::BadRequest(m),
        Q::NotFound(m) => ObjectError::NotFound(m),
        Q::Internal(m) => ObjectError::Internal(m),
    }
}

/// One record's outcome in [`emporium_write`]'s `results` array.
struct RecordOutcome {
    subject: String,
    outcome: &'static str,
    flags: Option<Value>,
}

/// A SHACL violation is BLOCKING iff its shape declared `sh:Violation`
/// severity — the SAME tier signal `memory_applier::is_blocking` reads,
/// duplicated here (deliberately: this is a READ-ONLY preview, and reusing
/// the real gate would also run its FlagAndAccept ledger side effect, which
/// `dry_run` must not pay).
fn is_blocking(v: &ViolationRecord) -> bool {
    v.severity == "Violation"
}

/// Reject a batch if ANY record carries a forbidden key — Law 2 (subject/
/// vocab smuggling) and, for the generic lane, Law 3 (no lineage machinery).
fn reject_forbidden_keys(records: &[Value], forbidden: &[&str]) -> Result<(), ObjectError> {
    for (i, r) in records.iter().enumerate() {
        let Some(obj) = r.as_object() else {
            return Err(ObjectError::BadRequest(format!(
                "records[{i}]: expected a JSON object"
            )));
        };
        for key in forbidden {
            if obj.contains_key(*key) {
                return Err(ObjectError::BadRequest(format!(
                    "records[{i}]: field '{key}' is not accepted by emporium_write — the client \
                     never mints identity or lineage out-of-band; use 'client_ref' for \
                     correlation, and (memory records only) 'supersedes'/'contradicts'"
                )));
            }
        }
    }
    Ok(())
}

/// `violations` is already-serialized JSON (each a [`ViolationRecord`]'s
/// `Serialize` output) — [`ViolationRecord`] is serialize-only (no
/// `Deserialize`), so a halted apply's structured violations (extracted
/// verbatim from the `ApplyReport`'s own JSON, mirroring
/// `geist_memory_service::violations_from_report`) and a dry_run preview's
/// freshly-built [`ViolationRecord`]s (converted once via
/// `serde_json::to_value`) share this ONE representation on the wire.
fn assemble_response(
    graph_id: &str,
    vocab_name: &str,
    dry_run: bool,
    outcomes: Vec<RecordOutcome>,
    violations: Vec<Value>,
    journal_ref: Option<String>,
    warnings: Vec<String>,
) -> Value {
    let ok = !outcomes.iter().any(|o| o.outcome == "halted");
    let results: Vec<Value> = outcomes
        .into_iter()
        .map(|o| {
            let mut v = json!({ "subject": o.subject, "outcome": o.outcome });
            if let Some(flags) = o.flags {
                v["flags"] = flags;
            }
            v
        })
        .collect();
    json!({
        "graphId": graph_id,
        "vocab": vocab_name,
        "dryRun": dry_run,
        "ok": ok,
        "results": results,
        "journalRef": journal_ref,
        "warnings": warnings,
        "violations": if violations.is_empty() {
            Value::Null
        } else {
            Value::Array(violations)
        },
    })
}

fn violations_to_json(violations: &[ViolationRecord]) -> Vec<Value> {
    violations
        .iter()
        .map(|v| serde_json::to_value(v).unwrap_or(Value::Null))
        .collect()
}

/// The most recently journaled applied-plan record's ref (`"{seq:012}-{appliedAt}"`),
/// read back AFTER a successful apply. The caller holds the per-graph write
/// gate across the whole call, so no concurrent writer can race this read.
fn latest_journal_ref(app: &AppHandle, graph_id: &str) -> Option<String> {
    read_applied_records(app, graph_id)
        .ok()
        .and_then(|records| records.into_iter().last())
        .map(|r| format!("{:012}-{}", r.seq, r.applied_at))
}

fn subject_exists_in_graph(store: &Store, graph_iri: &str, subject: &str) -> Result<bool, String> {
    // `subject` here is ALWAYS code-minted (memory_record_subject / the
    // subject_rule grammar), never spliced from raw caller input — no
    // `validate_sparql_iri` gate is needed for an internally-minted subject.
    let q = format!("ASK {{ GRAPH <{graph_iri}> {{ <{subject}> ?p ?o }} }}");
    match SparqlEvaluator::new()
        .parse_query(&q)
        .map_err(|e| e.to_string())?
        .on_store(store)
        .execute()
        .map_err(|e| e.to_string())?
    {
        QueryResults::Boolean(b) => Ok(b),
        _ => Err("subject_exists_in_graph: expected an ASK boolean".to_string()),
    }
}

// ---------------------------------------------------------------------------
// emporium_write
// ---------------------------------------------------------------------------

/// `emporium_write` — the MCP write surface over the existing spine (T-W item
/// 1). `vocab` names ONE registered (embedded or chamber-proposed) vocab;
/// `records` are flat JSON objects. Dispatches on the resolved contract's
/// `write_target`: `"projection:memory"` routes through the memory lane
/// (records use the SAME wire shape `remember`/`remember_batch` already
/// accept — content/scope/kind/contentOrientation/visibility/status/
/// sourceRefs/…, camelCase — plus per-record `supersedes`/`contradicts`);
/// any OTHER `"projection:*"` target routes through the generic lane
/// (records use `{kind: <ClassName>, ...predicate fields, localId |
/// clientRef}`, the SAME shape the existing generic ingest already accepts).
pub(crate) async fn emporium_write(
    app: &AppHandle,
    graph_id: &str,
    vocab: &str,
    records_raw: &[Value],
    dry_run: bool,
    publish: bool,
    top_observer: Option<&str>,
) -> Result<Value, ObjectError> {
    if records_raw.is_empty() {
        return Err(ObjectError::BadRequest(
            "emporium_write: records is empty".to_string(),
        ));
    }
    // Law 2, universal: the client never mints identity, and a batch is
    // single-vocab BY CONSTRUCTION (one `vocab` argument) — a per-record
    // 'vocab' key would smuggle a second vocab past that structural guarantee.
    reject_forbidden_keys(records_raw, &["subject", "vocab"])?;

    let store = open_memory_store(app, graph_id).map_err(ObjectError::Internal)?;
    let contract = resolve_ingest_contract(&store, graph_id, vocab)
        .map_err(|e| ObjectError::NotFound(format!("vocab '{vocab}': {e}")))?;
    let write_target = contract.write_target.clone().unwrap_or_default();
    if !write_target.starts_with("projection:") {
        return Err(ObjectError::BadRequest(format!(
            "vocab '{}' write_target {:?} is not a 'projection:*' sink — emporium_write \
             materializes into a projection sink only",
            contract.name, write_target
        )));
    }
    drop(store);

    if write_target == "projection:memory" {
        write_memory_lane(app, graph_id, records_raw, dry_run, publish, top_observer).await
    } else {
        write_generic_lane(
            app,
            graph_id,
            vocab,
            &contract,
            records_raw,
            dry_run,
            publish,
            top_observer,
        )
        .await
    }
}

/// Apply the Law 1 membrane gate to a homogeneous-observer memory batch:
/// resolve each record's effective observer (its own `observerAgentId`, or
/// the call's top-level `observer` default), then either force commons
/// (`publish:true` — the given observer, if any, is NOT used for placement)
/// or require every record to carry a NON-EMPTY observer (the default,
/// membrane-landing path) — an observer-less write without `publish:true` is
/// the loud rejection Law 1 mandates.
fn apply_membrane_gate(
    records: &mut [MemoryRecordIn],
    top_observer: Option<&str>,
    publish: bool,
    warnings: &mut Vec<String>,
) -> Result<(), ObjectError> {
    for r in records.iter_mut() {
        let has_own = r
            .observer_agent_id
            .as_deref()
            .map(str::trim)
            .is_some_and(|s| !s.is_empty());
        if !has_own {
            if let Some(top) = top_observer {
                if !top.trim().is_empty() {
                    r.observer_agent_id = Some(top.trim().to_string());
                }
            }
        }
    }
    if publish {
        for r in records.iter_mut() {
            if let Some(observer) = r
                .observer_agent_id
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                warnings.push(format!(
                    "publish=true routes this write to the shared commons; the given observer \
                     '{observer}' was not used for placement (it stays unset — a witnessed \
                     record is either in a membrane OR published, never both this wave)"
                ));
            }
            r.observer_agent_id = None;
        }
        return Ok(());
    }
    let any_observerless = records.iter().any(|r| {
        r.observer_agent_id
            .as_deref()
            .map(str::trim)
            .is_none_or(str::is_empty)
    });
    if any_observerless {
        return Err(ObjectError::BadRequest(
            "Law 1 (witnessed or explicitly commons): an observer-less write to a membrane-ed \
             class (sophia-memory-core) requires publish=true — pass 'observer' (top-level or \
             per-record 'observerAgentId') to land in your own membrane, or 'publish: true' to \
             publish to the shared commons. Nothing lands nowhere silently."
                .to_string(),
        ));
    }
    Ok(())
}

/// Remap the emporium_write-only aliases (`supersedes`/`contradicts`, a bare
/// `observer`) onto the exact camelCase keys [`MemoryRecordIn`]'s serde
/// expects, so a plain `serde_json::from_value` picks them up. Every OTHER
/// field keeps the existing `remember`/`remember_batch` camelCase wire
/// convention verbatim (content/scope/kind/contentOrientation/visibility/
/// status/sourceRefs/evidence/observedAt/validFrom/confidence/valence/
/// agentId/tags/clientRef/observerAgentId/supersedesRef/contradictsRef).
fn remap_memory_aliases(raw: &mut Value) {
    let Some(obj) = raw.as_object_mut() else {
        return;
    };
    // The bare alias is always CONSUMED (removed) — if the canonical key is
    // already present it wins and the alias is simply discarded, never left
    // behind as an inert extra key.
    if let Some(v) = obj.remove("supersedes") {
        obj.entry("supersedesRef".to_string()).or_insert(v);
    }
    if let Some(v) = obj.remove("contradicts") {
        obj.entry("contradictsRef".to_string()).or_insert(v);
    }
    if let Some(v) = obj.remove("observer") {
        obj.entry("observerAgentId".to_string()).or_insert(v);
    }
}

/// Read the per-graph [`ValidationPolicy`]-tiered SHACL preview WITHOUT the
/// real gate's ledger side effect (`validate_memory_write`'s FlagAndAccept
/// branch appends to `:projection:violations` — a write dry_run must never
/// pay). Mirrors `memory_applier::validate_memory_write`'s tiering exactly
/// (Off skips the engine; Halt blocks on any `sh:Violation`-severity finding;
/// FlagAndAccept never blocks) so a dry_run's "would this halt" answer agrees
/// with what a real apply would do.
fn preview_memory_validation(
    app: &AppHandle,
    graph_id: &str,
    plan: &Plan,
) -> Result<(Vec<ViolationRecord>, bool), ObjectError> {
    if plan.desired_inserts.is_empty() {
        return Ok((Vec::new(), false));
    }
    let policy = read_graph_record(app, graph_id)
        .map(|(_, record)| record.validation_policy)
        .unwrap_or_default();
    if policy == ValidationPolicy::Off {
        return Ok((Vec::new(), false));
    }
    let contract: &VocabularyContract = if plan.observer.is_empty() {
        memory_core_vocabulary()
    } else {
        agent_memory_validation_vocabulary()
    };
    let membrane_graph = plan.memory_graph_iri(graph_id);
    let violations = match validate_desired_structured_in_graph(
        &plan.desired_inserts,
        contract,
        &membrane_graph,
    ) {
        Ok(()) => Vec::new(),
        Err(v) => v,
    };
    let would_halt = policy == ValidationPolicy::Halt && violations.iter().any(is_blocking);
    Ok((violations, would_halt))
}

async fn write_memory_lane(
    app: &AppHandle,
    graph_id: &str,
    records_raw: &[Value],
    dry_run: bool,
    publish: bool,
    top_observer: Option<&str>,
) -> Result<Value, ObjectError> {
    let mut warnings: Vec<String> = Vec::new();
    let mut records: Vec<MemoryRecordIn> = Vec::with_capacity(records_raw.len());
    for (i, raw) in records_raw.iter().enumerate() {
        let mut raw = raw.clone();
        remap_memory_aliases(&mut raw);
        let record: MemoryRecordIn = serde_json::from_value(raw).map_err(|e| {
            ObjectError::BadRequest(format!("records[{i}]: malformed memory record: {e}"))
        })?;
        records.push(record);
    }

    apply_membrane_gate(&mut records, top_observer, publish, &mut warnings)?;

    // Predicted subjects BEFORE any store interaction — content-hash,
    // code-minted (Law 2), so this is deterministic regardless of outcome.
    let subjects: Vec<String> = records
        .iter()
        .map(|r| memory_record_subject(graph_id, r))
        .collect();
    let effective_observer = records
        .first()
        .map(|r| record_observer(r).to_string())
        .unwrap_or_default();
    let flags: Vec<Option<Value>> = records
        .iter()
        .map(|r| {
            let mut m = serde_json::Map::new();
            if let Some(s) = &r.supersedes_ref {
                m.insert("supersedes".to_string(), json!(s));
            }
            if let Some(c) = &r.contradicts_ref {
                m.insert("contradicts".to_string(), json!(c));
            }
            if m.is_empty() {
                None
            } else {
                Some(Value::Object(m))
            }
        })
        .collect();

    let request = IngestRequest {
        vocab: "memory".to_string(),
        dry_run,
        replace_class: false,
        payload: IngestPayload::Memory {
            records: records.clone(),
        },
    };
    request.validate().map_err(ObjectError::BadRequest)?;

    let _gate = acquire_write_gate(graph_id).await;

    let target_graph = memory_projection_graph_iri_for(graph_id, &effective_observer);
    let store = open_memory_store(app, graph_id).map_err(ObjectError::Internal)?;
    let mut pre_existed: Vec<bool> = Vec::with_capacity(subjects.len());
    for subject in &subjects {
        pre_existed.push(
            subject_exists_in_graph(&store, &target_graph, subject)
                .map_err(ObjectError::Internal)?,
        );
    }
    drop(store);

    let planned = gather_and_plan(app, graph_id, &request)
        .map_err(|e| ObjectError::BadRequest(e.message_ref().to_string()))?;

    if dry_run {
        let (violations, would_halt) = preview_memory_validation(app, graph_id, &planned.plan)?;
        let outcomes: Vec<RecordOutcome> = subjects
            .into_iter()
            .zip(pre_existed)
            .zip(flags)
            .map(|((subject, existed), flag)| RecordOutcome {
                subject,
                outcome: if would_halt {
                    "halted"
                } else if existed {
                    "converged"
                } else {
                    "applied"
                },
                flags: flag,
            })
            .collect();
        return Ok(assemble_response(
            graph_id,
            &planned.contract.name,
            true,
            outcomes,
            if would_halt {
                violations_to_json(&violations)
            } else {
                Vec::new()
            },
            None,
            warnings,
        ));
    }

    let report = apply_and_assert(app, graph_id, &planned.plan, planned.contract, &request).await;
    if !report.ok {
        let error = report
            .steps
            .iter()
            .rev()
            .find_map(|s| s.extra.get("error").and_then(Value::as_str))
            .unwrap_or("memory apply halted")
            .to_string();
        // The structured violations are carried VERBATIM (already-serialized
        // JSON, mirroring `geist_memory_service::violations_from_report`) —
        // `ViolationRecord` is serialize-only, so this is a raw JSON array
        // extraction, never a re-typed deserialize.
        let violations: Vec<Value> = report
            .steps
            .iter()
            .rev()
            .find_map(|s| s.extra.get("violations").and_then(Value::as_array).cloned())
            .unwrap_or_default();
        warnings.push(error);
        let outcomes: Vec<RecordOutcome> = subjects
            .into_iter()
            .zip(flags)
            .map(|(subject, flag)| RecordOutcome {
                subject,
                outcome: "halted",
                flags: flag,
            })
            .collect();
        return Ok(assemble_response(
            graph_id,
            &planned.contract.name,
            false,
            outcomes,
            violations,
            None,
            warnings,
        ));
    }

    let journal_ref = latest_journal_ref(app, graph_id);
    let outcomes: Vec<RecordOutcome> = subjects
        .into_iter()
        .zip(pre_existed)
        .zip(flags)
        .map(|((subject, existed), flag)| RecordOutcome {
            subject,
            outcome: if existed { "converged" } else { "applied" },
            flags: flag,
        })
        .collect();
    Ok(assemble_response(
        graph_id,
        &planned.contract.name,
        false,
        outcomes,
        Vec::new(),
        journal_ref,
        warnings,
    ))
}

#[allow(clippy::too_many_arguments)]
async fn write_generic_lane(
    app: &AppHandle,
    graph_id: &str,
    vocab: &str,
    contract: &VocabularyContract,
    records_raw: &[Value],
    dry_run: bool,
    publish: bool,
    top_observer: Option<&str>,
) -> Result<Value, ObjectError> {
    let mut warnings: Vec<String> = Vec::new();
    if publish {
        warnings.push(format!(
            "vocab '{}' has no membrane semantics (a single shared projection sink) — publish \
             is a no-op",
            contract.name
        ));
    }
    if let Some(observer) = top_observer.map(str::trim).filter(|s| !s.is_empty()) {
        warnings.push(format!(
            "vocab '{}' has no observer membrane — the given observer '{observer}' is ignored \
             (written to the shared sink only)",
            contract.name
        ));
    }

    // Law 3: no generic update semantics — a materialize/current-state class's
    // only reappearance is a plain resubmission of the same localId/client_ref
    // (the existing subject-scoped upsert). supersedes/contradicts have no
    // lineage machinery here, so their presence is a loud rejection, not a
    // silent no-op.
    reject_forbidden_keys(
        records_raw,
        &[
            "supersedes",
            "supersedesRef",
            "supersedes_ref",
            "contradicts",
            "contradictsRef",
            "contradicts_ref",
        ],
    )?;

    let mut generic_records: Vec<GenericRecordIn> = Vec::with_capacity(records_raw.len());
    let mut subjects: Vec<String> = Vec::with_capacity(records_raw.len());
    for (i, raw) in records_raw.iter().enumerate() {
        let mut raw = raw.clone();
        {
            let obj = raw.as_object_mut().ok_or_else(|| {
                ObjectError::BadRequest(format!("records[{i}]: expected a JSON object"))
            })?;
            if !obj.contains_key("localId") {
                if let Some(v) = obj.remove("clientRef").or_else(|| obj.remove("client_ref")) {
                    obj.insert("localId".to_string(), v);
                }
            }
        }
        let record: GenericRecordIn = serde_json::from_value(raw)
            .map_err(|e| ObjectError::BadRequest(format!("records[{i}]: malformed: {e}")))?;
        if record.kind.trim().is_empty() {
            return Err(ObjectError::BadRequest(format!(
                "records[{i}]: missing or empty 'kind'"
            )));
        }
        let dispatch = class_dispatch::resolve(contract, &record.kind)
            .map_err(|e| ObjectError::BadRequest(format!("records[{i}]: {e}")))?;
        if dispatch.route != DispatchRoute::SimpleProjection {
            return Err(ObjectError::BadRequest(format!(
                "records[{i}]: class '{}' resolves to {:?}, not a directly-writable \
                 materialize/current-state class — emporium_write serves \
                 'SimpleProjection'-routed classes only (a virtual/derived class is never \
                 written; a memory-family class is served by vocab='sophia-memory-core')",
                record.kind, dispatch.route
            )));
        }
        let local_id = record
            .fields
            .get("localId")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if local_id.trim().is_empty() {
            return Err(ObjectError::BadRequest(format!(
                "records[{i}]: missing 'localId'/'clientRef' — the subject_rule binding"
            )));
        }
        // Law 2 (the client never mints identity): `objects::resolve_subject`
        // ALSO accepts a full IRI verbatim (a deliberate affordance for the
        // object surface's read/update/delete addressing, which locates an
        // EXISTING subject). Minting a NEW record's identity from emporium_write
        // must always go through the class's declared subject_rule — a
        // caller-supplied 'localId'/'clientRef' that already looks like an
        // IRI (`scheme://…` or `urn:…`) would otherwise let the client choose
        // its own subject outright, and would silently desync this predicted
        // subject from the one `gather_and_plan`'s real mint produces below.
        if local_id.contains("://") || local_id.starts_with("urn:") {
            return Err(ObjectError::BadRequest(format!(
                "records[{i}]: 'localId'/'clientRef' ('{local_id}') looks like a full subject \
                 IRI — emporium_write mints identity through the class's subject_rule from a \
                 local token, it never accepts a client-chosen subject verbatim. Address an \
                 EXISTING object by full IRI via the read/update/delete object surface instead."
            )));
        }
        let subject = resolve_subject(contract, graph_id, &record.kind, local_id)?;
        subjects.push(subject);
        generic_records.push(record);
    }

    let request = IngestRequest {
        vocab: vocab.to_string(),
        dry_run,
        replace_class: false,
        payload: IngestPayload::Generic {
            records: generic_records,
        },
    };
    request.validate().map_err(ObjectError::BadRequest)?;

    let _gate = acquire_write_gate(graph_id).await;
    let planned = gather_and_plan(app, graph_id, &request)
        .map_err(|e| ObjectError::BadRequest(e.message_ref().to_string()))?;

    if dry_run {
        // Law 3's "Replace-strategy" classes have no convergence tracking of
        // their own (that is the content-hash memory lane's concept) — every
        // clean record previews as "applied".
        let violations = if planned.plan.desired_inserts.is_empty() {
            Vec::new()
        } else {
            match validate_desired_structured(&planned.plan.desired_inserts, contract) {
                Ok(()) => Vec::new(),
                Err(v) => v,
            }
        };
        let would_halt = !violations.is_empty();
        let outcomes: Vec<RecordOutcome> = subjects
            .into_iter()
            .map(|subject| RecordOutcome {
                subject,
                outcome: if would_halt { "halted" } else { "applied" },
                flags: None,
            })
            .collect();
        return Ok(assemble_response(
            graph_id,
            &contract.name,
            true,
            outcomes,
            violations_to_json(&violations),
            None,
            warnings,
        ));
    }

    let report = apply_and_assert(app, graph_id, &planned.plan, planned.contract, &request).await;
    if !report.ok {
        let error = report
            .steps
            .iter()
            .rev()
            .find_map(|s| s.extra.get("error").and_then(Value::as_str))
            .unwrap_or("apply halted")
            .to_string();
        warnings.push(error);
        let outcomes: Vec<RecordOutcome> = subjects
            .into_iter()
            .map(|subject| RecordOutcome {
                subject,
                outcome: "halted",
                flags: None,
            })
            .collect();
        return Ok(assemble_response(
            graph_id,
            &contract.name,
            false,
            outcomes,
            Vec::new(),
            None,
            warnings,
        ));
    }

    let journal_ref = latest_journal_ref(app, graph_id);
    let outcomes: Vec<RecordOutcome> = subjects
        .into_iter()
        .map(|subject| RecordOutcome {
            subject,
            outcome: "applied",
            flags: None,
        })
        .collect();
    Ok(assemble_response(
        graph_id,
        &contract.name,
        false,
        outcomes,
        Vec::new(),
        journal_ref,
        warnings,
    ))
}

// ---------------------------------------------------------------------------
// emporium_retract
// ---------------------------------------------------------------------------

fn locate_subject_graphs(store: &Store, subject: &str) -> Result<Vec<String>, String> {
    let q = format!("SELECT DISTINCT ?g WHERE {{ GRAPH ?g {{ <{subject}> ?p ?o }} }} ORDER BY ?g");
    let solutions = match SparqlEvaluator::new()
        .parse_query(&q)
        .map_err(|e| e.to_string())?
        .on_store(store)
        .execute()
        .map_err(|e| e.to_string())?
    {
        QueryResults::Solutions(s) => s,
        _ => return Err("locate_subject_graphs: expected SELECT solutions".to_string()),
    };
    let mut out = Vec::new();
    for sol in solutions {
        let sol = sol.map_err(|e| e.to_string())?;
        if let Some(oxigraph::model::Term::NamedNode(n)) = sol.get("g") {
            out.push(n.as_str().to_string());
        }
    }
    Ok(out)
}

fn subject_has_predicate(
    store: &Store,
    graph_iri: &str,
    subject: &str,
    predicate: &str,
) -> Result<bool, String> {
    let q = format!("ASK {{ GRAPH <{graph_iri}> {{ <{subject}> <{predicate}> ?x }} }}");
    match SparqlEvaluator::new()
        .parse_query(&q)
        .map_err(|e| e.to_string())?
        .on_store(store)
        .execute()
        .map_err(|e| e.to_string())?
    {
        QueryResults::Boolean(b) => Ok(b),
        _ => Err("subject_has_predicate: expected an ASK boolean".to_string()),
    }
}

/// `emporium_retract` — Law 4: mints a retraction EVENT on `subject` (who/
/// when/rationale), direct-on-store, in whichever named graph the subject
/// already lives in (located by a store-wide `GRAPH ?g` scan — this tool
/// takes no `vocab`/`class`, so the graph cannot be assumed from a naming
/// convention). Heads exclude it (`sweep::current_heads_by_lineage` /
/// `heads_as_of`, and therefore `emporium_heads` / `sparql_query_named`'s
/// `currentHeads` / `emporium_query`'s contested routing); the generic object
/// faces (`emporium_list` / `emporium_read` / `emporium_query`) hide it too.
/// The log keeps it: NO triple is removed (hard-delete stays deferred —
/// `objects::delete_object` is the separate, untouched admin-tier act).
pub(crate) async fn emporium_retract(
    app: &AppHandle,
    graph_id: &str,
    subject: &str,
    rationale: &str,
    kind: &str,
    observer: Option<&str>,
) -> Result<Value, ObjectError> {
    emporium_retract_with_identity(
        app, graph_id, subject, rationale, kind, observer, None, None,
    )
    .await
}

/// Identity-bearing retraction authority.  `event_id` + `at_ms` are supplied
/// by source-sync clients before they claim local durability. Replaying an
/// accepted event therefore inserts an identical RDF set and returns the same
/// `retractionRef`, even after a lost acknowledgement or process restart.
pub(crate) async fn emporium_retract_with_identity(
    app: &AppHandle,
    graph_id: &str,
    subject: &str,
    rationale: &str,
    kind: &str,
    observer: Option<&str>,
    event_id: Option<&str>,
    at_ms: Option<i64>,
) -> Result<Value, ObjectError> {
    validate_sparql_iri("subject", subject).map_err(query_emit_err_to_object_err)?;
    crate::pdf_source::require_not_authored_subject(graph_id, subject)
        .map_err(ObjectError::BadRequest)?;
    if rationale.trim().is_empty() {
        return Err(ObjectError::BadRequest(
            "emporium_retract: rationale is required (Law 4: who/when/rationale)".to_string(),
        ));
    }
    if !matches!(kind, "retract" | "archive") {
        return Err(ObjectError::BadRequest(format!(
            "emporium_retract: kind '{kind}' must be 'retract' or 'archive'"
        )));
    }

    let _gate = acquire_write_gate(graph_id).await;
    let store = open_memory_store(app, graph_id).map_err(ObjectError::Internal)?;

    let graphs = locate_subject_graphs(&store, subject).map_err(ObjectError::Internal)?;
    if graphs.is_empty() {
        return Err(ObjectError::NotFound(format!(
            "no object <{subject}> found in graph '{graph_id}'"
        )));
    }
    if graphs.len() > 1 {
        return Err(ObjectError::BadRequest(format!(
            "<{subject}> appears in {} named graphs {graphs:?} — emporium_retract cannot \
             disambiguate which one to retract in",
            graphs.len()
        )));
    }
    let target_graph = graphs.into_iter().next().expect("checked non-empty above");
    if target_graph == crate::pdf_source::sink(graph_id) {
        return Err(ObjectError::BadRequest("PDF source projection is read-only; edit its document".into()));
    }

    let already_retracted =
        subject_has_predicate(&store, &target_graph, subject, RETRACTED_AT_PRED)
            .map_err(ObjectError::Internal)?;

    let now_ms = at_ms.unwrap_or_else(|| chrono::Utc::now().timestamp_millis());
    if now_ms < 0 {
        return Err(ObjectError::BadRequest(
            "emporium_retract: atMs must be non-negative".to_string(),
        ));
    }
    let retraction_ref = event_id
        .map(|event_id| format!("urn:sophia:retraction:{event_id}"))
        .unwrap_or_else(|| format!("{subject}#retracted-{now_ms}"));
    let retraction_node = NamedNode::new(&retraction_ref)
        .map_err(|error| ObjectError::BadRequest(format!("retraction event id: {error}")))?;
    let iso = iso_from_ms(now_ms);
    let xsd_date_time = NamedNode::new("http://www.w3.org/2001/XMLSchema#dateTime")
        .expect("xsd:dateTime is a valid IRI");

    let mut triples: Vec<Triple> = vec![
        (
            subject.to_string(),
            RETRACTED_AT_PRED.to_string(),
            Term::Lit(Literal::new_typed_literal(iso.clone(), xsd_date_time)),
        ),
        (
            subject.to_string(),
            RETRACTION_RATIONALE_PRED.to_string(),
            Term::Lit(Literal::new_simple_literal(rationale)),
        ),
        (
            subject.to_string(),
            RETRACTION_KIND_PRED.to_string(),
            Term::Lit(Literal::new_simple_literal(kind)),
        ),
        (
            subject.to_string(),
            RETRACTION_EVENT_PRED.to_string(),
            Term::Uri(retraction_node),
        ),
    ];
    if let Some(observer_iri) = observer.and_then(observer_iri) {
        let observer_node = NamedNode::new(&observer_iri)
            .map_err(|e| ObjectError::Internal(format!("observer IRI: {e}")))?;
        triples.push((
            subject.to_string(),
            RETRACTED_BY_PRED.to_string(),
            Term::Uri(observer_node),
        ));
    }

    let bodies = render_updates("INSERT DATA", &triples, 60);
    for body in &bodies {
        run_memory_update(&store, &target_graph, body).map_err(ObjectError::Internal)?;
    }

    let mut warnings = Vec::new();
    if already_retracted && event_id.is_none() {
        warnings.push(format!(
            "<{subject}> was already retracted — this call recorded an ADDITIONAL retraction \
             event (the log is append-only; it does not overwrite the earlier one)"
        ));
    } else if already_retracted {
        warnings.push(format!(
            "<{subject}> was already retracted — stable event {retraction_ref} converged"
        ));
    }

    Ok(json!({
        "graphId": graph_id,
        "subject": subject,
        "graph": target_graph,
        "kind": kind,
        "rationale": rationale,
        "retractedAt": iso,
        "retractionRef": retraction_ref,
        "alreadyRetracted": already_retracted,
        "warnings": warnings,
    }))
}

#[cfg(test)]
mod unit_tests {
    use super::*;

    #[test]
    fn reject_forbidden_keys_names_the_offending_record_index() {
        let records = vec![
            json!({ "kind": "Bookmark", "localId": "a" }),
            json!({ "kind": "Bookmark", "localId": "b", "subject": "urn:smuggled" }),
        ];
        let err = reject_forbidden_keys(&records, &["subject", "vocab"]).unwrap_err();
        assert!(err.message().contains("records[1]"), "{}", err.message());
        assert!(err.message().contains("subject"), "{}", err.message());
    }

    #[test]
    fn remap_memory_aliases_prefers_an_already_canonical_key() {
        let mut raw = json!({ "supersedes": "urn:short", "supersedesRef": "urn:canonical" });
        remap_memory_aliases(&mut raw);
        assert_eq!(raw["supersedesRef"], json!("urn:canonical"));
        // the bare alias is consumed either way (never left to confuse a
        // downstream deny-unknown-fields gate).
        assert!(raw.get("supersedes").is_none());
    }

    #[test]
    fn remap_memory_aliases_promotes_the_bare_alias_when_canonical_is_absent() {
        let mut raw = json!({ "contradicts": "urn:disputed", "observer": "agent-abc" });
        remap_memory_aliases(&mut raw);
        assert_eq!(raw["contradictsRef"], json!("urn:disputed"));
        assert_eq!(raw["observerAgentId"], json!("agent-abc"));
    }

    #[test]
    fn membrane_gate_rejects_an_observer_less_batch_without_publish() {
        let mut records = vec![MemoryRecordIn {
            client_ref: None,
            scope: "agent".into(),
            kind: "ClaimMemory".into(),
            content_orientation: "knowledge".into(),
            visibility: "private".into(),
            status: "active".into(),
            content: "x".into(),
            source_refs: vec![],
            evidence: vec![],
            observed_at: None,
            valid_from: None,
            is_current: None,
            confidence: None,
            valence: None,
            agent_id: None,
            observer_agent_id: None,
            tags: vec![],
            supersedes_ref: None,
            contradicts_ref: None,
        }];
        let mut warnings = Vec::new();
        let err = apply_membrane_gate(&mut records, None, false, &mut warnings).unwrap_err();
        assert!(err.message().contains("Law 1"), "{}", err.message());
    }

    #[test]
    fn membrane_gate_forces_commons_and_warns_when_publish_overrides_a_given_observer() {
        let mut records = vec![MemoryRecordIn {
            client_ref: None,
            scope: "agent".into(),
            kind: "ClaimMemory".into(),
            content_orientation: "knowledge".into(),
            visibility: "private".into(),
            status: "active".into(),
            content: "x".into(),
            source_refs: vec![],
            evidence: vec![],
            observed_at: None,
            valid_from: None,
            is_current: None,
            confidence: None,
            valence: None,
            agent_id: None,
            observer_agent_id: Some("agent-vera".into()),
            tags: vec![],
            supersedes_ref: None,
            contradicts_ref: None,
        }];
        let mut warnings = Vec::new();
        apply_membrane_gate(&mut records, None, true, &mut warnings).expect("publish proceeds");
        assert!(records[0].observer_agent_id.is_none());
        assert!(
            warnings.iter().any(|w| w.contains("agent-vera")),
            "{warnings:?}"
        );
    }

    #[test]
    fn membrane_gate_lands_in_the_default_observer_without_publish() {
        let mut records = vec![MemoryRecordIn {
            client_ref: None,
            scope: "agent".into(),
            kind: "ClaimMemory".into(),
            content_orientation: "knowledge".into(),
            visibility: "private".into(),
            status: "active".into(),
            content: "x".into(),
            source_refs: vec![],
            evidence: vec![],
            observed_at: None,
            valid_from: None,
            is_current: None,
            confidence: None,
            valence: None,
            agent_id: None,
            observer_agent_id: None,
            tags: vec![],
            supersedes_ref: None,
            contradicts_ref: None,
        }];
        let mut warnings = Vec::new();
        apply_membrane_gate(&mut records, Some("agent-vera"), false, &mut warnings)
            .expect("observer given, publish absent — lands in the membrane");
        assert_eq!(records[0].observer_agent_id.as_deref(), Some("agent-vera"));
        assert!(warnings.is_empty());
    }
}

#[cfg(all(test, feature = "headless"))]
mod headless_tests {
    use super::*;
    use crate::emporium::sweep::current_heads_flagged;
    use crate::graph_service::{create_graph_service, CreateGraphInput};
    use std::path::PathBuf;
    use std::sync::Mutex;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn env_serial() -> &'static Mutex<()> {
        crate::tauri_runtime::profile_env_serial()
    }

    fn temp_profile(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        std::env::temp_dir().join(format!("garden-emporium-write-{name}-{nanos}"))
    }

    fn mock_app() -> AppHandle {
        crate::tauri_runtime::build_mock_app_for_tests(true)
    }

    fn seed_graph(app: &AppHandle, graph_id: &str) {
        create_graph_service(
            app,
            CreateGraphInput {
                graph_id: Some(graph_id.to_string()),
                title: "Emporium Write Lab".to_string(),
                description: None,
                operation_id: None,
            },
        )
        .expect("create graph");
    }

    fn run_isolated(name: &str, body: impl FnOnce() + std::panic::UnwindSafe) {
        let _serial = env_serial().lock().unwrap_or_else(|p| p.into_inner());
        let profile = temp_profile(name);
        std::env::set_var("GARDEN_PROFILE_DIR", &profile);
        let result = std::panic::catch_unwind(body);
        std::env::remove_var("GARDEN_PROFILE_DIR");
        let _ = std::fs::remove_dir_all(&profile);
        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
    }

    fn memory_record(client_ref: &str, content: &str) -> Value {
        json!({
            "clientRef": client_ref,
            "scope": "agent",
            "kind": "ClaimMemory",
            "contentOrientation": "knowledge",
            "visibility": "private",
            "status": "active",
            "content": content,
            "sourceRefs": [{ "sourceKind": "DocumentBlock", "blockId": "b1", "documentId": "doc-1" }],
            "isCurrent": true,
            "validFrom": 1_720_000_000_000i64,
        })
    }

    /// Law 1: an observer-less write to the membrane-ed memory vocab, no
    /// `publish`, is a LOUD rejection — nothing lands nowhere silently.
    #[test]
    fn observer_less_memory_write_without_publish_is_rejected() {
        run_isolated("observer-less", || {
            let app = mock_app();
            let graph_id = "ew-observerless";
            seed_graph(&app, graph_id);
            let err = crate::app_runtime::async_runtime::block_on(emporium_write(
                &app,
                graph_id,
                "sophia-memory-core",
                &[memory_record("r1", "vera prefers fish CLI")],
                false,
                false,
                None,
            ))
            .expect_err("observer-less write without publish must reject");
            assert!(err.message().contains("Law 1"), "{}", err.message());
        });
    }

    /// Law 1: WITH an observer and no publish, the record lands in the
    /// writer's OWN membrane (not commons) — proven by a REAL read: the
    /// commons `emporium_heads` sees nothing; the observer's own
    /// `emporium_heads` sees the head.
    #[test]
    fn membrane_default_lands_in_the_writers_own_membrane_not_commons() {
        run_isolated("membrane-default", || {
            let app = mock_app();
            let graph_id = "ew-membrane-default";
            seed_graph(&app, graph_id);
            let outcome = crate::app_runtime::async_runtime::block_on(emporium_write(
                &app,
                graph_id,
                "sophia-memory-core",
                &[memory_record("r1", "vera prefers fish CLI")],
                false,
                false,
                Some("agent-vera"),
            ))
            .expect("membrane write applies");
            assert_eq!(outcome["ok"], json!(true), "{outcome}");
            assert_eq!(
                outcome["results"][0]["outcome"],
                json!("applied"),
                "{outcome}"
            );
            assert!(
                outcome["journalRef"].is_string(),
                "a real apply journals: {outcome}"
            );

            let commons = current_heads_flagged(&app, graph_id, "").expect("commons heads read");
            assert!(
                commons.lineages.is_empty(),
                "commons must NOT see a membrane-landed record: {commons:?}"
            );
            let membrane =
                current_heads_flagged(&app, graph_id, "agent-vera").expect("observer heads read");
            assert_eq!(
                membrane.lineages.len(),
                1,
                "the writer's own membrane sees the head: {membrane:?}"
            );
        });
    }

    /// Law 1: `publish: true` routes to the shared COMMONS even though an
    /// observer was given — proven by the SAME real-read pair, inverted.
    #[test]
    fn publish_true_routes_to_commons_even_with_an_observer_given() {
        run_isolated("publish-commons", || {
            let app = mock_app();
            let graph_id = "ew-publish-commons";
            seed_graph(&app, graph_id);
            let outcome = crate::app_runtime::async_runtime::block_on(emporium_write(
                &app,
                graph_id,
                "sophia-memory-core",
                &[memory_record("r1", "vera prefers fish CLI")],
                false,
                true,
                Some("agent-vera"),
            ))
            .expect("publish write applies");
            assert_eq!(outcome["ok"], json!(true), "{outcome}");
            assert!(
                outcome["warnings"].as_array().is_some_and(|w| w
                    .iter()
                    .any(|x| x.as_str().unwrap_or("").contains("agent-vera"))),
                "publish overriding a given observer is flagged, not silent: {outcome}"
            );

            let commons = current_heads_flagged(&app, graph_id, "").expect("commons heads read");
            assert_eq!(
                commons.lineages.len(),
                1,
                "publish=true lands the record in the shared commons: {commons:?}"
            );
            let membrane =
                current_heads_flagged(&app, graph_id, "agent-vera").expect("observer heads read");
            assert!(
                membrane.lineages.is_empty(),
                "the given observer's OWN membrane must stay empty under publish=true: \
                 {membrane:?}"
            );
        });
    }

    /// Law 2: a record carrying a `subject` field is rejected loudly, whole
    /// batch, before any write — proven by re-reading the commons afterward
    /// and finding nothing landed.
    #[test]
    fn subject_in_record_is_rejected_and_nothing_lands() {
        run_isolated("subject-smuggle", || {
            let app = mock_app();
            let graph_id = "ew-subject-smuggle";
            seed_graph(&app, graph_id);
            let mut record = memory_record("r1", "vera prefers fish CLI");
            record["publish"] = json!(true);
            record.as_object_mut().unwrap().insert(
                "subject".to_string(),
                json!("urn:mnemosyne:local:graph:ew-subject-smuggle:projection:memory:record:smuggled"),
            );
            let err = crate::app_runtime::async_runtime::block_on(emporium_write(
                &app,
                graph_id,
                "sophia-memory-core",
                &[record],
                false,
                true,
                None,
            ))
            .expect_err("a subject field must reject the whole batch");
            assert!(err.message().contains("subject"), "{}", err.message());

            let commons = current_heads_flagged(&app, graph_id, "").expect("commons heads read");
            assert!(
                commons.lineages.is_empty(),
                "the rejected batch must leave NOTHING written: {commons:?}"
            );
        });
    }

    /// Law 2: content-hash convergence — resubmitting BYTE-IDENTICAL content
    /// (same observer) mints the SAME subject and reports `outcome:
    /// "converged"`, never a duplicate write.
    #[test]
    fn identical_content_resubmission_converges() {
        run_isolated("converge", || {
            let app = mock_app();
            let graph_id = "ew-converge";
            seed_graph(&app, graph_id);
            let first = crate::app_runtime::async_runtime::block_on(emporium_write(
                &app,
                graph_id,
                "sophia-memory-core",
                &[memory_record("r1", "vera prefers fish CLI")],
                false,
                true,
                None,
            ))
            .expect("first write applies");
            assert_eq!(first["results"][0]["outcome"], json!("applied"), "{first}");
            let subject = first["results"][0]["subject"].clone();

            let second = crate::app_runtime::async_runtime::block_on(emporium_write(
                &app,
                graph_id,
                "sophia-memory-core",
                &[memory_record("r2", "vera prefers fish CLI")],
                false,
                true,
                None,
            ))
            .expect("second (identical) write converges");
            assert_eq!(
                second["results"][0]["outcome"],
                json!("converged"),
                "{second}"
            );
            assert_eq!(
                second["results"][0]["subject"], subject,
                "identical content mints the SAME subject: {second}"
            );
        });
    }

    /// Law 3: supersedes lineage is visible through the T2 query face — write
    /// H, then supersede it; `sparql_query_named`'s `currentHeads` shows the
    /// NEW head, and `lineageOf` shows the OLD head in the same lineage.
    #[test]
    fn supersedes_lineage_is_visible_through_the_query_face() {
        run_isolated("supersedes-lineage", || {
            let app = mock_app();
            let graph_id = "ew-supersedes";
            seed_graph(&app, graph_id);
            let h = crate::app_runtime::async_runtime::block_on(emporium_write(
                &app,
                graph_id,
                "sophia-memory-core",
                &[memory_record("r-h", "vera prefers zsh")],
                false,
                true,
                None,
            ))
            .expect("H applies");
            let h_subject = h["results"][0]["subject"].as_str().unwrap().to_string();

            let mut successor = memory_record("r-succ", "vera prefers fish CLI");
            successor["supersedes"] = json!(h_subject.clone());
            let succ = crate::app_runtime::async_runtime::block_on(emporium_write(
                &app,
                graph_id,
                "sophia-memory-core",
                &[successor],
                false,
                true,
                None,
            ))
            .expect("supersession applies");
            assert_eq!(succ["ok"], json!(true), "{succ}");
            let new_subject = succ["results"][0]["subject"].as_str().unwrap().to_string();
            assert_ne!(new_subject, h_subject);
            assert_eq!(
                succ["results"][0]["flags"]["supersedes"],
                json!(h_subject),
                "{succ}"
            );

            let heads = current_heads_flagged(&app, graph_id, "").expect("commons heads read");
            let all_heads: Vec<&String> =
                heads.lineages.iter().flat_map(|l| l.heads.iter()).collect();
            assert!(
                all_heads.iter().any(|s| s.as_str() == new_subject),
                "the new head is current: {heads:?}"
            );
            assert!(
                !all_heads.iter().any(|s| s.as_str() == h_subject),
                "the superseded head is NOT current: {heads:?}"
            );

            // The SAME proof through the actual T2 query face (not just the
            // shared reader both delegate to): `sparql_query_named`'s
            // `currentHeads` shows the NEW head; `lineageOf` walks the chain
            // and shows the OLD head still present in the same lineage.
            let current = crate::emporium::query_engine::run_named_query(
                &app,
                graph_id,
                "sophia-memory-core",
                "MemoryRecord",
                "currentHeads",
                None,
                &json!({}),
            )
            .expect("currentHeads runs");
            // `lineage` legitimately equals the FIRST record's own subject
            // (h_subject) — check the `heads` ARRAY specifically, not the
            // whole row (which would false-positive on the lineage id).
            let current_heads_text = current
                .rows
                .iter()
                .map(|row| row.get("heads").cloned().unwrap_or(Value::Null))
                .map(|v| v.to_string())
                .collect::<Vec<_>>()
                .join(" ");
            assert!(
                current_heads_text.contains(&new_subject),
                "currentHeads shows the new head: {current_heads_text}"
            );
            assert!(
                !current_heads_text.contains(&h_subject),
                "currentHeads must NOT show the superseded head: {current_heads_text}"
            );

            let lineage = crate::emporium::query_engine::run_named_query(
                &app,
                graph_id,
                "sophia-memory-core",
                "MemoryRecord",
                "lineageOf",
                None,
                &json!({ "subject": new_subject }),
            )
            .expect("lineageOf runs");
            let lineage_rows_text = format!("{:?}", lineage.rows);
            assert!(
                lineage_rows_text.contains(&h_subject),
                "lineageOf(new head) still walks to the OLD head: {lineage_rows_text}"
            );
        });
    }

    /// Law 5: dry_run writes NOTHING (count before/after via a real commons
    /// read), and previews the correct outcome.
    #[test]
    fn dry_run_writes_nothing() {
        run_isolated("dry-run", || {
            let app = mock_app();
            let graph_id = "ew-dry-run";
            seed_graph(&app, graph_id);
            let before = current_heads_flagged(&app, graph_id, "").expect("commons heads read");
            assert_eq!(before.lineages.len(), 0);

            let preview = crate::app_runtime::async_runtime::block_on(emporium_write(
                &app,
                graph_id,
                "sophia-memory-core",
                &[memory_record("r1", "vera prefers fish CLI")],
                true,
                true,
                None,
            ))
            .expect("dry_run runs validation only");
            assert_eq!(preview["dryRun"], json!(true), "{preview}");
            assert_eq!(
                preview["results"][0]["outcome"],
                json!("applied"),
                "{preview}"
            );
            assert!(preview["journalRef"].is_null(), "{preview}");

            let after = current_heads_flagged(&app, graph_id, "").expect("commons heads read");
            assert_eq!(
                after.lineages.len(),
                0,
                "dry_run must write NOTHING: {after:?}"
            );
        });
    }

    /// Law 4: retraction excludes the subject from heads but the log keeps
    /// it (a raw SPARQL read still finds the triples).
    #[test]
    fn retraction_excludes_from_heads_but_stays_in_the_log() {
        run_isolated("retract", || {
            let app = mock_app();
            let graph_id = "ew-retract";
            seed_graph(&app, graph_id);
            let written = crate::app_runtime::async_runtime::block_on(emporium_write(
                &app,
                graph_id,
                "sophia-memory-core",
                &[memory_record("r1", "vera prefers fish CLI")],
                false,
                true,
                None,
            ))
            .expect("write applies");
            let subject = written["results"][0]["subject"]
                .as_str()
                .unwrap()
                .to_string();

            let before = current_heads_flagged(&app, graph_id, "").expect("commons heads read");
            assert_eq!(before.lineages.len(), 1, "{before:?}");

            let retraction = crate::app_runtime::async_runtime::block_on(emporium_retract(
                &app,
                graph_id,
                &subject,
                "test: superseded by direct observation",
                "retract",
                Some("agent-vera"),
            ))
            .expect("retraction applies");
            assert_eq!(retraction["ok"].as_bool(), None); // no "ok" key — check subject instead
            assert_eq!(retraction["subject"], json!(subject));

            let after = current_heads_flagged(&app, graph_id, "").expect("commons heads read");
            assert!(
                after.lineages.is_empty(),
                "retraction must exclude the subject from heads: {after:?}"
            );

            // The log keeps it: a raw store read still finds the record's
            // original triples AND the retraction event.
            let store = open_memory_store(&app, graph_id).expect("open store");
            let has_content = subject_has_predicate(
                &store,
                &memory_projection_graph_iri_for(graph_id, ""),
                &subject,
                "http://mnemosyne.dev/memory#content",
            )
            .expect("content check");
            assert!(has_content, "the log keeps the original testimony");
            let has_retraction = subject_has_predicate(
                &store,
                &memory_projection_graph_iri_for(graph_id, ""),
                &subject,
                RETRACTED_AT_PRED,
            )
            .expect("retraction check");
            assert!(has_retraction, "the retraction event is on the subject");
        });
    }

    /// Batches are all-or-halt: one malformed record (a bad enum) rejects the
    /// WHOLE batch — the well-formed sibling record must NOT land either.
    #[test]
    fn all_or_halt_batch_rejects_the_whole_batch_on_one_bad_record() {
        run_isolated("all-or-halt", || {
            let app = mock_app();
            let graph_id = "ew-all-or-halt";
            seed_graph(&app, graph_id);
            let good = memory_record("r-good", "vera prefers fish CLI");
            let mut bad = memory_record("r-bad", "vera prefers zsh");
            bad["scope"] = json!("not-a-real-scope");
            let err = crate::app_runtime::async_runtime::block_on(emporium_write(
                &app,
                graph_id,
                "sophia-memory-core",
                &[good, bad],
                false,
                true,
                None,
            ))
            .expect_err("a bad record rejects the whole batch");
            assert!(err.message().contains("scope"), "{}", err.message());

            let after = current_heads_flagged(&app, graph_id, "").expect("commons heads read");
            assert!(
                after.lineages.is_empty(),
                "the well-formed sibling must NOT land either: {after:?}"
            );
        });
    }

    /// Multi-vocab smuggling: a per-record `vocab` key is rejected loudly (a
    /// batch is single-vocab by construction; this defends against a caller
    /// trying to widen it anyway).
    #[test]
    fn multi_vocab_smuggling_via_a_per_record_vocab_key_is_rejected() {
        run_isolated("multi-vocab", || {
            let app = mock_app();
            let graph_id = "ew-multi-vocab";
            seed_graph(&app, graph_id);
            let mut record = memory_record("r1", "vera prefers fish CLI");
            record["vocab"] = json!("some-other-vocab");
            let err = crate::app_runtime::async_runtime::block_on(emporium_write(
                &app,
                graph_id,
                "sophia-memory-core",
                &[record],
                false,
                true,
                None,
            ))
            .expect_err("a per-record vocab key must reject");
            assert!(err.message().contains("vocab"), "{}", err.message());
        });
    }

    /// Injection discipline: a `rationale` carrying SPARQL-breaking
    /// characters must be recorded as an inert LITERAL VALUE, never allowed
    /// to splice a second clause into the retraction update.
    #[test]
    fn injection_payload_in_rationale_is_recorded_as_an_inert_literal() {
        run_isolated("injection", || {
            let app = mock_app();
            let graph_id = "ew-injection";
            seed_graph(&app, graph_id);
            let written = crate::app_runtime::async_runtime::block_on(emporium_write(
                &app,
                graph_id,
                "sophia-memory-core",
                &[memory_record("r1", "vera prefers fish CLI")],
                false,
                true,
                None,
            ))
            .expect("write applies");
            let subject = written["results"][0]["subject"]
                .as_str()
                .unwrap()
                .to_string();

            let payload = "bad\"} } ; DROP EVERYTHING ; INSERT DATA { <urn:evil> a <urn:Evil>";
            let retraction = crate::app_runtime::async_runtime::block_on(emporium_retract(
                &app, graph_id, &subject, payload, "retract", None,
            ))
            .expect("retraction with an adversarial rationale still applies safely");
            assert_eq!(retraction["rationale"], json!(payload));

            // The store must NOT contain the injected subject/type at all —
            // proof the payload never escaped its literal position.
            let store = open_memory_store(&app, graph_id).expect("open store");
            let injected = subject_has_predicate(
                &store,
                &memory_projection_graph_iri_for(graph_id, ""),
                "urn:evil",
                "http://www.w3.org/1999/02/22-rdf-syntax-ns#type",
            )
            .unwrap_or(false);
            assert!(!injected, "the injection payload must not have executed");

            // And the ORIGINAL record's content must be untouched (a
            // successful injection would typically also corrupt the store).
            let heads = current_heads_flagged(&app, graph_id, "").expect("commons heads read");
            assert!(
                heads.lineages.is_empty(),
                "retraction excludes the head as normal, injection notwithstanding: {heads:?}"
            );
        });
    }

    /// The generic (non-memory) lane: a materialize/current-state class
    /// (e.g. `emporium-bookmark`'s `Bookmark`) writes via emporium_write,
    /// reports `outcome: "applied"` (no membrane/convergence concept), and a
    /// resubmission of the SAME localId is the "Replace"-strategy reappearance
    /// — a plain upsert, not a rejected update.
    #[test]
    fn generic_lane_writes_a_materialize_class_and_replace_reappears_in_place() {
        run_isolated("generic-lane", || {
            let app = mock_app();
            let graph_id = "ew-generic";
            seed_graph(&app, graph_id);
            let first = crate::app_runtime::async_runtime::block_on(emporium_write(
                &app,
                graph_id,
                "emporium-bookmark",
                &[json!({
                    "kind": "Bookmark",
                    "clientRef": "book-1",
                    "url": "https://example.test/one",
                    "title": "First Title",
                })],
                false,
                false,
                None,
            ))
            .expect("generic write applies");
            assert_eq!(first["results"][0]["outcome"], json!("applied"), "{first}");
            let subject = first["results"][0]["subject"].as_str().unwrap().to_string();

            // Replace reappearance: resubmit the SAME localId with a new title.
            let second = crate::app_runtime::async_runtime::block_on(emporium_write(
                &app,
                graph_id,
                "emporium-bookmark",
                &[json!({
                    "kind": "Bookmark",
                    "clientRef": "book-1",
                    "url": "https://example.test/one",
                    "title": "Updated Title",
                })],
                false,
                false,
                None,
            ))
            .expect("replace reappearance applies");
            assert_eq!(second["results"][0]["subject"], json!(subject));
            assert_eq!(
                second["results"][0]["outcome"],
                json!("applied"),
                "{second}"
            );

            let read = crate::emporium::objects::read_object(
                &app,
                graph_id,
                "emporium-bookmark",
                "Bookmark",
                "book-1",
            )
            .expect("read back");
            let titles = read["predicates"]["http://mnemosyne.dev/bookmark#title"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            assert_eq!(titles.len(), 1, "{read}");
            assert!(
                titles[0].as_str().unwrap_or("").contains("Updated Title"),
                "{read}"
            );
        });
    }

    /// Generic-lane lineage rejection: `supersedes` on a materialize class has
    /// no machinery to run it — loud rejection, nothing written.
    #[test]
    fn generic_lane_rejects_supersedes_loudly() {
        run_isolated("generic-supersedes", || {
            let app = mock_app();
            let graph_id = "ew-generic-supersedes";
            seed_graph(&app, graph_id);
            let err = crate::app_runtime::async_runtime::block_on(emporium_write(
                &app,
                graph_id,
                "emporium-bookmark",
                &[json!({
                    "kind": "Bookmark",
                    "clientRef": "book-1",
                    "url": "https://example.test/one",
                    "title": "Title",
                    "supersedes": "urn:mnemosyne:local:graph:ew-generic-supersedes:projection:bookmark:bookmark:old",
                })],
                false,
                false,
                None,
            ))
            .expect_err("supersedes on a generic class must reject");
            assert!(err.message().contains("supersedes"), "{}", err.message());
        });
    }

    /// Review finding (objects.rs / write.rs generic lane): a full-IRI
    /// `clientRef` used to fall into `objects::resolve_subject`'s "looks like
    /// a URI, take it verbatim" branch — letting the caller dictate the
    /// minted subject outright (Law 2 violation) instead of it being minted
    /// through the class's declared `subject_rule`. Both `urn:` and
    /// `scheme://` forms must be rejected loudly, and nothing lands.
    #[test]
    fn generic_lane_rejects_a_full_iri_client_ref() {
        run_isolated("generic-full-iri-ref", || {
            let app = mock_app();
            let graph_id = "ew-generic-full-iri-ref";
            seed_graph(&app, graph_id);

            let urn_attempt = crate::app_runtime::async_runtime::block_on(emporium_write(
                &app,
                graph_id,
                "emporium-bookmark",
                &[json!({
                    "kind": "Bookmark",
                    "clientRef": "urn:attacker:chosen",
                    "url": "https://example.test/one",
                    "title": "Smuggled via urn:",
                })],
                false,
                false,
                None,
            ))
            .expect_err("a urn: clientRef must reject");
            assert!(
                urn_attempt.message().contains("clientRef")
                    || urn_attempt.message().contains("localId"),
                "{}",
                urn_attempt.message()
            );

            let scheme_attempt = crate::app_runtime::async_runtime::block_on(emporium_write(
                &app,
                graph_id,
                "emporium-bookmark",
                &[json!({
                    "kind": "Bookmark",
                    "clientRef": "https://attacker.example/chosen",
                    "url": "https://example.test/one",
                    "title": "Smuggled via scheme://",
                })],
                false,
                false,
                None,
            ))
            .expect_err("a scheme:// clientRef must reject");
            assert!(
                scheme_attempt.message().contains("clientRef")
                    || scheme_attempt.message().contains("localId"),
                "{}",
                scheme_attempt.message()
            );

            // Nothing landed at the attacker-chosen subject.
            let gone = crate::emporium::objects::read_object(
                &app,
                graph_id,
                "emporium-bookmark",
                "Bookmark",
                "urn:attacker:chosen",
            );
            assert!(
                matches!(gone, Err(ObjectError::Absent(_))),
                "the smuggled subject must not exist: {gone:?}"
            );
        });
    }
}
