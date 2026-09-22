//! The memory materializer — the SECOND applier output mode.
//!
//! Authoritative typed memory (the `mem:` core pack) is a BORN-RDF sink: it is
//! materialized DIRECT-ON-STORE into the reserved, materializer-only
//! `:projection:memory` named graph (the same lawful path the document/graph
//! materializers use — [`crate::geist_memory_rdf`] does the same for the legacy
//! flat projection) — NOT through the `user:rdf` applier service. The user:rdf /
//! wf: applier path stays BYTE-FOR-BYTE UNCHANGED; memory writes bypass the
//! text-gated `run_sparql_update_service` (which correctly refuses `:projection:`
//! targets — see `rdf_authority`).
//!
//! RECOVERABILITY (S3 — was a lie until now). This header once called the memory
//! projection "DERIVED, regenerable". That was FALSE: nothing derived it. The
//! supersession chains, provenance, observers, and lifecycle state materialized
//! here had no other system of record — the legacy `memory-queue.json` persists
//! only content strings, and `memory/failures/` journals halts. Before this slice
//! the oxigraph store was the SOLE copy of everything written here.
//!
//! From S3 the RECOVERY SOURCE is the APPLIED-PLAN JOURNAL (`emporium/applied/`,
//! one record per successful apply, carrying the serialized plan — the event log
//! of accepted memory writes). The store is now genuinely rebuildable: CLEAR this
//! named graph and REPLAY the journaled memory plans in order through the real
//! `spine::apply_memory_plan`, which reproduces the projection set-for-set (see
//! [`crate::emporium::applied_journal::rebuild_memory_projection_from_journal`],
//! tested). The projection is a projection OF THAT LOG — disposable because the
//! journal exists, never because the writes were derivable from elsewhere.
//!
//! The planner emits `Step::SparqlUpdate` bodies with NO GRAPH wrapper (graph-
//! agnostic `render_updates`); this sink graph-wraps each into
//! `GRAPH <{memory_projection_graph_iri}>` and runs it via `SparqlEvaluator` on the
//! per-graph oxigraph Store. LOUD-halt on the first error (the deliberate inversion
//! of emporium's fire-and-forget posture).

use std::sync::Arc;

use oxigraph::sparql::SparqlEvaluator;
use oxigraph::store::Store;

use crate::app_runtime::AppHandle;
use crate::emporium::contract::VocabularyContract;
use crate::emporium::shacl_validator::{validate_desired_structured_in_graph, ViolationRecord};
use crate::emporium::terms::Triple;
use crate::emporium::violation_ledger::{append_violations, DEFAULT_OBSERVER};
use crate::paths::existing_graph_dir;
use crate::rdf_authority::memory_projection_graph_iri;
use crate::rdf_service::open_graph_store;
use crate::runtime_config::ValidationPolicy;

/// Open the per-graph oxigraph Store for `graph_id` (resolving the graph dir
/// first — `open_graph_store` takes a `&Path`, not a graph_id). The store is the
/// process-cached handle shared with every other reader/writer of this graph.
pub(super) fn open_memory_store(app: &AppHandle, graph_id: &str) -> Result<Arc<Store>, String> {
    let graph_dir =
        existing_graph_dir(app, graph_id).map_err(|e| format!("resolve graph dir: {e}"))?;
    open_graph_store(&graph_dir)
}

/// The verdict of the per-graph memory-validation gate (EA-6). Decided BEFORE any
/// memory write runs; the apply fork acts on it.
///
/// - [`ValidationGate::Proceed`] — the write may apply. `flagged` carries the
///   violations recorded to the ledger under `FlagAndAccept` (empty under a clean
///   write or `Off`); the apply fork has already appended them direct-on-store.
/// - [`ValidationGate::Halt`] — the write is REJECTED. `violations` are the
///   structured, agent-actionable SHACL failures (focus node, path, value, shape,
///   message, severity) so the caller can repair and retry; NOTHING was written.
#[derive(Debug)]
pub(super) enum ValidationGate {
    Proceed { flagged: Vec<ViolationRecord> },
    Halt { violations: Vec<ViolationRecord> },
}

/// A SHACL violation is BLOCKING (subject to the policy's Halt) iff its shape
/// declared `sh:Violation` severity. `sh:Warning`/`sh:Info` are ADVISORY — they
/// NEVER block a write, regardless of policy; they are flagged to the ledger so
/// they stay observable (the honest-gap surfaces: the I1-Valuation advisory for the
/// un-witness-scoped salience gap, and I3d's monovocal-elision convention).
///
/// This is the TIER signal, and it is NOT hard-coded per shape: the tier rides each
/// shape's own `sh:severity` in the §3 golden (extracted verbatim by
/// [`super::shacl_sparql`] into [`ViolationRecord::severity`]). So the gate honors
/// the spec §3 ruling — I1-membrane-presence/I1b/I2/I2b/I3/I4b/IdentityUnification
/// are `sh:Violation` (Halt-eligible / Halt), the two advisories are `sh:Warning`
/// (never block) — purely by reading the severity the contract author set. The
/// per-graph [`ValidationPolicy`] then decides what happens to the BLOCKING subset
/// (Halt rejects; FlagAndAccept ledgers-and-proceeds). Structural rudof violations
/// (minCount/datatype/closed) carry "Violation" too, so they remain blocking.
fn is_blocking(v: &ViolationRecord) -> bool {
    v.severity == "Violation"
}

/// The memory-path SHACL ENFORCEMENT gate (EA-6 + CA-1 tiering). Validate a plan's
/// desired-insert triple set against the contract, then dispatch on the per-graph
/// [`ValidationPolicy`] — but tier by each violation's `sh:severity` first:
///
/// - [`ValidationPolicy::Off`] — skip validation entirely (byte-identical legacy
///   path); always `Proceed { flagged: [] }`, the engine is never invoked.
/// - clean write (conforms) — `Proceed { flagged: [] }` under any policy.
/// - The merged violation list is PARTITIONED by [`is_blocking`] (severity-driven,
///   NOT a per-shape hard-code): `sh:Violation` are blocking, `sh:Warning`/`sh:Info`
///   are advisory.
/// - ADVISORY violations (warnings) NEVER block. Under `Halt` OR `FlagAndAccept`
///   they are recorded to the ledger and the write proceeds (an advisory is
///   testimony about a known gap, not a rejected write).
/// - BLOCKING violations under [`ValidationPolicy::Halt`] — `Halt { violations }`
///   (the blocking subset only): the caller rejects the write and returns the
///   STRUCTURED violations as agent feedback. (Advisories are dropped from the Halt
///   payload — they are not the reason for rejection — but if there are ALSO
///   advisories alongside a Halt, the write is rejected anyway, so nothing is
///   ledgered: a rejected write left no trace to annotate.)
/// - BLOCKING violations under [`ValidationPolicy::FlagAndAccept`] — RECORD every
///   violation (blocking AND advisory) to the violation ledger
///   (`:projection:violations`, a Meaningful Object) DIRECT-ON-STORE, then
///   `Proceed { flagged }`: the write still lands.
///
/// The ledger is a DERIVED projection, never the source of truth for this
/// decision — it is appended only as a side-effect of FlagAndAccept (or of a
/// warning-only outcome), and a ledger-append failure is surfaced (`Err`) rather
/// than silently swallowed: the gate must not claim to have flagged a violation it
/// failed to durably record.
///
/// `desired_inserts.is_empty()` (a converged re-file) conforms trivially without
/// invoking the engine — so a no-op re-apply is never gated.
pub(super) fn validate_memory_write(
    store: &Store,
    graph_id: &str,
    policy: ValidationPolicy,
    contract: &VocabularyContract,
    desired_inserts: &[Triple],
    membrane_graph_iri: &str,
    observed_at_ms: i64,
) -> Result<ValidationGate, String> {
    // Off: the legacy path — never run the engine, never touch the ledger.
    if policy == ValidationPolicy::Off {
        return Ok(ValidationGate::Proceed {
            flagged: Vec::new(),
        });
    }

    // Real validation over the desired-insert triples (empty ⇒ conforms). The
    // GRAPH-AWARE variant loads the data into the per-observer membrane graph the
    // write targets, so membrane-scoped sh:select constraints (I1/I2b) see the
    // correct named-graph perspective; merged with rudof's structural violations.
    let violations =
        match validate_desired_structured_in_graph(desired_inserts, contract, membrane_graph_iri) {
            Ok(()) => {
                return Ok(ValidationGate::Proceed {
                    flagged: Vec::new(),
                })
            }
            Err(violations) => violations,
        };

    // Tier the merged list by each shape's own sh:severity (NOT a per-shape
    // hard-code): only `sh:Violation` is blocking; `sh:Warning`/`sh:Info` are
    // advisory and never block.
    let has_blocking = violations.iter().any(is_blocking);

    match policy {
        ValidationPolicy::Off => unreachable!("Off short-circuited above"),
        ValidationPolicy::Halt if has_blocking => {
            // A blocking violation under Halt rejects the write. Return ONLY the
            // blocking subset as the rejection reason (advisories are not why the
            // write was refused). Nothing is ledgered — a rejected write left no
            // landed state for an advisory to annotate.
            let blocking: Vec<ViolationRecord> =
                violations.into_iter().filter(is_blocking).collect();
            Ok(ValidationGate::Halt {
                violations: blocking,
            })
        }
        // Either FlagAndAccept (any violation), or Halt with ONLY advisories: the
        // write lands, and EVERY violation (blocking + advisory) is recorded to the
        // ledger MO. A ledger-append failure is loud — the gate cannot honestly
        // proceed-as-flagged without a durable record.
        ValidationPolicy::FlagAndAccept | ValidationPolicy::Halt => {
            append_violations(
                store,
                graph_id,
                DEFAULT_OBSERVER,
                observed_at_ms,
                &violations,
            )?;
            Ok(ValidationGate::Proceed {
                flagged: violations,
            })
        }
    }
}

