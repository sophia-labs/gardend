//! Backfill the LEGACY Geist memory queue into the typed memory projection —
//! the queue→projection migration tool (reconciliation ruling: deploy first,
//! then backfill; `plans/mo-reconciliation-policy-20260704.md` is the policy
//! frame, this is the data-motion half).
//!
//! The legacy queue (`{graph_dir}/memory/memory-queue.json`, flat
//! `{number, blockId, content, createdAt, lastActive}` rows) predates the
//! `mem:` Meaningful-Object representation, so a freshly deployed cell has a
//! populated queue and an EMPTY `:projection:memory` — every projection-side
//! reader (per-observer recall, choreograph's GATE-FUSE, `/emporium/heads`)
//! sees amnesia. `backfill_memory_projection` files each queue row through the
//! REAL ingest spine (`gather_and_plan` + `apply_and_assert`, the same path
//! `remember` uses) so the projection, the applied-plan journal, and the §6
//! memory event log all stay coherent — a backfilled record is replayable and
//! auditable exactly like a born-`remember` record.
//!
//! DELIBERATE INVERSIONS of the `remember` path:
//! - the queue is NEVER appended (the rows are already there — appending would
//!   duplicate them; see `ingest_memory_batch`'s queue-append seam, skipped);
//! - already-projected subjects are SKIPPED (content-addressed idempotency: the
//!   frozen-hash subject recipe makes a re-run mint the same IRIs, and the
//!   skip also keeps the event log from growing on converged re-files);
//! - provenance is the queue row itself: the record's own `block_id`
//!   (`memory-N`) rides as a `DocumentBlock` source ref (the self-referential
//!   sugar), labeled as backfill, and every record carries the
//!   [`BACKFILL_TAG`] `mem:tag` — backfilled testimony is distinguishable from
//!   born-attributed testimony FOREVER.
//!
//! OBSERVER ATTRIBUTION — the honest split. The queue is witness-blind, so the
//! tool never invents an observer: records land as commons (observer-less)
//! UNLESS the caller supplies `observer_agent_id` + `observer_numbers`, the
//! queue numbers RECOVERED from witnessed events (choreograph's log). Recovery
//! is ratified practice; invention is not. Attribution is part of the subject
//! hash (observer-in-hash), so RE-RUNS MUST PASS THE SAME SPLIT to converge —
//! a record backfilled as commons and re-run as observed would mint a second,
//! differently-attributed subject.

use std::collections::BTreeSet;

use crate::app_runtime::AppHandle;
use crate::{
    emporium::planner::memory_record_subject,
    emporium::schemas::{IngestPayload, IngestRequest, MemoryRecordIn, SourceRefIn},
    emporium::spine::{apply_and_assert, gather_and_plan},
    geist_memory_store::{read_memory_store, LocalMemoryRecord},
    mcp_utils::{mcp_arg_string, mcp_arg_u64_vec, mcp_graph_id_or_default},
    paths::existing_graph_dir,
    rdf_authority::memory_projection_graph_iri_for,
    rdf_service::open_graph_store,
};

/// The `mem:tag` every backfilled record carries — the permanent marker that a
/// record entered the projection via queue backfill, not a live `remember`.
pub(crate) const BACKFILL_TAG: &str = "backfill:memory-queue";

/// One rejected queue row in the report: the queue number + the loud reason.
#[derive(Debug)]
struct Rejection {
    number: u64,
    reason: String,
}

