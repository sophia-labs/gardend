//! The ingest spine — `gather_and_plan` over the gardend cell store.
//!
//! Port of `emporium_engine/spine.py::gather_and_plan` (the standalone twin of
//! the platform worker handler). The spine owns the ONE place where the read
//! stages are sequenced; the FastAPI/loopback route is a thin caller.
//!
//! The defining contract: **the planner stays pure**. [`survey`] does every read
//! (RDF entities + workspace structure), [`content`] builds doc bodies, and
//! [`planner::plan_compute`] / [`planner::plan_campaign_compute`] consume the
//! gathered snapshot and return an op list with NO I/O. [`gather_and_plan`]
//! exposes exactly that path: it surveys (reads), then calls the pure planner.
//!
//! Where the platform spine crosses the gateway MCP boundary, this twin reads
//! the gardend cell directly:
//!   - RDF entities / triples / provenance via SPARQL over the user:rdf graph
//!     (`survey::survey_live` / `current_wf_triples` / `docs_provenance`);
//!   - workspace structure (folders / docs / wires) from the cell workspace
//!     snapshot file — the gardend analog of the gateway `get_workspace` tool —
//!     since `survey_live` deliberately leaves `live.folders`/`live.docs` empty
//!     (see survey.rs);
//!   - document markdown via `read_document_record` → `document_markdown`, the
//!     gardend `canonical_md` analog (the provenance render-sha anchor).
//!
//! 🔴 READ-GRAPH == WRITE-GRAPH: every RDF read here flows through the survey
//! helpers, which name `GRAPH <{root}:user:rdf>` (`user_rdf_graph_iri`). The
//! doc-URI subject prefix is `live.prefix` (`graph_subject`); in gardend the two
//! DIVERGE, and the planner already uses `prefix` for `doc_uri` while diffing in
//! the user:rdf graph the survey read.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value as Json;

use crate::app_error::{AppError, AppResult};
use crate::app_runtime::AppHandle;
use crate::document_export_rendering::document_markdown;
use crate::document_record_store::read_document_record;
use crate::emporium::applied_journal::journal_applied_plan;
use crate::emporium::applier::{apply_plan, ApplyReport, StepReport};
use crate::emporium::asserts::{assert_workflow, AssertWire};
use crate::emporium::class_dispatch;
use crate::emporium::content::variant_doc_id;
use crate::emporium::contract::{
    agent_memory_validation_vocabulary, get_vocabulary, memory_core_vocabulary,
    workflow_vocabulary, VocabularyContract,
};
use crate::emporium::memory_applier::{
    open_memory_store, run_memory_update, validate_memory_write, ValidationGate,
};
use crate::emporium::planner::{
    plan_campaign_compute, plan_compute, plan_generic_compute, plan_memory_compute, CurrentWire,
    Plan, Step,
};
use crate::emporium::reconcile::{ClassScope, Placement, SpanKey};
use crate::emporium::schemas::{IngestPayload, IngestRequest};
use crate::emporium::shacl_validator::{violations_to_halt_string, ViolationRecord};
use crate::emporium::survey::{
    current_memory_triples, current_wf_triples, docs_provenance, survey_live, Live,
};
use crate::emporium::terms::{apply_slug, PlanError};
use crate::graph_record_store::read_graph_record;
use crate::paths::{existing_document_dir, existing_graph_dir};
use crate::storage::read_json;
use crate::ydoc_paths::workspace_snapshot_path;

struct MaterializedClassPartitions {
    scopes_desireds: Vec<(ClassScope, Vec<crate::emporium::terms::Triple>)>,
    materialized_triple_count: usize,
    virtual_triple_count: usize,
    virtual_class_count: usize,
}

/// The result of [`gather_and_plan`]: the computed [`Plan`] plus the contract it
/// was planned against. Mirrors the Python `{"plan": ..., "contract": ...}`.
pub(crate) struct Planned {
    pub(crate) plan: Plan,
    #[allow(dead_code)]
    pub(crate) contract: &'static VocabularyContract,
}

