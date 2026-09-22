//! The MEMORY EVENT LOG — born-RDF's §6 authority for the memory vocab.
//!
//! The whitepaper's move (`plans/meaningful-objects-whitepaper-20260620.md` §6):
//! *append events to a log, and make the RDF a projection of the log*. An event
//! here is the ACCEPTED INTENT — the typed records a witness filed plus the
//! clock the plan used — never the computed effect: replay re-derives every
//! demote and carry-forward through the SAME pure planner
//! ([`plan_memory_compute_at`]) with the SAME clock, so
//!
//! `project(events) == live :projection:memory`
//!
//! is a testable set-equality — THE disposability oracle. The triple store
//! earns its "projection cache" name the moment that oracle is green, because
//! the log can rebuild it from nothing.
//!
//! Posture: DUAL-WRITE. The live incremental path is unchanged (survey → plan →
//! apply, serialized by the per-graph write gate); the event append rides the
//! same success seam as the applied-plan journal, inside the gate, so the log
//! order is the apply order. The applied-plan journal (S3) remains the
//! apply-level audit; this log is the semantic layer above it — plans are how,
//! events are what.
//!
//! Contested heads: the fold deliberately carries EVERY witness's transition
//! (`supersededBy` accumulates, per the S1 carry-forward), so a fork replays as
//! a fork. What a fork MEANS is reader policy — §8.1 contested-by-default,
//! detected by [`crate::emporium::sweep`] — never resolved inside the fold.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;

use oxigraph::sparql::{QueryResults, SparqlEvaluator};
use oxigraph::store::Store;
use serde::{Deserialize, Serialize};

use crate::app_runtime::AppHandle;
use crate::emporium::contract::memory_core_vocabulary;
use crate::emporium::memory_applier::run_memory_update;
use crate::emporium::planner::{plan_memory_compute_at, Step};
use crate::emporium::schemas::MemoryRecordIn;
use crate::emporium::survey::{FolderEntry, Live};
use crate::emporium::terms::Triple;
use crate::paths::existing_graph_dir;
use crate::rdf_authority::memory_projection_graph_iri_for;

/// One accepted memory ingest: the witness, the records as filed, and the clock
/// the plan stamped (`Plan.planned_at_ms`) — everything replay needs, nothing
/// derived.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MemoryEventBatch {
    pub(crate) seq: u64,
    pub(crate) at_ms: i64,
    pub(crate) observer: String,
    pub(crate) records: Vec<MemoryRecordIn>,
}

/// `{graph_dir}/emporium/events/memory-events.jsonl` — one JSON line per
/// accepted batch, strictly append-only, ordered by `seq`.
fn events_path(app: &AppHandle, graph_id: &str) -> Result<PathBuf, String> {
    let graph_dir =
        existing_graph_dir(app, graph_id).map_err(|e| format!("resolve graph dir: {e}"))?;
    Ok(graph_dir
        .join("emporium")
        .join("events")
        .join("memory-events.jsonl"))
}

/// Append one accepted batch. The caller holds the per-graph write gate, so the
/// read-count → append pair is serialized (seq is dense and ordered).
pub(crate) fn append_memory_event(
    app: &AppHandle,
    graph_id: &str,
    at_ms: i64,
    observer: &str,
    records: &[MemoryRecordIn],
) -> Result<u64, String> {
    let path = events_path(app, graph_id)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create events dir: {e}"))?;
    }
    let seq = if path.exists() {
        BufReader::new(std::fs::File::open(&path).map_err(|e| format!("open events log: {e}"))?)
            .lines()
            .count() as u64
    } else {
        0
    };
    let batch = MemoryEventBatch {
        seq,
        at_ms,
        observer: observer.to_string(),
        records: records.to_vec(),
    };
    let line = serde_json::to_string(&batch).map_err(|e| format!("serialize memory event: {e}"))?;
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|e| format!("open events log for append: {e}"))?;
    writeln!(file, "{line}").map_err(|e| format!("append memory event: {e}"))?;
    Ok(seq)
}

/// Read the whole event log in order. Missing file = empty log (a graph that
/// never accepted a memory write).
pub(crate) fn read_memory_events(
    app: &AppHandle,
    graph_id: &str,
) -> Result<Vec<MemoryEventBatch>, String> {
    let path = events_path(app, graph_id)?;
    if !path.exists() {
        return Ok(Vec::new());
    }
    let file = std::fs::File::open(&path).map_err(|e| format!("open events log: {e}"))?;
    let mut out = Vec::new();
    for (i, line) in BufReader::new(file).lines().enumerate() {
        let line = line.map_err(|e| format!("read events log line {i}: {e}"))?;
        if line.trim().is_empty() {
            continue;
        }
        let batch: MemoryEventBatch =
            serde_json::from_str(&line).map_err(|e| format!("parse memory event line {i}: {e}"))?;
        out.push(batch);
    }
    Ok(out)
}

