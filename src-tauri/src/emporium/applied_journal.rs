//! The APPLIED-PLAN JOURNAL — the recovery source for the born-RDF sinks (S3).
//!
//! The `:projection:memory` (and its sibling `:projection:*`) sinks are written
//! DIRECT-ON-STORE (see [`crate::emporium::memory_applier`]). Before this slice the
//! oxigraph store was their ONLY copy: the module docs claimed "DERIVED,
//! regenerable" but nothing derived them, and only FAILURES were journaled
//! (`memory/failures/`). This module adds the missing SUCCESS lane so those sinks
//! become genuinely rebuildable.
//!
//! SHAPE. After every successful apply (`spine::apply_and_assert`) a single
//! [`AppliedRecord`] is appended under `<graph_dir>/emporium/applied/`, one JSON
//! file per apply. The filename is `{seq:012}-{appliedAt}.json`: a zero-padded
//! MONOTONIC sequence (max existing seq + 1, computed under the per-graph write
//! gate the mutating callers already hold) makes the replay order UNAMBIGUOUS by
//! lexical filename sort, and the millisecond timestamp is an audit anchor. The
//! record carries everything needed to re-apply: `graph_id`, `vocab`, `mode`,
//! `observer`, the contract `name@version`, `replace_class`, the applied
//! insert/delete counts, and the FULL serialized [`Plan`] (the event — replay does
//! NOT re-plan).
//!
//! REBUILD. [`rebuild_memory_projection_from_journal`] reads the journal in order,
//! keeps the records whose plan routes to the memory sink, CLEARs the per-observer
//! `:projection:memory` named graph(s) direct-on-store (the lawful materializer
//! path), then re-applies each stored plan through the REAL
//! `spine::apply_memory_plan` — reproducing the projection set-for-set (tested).
//!
//! JOURNAL vs REPLAY, an honest limit. `Plan::desired_inserts` is `#[serde(skip)]`,
//! so the serialized plan does NOT carry the typed triples the SHACL gate
//! validates. That is fine for the MEMORY sink — its writes ride `Step::SparqlUpdate`
//! bodies (which ARE serialized), and the gate on replay validates an empty desired
//! set (conforms trivially; the events were already accepted). But it means the
//! GENERIC simple-projection sink is NOT rebuildable from this journal (its writes
//! ARE the desired-inserts, which the journal omits) — generic-sink rebuild is
//! deferred to the event-log SoR (S9), which must carry the validated triples. This
//! module still JOURNALS every successful simple-projection apply (so the record
//! exists for audit + future replay); it just cannot yet REPLAY them.

use std::collections::BTreeSet;

use serde_json::Value as Json;

use crate::app_runtime::AppHandle;
use crate::emporium::applier::ApplyReport;
use crate::emporium::contract::VocabularyContract;
use crate::emporium::planner::{Plan, PlanSummary, Step};
use crate::emporium::terms::Triple;
use crate::graph_paths::existing_graph_dir;

/// The contract a journaled apply was minted against — carried for provenance /
/// version-drift forensics (the replay itself re-resolves the contract from the
/// stored plan, so this is metadata, not load-bearing for rebuild).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct ContractRef {
    pub(crate) name: String,
    pub(crate) version: String,
}

/// One appended applied-plan record — the durable event of a single successful
/// apply. `Deserialize` (unlike [`Plan`] itself, which is serialize-only) so the
/// journal round-trips; the `plan` field holds the full serialized [`Plan`] as a
/// `serde_json::Value` and [`plan_from_value`] reconstructs a `Plan` for replay.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct AppliedRecord {
    /// Monotonic per-graph sequence — the unambiguous replay order.
    pub(crate) seq: u64,
    #[serde(rename = "appliedAt")]
    pub(crate) applied_at: i64,
    #[serde(rename = "graphId")]
    pub(crate) graph_id: String,
    pub(crate) vocab: String,
    /// The apply-dispatch mode — `"memory"` (this sink) / `"simple-projection"` /
    /// a wf rendering mode. The rebuild filters on this.
    pub(crate) mode: String,
    /// The per-observer witness (empty ⇒ the shared commons memory graph).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub(crate) observer: String,
    pub(crate) contract: ContractRef,
    #[serde(rename = "replaceClass")]
    pub(crate) replace_class: bool,
    #[serde(rename = "rdfInsert")]
    pub(crate) rdf_insert: u64,
    #[serde(rename = "rdfDelete")]
    pub(crate) rdf_delete: u64,
    /// The FULL serialized plan (the event). `desired_inserts` is absent
    /// (`#[serde(skip)]` on `Plan`) — see the module-level honest limit.
    pub(crate) plan: Json,
}

