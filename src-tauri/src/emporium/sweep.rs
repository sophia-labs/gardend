//! Post-hoc conformance sweep over the LIVE memory projection — the detection
//! half of the reconciliation policy (§8.1, `plans/mo-reconciliation-policy-
//! 20260704.md`): *a forked supersession means CONTESTED, never overwritten*.
//!
//! The write-time SHACL gate validates DESIRED slices in a throwaway store, so
//! it is structurally blind to cross-record, store-context state — and a fork
//! is two individually-legal writes (two witnesses each superseded the same
//! head; the planner deliberately permits superseding a non-current record).
//! This sweep is the complementary READER: it queries the live graph for the
//! fork signature — more than one `mem:isCurrent=true` head sharing one
//! `mem:lineage` — and files each finding into the violation ledger
//! (`:projection:violations`) as this observer's ADVISORY testimony. Detection
//! before resolution: the sweep never mutates a record and never picks a
//! winner; resolution stays an explicit producer act (a new supersession).
//!
//! Findings are content-addressed by the ledger (focus/shape/message keyed),
//! so a re-sweep of an unchanged fork converges onto the same violation
//! subject instead of duplicating testimony.

use std::collections::BTreeMap;

use oxigraph::sparql::{QueryResults, SparqlEvaluator};
use oxigraph::store::Store;

use crate::app_runtime::AppHandle;
use crate::emporium::contract::{memory_core_vocabulary, VocabularyContract};
use crate::emporium::memory_applier::open_memory_store;
use crate::emporium::query_emit::union_over_graphs;
use crate::emporium::shacl_validator::ViolationRecord;
use crate::emporium::terms::RETRACTED_AT_PRED;
use crate::emporium::violation_ledger::append_violations;
use crate::rdf_authority::memory_projection_graph_iri_for;

/// The sweep's witness identity in the ledger — distinct from the write-time
/// validator so a reviewer can tell gate testimony from sweep testimony.
pub(crate) const SWEEP_OBSERVER: &str = "urn:sophia:observer:conformance-sweep";

/// The (non-SHACL) policy shape a contested-lineage finding cites.
pub(crate) const CONTESTED_LINEAGE_SHAPE: &str = "urn:sophia:policy:contested-lineage";

/// One contested lineage: the stable chain id and its current heads (sorted).
#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct ContestedLineage {
    pub(crate) lineage: String,
    pub(crate) heads: Vec<String>,
}

/// One lineage with ALL its current heads and whether it is contested (>1 head).
/// The read-back shape for the ratified `mem:contested` / "return all" policy —
/// storage never picks a winner, so a reader gets every head, flagged.
#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct LineageHeads {
    pub(crate) lineage: String,
    pub(crate) heads: Vec<String>,
    pub(crate) contested: bool,
}