/// Survey one memory projection graph of the REPLAY store — the same query
/// shape as `survey::current_memory_triples`, run against the throwaway store
/// instead of the cell service.
fn survey_replay_store(store: &Store, mem_graph: &str) -> Result<Vec<Triple>, String> {
    let q = format!(
        "SELECT ?s ?p ?o WHERE {{\n  GRAPH <{mem_graph}> {{\n    ?s ?p ?o .\n    FILTER(STRSTARTS(STR(?s), \"{mem_graph}\"))\n  }}\n}}"
    );
    let solutions = match SparqlEvaluator::new()
        .parse_query(&q)
        .map_err(|e| format!("parse replay survey: {e}"))?
        .on_store(store)
        .execute()
        .map_err(|e| format!("execute replay survey: {e}"))?
    {
        QueryResults::Solutions(s) => s,
        _ => return Err("replay survey expected SELECT solutions".to_string()),
    };
    let mut out = Vec::new();
    for sol in solutions {
        let sol = sol.map_err(|e| format!("replay survey row: {e}"))?;
        let strip = |t: &oxigraph::model::Term| {
            let s = t.to_string();
            s.strip_prefix('<')
                .and_then(|x| x.strip_suffix('>'))
                .map(str::to_string)
                .unwrap_or(s)
        };
        let s = strip(sol.get("s").ok_or("replay row missing ?s")?);
        let p = strip(sol.get("p").ok_or("replay row missing ?p")?);
        let o = crate::emporium::survey::parse_term(
            &sol.get("o").ok_or("replay row missing ?o")?.to_string(),
        );
        out.push((s, p, o));
    }
    Ok(out)
}

/// A minimal `Live` whose workspace already has the memory registry folder, so
/// replay never re-emits the (workspace-side, non-projection) CreateFolder step.
fn replay_live(graph_id: &str) -> Live {
    let mut live = Live {
        graph: graph_id.to_string(),
        ..Live::default()
    };
    live.folders.insert(
        "memory".to_string(),
        FolderEntry {
            label: "Memory".to_string(),
            parent_id: None,
        },
    );
    live
}

/// Project the event log into a fresh in-memory store and return, per memory
/// projection graph (the commons plus every per-observer membrane the log
/// touches), the replayed triple set. Deterministic: every plan is recomputed
/// with its event's recorded clock through the same pure planner the live path
/// used, and every rendered update runs through the same `run_memory_update`.
pub(crate) fn project_memory_events(
    graph_id: &str,
    events: &[MemoryEventBatch],
) -> Result<BTreeMap<String, Vec<Triple>>, String> {
    let store = Store::new().map_err(|e| format!("open replay store: {e}"))?;
    let contract = memory_core_vocabulary();
    let mut graphs: BTreeSet<String> = BTreeSet::new();

    let mut last_seq: Option<u64> = None;
    for batch in events {
        if let Some(prev) = last_seq {
            if batch.seq <= prev {
                return Err(format!(
                    "event log out of order: seq {} after {}",
                    batch.seq, prev
                ));
            }
        }
        last_seq = Some(batch.seq);

        let mem_graph = memory_projection_graph_iri_for(graph_id, &batch.observer);
        graphs.insert(mem_graph.clone());
        let current = survey_replay_store(&store, &mem_graph)?;
        let live = replay_live(graph_id);
        let plan = plan_memory_compute_at(
            contract,
            graph_id,
            &batch.records,
            &live,
            &current,
            batch.at_ms,
        )
        .map_err(|e| format!("replay plan (seq {}): {}", batch.seq, e.0))?;
        for step in &plan.steps {
            match step {
                Step::SparqlUpdate { update } => {
                    run_memory_update(&store, &mem_graph, update)
                        .map_err(|e| format!("replay apply (seq {}): {e}", batch.seq))?;
                }
                // Workspace-side effects (the registry folder) are not
                // projection state; everything else the memory planner emits is
                // a SPARQL update.
                _ => {}
            }
        }
    }

    let mut out = BTreeMap::new();
    for mem_graph in graphs {
        let triples = survey_replay_store(&store, &mem_graph)?;
        out.insert(mem_graph, triples);
    }
    Ok(out)
}