/// Survey the live graph (reads via the cell store), then run the PURE planner.
///
/// Returns the op-list [`Plan`] with no writes. Raises [`AppError`] on a read
/// failure (missing graph, SPARQL error) and surfaces a [`PlanError`] as a
/// validation `AppError` when the inputs cannot produce a valid plan.
///
/// The survey is the only step that reads the store; `plan_compute` /
/// `plan_campaign_compute` are pure functions of the gathered snapshot.
pub(crate) fn gather_and_plan(
    app: &AppHandle,
    graph_id: &str,
    request: &IngestRequest,
) -> AppResult<Planned> {
    // ── memory family (vocab "mem*", IngestPayload::Memory) ──
    // The additive path: select the memory contract, read the live memory
    // projection (NOT user:rdf), and run the pure memory planner. The wf/campaign
    // reads + planner below stay byte-for-byte unchanged.
    if let IngestPayload::Memory { records } = &request.payload {
        let contract = memory_core_vocabulary();
        // The memory planner self-creates its registry folder, so it still needs
        // the workspace structure (to gate the folder op on absence).
        let live = survey_live(app, graph_id, contract)?;
        let snapshot = read_workspace_snapshot(app, graph_id)?;
        let live = hydrate_workspace_structure(live, snapshot.as_ref());
        // 🔴 SUPERSESSION-SURVEY RE-SCOPE (fix #1): survey the SAME per-observer
        // graph the write targets. The batch observer is the first record's
        // (the planner re-derives + enforces homogeneity); reading the wrong-
        // observer graph would empty the survey → no demote (see survey.rs).
        let observer = records
            .first()
            .map(crate::emporium::planner::record_observer)
            .unwrap_or("");
        let current_mem = current_memory_triples(app, graph_id, observer)?;
        let plan = plan_memory_compute(contract, graph_id, records, &live, &current_mem)
            .map_err(plan_error_to_app)?;
        return Ok(Planned { plan, contract });
    }

    // ── GENERIC simple-projection family (EA-3, IngestPayload::Generic) ──
    // The vocab-agnostic path: resolve the contract BY NAME, run the PURE generic
    // planner (B5 subject minting + render_class_triples), and hand back a
    // `simple-projection` plan. The mint is store-independent — the reconcile
    // applier in `apply_and_assert` does the survey/diff/validate/apply against the
    // write_target sink — so no live read is needed for the EMBEDDED tier.
    //
    // TWO-TIER (A ⊂ B) resolution (EA-3 §4 chamber): try the EMBEDDED registry
    // first (`get_vocabulary`); on a miss, resolve the agent's RUNTIME in-graph
    // proposed ontology of that name from `:projection:chamber`
    // (`resolve_ingest_contract`, store-backed). The chamber tier needs the store,
    // so it is read HERE for planning; the apply fork RE-RESOLVES the same contract
    // (it re-opens the store anyway) so the validation gate uses the agent's OWN
    // proposed shapes. The embedded chamber pack is returned as the `&'static`
    // `Planned.contract` placeholder for a chamber-resolved plan (the simple-
    // projection apply ignores it and re-resolves from `plan.vocab`).
    if let IngestPayload::Generic { records } = &request.payload {
        use crate::emporium::chamber_ontology::resolve_ingest_contract;
        if let Some(embedded) = get_vocabulary(&request.vocab) {
            let plan =
                plan_generic_compute(embedded, graph_id, records).map_err(plan_error_to_app)?;
            return Ok(Planned {
                plan,
                contract: embedded,
            });
        }
        // Chamber tier: resolve the agent's proposed contract from the graph.
        let store = open_memory_store(app, graph_id)
            .map_err(|e| AppError::internal(format!("chamber ingest: open store: {e}")))?;
        let resolved = resolve_ingest_contract(&store, graph_id, &request.vocab)
            .map_err(AppError::validation)?;
        let plan = plan_generic_compute(&resolved, graph_id, records).map_err(plan_error_to_app)?;
        // The `&'static` placeholder (apply re-resolves the real chamber contract).
        let placeholder = get_vocabulary("emporium-chamber")
            .ok_or_else(|| AppError::internal("emporium-chamber pack must be registered"))?;
        return Ok(Planned {
            plan,
            contract: placeholder,
        });
    }

    // The wf/campaign golden contract is a process singleton.
    let contract = workflow_vocabulary();
    let slug = |t: &str| match &contract.slug_rule {
        Some(rule) => apply_slug(rule, t),
        None => t.to_string(),
    };

    // ── reads (the only I/O) ──
    let live = survey_live(app, graph_id, contract)?;
    let triples = current_wf_triples(app, graph_id, contract)?;
    // Workspace structure (folders/docs/wires) from the cell workspace snapshot
    // — survey_live leaves live.folders/live.docs empty; the spine fills them.
    let snapshot = read_workspace_snapshot(app, graph_id)?;
    let live = hydrate_workspace_structure(live, snapshot.as_ref());

    let plan = match &request.payload {
        IngestPayload::Workflow {
            parsed,
            judgment,
            journal_document_id,
        } => {
            // Candidate doc-id set (mirrors emporium_handler.py / spine.py): the
            // ids whose markdown/provenance/wires the planner may consult.
            let mut doc_ids: BTreeSet<String> = BTreeSet::new();
            let existing = live.workflows.get(&parsed.name).cloned();
            if let Some(ex) = &existing {
                if let Some(did) = &ex.doc_id {
                    doc_ids.insert(did.clone());
                }
                if let Some(nodes) = live.nodes_by_workflow.get(&ex.uri) {
                    doc_ids.extend(nodes.values().cloned());
                }
            }
            let short_guess = existing
                .as_ref()
                .and_then(|e| e.doc_id.clone())
                .or_else(|| judgment.as_ref().and_then(|j| j.short_id.clone()));
            if let Some(sg_full) = short_guess {
                let wf_folder_guess = existing
                    .as_ref()
                    .and_then(|e| e.doc_id.as_ref())
                    .and_then(|did| live.docs.get(did))
                    .and_then(|d| d.folder_id.clone());
                let sg = wf_folder_guess.unwrap_or_else(|| format!("{sg_full}-folder"));
                let sg = sg.strip_suffix("-folder").unwrap_or(&sg).to_string();
                doc_ids.insert(sg_full.clone());
                for n in &parsed.nodes {
                    doc_ids.insert(format!("{sg}-n-{}", slug(&n.label)));
                }
            }
            if let Some(j) = judgment {
                for a in &j.new_archetypes {
                    doc_ids.insert(format!("agent-{}", slug(&a.slug)));
                }
            }
            if let (Some(run), Some(ex)) = (&parsed.run, &existing) {
                let wf_folder = ex
                    .doc_id
                    .as_ref()
                    .and_then(|did| live.docs.get(did))
                    .and_then(|d| d.folder_id.clone())
                    .unwrap_or_default();
                let base = wf_folder.strip_suffix("-folder").unwrap_or(&wf_folder);
                doc_ids.insert(format!("{base}-run-{}", slug(&run.run_id)));
            }

            // Restrict to ids that actually exist as docs in the workspace.
            let existing_doc_ids: BTreeSet<String> = doc_ids
                .into_iter()
                .filter(|d| live.docs.contains_key(d))
                .collect();

            let wires = wires_from_snapshot(snapshot.as_ref(), Some(&existing_doc_ids));
            let docs_md = gather_docs_markdown(app, graph_id, &existing_doc_ids)?;
            let provenance = docs_provenance(
                app,
                graph_id,
                &existing_doc_ids.iter().cloned().collect::<Vec<_>>(),
            )?;

            let journal_doc_uri = journal_document_id
                .as_ref()
                .filter(|j| !j.is_empty())
                .map(|j| format!("{}:doc:{j}", live.prefix));

            // The content builders read the camelCase JSON the schema structs
            // deserialized from (Python `parsed.model_dump()`).
            let parsed_json = serde_json::to_value(parsed)
                .map_err(|e| AppError::serialization(format!("serialize parsed: {e}")))?;
            let run_json = parsed_json.get("run").filter(|v| !v.is_null()).cloned();

            plan_compute(
                contract,
                parsed,
                &parsed_json,
                run_json.as_ref(),
                judgment.as_ref(),
                &live,
                &triples,
                &wires,
                &docs_md,
                &provenance,
                journal_doc_uri.as_deref(),
            )
            .map_err(plan_error_to_app)?
        }
        IngestPayload::Campaign { campaign } => {
            let cid = campaign.campaign_id.clone();
            let campaign_json = serde_json::to_value(campaign)
                .map_err(|e| AppError::serialization(format!("serialize campaign: {e}")))?;
            let mut doc_ids: BTreeSet<String> = BTreeSet::new();
            if let Some(cands) = campaign_json.get("candidates").and_then(Json::as_array) {
                for c in cands {
                    let idx = c.get("idx").and_then(Json::as_i64).unwrap_or(0);
                    doc_ids.insert(variant_doc_id(&slug, &cid, idx));
                }
            }
            doc_ids.insert(format!("{}-record", slug(&cid)));
            let existing_doc_ids: BTreeSet<String> = doc_ids
                .into_iter()
                .filter(|d| live.docs.contains_key(d))
                .collect();

            let wires = wires_from_snapshot(snapshot.as_ref(), Some(&existing_doc_ids));
            let docs_md = gather_docs_markdown(app, graph_id, &existing_doc_ids)?;
            let provenance = docs_provenance(
                app,
                graph_id,
                &existing_doc_ids.iter().cloned().collect::<Vec<_>>(),
            )?;

            plan_campaign_compute(
                contract,
                campaign,
                &campaign_json,
                &live,
                &triples,
                &wires,
                &docs_md,
                &provenance,
            )
            .map_err(plan_error_to_app)?
        }
        // Handled by the early-return memory branch above (different contract +
        // reads); unreachable here.
        IngestPayload::Memory { .. } => unreachable!("memory payload handled above"),
        // Handled by the early-return generic branch above.
        IngestPayload::Generic { .. } => unreachable!("generic payload handled above"),
    };

    Ok(Planned { plan, contract })
}

// ---------------------------------------------------------------------------
// Apply + assert — the write path (P3-WP3).
// ---------------------------------------------------------------------------

