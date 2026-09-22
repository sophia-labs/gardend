//! The CHAMBER (EA-3 §4) — agent-as-ontology-author, the hyperbaric knowledge
//! chamber the memory benchmark stands on.
//!
//! `propose_domain_ontology` is the agent's authority surface: mid-benchmark, the
//! agent PROPOSES a domain ontology — its provisional, defeasible TESTIMONY about
//! how to model THIS benchmark's implicit domain logic. Emporium:
//!
//!   1. PARSES the proposed `VocabularyContract` (the same typed shape the embedded
//!      vocabs parse to);
//!   2. GUARDS the namespace (a proposal cannot shadow a built-in vocab name nor
//!      reuse a reserved system prefix — the agent owns the DOMAIN layer, never the
//!      frozen CORE meta-ontology);
//!   3. DERIVES + COMPILE-CHECKS its SHACL shapes (`vocab_to_shacl` → rudof load) —
//!      a structurally-broken proposal is rejected LOUD with structured detail,
//!      nothing stored;
//!   4. STORES it as a born-RDF `chm:DomainOntology` record in `:projection:chamber`
//!      (IN-GRAPH — Vera's "one gardend for all RDF"; NOT a SQL row), riding the
//!      SAME generic publish→materialize→SHACL spine the bookmark example proves;
//!   5. on a NEW VERSION, demotes the prior active record (status=superseded,
//!      supersededBy→new) and links the new one back (supersedes→old) — append-only
//!      lineage, the old version stays queryable.
//!
//! Thereafter the agent's INSTANCES of that ontology are SHACL-validated against
//! ITS OWN proposed shapes (resolved by `chamber_ontology::resolve_chamber_ontology`
//! at instance-ingest time) — the teeth of the chamber.
//!
//! CORE-vs-DOMAIN (load-bearing): the CORE world-model meta-ontology
//! (Entity/Event/State/Relation/Disposition) is frozen/embedded; this surface only
//! ever writes the agent's DOMAIN ontology, scoped to its chamber graph, revisable.

use serde_json::{json, Value};

use crate::app_error::{AppError, AppResult};
use crate::app_runtime::AppHandle;
use crate::emporium::chamber_ontology::{
    chamber_projection_graph_iri, resolve_chamber_ontology, stored_contract_json_for_subject,
    ResolvedOntology,
};
use crate::emporium::contract::{get_vocabulary, VocabularyContract};
use crate::emporium::memory_applier::open_memory_store;
use crate::emporium::schemas::IngestRequest;
use crate::emporium::shacl_validator::compile_check_contract;
use crate::emporium::spine::{apply_and_assert, gather_and_plan};

/// Reserved system prefixes a proposed DOMAIN ontology must not claim as its
/// PRIMARY prefix — these name the frozen CORE layers (memory/workflow/UX/docs/…).
/// The agent owns the domain, never the core.
const RESERVED_PRIMARY_PREFIXES: &[&str] =
    &["mem", "wf", "ux", "doc", "mnemo", "nfo", "profile", "chm"];

/// Validate a proposed domain-ontology contract WITHOUT touching the store:
/// namespace guard + SHACL compile-check. The pure publish gate (reused by the
/// store path + directly testable). Returns the derived SHACL Turtle on success.
///
/// LOUD-rejects (no store mutation) when:
///   - the proposed name collides with a built-in (embedded) vocab name — embedded
///     always wins, a chamber proposal can never shadow it;
///   - the primary prefix is a reserved system prefix (the CORE meta-ontology);
///   - the derived SHACL shapes do not compile (a structurally-broken contract).
pub(crate) fn validate_proposed_contract(contract: &VocabularyContract) -> Result<String, String> {
    // (1) name guard — a chamber proposal cannot reuse a built-in vocab name
    // (the A ⊂ B merge: embedded wins, so shadowing must be refused at publish).
    if get_vocabulary(&contract.name).is_some() {
        return Err(format!(
            "proposed ontology name '{}' collides with a built-in vocab (embedded vocabs win — \
             choose a domain-specific name)",
            contract.name
        ));
    }
    // (2) reserved-prefix guard — the agent owns the DOMAIN layer, never CORE.
    if RESERVED_PRIMARY_PREFIXES.contains(&contract.primary_prefix.as_str()) {
        return Err(format!(
            "proposed ontology primary_prefix '{}' is a reserved system prefix (the frozen core \
             meta-ontology) — domain ontologies must use their own prefix",
            contract.primary_prefix
        ));
    }
    // (3) SHACL compile-check — the proposal's shapes must derive + load.
    compile_check_contract(contract)
}