/// `backfill_memory_projection` — ADMIN: file every legacy queue row into
/// `:projection:memory` as a typed `mem:MemoryRecord` via the real spine.
///
/// Arguments:
/// - `graph_id` (optional, defaults to the app's default graph);
/// - `observer_agent_id` + `observer_numbers` (optional PAIR): the queue
///   numbers to attribute to that witness; both-or-neither, and every listed
///   number must exist in the queue (loud up-front reject otherwise — a
///   mis-mapped attribution is worse than no run).
///
/// Returns `{ok, scanned, ingested, skippedExisting, rejected:[{number,
/// reason}], subjects}` — no silent drops: every scanned row is accounted for
/// in exactly one of ingested / skippedExisting / rejected.
pub(super) async fn mcp_local_backfill_memory_projection(
    app: AppHandle,
    arguments: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let graph_id = mcp_graph_id_or_default(&app, arguments)?;
    let observer = mcp_arg_string(arguments, &["observer_agent_id", "observerAgentId"])
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty());
    let observer_numbers: BTreeSet<u64> =
        mcp_arg_u64_vec(arguments, &["observer_numbers", "observerNumbers"])
            .into_iter()
            .collect();

    // The attribution pair is both-or-neither: an observer with no covered
    // numbers attributes nothing (a caller mistake, not a no-op), and numbers
    // with no observer name no witness. Loud-reject both shapes.
    if observer.is_some() && observer_numbers.is_empty() {
        return Err(
            "observer_agent_id without observer_numbers attributes nothing — pass the queue \
             numbers recovered from witnessed events (or omit the observer entirely)"
                .to_string(),
        );
    }
    if observer.is_none() && !observer_numbers.is_empty() {
        return Err(
            "observer_numbers without observer_agent_id names no witness — pass the recovered \
             observer id (or omit the numbers entirely)"
                .to_string(),
        );
    }

    let graph_dir = existing_graph_dir(&app, &graph_id)?;
    let store = read_memory_store(&graph_dir, &graph_id)?;

    // Every attributed number must exist BEFORE any write: a dangling number
    // means the caller's recovered mapping is wrong, and partial attribution
    // under a wrong mapping is silent mis-testimony.
    let missing: Vec<u64> = observer_numbers
        .iter()
        .filter(|n| !store.memories.contains_key(&n.to_string()))
        .copied()
        .collect();
    if !missing.is_empty() {
        return Err(format!(
            "observer_numbers {missing:?} not found in the memory queue — nothing was written \
             (check the recovered witnessed-event mapping)"
        ));
    }

    // Queue-number order (the BTreeMap is keyed by number-as-STRING, which is
    // NOT numeric order — "10" < "2").
    let mut rows: Vec<&LocalMemoryRecord> = store.memories.values().collect();
    rows.sort_by_key(|m| m.number);

    let scanned = rows.len();
    let mut ingested: Vec<String> = Vec::new();
    let mut skipped_existing: u64 = 0;
    let mut rejected: Vec<Rejection> = Vec::new();

    for row in rows {
        let record_observer = if observer_numbers.contains(&row.number) {
            observer.as_deref()
        } else {
            None
        };
        let record = match queue_row_to_memory_record(row, record_observer) {
            Ok(record) => record,
            Err(reason) => {
                rejected.push(Rejection {
                    number: row.number,
                    reason,
                });
                continue;
            }
        };
        let subject = memory_record_subject(&graph_id, &record);

        // Per-graph write gate across exists-check → plan → apply: the same
        // serialization `ingest_memory_batch` holds, so a racing live remember
        // cannot interleave between the dedupe read and the write.
        let _gate = crate::emporium::write_gate::acquire_write_gate(&graph_id).await;

        match subject_in_projection(&app, &graph_id, record_observer.unwrap_or(""), &subject) {
            Ok(true) => {
                skipped_existing += 1;
                continue;
            }
            Ok(false) => {}
            Err(reason) => {
                rejected.push(Rejection {
                    number: row.number,
                    reason: format!("projection existence check failed: {reason}"),
                });
                continue;
            }
        }

        let request = IngestRequest {
            vocab: "memory".to_string(),
            dry_run: false,
            replace_class: false,
            payload: IngestPayload::Memory {
                records: vec![record],
            },
        };
        // The LOUD shape gate (I1 + enum membership) — catches degenerate
        // legacy rows (e.g. empty content) per-record, not whole-call.
        if let Err(reason) = request.validate() {
            rejected.push(Rejection {
                number: row.number,
                reason,
            });
            continue;
        }

        let planned = match gather_and_plan(&app, &graph_id, &request) {
            Ok(planned) => planned,
            Err(error) => {
                rejected.push(Rejection {
                    number: row.number,
                    reason: error.message_ref().to_string(),
                });
                continue;
            }
        };
        let report =
            apply_and_assert(&app, &graph_id, &planned.plan, planned.contract, &request).await;
        if !report.ok {
            // Loud halt: the applier already journaled to `memory/failures/`.
            let reason = report
                .steps
                .iter()
                .rev()
                .find_map(|s| s.extra.get("error").and_then(serde_json::Value::as_str))
                .map(str::to_string)
                .unwrap_or_else(|| "memory apply halted".to_string());
            rejected.push(Rejection {
                number: row.number,
                reason,
            });
            continue;
        }
        // NO queue append — the row is already the queue's. This is the whole
        // point of the tool existing instead of re-calling `remember`.
        ingested.push(subject);
    }

    let rejected_json: Vec<serde_json::Value> = rejected
        .iter()
        .map(|r| serde_json::json!({ "number": r.number, "reason": r.reason }))
        .collect();
    Ok(serde_json::json!({
        "ok": rejected.is_empty(),
        "scanned": scanned,
        "ingested": ingested.len(),
        "skipped_existing": skipped_existing,
        "skippedExisting": skipped_existing,
        "rejected": rejected_json,
        "subjects": ingested,
        "observer_agent_id": observer,
        "observerAgentId": observer,
        "source": "memory-projection-backfill",
    }))
}