/// Apply `plan` (the loud-halt CRDT applier — writes via the cell CRDT surface +
/// `GRAPH <{root}:user:rdf>`), then — for the **workflow** kind only — fold a
/// post-apply integrity assertion into the report. Port of
/// `spine.py::apply_and_assert`.
///
/// The applier itself never asserts: it returns `report.assertion == None`. This
/// is the seam where the spine re-surveys the *freshly written* state through the
/// SAME read helpers `gather_and_plan` uses (`current_wf_triples`, the workspace
/// snapshot, `survey_live` to learn the minted doc/node ids, `docs_provenance`),
/// then runs the PURE [`assert_workflow`] over that snapshot and attaches the
/// resulting [`AssertReport`].
///
/// 🔴 READ-GRAPH == WRITE-GRAPH: the re-survey reads the same user:rdf graph the
/// applier wrote, so the assertion sees exactly what was minted. A converged
/// re-apply (all-zero summary) still re-asserts → `assertion.pass == true`.
///
/// Campaign assert is deferred (textBlock placeholders need the capture pass,
/// same as the platform source), so the campaign kind sets `assertion = None`
/// (serialized as JSON `null`).
pub(crate) async fn apply_and_assert(
    app: &AppHandle,
    graph_id: &str,
    plan: &Plan,
    contract: &'static VocabularyContract,
    request: &IngestRequest,
) -> ApplyReport {
    // ── apply-dispatch fork (the single behavioral fork between the two write
    // sinks). Route on the plan's MODE: the wf/campaign path runs the UNCHANGED
    // user:rdf applier; a memory plan (mode "memory", set by plan_memory_compute)
    // runs the direct-on-store materializer that writes `:projection:memory`.
    //
    // NB: route on `mode` (via [`Plan::routes_to_memory_sink`]), NOT on
    // `plan.vocab.starts_with("mem")`. The ratified pack name is
    // "sophia-memory-core" (and `plan.vocab = contract.name`), which does NOT start
    // with "mem" — so the old gate silently fell through to the user:rdf applier,
    // leaking typed memory into `:user:rdf` and never populating
    // `:projection:memory` (so the survey saw no live heads → supersession never
    // fired). The request-level `vocab.starts_with("mem")` gate still works because
    // the REQUEST vocab is the literal "memory"; only the PLAN's vocab is the name.
    if plan.routes_to_memory_sink() {
        let mut report = apply_memory_plan(app, graph_id, plan).await;
        journal_applied_on_success(
            app,
            graph_id,
            plan,
            contract,
            request.replace_class,
            &mut report,
        );
        // ── the MEMORY EVENT LOG (§6): append the accepted INTENT ──
        // The applied-plan journal above records HOW (the rendered plan); the
        // event log records WHAT (the records as filed + the plan's clock), so
        // the projection is re-derivable through the pure planner. Same
        // inside-the-write-gate seam, same loud-on-lag posture.
        if report.ok {
            if let IngestPayload::Memory { records } = &request.payload {
                if let Err(error) = crate::emporium::memory_events::append_memory_event(
                    app,
                    graph_id,
                    plan.planned_at_ms,
                    &plan.observer,
                    records,
                ) {
                    let warning = format!(
                        "memory event log lagged (the write landed; the log did not): {error}"
                    );
                    log::error!("memory_event_append_failed graph={graph_id} error={error}");
                    report.warnings.push(warning);
                }
            }
        }
        return report;
    }

    // ── GENERIC simple-projection fork (EA-3 / B4) ──
    // A `simple-projection` plan reconciles its desired triples into the vocab's
    // `write_target` projection sink via the EA-1 `reconcile_class_validated`
    // primitive (survey → diff → SHACL-gate → apply; zero ops on a converged
    // re-ingest). Keyed on MODE, not vocab — so a new projection vocab needs no
    // dispatch edit (the table-driven registry + the mode IS the wiring).
    if plan.routes_to_simple_projection() {
        let mut report =
            apply_simple_projection_plan(app, graph_id, plan, contract, request.replace_class);
        journal_applied_on_success(
            app,
            graph_id,
            plan,
            contract,
            request.replace_class,
            &mut report,
        );
        return report;
    }

    let mut report = apply_plan(app, graph_id, plan, contract).await;

    // A loud halt never reaches the assertion stage — the report already names
    // the failed step (`ok=false`, `haltedAt`) and `assertion` stays `None`.
    if !report.ok {
        return report;
    }

    if let IngestPayload::Workflow { .. } = &request.payload {
        match assert_after_apply(app, graph_id, plan, contract) {
            Ok(assertion) => report.assertion = Some(assertion),
            // The re-survey is a read against the just-written cell; a read
            // failure here is a genuine fault, not a halted plan step. Surface it
            // by flipping `ok` and recording the cause as a warning rather than
            // silently dropping the assertion.
            Err(error) => {
                report.ok = false;
                report.warnings.push(format!(
                    "post-apply assertion survey failed: {}",
                    error.message_ref()
                ));
            }
        }
    }
    // Campaign kind: assertion stays `None` (deferred), serialized as `null`.

    report
}

/// Apply a memory plan via the direct-on-store memory materializer (the SECOND
/// write sink). Mirrors `apply_plan`'s loud-halt report shape but writes into
/// `:projection:memory` (NOT user:rdf) and runs `Step::CreateFolder` via the same
/// CRDT enqueue surface. On the FIRST error it LOUD-HALTS: journals the batch +
/// `haltedAt` + error to `memory/failures/` AND returns the 422-shaped report.
/// There is NO non-fatal path (the deliberate inversion of emporium's
/// fire-and-forget provenance posture).
pub(crate) async fn apply_memory_plan(app: &AppHandle, graph_id: &str, plan: &Plan) -> ApplyReport {
    let mut steps: Vec<StepReport> = Vec::new();

    // Resolve the per-graph store once (the direct-on-store sink). A resolution
    // failure is itself a loud halt before any step.
    let store = match open_memory_store(app, graph_id) {
        Ok(store) => store,
        Err(error) => {
            journal_memory_failure(app, graph_id, plan, None, &error);
            return memory_halt_report(plan, steps, 0, &format!("open memory store: {error}"));
        }
    };

    // ── EA-6: the per-graph SHACL VALIDATION GATE ──
    // Before ANY write lands, validate the plan's desired-insert triples against
    // the memory-core contract and dispatch on the per-graph ValidationPolicy:
    //   - Off            → no validation (legacy; byte-identical).
    //   - clean write    → proceed.
    //   - Halt + bad     → REJECT here, returning the STRUCTURED violations as
    //                      agent-actionable feedback (nothing is written).
    //   - FlagAndAccept  → record the violations to the ledger MO
    //                      (:projection:violations) direct-on-store, then proceed
    //                      (the write still lands).
    // The policy read defaults to the safe loud-halt for a legacy graph.json.
    let policy = read_graph_record(app, graph_id)
        .map(|(_, record)| record.validation_policy)
        .unwrap_or_default();
    let observed_at_ms = chrono::Utc::now().timestamp_millis();
    let validation_contract = memory_validation_contract(plan);
    match validate_memory_write(
        &store,
        graph_id,
        policy,
        validation_contract,
        &plan.desired_inserts,
        &plan.memory_graph_iri(graph_id),
        observed_at_ms,
    ) {
        Ok(ValidationGate::Proceed { .. }) => {
            // Clean (or Off, or flagged-and-recorded): fall through and apply.
        }
        Ok(ValidationGate::Halt { violations }) => {
            // The loud-halt-AS-FEEDBACK path: reject the write, carrying the
            // structured (agent-repairable) violations + the derived halt string.
            // Journal the failure like every OTHER halt (this arm was the one gap —
            // spine.rs §3.3): the violations ARE the failure, and the durable
            // `memory/failures/` record must carry them (halt string encodes each
            // violation's focus/path/value/shape/message).
            let halt_string = violations_to_halt_string(validation_contract, &violations);
            journal_memory_failure(app, graph_id, plan, Some(0), &halt_string);
            return memory_validation_halt_report(plan, steps, validation_contract, &violations);
        }
        Err(error) => {
            // A gate apparatus fault (e.g. the FlagAndAccept ledger append failed)
            // is a loud halt — the gate must not silently let a write through.
            let error = format!("memory validation gate: {error}");
            journal_memory_failure(app, graph_id, plan, None, &error);
            return memory_halt_report(plan, steps, 0, &error);
        }
    }

    for (index, step) in plan.steps.iter().enumerate() {
        let op = step.op_name();
        match step {
            Step::CreateFolder {
                folder_id,
                label,
                parent_id,
            } => {
                let payload = serde_json::json!({
                    "folderId": folder_id,
                    "name": label,
                    "parentId": parent_id,
                });
                if let Err(error) = enqueue_workspace_op(app, graph_id, payload).await {
                    journal_memory_failure(app, graph_id, plan, Some(index), &error);
                    return memory_halt_report(plan, steps, index, &error);
                }
            }
            Step::SparqlUpdate { update } => {
                // Wrap into the SAME per-observer projection graph the planner
                // minted the subjects under (Variant B) — `plan.observer` is the
                // single source of truth (empty ⇒ the shared commons graph).
                if let Err(error) =
                    run_memory_update(&store, &plan.memory_graph_iri(graph_id), update)
                {
                    journal_memory_failure(app, graph_id, plan, Some(index), &error);
                    return memory_halt_report(plan, steps, index, &error);
                }
            }
            // The memory planner never emits the doc/wire/move/rename verbs.
            other => {
                let error = format!("memory plan emitted unsupported step: {}", other.op_name());
                journal_memory_failure(app, graph_id, plan, Some(index), &error);
                return memory_halt_report(plan, steps, index, &error);
            }
        }
        let mut entry = serde_json::Map::new();
        entry.insert("ok".into(), serde_json::json!(true));
        steps.push(StepReport {
            op: op.to_string(),
            extra: entry,
        });
    }

    ApplyReport {
        graph: plan.graph.clone(),
        workflow: plan.workflow.clone(),
        mode: plan.mode.clone(),
        summary: serde_json::to_value(&plan.summary).unwrap_or(Json::Null),
        warnings: plan.warnings.clone(),
        steps,
        ok: true,
        halted_at: None,
        script_block: None,
        assertion: None,
    }
}