/// The result of a rebuild: how many journaled plans were replayed, and into which
/// sink (the distinct memory named graph[s] cleared + rewritten).
///
/// The rebuild surface (this + [`rebuild_memory_projection_from_journal`] +
/// [`read_applied_records`]) is the tested admin operation S3 delivers; S10 wires it
/// to a loopback/MCP route. Until then it is exercised only by the headless suites.
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct RebuildReport {
    pub(crate) replayed: usize,
    pub(crate) sink: String,
}

/// The applied-plan journal directory for a graph: `<graph_dir>/emporium/applied/`.
fn applied_dir(app: &AppHandle, graph_id: &str) -> Result<std::path::PathBuf, String> {
    let graph_dir = existing_graph_dir(app, graph_id)
        .map_err(|e| format!("applied journal: resolve graph dir for {graph_id}: {e}"))?;
    Ok(graph_dir.join("emporium").join("applied"))
}

/// The next monotonic sequence for this graph = (max existing seq in the dir) + 1.
/// Filenames are `{seq:012}-{ts}.json`; parse the leading zero-padded seq. Computed
/// under the per-graph write gate the mutating callers hold, so it is race-free on
/// the success path (one apply at a time per graph).
fn next_seq(dir: &std::path::Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut max_seq: Option<u64> = None;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(seq_str) = name.split('-').next() else {
            continue;
        };
        if let Ok(seq) = seq_str.parse::<u64>() {
            max_seq = Some(max_seq.map_or(seq, |m| m.max(seq)));
        }
    }
    max_seq.map_or(0, |m| m + 1)
}

/// A `u64` count read out of an `ApplyReport`'s serialized `summary` object (both
/// the memory and simple-projection appliers populate `rdfInsert`/`rdfDelete`
/// there — the memory path as the plan estimate, simple-projection as the actual
/// applied delta). Missing/absent ⇒ 0.
fn summary_count(report: &ApplyReport, key: &str) -> u64 {
    report.summary.get(key).and_then(Json::as_u64).unwrap_or(0)
}

/// Append one success record to the applied-plan journal. Returns `Err` (never
/// silent) so the caller can surface a LOUD warning: on the success path the
/// projection has ALREADY committed, so a journal-write failure means that apply is
/// once again unrebuildable and the caller must know its recovery record lagged.
pub(crate) fn journal_applied_plan(
    app: &AppHandle,
    graph_id: &str,
    plan: &Plan,
    contract: &VocabularyContract,
    replace_class: bool,
    report: &ApplyReport,
) -> Result<(), String> {
    let dir = applied_dir(app, graph_id)?;
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("applied journal: create dir failed: {e}"))?;
    let seq = next_seq(&dir);
    let applied_at = chrono::Utc::now().timestamp_millis();
    let record = AppliedRecord {
        seq,
        applied_at,
        graph_id: graph_id.to_string(),
        vocab: plan.vocab.clone(),
        mode: plan.mode.clone(),
        observer: plan.observer.clone(),
        contract: ContractRef {
            name: contract.name.to_string(),
            version: contract.version.to_string(),
        },
        replace_class,
        rdf_insert: summary_count(report, "rdfInsert"),
        rdf_delete: summary_count(report, "rdfDelete"),
        plan: serde_json::to_value(plan)
            .map_err(|e| format!("applied journal: serialize plan failed: {e}"))?,
    };
    let path = dir.join(format!("{seq:012}-{applied_at}.json"));
    let body = serde_json::to_vec_pretty(&record)
        .map_err(|e| format!("applied journal: serialize record failed: {e}"))?;
    crate::storage_file_ops::write_bytes(&path, &body)
        .map_err(|e| format!("applied journal: write failed: {e}"))
}