/// Build the typed [`MemoryRecordIn`] for one legacy queue row. The typed
/// defaults mirror `build_remember_record`'s (scope=agent, kind=ClaimMemory,
/// orientation=knowledge, visibility=private, status=active) — the queue never
/// carried these dimensions, and the `remember` defaults are the ratified
/// default shape. `observedAt` is the row's `createdAt` epoch-millis (part of
/// the frozen subject hash, so determinism REQUIRES the parse — a garbled
/// legacy stamp is a per-row rejection, not a now() fallback that would mint a
/// fresh subject every run).
fn queue_row_to_memory_record(
    row: &LocalMemoryRecord,
    observer: Option<&str>,
) -> Result<MemoryRecordIn, String> {
    let created_ms: i64 = row.created_at.trim().parse().map_err(|_| {
        format!(
            "createdAt '{}' is not epoch-millis — cannot derive a deterministic observedAt",
            row.created_at
        )
    })?;
    Ok(MemoryRecordIn {
        client_ref: Some(format!("backfill-{}", row.block_id)),
        scope: "agent".to_string(),
        kind: "ClaimMemory".to_string(),
        content_orientation: "knowledge".to_string(),
        visibility: "private".to_string(),
        status: "active".to_string(),
        content: row.content.clone(),
        // The self-referential provenance sugar: the queue row IS the source.
        // `external_id` keys each row to its own SourceReference subject (it
        // rides the src content-hash); `source_label` is the human-readable
        // backfill marker on the source node itself.
        source_refs: vec![SourceRefIn {
            source_kind: "DocumentBlock".to_string(),
            source_label: Some(format!(
                "backfill: legacy memory-queue.json #{}",
                row.number
            )),
            block_id: Some(row.block_id.clone()),
            document_id: None,
            external_id: Some(format!("memory-queue:{}", row.block_id)),
            external_uri: None,
            observed_at: Some(created_ms),
            trust_tier: None,
        }],
        evidence: Vec::new(),
        observed_at: Some(created_ms),
        valid_from: None,
        is_current: None,
        confidence: None,
        valence: None,
        agent_id: None,
        observer_agent_id: observer.map(str::to_string),
        tags: vec![BACKFILL_TAG.to_string()],
        supersedes_ref: None,
        contradicts_ref: None,
    })
}

/// ASK whether `subject` already has ANY triple in the (per-observer) memory
/// projection graph — the idempotency read. Content-addressed subjects make
/// this deterministic: a re-run of the same row + same attribution recomputes
/// the same IRI.
fn subject_in_projection(
    app: &AppHandle,
    graph_id: &str,
    observer: &str,
    subject: &str,
) -> Result<bool, String> {
    use oxigraph::sparql::{QueryResults, SparqlEvaluator};
    let graph_dir = existing_graph_dir(app, graph_id)?;
    let store = open_graph_store(&graph_dir)?;
    let mem_graph = memory_projection_graph_iri_for(graph_id, observer);
    let query = format!("ASK {{ GRAPH <{mem_graph}> {{ <{subject}> ?p ?o }} }}");
    match SparqlEvaluator::new()
        .parse_query(&query)
        .map_err(|e| format!("parse projection ASK: {e}"))?
        .on_store(&store)
        .execute()
        .map_err(|e| format!("execute projection ASK: {e}"))?
    {
        QueryResults::Boolean(present) => Ok(present),
        _ => Err("projection ASK returned a non-boolean result".to_string()),
    }
}

// The mock-cell harness (`build_mock_app_for_tests` / `profile_env_serial`) is
// gated behind the headless feature, same as `emporium/state_trace_tests.rs` —
// these run in the `--no-default-features --features headless` matrix.
#[cfg(all(test, feature = "headless"))]
mod tests {
    use super::*;
    use crate::emporium::memory_events::read_memory_events;
    use crate::geist_memory_store::{write_memory_store, LocalMemoryStore};
    use crate::graph_service::{create_graph_service, CreateGraphInput};
    use crate::rdf_service::{run_sparql_query_service, SparqlInput};
    use std::collections::{BTreeMap as StdBTreeMap, BTreeSet as StdBTreeSet};
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    // ── harness (mirrors emporium/state_trace_tests.rs) ─────────────────────