fn memory_validation_contract(plan: &Plan) -> &'static VocabularyContract {
    if plan.observer.is_empty() {
        memory_core_vocabulary()
    } else {
        agent_memory_validation_vocabulary()
    }
}

#[cfg(test)]
mod contract_selection_tests {
    use super::*;
    use crate::emporium::contract::{agent_memory_validation_vocabulary, memory_core_vocabulary};
    use crate::emporium::planner::PlanSummary;

    fn minimal_memory_plan(observer: &str) -> Plan {
        Plan {
            graph: "lab".to_string(),
            workflow: "memory".to_string(),
            vocab: "sophia-memory-core".to_string(),
            mode: "memory".to_string(),
            short_id: "memory".to_string(),
            workflow_doc_id: "memory".to_string(),
            steps: Vec::new(),
            summary: PlanSummary {
                folders: 0,
                doc_writes: 0,
                moves: 0,
                wires_create: 0,
                wires_delete: 0,
                rdf_delete: 0,
                rdf_insert: 0,
            },
            warnings: Vec::new(),
            desired_inserts: Vec::new(),
            observer: observer.to_string(),
            planned_at_ms: 0,
        }
    }

    #[test]
    fn wf4_memory_validation_contract_selects_agent_shapes_for_observed_plan() {
        let commons = minimal_memory_plan("");
        assert!(
            std::ptr::eq(
                memory_validation_contract(&commons),
                memory_core_vocabulary()
            ),
            "shared commons memory stays on the memory-core contract"
        );

        let observed = minimal_memory_plan("agent-0123456789abcdef");
        assert!(
            std::ptr::eq(
                memory_validation_contract(&observed),
                agent_memory_validation_vocabulary()
            ),
            "agent-observed memory selects the merged memory+agt validation contract"
        );
    }
}

/// Apply a GENERIC simple-projection plan (EA-3 / B4) via the EA-1 reconcile
/// primitive. The SECOND new write sink: the plan's `desired_inserts` are
/// reconciled into the vocab's `write_target` projection named graph
/// (`{graph_subject}:{write_target}`), partitioned by class, validated against the
/// vocab-derived SHACL shapes (the gate runs INSIDE `reconcile_classes_validated`,
/// after the diff, before any write — a loud halt on a conformance failure, no
/// partial write), and applied as a single delta. A converged re-ingest produces
/// zero ops (the convergence the oracle proves).
///
/// APPLY SEMANTICS (`replace_class`): the default (`false`) is SUBJECT-SCOPED
/// UPSERT — each class reconcile is restricted to the subjects present in this
/// batch, so a partial batch updates its own records and class siblings are
/// untouched. `replace_class=true` is the explicit destructive form: the batch
/// is the ENTIRE desired state of every class it mentions, and absent siblings
/// are reclaimed (the old implicit behavior, now opt-in — a partial batch under
/// the old default silently deleted every sibling record with ok:true).
///
/// CONSISTENCY TRAP: `plan.mode == "simple-projection"` and the contract's
/// `write_target` MUST agree. If the contract has no `projection:*` write_target
/// the apply LOUD-HALTS BEFORE any write — never silently falls through to the
/// user:rdf applier (the exact class of bug that once leaked memory into
/// `:user:rdf`).
fn apply_simple_projection_plan(
    app: &AppHandle,
    graph_id: &str,
    plan: &Plan,
    _contract: &VocabularyContract,
    replace_class: bool,
) -> ApplyReport {
    use crate::emporium::chamber_ontology::resolve_ingest_contract;
    use crate::emporium::reconcile::reconcile_classes_validated;
    use crate::rdf::graph_subject;

    let store = match open_memory_store(app, graph_id) {
        Ok(store) => store,
        Err(error) => {
            return simple_projection_halt_report(plan, &format!("open projection store: {error}"))
        }
    };

    // ── TWO-TIER contract resolution (EA-3 §4): re-resolve the contract from
    // `plan.vocab` against the SAME store, so a CHAMBER instance is validated
    // against the agent's OWN proposed shapes (not the placeholder passed in).
    // Embedded vocabs resolve to themselves (byte-identical to the old path); a
    // chamber vocab resolves to its in-graph proposed contract. A genuinely-unknown
    // vocab (neither tier) is a loud halt BEFORE any write.
    let contract = match resolve_ingest_contract(&store, graph_id, &plan.vocab) {
        Ok(c) => c,
        Err(error) => {
            return simple_projection_halt_report(
                plan,
                &format!("resolve ingest contract: {error}"),
            )
        }
    };
    let contract = &contract;

    // ── consistency trap: mode ⟺ write_target ──
    let write_target = match contract.write_target.as_deref() {
        Some(t) if t.starts_with("projection:") => t,
        other => {
            let error = format!(
                "simple-projection apply: vocab '{}' write_target {:?} is not a 'projection:*' sink \
                 (mode/write_target mismatch — refusing to apply)",
                contract.name, other
            );
            return simple_projection_halt_report(plan, &error);
        }
    };
    // The sink named graph: `{graph_subject}:{write_target}` (a reserved
    // `:projection:*` graph the authority gate bans from the user path — direct-on-
    // store is the lawful materializer path, same as memory/salience/song). The
    // store was opened above (for contract resolution); reuse it.
    let sink_iri = format!("{}:{}", graph_subject(graph_id), write_target);

    let partitions = match materialized_class_partitions(
        contract,
        &sink_iri,
        &plan.desired_inserts,
        replace_class,
    ) {
        Ok(partitions) => partitions,
        Err(error) => return simple_projection_halt_report(plan, &error),
    };

    // ── reconcile (survey → diff → SHACL-gate → apply), TIMED ──
    // The deterministic CHAMBER instrumentation (EA-3 §4, the metrics Vera scoped):
    // `materializationLatencyMs` wraps the whole survey→diff→SHACL→apply pass (the
    // SHACL validation latency is a sub-interval of it, not separately split out in
    // v1 — the reconcile primitive owns the gate). The answer-correctness oracle +
    // the free-choice-agent A/B confounder are explicitly NOT this build's job.
    let materialization_started = std::time::Instant::now();
    let diff =
        match reconcile_classes_validated(&store, &partitions.scopes_desireds, Some(contract)) {
            Ok(diff) => diff,
            Err(error) => {
                // A SHACL loud-halt (`"SHACL: …"`) or a store error: reject the whole
                // ingest, NOTHING written (reconcile validates before apply). The
                // structured detail is the agent-actionable feedback.
                return simple_projection_halt_report(plan, &error);
            }
        };
    let materialization_latency_ms = materialization_started.elapsed().as_millis() as u64;

    // Reflect the ACTUAL applied delta (zero on a converged re-ingest) in the
    // summary, so the report's counts are store-truth, not just the plan estimate.
    let mut summary = serde_json::to_value(&plan.summary).unwrap_or(Json::Null);
    if let Some(obj) = summary.as_object_mut() {
        obj.insert("rdfInsert".into(), serde_json::json!(diff.adds.len()));
        obj.insert("rdfDelete".into(), serde_json::json!(diff.removes.len()));
    }
    let mut entry = serde_json::Map::new();
    entry.insert("ok".into(), serde_json::json!(true));
    entry.insert("rdfInsert".into(), serde_json::json!(diff.adds.len()));
    entry.insert("rdfDelete".into(), serde_json::json!(diff.removes.len()));
    entry.insert("sink".into(), serde_json::json!(sink_iri));
    // ── deterministic instrumentation (the build-the-instrument, not-the-
    // measurement scope): which ontology validated, how long it took, how many
    // instances passed (a clean apply = 0 schema violations — a violation is a
    // loud halt above, never a successful apply). `result_count` = the desired
    // triples that passed validation and were reconciled into the sink.
    entry.insert(
        "ontologyUsed".into(),
        serde_json::json!(format!("{}@{}", contract.name, contract.version)),
    );
    entry.insert(
        "materializationLatencyMs".into(),
        serde_json::json!(materialization_latency_ms),
    );
    entry.insert("schemaViolationCount".into(), serde_json::json!(0));
    entry.insert(
        "resultCount".into(),
        serde_json::json!(partitions.materialized_triple_count),
    );
    entry.insert(
        "virtualClassCount".into(),
        serde_json::json!(partitions.virtual_class_count),
    );
    entry.insert(
        "virtualTripleCount".into(),
        serde_json::json!(partitions.virtual_triple_count),
    );

    ApplyReport {
        graph: plan.graph.clone(),
        workflow: plan.workflow.clone(),
        mode: plan.mode.clone(),
        summary,
        warnings: plan.warnings.clone(),
        steps: vec![StepReport {
            op: "simple_projection_reconcile".to_string(),
            extra: entry,
        }],
        ok: true,
        halted_at: None,
        script_block: None,
        assertion: None,
    }
}

