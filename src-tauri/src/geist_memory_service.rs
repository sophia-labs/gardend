use crate::app_runtime::AppHandle;
use crate::{
    clock::{epoch_millis, timestamp},
    emporium::planner::memory_record_subject,
    emporium::schemas::{IngestPayload, IngestRequest, MemoryRecordIn, SourceRefIn},
    emporium::spine::{apply_and_assert, gather_and_plan},
    geist_memory_archive::write_memory_archive_document,
    geist_memory_rdf::materialize_memory_store,
    geist_memory_store::{
        memory_sort_key, read_memory_store, write_memory_archive_file, write_memory_store,
        LocalMemoryArchiveRecord, LocalMemoryRecord,
    },
    graph_service::touch_graph_updated_at,
    mcp_utils::{
        mcp_arg_string, mcp_arg_string_vec, mcp_arg_u64, mcp_arg_u64_vec, mcp_arg_usize,
        mcp_graph_id_or_default,
    },
    paths::existing_graph_dir,
    profile_service::touch_profile_updated_at,
};

/// The `mem:supersededBy` predicate URI, scanned out of the apply plan to echo
/// the demoted (superseded) subject in the `remember` result.
const SUPERSEDED_BY_PRED: &str = "http://mnemosyne.dev/memory#supersededBy";

/// `remember` — the typed front-end over the memory ingest path.
///
/// LOUD-REJECTS provenance-less writes (I1): the call MUST carry `content` AND at
/// least one of `source_refs` / `block_ids`. It builds a one-record memory
/// [`IngestRequest`], runs the in-process spine (`gather_and_plan` + the memory
/// apply fork in `apply_and_assert`), and ONLY on a successful apply appends the
/// legacy `memory-queue.json` row (so the queue and the `:projection:memory`
/// projection never diverge — append on ok, nothing on halt). Returns
/// `{ok, subject, supersededSubject?, error?}`.
///
/// The legacy flat `mnemo:Memory` projection is still materialized from the queue
/// (recall reads the queue in v1); the new `:projection:memory` projection is
/// shadow-built ADDITIVELY alongside it.
pub(super) async fn mcp_local_remember(
    app: AppHandle,
    arguments: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let graph_id = mcp_graph_id_or_default(&app, arguments)?;
    let record = build_remember_record(arguments)?;
    let outcome = ingest_one_memory(&app, &graph_id, record).await?;
    Ok(outcome.into_json())
}

/// Ingest one already-typed memory record through the same loud memory path used
/// by the public `remember` tool. Internal producers use this when another
/// Meaningful Object emits retained knowledge as a typed `mem:MemoryRecord`.
pub(crate) async fn ingest_memory_record(
    app: &AppHandle,
    graph_id: &str,
    record: MemoryRecordIn,
) -> Result<serde_json::Value, String> {
    Ok(ingest_one_memory(app, graph_id, record).await?.into_json())
}

/// `remember_batch` — N records → one memory [`IngestRequest`]. Each record is
/// built + provenance-gated through the SAME [`build_remember_record`] path, so a
/// provenance-less entry LOUD-REJECTS the whole batch BEFORE any apply. The batch
/// applies atomically (one loud-halt plan); the queue is appended transactionally
/// (only the records the apply accepted, which in v1 is all-or-halt).
pub(super) async fn mcp_local_remember_batch(
    app: AppHandle,
    arguments: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let graph_id = mcp_graph_id_or_default(&app, arguments)?;
    let raw_records = arguments
        .get("records")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| "records is required (an array of memory records)".to_string())?;
    if raw_records.is_empty() {
        return Err("records is empty".to_string());
    }
    let mut records = Vec::with_capacity(raw_records.len());
    for (i, raw) in raw_records.iter().enumerate() {
        records.push(build_remember_record(raw).map_err(|e| format!("records[{i}]: {e}"))?);
    }
    let outcome = ingest_memory_batch(&app, &graph_id, records).await?;
    Ok(outcome.into_json())
}