/// Run one already-rendered `INSERT DATA`/`DELETE DATA` body against the memory
/// projection graph, GRAPH-wrapping it into `mem_graph` first. `mem_graph` is the
/// per-observer projection graph the planner minted the subjects under (Variant B)
/// — the caller (the apply fork) passes `plan.memory_graph_iri(graph_id)`, so the
/// wrap target and the subject root always agree. Mirrors the applier's
/// `graph_wrap` shape but runs direct-on-store (no authority service).
pub(super) fn run_memory_update(
    store: &Store,
    mem_graph: &str,
    update: &str,
) -> Result<(), String> {
    let wrapped = graph_wrap_memory(update, mem_graph)?;
    SparqlEvaluator::new()
        .parse_update(&wrapped)
        .map_err(|e| format!("parse memory update: {e}"))?
        .on_store(store)
        .execute()
        .map_err(|e| format!("execute memory update: {e}"))
}

/// `INSERT/DELETE DATA { body }` → same verb wrapped in `GRAPH <{mem}> { body }`.
/// Port of the applier's `graph_wrap`, scoped to the memory graph: match the
/// leading verb, take the body between the FIRST `{` and the LAST `}`, re-emit
/// wrapped. The planner's `render_updates` output satisfies this shape.
fn graph_wrap_memory(update: &str, mem_graph: &str) -> Result<String, String> {
    let trimmed = update.trim_start();
    let verb = if let Some(rest) = trimmed.strip_prefix("INSERT DATA") {
        ("INSERT DATA", rest)
    } else if let Some(rest) = trimmed.strip_prefix("DELETE DATA") {
        ("DELETE DATA", rest)
    } else {
        return Err(format!(
            "unexpected memory update shape: {}",
            &update.chars().take(80).collect::<String>()
        ));
    };
    let (verb_word, after) = verb;
    let open = after
        .find('{')
        .ok_or_else(|| "memory update missing opening brace".to_string())?;
    let close = update
        .rfind('}')
        .ok_or_else(|| "memory update missing closing brace".to_string())?;
    // Recompute the body span against the original string for the close index.
    let body_start = update.len() - after.len() + open + 1;
    if body_start > close {
        return Err("memory update has empty/invalid body span".to_string());
    }
    let body = &update[body_start..close];
    Ok(format!(
        "{verb_word} {{ GRAPH <{mem_graph}> {{\n{body}\n}} }}"
    ))
}

/// Materialize a single memory record subject's full triple set via a GRAPH-
/// wrapped DELETE+INSERT (the document/graph materializer shape). Provided for
/// callers that want a per-subject full replace; the v1 apply path uses the diff
/// → [`run_memory_update`] path instead (it yields zero-ops idempotency for free).
#[cfg_attr(not(test), allow(dead_code))]
pub(super) fn materialize_memory_record(
    store: &Store,
    graph_id: &str,
    subject: &str,
    triples_body: &str,
) -> Result<(), String> {
    let g = memory_projection_graph_iri(graph_id);
    let update = format!(
        "DELETE {{ GRAPH <{g}> {{ <{subject}> ?p ?o . }} }}\n\
         INSERT {{ GRAPH <{g}> {{\n{triples_body}\n}} }}\n\
         WHERE  {{ OPTIONAL {{ GRAPH <{g}> {{ <{subject}> ?p ?o . }} }} }}"
    );
    SparqlEvaluator::new()
        .parse_update(&update)
        .map_err(|e| format!("parse memory materialization: {e}"))?
        .on_store(store)
        .execute()
        .map_err(|e| format!("materialize memory record: {e}"))
}

// ── Per-observer recall (net-new — the experiment's read-side payload) ────────
//
// The live `recall` MCP tool reads the flat `memory-queue.json` (agent-blind,
// SPARQL-free). Per-observer recall instead queries the per-observer typed
// projection graph (Variant B), scoped to the observer's graph AND the
// current-head filter (`mem:status = "active"`). This is the SPARQL read-side the
// per-agent Geist needs; `recall_for_observer_in_store` is the pure, testable
// core (a `&Store` query) and `recall_for_observer` is the AppHandle path that
// opens the per-graph store and runs it.

/// One recalled record's subject IRI + content, ordered most-recent-first by
/// `mem:createdAt`.
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, Clone)]
pub(crate) struct RecallHit {
    pub(crate) subject: String,
    pub(crate) content: String,
    pub(crate) created_at: Option<i64>,
}

/// Build the per-observer current-head recall SPARQL: every ACTIVE
/// (`mem:status = "active"`) MemoryRecord in `observer`'s projection graph whose
/// content contains `query` (case-insensitive; empty `query` = all heads),
/// most-recent-first, capped at `limit`. The `GRAPH <{mem}>` clause confines the
/// read to THIS observer's perspective — B-only memories are unreachable from A.
///
/// KNOWN DIVERGENCE (T2 query-face audit, 2026-07-06): this is NOT the ratified
/// "current head" semantics (`plans/mo-reconciliation-policy-20260704.md`:
/// "Head — a record with `mem:isCurrent=true`"), which
/// [`crate::emporium::sweep::current_heads_by_lineage`] implements and
/// [`crate::emporium::query_engine`]'s `currentHeads` named query now shares.
/// This function instead filters on `mem:status = "active"` — a DIFFERENT
/// predicate the planner sets unconditionally, whereas `mem:isCurrent` is
/// OPTIONAL (only emitted when a record declares it, gated by I7's
/// `validFrom` requirement). The two notions genuinely disagree on real data:
/// `geist_memory_backfill.rs` migrates legacy queue rows with `status: "active"`
/// but `is_current: None` (no `mem:isCurrent` triple at all), so those records
/// are INVISIBLE to the ratified isCurrent-based reader but VISIBLE here.
/// Switching this query to `mem:isCurrent=true` would silently drop every
/// backfilled per-observer memory from `recall_for_observer`'s results — a real
/// regression, not a wash — so it is deliberately left AS-IS this wave (flagged
/// for review rather than smuggled; see the T2 query-face report).
fn recall_query(mem_graph: &str, query: &str, limit: usize) -> String {
    // Escape the user query for a SPARQL string literal (the only injected text).
    let needle = query
        .trim()
        .to_lowercase()
        .replace('\\', "\\\\")
        .replace('"', "\\\"");
    let content_filter = if needle.is_empty() {
        String::new()
    } else {
        format!("    FILTER(CONTAINS(LCASE(STR(?content)), \"{needle}\"))\n")
    };
    format!(
        "SELECT ?s ?content ?created WHERE {{\n  \
           GRAPH <{mem_graph}> {{\n    \
             ?s a <{NS}MemoryRecord> ;\n       \
                <{NS}status> \"active\" ;\n       \
                <{NS}content> ?content .\n    \
             OPTIONAL {{ ?s <{NS}createdAt> ?created }}\n\
{content_filter}  }}\n}}\n\
ORDER BY DESC(?created)\nLIMIT {limit}",
        NS = "http://mnemosyne.dev/memory#",
    )
}

/// Pure store-level per-observer recall — the testable core. Returns the matching
/// record SUBJECT IRIs (the test's isolation oracle); the AppHandle path returns
/// the richer [`RecallHit`].
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn recall_for_observer_in_store(
    store: &Store,
    graph_id: &str,
    observer: &str,
    query: &str,
    limit: usize,
) -> Vec<String> {
    recall_hits_in_store(store, graph_id, observer, query, limit)
        .into_iter()
        .map(|h| h.subject)
        .collect()
}

/// Pure store-level per-observer recall returning full [`RecallHit`]s.
fn recall_hits_in_store(
    store: &Store,
    graph_id: &str,
    observer: &str,
    query: &str,
    limit: usize,
) -> Vec<RecallHit> {
    use oxigraph::sparql::QueryResults;
    let mem = crate::rdf_authority::memory_projection_graph_iri_for(graph_id, observer);
    let q = recall_query(&mem, query, limit);
    let prepared = match SparqlEvaluator::new().parse_query(&q) {
        Ok(p) => p,
        Err(_) => return Vec::new(),
    };
    let solutions = match prepared.on_store(store).execute() {
        Ok(QueryResults::Solutions(s)) => s,
        _ => return Vec::new(),
    };
    let mut out = Vec::new();
    for sol in solutions.flatten() {
        let Some(oxigraph::model::Term::NamedNode(s)) = sol.get("s") else {
            continue;
        };
        let content = match sol.get("content") {
            Some(oxigraph::model::Term::Literal(l)) => l.value().to_string(),
            _ => String::new(),
        };
        let created_at = match sol.get("created") {
            Some(oxigraph::model::Term::Literal(l)) => l.value().parse::<i64>().ok(),
            _ => None,
        };
        out.push(RecallHit {
            subject: s.as_str().to_string(),
            content,
            created_at,
        });
    }
    out
}