fn materialized_class_partitions(
    contract: &VocabularyContract,
    sink_iri: &str,
    desired_inserts: &[crate::emporium::terms::Triple],
    replace_class: bool,
) -> Result<MaterializedClassPartitions, String> {
    use crate::runtime_config::RDF_TYPE;

    // Each scope surveys + reconciles ONLY its own materialized class's subjects
    // in the sink, so a sister class's triples never read as spurious ADDs. A
    // virtual class may be declared in the contract and even appear in an
    // internally-constructed desired set; it is deliberately omitted from storage.
    //
    // `replace_class=false` (the default) additionally restricts each class
    // scope to THE BATCH'S OWN SUBJECTS — subject-scoped upsert. Only the
    // explicit `replace_class=true` ingest widens the scope to the whole class
    // span, where absent siblings read as removes.
    let primary_ns = contract.primary_namespace();
    let mut scopes_desireds: Vec<(ClassScope, Vec<crate::emporium::terms::Triple>)> = Vec::new();
    let mut virtual_triple_count = 0usize;
    let mut virtual_class_count = 0usize;
    for (class_name, spec) in &contract.classes {
        // The class's declared signature resolved to its dispatch route (T5.22):
        // reads `store_mode`/`store_target` through the shared class_dispatch
        // module rather than a raw field comparison, so this fork and the
        // memory/generic-planner forks share one source of truth for "what does
        // this class's declaration mean".
        let dispatch = class_dispatch::resolve(contract, class_name)?;
        // The class-discriminating rdf:type is the one in the primary namespace
        // (matches shacl_emit's target selection + survey_class's key).
        let Some(type_uri) = spec
            .rdf_types
            .iter()
            .filter_map(|t| contract.expand(t).ok())
            .find(|uri| uri.starts_with(primary_ns))
        else {
            continue; // no targetable type -> never reconciled as a class span.
        };
        let type_nt = format!("<{type_uri}>");
        let subjects: BTreeSet<&str> = desired_inserts
            .iter()
            .filter(|(_, p, o)| p == RDF_TYPE && o.as_nt() == type_nt)
            .map(|(s, _, _)| s.as_str())
            .collect();
        if subjects.is_empty() {
            continue;
        }
        let subset: Vec<_> = desired_inserts
            .iter()
            .filter(|(s, _, _)| subjects.contains(s.as_str()))
            .cloned()
            .collect();
        if dispatch.route == class_dispatch::DispatchRoute::VirtualSkip {
            virtual_class_count += 1;
            virtual_triple_count += subset.len();
            continue;
        }
        let scope = ClassScope {
            placement: Placement::Named(sink_iri.to_string()),
            key: SpanKey::Fixed {
                rdf_type: type_uri.clone(),
            },
            graph_id_conjunct: None,
            subjects: if replace_class {
                None
            } else {
                Some(subjects.iter().map(|s| s.to_string()).collect())
            },
        };
        scopes_desireds.push((scope, subset));
    }
    let materialized_triple_count = scopes_desireds
        .iter()
        .map(|(_, desired)| desired.len())
        .sum();

    Ok(MaterializedClassPartitions {
        scopes_desireds,
        materialized_triple_count,
        virtual_triple_count,
        virtual_class_count,
    })
}

#[cfg(test)]
mod class_dispatch_tests {
    use super::*;
    use crate::emporium::terms::{Term, Triple};
    use oxigraph::model::NamedNode;
    use serde_json::json;

    const RDF_TYPE: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";

    fn uri(value: &str) -> Term {
        Term::Uri(NamedNode::new(value).expect("test URI is valid"))
    }