/// The result of one (or a batch of) `remember` apply(s): the minted subjects, an
/// optional superseded subject, and the loud-failure shape on halt.
struct RememberOutcome {
    ok: bool,
    subjects: Vec<String>,
    superseded: Vec<String>,
    halted_at: Option<usize>,
    error: Option<String>,
    /// The SHACL gate's structured `ViolationRecord[]` (focusNode/shape/path/
    /// value/message/severity), carried verbatim from the halted `ApplyReport`'s
    /// `memory_validate` step — the SAME shape `spine.rs`'s
    /// `memory_validation_halt_report` builds and the HTTP ingest path already
    /// returns wholesale. `None` on a clean apply, or when the halt was NOT a
    /// validation halt (a step-level fault has no structured violations).
    /// Additive: `ok`/`halted_at`/`error` are unchanged.
    violations: Option<Vec<serde_json::Value>>,
}

impl RememberOutcome {
    fn into_json(self) -> serde_json::Value {
        // Single-record callers (remember) want a scalar `subject`; the batch
        // wants `subjects`. Emit both shapes so either caller reads naturally.
        let subject = self.subjects.first().cloned();
        let superseded_subject = self.superseded.first().cloned();
        let mut out = serde_json::json!({
            "ok": self.ok,
            "subject": subject,
            "subjects": self.subjects,
            "supersededSubject": superseded_subject,
            "supersededSubjects": self.superseded,
            "source": "memory-projection",
        });
        if let Some(at) = self.halted_at {
            out["haltedAt"] = serde_json::json!(at);
        }
        if let Some(err) = self.error {
            out["error"] = serde_json::json!(err);
        }
        if let Some(violations) = self.violations {
            out["violations"] = serde_json::json!(violations);
        }
        out
    }
}

/// Pull the SHACL gate's structured violations (verbatim, no re-typing) out of
/// a halted apply report's steps — the same `memory_validate` step
/// `spine.rs`'s `memory_validation_halt_report` populates with `extra.violations`
/// (a serialized `Vec<ViolationRecord>`). `None` when no step carries a
/// `violations` array (e.g. a step-level fault, which has only `error`).
fn violations_from_report(
    report: &crate::emporium::applier::ApplyReport,
) -> Option<Vec<serde_json::Value>> {
    report.steps.iter().rev().find_map(|step| {
        step.extra
            .get("violations")
            .and_then(serde_json::Value::as_array)
            .cloned()
    })
}