/// AppHandle-driven per-observer recall — opens the per-graph store and runs the
/// pure core. The real read-side entry point (the MCP `recall` rewiring can call
/// this; the boundary is reported in the deliverable). NOT yet wired into the MCP
/// dispatch (the live `recall` still reads the flat queue — see the report).
#[allow(dead_code)]
pub(crate) fn recall_for_observer(
    app: &AppHandle,
    graph_id: &str,
    observer: &str,
    query: &str,
    limit: usize,
) -> Result<Vec<RecallHit>, String> {
    let store = open_memory_store(app, graph_id)?;
    Ok(recall_hits_in_store(
        &store, graph_id, observer, query, limit,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    use oxigraph::sparql::QueryResults;

    use crate::emporium::contract::memory_core_vocabulary;
    use crate::emporium::planner::{memory_record_subject, plan_memory_compute, Plan, Step};
    use crate::emporium::schemas::{MemoryRecordIn, SourceRefIn};
    use crate::emporium::survey::{FolderEntry, Live};
    use crate::rdf_authority::user_rdf_graph_iri;

    const GRAPH: &str = "lab";

    #[test]
    fn graph_wrap_memory_wraps_insert_data() {
        let wrapped = graph_wrap_memory(
            "INSERT DATA {\n<urn:s> <urn:p> <urn:o> .\n}",
            "urn:mnemosyne:local:graph:lab:projection:memory",
        )
        .unwrap();
        assert!(wrapped.starts_with(
            "INSERT DATA { GRAPH <urn:mnemosyne:local:graph:lab:projection:memory> {"
        ));
        assert!(wrapped.contains("<urn:s> <urn:p> <urn:o> ."));
        assert!(wrapped.trim_end().ends_with("} }"));
    }

    #[test]
    fn graph_wrap_memory_rejects_non_data_shape() {
        assert!(graph_wrap_memory("SELECT * WHERE { ?s ?p ?o }", "urn:g").is_err());
    }

    // ── store-level harness (Caveat 3) ──────────────────────────────────────
    //
    // These drive the SAME direct-on-store sink the apply fork uses
    // (`run_memory_update` → `graph_wrap_memory` → `SparqlEvaluator`) against an
    // in-memory oxigraph `Store`, so they pin the materializer end-to-end (no
    // AppHandle / on-disk graph dir needed). The pure-planner zero-ops and
    // content-hash determinism tests live in `planner/tests.rs`; these add the
    // store-truth assertions the planner cannot reach: named-graph isolation,
    // clean-store bootstrap, and re-correction convergence through the real sink.

    fn mem_live() -> Live {
        Live {
            graph: GRAPH.to_string(),
            prefix: format!("urn:mnemosyne:local:graph:{GRAPH}"),
            read_graph: memory_projection_graph_iri(GRAPH),
            ..Live::default()
        }
    }

    fn live_with_folder() -> Live {
        let mut live = mem_live();
        live.folders.insert(
            "memory".to_string(),
            FolderEntry {
                label: "Memory".to_string(),
                parent_id: None,
            },
        );
        live
    }

    fn record(content: &str) -> MemoryRecordIn {
        MemoryRecordIn {
            client_ref: Some("r-0".to_string()),
            scope: "agent".to_string(),
            kind: "ClaimMemory".to_string(),
            content_orientation: "knowledge".to_string(),
            visibility: "private".to_string(),
            status: "active".to_string(),
            content: content.to_string(),
            source_refs: vec![SourceRefIn {
                source_kind: "DocumentBlock".to_string(),
                source_label: None,
                block_id: Some("abc".to_string()),
                document_id: Some("doc-shell".to_string()),
                external_id: None,
                external_uri: None,
                observed_at: None,
                trust_tier: None,
            }],
            evidence: vec![],
            observed_at: Some(1_718_700_000_000),
            valid_from: Some(1_718_700_000_000),
            is_current: Some(true),
            confidence: None,
            valence: None,
            agent_id: Some("gamma".to_string()),
            observer_agent_id: None,
            tags: vec![],
            supersedes_ref: None,
            contradicts_ref: None,
        }
    }

    /// Run every `Step::SparqlUpdate` in a plan through the real memory sink
    /// (`run_memory_update`) against `store`, loud-halting on the first error —
    /// exactly the apply fork's behaviour, minus the CRDT-folder enqueue (the
    /// folder op has no RDF effect on the projection graph).
    fn apply_plan_to_store(store: &Store, plan: &Plan) -> Result<usize, String> {
        let mut updates = 0usize;
        for step in &plan.steps {
            if let Step::SparqlUpdate { update } = step {
                // These existing tests are the COMMONS path (empty observer), so
                // the wrap target is the shared `:projection:memory` graph — the
                // same IRI `plan.memory_graph_iri(GRAPH)` returns for an empty
                // observer. Pass it explicitly now that the wrap is graph-keyed.
                run_memory_update(store, &memory_projection_graph_iri(GRAPH), update)?;
                updates += 1;
            }
        }
        Ok(updates)
    }

    /// Count rows for a SELECT (the per-graph row counter the isolation test uses).
    fn count(store: &Store, query: &str) -> usize {
        match SparqlEvaluator::new()
            .parse_query(query)
            .expect("parse count query")
            .on_store(store)
            .execute()
            .expect("execute count query")
        {
            QueryResults::Solutions(solutions) => solutions.count(),
            _ => panic!("expected SELECT solutions"),
        }
    }

    /// The N-Triples INSERT/DELETE bodies a plan rendered, summed across steps —
    /// re-derives the plan's own rdf op counts so the test asserts against the
    /// store, not against the summary it is validating.
    fn plan_has_writes(plan: &Plan) -> bool {
        plan.steps
            .iter()
            .any(|s| matches!(s, Step::SparqlUpdate { .. }))
    }

    /// (d) CLEAN-STORE BOOTSTRAP — the very FIRST memory write on a brand-new,
    /// empty store must SUCCEED (no 400/parse/exec error) and self-create the
    /// memory folder op in the plan. The wf path's `judgment.shortId` prerequisite
    /// has no analog here: a first `remember` on a fresh graph cannot fail because
    /// the registry folder is missing.
    #[test]
    fn clean_store_bootstrap_first_write_succeeds() {
        let store = Store::new().expect("in-memory store");
        let contract = memory_core_vocabulary();
        let recs = vec![record("vera prefers the fish shell")];

        // Plan against an EMPTY live (clean graph) + EMPTY current triples.
        let plan = plan_memory_compute(contract, GRAPH, &recs, &mem_live(), &[])
            .expect("first plan on a clean graph must not error");
        assert!(
            plan.steps.iter().any(|s| s.op_name() == "create_folder"),
            "clean-graph first write self-creates the memory folder"
        );
        assert!(plan_has_writes(&plan), "first write inserts triples");

        // The sink runs the rendered updates against the empty store with NO 400.
        let ran = apply_plan_to_store(&store, &plan).expect("first write must not 400");
        assert!(ran > 0, "at least one INSERT ran on the clean store");

        // The record landed in the projection graph.
        let mem = memory_projection_graph_iri(GRAPH);
        let in_mem = count(
            &store,
            &format!("SELECT ?s WHERE {{ GRAPH <{mem}> {{ ?s a <http://mnemosyne.dev/memory#MemoryRecord> }} }}"),
        );
        assert_eq!(in_mem, 1, "the first memory record materialized");
    }

    /// (c) NAMED-GRAPH ISOLATION — after a memory apply, memory subjects exist
    /// ONLY in `:projection:memory`. The default graph and the `:user:rdf`
    /// authority graph carry ZERO rows for those subjects. This is the structural
    /// guarantee that the derived projection never competes with CRDT authority
    /// nor leaks into the user:rdf path.
    #[test]
    fn memory_triples_land_only_in_projection_graph() {
        let store = Store::new().expect("in-memory store");
        let contract = memory_core_vocabulary();
        let recs = vec![record("isolation under test")];

        let plan = plan_memory_compute(contract, GRAPH, &recs, &mem_live(), &[]).expect("plan");
        apply_plan_to_store(&store, &plan).expect("apply");

        let mem = memory_projection_graph_iri(GRAPH);
        let user_rdf = user_rdf_graph_iri(GRAPH);
        let mem_type = "<http://mnemosyne.dev/memory#MemoryRecord>";

        // Memory subjects ARE in :projection:memory …
        let in_mem = count(
            &store,
            &format!("SELECT ?s WHERE {{ GRAPH <{mem}> {{ ?s a {mem_type} }} }}"),
        );
        assert!(in_mem >= 1, "memory record must be in :projection:memory");

        // … and NOT in the default graph …
        let in_default = count(&store, &format!("SELECT ?s WHERE {{ ?s a {mem_type} }}"));
        assert_eq!(
            in_default, 0,
            "memory subjects must NOT appear in the default graph"
        );

        // … and NOT in the user:rdf authority graph.
        let in_user_rdf = count(
            &store,
            &format!("SELECT ?s WHERE {{ GRAPH <{user_rdf}> {{ ?s a {mem_type} }} }}"),
        );
        assert_eq!(
            in_user_rdf, 0,
            "memory subjects must NOT appear in the :user:rdf graph"
        );

        // Belt-and-suspenders: ALL quads in the store are in :projection:memory
        // (the sink wrote nothing anywhere else).
        let elsewhere = count(
            &store,
            &format!("SELECT ?s ?p ?o WHERE {{ GRAPH ?g {{ ?s ?p ?o }} FILTER(?g != <{mem}>) }}"),
        );
        assert_eq!(
            elsewhere, 0,
            "the memory sink wrote outside :projection:memory"
        );
    }

    /// (b) RE-FILE + RE-CORRECTION CONVERGENCE through the real sink. Mint, then
    /// materialize the IDENTICAL record again: the store is byte-stable (the
    /// re-file is a content-addressed no-op). Then file a CORRECTED value: the new
    /// content-addressed head materializes alongside (v1 supersedes the old head
    /// in place); re-filing THAT corrected record is again a no-op. This pins the
    /// store truth behind the planner's `rdfInsert/rdfDelete == 0` assertions.
    #[test]
    fn refile_and_recorrect_converge_on_store() {
        let store = Store::new().expect("in-memory store");
        let contract = memory_core_vocabulary();
        let mem = memory_projection_graph_iri(GRAPH);
        let all = format!("SELECT ?s ?p ?o WHERE {{ GRAPH <{mem}> {{ ?s ?p ?o }} }}");

        // ── mint ──
        let recs = vec![record("vera prefers fish")];
        let p1 = plan_memory_compute(contract, GRAPH, &recs, &mem_live(), &[]).expect("p1");
        apply_plan_to_store(&store, &p1).expect("apply mint");
        let after_mint = count(&store, &all);
        assert!(after_mint > 0, "mint wrote triples");

        // ── re-file the IDENTICAL record (folder now exists) ──
        // Survey the current memory triples back out and plan again: a converged
        // plan must emit ZERO rdf ops, and re-running the sink must not perturb the
        // store.
        let current = survey_memory(&store, &mem);
        let p2 =
            plan_memory_compute(contract, GRAPH, &recs, &live_with_folder(), &current).expect("p2");
        assert_eq!(p2.summary.rdf_insert, 0, "re-file inserts nothing");
        assert_eq!(p2.summary.rdf_delete, 0, "re-file deletes nothing");
        apply_plan_to_store(&store, &p2).expect("apply re-file");
        assert_eq!(
            count(&store, &all),
            after_mint,
            "the store is byte-stable across an identical re-file"
        );

        // ── re-correction: a CHANGED value mints a NEW content-addressed head and
        // explicitly supersedes the original (producer-directed). ──
        let mut corrected_rec = record("vera prefers fish AND zsh");
        corrected_rec.supersedes_ref =
            Some(memory_record_subject(GRAPH, &record("vera prefers fish")));
        let corrected = vec![corrected_rec];
        let current = survey_memory(&store, &mem);
        let p3 = plan_memory_compute(contract, GRAPH, &corrected, &live_with_folder(), &current)
            .expect("p3");
        assert!(
            p3.summary.rdf_insert > 0,
            "a corrected value writes the new head"
        );
        apply_plan_to_store(&store, &p3).expect("apply correction");
        let after_correction = count(&store, &all);
        assert!(
            after_correction > after_mint,
            "the corrected head materialized alongside (v1 keeps history)"
        );

        // ── re-file the CORRECTED record → converges to zero ops again ──
        let current = survey_memory(&store, &mem);
        let p4 = plan_memory_compute(contract, GRAPH, &corrected, &live_with_folder(), &current)
            .expect("p4");
        assert_eq!(
            p4.summary.rdf_insert, 0,
            "re-file of correction inserts nothing"
        );
        assert_eq!(
            p4.summary.rdf_delete, 0,
            "re-file of correction deletes nothing"
        );
        apply_plan_to_store(&store, &p4).expect("apply re-file of correction");
        assert_eq!(
            count(&store, &all),
            after_correction,
            "store byte-stable across an identical re-file of the corrected head"
        );
    }

    /// (a) AC4 SUPERSESSION SEMANTICS — a correction must demote the old head IN
    /// PLACE without destroying its content/provenance, and link the lineage both
    /// ways. The convergence test above only checks row *counts*; this pins the
    /// append-only demote contract:
    ///   - the old head keeps EVERY content + provenance triple (only its lifecycle
    ///     flips), gaining exactly one row (`supersededBy`) — guarding the
    ///     content-stripping regression where the value-canonical diff deleted the
    ///     old head's non-lifecycle triples;
    ///   - the old head becomes `superseded` + `isCurrent=false`, pointing forward;
    ///   - the new head is `active` and points back via `supersedes`;
    ///   - exactly one current head survives for the (scope, orientation).
    #[test]
    fn correction_demotes_old_head_preserving_content_and_links_lineage() {
        let store = Store::new().expect("in-memory store");
        let contract = memory_core_vocabulary();
        let mem = memory_projection_graph_iri(GRAPH);
        let ns = "http://mnemosyne.dev/memory#";

        let v1 = record("vera prefers fish");
        let v1_s = memory_record_subject(GRAPH, &v1);
        let mut v2 = record("vera prefers fish AND zsh");
        v2.supersedes_ref = Some(v1_s.clone()); // producer-directed supersession (§8.1 Case C)
        let v2_s = memory_record_subject(GRAPH, &v2);
        assert_ne!(
            v1_s, v2_s,
            "a changed value mints a distinct content-addressed head"
        );

        // ── mint v1, snapshot its triple count ──
        let p1 = plan_memory_compute(contract, GRAPH, &[v1.clone()], &mem_live(), &[]).expect("p1");
        apply_plan_to_store(&store, &p1).expect("apply mint");
        let v1_at_mint = count(
            &store,
            &format!("SELECT ?p ?o WHERE {{ GRAPH <{mem}> {{ <{v1_s}> ?p ?o }} }}"),
        );
        assert!(
            v1_at_mint > 3,
            "v1 minted with content + provenance, not just lifecycle"
        );

        // ── correct → v2 ──
        let current = survey_memory(&store, &mem);
        let p2 = plan_memory_compute(
            contract,
            GRAPH,
            &[v2.clone()],
            &live_with_folder(),
            &current,
        )
        .expect("p2");
        assert!(
            p2.summary.rdf_delete > 0,
            "the correction flips the old head's lifecycle"
        );
        apply_plan_to_store(&store, &p2).expect("apply correction");

        // CONTENT PRESERVED: old head kept all non-lifecycle triples; status and
        // isCurrent are value-swaps (net 0); supersededBy is the only net add (+1).
        let v1_after = count(
            &store,
            &format!("SELECT ?p ?o WHERE {{ GRAPH <{mem}> {{ <{v1_s}> ?p ?o }} }}"),
        );
        assert_eq!(
            v1_after,
            v1_at_mint + 1,
            "old head preserved all content/provenance (+1 supersededBy), NOT stripped"
        );

        let has = |s: &str, p: &str, o: &str| {
            count(
                &store,
                &format!("SELECT ?x WHERE {{ GRAPH <{mem}> {{ <{s}> <{ns}{p}> {o} }} }}"),
            )
        };
        // old head: superseded, not current, points FORWARD to v2.
        assert_eq!(
            has(&v1_s, "status", "\"superseded\""),
            1,
            "old head status=superseded"
        );
        assert_eq!(
            has(&v1_s, "isCurrent", "false"),
            1,
            "old head isCurrent=false"
        );
        assert_eq!(
            has(&v1_s, "supersededBy", &format!("<{v2_s}>")),
            1,
            "old head supersededBy → v2"
        );
        // new head: active, points BACK to v1.
        assert_eq!(
            has(&v2_s, "status", "\"active\""),
            1,
            "new head status=active"
        );
        assert_eq!(
            has(&v2_s, "supersedes", &format!("<{v1_s}>")),
            1,
            "new head supersedes → v1"
        );

        // exactly one current head after the correction.
        let heads = count(
            &store,
            &format!("SELECT ?s WHERE {{ GRAPH <{mem}> {{ ?s <{ns}isCurrent> true }} }}"),
        );
        assert_eq!(heads, 1, "exactly one current head after the correction");
    }

    /// REGRESSION (coarse-key collision): two DISTINCT memories that merely share a
    /// (scope, contentOrientation) but supersede nothing must COEXIST — neither
    /// demotes the other. Before producer-directed supersession the demote keyed on
    /// (scope, contentOrientation), so the second wrongly superseded the first (the
    /// bug the DeepSeek run surfaced: a shell preference and an editor preference,
    /// both user/knowledge, collided).
    #[test]
    fn distinct_memories_same_scope_orientation_coexist() {
        let store = Store::new().expect("in-memory store");
        let contract = memory_core_vocabulary();
        let mem = memory_projection_graph_iri(GRAPH);
        let ns = "http://mnemosyne.dev/memory#";

        let a = record("user prefers fish as their shell");
        let b = record("user prefers helix as their editor"); // same scope+orientation, NO supersedesRef
        let a_s = memory_record_subject(GRAPH, &a);
        let b_s = memory_record_subject(GRAPH, &b);

        let p1 = plan_memory_compute(contract, GRAPH, &[a.clone()], &mem_live(), &[]).expect("p1");
        apply_plan_to_store(&store, &p1).expect("apply a");
        let current = survey_memory(&store, &mem);
        let p2 = plan_memory_compute(contract, GRAPH, &[b.clone()], &live_with_folder(), &current)
            .expect("p2");
        assert_eq!(
            p2.summary.rdf_delete, 0,
            "filing a distinct memory must demote NOTHING"
        );
        apply_plan_to_store(&store, &p2).expect("apply b");

        let active = |s: &str| {
            count(
                &store,
                &format!("SELECT ?x WHERE {{ GRAPH <{mem}> {{ <{s}> <{ns}status> \"active\" }} }}"),
            )
        };
        assert_eq!(
            active(&a_s),
            1,
            "A stays active (NOT superseded by the distinct B)"
        );
        assert_eq!(active(&b_s), 1, "B is active");
        let heads = count(
            &store,
            &format!("SELECT ?s WHERE {{ GRAPH <{mem}> {{ ?s <{ns}status> \"active\" ; a <{ns}MemoryRecord> }} }}"),
        );
        assert_eq!(heads, 2, "both distinct memories coexist as active heads");
    }

    /// Read the current memory triples back out of `:projection:memory` as the
    /// planner's diff input (the store-truth analog of `survey::current_memory_triples`).
    fn survey_memory(store: &Store, mem_graph: &str) -> Vec<crate::emporium::terms::Triple> {
        let q = format!("SELECT ?s ?p ?o WHERE {{ GRAPH <{mem_graph}> {{ ?s ?p ?o }} }}");
        let solutions = match SparqlEvaluator::new()
            .parse_query(&q)
            .expect("parse survey")
            .on_store(store)
            .execute()
            .expect("execute survey")
        {
            QueryResults::Solutions(s) => s,
            _ => panic!("expected solutions"),
        };
        let mut out = Vec::new();
        for sol in solutions {
            let sol = sol.expect("solution row");
            let s = term_to_string(sol.get("s").expect("?s"));
            let p = term_to_string(sol.get("p").expect("?p"));
            let o = crate::emporium::survey::parse_term(&term_to_nt(sol.get("o").expect("?o")));
            out.push((s, p, o));
        }
        out
    }

    /// Bare IRI string for a subject/predicate term (strip the angle brackets the
    /// N-Triples serializer adds).
    fn term_to_string(t: &oxigraph::model::Term) -> String {
        match t {
            oxigraph::model::Term::NamedNode(n) => n.as_str().to_string(),
            other => panic!("expected a NamedNode subject/predicate, got {other}"),
        }
    }

    /// N-Triples serialization of an object term, the shape `survey::parse_term`
    /// consumes (`<uri>` / `"lit"` / `"lit"^^<dt>` / `"lit"@lang`).
    fn term_to_nt(t: &oxigraph::model::Term) -> String {
        t.to_string()
    }

    // ═══════════════════════════════════════════════════════════════════════
    // EA-6 — the per-graph SHACL VALIDATION GATE oracle (NO MOCKS).
    //
    // Real memory planner + real rudof (`validate_memory_write`) + real Oxigraph
    // store + real direct-on-store sink (`run_memory_update`) + real violation
    // ledger (`append_violations`). These drive the EXACT decision sequence
    // `spine::apply_memory_plan` runs (gate → on Proceed, run the sink), minus the
    // AppHandle/graph.json read — which only SELECTS the policy. So the three
    // modes + their teeth are proven end-to-end on a real graph store.
    // ═══════════════════════════════════════════════════════════════════════

    use crate::emporium::shacl_validator::ViolationRecord;
    use crate::emporium::terms::{render_updates, Term, Triple};
    use crate::rdf_authority::violations_projection_graph_iri;
    use crate::runtime_config::ValidationPolicy;
    use oxigraph::model::{Literal, NamedNode};

    const MEM: &str = "http://mnemosyne.dev/memory#";
    const RDF_TYPE_IRI: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";
    const OBS_AT: i64 = 1_718_700_000_000;

    fn t_uri(s: &str) -> Term {
        Term::Uri(NamedNode::new(s).expect("valid IRI"))
    }
    fn t_str(s: &str) -> Term {
        Term::Lit(Literal::new_simple_literal(s))
    }

    /// A WELL-FORMED desired-insert set the memory-core SHACL contract conforms to
    /// (a SourceReference carrying its required mem:sourceKind). The same shape the
    /// real planner mints for a provenance ref.
    fn well_formed_desired() -> Vec<Triple> {
        let s = "urn:mnemosyne:local:graph:lab:projection:memory:src:ok";
        vec![
            (
                s.to_string(),
                RDF_TYPE_IRI.to_string(),
                t_uri(&format!("{MEM}SourceReference")),
            ),
            (
                s.to_string(),
                RDF_TYPE_IRI.to_string(),
                t_uri("http://www.w3.org/ns/prov#Entity"),
            ),
            (
                s.to_string(),
                format!("{MEM}sourceKind"),
                t_str("DocumentBlock"),
            ),
        ]
    }

    /// A MALFORMED desired-insert set: a SourceReference MISSING its required
    /// mem:sourceKind (sh:minCount 1). The teeth case — conformant subjects pass,
    /// this one must be caught when validation is on.
    fn malformed_desired() -> Vec<Triple> {
        let s = "urn:mnemosyne:local:graph:lab:projection:memory:src:missing-kind";
        vec![
            (
                s.to_string(),
                RDF_TYPE_IRI.to_string(),
                t_uri(&format!("{MEM}SourceReference")),
            ),
            (
                s.to_string(),
                RDF_TYPE_IRI.to_string(),
                t_uri("http://www.w3.org/ns/prov#Entity"),
            ),
            // mem:sourceKind (required) deliberately OMITTED.
        ]
    }

    /// Drive the EXACT `apply_memory_plan` decision sequence against a real store:
    /// run the gate; iff it says Proceed, run the sink (`run_memory_update`) for
    /// the desired triples — the real direct-on-store write. Returns the gate so
    /// the test can inspect Halt/flagged. On Halt, NOTHING is written (the sink is
    /// never reached) — exactly the production fork.
    fn gate_then_apply(
        store: &Store,
        policy: ValidationPolicy,
        desired: &[Triple],
    ) -> Result<ValidationGate, String> {
        let contract = memory_core_vocabulary();
        // The commons memory graph (empty observer) — matches the sink target below.
        let membrane = memory_projection_graph_iri(GRAPH);
        let gate =
            validate_memory_write(store, GRAPH, policy, contract, desired, &membrane, OBS_AT)?;
        if let ValidationGate::Proceed { .. } = gate {
            // Mirror the planner→sink path: render the desired triples to an
            // INSERT DATA body and run it through the real memory sink. The sink
            // takes the per-observer projection GRAPH IRI (post per-agent-Geist);
            // these EA-6 subjects are the empty-observer commons, so wrap into the
            // commons memory graph (== plan.memory_graph_iri for an empty observer).
            for update in render_updates("INSERT DATA", desired, 60) {
                run_memory_update(store, &memory_projection_graph_iri(GRAPH), &update)?;
            }
        }
        Ok(gate)
    }

    fn count_q(store: &Store, query: &str) -> usize {
        count(store, query)
    }

    /// Count the memory subjects written into :projection:memory.
    fn mem_src_count(store: &Store) -> usize {
        let mem = memory_projection_graph_iri(GRAPH);
        count_q(
            store,
            &format!("SELECT ?s WHERE {{ GRAPH <{mem}> {{ ?s a <{MEM}SourceReference> }} }}"),
        )
    }

    /// Count the violation resources in the ledger projection graph.
    fn violation_count(store: &Store) -> usize {
        let g = violations_projection_graph_iri(GRAPH);
        count_q(
            store,
            &format!(
                "SELECT ?v WHERE {{ GRAPH <{g}> {{ ?v a <http://mnemosyne.dev/memory/violation#Violation> }} }}"
            ),
        )
    }

    // ── (1) WELL-FORMED write SUCCEEDS under ALL THREE policies ──────────────

    #[test]
    fn well_formed_write_succeeds_under_halt() {
        let store = Store::new().unwrap();
        let gate = gate_then_apply(&store, ValidationPolicy::Halt, &well_formed_desired())
            .expect("clean write must not error under Halt");
        assert!(
            matches!(gate, ValidationGate::Proceed { .. }),
            "clean write proceeds"
        );
        assert_eq!(mem_src_count(&store), 1, "the well-formed source landed");
        assert_eq!(
            violation_count(&store),
            0,
            "a clean write records NO violation"
        );
    }

    #[test]
    fn well_formed_write_succeeds_under_flag_and_accept() {
        let store = Store::new().unwrap();
        let gate = gate_then_apply(
            &store,
            ValidationPolicy::FlagAndAccept,
            &well_formed_desired(),
        )
        .expect("clean write must not error under FlagAndAccept");
        assert!(matches!(gate, ValidationGate::Proceed { flagged } if flagged.is_empty()));
        assert_eq!(mem_src_count(&store), 1, "the well-formed source landed");
        assert_eq!(
            violation_count(&store),
            0,
            "a clean write records NO violation"
        );
    }

    #[test]
    fn well_formed_write_succeeds_under_off() {
        let store = Store::new().unwrap();
        let gate = gate_then_apply(&store, ValidationPolicy::Off, &well_formed_desired())
            .expect("clean write must not error under Off");
        assert!(matches!(gate, ValidationGate::Proceed { flagged } if flagged.is_empty()));
        assert_eq!(mem_src_count(&store), 1, "the well-formed source landed");
    }

    // ── (2) MALFORMED write — Halt => Err + NO write + structured violations ─

    #[test]
    fn malformed_write_under_halt_rejects_with_structured_violations_and_no_write() {
        let store = Store::new().unwrap();
        let gate = gate_then_apply(&store, ValidationPolicy::Halt, &malformed_desired())
            .expect("the gate itself does not error (Halt is a verdict, not a fault)");
        // The verdict is Halt, carrying STRUCTURED violations (agent-repairable).
        let violations = match gate {
            ValidationGate::Halt { violations } => violations,
            other => panic!("expected Halt verdict, got {other:?}"),
        };
        assert!(
            !violations.is_empty(),
            "Halt carries at least one structured violation"
        );
        let v: &ViolationRecord = &violations[0];
        assert!(
            v.focus_node.contains("missing-kind"),
            "the violation names the malformed focus node: {}",
            v.focus_node
        );
        assert_eq!(
            v.property_path.as_deref(),
            Some("http://mnemosyne.dev/memory#sourceKind"),
            "the violated path is the missing required predicate (agent-actionable)"
        );
        assert_eq!(v.severity, "Violation");
        // TEETH: under Halt, NOTHING was written — the projection graph is empty.
        assert_eq!(mem_src_count(&store), 0, "Halt wrote NO memory triple");
        assert_eq!(
            violation_count(&store),
            0,
            "Halt does NOT record to the ledger (it rejects)"
        );
    }

    // ── (3) MALFORMED write — FlagAndAccept => write LANDS + ledger queryable ─

    #[test]
    fn malformed_write_under_flag_and_accept_lands_and_is_queryable_in_ledger() {
        let store = Store::new().unwrap();
        let gate = gate_then_apply(
            &store,
            ValidationPolicy::FlagAndAccept,
            &malformed_desired(),
        )
        .expect("FlagAndAccept proceeds (the write lands)");
        let flagged = match gate {
            ValidationGate::Proceed { flagged } => flagged,
            other => panic!("expected Proceed verdict, got {other:?}"),
        };
        assert!(
            !flagged.is_empty(),
            "the flagged violations are reported back"
        );

        // TEETH (a): the write LANDED despite being malformed.
        assert_eq!(
            mem_src_count(&store),
            1,
            "FlagAndAccept lands the malformed write"
        );

        // TEETH (b): the violation is QUERYABLE back from :projection:violations
        // via SPARQL — a real Meaningful Object, not just an in-memory struct.
        let g = violations_projection_graph_iri(GRAPH);
        let vns = "http://mnemosyne.dev/memory/violation#";
        let rows = count_q(
            &store,
            &format!(
                "SELECT ?v ?focus ?path WHERE {{ GRAPH <{g}> {{ \
                 ?v a <{vns}Violation> ; \
                    <{vns}focusNode> ?focus ; \
                    <{vns}path> ?path ; \
                    <http://www.w3.org/ns/prov#wasAttributedTo> ?obs }} }}"
            ),
        );
        assert_eq!(rows, 1, "the flagged violation is queryable back as an MO");

        // And the recorded path is the agent-actionable detail (mem:sourceKind).
        let path_rows = count_q(
            &store,
            &format!(
                "SELECT ?v WHERE {{ GRAPH <{g}> {{ ?v <{vns}path> \"http://mnemosyne.dev/memory#sourceKind\" }} }}"
            ),
        );
        assert_eq!(
            path_rows, 1,
            "the ledger records the offending property path"
        );
    }

    // ── (4) ANTI-TAUTOLOGY: the malformed case really fails when validation is
    //        ON, and really passes (writes, un-flagged) when it is OFF. ────────

    #[test]
    fn bend_and_revert_malformed_fails_on_validation_passes_off() {
        // ON (Halt): the malformed write is REJECTED (the seam has teeth).
        let on = Store::new().unwrap();
        let on_gate = gate_then_apply(&on, ValidationPolicy::Halt, &malformed_desired()).unwrap();
        assert!(
            matches!(on_gate, ValidationGate::Halt { .. }),
            "validation ON must REJECT the malformed write"
        );
        assert_eq!(mem_src_count(&on), 0, "ON: nothing written");

        // OFF (the revert): the SAME malformed write goes through, un-flagged —
        // proving the rejection above was the validator doing real work, not an
        // unrelated planner/sink error.
        let off = Store::new().unwrap();
        let off_gate = gate_then_apply(&off, ValidationPolicy::Off, &malformed_desired()).unwrap();
        assert!(
            matches!(off_gate, ValidationGate::Proceed { flagged } if flagged.is_empty()),
            "validation OFF must accept the SAME malformed write with no flag"
        );
        assert_eq!(
            mem_src_count(&off),
            1,
            "OFF: the malformed write lands (legacy)"
        );
        assert_eq!(
            violation_count(&off),
            0,
            "OFF: no ledger entry (validation never ran)"
        );
    }

    // ── (4b) CA-1 SEVERITY TIERING: a sh:Warning advisory NEVER blocks, even under
    //         Halt — it flags to the ledger and the write PROCEEDS. A sh:Violation
    //         under Halt rejects. The tier rides the shape's own severity (NOT a
    //         per-shape hard-code), proven through the live agent-core contract whose
    //         vocab_to_shacl now emits the §3 shapes. ─────────────────────────────

    /// A `mnemo:BlockValuation` in a NON-witness-scoped salience graph trips ONLY the
    /// §3 I1-Valuation ADVISORY (sh:Warning) — no Violation-severity §3 shape targets
    /// BlockValuation. The honest-gap surface (salience not yet per-observer).
    fn unscoped_valuation() -> Vec<Triple> {
        let s = "urn:mnemosyne:local:graph:lab:projection:salience:block:abc";
        vec![(
            s.to_string(),
            RDF_TYPE_IRI.to_string(),
            t_uri("https://mnemosyne.local/ns#BlockValuation"),
        )]
    }

    /// TEETH (live path, no-mock): under `Halt`, a sh:Warning advisory (I1-Valuation,
    /// an unscoped salience valuation) does NOT halt — the write PROCEEDS and the
    /// advisory is recorded to the ledger. This is the tier: warnings are advisory,
    /// only sh:Violation is Halt-eligible. The agent-core contract's §3 shapes reach
    /// the gate via the now-wired vocab_to_shacl emit.
    #[test]
    fn warning_advisory_does_not_halt_even_under_halt_policy() {
        let store = Store::new().unwrap();
        let contract = crate::emporium::contract::get_vocabulary("sophia-agent-core")
            .expect("agent-core registered");
        // The salience membrane the valuation lives in (no :agent: segment → the
        // advisory fires). Load the data under THIS graph for the sh:select context.
        let salience = "urn:mnemosyne:local:graph:lab:projection:salience";
        let gate = validate_memory_write(
            &store,
            GRAPH,
            ValidationPolicy::Halt,
            contract,
            &unscoped_valuation(),
            salience,
            OBS_AT,
        )
        .expect("the gate does not error on a warning-only outcome");
        // The tier: under Halt, a warning-only result PROCEEDS (not Halt).
        let flagged = match gate {
            ValidationGate::Proceed { flagged } => flagged,
            other => panic!("a sh:Warning advisory must NOT halt; got {other:?}"),
        };
        assert!(
            flagged.iter().any(|v| v.severity == "Warning"
                && v.shape.as_deref()
                    == Some("http://mnemosyne.dev/agent#I1_ValuationMembraneAdvisory")),
            "the I1-Valuation advisory is flagged (not blocking): {flagged:?}"
        );
        assert!(
            flagged.iter().all(|v| v.severity != "Violation"),
            "no blocking violation in a warning-only outcome"
        );
        // The advisory is durably recorded to the ledger MO (observable gap).
        let g = violations_projection_graph_iri(GRAPH);
        let vns = "http://mnemosyne.dev/memory/violation#";
        let rows = count_q(
            &store,
            &format!(
                "SELECT ?v WHERE {{ GRAPH <{g}> {{ ?v a <{vns}Violation> ; <{vns}severity> \"Warning\" }} }}"
            ),
        );
        assert_eq!(
            rows, 1,
            "the advisory is recorded to the ledger under Halt (proceed)"
        );
    }

    /// CONTRAST (the teeth on the tier): a sh:Violation §3 invariant (I1-membrane: a
    /// witnessless MemoryRecord INSIDE a per-observer membrane) under `Halt` DOES halt —
    /// nothing is written, the structured violation is returned. Same contract, same
    /// gate, different severity → opposite verdict. Proves the tier is severity-driven.
    #[test]
    fn blocking_violation_halts_under_halt_policy_same_contract() {
        let store = Store::new().unwrap();
        let contract = crate::emporium::contract::get_vocabulary("sophia-agent-core")
            .expect("agent-core registered");
        let membrane = "urn:mnemosyne:local:graph:lab:projection:memory:agent:agent-deadbeef";
        let witnessless = vec![(
            "urn:rec:naked".to_string(),
            RDF_TYPE_IRI.to_string(),
            t_uri(&format!("{MEM}MemoryRecord")),
        )];
        let gate = validate_memory_write(
            &store,
            GRAPH,
            ValidationPolicy::Halt,
            contract,
            &witnessless,
            membrane,
            OBS_AT,
        )
        .expect("the gate is a verdict, not a fault");
        let violations = match gate {
            ValidationGate::Halt { violations } => violations,
            other => panic!("a sh:Violation §3 invariant must HALT under Halt; got {other:?}"),
        };
        assert!(
            violations.iter().all(is_blocking),
            "the Halt payload carries ONLY blocking violations: {violations:?}"
        );
        assert!(
            violations.iter().any(|v| v.shape.as_deref()
                == Some("http://mnemosyne.dev/agent#I1_MembraneWitnessShape")),
            "the blocking I1-membrane invariant is the reason for the halt: {violations:?}"
        );
        // TEETH: under Halt, nothing landed in the ledger (a rejected write leaves no trace).
        assert_eq!(
            violation_count(&store),
            0,
            "Halt records nothing to the ledger"
        );
    }

    // ── (5) END-TO-END with the REAL PLANNER: a well-formed `remember` batch
    //        planned by `plan_memory_compute` conforms + lands under Halt. This
    //        binds the gate to the planner's ACTUAL desired_inserts (not a hand-
    //        built triple set), proving the field the planner now populates is the
    //        right one to validate. ───────────────────────────────────────────
    #[test]
    fn real_planner_well_formed_batch_conforms_under_halt() {
        let store = Store::new().unwrap();
        let contract = memory_core_vocabulary();
        let recs = vec![record("vera prefers the fish shell")];
        let plan = plan_memory_compute(contract, GRAPH, &recs, &mem_live(), &[])
            .expect("plan a well-formed remember batch");
        assert!(
            !plan.desired_inserts.is_empty(),
            "the planner populated desired_inserts"
        );

        // The gate over the planner's real desired_inserts conforms under Halt.
        let gate = validate_memory_write(
            &store,
            GRAPH,
            ValidationPolicy::Halt,
            contract,
            &plan.desired_inserts,
            &plan.memory_graph_iri(GRAPH),
            OBS_AT,
        )
        .expect("gate runs");
        assert!(
            matches!(gate, ValidationGate::Proceed { .. }),
            "a real well-formed planned batch conforms under Halt"
        );

        // And applying the plan's steps lands the record in :projection:memory.
        apply_plan_to_store(&store, &plan).expect("apply the conformant plan");
        let mem = memory_projection_graph_iri(GRAPH);
        let recs_in = count_q(
            &store,
            &format!("SELECT ?s WHERE {{ GRAPH <{mem}> {{ ?s a <{MEM}MemoryRecord> }} }}"),
        );
        assert_eq!(recs_in, 1, "the conformant memory record materialized");
    }
}

/// ── PER-AGENT GEIST — the acceptance organism (Variant A + B) ────────────────
///
/// The deliverable proof, driven end-to-end through the REAL planner + REAL sink
/// (`plan_memory_compute` → `run_memory_update` → `graph_wrap_memory` →
/// `SparqlEvaluator`) against an in-memory oxigraph `Store` — NO mocks, NO
/// AppHandle. It pins the five acceptance criteria on a fresh in-test graph:
/// (1) observer isolation (distinct subjects), (2) per-observer recall, (3) no
/// cross-agent supersession + self-supersession works (fix #1), (4) Song isolation
/// (fix #3's GRAPH-scoped DELETE), (5) byte-identity of the empty-observer hash
/// (fix #2's conditional segment).
#[cfg(test)]
mod per_observer_acceptance {
    use super::*;

    use oxigraph::sparql::QueryResults;

    use crate::emporium::contract::memory_core_vocabulary;
    use crate::emporium::planner::{memory_record_subject, plan_memory_compute, Plan, Step};
    use crate::emporium::schemas::{MemoryRecordIn, SourceRefIn};
    use crate::emporium::survey::{FolderEntry, Live};
    use crate::rdf_authority::memory_projection_graph_iri_for;

    const GRAPH: &str = "lab";
    const MEM_TYPE: &str = "<http://mnemosyne.dev/memory#MemoryRecord>";
    const NS: &str = "http://mnemosyne.dev/memory#";

    fn live_with_folder() -> Live {
        let mut live = Live {
            graph: GRAPH.to_string(),
            prefix: format!("urn:mnemosyne:local:graph:{GRAPH}"),
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

    /// A record attributed to `observer` (empty ⇒ shared commons). Same content
    /// shape across observers, so AC1's "identical content, distinct subject" has
    /// teeth: ONLY the observer differs.
    fn record_for(observer: &str, content: &str) -> MemoryRecordIn {
        MemoryRecordIn {
            client_ref: Some("r-acc".to_string()),
            scope: "agent".to_string(),
            kind: "ClaimMemory".to_string(),
            content_orientation: "knowledge".to_string(),
            visibility: "private".to_string(),
            status: "active".to_string(),
            content: content.to_string(),
            source_refs: vec![SourceRefIn {
                source_kind: "DocumentBlock".to_string(),
                source_label: None,
                block_id: Some("abc".to_string()),
                document_id: Some("doc-shell".to_string()),
                external_id: None,
                external_uri: None,
                observed_at: None,
                trust_tier: None,
            }],
            evidence: vec![],
            observed_at: Some(1_718_700_000_000),
            valid_from: Some(1_718_700_000_000),
            is_current: Some(true),
            confidence: None,
            valence: None,
            agent_id: Some("gamma".to_string()),
            observer_agent_id: if observer.is_empty() {
                None
            } else {
                Some(observer.to_string())
            },
            tags: vec![],
            supersedes_ref: None,
            contradicts_ref: None,
        }
    }

    fn apply_plan(store: &Store, plan: &Plan) {
        let mem_graph = plan.memory_graph_iri(GRAPH);
        for step in &plan.steps {
            if let Step::SparqlUpdate { update } = step {
                run_memory_update(store, &mem_graph, update).expect("sink update");
            }
        }
    }

    fn count(store: &Store, query: &str) -> usize {
        match SparqlEvaluator::new()
            .parse_query(query)
            .expect("parse query")
            .on_store(store)
            .execute()
            .expect("execute query")
        {
            QueryResults::Solutions(s) => s.count(),
            _ => panic!("expected SELECT solutions"),
        }
    }

    /// Read the live triples back out of an observer's projection graph as the
    /// planner's diff input — the store-truth analog of `current_memory_triples`
    /// (same per-observer graph, same no-trailing-colon subject prefix).
    fn survey_for(store: &Store, observer: &str) -> Vec<crate::emporium::terms::Triple> {
        let mem = memory_projection_graph_iri_for(GRAPH, observer);
        let q = format!(
            "SELECT ?s ?p ?o WHERE {{ GRAPH <{mem}> {{ ?s ?p ?o . FILTER(STRSTARTS(STR(?s), \"{mem}\")) }} }}"
        );
        let solutions = match SparqlEvaluator::new()
            .parse_query(&q)
            .expect("parse survey")
            .on_store(store)
            .execute()
            .expect("execute survey")
        {
            QueryResults::Solutions(s) => s,
            _ => panic!("expected solutions"),
        };
        let mut out = Vec::new();
        for sol in solutions {
            let sol = sol.expect("row");
            let s = match sol.get("s").expect("?s") {
                oxigraph::model::Term::NamedNode(n) => n.as_str().to_string(),
                other => panic!("non-IRI subject: {other}"),
            };
            let p = match sol.get("p").expect("?p") {
                oxigraph::model::Term::NamedNode(n) => n.as_str().to_string(),
                other => panic!("non-IRI predicate: {other}"),
            };
            let o = crate::emporium::survey::parse_term(&sol.get("o").expect("?o").to_string());
            out.push((s, p, o));
        }
        out
    }

    /// File a batch attributed to `observer` against its own per-observer survey
    /// (the spine's exact sequence: survey same-observer graph → plan → apply).
    fn file(store: &Store, observer: &str, records: &[MemoryRecordIn]) -> Plan {
        let contract = memory_core_vocabulary();
        let current = survey_for(store, observer);
        let plan = plan_memory_compute(contract, GRAPH, records, &live_with_folder(), &current)
            .expect("plan");
        apply_plan(store, &plan);
        plan
    }

    /// AC1 + AC5 — observer isolation AND byte-identity of the empty-observer hash.
    #[test]
    fn ac1_isolation_and_ac5_byte_identity() {
        // AC5 (fix #2): an EMPTY observer reproduces the pre-bump subject IRI
        // byte-for-byte. The golden was computed independently (sha256 over the
        // pre-`@1.1.0` 4-separator recipe) — see the planner report. If the
        // conditional 5th segment regressed (an unconditional separator), this flips.
        const PRE_BUMP_GOLDEN: &str = "urn:mnemosyne:local:graph:lab:projection:memory:record:\
4c60fd09c1936825e46c9786726aed8a7284c2021130063db4a1dbe1c5aa53be";
        let commons = record_for("", "isolation under test");
        assert_eq!(
            memory_record_subject(GRAPH, &commons),
            PRE_BUMP_GOLDEN,
            "empty-observer subject MUST equal the pre-bump golden (fix #2 conditional segment)"
        );

        // AC1 (Variant A): two DISTINCT observers, IDENTICAL content → DISTINCT
        // subjects. Pre-fix these aliased to ONE subject IRI.
        let a = record_for("agent-aaaa", "isolation under test");
        let b = record_for("agent-bbbb", "isolation under test");
        let sa = memory_record_subject(GRAPH, &a);
        let sb = memory_record_subject(GRAPH, &b);
        assert_ne!(
            sa, sb,
            "AC1: identical content from distinct observers must NOT alias"
        );
        assert!(
            sa.contains(":memory:agent:agent-aaaa:record:"),
            "A under A's perspective graph"
        );
        assert!(
            sb.contains(":memory:agent:agent-bbbb:record:"),
            "B under B's perspective graph"
        );
        // And both differ from the commons subject (the observer changed the hash).
        assert_ne!(sa, PRE_BUMP_GOLDEN);
        assert_ne!(sb, PRE_BUMP_GOLDEN);
    }

    /// AC2 — per-observer recall returns ONLY the observer's memories.
    #[test]
    fn ac2_per_observer_recall_is_isolated() {
        let store = Store::new().expect("store");
        file(
            &store,
            "agent-aaaa",
            &[record_for("agent-aaaa", "A-only fact one")],
        );
        file(
            &store,
            "agent-aaaa",
            &[record_for("agent-aaaa", "A-only fact two")],
        );
        file(
            &store,
            "agent-bbbb",
            &[record_for("agent-bbbb", "B-only fact")],
        );

        let recall = |observer: &str, query: &str| {
            super::recall_for_observer_in_store(&store, GRAPH, observer, query, 10)
        };

        // A's recall sees BOTH of A's records and NONE of B's.
        let a_hits = recall("agent-aaaa", "fact");
        assert_eq!(
            a_hits.len(),
            2,
            "AC2: A recalls 100% of its own ({a_hits:?})"
        );
        assert!(
            a_hits
                .iter()
                .all(|h| h.contains(":memory:agent:agent-aaaa:")),
            "AC2: every A hit is in A's perspective graph"
        );
        // ∩ B-only = ∅: A never sees B's content.
        let b_hits = recall("agent-bbbb", "fact");
        assert_eq!(b_hits.len(), 1, "B recalls only its own record");
        let a_set: std::collections::BTreeSet<_> = a_hits.iter().collect();
        let b_set: std::collections::BTreeSet<_> = b_hits.iter().collect();
        assert!(a_set.is_disjoint(&b_set), "AC2: A∩B = ∅");
    }

    /// AC3 — no cross-agent supersession; self-supersession DOES work (fix #1).
    #[test]
    fn ac3_no_cross_agent_supersession_but_self_supersession_works() {
        let store = Store::new().expect("store");
        let mem_a = memory_projection_graph_iri_for(GRAPH, "agent-aaaa");

        // A files a head.
        let a_head = record_for("agent-aaaa", "the sky is blue");
        let a_head_subj = memory_record_subject(GRAPH, &a_head);
        file(&store, "agent-aaaa", &[a_head]);

        let active = |graph: &str, subj: &str| {
            count(
                &store,
                &format!(
                    "SELECT ?x WHERE {{ GRAPH <{graph}> {{ <{subj}> <{NS}status> \"active\" }} }}"
                ),
            )
        };
        assert_eq!(active(&mem_a, &a_head_subj), 1, "A's head is active");

        // ── B tries to supersede A's record by naming A's subject IRI. ──
        // B's write is surveyed against B's OWN graph (which does NOT contain A's
        // subject), so the demote pass sees A's subject as "not a live record" and
        // emits NO demote — A's head is untouched (the GRAPH-scoping + per-observer
        // survey make cross-agent supersession structurally impossible).
        let mut b_super = record_for("agent-bbbb", "the sky is green");
        b_super.supersedes_ref = Some(a_head_subj.clone());
        let b_plan = file(&store, "agent-bbbb", &[b_super]);
        assert!(
            b_plan
                .warnings
                .iter()
                .any(|w| w.contains("not a live record")),
            "AC3: B's cross-agent supersedesRef is a warning, not a demote ({:?})",
            b_plan.warnings
        );
        assert_eq!(
            active(&mem_a, &a_head_subj),
            1,
            "AC3: A's head STAYS active — B cannot supersede across the observer boundary"
        );

        // ── A supersedes its OWN prior head (the headline accumulation scenario,
        // e.g. across a respawn). This MUST work — proving fix #1: the survey is
        // scoped to A's graph, NOT empty, so the demote pass sees A's live head. ──
        let mut a_correction = record_for("agent-aaaa", "the sky is grey today");
        a_correction.supersedes_ref = Some(a_head_subj.clone());
        let a_plan = file(&store, "agent-aaaa", &[a_correction]);
        assert!(
            a_plan.warnings.is_empty()
                || !a_plan
                    .warnings
                    .iter()
                    .any(|w| w.contains("not a live record")),
            "AC3/fix#1: A's self-supersession must NOT warn 'not a live record' ({:?})",
            a_plan.warnings
        );
        let superseded = count(
            &store,
            &format!("SELECT ?x WHERE {{ GRAPH <{mem_a}> {{ <{a_head_subj}> <{NS}status> \"superseded\" }} }}"),
        );
        assert_eq!(
            superseded, 1,
            "AC3/fix#1: A's OWN prior head IS demoted to superseded (survey scoped, not empty)"
        );
        assert_eq!(
            active(&mem_a, &a_head_subj),
            0,
            "the superseded head is no longer active"
        );
    }

    /// AC5 belt-and-suspenders + named-graph routing — the two observers' records
    /// land in DISTINCT named graphs, and neither lands in the commons graph.
    #[test]
    fn records_route_to_distinct_per_observer_graphs() {
        let store = Store::new().expect("store");
        file(
            &store,
            "agent-aaaa",
            &[record_for("agent-aaaa", "alpha fact")],
        );
        file(
            &store,
            "agent-bbbb",
            &[record_for("agent-bbbb", "beta fact")],
        );

        let mem_a = memory_projection_graph_iri_for(GRAPH, "agent-aaaa");
        let mem_b = memory_projection_graph_iri_for(GRAPH, "agent-bbbb");
        let commons = memory_projection_graph_iri_for(GRAPH, "");

        let recs_in = |g: &str| {
            count(
                &store,
                &format!("SELECT ?s WHERE {{ GRAPH <{g}> {{ ?s a {MEM_TYPE} }} }}"),
            )
        };
        assert_eq!(recs_in(&mem_a), 1, "A's record in A's graph");
        assert_eq!(recs_in(&mem_b), 1, "B's record in B's graph");
        assert_eq!(
            recs_in(&commons),
            0,
            "NOTHING leaks into the shared commons graph"
        );

        // The PROV attribution of each perspective graph is present. A BARE-token
        // observer is serialized as the canonical agent-URN IRI form.
        let obs_iri = |obs: &str| crate::rdf_authority::observer_iri(obs).expect("non-empty");
        let prov = |g: &str, obs: &str| {
            count(
                &store,
                &format!(
                    "SELECT ?x WHERE {{ GRAPH <{g}> {{ <{g}> <http://www.w3.org/ns/prov#wasAttributedTo> <{}> }} }}",
                    obs_iri(obs)
                ),
            )
        };
        assert_eq!(
            prov(&mem_a, "agent-aaaa"),
            1,
            "A's graph is prov:wasAttributedTo A"
        );
        assert_eq!(
            prov(&mem_b, "agent-bbbb"),
            1,
            "B's graph is prov:wasAttributedTo B"
        );

        // mem:observedBy (the canonical observer predicate) is on each record.
        let observed_by = |g: &str, obs: &str| {
            count(
                &store,
                &format!(
                    "SELECT ?s WHERE {{ GRAPH <{g}> {{ ?s <{NS}observedBy> <{}> }} }}",
                    obs_iri(obs)
                ),
            )
        };
        assert_eq!(
            observed_by(&mem_a, "agent-aaaa"),
            1,
            "A's record carries mem:observedBy A"
        );
        assert_eq!(
            observed_by(&mem_b, "agent-bbbb"),
            1,
            "B's record carries mem:observedBy B"
        );
    }
}