    #[test]
    fn simple_projection_partitions_materialized_classes_and_skips_virtual_classes() {
        let contract: VocabularyContract = serde_json::from_value(json!({
            "name": "demo",
            "version": "1.0.0",
            "title": "Demo",
            "description": "demo",
            "namespaces": {
                "demo": "http://example.test/demo#",
                "rdf": "http://www.w3.org/1999/02/22-rdf-syntax-ns#",
                "xsd": "http://www.w3.org/2001/XMLSchema#"
            },
            "primary_prefix": "demo",
            "write_target": "projection:demo",
            "classes": {
                "Materialized": {
                    "rdf_types": ["demo:Materialized"],
                    "subject_rule": "{graph_subject}:projection:demo:{localId}",
                    "source_kind": "current-state",
                    "identity_kind": "urn-template",
                    "store_mode": "materialize",
                    "store_target": "projection:demo",
                    "enforcement": "halt",
                    "reconciliation_strategy": "codeBacked",
                    "dispatch_mode": "current-state-materialize",
                    "predicates": {
                        "demo:label": {"datatype": "string", "required": false, "multi": false}
                    }
                },
                "LiveOpinion": {
                    "rdf_types": ["demo:LiveOpinion"],
                    "subject_rule": "virtual:resolve-by-query:demo.liveOpinion",
                    "source_kind": "derived",
                    "identity_kind": "resolve-by-query",
                    "store_mode": "virtual",
                    "store_target": "none",
                    "enforcement": "warning",
                    "reconciliation_strategy": "codeBacked",
                    "dispatch_mode": "derived-virtual",
                    "derived_from_query": "demo.liveOpinion(current graph slice)",
                    "predicates": {
                        "demo:rationale": {"datatype": "string", "required": true, "multi": false}
                    }
                }
            }
        }))
        .expect("demo contract parses");

        let desired: Vec<Triple> = vec![
            (
                "urn:test:materialized".to_string(),
                RDF_TYPE.to_string(),
                uri("http://example.test/demo#Materialized"),
            ),
            (
                "urn:test:materialized".to_string(),
                "http://example.test/demo#label".to_string(),
                Term::Lit(oxigraph::model::Literal::new_simple_literal("stored")),
            ),
            (
                "urn:test:virtual".to_string(),
                RDF_TYPE.to_string(),
                uri("http://example.test/demo#LiveOpinion"),
            ),
            (
                "urn:test:virtual".to_string(),
                "http://example.test/demo#rationale".to_string(),
                Term::Lit(oxigraph::model::Literal::new_simple_literal("read-derived")),
            ),
        ];

        let partitions =
            materialized_class_partitions(&contract, "urn:test:projection", &desired, false)
                .expect("partition succeeds");
        assert_eq!(partitions.scopes_desireds.len(), 1);
        assert_eq!(partitions.materialized_triple_count, 2);
        assert_eq!(partitions.virtual_class_count, 1);
        assert_eq!(partitions.virtual_triple_count, 2);
        let (scope, subset) = &partitions.scopes_desireds[0];
        assert_eq!(subset.len(), 2);
        assert!(subset
            .iter()
            .all(|(subject, _, _)| subject == "urn:test:materialized"));
        let SpanKey::Fixed { rdf_type } = &scope.key;
        assert_eq!(rdf_type, "http://example.test/demo#Materialized");
        // Default (subject-scoped upsert): the scope is pinned to the batch's
        // own subjects, so class siblings never enter the survey.
        assert_eq!(
            scope.subjects,
            Some(
                ["urn:test:materialized".to_string()]
                    .into_iter()
                    .collect::<std::collections::BTreeSet<_>>()
            ),
            "replace_class=false restricts the scope to the batch's subjects"
        );

        // Explicit replace_class=true: the scope widens to the WHOLE class span
        // (absent siblings read as removes — the destructive, opt-in form).
        let partitions =
            materialized_class_partitions(&contract, "urn:test:projection", &desired, true)
                .expect("partition succeeds");
        let (scope, _) = &partitions.scopes_desireds[0];
        assert_eq!(
            scope.subjects, None,
            "replace_class=true widens to the whole class span"
        );
    }

    /// LOAD-BEARING PROOF for THIS fork specifically (T5.22): the SAME class
    /// name, the SAME desired triples, only the contract's declared
    /// `store_mode` (+ the cross-field-consistent signature it requires)
    /// differing — and the partition outcome flips between "materialized"
    /// and "skipped as virtual". Nothing in `materialized_class_partitions`
    /// branches on the class's NAME; it goes through `class_dispatch::resolve`
    /// and the declaration alone decides.
    #[test]
    fn flipping_the_declared_store_mode_moves_a_class_between_materialize_and_virtual() {
        fn contract_with_mode(store_mode: &str) -> VocabularyContract {
            let (identity_kind, source_kind, dispatch_mode, extra) = if store_mode == "virtual" {
                (
                    "resolve-by-query",
                    "derived",
                    "derived-virtual",
                    json!({"derived_from_query": "demo.widget(test slice)"}),
                )
            } else {
                (
                    "urn-template",
                    "current-state",
                    "current-state-materialize",
                    json!({}),
                )
            };
            let mut class = json!({
                "rdf_types": ["demo:Widget"],
                "subject_rule": "{graph_subject}:projection:demo:{localId}",
                "source_kind": source_kind,
                "identity_kind": identity_kind,
                "store_mode": store_mode,
                "store_target": if store_mode == "virtual" { "none" } else { "projection:demo" },
                "enforcement": "halt",
                "reconciliation_strategy": "codeBacked",
                "dispatch_mode": dispatch_mode,
                "predicates": {
                    "demo:label": {"datatype": "string", "required": false}
                }
            });
            class
                .as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            serde_json::from_value(json!({
                "name": "demo-partitions",
                "version": "1.0.0",
                "title": "Demo Partitions",
                "description": "demo",
                "namespaces": {
                    "demo": "http://example.test/demo#",
                    "rdf": "http://www.w3.org/1999/02/22-rdf-syntax-ns#",
                    "xsd": "http://www.w3.org/2001/XMLSchema#"
                },
                "primary_prefix": "demo",
                "write_target": "projection:demo",
                "classes": { "Widget": class }
            }))
            .expect("contract parses")
        }

        let desired: Vec<Triple> = vec![(
            "urn:test:widget".to_string(),
            RDF_TYPE.to_string(),
            uri("http://example.test/demo#Widget"),
        )];

        let materialize_contract = contract_with_mode("materialize");
        let partitions = materialized_class_partitions(
            &materialize_contract,
            "urn:test:projection",
            &desired,
            false,
        )
        .expect("partition succeeds");
        assert_eq!(partitions.materialized_triple_count, 1);
        assert_eq!(partitions.virtual_class_count, 0);

        let virtual_contract = contract_with_mode("virtual");
        let partitions = materialized_class_partitions(
            &virtual_contract,
            "urn:test:projection",
            &desired,
            false,
        )
        .expect("partition succeeds");
        assert_eq!(
            partitions.materialized_triple_count, 0,
            "the SAME triple is no longer materialized once the declaration says virtual"
        );
        assert_eq!(partitions.virtual_class_count, 1);
        assert_eq!(partitions.virtual_triple_count, 1);
    }
}

/// Build the loud-halt `ApplyReport` for a simple-projection apply (the 422 shape).
/// Carries the (possibly SHACL-structured) error string; on a SHACL halt the
/// `"SHACL: …"` prefix names the conformance failure so the agent can repair.
fn simple_projection_halt_report(plan: &Plan, error: &str) -> ApplyReport {
    let mut entry = serde_json::Map::new();
    entry.insert("ok".into(), serde_json::json!(false));
    entry.insert(
        "error".into(),
        serde_json::json!(error.chars().take(800).collect::<String>()),
    );
    log::warn!(
        "emporium_simple_projection_halt graph={} vocab={} error={}",
        plan.graph,
        plan.vocab,
        error.chars().take(300).collect::<String>()
    );
    ApplyReport {
        graph: plan.graph.clone(),
        workflow: plan.workflow.clone(),
        mode: plan.mode.clone(),
        summary: serde_json::to_value(&plan.summary).unwrap_or(Json::Null),
        warnings: plan.warnings.clone(),
        steps: vec![StepReport {
            op: "simple_projection_reconcile".to_string(),
            extra: entry,
        }],
        ok: false,
        halted_at: Some(0),
        script_block: None,
        assertion: None,
    }
}