/// Read every applied-plan record for a graph, ordered by `seq` (the replay order).
/// A missing directory (no applies yet) reads as an empty log, not an error.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn read_applied_records(
    app: &AppHandle,
    graph_id: &str,
) -> Result<Vec<AppliedRecord>, String> {
    let dir = applied_dir(app, graph_id)?;
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("applied journal: read dir failed: {e}")),
    };
    let mut records: Vec<AppliedRecord> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let bytes = std::fs::read(&path)
            .map_err(|e| format!("applied journal: read {} failed: {e}", path.display()))?;
        let record: AppliedRecord = serde_json::from_slice(&bytes)
            .map_err(|e| format!("applied journal: parse {} failed: {e}", path.display()))?;
        records.push(record);
    }
    records.sort_by_key(|r| r.seq);
    Ok(records)
}

/// Reconstruct a [`Plan`] from a journaled plan `Value`. `Plan` is serialize-only
/// (and lives in `planner.rs`, which this slice does not touch), so we rebuild it
/// field-by-field from its own serialized shape. `desired_inserts` is absent from
/// the JSON (`#[serde(skip)]`) and reconstructs as EMPTY — sound for MEMORY replay
/// (the writes ride `steps`, and the SHACL gate conforms trivially on an empty
/// desired set for an already-accepted event). Callers that need the desired set
/// (generic simple-projection) cannot use this — see the module honest limit.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn plan_from_value(value: &Json) -> Result<Plan, String> {
    let obj = value
        .as_object()
        .ok_or_else(|| "applied journal: plan is not a JSON object".to_string())?;
    let s = |key: &str| -> Result<String, String> {
        obj.get(key)
            .and_then(Json::as_str)
            .map(str::to_string)
            .ok_or_else(|| format!("applied journal: plan missing string field '{key}'"))
    };
    let usize_of = |v: Option<&Json>| -> usize { v.and_then(Json::as_u64).unwrap_or(0) as usize };

    let summary_json = obj
        .get("summary")
        .and_then(Json::as_object)
        .ok_or_else(|| "applied journal: plan missing 'summary' object".to_string())?;
    let summary = PlanSummary {
        folders: usize_of(summary_json.get("folders")),
        doc_writes: usize_of(summary_json.get("docWrites")),
        moves: usize_of(summary_json.get("moves")),
        wires_create: usize_of(summary_json.get("wiresCreate")),
        wires_delete: usize_of(summary_json.get("wiresDelete")),
        rdf_delete: usize_of(summary_json.get("rdfDelete")),
        rdf_insert: usize_of(summary_json.get("rdfInsert")),
    };

    let steps_json = obj
        .get("steps")
        .and_then(Json::as_array)
        .ok_or_else(|| "applied journal: plan missing 'steps' array".to_string())?;
    let mut steps: Vec<Step> = Vec::with_capacity(steps_json.len());
    for step in steps_json {
        steps.push(step_from_value(step)?);
    }

    let warnings = obj
        .get("warnings")
        .and_then(Json::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|w| w.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();

    Ok(Plan {
        graph: s("graph")?,
        workflow: s("workflow")?,
        vocab: s("vocab")?,
        mode: s("mode")?,
        short_id: s("shortId")?,
        workflow_doc_id: s("workflowDocId")?,
        steps,
        summary,
        warnings,
        // Absent from the serialized plan (`#[serde(skip)]`); empty on replay.
        desired_inserts: Vec::<Triple>::new(),
        // Serialized only when non-empty (`skip_serializing_if`).
        observer: obj
            .get("observerAgentId")
            .and_then(Json::as_str)
            .unwrap_or("")
            .to_string(),
        // Serialized only when non-zero (`skip_serializing_if`).
        planned_at_ms: obj.get("plannedAtMs").and_then(Json::as_i64).unwrap_or(0),
    })
}