/// Build ONE [`MemoryRecordIn`] from the loose `remember` arguments, applying the
/// I1 provenance gate UP FRONT (`content` non-empty AND ≥1 of `source_refs` /
/// `block_ids`). `block_ids` is sugar: each id expands into a `DocumentBlock`
/// `SourceRefIn`. Defaults keep the single-call ergonomics of the old `remember`
/// (scope=agent, kind=ClaimMemory, orientation=knowledge, visibility=private,
/// status=active) while still minting a fully-typed, provenance-anchored record.
fn build_remember_record(arguments: &serde_json::Value) -> Result<MemoryRecordIn, String> {
    let content =
        mcp_arg_string(arguments, &["content"]).ok_or_else(|| "content is required".to_string())?;
    if content.trim().is_empty() {
        return Err("content is empty".to_string());
    }

    // Provenance: explicit `source_refs` (full shape) OR `block_ids` sugar.
    let mut source_refs: Vec<SourceRefIn> = match arguments
        .get("source_refs")
        .or_else(|| arguments.get("sourceRefs"))
    {
        Some(value) => serde_json::from_value(value.clone())
            .map_err(|e| format!("source_refs is malformed: {e}"))?,
        None => Vec::new(),
    };
    let document_id = mcp_arg_string(arguments, &["document_id", "documentId"]);
    for block_id in mcp_arg_string_vec(arguments, &["block_ids", "blockIds"]) {
        source_refs.push(SourceRefIn {
            source_kind: "DocumentBlock".to_string(),
            source_label: None,
            block_id: Some(block_id),
            document_id: document_id.clone(),
            external_id: None,
            external_uri: None,
            observed_at: None,
            trust_tier: None,
        });
    }
    // The I1 LOUD-REJECT for provenance-less writes — the central inversion of
    // the legacy `remember` (which silently dropped provenance entirely).
    if source_refs.is_empty() {
        return Err(
            "I1 NO MEMORY WITHOUT PROVENANCE: remember requires at least one of \
             source_refs / block_ids"
                .to_string(),
        );
    }

    let scope = mcp_arg_string(arguments, &["scope"]).unwrap_or_else(|| "agent".to_string());
    let kind = mcp_arg_string(arguments, &["kind"]).unwrap_or_else(|| "ClaimMemory".to_string());
    let content_orientation =
        mcp_arg_string(arguments, &["content_orientation", "contentOrientation"])
            .unwrap_or_else(|| "knowledge".to_string());
    let visibility =
        mcp_arg_string(arguments, &["visibility"]).unwrap_or_else(|| "private".to_string());
    let status = mcp_arg_string(arguments, &["status"]).unwrap_or_else(|| "active".to_string());

    Ok(MemoryRecordIn {
        client_ref: mcp_arg_string(arguments, &["client_ref", "clientRef"]),
        scope,
        kind,
        content_orientation,
        visibility,
        status,
        content,
        source_refs,
        evidence: Vec::new(),
        observed_at: mcp_arg_u64(arguments, &["observed_at", "observedAt"]).map(|v| v as i64),
        valid_from: None,
        is_current: None,
        confidence: None,
        valence: None,
        agent_id: mcp_arg_string(arguments, &["agent_id", "agentId"]),
        observer_agent_id: mcp_arg_string(arguments, &["observer_agent_id", "observerAgentId"]),
        tags: mcp_arg_string_vec(arguments, &["tags"]),
        supersedes_ref: mcp_arg_string(arguments, &["supersedes_ref", "supersedesRef"]),
        contradicts_ref: mcp_arg_string(arguments, &["contradicts_ref", "contradictsRef"]),
    })
}

/// Ingest a single typed memory record through the in-process spine, then keep the
/// legacy queue transactional. Convenience wrapper over [`ingest_memory_batch`].
async fn ingest_one_memory(
    app: &AppHandle,
    graph_id: &str,
    record: MemoryRecordIn,
) -> Result<RememberOutcome, String> {
    ingest_memory_batch(app, graph_id, vec![record]).await
}

/// The shared apply path for `remember` / `remember_batch`: validate the shape
/// (the LOUD 400 gate), recompute the minted subjects, run the spine + the memory
/// apply fork, and — ONLY on a successful apply — append the legacy queue rows so
/// the queue and the projection never diverge.
async fn ingest_memory_batch(
    app: &AppHandle,
    graph_id: &str,
    records: Vec<MemoryRecordIn>,
) -> Result<RememberOutcome, String> {
    // Recompute the minted subjects BEFORE the move into the request, so the
    // result can echo them whatever the apply outcome.
    let subjects: Vec<String> = records
        .iter()
        .map(|r| memory_record_subject(graph_id, r))
        .collect();
    let queue_contents: Vec<String> = records.iter().map(|r| r.content.clone()).collect();

    let request = IngestRequest {
        vocab: "memory".to_string(),
        dry_run: false,
        replace_class: false,
        payload: IngestPayload::Memory { records },
    };
    // The LOUD shape gate (I1 + enum membership) — a 400 BEFORE any plan/apply.
    request.validate()?;

    // Per-graph write gate: survey → plan → apply → queue-append is one
    // read-modify-write; holding the gate across ALL of it serializes
    // concurrent supersessions of the same head and keeps the legacy queue
    // ordered with the projection.
    let _write_gate = crate::emporium::write_gate::acquire_write_gate(graph_id).await;

    let planned =
        gather_and_plan(app, graph_id, &request).map_err(|e| e.message_ref().to_string())?;
    // The superseded subjects are derivable from the demote triples the plan
    // emitted; scan them out of the plan steps before the apply moves on.
    let superseded = superseded_subjects_from_plan(&planned.plan);

    let report = apply_and_assert(app, graph_id, &planned.plan, planned.contract, &request).await;

    if !report.ok {
        // Loud halt: the applier already journaled to `memory/failures/` and the
        // report names `haltedAt`. NOTHING is written to the queue (queue and
        // projection never diverge). Surface the halt, do not silently drop.
        let error = report
            .steps
            .iter()
            .rev()
            .find_map(|s| s.extra.get("error").and_then(serde_json::Value::as_str))
            .map(str::to_string)
            .unwrap_or_else(|| "memory apply halted".to_string());
        let violations = violations_from_report(&report);
        return Ok(RememberOutcome {
            ok: false,
            subjects,
            superseded,
            halted_at: report.halted_at,
            error: Some(error),
            violations,
        });
    }

    // Apply succeeded → append the legacy queue rows TRANSACTIONALLY. A queue
    // write failure here is itself loud (the projection already committed, so the
    // caller must know the queue lagged).
    append_memory_queue(app, graph_id, &queue_contents)?;

    Ok(RememberOutcome {
        ok: true,
        subjects,
        superseded,
        halted_at: None,
        error: None,
        violations: None,
    })
}