/// Build the VALIDATION-halt `ApplyReport` (EA-6, `ValidationPolicy::Halt`): the
/// write was rejected because the desired memory triples failed the memory-core
/// SHACL contract. Unlike a step fault, this carries the STRUCTURED violations
/// (`focusNode` / `path` / `value` / `shape` / `message` / `severity`) in the
/// step's `extra` map so the AGENT can repair and retry — not just a flat string.
/// `haltedAt = 0` (the gate runs before any step) and `ok = false`.
fn memory_validation_halt_report(
    plan: &Plan,
    mut steps: Vec<StepReport>,
    contract: &VocabularyContract,
    violations: &[ViolationRecord],
) -> ApplyReport {
    let halt_string = violations_to_halt_string(contract, violations);
    let mut entry = serde_json::Map::new();
    entry.insert("ok".into(), serde_json::json!(false));
    entry.insert(
        "validationContract".into(),
        serde_json::json!(contract.name),
    );
    entry.insert("error".into(), serde_json::json!(halt_string));
    // The agent-actionable, repairable detail — the whole point of loud-halt-as-
    // feedback. Each ViolationRecord serializes to its structured fields.
    entry.insert(
        "violations".into(),
        serde_json::to_value(violations).unwrap_or(Json::Null),
    );
    steps.push(StepReport {
        op: "memory_validate".to_string(),
        extra: entry,
    });
    log::warn!(
        "emporium_memory_validation_halt graph={} violations={} detail={}",
        plan.graph,
        violations.len(),
        halt_string.chars().take(300).collect::<String>()
    );
    ApplyReport {
        graph: plan.graph.clone(),
        workflow: plan.workflow.clone(),
        mode: plan.mode.clone(),
        summary: serde_json::to_value(&plan.summary).unwrap_or(Json::Null),
        warnings: plan.warnings.clone(),
        steps,
        ok: false,
        halted_at: Some(0),
        script_block: None,
        assertion: None,
    }
}

/// Build the loud-halt `ApplyReport` for a failed memory step (the 422 shape:
/// `ok=false`, `haltedAt`, the failed step recorded with its error).
fn memory_halt_report(
    plan: &Plan,
    mut steps: Vec<StepReport>,
    halted_at: usize,
    error: &str,
) -> ApplyReport {
    let mut entry = serde_json::Map::new();
    entry.insert("ok".into(), serde_json::json!(false));
    entry.insert(
        "error".into(),
        serde_json::json!(error.chars().take(800).collect::<String>()),
    );
    let op = plan
        .steps
        .get(halted_at)
        .map(|s| s.op_name())
        .unwrap_or("memory");
    steps.push(StepReport {
        op: op.to_string(),
        extra: entry,
    });
    log::error!(
        "emporium_memory_apply_halt step={halted_at} op={op} error={}",
        error.chars().take(300).collect::<String>()
    );
    ApplyReport {
        graph: plan.graph.clone(),
        workflow: plan.workflow.clone(),
        mode: plan.mode.clone(),
        summary: serde_json::to_value(&plan.summary).unwrap_or(Json::Null),
        warnings: plan.warnings.clone(),
        steps,
        ok: false,
        halted_at: Some(halted_at),
        script_block: None,
        assertion: None,
    }
}

/// Enqueue a `workspace.createFolder` CRDT op (the same surface the wf applier
/// uses for folder creation).
async fn enqueue_workspace_op(
    app: &AppHandle,
    graph_id: &str,
    payload: Json,
) -> Result<(), String> {
    use crate::crdt_operation_types::EnqueueCrdtOperationInput;
    use crate::crdt_queue::enqueue_crdt_operation;
    enqueue_crdt_operation(
        app.clone(),
        EnqueueCrdtOperationInput {
            kind: "workspace.createFolder".to_string(),
            graph_id: graph_id.to_string(),
            document_id: None,
            payload,
        },
    )
    .await
    .map(|_| ())
}

/// Journal a memory apply halt to `memory/failures/{ts}-{clientRef}.json` — the
/// durable failure lane (Caveat 2). Best-effort: a journal write failure is logged
/// but does not mask the original halt (which is already loud via the report).
fn journal_memory_failure(
    app: &AppHandle,
    graph_id: &str,
    plan: &Plan,
    halted_at: Option<usize>,
    error: &str,
) {
    let Ok(graph_dir) = existing_graph_dir(app, graph_id) else {
        log::error!("memory failure journal: cannot resolve graph dir for {graph_id}");
        return;
    };
    let dir = graph_dir.join("memory").join("failures");
    if let Err(e) = std::fs::create_dir_all(&dir) {
        log::error!("memory failure journal: create dir failed: {e}");
        return;
    }
    let ts = chrono::Utc::now().timestamp_millis();
    let client_ref: String = plan
        .workflow
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let path = dir.join(format!("{ts}-{client_ref}.json"));
    let body = serde_json::json!({
        "graphId": graph_id,
        "vocab": plan.vocab,
        "haltedAt": halted_at,
        "error": error,
        "plan": plan,
    });
    let bytes = serde_json::to_vec_pretty(&body).unwrap_or_default();
    if let Err(e) = crate::storage_file_ops::write_bytes(&path, &bytes) {
        log::error!("memory failure journal: write failed: {e}");
    }
}

/// On a SUCCESSFUL apply, append the applied-plan journal record — the recovery
/// source that makes the born-RDF sinks rebuildable (S3). Called for the memory +
/// simple-projection sinks (the direct-on-store born-RDF writers); the user:rdf
/// applier keeps its own provenance and is not journaled here.
///
/// LOUD on failure, never silent (mirrors the queue-append posture in
/// `geist_memory_service`): the projection has ALREADY committed by the time we
/// journal, so a journal-write failure means THIS apply is once again unrebuildable.
/// `apply_and_assert` returns an `ApplyReport` (not a `Result`), so the loud signal
/// is an error log + a report warning rather than a propagated `Err`; we do NOT flip
/// `ok` (the write is valid — only its recovery record lagged).
fn journal_applied_on_success(
    app: &AppHandle,
    graph_id: &str,
    plan: &Plan,
    contract: &VocabularyContract,
    replace_class: bool,
    report: &mut ApplyReport,
) {
    if !report.ok {
        return;
    }
    if let Err(e) = journal_applied_plan(app, graph_id, plan, contract, replace_class, report) {
        log::error!(
            "emporium_applied_journal_failed graph={graph_id} vocab={} mode={}: {e} \
             (projection committed but is UNREBUILDABLE for this apply)",
            plan.vocab,
            plan.mode
        );
        report.warnings.push(format!(
            "applied-plan journal write FAILED — this apply's projection is unrebuildable \
             from the journal (the store is again its only copy): {e}"
        ));
    }
}

/// Re-survey the freshly-written workflow and run the pure assertion suite.
///
/// Mirrors `spine.py::apply_and_assert` (workflow branch): gather fresh triples,
/// a fresh workspace snapshot, and `survey_live` to learn the minted doc/node
/// ids; union with the plan's `workflowDocId` and every `write_doc` target; then
/// pull the fresh wires / markdown / provenance for that doc-id set and call
/// [`assert_workflow`] with the workflow doc's markdown as the sha round-trip
/// anchor.
fn assert_after_apply(
    app: &AppHandle,
    graph_id: &str,
    plan: &Plan,
    contract: &'static VocabularyContract,
) -> AppResult<crate::emporium::asserts::AssertReport> {
    let wf_name = &plan.workflow;
    let wf_doc_id = &plan.workflow_doc_id;

    // Fresh reads against the just-written user:rdf graph + workspace snapshot.
    let fresh_triples = current_wf_triples(app, graph_id, contract)?;
    let snapshot = read_workspace_snapshot(app, graph_id)?;
    let live_after = survey_live(app, graph_id, contract)?;
    let live_after = hydrate_workspace_structure(live_after, snapshot.as_ref());

    // Re-survey to learn the freshly-written workflow's doc/node ids.
    let mut assert_doc_ids: BTreeSet<String> = BTreeSet::new();
    if let Some(existing) = live_after.workflows.get(wf_name) {
        if let Some(did) = &existing.doc_id {
            assert_doc_ids.insert(did.clone());
        }
        if let Some(nodes) = live_after.nodes_by_workflow.get(&existing.uri) {
            assert_doc_ids.extend(nodes.values().cloned());
        }
    }
    if !wf_doc_id.is_empty() {
        assert_doc_ids.insert(wf_doc_id.clone());
    }
    for step in &plan.steps {
        if let Step::WriteDoc { doc_id, .. } = step {
            assert_doc_ids.insert(doc_id.clone());
        }
    }

    // `wires_from_snapshot` filters to wires touching the set (or all when empty,
    // matching the Python `doc_ids=assert_doc_ids or None`).
    let wire_filter = if assert_doc_ids.is_empty() {
        None
    } else {
        Some(&assert_doc_ids)
    };
    let fresh_wires: Vec<AssertWire> = wires_from_snapshot(snapshot.as_ref(), wire_filter)
        .into_iter()
        .map(assert_wire_from_current)
        .collect();
    let assert_md = gather_docs_markdown(app, graph_id, &assert_doc_ids)?;
    let assert_prov = docs_provenance(
        app,
        graph_id,
        &assert_doc_ids.iter().cloned().collect::<Vec<_>>(),
    )?;

    Ok(assert_workflow(
        contract,
        wf_name,
        &fresh_triples,
        &fresh_wires,
        assert_md.get(wf_doc_id).map(String::as_str),
        Some(&assert_md),
        Some(&assert_prov),
    ))
}