/// Reconstruct one [`Step`] from its serialized `{op, …}` form. The memory sink
/// only ever emits `create_folder` + `sparql_update`; the other verbs are
/// reconstructed too so the helper is total over the serialized grammar, EXCEPT
/// `create_wires` (whose `WireSpec` payload no memory/simple plan emits) which
/// loudly refuses rather than silently dropping wires.
#[cfg_attr(not(test), allow(dead_code))]
fn step_from_value(step: &Json) -> Result<Step, String> {
    let obj = step
        .as_object()
        .ok_or_else(|| "applied journal: step is not an object".to_string())?;
    let op = obj
        .get("op")
        .and_then(Json::as_str)
        .ok_or_else(|| "applied journal: step missing 'op'".to_string())?;
    let opt_str =
        |key: &str| -> Option<String> { obj.get(key).and_then(Json::as_str).map(str::to_string) };
    let req_str = |key: &str| -> Result<String, String> {
        opt_str(key).ok_or_else(|| format!("applied journal: step '{op}' missing '{key}'"))
    };
    match op {
        "create_folder" => Ok(Step::CreateFolder {
            folder_id: req_str("folderId")?,
            label: req_str("label")?,
            parent_id: opt_str("parentId"),
        }),
        "rename_folder" => Ok(Step::RenameFolder {
            folder_id: req_str("folderId")?,
            label: req_str("label")?,
        }),
        "write_doc" => Ok(Step::WriteDoc {
            doc_id: req_str("docId")?,
            content: req_str("content")?,
            capture_script_block: obj.get("captureScriptBlock").and_then(Json::as_bool),
            capture_block_var: opt_str("captureBlockVar"),
        }),
        "move" => Ok(Step::Move {
            doc_id: req_str("docId")?,
            folder_id: req_str("folderId")?,
        }),
        "delete_wires" => {
            let wire_ids = obj
                .get("wireIds")
                .and_then(Json::as_array)
                .map(|arr| {
                    arr.iter()
                        .filter_map(|w| w.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            Ok(Step::DeleteWires { wire_ids })
        }
        "sparql_update" => Ok(Step::SparqlUpdate {
            update: req_str("update")?,
        }),
        "create_wires" => Err(
            "applied journal: replay of 'create_wires' steps is unsupported (no memory / \
             simple-projection plan emits wires); refusing to silently drop them"
                .to_string(),
        ),
        other => Err(format!(
            "applied journal: unknown step op '{other}' in journaled plan"
        )),
    }
}

/// CLEAR one reserved `:projection:*` named graph direct-on-store — the lawful
/// materializer path (`SparqlEvaluator` on the per-graph store, the same surface
/// [`crate::emporium::memory_applier::run_memory_update`] uses). `CLEAR SILENT`
/// so an already-empty / absent graph is not an error.
#[cfg_attr(not(test), allow(dead_code))]
fn clear_named_graph(store: &oxigraph::store::Store, iri: &str) -> Result<(), String> {
    oxigraph::sparql::SparqlEvaluator::new()
        .parse_update(&format!("CLEAR SILENT GRAPH <{iri}>"))
        .map_err(|e| format!("applied journal: parse CLEAR failed: {e}"))?
        .on_store(store)
        .execute()
        .map_err(|e| format!("applied journal: execute CLEAR <{iri}> failed: {e}"))
}

/// Rebuild the `:projection:memory` sink for a graph from its applied-plan journal.
///
/// Reads the journal in order, keeps the records whose plan routes to the memory
/// sink (`mode == "memory"`), CLEARs the per-observer memory named graph(s) those
/// records wrote (direct-on-store, the lawful materializer path), then REPLAYS each
/// stored plan — in journal order — through the REAL `spine::apply_memory_plan`. It
/// does NOT re-plan: the journal is the event log and the stored plan is the event.
///
/// The per-graph write gate is held across CLEAR + replay so a concurrent memory
/// write cannot interleave. Replay drives `apply_memory_plan` DIRECTLY (not
/// `apply_and_assert`), so it does NOT append new journal records — the log is not
/// polluted by its own replay.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn rebuild_memory_projection_from_journal(
    app: &AppHandle,
    graph_id: &str,
) -> Result<RebuildReport, String> {
    let records = read_applied_records(app, graph_id)?;
    let mut plans: Vec<Plan> = Vec::new();
    for record in &records {
        if record.mode != "memory" {
            continue;
        }
        plans.push(plan_from_value(&record.plan)?);
    }

    // The distinct per-observer memory sinks these plans wrote (commons + any
    // `:agent:{id}` perspectives) — every one must be cleared before replay.
    let mut sinks: BTreeSet<String> = BTreeSet::new();
    for plan in &plans {
        sinks.insert(plan.memory_graph_iri(graph_id));
    }
    // Nothing to rebuild ⇒ the canonical commons sink label, replayed = 0.
    let sink_label = if sinks.is_empty() {
        crate::rdf_authority::memory_projection_graph_iri(graph_id)
    } else {
        sinks.iter().cloned().collect::<Vec<_>>().join(", ")
    };

    let store = crate::emporium::memory_applier::open_memory_store(app, graph_id)?;
    let replayed = crate::app_runtime::async_runtime::block_on(async {
        let _gate = crate::emporium::write_gate::acquire_write_gate(graph_id).await;
        for sink in &sinks {
            clear_named_graph(&store, sink)?;
        }
        for (i, plan) in plans.iter().enumerate() {
            let report = crate::emporium::spine::apply_memory_plan(app, graph_id, plan).await;
            if !report.ok {
                return Err(format!(
                    "applied journal: replay halted on plan #{i} (workflow={:?}, haltedAt={:?})",
                    plan.workflow, report.halted_at
                ));
            }
        }
        Ok::<usize, String>(plans.len())
    })?;

    Ok(RebuildReport {
        replayed,
        sink: sink_label,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal serialized memory plan value, mirroring `Plan`'s serde shape
    /// (`desired_inserts` deliberately absent, as the real skip produces).
    fn sample_plan_json(update: &str) -> Json {
        serde_json::json!({
            "graph": "lab",
            "workflow": "memory",
            "vocab": "sophia-memory-core",
            "mode": "memory",
            "shortId": "memory",
            "workflowDocId": "memory",
            "steps": [
                { "op": "create_folder", "folderId": "memory", "label": "Memory", "parentId": null },
                { "op": "sparql_update", "update": update }
            ],
            "summary": {
                "folders": 1, "docWrites": 0, "moves": 0,
                "wiresCreate": 0, "wiresDelete": 0, "rdfDelete": 0, "rdfInsert": 3
            },
            "warnings": []
        })
    }

    /// `plan_from_value` round-trips the load-bearing fields and rebuilds the step
    /// grammar (empty `desired_inserts`, as documented).
    #[test]
    fn plan_from_value_reconstructs_memory_plan() {
        let plan = plan_from_value(&sample_plan_json("INSERT DATA { <a> <b> <c> }"))
            .expect("reconstruct memory plan");
        assert_eq!(plan.mode, "memory");
        assert_eq!(plan.vocab, "sophia-memory-core");
        assert!(plan.routes_to_memory_sink());
        assert_eq!(plan.summary.rdf_insert, 3);
        assert!(
            plan.desired_inserts.is_empty(),
            "desired_inserts reconstructs empty"
        );
        assert_eq!(plan.steps.len(), 2);
        assert!(matches!(plan.steps[0], Step::CreateFolder { .. }));
        match &plan.steps[1] {
            Step::SparqlUpdate { update } => {
                assert!(update.starts_with("INSERT DATA"))
            }
            other => panic!("expected sparql_update, got {other:?}"),
        }
    }

    /// The observer round-trips (empty ⇒ commons; a named observer ⇒ carried).
    #[test]
    fn plan_from_value_carries_observer() {
        let mut value = sample_plan_json("INSERT DATA { <a> <b> <c> }");
        value["observerAgentId"] = serde_json::json!("agent-0123456789abcdef");
        let plan = plan_from_value(&value).expect("reconstruct");
        assert_eq!(plan.observer, "agent-0123456789abcdef");

        let commons = plan_from_value(&sample_plan_json("INSERT DATA { <a> <b> <c> }")).unwrap();
        assert_eq!(commons.observer, "", "absent observerAgentId ⇒ commons");
    }

    /// `create_wires` replay is a loud refusal, not a silent wire-drop.
    #[test]
    fn step_from_value_refuses_create_wires() {
        let step = serde_json::json!({ "op": "create_wires", "wires": [] });
        let err = step_from_value(&step).expect_err("create_wires must refuse");
        assert!(err.contains("create_wires"), "loud refusal: {err}");
    }

    /// An unknown step op is a loud error (no silent skip).
    #[test]
    fn step_from_value_rejects_unknown_op() {
        let step = serde_json::json!({ "op": "teleport" });
        assert!(step_from_value(&step).is_err());
    }
}