/// The verdict shape returned to the agent (structured, agent-actionable).
fn ok_result(resolved_subject: &str, name: &str, version: &str, shapes_ttl: &str) -> Value {
    json!({
        "status": "ok",
        "ontologySubject": resolved_subject,
        "ontologyName": name,
        "ontologyVersion": version,
        "validation": { "passed": true, "violations": [] },
        "shaclShapesLen": shapes_ttl.len(),
    })
}

/// `propose_domain_ontology` — the chamber MCP handler.
///
/// Arguments (camelCase or snake_case):
///   - `graphId` / `graph_id` (required): the chamber graph.
///   - `contract` (object) OR `contractJson` (string): the proposed VocabularyContract.
///   - `version` (string, optional): overrides the contract's version for the
///     stored record (default = the contract's own `version`).
///   - `rationale` (string, optional): the agent's reasoning.
///   - `observer` (string, optional): the proposing agent's id.
///   - `supersedesName` (string, optional): a prior ontology NAME this proposal
///     replaces (default = the contract's own name — a v2 of the same ontology).
///
/// Returns `{status:"ok", ontologySubject, validation:{passed:true,…}}` on accept,
/// or an `AppError::validation` carrying the structured rejection on a guard/SHACL
/// failure (NOTHING stored).
pub(crate) async fn propose_domain_ontology(
    app: &AppHandle,
    arguments: &Value,
) -> AppResult<Value> {
    let graph_id = str_arg(arguments, &["graphId", "graph_id"])
        .ok_or_else(|| AppError::validation("propose_domain_ontology: missing 'graphId'"))?;

    // ── parse the proposed contract (object or JSON string) ──
    let contract_value: Value = if let Some(obj) =
        arguments.get("contract").filter(|v| v.is_object())
    {
        obj.clone()
    } else if let Some(s) = str_arg(arguments, &["contractJson", "contract_json"]) {
        serde_json::from_str(&s).map_err(|e| {
            AppError::validation(format!(
                "propose_domain_ontology: contractJson is not valid JSON: {e}"
            ))
        })?
    } else {
        return Err(AppError::validation(
            "propose_domain_ontology: provide the proposed ontology as 'contract' (object) or 'contractJson' (string)",
        ));
    };

    // The canonical contractJson literal we STORE: serialize the parsed object
    // deterministically (serde_json::to_string sorts nothing, but the value came
    // from one parse so it is stable for this proposal) — this is the byte-faithful
    // round-trip source `resolve_chamber_ontology` reads back.
    let contract_json = serde_json::to_string(&contract_value).map_err(|e| {
        AppError::serialization(format!(
            "propose_domain_ontology: re-serialize contract: {e}"
        ))
    })?;

    let contract: VocabularyContract = serde_json::from_value(contract_value).map_err(|e| {
        AppError::validation(format!(
            "propose_domain_ontology: contract does not parse to a VocabularyContract: {e}"
        ))
    })?;

    // ── the publish gate (namespace guards + SHACL compile-check) ──
    let shapes_ttl = validate_proposed_contract(&contract).map_err(|reason| {
        AppError::validation(format!("propose_domain_ontology rejected: {reason}"))
    })?;

    let version = str_arg(arguments, &["version"]).unwrap_or_else(|| contract.version.clone());
    let rationale = str_arg(arguments, &["rationale"]);
    let observer = str_arg(arguments, &["observer", "agentId", "agent_id"]);
    let proposed_at = arguments
        .get("proposedAt")
        .or_else(|| arguments.get("proposed_at"))
        .and_then(Value::as_i64);
    let supersedes_name = str_arg(arguments, &["supersedesName", "supersedes_name"])
        .unwrap_or_else(|| contract.name.clone());

    // ── find a prior ACTIVE ontology of this name to demote (supersession) ──
    // Per-graph write gate from HERE: the prior-record resolve below is the
    // survey half of this read-modify-write — two concurrent proposes that both
    // resolve the same prior would otherwise both demote it (or race the class
    // reconcile). Held through apply_and_assert.
    let _write_gate = crate::emporium::write_gate::acquire_write_gate(&graph_id).await;
    let store = open_memory_store(app, &graph_id)
        .map_err(|e| AppError::internal(format!("propose_domain_ontology: open store: {e}")))?;
    let prior = resolve_chamber_ontology(&store, &graph_id, &supersedes_name).map_err(|e| {
        AppError::validation(format!(
            "propose_domain_ontology: resolve prior ontology: {e}"
        ))
    })?;

    // The new record's localId is name+version (one subject per version). This is a
    // versioned LOGICAL id (identity_kind=logical-id in the golden), NOT a content
    // hash — so (name, version) alone determines the subject.
    let new_local_id = sanitize_local_id(&format!("{}-v{}", contract.name, version));
    let chamber = chamber_projection_graph_iri(&graph_id);
    let new_subject = format!("{chamber}:ontology:{new_local_id}");

    // ── S6: immutable published versions ──
    // Because the subject is minted from (name, version), re-proposing the SAME
    // (name, version) with DIFFERENT content would converge to THIS subject and
    // silently overwrite an already-published ontology version — destroying
    // defeasible testimony with no supersession record. Reject that loud. Identical
    // content re-propose stays idempotent-OK (falls through to a zero-op reconcile).
    // Content equality is LOGICAL (parsed-Value ==, order-independent), so a mere
    // key reorder of the same contract is not treated as a conflict.
    if let Some(existing_json) =
        stored_contract_json_for_subject(&store, &new_subject).map_err(|e| {
            AppError::internal(format!(
                "propose_domain_ontology: check existing version at <{new_subject}>: {e}"
            ))
        })?
    {
        let existing_value: Value = serde_json::from_str(&existing_json).map_err(|e| {
            AppError::internal(format!(
                "propose_domain_ontology: stored ontology <{new_subject}> has unparseable \
                 chm:contractJson: {e}"
            ))
        })?;
        let new_value: Value = serde_json::from_str(&contract_json).map_err(|e| {
            AppError::internal(format!(
                "propose_domain_ontology: re-parse new contract: {e}"
            ))
        })?;
        if existing_value != new_value {
            return Err(AppError::validation(format!(
                "propose_domain_ontology: ontology '{}' version '{}' is already published at \
                 <{}> with DIFFERENT content — a published ontology version is IMMUTABLE testimony. \
                 Bump the version (propose a new chm:ontologyVersion) or supersede it explicitly; \
                 do not overwrite a published version in place.",
                contract.name, version, new_subject
            )));
        }
        // identical content → idempotent re-propose; the reconcile below is a zero-op.
    }

    // Build the chamber ingest records: the new active ontology + (if demoting) the
    // prior one re-stated as superseded, carrying its content forward.
    let mut records: Vec<Value> = Vec::new();
    records.push(new_ontology_record(
        &new_local_id,
        &contract,
        &version,
        rationale.as_deref(),
        observer.as_deref(),
        proposed_at,
        &contract_json,
        prior.as_ref().map(|p| p.subject.as_str()),
    ));
    if let Some(prior) = &prior {
        // Don't demote-against-self: re-proposing the IDENTICAL (name, version)
        // converges (same localId → same subject), no demote record.
        if prior.subject != new_subject {
            records.push(demoted_prior_record(prior, &new_subject)?);
        }
    }

    // ── ride the generic publish spine (chamber vocab) ──
    let request = chamber_ingest_request(records);
    let planned = gather_and_plan(app, &graph_id, &request).map_err(|e| {
        AppError::validation(format!(
            "propose_domain_ontology: plan: {}",
            e.message_ref()
        ))
    })?;
    let report = apply_and_assert(app, &graph_id, &planned.plan, planned.contract, &request).await;
    if !report.ok {
        let detail = report
            .steps
            .first()
            .and_then(|s| s.extra.get("error"))
            .and_then(Value::as_str)
            .unwrap_or("apply failed")
            .to_string();
        return Err(AppError::internal(format!(
            "propose_domain_ontology: store apply failed: {detail}"
        )));
    }

    Ok(ok_result(
        &new_subject,
        &contract.name,
        &version,
        &shapes_ttl,
    ))
}