/// Project a survey [`CurrentWire`] into the assertion suite's [`AssertWire`]
/// (the suite's fields are optional to mirror the Python `w.get(...)`).
fn assert_wire_from_current(w: CurrentWire) -> AssertWire {
    AssertWire {
        id: w.id,
        predicate: Some(w.predicate),
        source_document_id: Some(w.source_document_id),
        target_document_id: Some(w.target_document_id),
    }
}

// ---------------------------------------------------------------------------
// Workspace-snapshot reads — the gardend analog of the gateway get_workspace.
// ---------------------------------------------------------------------------

/// Read the cell workspace snapshot JSON for a graph, if one has been
/// materialized. `None` when there is no snapshot (a flat-document graph) — the
/// planner then sees empty folders/docs, exactly like the platform fallback.
fn read_workspace_snapshot(app: &AppHandle, graph_id: &str) -> AppResult<Option<Json>> {
    let graph_dir = existing_graph_dir(app, graph_id).map_err(AppError::storage)?;
    let path = workspace_snapshot_path(&graph_dir);
    if !path.is_file() {
        return Ok(None);
    }
    let snapshot: Json = read_json(&path).map_err(AppError::storage)?;
    Ok(Some(snapshot))
}

/// Fill `live.folders` / `live.docs` from the workspace snapshot's `folders` /
/// `documents` arrays. Mirrors `survey.py`'s folder/doc projection: a folder's
/// `label` comes from `name`/`title`, a doc's `folderId` from its `parentId`
/// (camel- or snake-case aliases). Entities with no resolvable id are dropped.
fn hydrate_workspace_structure(mut live: Live, snapshot: Option<&Json>) -> Live {
    use crate::emporium::survey::{DocEntry, FolderEntry};
    let Some(snapshot) = snapshot else {
        return live;
    };
    for f in array_of(snapshot, "folders") {
        let Some(id) = entity_id(f) else { continue };
        live.folders.insert(
            id,
            FolderEntry {
                label: entity_name(f),
                parent_id: entity_parent(f),
            },
        );
    }
    for d in array_of(snapshot, "documents") {
        let Some(id) = entity_id(d) else { continue };
        live.docs.insert(
            id,
            DocEntry {
                title: entity_name(d),
                folder_id: entity_parent(d),
            },
        );
    }
    live
}

/// Extract normalised [`CurrentWire`]s from a workspace snapshot. Mirrors
/// `survey.wires_from_snapshot`: drop `inverseOf` mirror projections; if
/// `doc_ids` is given, keep only wires touching that set on either endpoint.
fn wires_from_snapshot(
    snapshot: Option<&Json>,
    doc_ids: Option<&BTreeSet<String>>,
) -> Vec<CurrentWire> {
    let Some(snapshot) = snapshot else {
        return Vec::new();
    };
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut out: Vec<CurrentWire> = Vec::new();
    for w in array_of(snapshot, "wires") {
        let id = w.get("id").and_then(Json::as_str).filter(|s| !s.is_empty());
        let Some(id) = id else { continue };
        if !seen.insert(id.to_string()) {
            continue;
        }
        // Drop inverse-of projections (mirror image, not the authoritative dir).
        if w.get("inverseOf")
            .and_then(Json::as_str)
            .map(|s| !s.is_empty())
            .unwrap_or(false)
        {
            continue;
        }
        let src = w
            .get("sourceDocumentId")
            .and_then(Json::as_str)
            .unwrap_or("")
            .to_string();
        let tgt = w
            .get("targetDocumentId")
            .and_then(Json::as_str)
            .unwrap_or("")
            .to_string();
        if let Some(ids) = doc_ids {
            if !ids.contains(&src) && !ids.contains(&tgt) {
                continue;
            }
        }
        out.push(CurrentWire {
            id: id.to_string(),
            predicate: w
                .get("predicate")
                .and_then(Json::as_str)
                .unwrap_or("")
                .to_string(),
            source_document_id: src,
            target_document_id: tgt,
        });
    }
    out
}

/// Read each doc's canonical markdown (`document_markdown`) for the convergence
/// (rewrite-vs-skip) comparison. Mirrors `survey.docs_markdown` over the cell
/// `read_document` tool. A doc that cannot be read is simply omitted (the
/// planner treats a missing entry as "not converged" → rewrite).
fn gather_docs_markdown(
    app: &AppHandle,
    graph_id: &str,
    doc_ids: &BTreeSet<String>,
) -> AppResult<BTreeMap<String, String>> {
    let graph_dir = existing_graph_dir(app, graph_id).map_err(AppError::storage)?;
    let mut out: BTreeMap<String, String> = BTreeMap::new();
    for doc_id in doc_ids {
        let Ok(document_dir) = existing_document_dir(&graph_dir, doc_id) else {
            continue;
        };
        let manifest = document_dir.join("document.json");
        if !manifest.is_file() {
            continue;
        }
        match read_document_record(&graph_dir, &manifest) {
            Ok(record) => {
                out.insert(doc_id.clone(), document_markdown(&record));
            }
            Err(_) => continue,
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// snapshot entity helpers (camel/snake aliases — mirror survey.py)
// ---------------------------------------------------------------------------

fn array_of<'a>(snapshot: &'a Json, key: &str) -> &'a [Json] {
    snapshot
        .get(key)
        .and_then(Json::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

fn entity_id(entity: &Json) -> Option<String> {
    for key in ["id", "documentId", "document_id", "folderId", "folder_id"] {
        if let Some(v) = entity.get(key).and_then(Json::as_str) {
            if !v.is_empty() {
                return Some(v.to_string());
            }
        }
    }
    None
}

fn entity_name(entity: &Json) -> String {
    for key in ["name", "title"] {
        if let Some(v) = entity.get(key).and_then(Json::as_str) {
            return v.to_string();
        }
    }
    String::new()
}

fn entity_parent(entity: &Json) -> Option<String> {
    for key in ["parentId", "parent_id"] {
        if let Some(v) = entity.get(key).and_then(Json::as_str) {
            if !v.is_empty() {
                return Some(v.to_string());
            }
        }
    }
    None
}

/// Surface a [`PlanError`] as a validation [`AppError`] (a bad def is a 400, not
/// a 500). The planner raises on unminted predicates and invalid judgments.
fn plan_error_to_app(error: PlanError) -> AppError {
    AppError::validation(error.0)
}

#[cfg(all(test, feature = "headless"))]
mod tests;