/// Append `contents` to the legacy `memory-queue.json` + re-materialize the flat
/// `mnemo:Memory` projection (recall still reads the queue in v1). Mirrors the old
/// `remember` queue mutation, batched.
fn append_memory_queue(app: &AppHandle, graph_id: &str, contents: &[String]) -> Result<(), String> {
    if contents.is_empty() {
        return Ok(());
    }
    let graph_dir = existing_graph_dir(app, graph_id)?;
    let mut store = read_memory_store(&graph_dir, graph_id)?;
    let now = timestamp();
    for content in contents {
        let number = store.next_number.max(1);
        let block_id = format!("memory-{number}");
        store.memories.insert(
            number.to_string(),
            LocalMemoryRecord {
                number,
                block_id,
                content: content.clone(),
                created_at: now.clone(),
                last_active: now.clone(),
            },
        );
        store.next_number = number + 1;
    }
    write_memory_store(&graph_dir, &store)?;
    materialize_memory_store(&graph_dir, &store)?;
    touch_graph_updated_at(&graph_dir)?;
    touch_profile_updated_at(app)?;
    Ok(())
}

/// Pull the demoted (superseded) subjects out of a memory plan by scanning its
/// `INSERT DATA` steps for the `mem:supersededBy` predicate. The demote triple is
/// `<old> <…#supersededBy> <new> .`; we return the `<old>` subject. v1 heuristic
/// (the plan does not echo demotions structurally yet).
fn superseded_subjects_from_plan(plan: &crate::emporium::planner::Plan) -> Vec<String> {
    use crate::emporium::planner::Step;
    let mut out = Vec::new();
    for step in &plan.steps {
        let Step::SparqlUpdate { update } = step else {
            continue;
        };
        for line in update.lines() {
            let Some(pred_pos) = line.find(SUPERSEDED_BY_PRED) else {
                continue;
            };
            // `<old> <pred> <new> .` — the old subject is the first <...> token.
            let prefix = &line[..pred_pos];
            if let (Some(open), Some(close)) = (prefix.find('<'), prefix.find('>')) {
                if open < close {
                    let subject = &prefix[open + 1..close];
                    if !subject.is_empty() && !out.iter().any(|s| s == subject) {
                        out.push(subject.to_string());
                    }
                }
            }
        }
    }
    out
}

pub(super) fn mcp_local_care_memories(
    app: AppHandle,
    arguments: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let graph_id = mcp_graph_id_or_default(&app, arguments)?;
    let numbers = mcp_arg_u64_vec(arguments, &["numbers"]);
    let graph_dir = existing_graph_dir(&app, &graph_id)?;
    let mut store = read_memory_store(&graph_dir, &graph_id)?;
    let now = timestamp();
    let mut cared = Vec::new();

    for number in numbers {
        if let Some(memory) = store.memories.get_mut(&number.to_string()) {
            memory.last_active = now.clone();
            cared.push(number);
        }
    }

    if !cared.is_empty() {
        write_memory_store(&graph_dir, &store)?;
        materialize_memory_store(&graph_dir, &store)?;
        touch_graph_updated_at(&graph_dir)?;
        touch_profile_updated_at(&app)?;
    }

    Ok(serde_json::json!({
        "cared": cared,
        "timestamp": now,
        "source": "local-memory-store",
    }))
}