/// The ratified conflict strategy a memory graph runs under. `contested` (the
/// declared default, `plans/mo-reconciliation-policy-20260704.md`): a fork is
/// FLAGGED and all heads are returned; the engine never resolves. Any other
/// declared value is carried verbatim as a v2 resolver hook — v1 still only
/// flags. Read from the contract's `conflict_policies` (the ratified override
/// surface), so behavior is declaration-driven, not hard-coded.
pub(crate) fn declared_conflict_strategy(contract: &VocabularyContract) -> String {
    contract
        .conflict_policies
        .iter()
        .find_map(|p| {
            p.get("conflictStrategy")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_else(|| "contested".to_string())
}

/// What a sweep found and recorded.
#[derive(Debug, serde::Serialize)]
pub(crate) struct SweepReport {
    pub(crate) graph_id: String,
    pub(crate) observer_graph: String,
    pub(crate) strategy: String,
    pub(crate) contested: Vec<ContestedLineage>,
    /// Violation RESOURCES written this sweep (0 on a clean graph; identical
    /// re-findings collapse onto their existing content-addressed subjects).
    pub(crate) ledgered: usize,
}

/// The read-back of every current head, grouped by lineage and contested-flagged
/// — the "return all heads flagged" primitive a recall reader consumes to honor
/// the ratified policy (surface all testimony, never let storage rank).
#[derive(Debug, serde::Serialize)]
pub(crate) struct HeadsReport {
    pub(crate) graph_id: String,
    pub(crate) observer_graph: String,
    pub(crate) strategy: String,
    pub(crate) lineages: Vec<LineageHeads>,
}

/// Run a `SELECT ?lineage ?s WHERE { … }` query and group the solutions into
/// `lineage → [head, …]` (heads sorted; lineages sorted via the `BTreeMap`).
/// The shared execution tail for [`current_heads_by_lineage`] and
/// [`heads_as_of`] — the two differ only in the WHERE-clause body they build,
/// never in how the rows are gathered.
fn run_lineage_head_query(
    store: &Store,
    query: &str,
) -> Result<BTreeMap<String, Vec<String>>, String> {
    let solutions = match SparqlEvaluator::new()
        .parse_query(query)
        .map_err(|e| format!("parse heads query: {e}"))?
        .on_store(store)
        .execute()
        .map_err(|e| format!("execute heads query: {e}"))?
    {
        QueryResults::Solutions(s) => s,
        _ => return Err("heads query expected SELECT solutions".to_string()),
    };
    let strip = |t: &oxigraph::model::Term| {
        let s = t.to_string();
        s.strip_prefix('<')
            .and_then(|x| x.strip_suffix('>'))
            .map(str::to_string)
            .unwrap_or(s)
    };
    let mut heads_by_lineage: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for sol in solutions {
        let sol = sol.map_err(|e| format!("heads row: {e}"))?;
        let (Some(lineage), Some(head)) = (sol.get("lineage"), sol.get("s")) else {
            continue;
        };
        heads_by_lineage
            .entry(strip(lineage))
            .or_default()
            .push(strip(head));
    }
    for heads in heads_by_lineage.values_mut() {
        heads.sort();
    }
    Ok(heads_by_lineage)
}

/// Gather every `(lineage → [current head])` across one or more graphs — THE
/// server-authoritative "current head" semantics (ratified, `plans/mo-
/// reconciliation-policy-20260704.md`: "Head — a record with
/// `mem:isCurrent=true`"). The shared read behind the sweep (contested
/// subset), the return-all reader ([`current_heads_flagged`]), AND the T2
/// `query_emit` `currentHeads` plan ([`crate::emporium::query_engine`]) — ONE
/// function, so "current head" cannot mean two different things depending on
/// which caller asks.
///
/// `graphs` is queried as a UNION (one branch per graph) so a membrane-ed
/// class's commons-∪-observer scoping is the SAME shape as a single-graph
/// read — a 1-element slice degenerates to exactly the pre-generalization
/// query (an inert extra `{ }` grouping; the BGP and its results are
/// unchanged). `lineage_pred`/`is_current_pred` are the class's OWN expanded
/// predicate URIs (not hardcoded to `mem:`), so this generalizes to any class
/// whose shape declares the same lineage convention.
///
/// T-W Law 4 (retraction excludes a head): every branch ALSO excludes a
/// subject carrying [`RETRACTED_AT_PRED`] in that same graph — a retracted
/// record is never a current head again, in EVERY caller of this ONE
/// function (the sweep, `emporium_heads`, `sparql_query_named`'s
/// `currentHeads`, and `emporium_query`'s contested-class routing).
pub(crate) fn current_heads_by_lineage(
    store: &Store,
    graphs: &[String],
    lineage_pred: &str,
    is_current_pred: &str,
) -> Result<BTreeMap<String, Vec<String>>, String> {
    if graphs.is_empty() {
        return Err("current_heads_by_lineage: no graphs to scope the read to".to_string());
    }
    let branches: Vec<String> = graphs
        .iter()
        .map(|g| {
            format!(
                "{{ GRAPH <{g}> {{ ?s <{lineage_pred}> ?lineage ; <{is_current_pred}> true . \
                 FILTER NOT EXISTS {{ ?s <{RETRACTED_AT_PRED}> ?emporiumRetractedAt }} }} }}"
            )
        })
        .collect();
    let query = format!(
        "SELECT ?lineage ?s WHERE {{ {} }}",
        branches.join(" UNION ")
    );
    run_lineage_head_query(store, &query)
}

/// Reconstruct the heads AS OF a point in time — the historical counterpart to
/// [`current_heads_by_lineage`]. `mem:isCurrent` is a LIVE flag with no history
/// of its own (it only ever describes "now"), so "what was current at time T"
/// cannot be read off it; instead a record `r` is a head-as-of-`asOf` in its
/// lineage iff `r.createdAt <= asOf` AND no OTHER record supersedes it with ITS
/// OWN `createdAt <= asOf` too. This walks the append-only supersession log
/// (`supersedes`/`createdAt`), so it is well-defined for any past timestamp —
/// the §8.1 "return all heads flagged" policy generalized across time, not just
/// the live instant. `as_of_literal` is a ready-to-splice SPARQL term (a typed
/// `"…"^^xsd:dateTime` literal); the caller owns formatting it.
///
/// The supersession check is scoped to the SAME union of `graphs` as the
/// candidate search, NOT re-scoped per candidate's own graph: a membrane-ed
/// class's `graphs` is commons ∪ an observer's membrane, and a record filed
/// in commons can be superseded by a later record filed in the observer's own
/// membrane (recall's ordinary shape). Nesting `FILTER NOT EXISTS` inside each
/// `GRAPH <g>` branch would make that supersession invisible whenever the
/// superseding record lives in a DIFFERENT graph than the one being checked —
/// the commons branch can't see into the membrane branch — silently
/// resurrecting an already-superseded head as "current as of". Both the
/// candidate BGP and the NOT-EXISTS BGP are therefore UNIONed across the same
/// `graphs` (a 1-graph slice degenerates to the pre-existing per-graph shape).
pub(crate) fn heads_as_of(
    store: &Store,
    graphs: &[String],
    lineage_pred: &str,
    created_at_pred: &str,
    supersedes_pred: &str,
    as_of_literal: &str,
) -> Result<BTreeMap<String, Vec<String>>, String> {
    if graphs.is_empty() {
        return Err("heads_as_of: no graphs to scope the read to".to_string());
    }
    // T-W Law 4: a retracted candidate is never a historical head either —
    // but ONLY once the retraction had actually happened as of `as_of`. A
    // retraction is itself a timestamped event; a query for a moment BEFORE
    // that retraction must still see the record as it stood then, or an
    // as-of read would smuggle "currently retracted" into the past (the
    // review finding this guards). Compare, don't just check existence —
    // mirrors the `?createdAt <= as_of_literal` treatment below.
    let candidate_body = format!(
        "?s <{lineage_pred}> ?lineage ; <{created_at_pred}> ?createdAt . \
         FILTER NOT EXISTS {{ \
           ?s <{RETRACTED_AT_PRED}> ?emporiumRetractedAtCandidate . \
           FILTER(?emporiumRetractedAtCandidate <= {as_of_literal}) \
         }}"
    );
    let supersession_body = format!("?s2 <{supersedes_pred}> ?s ; <{created_at_pred}> ?createdAt2");
    let query = format!(
        "SELECT ?lineage ?s WHERE {{ \
           {candidates} \
           FILTER(?createdAt <= {as_of_literal}) \
           FILTER NOT EXISTS {{ \
             {supersessions} \
             FILTER(?createdAt2 <= {as_of_literal}) \
           }} \
         }}",
        candidates = union_over_graphs(graphs, &candidate_body),
        supersessions = union_over_graphs(graphs, &supersession_body),
    );
    run_lineage_head_query(store, &query)
}

/// Read every current head, grouped + contested-flagged (the "return all"
/// policy). Pure read — never writes, never resolves.
pub(crate) fn current_heads_flagged(
    app: &AppHandle,
    graph_id: &str,
    observer: &str,
) -> Result<HeadsReport, String> {
    let store = open_memory_store(app, graph_id)?;
    let mem_graph = memory_projection_graph_iri_for(graph_id, observer);
    let contract = memory_core_vocabulary();
    let mem = contract.primary_namespace();
    let heads_by_lineage = current_heads_by_lineage(
        &store,
        std::slice::from_ref(&mem_graph),
        &format!("{mem}lineage"),
        &format!("{mem}isCurrent"),
    )?;
    let lineages = heads_by_lineage
        .into_iter()
        .map(|(lineage, heads)| LineageHeads {
            contested: heads.len() > 1,
            lineage,
            heads,
        })
        .collect();
    Ok(HeadsReport {
        graph_id: graph_id.to_string(),
        observer_graph: mem_graph,
        strategy: declared_conflict_strategy(contract),
        lineages,
    })
}

/// Sweep one observer's memory projection (empty `observer` = the shared
/// commons) for contested lineages and file the findings.
pub(crate) fn sweep_memory_conformance(
    app: &AppHandle,
    graph_id: &str,
    observer: &str,
) -> Result<SweepReport, String> {
    let store = open_memory_store(app, graph_id)?;
    let mem_graph = memory_projection_graph_iri_for(graph_id, observer);
    let contract = memory_core_vocabulary();
    let mem = contract.primary_namespace();

    // ── the fork signature: every lineage with >1 current head (shared gather) ──
    let heads_by_lineage = current_heads_by_lineage(
        &store,
        std::slice::from_ref(&mem_graph),
        &format!("{mem}lineage"),
        &format!("{mem}isCurrent"),
    )?;
    let contested: Vec<ContestedLineage> = heads_by_lineage
        .into_iter()
        .filter(|(_, heads)| heads.len() > 1)
        .map(|(lineage, heads)| ContestedLineage { lineage, heads })
        .collect();

    // ── file each contested lineage as advisory testimony ──
    // The message is deterministic over the sorted head set, so the ledger's
    // content-addressing collapses an unchanged re-finding onto one subject.
    let violations: Vec<ViolationRecord> = contested
        .iter()
        .map(|c| ViolationRecord {
            focus_node: c.lineage.clone(),
            shape: Some(CONTESTED_LINEAGE_SHAPE.to_string()),
            property_path: Some(format!("{mem}isCurrent")),
            offending_value: Some(c.heads.join(" ")),
            message: format!(
                "contested lineage: {} current heads ({}) — resolution is an explicit \
                 supersession by a witness, never engine-side last-writer-wins",
                c.heads.len(),
                c.heads.join(", "),
            ),
            severity: "Warning".to_string(),
        })
        .collect();
    let observed_at_ms = chrono::Utc::now().timestamp_millis();
    let ledgered = append_violations(
        &store,
        graph_id,
        SWEEP_OBSERVER,
        observed_at_ms,
        &violations,
    )?;

    Ok(SweepReport {
        graph_id: graph_id.to_string(),
        observer_graph: mem_graph,
        strategy: declared_conflict_strategy(contract),
        contested,
        ledgered,
    })
}