    fn env_serial() -> &'static std::sync::Mutex<()> {
        crate::tauri_runtime::profile_env_serial()
    }

    fn temp_profile(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        std::env::temp_dir().join(format!("garden-memory-backfill-{name}-{nanos}"))
    }

    fn mock_app() -> AppHandle {
        crate::tauri_runtime::build_mock_app_for_tests(true)
    }

    fn seed_graph(app: &AppHandle, graph_id: &str) {
        create_graph_service(
            app,
            CreateGraphInput {
                graph_id: Some(graph_id.to_string()),
                title: "Backfill Lab".to_string(),
                description: None,
                operation_id: None,
            },
        )
        .expect("create graph");
    }

    /// Seed a REAL legacy `memory-queue.json`: rows 1..=3 valid, row 4 with a
    /// garbled (non-epoch) createdAt to exercise per-row rejection.
    fn seed_queue(app: &AppHandle, graph_id: &str) {
        let graph_dir = existing_graph_dir(app, graph_id).expect("graph dir");
        let mut memories = StdBTreeMap::new();
        let mut put = |number: u64, content: &str, created_at: &str| {
            memories.insert(
                number.to_string(),
                LocalMemoryRecord {
                    number,
                    block_id: format!("memory-{number}"),
                    content: content.to_string(),
                    created_at: created_at.to_string(),
                    last_active: created_at.to_string(),
                },
            );
        };
        put(1, "vera prefers fish CLI", "1718700000001");
        put(
            2,
            "the vehicle room defaults to honest identities",
            "1718700000002",
        );
        put(
            3,
            "cow-tools is the Uruguay open-data cell",
            "1718700000003",
        );
        put(4, "garbled legacy row", "not-a-timestamp");
        let store = LocalMemoryStore {
            schema_version: 1,
            graph_id: graph_id.to_string(),
            next_number: 5,
            memories,
            archives: Vec::new(),
        };
        write_memory_store(&graph_dir, &store).expect("write memory queue");
    }

    fn select(app: &AppHandle, graph_id: &str, query: &str) -> Vec<StdBTreeMap<String, String>> {
        run_sparql_query_service(
            app.clone(),
            SparqlInput {
                graph_id: graph_id.to_string(),
                query: query.to_string(),
            },
        )
        .expect("sparql query")
        .rows
    }

    fn all_triples(
        app: &AppHandle,
        graph_id: &str,
        named_graph: &str,
    ) -> StdBTreeSet<(String, String, String)> {
        let q = format!(
            "SELECT ?s ?p ?o WHERE {{ GRAPH <{named_graph}> {{ ?s ?p ?o }} }} ORDER BY ?s ?p ?o"
        );
        select(app, graph_id, &q)
            .into_iter()
            .map(|row| {
                (
                    row.get("s").cloned().unwrap_or_default(),
                    row.get("p").cloned().unwrap_or_default(),
                    row.get("o").cloned().unwrap_or_default(),
                )
            })
            .collect()
    }

    fn run_backfill(app: &AppHandle, args: serde_json::Value) -> Result<serde_json::Value, String> {
        crate::app_runtime::async_runtime::block_on(mcp_local_backfill_memory_projection(app.clone(), &args))
    }

    const MEM_NS: &str = "http://mnemosyne.dev/memory#";
    const OBSERVER: &str = "agent-132c7244aabbccdd";

    // ── the end-to-end trace ─────────────────────────────────────────────────

    #[test]
    fn backfill_memory_projection_end_to_end() {
        let _serial = env_serial().lock().unwrap_or_else(|p| p.into_inner());
        let profile = temp_profile("e2e");
        std::env::set_var("GARDEN_PROFILE_DIR", &profile);
        let result = std::panic::catch_unwind(run_backfill_trace);
        std::env::remove_var("GARDEN_PROFILE_DIR");
        let _ = std::fs::remove_dir_all(&profile);
        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
    }

    fn run_backfill_trace() {
        let app = mock_app();
        let graph_id = "backfill-lab";
        seed_graph(&app, graph_id);
        seed_queue(&app, graph_id);

        let commons_graph = memory_projection_graph_iri_for(graph_id, "");
        let observer_graph = memory_projection_graph_iri_for(graph_id, OBSERVER);
        let args = serde_json::json!({
            "graph_id": graph_id,
            "observer_agent_id": OBSERVER,
            "observer_numbers": [2],
        });

        // ── RUN 1: rows 1,3 → commons; row 2 → observer; row 4 → rejected ──
        let report = run_backfill(&app, args.clone()).expect("backfill run 1");
        assert_eq!(report["scanned"], 4, "report: {report:#}");
        assert_eq!(report["ingested"], 3, "report: {report:#}");
        assert_eq!(report["skipped_existing"], 0, "report: {report:#}");
        let rejected = report["rejected"].as_array().expect("rejected array");
        assert_eq!(rejected.len(), 1, "exactly the garbled row rejects");
        assert_eq!(rejected[0]["number"], 4);
        assert!(
            rejected[0]["reason"]
                .as_str()
                .unwrap_or_default()
                .contains("not epoch-millis"),
            "loud reason names the parse failure: {}",
            rejected[0]["reason"]
        );
        assert_eq!(report["ok"], false, "a rejection flips ok");

        // Commons graph: rows 1 and 3, tagged, no observer attribution.
        let commons_contents: Vec<String> = select(
            &app,
            graph_id,
            &format!(
                "SELECT ?c WHERE {{ GRAPH <{commons_graph}> {{ ?s a <{MEM_NS}MemoryRecord> ; \
                 <{MEM_NS}content> ?c }} }} ORDER BY ?c"
            ),
        )
        .into_iter()
        .filter_map(|r| r.get("c").cloned())
        .collect();
        assert_eq!(
            commons_contents,
            vec![
                "\"cow-tools is the Uruguay open-data cell\"".to_string(),
                "\"vera prefers fish CLI\"".to_string(),
            ],
            "commons carries exactly the un-attributed rows"
        );
        let commons_observed_by = select(
            &app,
            graph_id,
            &format!(
                "SELECT ?s WHERE {{ GRAPH <{commons_graph}> {{ ?s <{MEM_NS}observedBy> ?o }} }}"
            ),
        );
        assert!(
            commons_observed_by.is_empty(),
            "commons records carry NO mem:observedBy"
        );
        let tag_rows = select(
            &app,
            graph_id,
            &format!(
                "SELECT ?s WHERE {{ GRAPH <{commons_graph}> {{ ?s <{MEM_NS}tag> \
                 \"{BACKFILL_TAG}\" }} }}"
            ),
        );
        assert_eq!(
            tag_rows.len(),
            2,
            "every backfilled commons record carries the backfill mem:tag"
        );
        // createdAt derives from the queue row's epoch stamp, not now().
        let created = select(
            &app,
            graph_id,
            &format!(
                "SELECT ?t WHERE {{ GRAPH <{commons_graph}> {{ ?s <{MEM_NS}content> \
                 \"vera prefers fish CLI\" ; <{MEM_NS}createdAt> ?t }} }}"
            ),
        );
        assert_eq!(created.len(), 1);
        // 1718700000001 ms == 2024-06-18T08:40:00.001Z (the planner renders
        // epoch-millis as an ISO xsd:dateTime) — the queue stamp, not now().
        assert!(
            created[0]["t"].contains("2024-06-18T08:40:00.001"),
            "createdAt is the queue row's stamp: {}",
            created[0]["t"]
        );
        // Provenance: derivedFrom → a SourceReference labeled as backfill.
        let prov = select(
            &app,
            graph_id,
            &format!(
                "SELECT ?label WHERE {{ GRAPH <{commons_graph}> {{ ?s <{MEM_NS}content> \
                 \"vera prefers fish CLI\" ; <{MEM_NS}derivedFrom> ?src . \
                 ?src <{MEM_NS}sourceLabel> ?label }} }}"
            ),
        );
        assert_eq!(prov.len(), 1, "derivedFrom resolves to a SourceReference");
        assert!(
            prov[0]["label"].contains("backfill: legacy memory-queue.json #1"),
            "the source node names the queue row: {}",
            prov[0]["label"]
        );

        // Observer graph: row 2 only, attributed to the recovered witness.
        let observed = select(
            &app,
            graph_id,
            &format!(
                "SELECT ?c ?w WHERE {{ GRAPH <{observer_graph}> {{ ?s a <{MEM_NS}MemoryRecord> ; \
                 <{MEM_NS}content> ?c ; <{MEM_NS}observedBy> ?w }} }}"
            ),
        );
        assert_eq!(observed.len(), 1, "exactly the attributed row");
        assert_eq!(
            observed[0]["c"],
            "\"the vehicle room defaults to honest identities\""
        );
        assert_eq!(
            observed[0]["w"],
            format!("<urn:sophia:agent:{OBSERVER}>"),
            "mem:observedBy is the recovered witness IRI"
        );

        // The queue itself is untouched (NO double-append).
        let graph_dir = existing_graph_dir(&app, graph_id).expect("graph dir");
        let queue = read_memory_store(&graph_dir, graph_id).expect("read queue");
        assert_eq!(queue.memories.len(), 4, "queue rows unchanged");
        assert_eq!(queue.next_number, 5, "queue next_number unchanged");

        // The §6 memory event log rode along: one accepted event per ingest.
        let events = read_memory_events(&app, graph_id).expect("read event log");
        assert_eq!(events.len(), 3, "one event per accepted backfill row");

        // ── RUN 2 (same attribution): a strict no-op ──
        let commons_before = all_triples(&app, graph_id, &commons_graph);
        let observer_before = all_triples(&app, graph_id, &observer_graph);
        let report2 = run_backfill(&app, args).expect("backfill run 2");
        assert_eq!(report2["scanned"], 4);
        assert_eq!(
            report2["ingested"], 0,
            "re-run ingests nothing: {report2:#}"
        );
        assert_eq!(report2["skipped_existing"], 3, "all landed rows skip");
        assert_eq!(
            report2["rejected"].as_array().map(Vec::len),
            Some(1),
            "the garbled row still rejects (still no silent drop)"
        );
        assert_eq!(
            all_triples(&app, graph_id, &commons_graph),
            commons_before,
            "commons projection is byte-stable across re-runs"
        );
        assert_eq!(
            all_triples(&app, graph_id, &observer_graph),
            observer_before,
            "observer projection is byte-stable across re-runs"
        );
        let events2 = read_memory_events(&app, graph_id).expect("read event log");
        assert_eq!(
            events2.len(),
            3,
            "the skip keeps the event log from growing on re-runs"
        );
    }

    // ── argument validation: loud rejects, nothing written ──────────────────

    #[test]
    fn backfill_argument_validation_is_loud_and_writes_nothing() {
        let _serial = env_serial().lock().unwrap_or_else(|p| p.into_inner());
        let profile = temp_profile("args");
        std::env::set_var("GARDEN_PROFILE_DIR", &profile);
        let result = std::panic::catch_unwind(run_argument_validation);
        std::env::remove_var("GARDEN_PROFILE_DIR");
        let _ = std::fs::remove_dir_all(&profile);
        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
    }

    fn run_argument_validation() {
        let app = mock_app();
        let graph_id = "backfill-args-lab";
        seed_graph(&app, graph_id);

        // Empty queue: a clean zero report, not an error.
        let report = run_backfill(&app, serde_json::json!({ "graph_id": graph_id }))
            .expect("empty-queue backfill");
        assert_eq!(report["scanned"], 0);
        assert_eq!(report["ingested"], 0);
        assert_eq!(report["ok"], true);

        seed_queue(&app, graph_id);

        // Observer without numbers → loud reject.
        let err = run_backfill(
            &app,
            serde_json::json!({ "graph_id": graph_id, "observer_agent_id": OBSERVER }),
        )
        .expect_err("observer without numbers must reject");
        assert!(err.contains("observer_numbers"), "loud reason: {err}");

        // Numbers without observer → loud reject.
        let err = run_backfill(
            &app,
            serde_json::json!({ "graph_id": graph_id, "observer_numbers": [1] }),
        )
        .expect_err("numbers without observer must reject");
        assert!(err.contains("observer_agent_id"), "loud reason: {err}");

        // A number missing from the queue → loud reject BEFORE any write.
        let err = run_backfill(
            &app,
            serde_json::json!({
                "graph_id": graph_id,
                "observer_agent_id": OBSERVER,
                "observer_numbers": [2, 99],
            }),
        )
        .expect_err("missing queue number must reject the whole call");
        assert!(err.contains("99"), "the missing number is named: {err}");

        // NOTHING was written by any of the rejected calls.
        let commons_graph = memory_projection_graph_iri_for(graph_id, "");
        let observer_graph = memory_projection_graph_iri_for(graph_id, OBSERVER);
        assert!(
            all_triples(&app, graph_id, &commons_graph).is_empty(),
            "commons projection stays empty after loud rejects"
        );
        assert!(
            all_triples(&app, graph_id, &observer_graph).is_empty(),
            "observer projection stays empty after loud rejects"
        );
    }
}