pub(super) fn mcp_local_archive_memories(
    app: AppHandle,
    arguments: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let graph_id = mcp_graph_id_or_default(&app, arguments)?;
    let keep = mcp_arg_usize(arguments, &["keep"], 50);
    if keep < 1 {
        return Ok(serde_json::json!({
            "error": "keep must be >= 1",
            "source": "local-memory-store",
        }));
    }

    let graph_dir = existing_graph_dir(&app, &graph_id)?;
    let mut store = read_memory_store(&graph_dir, &graph_id)?;
    if store.memories.is_empty() {
        return Ok(serde_json::json!({
            "kept": 0,
            "archived": 0,
            "note": "Memory queue is empty.",
            "source": "local-memory-store",
        }));
    }

    let mut memories = store.memories.values().cloned().collect::<Vec<_>>();
    memories.sort_by(|left, right| memory_sort_key(right).cmp(memory_sort_key(left)));
    let keep_count = keep.min(memories.len());
    let keep_memories = memories[..keep_count].to_vec();
    let mut archive_memories = memories[keep_count..].to_vec();

    if archive_memories.is_empty() {
        return Ok(serde_json::json!({
            "kept": keep_memories.len(),
            "archived": 0,
            "note": format!("Queue has {} memories, nothing to archive.", keep_memories.len()),
            "source": "local-memory-store",
        }));
    }

    archive_memories.sort_by_key(|memory| memory.number);
    let archived_at = timestamp();
    let archive_doc_id = format!("geist-memory-archive-{}", epoch_millis());
    write_memory_archive_file(&graph_dir, &archive_doc_id, &archive_memories)?;
    write_memory_archive_document(
        &graph_dir,
        &graph_id,
        &archive_doc_id,
        &archived_at,
        keep_memories.len(),
        &archive_memories,
    )?;

    store.memories = keep_memories
        .into_iter()
        .map(|memory| (memory.number.to_string(), memory))
        .collect();
    store.archives.push(LocalMemoryArchiveRecord {
        archive_doc_id: archive_doc_id.clone(),
        archived_at: archived_at.clone(),
        kept: store.memories.len(),
        archived: archive_memories.len(),
        memories: archive_memories.clone(),
    });
    write_memory_store(&graph_dir, &store)?;
    materialize_memory_store(&graph_dir, &store)?;
    touch_graph_updated_at(&graph_dir)?;
    touch_profile_updated_at(&app)?;

    Ok(serde_json::json!({
        "kept": store.memories.len(),
        "archived": archive_memories.len(),
        "archive_doc_id": archive_doc_id,
        "archiveDocId": archive_doc_id,
        "note": format!(
            "Archived {} memories to {}. Queue rewritten with {} memories.",
            archive_memories.len(),
            archive_doc_id,
            store.memories.len()
        ),
        "source": "local-memory-store",
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::emporium::applier::{ApplyReport, StepReport};

    fn halted_step(op: &str, extra: serde_json::Map<String, serde_json::Value>) -> ApplyReport {
        ApplyReport {
            graph: "lab".to_string(),
            workflow: "memory".to_string(),
            mode: "memory".to_string(),
            summary: serde_json::json!({}),
            warnings: Vec::new(),
            steps: vec![StepReport {
                op: op.to_string(),
                extra,
            }],
            ok: false,
            halted_at: Some(0),
            script_block: None,
            assertion: None,
        }
    }

    /// `violations_from_report` pulls the `memory_validate` step's structured
    /// `violations` array through VERBATIM (no re-typing) — the exact shape
    /// `spine.rs`'s `memory_validation_halt_report` populates.
    #[test]
    fn violations_from_report_extracts_the_memory_validate_step_verbatim() {
        let violations = serde_json::json!([{
            "focus_node": "urn:test:bad",
            "shape": null,
            "property_path": null,
            "offending_value": null,
            "message": "Less than 1 values",
            "severity": "Violation",
        }]);
        let mut extra = serde_json::Map::new();
        extra.insert("ok".to_string(), serde_json::json!(false));
        extra.insert(
            "error".to_string(),
            serde_json::json!("memory validation halted"),
        );
        extra.insert("violations".to_string(), violations.clone());
        let report = halted_step("memory_validate", extra);

        let extracted = violations_from_report(&report).expect("violations present");
        assert_eq!(serde_json::Value::Array(extracted), violations);
    }

    /// A step-level fault (no SHACL gate involved) carries only `error` — the
    /// extraction must stay `None`, not synthesize an empty array.
    #[test]
    fn violations_from_report_is_none_for_a_step_level_fault() {
        let mut extra = serde_json::Map::new();
        extra.insert("ok".to_string(), serde_json::json!(false));
        extra.insert("error".to_string(), serde_json::json!("boom"));
        let report = halted_step("sparql_update", extra);

        assert!(violations_from_report(&report).is_none());
    }

    /// `RememberOutcome::into_json` forwards `violations` verbatim ADDITIVELY —
    /// `ok`/`haltedAt`/`error` are unchanged.
    #[test]
    fn remember_outcome_into_json_forwards_violations_verbatim_and_stays_additive() {
        let violations = vec![serde_json::json!({
            "focus_node": "urn:test:bad",
            "message": "Less than 1 values",
            "severity": "Violation",
        })];
        let outcome = RememberOutcome {
            ok: false,
            subjects: vec!["urn:test:subject".to_string()],
            superseded: Vec::new(),
            halted_at: Some(0),
            error: Some("memory validation halted".to_string()),
            violations: Some(violations.clone()),
        };
        let json = outcome.into_json();
        assert_eq!(json["ok"], serde_json::json!(false));
        assert_eq!(json["haltedAt"], serde_json::json!(0));
        assert_eq!(json["error"], serde_json::json!("memory validation halted"));
        assert_eq!(json["violations"], serde_json::json!(violations));
    }

    /// A clean (`ok=true`) outcome carries NO `violations` key at all — the
    /// passthrough is additive, never invents an empty array.
    #[test]
    fn remember_outcome_into_json_omits_violations_key_when_none() {
        let outcome = RememberOutcome {
            ok: true,
            subjects: vec!["urn:test:subject".to_string()],
            superseded: Vec::new(),
            halted_at: None,
            error: None,
            violations: None,
        };
        let json = outcome.into_json();
        assert!(json.get("violations").is_none(), "{json}");
    }
}

// `remember_outcome_survives_a_real_shacl_halt` drives a GENUINE SHACL Halt
// through the real gate (`gather_and_plan` + `apply_and_assert`), so it needs a
// real per-graph oxigraph store — headless harness, mirrors
// `emporium/state_trace_tests.rs`'s `trace_memory_shacl_halt_is_journaled`.
// NO MOCKS: real store, real SHACL validator, real halted `ApplyReport`.
//
// WHY the injected desired-insert (not a malformed `remember` call): the memory
// shape gate's Rust-level `validate_memory` (I1 + enum membership, run BEFORE
// any plan/apply) already guarantees every SHACL-required predicate
// (`mem:sourceKind`) is present and valid on any request that reaches the
// planner — so a genuine SHACL Violation is structurally unreachable from a
// well-formed `remember` call through the fully public path (the same honest
// gap `state_trace_tests.rs` documents). The desired_inserts seam is the exact
// triple set the gate validates, so injecting there drives the REAL gate
// without a mock.
#[cfg(all(test, feature = "headless"))]
mod headless_tests {
    use super::*;
    use crate::emporium::contract::memory_core_vocabulary;
    use crate::emporium::terms::Term;
    use crate::graph_service::{create_graph_service, CreateGraphInput};
    use oxigraph::model::NamedNode;
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
        std::env::temp_dir().join(format!("garden-remember-violations-{name}-{nanos}"))
    }

    fn mock_app() -> AppHandle {
        crate::tauri_runtime::build_mock_app_for_tests(true)
    }

    fn seed_graph(app: &AppHandle, graph_id: &str) {
        create_graph_service(
            app,
            CreateGraphInput {
                graph_id: Some(graph_id.to_string()),
                title: "Remember Violations Lab".to_string(),
                description: None,
                operation_id: None,
            },
        )
        .expect("create graph");
    }

    #[test]
    fn remember_outcome_survives_a_real_shacl_halt() {
        let _serial = env_serial().lock().unwrap_or_else(|p| p.into_inner());
        let profile = temp_profile("halt");
        std::env::set_var("GARDEN_PROFILE_DIR", &profile);
        let result = std::panic::catch_unwind(run_remember_violations_trace);
        std::env::remove_var("GARDEN_PROFILE_DIR");
        let _ = std::fs::remove_dir_all(&profile);
        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
    }

    fn run_remember_violations_trace() {
        let app = mock_app();
        let graph_id = "remember-violations-lab";
        seed_graph(&app, graph_id); // freshly-seeded graph defaults to ValidationPolicy::Halt

        // A record built through the SAME `remember`-argument parsing `mcp_local_remember`
        // uses — a genuinely well-formed, provenance-anchored request.
        let record = build_remember_record(&serde_json::json!({
            "content": "vera prefers fish CLI",
            "block_ids": ["abc"],
            "document_id": "doc-shell",
        }))
        .expect("well-formed remember record");
        let request = IngestRequest {
            vocab: "memory".to_string(),
            dry_run: false,
            replace_class: false,
            payload: IngestPayload::Memory {
                records: vec![record],
            },
        };
        request.validate().expect("well-formed request validates");

        let mut planned = gather_and_plan(&app, graph_id, &request).expect("gather_and_plan");
        assert_eq!(planned.plan.mode, "memory");

        // Inject a SHACL-VIOLATING desired-insert: a mem:SourceReference subject
        // MISSING its required mem:sourceKind (sh:minCount 1) — the ONE seam a
        // well-formed record can never reach (see module doc above).
        let mem = memory_core_vocabulary().primary_namespace();
        let rdf_type = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type".to_string();
        let bad =
            format!("urn:mnemosyne:local:graph:{graph_id}:projection:memory:src:remember-halt-bad");
        let uri = |s: &str| Term::Uri(NamedNode::new(s).expect("valid IRI"));
        planned.plan.desired_inserts.push((
            bad.clone(),
            rdf_type.clone(),
            uri(&format!("{mem}SourceReference")),
        ));
        planned
            .plan
            .desired_inserts
            .push((bad, rdf_type, uri("http://www.w3.org/ns/prov#Entity")));
        // deliberately NO mem:sourceKind → sh:minCount 1 violation (severity Violation).

        let report = crate::app_runtime::async_runtime::block_on(apply_and_assert(
            &app,
            graph_id,
            &planned.plan,
            planned.contract,
            &request,
        ));
        assert!(!report.ok, "the SHACL Halt rejects the write");
        assert!(
            report.steps.iter().any(|s| s.op == "memory_validate"),
            "the report carries the validation halt: {report:#?}"
        );

        // Exactly what `ingest_memory_batch`'s halt branch does: extract the
        // structured violations, build the RememberOutcome, and serialize it —
        // the "failing remember" survives to the tool result.
        let violations = violations_from_report(&report);
        assert!(
            violations.as_ref().is_some_and(|v| !v.is_empty()),
            "structured violations present: {report:#?}"
        );
        let outcome = RememberOutcome {
            ok: false,
            subjects: vec![],
            superseded: vec![],
            halted_at: report.halted_at,
            error: Some("memory apply halted".to_string()),
            violations: violations.clone(),
        };
        let json = outcome.into_json();
        assert_eq!(json["ok"], serde_json::json!(false));
        assert_eq!(
            json["violations"].as_array().map(Vec::len),
            violations.as_ref().map(Vec::len),
            "the tool result's violations array matches the gate's structured report: {json}"
        );
        assert!(
            json["violations"][0]["focus_node"]
                .as_str()
                .is_some_and(|s| s.ends_with("remember-halt-bad")),
            "{json}"
        );
    }
}