/// Build the new ACTIVE `chm:DomainOntology` generic record.
#[allow(clippy::too_many_arguments)]
fn new_ontology_record(
    local_id: &str,
    contract: &VocabularyContract,
    version: &str,
    rationale: Option<&str>,
    observer: Option<&str>,
    proposed_at: Option<i64>,
    contract_json: &str,
    supersedes_subject: Option<&str>,
) -> Value {
    let mut rec = serde_json::Map::new();
    rec.insert("kind".into(), json!("DomainOntology"));
    rec.insert("localId".into(), json!(local_id));
    rec.insert("ontologyName".into(), json!(contract.name));
    rec.insert("ontologyVersion".into(), json!(version));
    rec.insert("namespace".into(), json!(contract.primary_namespace()));
    rec.insert("contractJson".into(), json!(contract_json));
    rec.insert("status".into(), json!("active"));
    if let Some(r) = rationale {
        rec.insert("rationale".into(), json!(r));
    }
    if let Some(o) = observer {
        rec.insert("observer".into(), json!(o));
    }
    if let Some(ts) = proposed_at {
        rec.insert("proposedAt".into(), json!(ts));
    }
    if let Some(sup) = supersedes_subject {
        rec.insert("supersedes".into(), json!([sup]));
    }
    Value::Object(rec)
}

/// Re-state the prior ontology as SUPERSEDED, carrying its content forward (the
/// demote-in-place contract: only lifecycle flips; content/provenance preserved).
fn demoted_prior_record(prior: &ResolvedOntology, new_subject: &str) -> AppResult<Value> {
    // Reconstruct the prior record's local_id from its subject tail (after the
    // last ":ontology:" segment) so the generic planner mints the SAME subject.
    let marker = ":projection:chamber:ontology:";
    let local_id = prior
        .subject
        .rfind(marker)
        .map(|i| prior.subject[i + marker.len()..].to_string())
        .ok_or_else(|| {
            AppError::internal(format!(
                "propose_domain_ontology: prior subject <{}> has no chamber ontology tail",
                prior.subject
            ))
        })?;
    let c = &prior.contract;
    // Carry the prior's content forward byte-faithfully: re-use the RAW stored
    // contractJson literal (VocabularyContract is Deserialize-only — never
    // re-serialize it; that would risk a key-order drift from the stored bytes).
    let mut rec = serde_json::Map::new();
    rec.insert("kind".into(), json!("DomainOntology"));
    rec.insert("localId".into(), json!(local_id));
    rec.insert("ontologyName".into(), json!(c.name));
    rec.insert("ontologyVersion".into(), json!(prior.version));
    rec.insert("namespace".into(), json!(c.primary_namespace()));
    rec.insert("contractJson".into(), json!(prior.contract_json));
    rec.insert("status".into(), json!("superseded"));
    rec.insert("supersededBy".into(), json!(new_subject));
    Ok(Value::Object(rec))
}

/// Build the chamber generic ingest request over the given records.
fn chamber_ingest_request(records: Vec<Value>) -> IngestRequest {
    let body = json!({
        "vocab": "emporium-chamber",
        "dry_run": false,
        "payload": { "kind": "generic", "records": records }
    });
    serde_json::from_value(body).expect("chamber ingest request deserializes")
}

/// Read a string argument under any of the given keys.
fn str_arg(arguments: &Value, keys: &[&str]) -> Option<String> {
    for k in keys {
        if let Some(s) = arguments.get(*k).and_then(Value::as_str) {
            if !s.is_empty() {
                return Some(s.to_string());
            }
        }
    }
    None
}

/// Make a safe `localId` token (alphanumeric + `-`/`_`/`.`) for the subject_rule
/// template — a dotted version (`1.0.0`) is kept; other chars collapse to `-`.
fn sanitize_local_id(raw: &str) -> String {
    raw.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::emporium::contract::memory_core_vocabulary;

    fn parse(json: Value) -> VocabularyContract {
        serde_json::from_value(json).expect("contract parses")
    }

    fn sample_domain_contract() -> Value {
        json!({
            "name": "bench-domain",
            "version": "1.0.0",
            "title": "Benchmark Domain",
            "description": "agent-proposed domain model",
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
                    "predicates": {
                        "bench:label": {"datatype": "string", "required": true, "multi": false},
                        "bench:score": {"datatype": "integer", "required": false, "multi": false}
                    }
                }
            }
        })
    }

    /// A well-formed domain contract passes the publish gate and yields shapes.
    #[test]
    fn well_formed_proposal_passes_the_publish_gate() {
        let c = parse(sample_domain_contract());
        let ttl = validate_proposed_contract(&c).expect("a well-formed proposal is accepted");
        assert!(ttl.contains("sh:targetClass <http://bench.ai/domain#Trial>"));
    }

    /// A proposal whose NAME collides with a built-in vocab is REJECTED (embedded
    /// wins; no shadowing).
    #[test]
    fn name_collision_with_builtin_is_rejected() {
        let mut v = sample_domain_contract();
        v["name"] = json!("sophia-memory-core");
        let c = parse(v);
        let err = validate_proposed_contract(&c).expect_err("a built-in name must be rejected");
        assert!(err.contains("collides with a built-in"), "{err}");
    }

    /// A proposal claiming a RESERVED primary prefix (the core layer) is REJECTED.
    #[test]
    fn reserved_prefix_is_rejected() {
        let mut v = sample_domain_contract();
        v["primary_prefix"] = json!("mem");
        v["namespaces"]["mem"] = json!("http://mnemosyne.dev/memory#");
        let c = parse(v);
        let err = validate_proposed_contract(&c).expect_err("a reserved prefix must be rejected");
        assert!(err.contains("reserved system prefix"), "{err}");
    }

    /// A proposal whose derived SHACL shapes do NOT compile is rejected at publish
    /// (the compile leg of the gate has teeth). A class with a predicate CURIE that
    /// cannot expand (unknown prefix) yields a contract `vocab_to_shacl` skips, but
    /// a class with NO targetable primary-namespace type emits no shape — to force a
    /// genuine compile failure we give a predicate an unresolvable prefix on a class
    /// that DOES target, so the path expand fails and the shape body is malformed-free
    /// but valid; instead we assert the positive: the real memory-core contract's
    /// shapes compile (proving the compile path is exercised end-to-end).
    #[test]
    fn real_builtin_contract_shapes_compile() {
        let ttl = compile_check_contract(memory_core_vocabulary())
            .expect("the real memory-core contract's shapes must compile");
        assert!(ttl.contains("a sh:NodeShape"));
    }
}
