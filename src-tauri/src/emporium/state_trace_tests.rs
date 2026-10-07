//! STATE TRACE — make the real emporium ingest data-flow visible & verified at
//! every stage, against a REAL gardend MockRuntime cell (no engine mocks).
//!
//! Two flows, each PRINTS its stage-by-stage state (run with `--nocapture`) AND
//! asserts the invariants:
//!
//!   1. `trace_workflow_ingest_end_to_end` — a real workflow ingest through the
//!      REAL spine: STAGE 0 request → STAGE 1 survey/plan (gather_and_plan) →
//!      STAGE 2 apply (apply_and_assert) → STAGE 3 store state (SPARQL the real
//!      `GRAPH <{root}:user:rdf>`) → STAGE 4 convergence (re-plan = zero ops).
//!
//!   2. `trace_memory_supersession_end_to_end` — file head H (a real
//!      `MemoryRecordIn` with provenance) THROUGH the spine, then file H′ with
//!      supersedesRef=H. We read the REAL `:projection:memory` graph after each
//!      file, print the demote DELETE/INSERT plan, and prove the supersession
//!      delta + re-file = zero ops.
//!
//! Both reuse the working harness from `spine/tests.rs` / `applier/tests.rs`:
//! `build_mock_app_for_tests(true)` (CRDT queue managed), a temp profile +
//! `profile_env_serial` lock, `create_graph_service` to seed the graph, the
//! `gather_and_plan` + `apply_and_assert` spine entry points, and SPARQL reads
//! over the real per-graph oxigraph store via `run_sparql_query_service`.

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use crate::app_runtime::AppHandle;
use crate::emporium::contract::{memory_core_vocabulary, workflow_vocabulary};
use crate::emporium::planner::{memory_record_subject, plan_memory_compute, Plan, Step};
use crate::emporium::schemas::{IngestRequest, MemoryRecordIn, SourceRefIn};
use crate::emporium::spine::{apply_and_assert, gather_and_plan};
use crate::graph_service::{create_graph_service, CreateGraphInput};
use crate::rdf_authority::{memory_projection_graph_iri, user_rdf_graph_iri};
use crate::rdf_service::{run_sparql_query_service, SparqlInput};

// ---------------------------------------------------------------------------
// harness (mirrors spine/tests.rs + applier/tests.rs)
// ---------------------------------------------------------------------------

/// `GARDEN_PROFILE_DIR` is process-global; serialize the headless tests that set
/// it (across ALL modules) so they cannot stomp each other's profile.
fn env_serial() -> &'static Mutex<()> {
    crate::tauri_runtime::profile_env_serial()
}

fn temp_profile(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    std::env::temp_dir().join(format!("garden-emporium-trace-{name}-{nanos}"))
}

/// MockRuntime app WITH the CRDT queue + room registry managed — the real applier
/// enqueues CRDT ops that drain through the in-process headless executor.
fn mock_app() -> AppHandle {
    crate::tauri_runtime::build_mock_app_for_tests(true)
}

fn seed_graph(app: &AppHandle, graph_id: &str) {
    create_graph_service(
        app,
        CreateGraphInput {
            graph_id: Some(graph_id.to_string()),
            title: "Lab".to_string(),
            description: None,
            operation_id: None,
        },
    )
    .expect("create graph");
}

/// Run a SELECT over the real per-graph store and return the rows (each a
/// `{var: rendered-term}` map). The rendered term is oxigraph's `to_string()`
/// (`<uri>` / `"lit"` / `"lit"^^<dt>`), which is what the trace prints verbatim.
fn select(
    app: &AppHandle,
    graph_id: &str,
    query: &str,
) -> Vec<std::collections::BTreeMap<String, String>> {
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

/// Strip the `<…>` from a rendered URI binding (predicates / URI subjects come
/// back from the SPARQL service bracketed); pass any other rendering through.
fn strip_uri(s: &str) -> String {
    s.strip_prefix('<')
        .and_then(|x| x.strip_suffix('>'))
        .map(str::to_string)
        .unwrap_or_else(|| s.to_string())
}

/// All `(s, p, o)` triples in a named graph, ordered for a stable trace.
fn all_triples(
    app: &AppHandle,
    graph_id: &str,
    named_graph: &str,
) -> Vec<(String, String, String)> {
    let q = format!(
        "SELECT ?s ?p ?o WHERE {{ GRAPH <{named_graph}> {{ ?s ?p ?o }} }} ORDER BY ?s ?p ?o"
    );
    let mut out = Vec::new();
    for row in select(app, graph_id, &q) {
        out.push((
            row.get("s").cloned().unwrap_or_default(),
            row.get("p").cloned().unwrap_or_default(),
            row.get("o").cloned().unwrap_or_default(),
        ));
    }
    out
}

/// Distinct subjects of `rdf:type <{wfns}{class}>` in the user:rdf graph.
fn class_subjects(app: &AppHandle, graph_id: &str, class_local: &str) -> Vec<String> {
    let user_rdf = user_rdf_graph_iri(graph_id);
    let wfns = workflow_vocabulary().primary_namespace();
    let q = format!(
        "SELECT DISTINCT ?s WHERE {{ GRAPH <{user_rdf}> {{ ?s a <{wfns}{class_local}> }} }} ORDER BY ?s"
    );
    select(app, graph_id, &q)
        .into_iter()
        .filter_map(|r| r.get("s").cloned())
        .collect()
}

/// Pretty-print a plan's steps (the load-bearing STAGE-1 deliverable): folder /
/// doc / wire ops named, and the LITERAL `DELETE DATA` / `INSERT DATA` SPARQL
/// bodies for each `sparql_update` step.
fn print_steps(plan: &Plan) {
    for (i, step) in plan.steps.iter().enumerate() {
        match step {
            Step::CreateFolder {
                folder_id,
                label,
                parent_id,
            } => {
                println!(
                    "    [{i:>2}] create_folder  folderId={folder_id:?} label={label:?} parentId={parent_id:?}"
                );
            }
            Step::RenameFolder { folder_id, label } => {
                println!("    [{i:>2}] rename_folder  folderId={folder_id:?} label={label:?}");
            }
            Step::WriteDoc {
                doc_id,
                content,
                capture_script_block,
                ..
            } => {
                let first_line = content.lines().next().unwrap_or("");
                println!(
                    "    [{i:>2}] write_doc      docId={doc_id:?} captureScriptBlock={capture_script_block:?} \
                     bytes={} firstLine={first_line:?}",
                    content.len()
                );
            }
            Step::Move { doc_id, folder_id } => {
                println!("    [{i:>2}] move           docId={doc_id:?} → folderId={folder_id:?}");
            }
            Step::CreateWires { wires } => {
                for w in wires {
                    println!(
                        "    [{i:>2}] create_wire    {} : {} → {}",
                        w.predicate, w.source_document_id, w.target_document_id
                    );
                }
            }
            Step::DeleteWires { wire_ids } => {
                println!("    [{i:>2}] delete_wires   wireIds={wire_ids:?}");
            }
            Step::SparqlUpdate { update } => {
                let verb = if update.starts_with("DELETE") {
                    "sparql_update DELETE"
                } else if update.starts_with("INSERT") {
                    "sparql_update INSERT"
                } else {
                    "sparql_update ?????"
                };
                println!("    [{i:>2}] {verb}");
                for line in update.lines() {
                    println!("         | {line}");
                }
            }
        }
    }
}

fn print_summary(label: &str, plan: &Plan) {
    let s = &plan.summary;
    println!(
        "  {label}: mode={:?} folders={} docWrites={} moves={} wiresCreate={} wiresDelete={} rdfInsert={} rdfDelete={}",
        plan.mode, s.folders, s.doc_writes, s.moves, s.wires_create, s.wires_delete, s.rdf_insert, s.rdf_delete
    );
}

// ===========================================================================
// FLOW 1 — workflow ingest, end to end
// ===========================================================================

fn workflow_parsed_json() -> Value {
    let script = "export const meta = {}\n";
    let sha = crate::emporium::terms::sha256_text(script);
    json!({
        "kind": "run-record",
        "name": "demo",
        "description": "d",
        "whenToUse": null,
        "script": script,
        "scriptSha256": sha,
        "phases": [{"order": 1, "title": "Go", "detail": null}],
        "nodes": [
            {"label": "a", "phase": "Go", "phaseIndex": 1, "agentType": null, "prompt": "p1"},
            {"label": "b", "phase": "Go", "phaseIndex": 1, "agentType": null, "prompt": "p2 [output of a]"}
        ],
        "edges": [["a", "b"]],
        "duplicateLabels": [],
        "run": {
            "runId": "wf_test-1",
            "status": "completed",
            "startTimeMs": 1000,
            "endTimeMs": 5000,
            "endTimeIso": null,
            "totalTokens": 10,
            "agentCount": 2,
            "durationMs": 4000,
            "recordPath": "/tmp/x.json",
            "phases": [{"index": 1, "title": "Go"}],
            "agents": [
                {"label": "a", "phaseIndex": 1, "model": "m", "state": "done", "tokens": 5,
                 "toolCalls": 1, "durationMs": 2000, "queuedAt": 1000, "startedAt": 1100,
                 "cached": false, "agentType": null},
                {"label": "b", "phaseIndex": 1, "model": "m", "state": "done", "tokens": 5,
                 "toolCalls": 1, "durationMs": 1000, "queuedAt": 3000, "startedAt": 3100,
                 "cached": false, "agentType": null}
            ]
        }
    })
}

fn workflow_request(dry_run: bool) -> IngestRequest {
    let body = json!({
        "vocab": "workflow",
        "dry_run": dry_run,
        "payload": {
            "kind": "workflow",
            "parsed": workflow_parsed_json(),
            "judgment": {
                "shortId": "wf-demo",
                "preamble": "Demo.",
                "rationale": "",
                "nodeArchetypes": {"a": "NEW:alpha", "b": "NEW:alpha"},
                "newArchetypes": [
                    {"slug": "alpha", "title": "Alpha", "role": "r", "template": "t"}
                ]
            }
        }
    });
    serde_json::from_value(body).expect("ingest request deserializes")
}

#[test]
fn trace_workflow_ingest_end_to_end() {
    let _serial = env_serial().lock().unwrap_or_else(|p| p.into_inner());
    let profile = temp_profile("workflow");
    std::env::set_var("GARDEN_PROFILE_DIR", &profile);
    let result = std::panic::catch_unwind(run_workflow_trace);
    std::env::remove_var("GARDEN_PROFILE_DIR");
    let _ = std::fs::remove_dir_all(&profile);
    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}

fn run_workflow_trace() {
    let app = mock_app();
    let graph_id = "lab";
    seed_graph(&app, graph_id);
    let contract = workflow_vocabulary();
    let user_rdf = user_rdf_graph_iri(graph_id);

    println!("\n================ FLOW 1: WORKFLOW INGEST ================");
    println!("graph_id={graph_id:?}  user:rdf graph=<{user_rdf}>");

    // ── STAGE 0 — the IngestRequest ─────────────────────────────────────────
    let request = workflow_request(false);
    println!("\n-- STAGE 0  INGEST REQUEST --");
    println!("  vocab={:?}  dry_run={}", request.vocab, request.dry_run);
    println!("  payload kind=workflow  workflow.name=\"demo\"  shortId=\"wf-demo\"  nodes=[a,b]  phase=[Go]  run=wf_test-1");

    // ── STAGE 1 — SURVEY + PLAN (gather_and_plan, the read-only spine) ───────
    let planned1 = gather_and_plan(&app, graph_id, &request).expect("first gather_and_plan");
    let plan1 = &planned1.plan;
    println!("\n-- STAGE 1  SURVEY/PLAN (gather_and_plan over the empty seeded cell) --");
    print_summary("plan.summary", plan1);
    println!("  steps ({}):", plan1.steps.len());
    print_steps(plan1);

    assert_eq!(plan1.mode, "new", "first plan is a fresh mint");
    assert!(
        plan1.summary.rdf_insert > 0,
        "rdfInsert>0 on first plan: {:?}",
        plan1.summary
    );
    assert_eq!(plan1.summary.rdf_delete, 0, "rdfDelete==0 on first plan");

    // ── STAGE 2 — APPLY (apply_and_assert, the real write path) ──────────────
    let report =
        crate::app_runtime::async_runtime::block_on(apply_and_assert(&app, graph_id, plan1, contract, &request));
    println!("\n-- STAGE 2  APPLY (apply_and_assert → CRDT surface + GRAPH <user:rdf>) --");
    println!(
        "  ApplyReport: ok={} haltedAt={:?} steps={} scriptBlock={:?}",
        report.ok,
        report.halted_at,
        report.steps.len(),
        report.script_block
    );
    println!("  summary={}", report.summary);
    if let Some(a) = &report.assertion {
        println!(
            "  assertion: passed={} failures={:?} stats(nodes={:?} phases={:?} agentRuns={:?})",
            a.passed, a.failures, a.stats.nodes, a.stats.phases, a.stats.agent_runs
        );
    } else {
        println!("  assertion: <none>");
    }
    assert!(report.ok, "apply ok=true: {report:?}");
    assert!(report.halted_at.is_none(), "no halt: {report:?}");
    let assertion = report
        .assertion
        .as_ref()
        .expect("workflow assertion present");
    assert!(
        assertion.passed,
        "assertion passed: failures={:?}",
        assertion.failures
    );

    // ── STAGE 3 — STORE STATE (SPARQL the real user:rdf graph) ───────────────
    let triples = all_triples(&app, graph_id, &user_rdf);
    println!(
        "\n-- STAGE 3  STORE STATE  GRAPH <{user_rdf}>  ({} triples) --",
        triples.len()
    );
    println!("  wf: anatomy subjects (from rdf:type):");
    for class in ["Workflow", "Phase", "AgentNode", "Run", "Archetype"] {
        let subs = class_subjects(&app, graph_id, class);
        println!("    wf:{class:<10} × {} : {subs:?}", subs.len());
    }
    println!(
        "  ALL ({}) triples (subject, predicate, object):",
        triples.len()
    );
    for (s, p, o) in &triples {
        println!("    {s}  {p}  {o}");
    }
    assert!(
        !triples.is_empty(),
        "user:rdf graph is non-empty after apply"
    );
    assert_eq!(
        class_subjects(&app, graph_id, "Workflow").len(),
        1,
        "exactly one wf:Workflow"
    );
    assert_eq!(
        class_subjects(&app, graph_id, "Phase").len(),
        1,
        "one wf:Phase"
    );
    assert_eq!(
        class_subjects(&app, graph_id, "AgentNode").len(),
        2,
        "two wf:AgentNode"
    );
    assert_eq!(class_subjects(&app, graph_id, "Run").len(), 1, "one wf:Run");

    // ── STAGE 4 — CONVERGENCE (re-plan against the populated cell = zero ops) ─
    let planned2 = gather_and_plan(&app, graph_id, &request).expect("second gather_and_plan");
    let plan2 = &planned2.plan;
    println!("\n-- STAGE 4  CONVERGENCE (gather_and_plan #2 over the populated cell) --");
    print_summary("re-plan.summary", plan2);
    println!("  steps ({}):", plan2.steps.len());
    print_steps(plan2);

    assert_eq!(
        plan2.mode, "update",
        "re-plan is an update (the workflow now exists)"
    );
    let s2 = &plan2.summary;
    assert_eq!(s2.folders, 0, "folders: {s2:?}");
    assert_eq!(s2.doc_writes, 0, "docWrites: {s2:?}");
    assert_eq!(s2.moves, 0, "moves: {s2:?}");
    assert_eq!(s2.wires_create, 0, "wiresCreate: {s2:?}");
    assert_eq!(s2.wires_delete, 0, "wiresDelete: {s2:?}");
    assert_eq!(
        s2.rdf_insert, 0,
        "rdfInsert must be zero on re-plan: {s2:?}"
    );
    assert_eq!(
        s2.rdf_delete, 0,
        "rdfDelete must be zero on re-plan: {s2:?}"
    );
    println!("\n  ✓ idempotent: re-ingest of the same def → ZERO rdf/doc/wire ops.");
    println!("================ FLOW 1 PASS ================\n");
}

// ===========================================================================
// FLOW 2 — memory supersession, end to end
// ===========================================================================

/// A valid `MemoryRecordIn` with a DocumentBlock provenance ref (so the
/// provenance invariant I1 passes — `plan_memory_compute` rejects a
/// provenanceless record).
fn memory_record(client_ref: &str, content: &str, supersedes: Option<String>) -> MemoryRecordIn {
    MemoryRecordIn {
        client_ref: Some(client_ref.to_string()),
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
        supersedes_ref: supersedes,
        contradicts_ref: None,
    }
}

/// Build the `IngestRequest` (vocab "memory") wrapping one record — the SAME
/// route shape a `remember` producer sends; the spine deserializes
/// `IngestPayload::Memory`.
fn memory_request(record: &MemoryRecordIn) -> IngestRequest {
    let body = json!({
        "vocab": "memory",
        "dry_run": false,
        "payload": {
            "kind": "memory",
            "records": [record],
        }
    });
    serde_json::from_value(body).expect("memory ingest request deserializes")
}

/// File one memory record THROUGH the real spine (gather_and_plan +
/// apply_and_assert) and return the apply report's ok flag. The memory branch of
/// `apply_and_assert` routes (on `mode=="memory"`) to `apply_memory_plan`, which
/// writes direct-on-store into `:projection:memory`.
fn file_through_spine(app: &AppHandle, graph_id: &str, record: &MemoryRecordIn) -> bool {
    let request = memory_request(record);
    let planned = gather_and_plan(app, graph_id, &request).expect("memory gather_and_plan");
    let report = crate::app_runtime::async_runtime::block_on(apply_and_assert(
        app,
        graph_id,
        &planned.plan,
        memory_core_vocabulary(),
        &request,
    ));
    report.ok
}

/// The `(predicate, object)` rows for a memory subject in `:projection:memory`.
fn subject_po(app: &AppHandle, graph_id: &str, subject: &str) -> Vec<(String, String)> {
    let mem = memory_projection_graph_iri(graph_id);
    let q =
        format!("SELECT ?p ?o WHERE {{ GRAPH <{mem}> {{ <{subject}> ?p ?o }} }} ORDER BY ?p ?o");
    select(app, graph_id, &q)
        .into_iter()
        .map(|r| {
            (
                strip_uri(&r.get("p").cloned().unwrap_or_default()),
                r.get("o").cloned().unwrap_or_default(),
            )
        })
        .collect()
}

fn print_subject(label: &str, app: &AppHandle, graph_id: &str, subject: &str) {
    let mem = memory_core_vocabulary().primary_namespace();
    let rows = subject_po(app, graph_id, subject);
    println!("  {label}  <{subject}>  ({} triples):", rows.len());
    for (p, o) in &rows {
        // Annotate the lifecycle predicates the supersession algebra flips.
        let tag = if p == &format!("{mem}status")
            || p == &format!("{mem}isCurrent")
            || p == &format!("{mem}supersededBy")
            || p == &format!("{mem}supersedes")
        {
            "  ◀ lifecycle"
        } else {
            ""
        };
        println!("      {p}  {o}{tag}");
    }
}

#[test]
fn trace_memory_supersession_end_to_end() {
    let _serial = env_serial().lock().unwrap_or_else(|p| p.into_inner());
    let profile = temp_profile("memory");
    std::env::set_var("GARDEN_PROFILE_DIR", &profile);
    let result = std::panic::catch_unwind(run_memory_trace);
    std::env::remove_var("GARDEN_PROFILE_DIR");
    let _ = std::fs::remove_dir_all(&profile);
    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}

fn run_memory_trace() {
    let app = mock_app();
    let graph_id = "lab";
    seed_graph(&app, graph_id);
    let contract = memory_core_vocabulary();
    let mem_graph = memory_projection_graph_iri(graph_id);
    let mem_ns = contract.primary_namespace();
    let status_p = format!("{mem_ns}status");
    let iscur_p = format!("{mem_ns}isCurrent");
    let supby_p = format!("{mem_ns}supersededBy");
    let sup_p = format!("{mem_ns}supersedes");
    // oxigraph renders xsd:boolean literals WITH the datatype tag on both SELECT
    // read-back and our N-Triples plan parse (the store does not collapse them to
    // bare true/false), so the expected object strings carry it verbatim.
    let true_lit = TRUE_LIT.to_string();
    let false_lit = FALSE_LIT.to_string();

    println!("\n================ FLOW 2: MEMORY SUPERSESSION ================");
    println!("graph_id={graph_id:?}  projection:memory graph=<{mem_graph}>");
    println!("  PATH: memory IS driven THROUGH the spine — gather_and_plan(memory branch) +");
    println!("        apply_and_assert routes on mode==\"memory\" → apply_memory_plan →");
    println!("        run_memory_update → direct-on-store INSERT/DELETE into <{mem_graph}>.");

    // ── file head H through the spine ────────────────────────────────────────
    let h = memory_record("r-0", "vera prefers fish CLI", None);
    let h_subject = memory_record_subject(graph_id, &h);
    println!("\n-- STAGE 0  FILE HEAD H (through the spine) --");
    println!("  H.content={:?}  H.subject=<{h_subject}>", h.content);
    let h_ok = file_through_spine(&app, graph_id, &h);
    assert!(
        h_ok,
        "filing H through the spine must succeed (apply ok=true)"
    );

    println!("\n-- STAGE 1  PROJECTION STATE AFTER H --");
    print_subject("H", &app, graph_id, &h_subject);
    // H is active + current.
    let h_after = subject_po(&app, graph_id, &h_subject);
    assert!(
        h_after
            .iter()
            .any(|(p, o)| p == &status_p && o == "\"active\""),
        "H filed mem:status=active; got {h_after:?}"
    );
    assert!(
        h_after.iter().any(|(p, o)| p == &iscur_p && o == &true_lit),
        "H filed mem:isCurrent=true; got {h_after:?}"
    );
    let h_triple_count_before = h_after.len();

    // ── file H′ with supersedesRef = H through the spine ─────────────────────
    let hp = memory_record("r-1", "vera prefers zsh", Some(h_subject.clone()));
    let hp_subject = memory_record_subject(graph_id, &hp);
    assert_ne!(
        h_subject, hp_subject,
        "changed content re-mints a distinct subject"
    );
    println!("\n-- STAGE 2  FILE H' (supersedesRef=H, through the spine) --");
    println!("  H'.content={:?}  H'.subject=<{hp_subject}>", hp.content);
    println!("  H'.supersedesRef=<{h_subject}>");

    // Show the supersession PLAN's steps (the demote DELETE/INSERT) BEFORE we
    // apply it — gather_and_plan is the read-only spine, so this is the literal
    // plan that apply_and_assert will run.
    let hp_request = memory_request(&hp);
    let planned_hp = gather_and_plan(&app, graph_id, &hp_request).expect("H' gather_and_plan");
    let plan_hp = &planned_hp.plan;
    println!("\n  SUPERSESSION PLAN (gather_and_plan over the H-populated projection):");
    print_summary("  plan.summary", plan_hp);
    println!("  steps ({}):", plan_hp.steps.len());
    print_steps(plan_hp);
    assert_eq!(plan_hp.mode, "memory", "memory plan mode");
    assert!(
        plan_hp.routes_to_memory_sink(),
        "memory plan routes to :projection:memory"
    );

    // Extract the demote delta from the plan (removes / adds) for the assertion.
    let (removes, adds) = split_data_steps(plan_hp);
    println!("\n  DEMOTE DELTA (parsed from the plan's DELETE/INSERT DATA bodies):");
    println!("    removes ({}):", removes.len());
    for (s, p, o) in &removes {
        println!("      - {s}  {p}  {o}");
    }
    println!("    adds ({}):", adds.len());
    for (s, p, o) in &adds {
        println!("      + {s}  {p}  {o}");
    }

    // Assert the demote delta (removes ⊇ {…}, adds ⊇ {…}). Objects are oxigraph-
    // rendered: `"active"` / `true` / `<uri>`.
    assert!(
        removes.contains(&(
            h_subject.clone(),
            status_p.clone(),
            "\"active\"".to_string()
        )),
        "removes must ⊇ (H, status, active); removes={removes:?}"
    );
    assert!(
        removes.contains(&(h_subject.clone(), iscur_p.clone(), true_lit.clone())),
        "removes must ⊇ (H, isCurrent, true); removes={removes:?}"
    );
    assert!(
        adds.contains(&(
            h_subject.clone(),
            status_p.clone(),
            "\"superseded\"".to_string()
        )),
        "adds must ⊇ (H, status, superseded); adds={adds:?}"
    );
    assert!(
        adds.contains(&(h_subject.clone(), iscur_p.clone(), false_lit.clone())),
        "adds must ⊇ (H, isCurrent, false); adds={adds:?}"
    );
    assert!(
        adds.contains(&(
            h_subject.clone(),
            supby_p.clone(),
            format!("<{hp_subject}>")
        )),
        "adds must ⊇ (H, supersededBy, H'); adds={adds:?}"
    );
    assert!(
        adds.contains(&(hp_subject.clone(), sup_p.clone(), format!("<{h_subject}>"))),
        "adds must ⊇ (H', supersedes, H); adds={adds:?}"
    );

    // Now APPLY H' through the spine and re-read the projection state.
    let hp_report = crate::app_runtime::async_runtime::block_on(apply_and_assert(
        &app,
        graph_id,
        plan_hp,
        contract,
        &hp_request,
    ));
    assert!(
        hp_report.ok,
        "applying H' through the spine must succeed: {hp_report:?}"
    );

    println!("\n-- STAGE 3  PROJECTION STATE AFTER H' --");
    print_subject("H  (now demoted)", &app, graph_id, &h_subject);
    print_subject("H' (new head)", &app, graph_id, &hp_subject);

    // H is now superseded + not current + supersededBy → H', AND kept its content.
    let h_now = subject_po(&app, graph_id, &h_subject);
    assert!(
        h_now
            .iter()
            .any(|(p, o)| p == &status_p && o == "\"superseded\""),
        "H now mem:status=superseded; got {h_now:?}"
    );
    assert!(
        h_now.iter().any(|(p, o)| p == &iscur_p && o == &false_lit),
        "H now mem:isCurrent=false; got {h_now:?}"
    );
    assert!(
        h_now
            .iter()
            .any(|(p, o)| p == &supby_p && o == &format!("<{hp_subject}>")),
        "H now mem:supersededBy → H'; got {h_now:?}"
    );
    // Append-only demote: content preserved (status+isCurrent are value-swaps,
    // supersededBy is the only net add → exactly +1 triple).
    assert_eq!(
        h_now.len(),
        h_triple_count_before + 1,
        "H demote is append-only: content/provenance preserved (+1 supersededBy), got {} (was {})",
        h_now.len(),
        h_triple_count_before
    );
    // H' is the live head.
    let hp_now = subject_po(&app, graph_id, &hp_subject);
    assert!(
        hp_now
            .iter()
            .any(|(p, o)| p == &status_p && o == "\"active\""),
        "H' is mem:status=active; got {hp_now:?}"
    );
    assert!(
        hp_now
            .iter()
            .any(|(p, o)| p == &sup_p && o == &format!("<{h_subject}>")),
        "H' mem:supersedes → H; got {hp_now:?}"
    );
    // Exactly one current head.
    let heads = select(
        &app,
        graph_id,
        &format!("SELECT ?s WHERE {{ GRAPH <{mem_graph}> {{ ?s <{iscur_p}> true }} }}"),
    );
    println!("\n  current heads (mem:isCurrent=true): {}", heads.len());
    assert_eq!(
        heads.len(),
        1,
        "exactly one current head after supersession"
    );

    // ── STAGE 4 — re-file H' → zero ops ──────────────────────────────────────
    let replanned =
        gather_and_plan(&app, graph_id, &memory_request(&hp)).expect("re-file gather_and_plan");
    let replan = &replanned.plan;
    println!("\n-- STAGE 4  RE-FILE H' (convergence) --");
    print_summary("re-file.summary", replan);
    let (re_removes, re_adds) = split_data_steps(replan);
    println!(
        "  removes={} adds={}  (steps={})",
        re_removes.len(),
        re_adds.len(),
        replan.steps.len()
    );
    assert_eq!(
        replan.summary.rdf_insert, 0,
        "re-file inserts nothing: {:?}",
        replan.summary
    );
    assert_eq!(
        replan.summary.rdf_delete, 0,
        "re-file deletes nothing: {:?}",
        replan.summary
    );
    assert!(
        re_removes.is_empty() && re_adds.is_empty(),
        "re-file emits zero rdf data ops"
    );
    println!("\n  ✓ idempotent: re-file of H' → ZERO rdf ops.");
    println!("================ FLOW 2 PASS ================\n");
}

/// Split a plan's `DELETE DATA` / `INSERT DATA` steps into (removes, adds) as
/// `(subject, predicate, object)` STRING triples, where the object is rendered in
/// the SAME shape oxigraph's SELECT prints (`"lit"` / `true` / `<uri>`). The
/// planner emits N-Triples bodies; we parse them back with a tiny statement
/// splitter and normalize the object the way the store would render it on read.
fn split_data_steps(plan: &Plan) -> (Vec<(String, String, String)>, Vec<(String, String, String)>) {
    let mut removes = Vec::new();
    let mut adds = Vec::new();
    for step in &plan.steps {
        if let Step::SparqlUpdate { update } = step {
            let into = if update.starts_with("DELETE DATA") {
                &mut removes
            } else if update.starts_with("INSERT DATA") {
                &mut adds
            } else {
                continue;
            };
            into.extend(parse_nt_body(update));
        }
    }
    (removes, adds)
}

/// Parse the `{ <s> <p> obj . ... }` body of an `INSERT/DELETE DATA` into
/// `(s, p, obj-as-store-renders-it)` triples. Normalizes typed literals to the
/// store's SELECT rendering so the trace's expected objects (`"active"`, `true`,
/// `<uri>`) match what STAGE-3 SPARQL prints: a plain `"x"` stays `"x"`, an
/// `xsd:boolean` literal collapses to bare `true`/`false`, and a `<uri>` is kept.
fn parse_nt_body(update: &str) -> Vec<(String, String, String)> {
    let open = update.find('{').expect("update body open");
    let close = update.rfind('}').expect("update body close");
    let body = &update[open + 1..close];
    let mut out = Vec::new();
    for stmt in body.split(" .\n") {
        let stmt = stmt.trim().trim_end_matches('.').trim();
        if stmt.is_empty() {
            continue;
        }
        let s_end = match stmt.find('>') {
            Some(i) => i,
            None => continue,
        };
        let subject = stmt[1..s_end].to_string();
        let rest = stmt[s_end + 1..].trim_start();
        let p_end = match rest.find('>') {
            Some(i) => i,
            None => continue,
        };
        let predicate = rest[1..p_end].to_string();
        let obj_raw = rest[p_end + 1..].trim();
        out.push((subject, predicate, normalize_object(obj_raw)));
    }
    out
}

/// oxigraph's SELECT rendering of the two `xsd:boolean` lifecycle literals — kept
/// WITH the datatype tag (the store does not collapse typed booleans to bare
/// `true`/`false` on read-back), so both the live-read rows and the plan-parsed
/// delta compare against this exact string.
const TRUE_LIT: &str = "\"true\"^^<http://www.w3.org/2001/XMLSchema#boolean>";
const FALSE_LIT: &str = "\"false\"^^<http://www.w3.org/2001/XMLSchema#boolean>";

/// Render an N-Triples object the way the oxigraph SELECT does: a typed
/// `xsd:string` literal drops its datatype tag (→ plain `"x"`); a `<uri>` stays
/// `<uri>`; booleans / numerics / dateTime keep their typed `"value"^^<dt>`
/// rendering verbatim (matching the store's read-back, incl. [`TRUE_LIT`]).
fn normalize_object(obj: &str) -> String {
    if obj.starts_with('<') {
        return obj.to_string();
    }
    if let Some(idx) = obj.find("^^<") {
        let lexical = &obj[..idx]; // includes the surrounding quotes
        let dtype = &obj[idx + 3..obj.len().saturating_sub(1)];
        // string datatype is dropped by oxigraph's plain rendering.
        if dtype.ends_with("#string") {
            let inner = lexical.trim_matches('"');
            return format!("\"{inner}\"");
        }
        // booleans / numerics / dateTime: keep the typed rendering verbatim.
        return obj.to_string();
    }
    obj.to_string()
}

// ===========================================================================
// FLOW 3 — the GENERIC PUBLICATION CONVERGENCE ORACLE (EA-3 / B4 + B5).
//
// The load-bearing proof that B4's NEW placement (`:projection:bookmark`)
// converges — NOT a free corollary of memory's oracle, because it is a NEW named
// graph + a NEW rdf:type span the reconcile primitive has never reconciled. NO
// MOCKS: a real gardend MockRuntime cell, a real per-graph Oxigraph store, the
// real generic planner (B5 subject minting + render_class_triples), the real
// `apply_simple_projection_plan` fork, and the real rudof SHACL gate inside
// `reconcile_classes_validated`.
//
// The full vertical, end to end, on the example `emporium-bookmark` vocab:
//   register → resolve → generic-ingest → B5/B4-materialize → SHACL-validate →
//   query-back (lands in :projection:bookmark, NOT user:rdf/default) →
//   RE-INGEST = 0 ops (the convergence proof) →
//   TEETH: a malformed record is REJECTED by SHACL, NOTHING written →
//   ANTI-TAUTOLOGY (bend-and-revert): an EDITED record produces a real delta,
//   then re-ingest of the edit converges to 0 ops again.
// ===========================================================================

use crate::emporium::contract::get_vocabulary;
use crate::rdf::graph_subject;

/// Build a generic bookmark ingest request over one or more records. Each record
/// is a flat object: `{kind:"Bookmark", localId, url, title, ...}` — the SAME wire
/// shape a product author / agent POSTs to `/emporium/ingest/{graph}` with
/// `vocab=emporium-bookmark`.
fn bookmark_request(records: Value) -> IngestRequest {
    let body = json!({
        "vocab": "emporium-bookmark",
        "dry_run": false,
        "payload": { "kind": "generic", "records": records }
    });
    serde_json::from_value(body).expect("bookmark ingest request deserializes")
}

/// The `:projection:bookmark` sink IRI for a graph (the new placement under test).
fn bookmark_sink(graph_id: &str) -> String {
    format!("{}:projection:bookmark", graph_subject(graph_id))
}

/// Count `bm:Bookmark` subjects in the bookmark projection graph.
fn bookmark_count(app: &AppHandle, graph_id: &str) -> usize {
    let sink = bookmark_sink(graph_id);
    select(
        app,
        graph_id,
        &format!(
            "SELECT ?s WHERE {{ GRAPH <{sink}> {{ ?s a <http://mnemosyne.dev/bookmark#Bookmark> }} }}"
        ),
    )
    .len()
}

/// All triples in the bookmark sink (for the byte-stability assertion).
fn bookmark_triples(app: &AppHandle, graph_id: &str) -> Vec<(String, String, String)> {
    all_triples(app, graph_id, &bookmark_sink(graph_id))
}

/// Drive ONE bookmark ingest THROUGH the real spine and return the apply report.
/// `gather_and_plan` runs the generic planner; `apply_and_assert` routes on
/// `mode=="simple-projection"` → `apply_simple_projection_plan` →
/// `reconcile_classes_validated` (survey → diff → SHACL gate → apply).
fn ingest_bookmarks(
    app: &AppHandle,
    graph_id: &str,
    records: Value,
) -> crate::emporium::applier::ApplyReport {
    let request = bookmark_request(records);
    let planned = gather_and_plan(app, graph_id, &request).expect("bookmark gather_and_plan");
    assert_eq!(planned.plan.mode, "simple-projection", "generic plan mode");
    assert!(
        planned.plan.routes_to_simple_projection(),
        "generic plan routes to the simple-projection sink"
    );
    crate::app_runtime::async_runtime::block_on(apply_and_assert(
        app,
        graph_id,
        &planned.plan,
        planned.contract,
        &request,
    ))
}

/// The reconcile delta the apply report recorded (rdfInsert/rdfDelete on its single
/// `simple_projection_reconcile` step) — the ACTUAL applied ops against the store,
/// which is the convergence signal (zero on a converged re-ingest).
fn report_delta(report: &crate::emporium::applier::ApplyReport) -> (i64, i64) {
    let step = report.steps.first().expect("one reconcile step");
    let ins = step
        .extra
        .get("rdfInsert")
        .and_then(Value::as_i64)
        .unwrap_or(-1);
    let del = step
        .extra
        .get("rdfDelete")
        .and_then(Value::as_i64)
        .unwrap_or(-1);
    (ins, del)
}

#[test]
fn trace_generic_publication_convergence_oracle() {
    let _serial = env_serial().lock().unwrap_or_else(|p| p.into_inner());
    let profile = temp_profile("generic-pub");
    std::env::set_var("GARDEN_PROFILE_DIR", &profile);
    let result = std::panic::catch_unwind(run_generic_publication_oracle);
    std::env::remove_var("GARDEN_PROFILE_DIR");
    let _ = std::fs::remove_dir_all(&profile);
    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}

fn run_generic_publication_oracle() {
    let app = mock_app();
    let graph_id = "lab";
    seed_graph(&app, graph_id);
    let sink = bookmark_sink(graph_id);
    let user_rdf = user_rdf_graph_iri(graph_id);

    println!("\n================ FLOW 3: GENERIC PUBLICATION CONVERGENCE ORACLE ================");
    println!("vocab=emporium-bookmark  sink=<{sink}>");

    // ── STAGE 0  REGISTER → RESOLVE ──────────────────────────────────────────
    let contract = get_vocabulary("emporium-bookmark").expect("the example vocab is registered");
    assert_eq!(
        contract.write_target.as_deref(),
        Some("projection:bookmark")
    );
    println!("\n-- STAGE 0  REGISTER → RESOLVE --");
    println!("  get_vocabulary(\"emporium-bookmark\") → 1 class, write_target=projection:bookmark");

    // ── STAGE 1  GENERIC INGEST → MATERIALIZE ────────────────────────────────
    let records = json!([
        {"kind": "Bookmark", "localId": "rust-book",
         "url": "https://doc.rust-lang.org/book/", "title": "The Rust Book",
         "note": "the canonical intro", "tag": ["rust", "reference"],
         "createdAt": 1_718_700_000_000_i64},
        {"kind": "Bookmark", "localId": "sparql-spec",
         "url": "https://www.w3.org/TR/sparql11-query/", "title": "SPARQL 1.1"}
    ]);
    println!("\n-- STAGE 1  GENERIC INGEST (2 bookmarks, through the spine) --");
    let r1 = ingest_bookmarks(&app, graph_id, records.clone());
    assert!(
        r1.ok,
        "first generic ingest must succeed (apply ok=true): {r1:?}"
    );
    let (ins1, del1) = report_delta(&r1);
    println!(
        "  apply report: ok={} rdfInsert={ins1} rdfDelete={del1}",
        r1.ok
    );
    assert!(ins1 > 0, "first ingest INSERTS the bookmark footprint");
    assert_eq!(del1, 0, "first ingest deletes nothing");

    // ── STAGE 2  QUERY-BACK + PLACEMENT ISOLATION ────────────────────────────
    println!("\n-- STAGE 2  QUERY-BACK + PLACEMENT ISOLATION --");
    let count = bookmark_count(&app, graph_id);
    println!("  bm:Bookmark subjects in <{sink}>: {count}");
    assert_eq!(count, 2, "both bookmarks materialized in the bookmark sink");

    // The required + optional predicates landed for the first bookmark.
    let rust_subject = format!("{sink}:bookmark:rust-book");
    let rust_rows = select(
        &app,
        graph_id,
        &format!(
            "SELECT ?p ?o WHERE {{ GRAPH <{sink}> {{ <{rust_subject}> ?p ?o }} }} ORDER BY ?p ?o"
        ),
    );
    println!("  <{rust_subject}> ({} triples):", rust_rows.len());
    for row in &rust_rows {
        println!(
            "      {}  {}",
            strip_uri(&row.get("p").cloned().unwrap_or_default()),
            row.get("o").cloned().unwrap_or_default()
        );
    }
    let bm = "http://mnemosyne.dev/bookmark#";
    let has = |p: &str, o: &str| {
        select(
            &app,
            graph_id,
            &format!("SELECT ?x WHERE {{ GRAPH <{sink}> {{ <{rust_subject}> <{bm}{p}> {o} }} }}"),
        )
        .len()
    };
    assert_eq!(
        has("url", &format!("<https://doc.rust-lang.org/book/>")),
        1,
        "bm:url (uri) landed"
    );
    assert_eq!(has("title", "\"The Rust Book\""), 1, "bm:title landed");
    assert_eq!(
        has("note", "\"the canonical intro\""),
        1,
        "bm:note (optional) landed"
    );
    // tag is multi → two triples.
    let tag_rows = select(
        &app,
        graph_id,
        &format!("SELECT ?o WHERE {{ GRAPH <{sink}> {{ <{rust_subject}> <{bm}tag> ?o }} }}"),
    );
    assert_eq!(tag_rows.len(), 2, "both tags landed (multi predicate)");

    // ISOLATION: bookmark subjects exist ONLY in :projection:bookmark — not in the
    // user:rdf authority graph, not in the default graph (the structural guarantee
    // the new placement does not compete with CRDT authority nor leak).
    let in_user_rdf = select(
        &app,
        graph_id,
        &format!("SELECT ?s WHERE {{ GRAPH <{user_rdf}> {{ ?s a <{bm}Bookmark> }} }}"),
    )
    .len();
    let in_default = select(
        &app,
        graph_id,
        &format!("SELECT ?s WHERE {{ ?s a <{bm}Bookmark> }}"),
    )
    .len();
    assert_eq!(in_user_rdf, 0, "bookmarks must NOT leak into :user:rdf");
    assert_eq!(
        in_default, 0,
        "bookmarks must NOT appear in the default graph"
    );
    println!(
        "  ✓ isolation: bookmarks live ONLY in :projection:bookmark (0 in user:rdf, 0 in default)."
    );

    let footprint_after_first = bookmark_triples(&app, graph_id);

    // ── STAGE 3  RE-INGEST = ZERO OPS (the convergence proof) ────────────────
    println!("\n-- STAGE 3  RE-INGEST THE SAME RECORDS (convergence) --");
    let r2 = ingest_bookmarks(&app, graph_id, records.clone());
    assert!(r2.ok, "re-ingest must succeed");
    let (ins2, del2) = report_delta(&r2);
    println!(
        "  apply report: ok={} rdfInsert={ins2} rdfDelete={del2}",
        r2.ok
    );
    assert_eq!(
        ins2, 0,
        "RE-INGEST INSERTS NOTHING (convergence for the new placement)"
    );
    assert_eq!(
        del2, 0,
        "RE-INGEST DELETES NOTHING (convergence for the new placement)"
    );
    assert_eq!(
        bookmark_triples(&app, graph_id),
        footprint_after_first,
        "the bookmark sink is byte-stable across an identical re-ingest"
    );
    println!("  ✓ CONVERGENCE: re-ingest of the same records → ZERO ops; sink byte-stable.");

    // ── STAGE 4  TEETH — a malformed record is REJECTED by SHACL, no write ────
    println!("\n-- STAGE 4  TEETH (SHACL rejects a malformed record) --");
    // A Bookmark MISSING the required bm:url. The planner's frozen-vocab guard
    // (required predicate absent) raises a PlanError at gather time — the loud-halt
    // BEFORE any write. (If a malformed shape ever slipped past the planner, the
    // SHACL gate inside reconcile is the second wall; both are real, no mocks.)
    let malformed = json!([
        {"kind": "Bookmark", "localId": "broken", "title": "No URL"}
    ]);
    let bad_request = bookmark_request(malformed);
    let plan_result = gather_and_plan(&app, graph_id, &bad_request);
    assert!(
        plan_result.is_err(),
        "a Bookmark missing the required bm:url must be REJECTED (no plan)"
    );
    let err = format!("{:?}", plan_result.err().unwrap());
    assert!(
        err.contains("url") || err.contains("required"),
        "rejection names the missing required: {err}"
    );
    println!("  ✓ TEETH: a Bookmark missing required bm:url is rejected loud (no write).");
    // The sink is UNTOUCHED by the rejected ingest.
    assert_eq!(
        bookmark_count(&app, graph_id),
        2,
        "the rejected ingest wrote nothing"
    );

    // ── STAGE 5  ANTI-TAUTOLOGY (bend-and-revert) ────────────────────────────
    // EDIT the first bookmark's title (same localId → same subject): the reconcile
    // must produce a REAL delta (the rejection above was the validator doing work,
    // not a planner/sink no-op), then re-ingesting the edit converges to 0 again.
    println!("\n-- STAGE 5  ANTI-TAUTOLOGY (bend the value → real delta → converge) --");
    let edited = json!([
        {"kind": "Bookmark", "localId": "rust-book",
         "url": "https://doc.rust-lang.org/book/", "title": "The Rust Programming Language",
         "note": "the canonical intro", "tag": ["rust", "reference"],
         "createdAt": 1_718_700_000_000_i64},
        {"kind": "Bookmark", "localId": "sparql-spec",
         "url": "https://www.w3.org/TR/sparql11-query/", "title": "SPARQL 1.1"}
    ]);
    let r3 = ingest_bookmarks(&app, graph_id, edited.clone());
    assert!(r3.ok, "the edit must apply");
    let (ins3, del3) = report_delta(&r3);
    println!("  edit apply report: rdfInsert={ins3} rdfDelete={del3}");
    assert!(
        ins3 > 0 && del3 > 0,
        "the title edit is a real swap (≥1 delete + ≥1 insert)"
    );
    // The new title is live; the old title is gone (single-valued predicate).
    assert_eq!(
        has("title", "\"The Rust Programming Language\""),
        1,
        "edited title is live"
    );
    assert_eq!(
        has("title", "\"The Rust Book\""),
        0,
        "old title was replaced (not appended)"
    );
    assert_eq!(
        bookmark_count(&app, graph_id),
        2,
        "still exactly two bookmarks (edit, not add)"
    );

    // Re-ingest the edit → zero ops again (convergence holds AFTER an edit).
    let r4 = ingest_bookmarks(&app, graph_id, edited);
    let (ins4, del4) = report_delta(&r4);
    println!("  re-ingest of the edit: rdfInsert={ins4} rdfDelete={del4}");
    assert_eq!(
        (ins4, del4),
        (0, 0),
        "re-ingest of the edited records → ZERO ops"
    );
    println!("  ✓ ANTI-TAUTOLOGY: a real value change moves the store; re-applying it converges.");
    println!("================ FLOW 3 PASS ================\n");
}

// ===========================================================================
// FLOW 4 — the CHAMBER ORACLE (EA-3 §4, the hyperbaric knowledge chamber).
//
// The load-bearing proof of the agent-as-ontology-author loop, end-to-end on a
// REAL agent-proposed domain ontology. NO MOCKS: a real gardend MockRuntime cell,
// a real per-graph Oxigraph store, the real `propose_domain_ontology` handler
// (parse -> namespace-guard -> vocab_to_shacl -> rudof compile-check -> store as
// born-RDF chm:DomainOntology via the generic spine), the real two-tier contract
// resolver (`resolve_ingest_contract`), and the real rudof SHACL gate validating
// the agent's INSTANCES against ITS OWN proposed shapes.
//
// The full chamber vertical:
//   STAGE 0  PROPOSE v1 -> validated + stored in-graph (query chm:DomainOntology back)
//   STAGE 1  INGEST instances of the PROPOSED ontology (vocab=bench-domain) ->
//            validated against the AGENT'S proposed shapes -> materialized
//   STAGE 2  RE-INGEST = 0 ops (convergence for the runtime ontology's placement)
//   STAGE 3  TEETH: an instance VIOLATING the agent's proposed shape is rejected
//            loud (no write) — and the SAME instance is ACCEPTED once the agent
//            proposes a v2 whose shape permits it (the chamber is the agent's law)
//   STAGE 4  ANTI-TAUTOLOGY (bend-and-revert): a real edit moves the store, then
//            re-ingest of the edit converges to 0 ops again
//   STAGE 5  SUPERSESSION: v2 supersedes v1; v1 -> status superseded (still
//            queryable), v2 -> active; new instances validate against v2's shape.
// ===========================================================================

use crate::emporium::chamber::propose_domain_ontology;
use crate::emporium::chamber_ontology::{chamber_projection_graph_iri, CHM_NS};

/// The agent's proposed domain ontology, v1: a `bench:Trial` with a required
/// `bench:label` (string) and an optional `bench:score` (integer). The agent's
/// provisional testimony about how to model THIS benchmark's domain.
fn bench_contract_v1() -> Value {
    json!({
        "name": "bench-domain",
        "version": "1.0.0",
        "title": "Benchmark Domain",
        "description": "agent-proposed domain model for the chamber",
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
                "source_kind": "current-state",
                "identity_kind": "urn-template",
                "store_mode": "materialize",
                "store_target": "projection:bench",
                "enforcement": "halt",
                "reconciliation_strategy": "codeBacked",
                "dispatch_mode": "current-state-materialize",
                "predicates": {
                    "bench:label": {"datatype": "string", "required": true, "multi": false},
                    "bench:score": {"datatype": "integer", "required": false, "multi": false},
                    "bench:when": {"datatype": "dateTime", "required": false, "multi": false}
                }
            }
        }
    })
}

/// The agent's REFINED ontology, v2: it adds a REQUIRED `bench:phase` (string) to
/// Trial — a stricter shape (a v1 instance with no phase would now fail v2). This
/// is the agent revising its domain model mid-benchmark.
fn bench_contract_v2() -> Value {
    json!({
        "name": "bench-domain",
        "version": "2.0.0",
        "title": "Benchmark Domain",
        "description": "refined: Trial now carries a required phase",
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
                "source_kind": "current-state",
                "identity_kind": "urn-template",
                "store_mode": "materialize",
                "store_target": "projection:bench",
                "enforcement": "halt",
                "reconciliation_strategy": "codeBacked",
                "dispatch_mode": "current-state-materialize",
                "predicates": {
                    "bench:label": {"datatype": "string", "required": true, "multi": false},
                    "bench:score": {"datatype": "integer", "required": false, "multi": false},
                    "bench:when": {"datatype": "dateTime", "required": false, "multi": false},
                    "bench:phase": {"datatype": "string", "required": true, "multi": false}
                }
            }
        }
    })
}

/// Propose an ontology through the REAL `propose_domain_ontology` handler.
fn propose(app: &AppHandle, graph_id: &str, contract: Value, rationale: &str) -> AppResult<Value> {
    let args = json!({
        "graphId": graph_id,
        "contract": contract,
        "rationale": rationale,
        "observer": "chamber-agent",
        "proposedAt": 1_718_700_000_000_i64,
    });
    crate::app_runtime::async_runtime::block_on(propose_domain_ontology(app, &args))
}

use crate::app_error::AppResult;

/// Ingest `bench:Trial` instances against the agent's PROPOSED ontology
/// (vocab=bench-domain) THROUGH the real spine. The two-tier resolver finds the
/// in-graph contract; the SHACL gate validates against its shapes.
fn ingest_trials(
    app: &AppHandle,
    graph_id: &str,
    records: Value,
) -> crate::emporium::applier::ApplyReport {
    let body = json!({
        "vocab": "bench-domain",
        "dry_run": false,
        "payload": { "kind": "generic", "records": records }
    });
    let request: IngestRequest =
        serde_json::from_value(body).expect("bench ingest request deserializes");
    let planned = gather_and_plan(app, graph_id, &request).expect("bench gather_and_plan");
    assert_eq!(
        planned.plan.mode, "simple-projection",
        "bench instance plan mode"
    );
    crate::app_runtime::async_runtime::block_on(apply_and_assert(
        app,
        graph_id,
        &planned.plan,
        planned.contract,
        &request,
    ))
}

/// TRY to ingest instances and report whether they were REJECTED LOUD with NO
/// write — capturing BOTH walls the chamber raises against the AGENT'S proposed
/// shapes: (1) the planner's frozen-vocab guard (missing-required / rogue
/// predicate / unknown class — a plan-time PlanError, derived from the agent's
/// contract), and (2) the apply-time SHACL gate inside reconcile (datatype /
/// cardinality / closed-shape). Returns `Ok(())` on accept, `Err(reason)` on a
/// loud rejection at EITHER wall — so a teeth assertion does not have to know which
/// wall fired, only that the bad instance never landed.
fn try_ingest_trials(app: &AppHandle, graph_id: &str, records: Value) -> Result<(), String> {
    let body = json!({
        "vocab": "bench-domain",
        "dry_run": false,
        "payload": { "kind": "generic", "records": records }
    });
    let request: IngestRequest =
        serde_json::from_value(body).expect("bench ingest request deserializes");
    let planned = match gather_and_plan(app, graph_id, &request) {
        Ok(p) => p,
        // Wall 1: the planner's frozen-vocab guard (derived from the agent's
        // proposed contract) rejects BEFORE any write.
        Err(e) => return Err(format!("plan: {}", e.message_ref())),
    };
    let report = crate::app_runtime::async_runtime::block_on(apply_and_assert(
        app,
        graph_id,
        &planned.plan,
        planned.contract,
        &request,
    ));
    if report.ok {
        Ok(())
    } else {
        // Wall 2: the apply-time SHACL gate (against the agent's derived shapes).
        let err = report
            .steps
            .first()
            .and_then(|s| s.extra.get("error"))
            .and_then(Value::as_str)
            .unwrap_or("apply halted")
            .to_string();
        Err(format!("apply: {err}"))
    }
}

/// The `:projection:bench` sink IRI (the agent's proposed write_target).
fn bench_sink(graph_id: &str) -> String {
    format!("{}:projection:bench", crate::rdf::graph_subject(graph_id))
}

/// Count `bench:Trial` subjects in the agent's proposed sink.
fn trial_count(app: &AppHandle, graph_id: &str) -> usize {
    let sink = bench_sink(graph_id);
    select(
        app,
        graph_id,
        &format!("SELECT ?s WHERE {{ GRAPH <{sink}> {{ ?s a <http://bench.ai/domain#Trial> }} }}"),
    )
    .len()
}

fn trial_report_delta(report: &crate::emporium::applier::ApplyReport) -> (i64, i64) {
    let step = report.steps.first().expect("one reconcile step");
    let ins = step
        .extra
        .get("rdfInsert")
        .and_then(Value::as_i64)
        .unwrap_or(-1);
    let del = step
        .extra
        .get("rdfDelete")
        .and_then(Value::as_i64)
        .unwrap_or(-1);
    (ins, del)
}

/// Count ACTIVE chm:DomainOntology records of a given name in the chamber graph.
fn active_ontology_count(app: &AppHandle, graph_id: &str, name: &str) -> usize {
    let chamber = chamber_projection_graph_iri(graph_id);
    select(
        app,
        graph_id,
        &format!(
            "SELECT ?s WHERE {{ GRAPH <{chamber}> {{ \
               ?s a <{CHM_NS}DomainOntology> ; \
                  <{CHM_NS}ontologyName> \"{name}\" ; \
                  <{CHM_NS}status> \"active\" }} }}"
        ),
    )
    .len()
}

/// The status of a chm:DomainOntology of a given (name, version), if present.
fn ontology_status(app: &AppHandle, graph_id: &str, name: &str, version: &str) -> Option<String> {
    let chamber = chamber_projection_graph_iri(graph_id);
    select(
        app,
        graph_id,
        &format!(
            "SELECT ?status WHERE {{ GRAPH <{chamber}> {{ \
               ?s a <{CHM_NS}DomainOntology> ; \
                  <{CHM_NS}ontologyName> \"{name}\" ; \
                  <{CHM_NS}ontologyVersion> \"{version}\" ; \
                  <{CHM_NS}status> ?status }} }}"
        ),
    )
    .first()
    .and_then(|r| r.get("status").cloned())
    .map(|s| s.trim_matches('"').to_string())
}

#[test]
fn trace_chamber_oracle() {
    let _serial = env_serial().lock().unwrap_or_else(|p| p.into_inner());
    let profile = temp_profile("chamber");
    std::env::set_var("GARDEN_PROFILE_DIR", &profile);
    let result = std::panic::catch_unwind(run_chamber_oracle);
    std::env::remove_var("GARDEN_PROFILE_DIR");
    let _ = std::fs::remove_dir_all(&profile);
    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}

fn run_chamber_oracle() {
    let app = mock_app();
    let graph_id = "lab";
    seed_graph(&app, graph_id);
    let chamber = chamber_projection_graph_iri(graph_id);
    let sink = bench_sink(graph_id);

    println!(
        "\n================ FLOW 4: CHAMBER ORACLE (agent-as-ontology-author) ================"
    );
    println!("chamber=<{chamber}>  bench-sink=<{sink}>");

    // ── STAGE 0  PROPOSE v1 → validate + store in-graph ──────────────────────
    println!("\n-- STAGE 0  PROPOSE the domain ontology (v1) --");
    let res = propose(
        &app,
        graph_id,
        bench_contract_v1(),
        "Trial = a labelled benchmark trial",
    )
    .expect("propose v1 must succeed (the contract is well-formed)");
    assert_eq!(res.get("status").and_then(Value::as_str), Some("ok"));
    let v1_subject = res
        .get("ontologySubject")
        .and_then(Value::as_str)
        .expect("the stored ontology subject")
        .to_string();
    println!("  propose ok → ontologySubject=<{v1_subject}>");

    // PROVE it is REAL born-RDF: query the chm:DomainOntology back out of the graph.
    let onto_rows = select(
        &app,
        graph_id,
        &format!(
            "SELECT ?p ?o WHERE {{ GRAPH <{chamber}> {{ <{v1_subject}> ?p ?o }} }} ORDER BY ?p"
        ),
    );
    println!(
        "  in-graph chm:DomainOntology <{v1_subject}> ({} triples):",
        onto_rows.len()
    );
    for row in &onto_rows {
        let o = row.get("o").cloned().unwrap_or_default();
        let o = if o.len() > 90 {
            format!("{}…", &o[..90])
        } else {
            o
        };
        println!(
            "      {}  {}",
            strip_uri(&row.get("p").cloned().unwrap_or_default()),
            o
        );
    }
    assert_eq!(
        active_ontology_count(&app, graph_id, "bench-domain"),
        1,
        "exactly one active v1"
    );
    assert_eq!(
        ontology_status(&app, graph_id, "bench-domain", "1.0.0").as_deref(),
        Some("active")
    );
    // The contractJson literal round-trips (the source of the derived shapes).
    let has_json = select(
        &app,
        graph_id,
        &format!(
            "SELECT ?j WHERE {{ GRAPH <{chamber}> {{ <{v1_subject}> <{CHM_NS}contractJson> ?j }} }}"
        ),
    );
    assert_eq!(
        has_json.len(),
        1,
        "the proposed contract JSON is stored as born-RDF testimony"
    );
    println!("  ✓ the agent's proposed ontology is REAL born-RDF in the chamber graph.");

    // TEETH on the PROPOSE gate: a structurally-broken proposal (reserved prefix)
    // is rejected loud, NOTHING stored.
    let mut bad = bench_contract_v1();
    bad["primary_prefix"] = json!("mem");
    bad["namespaces"]["mem"] = json!("http://mnemosyne.dev/memory#");
    let bad_res = propose(&app, graph_id, bad, "should be rejected");
    assert!(
        bad_res.is_err(),
        "a proposal claiming a reserved core prefix must be rejected"
    );
    println!("  ✓ TEETH(propose): a reserved-prefix proposal is rejected loud (core is frozen).");

    // ── STAGE 1  INGEST instances of the PROPOSED ontology ───────────────────
    println!(
        "\n-- STAGE 1  INGEST bench:Trial instances (validated against the AGENT'S shapes) --"
    );
    let trials = json!([
        {"kind": "Trial", "localId": "t-001", "label": "first trial", "score": 7},
        {"kind": "Trial", "localId": "t-002", "label": "second trial"}
    ]);
    let r1 = ingest_trials(&app, graph_id, trials.clone());
    assert!(
        r1.ok,
        "instances of the proposed ontology must validate + land: {r1:?}"
    );
    let (ins1, del1) = trial_report_delta(&r1);
    println!("  apply: ok={} rdfInsert={ins1} rdfDelete={del1}", r1.ok);
    assert!(
        ins1 > 0 && del1 == 0,
        "first instance ingest inserts the footprint"
    );
    assert_eq!(
        trial_count(&app, graph_id),
        2,
        "both trials materialized in the proposed sink"
    );
    // The required + optional predicates landed against the agent's own shape.
    let t1 = format!("{sink}:trial:t-001");
    let has = |p: &str, o: &str| {
        select(
            &app,
            graph_id,
            &format!("SELECT ?x WHERE {{ GRAPH <{sink}> {{ <{t1}> <http://bench.ai/domain#{p}> {o} }} }}"),
        )
        .len()
    };
    assert_eq!(has("label", "\"first trial\""), 1, "bench:label landed");
    assert_eq!(has("score", "7"), 1, "bench:score (xsd:integer) landed");
    println!("  ✓ instances validated against the agent's OWN proposed shapes + materialized.");

    // DETERMINISTIC INSTRUMENTATION (the metrics Vera scoped — build the instrument):
    // the apply report names the ontology used, the materialization latency, a
    // (clean-apply) zero violation count, and the result count.
    let step1 = r1.steps.first().expect("one reconcile step");
    assert_eq!(
        step1.extra.get("ontologyUsed").and_then(Value::as_str),
        Some("bench-domain@1.0.0"),
        "the metric records WHICH ontology validated the instances"
    );
    assert_eq!(
        step1
            .extra
            .get("schemaViolationCount")
            .and_then(Value::as_i64),
        Some(0),
        "a clean apply records zero schema violations"
    );
    assert!(
        step1
            .extra
            .get("materializationLatencyMs")
            .and_then(Value::as_u64)
            .is_some(),
        "materialization latency is instrumented"
    );
    assert!(
        step1
            .extra
            .get("resultCount")
            .and_then(Value::as_i64)
            .unwrap_or(0)
            > 0,
        "result_count (validated triples) is instrumented"
    );
    println!(
        "  ✓ INSTRUMENTATION: ontologyUsed={:?} materializationLatencyMs={:?} schemaViolationCount=0 resultCount={:?}",
        step1.extra.get("ontologyUsed"),
        step1.extra.get("materializationLatencyMs"),
        step1.extra.get("resultCount"),
    );

    let footprint_after_first = all_triples(&app, graph_id, &sink);

    // ── STAGE 2  RE-INGEST = ZERO OPS (convergence) ──────────────────────────
    println!("\n-- STAGE 2  RE-INGEST the same instances (convergence) --");
    let r2 = ingest_trials(&app, graph_id, trials.clone());
    let (ins2, del2) = trial_report_delta(&r2);
    println!("  apply: rdfInsert={ins2} rdfDelete={del2}");
    assert_eq!(
        (ins2, del2),
        (0, 0),
        "re-ingest of identical instances → ZERO ops"
    );
    assert_eq!(
        all_triples(&app, graph_id, &sink),
        footprint_after_first,
        "the proposed sink is byte-stable across an identical re-ingest"
    );
    println!("  ✓ CONVERGENCE: the runtime ontology's instances converge to zero ops.");

    // ── STAGE 3  TEETH — an instance VIOLATING the agent's shape is rejected ──
    println!(
        "\n-- STAGE 3  TEETH (the APPLY-TIME SHACL gate rejects against the agent's shape) --"
    );
    // bench:when is declared xsd:dateTime. A non-dateTime string ("not-a-date") is
    // rendered FAITHFULLY by the planner (it is a declared predicate, and the
    // dateTime renderer passes a string through verbatim) — so the only wall that
    // can catch it is the apply-time SHACL gate inside reconcile, validating the
    // instance against the shapes DERIVED FROM THE AGENT'S PROPOSED CONTRACT. This
    // is the chamber's distinctive proof: the gate uses the agent's OWN shapes.
    let bad_instance = json!([
        {"kind": "Trial", "localId": "t-bad", "label": "bad", "when": "not-a-date"}
    ]);
    let r_bad = ingest_trials(&app, graph_id, bad_instance);
    assert!(
        !r_bad.ok,
        "an instance violating the agent's proposed shape must be rejected"
    );
    let bad_err = r_bad
        .steps
        .first()
        .and_then(|s| s.extra.get("error"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    assert!(
        bad_err.contains("SHACL"),
        "the rejection is the apply-time SHACL gate (against the agent's shape): {bad_err}"
    );
    assert!(
        bad_err.contains("when") || bad_err.contains("dateTime") || bad_err.contains("t-bad"),
        "the rejection names the offending bench:when datatype: {bad_err}"
    );
    assert_eq!(
        trial_count(&app, graph_id),
        2,
        "the rejected instance wrote NOTHING (no t-bad)"
    );
    let no_bad = select(
        &app,
        graph_id,
        &format!("SELECT ?s WHERE {{ GRAPH <{sink}> {{ <{sink}:trial:t-bad> ?p ?o }} }}"),
    );
    assert!(
        no_bad.is_empty(),
        "no partial write of the rejected instance"
    );
    println!("  ✓ TEETH: the apply-time SHACL gate rejects against the agent's OWN derived shape.");

    // ── STAGE 4  ANTI-TAUTOLOGY (bend-and-revert) ────────────────────────────
    println!("\n-- STAGE 4  ANTI-TAUTOLOGY (edit an instance → real delta → converge) --");
    let edited = json!([
        {"kind": "Trial", "localId": "t-001", "label": "FIRST trial (edited)", "score": 7},
        {"kind": "Trial", "localId": "t-002", "label": "second trial"}
    ]);
    let r3 = ingest_trials(&app, graph_id, edited.clone());
    assert!(r3.ok, "the edit must apply");
    let (ins3, del3) = trial_report_delta(&r3);
    println!("  edit apply: rdfInsert={ins3} rdfDelete={del3}");
    assert!(
        ins3 > 0 && del3 > 0,
        "the label edit is a real swap (≥1 delete + ≥1 insert)"
    );
    assert_eq!(
        has("label", "\"FIRST trial (edited)\""),
        1,
        "edited label is live"
    );
    assert_eq!(
        has("label", "\"first trial\""),
        0,
        "old label replaced (single-valued)"
    );
    let r4 = ingest_trials(&app, graph_id, edited);
    assert_eq!(
        trial_report_delta(&r4),
        (0, 0),
        "re-ingest of the edit → ZERO ops"
    );
    println!("  ✓ ANTI-TAUTOLOGY: a real instance change moves the store; re-applying converges.");

    // ── STAGE 5  SUPERSESSION — propose v2 (a stricter shape) ────────────────
    println!("\n-- STAGE 5  SUPERSESSION (propose v2; v1 → superseded but queryable) --");
    let res2 = propose(
        &app,
        graph_id,
        bench_contract_v2(),
        "Trial now requires a phase",
    )
    .expect("propose v2 must succeed");
    let v2_subject = res2
        .get("ontologySubject")
        .and_then(Value::as_str)
        .unwrap()
        .to_string();
    println!("  propose v2 ok → ontologySubject=<{v2_subject}>");
    assert_ne!(v2_subject, v1_subject, "v2 mints a distinct subject");
    // Exactly one ACTIVE bench-domain (v2); v1 is now superseded but still present.
    assert_eq!(
        active_ontology_count(&app, graph_id, "bench-domain"),
        1,
        "exactly one active (v2)"
    );
    assert_eq!(
        ontology_status(&app, graph_id, "bench-domain", "1.0.0").as_deref(),
        Some("superseded"),
        "v1 is demoted to superseded"
    );
    assert_eq!(
        ontology_status(&app, graph_id, "bench-domain", "2.0.0").as_deref(),
        Some("active"),
        "v2 is active"
    );
    // v1 stays QUERYABLE (the append-only lineage): its record + back-pointer survive.
    let v1_still = select(
        &app,
        graph_id,
        &format!(
            "SELECT ?by WHERE {{ GRAPH <{chamber}> {{ <{v1_subject}> <{CHM_NS}supersededBy> ?by }} }}"
        ),
    );
    assert_eq!(
        v1_still.len(),
        1,
        "v1 carries a supersededBy → v2 back-pointer (still queryable)"
    );
    let v2_fwd = select(
        &app,
        graph_id,
        &format!(
            "SELECT ?s WHERE {{ GRAPH <{chamber}> {{ <{v2_subject}> <{CHM_NS}supersedes> ?s }} }}"
        ),
    );
    assert_eq!(v2_fwd.len(), 1, "v2 carries a supersedes → v1 edge");
    println!("  ✓ SUPERSESSION: v1 superseded (queryable) ← supersededBy → v2 active.");

    // TEETH on v2's stricter shape: a Trial WITHOUT the now-required bench:phase is
    // rejected against v2 (the agent's revised law governs new instances).
    println!("\n-- STAGE 5b  v2's stricter shape governs new instances --");
    let v1_shaped = json!([
        {"kind": "Trial", "localId": "t-003", "label": "no phase"}
    ]);
    let rejected = try_ingest_trials(&app, graph_id, v1_shaped);
    assert!(
        rejected.is_err(),
        "a Trial without the v2-required bench:phase must be rejected (v2 is now active)"
    );
    println!("  v2 rejects a phase-less Trial: {}", rejected.unwrap_err());
    // The SAME instance WITH a phase validates against v2.
    let v2_shaped = json!([
        {"kind": "Trial", "localId": "t-003", "label": "with phase", "phase": "warmup"}
    ]);
    let r_v2shape = ingest_trials(&app, graph_id, v2_shaped);
    assert!(
        r_v2shape.ok,
        "a Trial carrying bench:phase validates against v2: {r_v2shape:?}"
    );
    assert_eq!(
        has_pred(
            &app,
            graph_id,
            &sink,
            &format!("{sink}:trial:t-003"),
            "phase",
            "\"warmup\""
        ),
        1,
        "the v2-shaped instance landed with its phase"
    );
    println!("  ✓ the agent's revised v2 shape governs new instances (chamber = the agent's law).");
    println!("================ FLOW 4 PASS ================\n");
}

/// Count triples `<subject> bench:<pred> <object>` in a sink (the v2-shape probe).
fn has_pred(
    app: &AppHandle,
    graph_id: &str,
    sink: &str,
    subject: &str,
    pred: &str,
    obj: &str,
) -> usize {
    select(
        app,
        graph_id,
        &format!("SELECT ?x WHERE {{ GRAPH <{sink}> {{ <{subject}> <http://bench.ai/domain#{pred}> {obj} }} }}"),
    )
    .len()
}

// ===========================================================================
// FLOW 5 — the OPENAPI POPULATED SERVING ORACLE (EA-3 Seq 9 / UC-2).
//
// The load-bearing proof that the OpenAPI face's POPULATED serving works end to
// end — the difference between "the materializer works in tests" and "publish an
// API as triples → curl its REAL OpenAPI doc". NO MOCKS: a real gardend
// MockRuntime cell + per-graph Oxigraph store, the real generic spine
// (gather_and_plan → apply_simple_projection_plan → reconcile_classes_validated +
// the rudof SHACL gate), the real `read_api_surface` SPARQL read-back, and the
// real pure `vocab_to_openapi` materializer.
//
// The full vertical, on a concrete example API (createTask POST /tasks + getTask
// GET /tasks/{id}):
//   PUBLISH api:Operation/Parameter/Response/WorkflowBinding/Server via the
//   generic spine (write_target projection:api, SHACL-validated) →
//   READ them back with read_api_surface (reconstruct the ApiSurface) →
//   MATERIALIZE with vocab_to_openapi (the POPULATED spec, NOT a metadata shell) →
//   ASSERT the published paths/methods + x-executor + the inputSchema/outputSchema
//   embedded VERBATIM (byte-faithful round-trip: the published JSON Schema literal
//   survives store→SPARQL→struct→OpenAPI unchanged) →
//   SERVE via the real route helper (populated_openapi_response → 200 +
//   application/openapi+json + ETag) →
//   CONVERGENCE: re-ingest the same records → ZERO ops →
//   TEETH: an Operation missing the required api:operationId is REJECTED, no write.
// ===========================================================================

use axum::http::HeaderMap;

use crate::emporium::openapi_emit::{
    api_projection_graph_iri, open_api_store, read_api_surface, vocab_to_openapi,
};
use crate::emporium::vocab_routes::populated_openapi_response;

/// Build a generic `sophia-api` ingest request over the given records — the SAME
/// wire shape a product author / agent POSTs to `/emporium/ingest/{graph}` with
/// `vocab=sophia-api`.
fn api_request(records: Value) -> IngestRequest {
    let body = json!({
        "vocab": "sophia-api",
        "dry_run": false,
        "payload": { "kind": "generic", "records": records }
    });
    serde_json::from_value(body).expect("sophia-api ingest request deserializes")
}

/// Drive ONE sophia-api ingest THROUGH the real spine and return the apply report.
/// Routes (on `mode=="simple-projection"`) to `apply_simple_projection_plan` →
/// `reconcile_classes_validated` (survey → diff → SHACL gate → apply into
/// `:projection:api`).
fn ingest_api(
    app: &AppHandle,
    graph_id: &str,
    records: Value,
) -> crate::emporium::applier::ApplyReport {
    let request = api_request(records);
    let planned = gather_and_plan(app, graph_id, &request).expect("sophia-api gather_and_plan");
    assert_eq!(planned.plan.mode, "simple-projection", "generic plan mode");
    assert!(
        planned.plan.routes_to_simple_projection(),
        "generic plan routes to the simple-projection sink"
    );
    crate::app_runtime::async_runtime::block_on(apply_and_assert(
        app,
        graph_id,
        &planned.plan,
        planned.contract,
        &request,
    ))
}

/// The example API surface authored against the sophia-api vocab: createTask (POST
/// /tasks, gated, full I/O JSON-Schema literals) + getTask (GET /tasks/{id}, dumb,
/// a path param + output schema) + a production Server. The Operation/Binding link
/// URIs are the minted projection subjects (`{graph_subject}:projection:api:…`).
fn example_api_records(graph_id: &str) -> (Value, String, String) {
    let root = format!("urn:mnemosyne:local:graph:{graph_id}");
    let create_op = format!("{root}:projection:api:operation:createTask");
    let get_op = format!("{root}:projection:api:operation:getTask");
    let create_binding = format!("{root}:projection:api:binding:createTask");
    let get_binding = format!("{root}:projection:api:binding:getTask");

    // The load-bearing fixtures — these JSON-Schema literals MUST survive verbatim.
    let create_input = r#"{"type":"object","required":["title"],"properties":{"title":{"type":"string"},"due":{"type":"string","format":"date-time"}}}"#;
    let create_output =
        r#"{"type":"object","properties":{"id":{"type":"string"},"title":{"type":"string"}}}"#;
    let get_output = r#"{"type":"object","properties":{"id":{"type":"string"},"title":{"type":"string"},"done":{"type":"boolean"}}}"#;

    let records = json!([
        {"kind": "Operation", "localId": "createTask",
         "operationId": "createTask", "method": "POST", "path": "/tasks",
         "summary": "Create a task", "description": "Files a new task into the graph.",
         "deprecated": false, "hasBinding": create_binding},
        {"kind": "Parameter", "localId": "createTask:title",
         "operationId": "createTask", "name": "title", "in": "query",
         "required": true, "datatype": "string", "description": "The task title",
         "ofOperation": create_op},
        {"kind": "Response", "localId": "createTask:200",
         "operationId": "createTask", "statusCode": 200,
         "description": "The created task", "ofOperation": create_op},
        {"kind": "Response", "localId": "createTask:400",
         "operationId": "createTask", "statusCode": 400,
         "description": "Invalid input", "ofOperation": create_op, "errorCode": "invalid_input"},
        {"kind": "WorkflowBinding", "localId": "createTask",
         "operationId": "createTask", "bindsOperation": create_op,
         "workflowName": "file-task", "executor": "gated",
         "inputSchema": create_input, "outputSchema": create_output},
        {"kind": "Operation", "localId": "getTask",
         "operationId": "getTask", "method": "GET", "path": "/tasks/{id}",
         "hasBinding": get_binding},
        {"kind": "Parameter", "localId": "getTask:id",
         "operationId": "getTask", "name": "id", "in": "path",
         "required": true, "datatype": "string", "description": "The task id",
         "ofOperation": get_op},
        {"kind": "WorkflowBinding", "localId": "getTask",
         "operationId": "getTask", "bindsOperation": get_op,
         "workflowName": "read-task", "executor": "dumb",
         "outputSchema": get_output},
        {"kind": "Server", "localId": "prod",
         "url": "https://api.sophia-labs.com", "description": "Production",
         "environment": "prod"}
    ]);
    (records, create_input.to_string(), get_output.to_string())
}

#[test]
fn trace_openapi_populated_serving_oracle() {
    let _serial = env_serial().lock().unwrap_or_else(|p| p.into_inner());
    let profile = temp_profile("openapi-serve");
    std::env::set_var("GARDEN_PROFILE_DIR", &profile);
    let result = std::panic::catch_unwind(run_openapi_populated_serving_oracle);
    std::env::remove_var("GARDEN_PROFILE_DIR");
    let _ = std::fs::remove_dir_all(&profile);
    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}

fn run_openapi_populated_serving_oracle() {
    let app = mock_app();
    let graph_id = "lab";
    seed_graph(&app, graph_id);
    let sink = api_projection_graph_iri(graph_id);
    let user_rdf = user_rdf_graph_iri(graph_id);
    let api_ns = "http://sophia.ai/api#";

    println!("\n================ FLOW 5: OPENAPI POPULATED SERVING ORACLE ================");
    println!("vocab=sophia-api  sink=<{sink}>");

    // ── STAGE 0  REGISTER → RESOLVE ──────────────────────────────────────────
    let contract = get_vocabulary("sophia-api").expect("the sophia-api vocab is registered");
    assert_eq!(contract.write_target.as_deref(), Some("projection:api"));
    println!("\n-- STAGE 0  REGISTER → RESOLVE --");
    println!("  get_vocabulary(\"sophia-api\") → 5 classes, write_target=projection:api");

    // ── STAGE 1  PUBLISH api: INSTANCES THROUGH THE GENERIC SPINE ────────────
    let (records, create_input, get_output) = example_api_records(graph_id);
    println!("\n-- STAGE 1  GENERIC INGEST (createTask + getTask, through the spine) --");
    let r1 = ingest_api(&app, graph_id, records.clone());
    assert!(
        r1.ok,
        "first sophia-api ingest must succeed (apply ok=true): {r1:?}"
    );
    let (ins1, del1) = report_delta(&r1);
    println!(
        "  apply report: ok={} rdfInsert={ins1} rdfDelete={del1}",
        r1.ok
    );
    assert!(ins1 > 0, "first ingest INSERTS the api: footprint");
    assert_eq!(del1, 0, "first ingest deletes nothing");

    // ── STAGE 2  QUERY-BACK + PLACEMENT ISOLATION ────────────────────────────
    println!("\n-- STAGE 2  QUERY-BACK + PLACEMENT ISOLATION --");
    let op_count = select(
        &app,
        graph_id,
        &format!("SELECT ?s WHERE {{ GRAPH <{sink}> {{ ?s a <{api_ns}Operation> }} }}"),
    )
    .len();
    println!("  api:Operation subjects in <{sink}>: {op_count}");
    assert_eq!(op_count, 2, "both operations materialized in the api sink");

    // ISOLATION: api: subjects exist ONLY in :projection:api — not user:rdf, not default.
    let in_user_rdf = select(
        &app,
        graph_id,
        &format!("SELECT ?s WHERE {{ GRAPH <{user_rdf}> {{ ?s a <{api_ns}Operation> }} }}"),
    )
    .len();
    let in_default = select(
        &app,
        graph_id,
        &format!("SELECT ?s WHERE {{ ?s a <{api_ns}Operation> }}"),
    )
    .len();
    assert_eq!(
        in_user_rdf, 0,
        "api: instances must NOT leak into :user:rdf"
    );
    assert_eq!(
        in_default, 0,
        "api: instances must NOT appear in the default graph"
    );
    println!(
        "  ✓ isolation: api: instances live ONLY in :projection:api (0 in user:rdf, 0 in default)."
    );

    // ── STAGE 3  READ-BACK — reconstruct the ApiSurface from the store ───────
    println!("\n-- STAGE 3  READ-BACK (read_api_surface reconstructs the ApiSurface) --");
    let store = open_api_store(&app, graph_id).expect("open the per-graph store");
    let surface = read_api_surface(&store, graph_id).expect("read_api_surface");
    println!(
        "  reconstructed surface: {} operations, {} servers",
        surface.operations.len(),
        surface.servers.len()
    );
    assert_eq!(surface.operations.len(), 2, "two operations reconstructed");
    assert_eq!(surface.servers.len(), 1, "one server reconstructed");

    // The operations are ordered alphabetically by operationId (createTask, getTask).
    let create = surface
        .operations
        .iter()
        .find(|o| o.operation_id == "createTask")
        .expect("createTask reconstructed");
    assert_eq!(create.method, "POST");
    assert_eq!(create.path, "/tasks");
    assert_eq!(create.summary.as_deref(), Some("Create a task"));
    // createTask carries its 200 + 400 responses + the gated binding with BOTH schemas.
    // The Cartesian-unwind dedup is load-bearing here: createTask's SELECT rows are
    // (1 param × 2 responses × 1 binding) = 2 rows, so the title param + each response
    // must each appear EXACTLY ONCE despite repeating across rows.
    assert_eq!(
        create.parameters.len(),
        1,
        "createTask's title param appears once (dedup)"
    );
    assert_eq!(create.parameters[0].name, "title");
    assert_eq!(create.parameters[0].location, "query");
    assert_eq!(
        create.responses.len(),
        2,
        "createTask has 2 responses (dedup, not 2×param)"
    );
    let create_binding = create.binding.as_ref().expect("createTask has a binding");
    assert_eq!(create_binding.workflow_name, "file-task");
    assert_eq!(create_binding.executor, "gated");
    assert_eq!(
        create_binding.input_schema.as_deref(),
        Some(create_input.as_str()),
        "the published inputSchema literal survives read-back VERBATIM"
    );

    let get = surface
        .operations
        .iter()
        .find(|o| o.operation_id == "getTask")
        .expect("getTask reconstructed");
    assert_eq!(get.method, "GET");
    assert_eq!(get.path, "/tasks/{id}");
    assert_eq!(get.parameters.len(), 1, "getTask has its path param");
    assert_eq!(get.parameters[0].name, "id");
    assert_eq!(get.parameters[0].location, "path");
    let get_binding = get.binding.as_ref().expect("getTask has a binding");
    assert_eq!(get_binding.executor, "dumb");
    assert_eq!(
        get_binding.output_schema.as_deref(),
        Some(get_output.as_str())
    );
    println!("  ✓ surface reconstructed: createTask(POST /tasks, gated, in+out schema) + getTask(GET /tasks/{{id}}, dumb).");

    // ── STAGE 4  MATERIALIZE — the POPULATED spec (NOT a shell) ──────────────
    println!("\n-- STAGE 4  MATERIALIZE (vocab_to_openapi over the read-back surface) --");
    let spec = vocab_to_openapi(contract, &surface).expect("materialize the populated spec");
    assert!(spec["openapi"].as_str().unwrap().starts_with("3."));
    // POPULATED: the published paths/methods are present (NOT an empty-paths shell).
    let post = &spec["paths"]["/tasks"]["post"];
    assert_eq!(
        post["operationId"], "createTask",
        "POST /tasks is in the populated paths"
    );
    assert_eq!(
        post["x-executor"], "gated",
        "x-executor discovery hook present"
    );
    assert_eq!(post["x-workflow-name"], "file-task");
    let getp = &spec["paths"]["/tasks/{id}"]["get"];
    assert_eq!(
        getp["operationId"], "getTask",
        "GET /tasks/{{id}} is in the populated paths"
    );
    assert_eq!(getp["x-executor"], "dumb");
    // The 400 error response landed.
    assert_eq!(post["responses"]["400"]["description"], "Invalid input");

    // ── ROUND-TRIP TEETH — the inputSchema is byte-faithful in components/schemas.
    let expected_input: Value = serde_json::from_str(&create_input).unwrap();
    assert_eq!(
        spec["components"]["schemas"]["createTaskInput"], expected_input,
        "the published inputSchema is embedded VERBATIM (publish→read→materialize byte-faithful)"
    );
    assert_eq!(
        post["requestBody"]["content"]["application/json"]["schema"]["$ref"],
        "#/components/schemas/createTaskInput"
    );
    let expected_output: Value = serde_json::from_str(&get_output).unwrap();
    assert_eq!(
        spec["components"]["schemas"]["getTaskOutput"], expected_output,
        "the published outputSchema is embedded VERBATIM"
    );
    // The populated spec is NOT the empty-paths shell render_body emits without a graph.
    assert!(
        spec["paths"].as_object().unwrap().len() == 2,
        "POPULATED: exactly the 2 published paths (not an empty shell)"
    );
    println!(
        "  ✓ POPULATED spec: /tasks + /tasks/{{id}} present; createTaskInput embedded VERBATIM."
    );

    // ── STAGE 5  SERVE — the real route helper returns 200 + openapi media ───
    println!("\n-- STAGE 5  SERVE (populated_openapi_response → 200 application/openapi+json) --");
    let response = populated_openapi_response(&store, graph_id, Some("openapi"), &HeaderMap::new());
    assert_eq!(
        response.status(),
        axum::http::StatusCode::OK,
        "the route serves 200"
    );
    assert_eq!(
        response
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .unwrap()
            .to_str()
            .unwrap(),
        "application/openapi+json"
    );
    // Read the served body and prove it is the SAME populated spec.
    let body_bytes =
        crate::app_runtime::async_runtime::block_on(axum::body::to_bytes(response.into_body(), usize::MAX))
            .expect("collect the served body");
    let served: Value = serde_json::from_slice(&body_bytes).expect("served body is valid JSON");
    assert_eq!(
        served["paths"]["/tasks"]["post"]["operationId"],
        "createTask"
    );
    assert_eq!(
        served["components"]["schemas"]["createTaskInput"], expected_input,
        "the SERVED body carries the inputSchema VERBATIM (curl gets the real schema)"
    );
    println!("  ✓ SERVE: 200 + application/openapi+json; the served body IS the populated spec.");

    // ── STAGE 6  CONVERGENCE — re-ingest the same records → ZERO ops ─────────
    println!("\n-- STAGE 6  RE-INGEST THE SAME RECORDS (convergence) --");
    let r2 = ingest_api(&app, graph_id, records.clone());
    assert!(r2.ok, "re-ingest must succeed");
    let (ins2, del2) = report_delta(&r2);
    println!(
        "  apply report: ok={} rdfInsert={ins2} rdfDelete={del2}",
        r2.ok
    );
    assert_eq!(
        ins2, 0,
        "RE-INGEST INSERTS NOTHING (convergence for :projection:api)"
    );
    assert_eq!(
        del2, 0,
        "RE-INGEST DELETES NOTHING (convergence for :projection:api)"
    );
    // The served spec is byte-stable across a converged re-ingest.
    let surface2 = read_api_surface(&store, graph_id).expect("read_api_surface after re-ingest");
    let spec2 = vocab_to_openapi(contract, &surface2).expect("re-materialize");
    assert_eq!(
        serde_json::to_string(&spec).unwrap(),
        serde_json::to_string(&spec2).unwrap(),
        "the populated spec is byte-stable across an identical re-ingest"
    );
    println!("  ✓ CONVERGENCE: re-ingest → ZERO ops; populated spec byte-stable.");

    // ── STAGE 7  TEETH — a malformed Operation is REJECTED, no write ─────────
    println!("\n-- STAGE 7  TEETH (an Operation missing required api:operationId is rejected) --");
    let malformed = json!([
        {"kind": "Operation", "localId": "broken", "method": "GET", "path": "/broken"}
    ]);
    let bad_request = api_request(malformed);
    let plan_result = gather_and_plan(&app, graph_id, &bad_request);
    assert!(
        plan_result.is_err(),
        "an Operation missing the required api:operationId must be REJECTED (no plan)"
    );
    let err = format!("{:?}", plan_result.err().unwrap());
    assert!(
        err.contains("operationId") || err.contains("required"),
        "rejection names the missing required: {err}"
    );
    println!(
        "  ✓ TEETH: an Operation missing required api:operationId is rejected loud (no write)."
    );
    // The sink is UNTOUCHED by the rejected ingest — still exactly 2 operations.
    let op_count_after = select(
        &app,
        graph_id,
        &format!("SELECT ?s WHERE {{ GRAPH <{sink}> {{ ?s a <{api_ns}Operation> }} }}"),
    )
    .len();
    assert_eq!(op_count_after, 2, "the rejected ingest wrote nothing");
    println!("================ FLOW 5 PASS ================\n");
}

// ════════════════════════════════════════════════════════════════════════════
//  CA-1 STRETCH (DoD): publish the `graph-survey-synthesize` api:WorkflowBinding
//  as a REAL api: object and curl-emit the OpenAPI 3.x doc.
//
//  This is the shape choreograph's live `GET /api/workflows/{name}/schema`
//  returns (workflowName + executor + I/O JSON Schemas), published here as
//  born-RDF through the SAME sophia-api publication spine the createTask/getTask
//  oracle proves — then read back and curl-emitted via the real OpenAPI face.
//
//  NO MOCK: real generic ingest → real :projection:api store → real
//  read_api_surface SPARQL read-back → real vocab_to_openapi → real
//  populated_openapi_response serve. The ONLY remaining integration is the LIVE
//  fetch from a running choreograph instance (needs a reachable endpoint); the
//  schema content here is the documented contract that fetch returns.
// ════════════════════════════════════════════════════════════════════════════

/// The `graph-survey-synthesize` WorkflowBinding records: one Operation
/// (`POST /api/workflows/graph-survey-synthesize/runs`) bound to the gated
/// choreograph workflow, carrying the I/O JSON Schemas as verbatim literals.
fn survey_synthesize_records(graph_id: &str) -> (Value, String, String) {
    let root = format!("urn:mnemosyne:local:graph:{graph_id}");
    let op = format!("{root}:projection:api:operation:graphSurveySynthesize");
    let binding = format!("{root}:projection:api:binding:graphSurveySynthesize");

    // The I/O contract choreograph's GET /api/workflows/{name}/schema returns for
    // graph-survey-synthesize: an input naming the target graph + survey scope, an
    // output carrying the synthesized document ref + the surveyed-span summary.
    let input = r#"{"type":"object","required":["graphId"],"properties":{"graphId":{"type":"string","description":"the graph to survey"},"scope":{"type":"string","enum":["workspace","folder","document"],"default":"workspace"},"prompt":{"type":"string","description":"synthesis instruction"}}}"#;
    let output = r#"{"type":"object","required":["documentId","spanCount"],"properties":{"documentId":{"type":"string","description":"the synthesized document"},"spanCount":{"type":"integer","description":"number of surveyed spans"},"summary":{"type":"string"}}}"#;

    let records = json!([
        {"kind": "Operation", "localId": "graphSurveySynthesize",
         "operationId": "graphSurveySynthesize",
         "method": "POST", "path": "/api/workflows/graph-survey-synthesize/runs",
         "summary": "Survey a graph and synthesize a document",
         "description": "Runs choreograph's graph-survey-synthesize workflow: survey the graph's spans, then synthesize a new document. Mirrors GET /api/workflows/{name}/schema.",
         "deprecated": false, "hasBinding": binding},
        {"kind": "WorkflowBinding", "localId": "graphSurveySynthesize",
         "operationId": "graphSurveySynthesize", "bindsOperation": op,
         "workflowName": "graph-survey-synthesize", "executor": "gated",
         "description": "Bound to the choreograph graph-survey-synthesize workflow.",
         "inputSchema": input, "outputSchema": output}
    ]);
    (records, input.to_string(), output.to_string())
}

#[test]
fn trace_survey_synthesize_workflow_binding_openapi() {
    let _serial = env_serial().lock().unwrap_or_else(|p| p.into_inner());
    let profile = temp_profile("survey-synthesize-openapi");
    std::env::set_var("GARDEN_PROFILE_DIR", &profile);
    let result = std::panic::catch_unwind(run_survey_synthesize_workflow_binding_openapi);
    std::env::remove_var("GARDEN_PROFILE_DIR");
    let _ = std::fs::remove_dir_all(&profile);
    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}

fn run_survey_synthesize_workflow_binding_openapi() {
    let app = mock_app();
    let graph_id = "lab";
    seed_graph(&app, graph_id);
    let contract = get_vocabulary("sophia-api").expect("sophia-api registered");

    println!(
        "\n========== CA-1 STRETCH: graph-survey-synthesize WorkflowBinding → OpenAPI =========="
    );

    // ── PUBLISH the binding as born-RDF through the real generic spine ───────
    let (records, input, output) = survey_synthesize_records(graph_id);
    let r1 = ingest_api(&app, graph_id, records);
    assert!(
        r1.ok,
        "publishing the survey-synthesize binding must succeed: {r1:?}"
    );
    let (ins1, _del1) = report_delta(&r1);
    assert!(
        ins1 > 0,
        "the api: footprint is written into :projection:api"
    );
    println!("  ✓ PUBLISH: graph-survey-synthesize Operation + WorkflowBinding ingested ({ins1} triples).");

    // ── READ-BACK the surface from the real store ────────────────────────────
    let store = open_api_store(&app, graph_id).expect("open the per-graph store");
    let surface = read_api_surface(&store, graph_id).expect("read_api_surface");
    assert_eq!(surface.operations.len(), 1, "one operation reconstructed");
    let op = &surface.operations[0];
    assert_eq!(op.operation_id, "graphSurveySynthesize");
    assert_eq!(op.method, "POST");
    assert_eq!(op.path, "/api/workflows/graph-survey-synthesize/runs");
    let binding = op
        .binding
        .as_ref()
        .expect("the WorkflowBinding reconstructed");
    assert_eq!(binding.workflow_name, "graph-survey-synthesize");
    assert_eq!(binding.executor, "gated");
    assert_eq!(
        binding.input_schema.as_deref(),
        Some(input.as_str()),
        "the published inputSchema survives read-back VERBATIM"
    );
    assert_eq!(binding.output_schema.as_deref(), Some(output.as_str()));
    println!("  ✓ READ-BACK: binding(workflowName=graph-survey-synthesize, gated, in+out schema).");

    // ── CURL-EMIT the OpenAPI doc through the real serve face ────────────────
    let response = populated_openapi_response(&store, graph_id, Some("openapi"), &HeaderMap::new());
    assert_eq!(response.status(), axum::http::StatusCode::OK);
    let body_bytes =
        crate::app_runtime::async_runtime::block_on(axum::body::to_bytes(response.into_body(), usize::MAX))
            .expect("collect served body");
    let served: Value =
        serde_json::from_slice(&body_bytes).expect("served body is valid OpenAPI JSON");

    let post = &served["paths"]["/api/workflows/graph-survey-synthesize/runs"]["post"];
    assert_eq!(
        post["operationId"], "graphSurveySynthesize",
        "the published path is in the served spec"
    );
    assert_eq!(
        post["x-workflow-name"], "graph-survey-synthesize",
        "x-workflow-name discovery hook"
    );
    assert_eq!(post["x-executor"], "gated", "x-executor discovery hook");
    // The I/O JSON Schemas are embedded VERBATIM into components/schemas.
    let expected_input: Value = serde_json::from_str(&input).unwrap();
    let expected_output: Value = serde_json::from_str(&output).unwrap();
    assert_eq!(
        served["components"]["schemas"]["graphSurveySynthesizeInput"], expected_input,
        "the published inputSchema is curl-emitted VERBATIM (the choreograph schema boundary)"
    );
    assert_eq!(
        served["components"]["schemas"]["graphSurveySynthesizeOutput"],
        expected_output
    );
    assert_eq!(
        post["requestBody"]["content"]["application/json"]["schema"]["$ref"],
        "#/components/schemas/graphSurveySynthesizeInput"
    );
    // The contract is well-formed against vocab_to_openapi's purity too.
    let spec = vocab_to_openapi(contract, &surface).expect("re-materialize");
    assert!(spec["openapi"].as_str().unwrap().starts_with("3."));
    println!(
        "  ✓ CURL-EMIT: OpenAPI 3.x doc with POST /api/workflows/graph-survey-synthesize/runs,"
    );
    println!("    x-workflow-name=graph-survey-synthesize, I/O schemas embedded VERBATIM.");
    println!("========== CA-1 STRETCH PASS ==========\n");
}

// ---------------------------------------------------------------------------
// FLOW: CONCURRENT SUPERSESSION THROUGH THE WRITE GATE — two writers race to
// supersede the same head through the gated memory funnel
// (`ingest_memory_record` holds the per-graph write gate across
// survey→plan→apply→queue-append). NO MOCKS: real cell, real store, real spine.
// ---------------------------------------------------------------------------

/// What the gate guarantees (and this test asserts): both ingests complete ok
/// (serialized, never torn), H ends coherently demoted — status=superseded with
/// NO residual active/current triple — and `mem:supersededBy` carries BOTH new
/// subjects (no lost demote), each new record carrying `mem:supersedes`=H.
///
/// What the gate deliberately does NOT decide: both new records end
/// `isCurrent=true`. Sequential double-supersession is LEGAL today (the planner
/// checks the demote target exists, not that it is current), so the fork is a
/// policy question (§8.1 contested-by-default), not a serialization bug. When
/// the lineage/contested slices land, the policy assertion belongs here.
#[test]
fn trace_concurrent_supersession_is_serialized_and_coherent() {
    let _serial = env_serial().lock().unwrap_or_else(|p| p.into_inner());
    let profile = temp_profile("gate");
    std::env::set_var("GARDEN_PROFILE_DIR", &profile);
    let result = std::panic::catch_unwind(run_concurrent_supersession_trace);
    std::env::remove_var("GARDEN_PROFILE_DIR");
    let _ = std::fs::remove_dir_all(&profile);
    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}

fn run_concurrent_supersession_trace() {
    let app = mock_app();
    let graph_id = "gate-lab";
    seed_graph(&app, graph_id);
    let contract = memory_core_vocabulary();
    let mem_ns = contract.primary_namespace();
    let status_p = format!("{mem_ns}status");
    let iscur_p = format!("{mem_ns}isCurrent");
    let supby_p = format!("{mem_ns}supersededBy");
    let sup_p = format!("{mem_ns}supersedes");

    println!("\n================ FLOW: CONCURRENT SUPERSESSION (WRITE GATE) ================");

    // ── STAGE 0: file head H through the spine ──
    let h = memory_record("r-h", "vera prefers fish CLI", None);
    let h_subject = memory_record_subject(graph_id, &h);
    assert!(
        file_through_spine(&app, graph_id, &h),
        "filing head H must succeed"
    );

    // ── STAGE 1: two supersessions of H race through the gated funnel ──
    let a = memory_record("r-a", "vera prefers zsh", Some(h_subject.clone()));
    let b = memory_record("r-b", "vera prefers nushell", Some(h_subject.clone()));
    let a_subject = memory_record_subject(graph_id, &a);
    let b_subject = memory_record_subject(graph_id, &b);
    assert_ne!(a_subject, b_subject, "distinct content, distinct subjects");
    println!("  racing A=<{a_subject}> and B=<{b_subject}> against H=<{h_subject}>");

    let (ra, rb) = crate::app_runtime::async_runtime::block_on(async {
        tokio::join!(
            crate::geist_memory_service::ingest_memory_record(&app, graph_id, a.clone()),
            crate::geist_memory_service::ingest_memory_record(&app, graph_id, b.clone()),
        )
    });
    let ra = ra.expect("ingest A ran");
    let rb = rb.expect("ingest B ran");
    assert_eq!(ra["ok"], json!(true), "A applied cleanly: {ra}");
    assert_eq!(rb["ok"], json!(true), "B applied cleanly: {rb}");

    // ── STAGE 2: H is coherently demoted — no torn lifecycle state ──
    let h_po = subject_po(&app, graph_id, &h_subject);
    print_subject("H (after both)", &app, graph_id, &h_subject);
    assert!(
        h_po.iter()
            .any(|(p, o)| p == &status_p && o == "\"superseded\""),
        "H demoted to status=superseded; got {h_po:?}"
    );
    assert!(
        !h_po
            .iter()
            .any(|(p, o)| p == &status_p && o == "\"active\""),
        "TORN STATE: H still carries status=active alongside superseded"
    );
    assert!(
        h_po.iter().any(|(p, o)| p == &iscur_p && o == FALSE_LIT),
        "H flipped isCurrent=false"
    );
    assert!(
        !h_po.iter().any(|(p, o)| p == &iscur_p && o == TRUE_LIT),
        "TORN STATE: H still carries isCurrent=true"
    );
    let superseders: Vec<&(String, String)> = h_po.iter().filter(|(p, _)| p == &supby_p).collect();
    assert_eq!(
        superseders.len(),
        2,
        "H supersededBy BOTH writers (no lost demote); got {superseders:?}"
    );

    // ── STAGE 3: both new records landed, each superseding H ──
    for (subject, name) in [(&a_subject, "A"), (&b_subject, "B")] {
        let po = subject_po(&app, graph_id, subject);
        assert!(
            po.iter()
                .any(|(p, o)| p == &sup_p && o == &format!("<{h_subject}>")),
            "{name} carries supersedes=H; got {po:?}"
        );
        assert!(
            po.iter().any(|(p, o)| p == &iscur_p && o == TRUE_LIT),
            "{name} is current (the §8.1-legal fork this stage documents)"
        );
    }

    // ── STAGE 4: §8.2 lineage makes the fork DETECTABLE ──
    // Both superseding records inherit H's lineage (H, a fresh assert, rooted
    // its lineage at itself), so ">1 current head per lineage" is now one
    // query — the S5 sweep's fork signature.
    let lineage_p = format!("{mem_ns}lineage");
    for (subject, name) in [(&a_subject, "A"), (&b_subject, "B")] {
        let po = subject_po(&app, graph_id, subject);
        assert!(
            po.iter()
                .any(|(p, o)| p == &lineage_p && o == &format!("<{h_subject}>")),
            "{name} inherits H's lineage; got {po:?}"
        );
    }
    let mem_graph = memory_projection_graph_iri(graph_id);
    let fork_heads: Vec<String> = select(
        &app,
        graph_id,
        &format!(
            "SELECT ?s WHERE {{ GRAPH <{mem_graph}> {{ ?s <{lineage_p}> <{h_subject}> ; \
             <{iscur_p}> true }} }} ORDER BY ?s"
        ),
    )
    .into_iter()
    .filter_map(|row| row.get("s").map(|s| strip_uri(s)))
    .collect();
    assert_eq!(
        fork_heads.len(),
        2,
        "the fork is DETECTABLE: two current heads in lineage H; got {fork_heads:?}"
    );
    assert!(fork_heads.contains(&a_subject) && fork_heads.contains(&b_subject));
    println!("  ✓ serialized: both applied, H demoted once-coherently, supersededBy×2,");
    println!("    lineage(A)=lineage(B)=H → the fork is one query away (§8.1 contested).");
}

// ---------------------------------------------------------------------------
// FLOW: createdAt CARRIES on re-file — a record WITHOUT observedAt stamps
// now() at first mint; the §8.2 carry-forward must replace the re-file's fresh
// stamp with the stored one so Case B converges to zero ops.
// ---------------------------------------------------------------------------

#[test]
fn trace_created_at_carries_on_refile() {
    let _serial = env_serial().lock().unwrap_or_else(|p| p.into_inner());
    let profile = temp_profile("created-carry");
    std::env::set_var("GARDEN_PROFILE_DIR", &profile);
    let result = std::panic::catch_unwind(run_created_at_carry_trace);
    std::env::remove_var("GARDEN_PROFILE_DIR");
    let _ = std::fs::remove_dir_all(&profile);
    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}

fn run_created_at_carry_trace() {
    let app = mock_app();
    let graph_id = "carry-lab";
    seed_graph(&app, graph_id);

    println!("\n================ FLOW: createdAt CARRY (observedAt-less re-file) ================");
    let mut r = memory_record("r-nc", "a fact filed without observedAt", None);
    r.observed_at = None;
    assert!(
        file_through_spine(&app, graph_id, &r),
        "first file (mints createdAt=now) must succeed"
    );

    // Re-plan the SAME record: before the carry-forward this churned exactly
    // one triple (createdAt now₂ replacing now₁) on every re-file, falsifying
    // the zero-op convergence the memory pack advertises.
    let replanned =
        gather_and_plan(&app, graph_id, &memory_request(&r)).expect("re-file gather_and_plan");
    assert_eq!(
        replanned.plan.summary.rdf_insert, 0,
        "observedAt-less re-file inserts nothing: {:?}",
        replanned.plan.summary
    );
    assert_eq!(
        replanned.plan.summary.rdf_delete, 0,
        "observedAt-less re-file deletes nothing: {:?}",
        replanned.plan.summary
    );
    println!("  ✓ re-file of an observedAt-less record is zero ops (createdAt carried).");
}

// ---------------------------------------------------------------------------
// FLOW: §8.1 CONFORMANCE SWEEP — sequential double-supersession is LEGAL and
// invisible at write time (the SHACL gate validates desired slices in a
// throwaway store); the post-hoc sweep finds the contested lineage over the
// LIVE graph and files it into the violation ledger as advisory testimony,
// content-addressed so a re-sweep converges instead of duplicating.
// ---------------------------------------------------------------------------

#[test]
fn trace_contested_lineage_sweep() {
    let _serial = env_serial().lock().unwrap_or_else(|p| p.into_inner());
    let profile = temp_profile("sweep");
    std::env::set_var("GARDEN_PROFILE_DIR", &profile);
    let result = std::panic::catch_unwind(run_contested_sweep_trace);
    std::env::remove_var("GARDEN_PROFILE_DIR");
    let _ = std::fs::remove_dir_all(&profile);
    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}

fn run_contested_sweep_trace() {
    use crate::emporium::sweep::sweep_memory_conformance;
    use crate::rdf_authority::violations_projection_graph_iri;

    let app = mock_app();
    let graph_id = "sweep-lab";
    seed_graph(&app, graph_id);

    println!("\n================ FLOW: §8.1 CONTESTED-LINEAGE SWEEP ================");

    // ── one clean supersession: H → A (single head, nothing contested) ──
    let h = memory_record("r-h", "the fact, first telling", None);
    let h_subject = memory_record_subject(graph_id, &h);
    assert!(file_through_spine(&app, graph_id, &h));
    let a = memory_record(
        "r-a",
        "the fact, revised by witness A",
        Some(h_subject.clone()),
    );
    let a_subject = memory_record_subject(graph_id, &a);
    assert!(file_through_spine(&app, graph_id, &a));

    let clean = sweep_memory_conformance(&app, graph_id, "").expect("clean sweep runs");
    assert!(
        clean.contested.is_empty(),
        "one head per lineage = nothing contested; got {:?}",
        clean.contested
    );
    assert_eq!(clean.ledgered, 0, "a clean sweep files no testimony");

    // ── the SEQUENTIAL fork: a second witness supersedes H again — legal,
    // accepted, and invisible to the write-time gate ──
    let b = memory_record(
        "r-b",
        "the fact, revised by witness B",
        Some(h_subject.clone()),
    );
    let b_subject = memory_record_subject(graph_id, &b);
    assert!(file_through_spine(&app, graph_id, &b));

    let report = sweep_memory_conformance(&app, graph_id, "").expect("sweep runs");
    assert_eq!(
        report.contested.len(),
        1,
        "the fork is one contested lineage; got {:?}",
        report.contested
    );
    let contested = &report.contested[0];
    assert_eq!(contested.lineage, h_subject, "the lineage is rooted at H");
    let mut expected_heads = vec![a_subject.clone(), b_subject.clone()];
    expected_heads.sort();
    assert_eq!(
        contested.heads, expected_heads,
        "both witnesses' heads are named"
    );
    assert_eq!(report.ledgered, 1, "one finding filed to the ledger");

    // ── the testimony is real, attributed born-RDF in :projection:violations ──
    let vlog = violations_projection_graph_iri(graph_id);
    let ledger_subjects = |app: &AppHandle| -> usize {
        select(
            app,
            graph_id,
            &format!("SELECT DISTINCT ?s WHERE {{ GRAPH <{vlog}> {{ ?s ?p ?o }} }}"),
        )
        .len()
    };
    let count_after_first = ledger_subjects(&app);
    assert!(
        count_after_first >= 1,
        "the contested finding landed in the ledger graph"
    );

    // ── idempotent re-sweep: same fork, same content-addressed subject(s) ──
    let again = sweep_memory_conformance(&app, graph_id, "").expect("re-sweep runs");
    assert_eq!(again.contested.len(), 1, "the fork is still contested");
    assert_eq!(
        ledger_subjects(&app),
        count_after_first,
        "re-sweep converges onto the same content-addressed ledger subjects"
    );
    println!("  ✓ fork detected post-hoc, filed as advisory testimony, re-sweep idempotent.");

    // ── the ratified read: return ALL heads flagged, under the declared
    // contested-by-default strategy (2026-07-05) ──
    use crate::emporium::sweep::current_heads_flagged;
    let heads = current_heads_flagged(&app, graph_id, "").expect("heads read");
    assert_eq!(
        heads.strategy, "contested",
        "the memory pack declares contested-by-default (conflict_policies): {}",
        heads.strategy
    );
    // The one forked lineage (H) is present, flagged, with BOTH heads returned —
    // storage never picked a winner.
    let forked = heads
        .lineages
        .iter()
        .find(|l| l.contested)
        .expect("the contested lineage is returned");
    assert_eq!(forked.lineage, h_subject, "lineage rooted at H");
    assert_eq!(forked.heads.len(), 2, "BOTH heads returned, not one");
    assert!(forked.heads.contains(&a_subject) && forked.heads.contains(&b_subject));
    println!("  ✓ return-all: strategy=contested, forked lineage returns both heads flagged.");
}

// ---------------------------------------------------------------------------
// FLOW: THE DISPOSABILITY ORACLE (§6) — projection(event log) == live store.
// Every accepted memory ingest appends its INTENT (records + the plan's clock)
// to the event log; replaying the log through the pure planner into a fresh
// in-memory store must reproduce the live :projection:memory EXACTLY. When
// this holds, the store is honestly a projection cache: the log can rebuild
// it from nothing. NO MOCKS: real spine writes, real replay, real set-equality.
// ---------------------------------------------------------------------------

#[test]
fn trace_memory_projection_equals_event_replay() {
    let _serial = env_serial().lock().unwrap_or_else(|p| p.into_inner());
    let profile = temp_profile("events");
    std::env::set_var("GARDEN_PROFILE_DIR", &profile);
    let result = std::panic::catch_unwind(run_event_replay_oracle_trace);
    std::env::remove_var("GARDEN_PROFILE_DIR");
    let _ = std::fs::remove_dir_all(&profile);
    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}

fn run_event_replay_oracle_trace() {
    use crate::emporium::memory_events::{project_memory_events, read_memory_events};
    use crate::emporium::survey::parse_term as survey_parse_term;

    let app = mock_app();
    let graph_id = "events-lab";
    seed_graph(&app, graph_id);

    println!("\n================ FLOW: DISPOSABILITY ORACLE (event replay) ================");

    // A history with every interesting shape: an observedAt-LESS head (the
    // wall-clock case the planned_at_ms capture exists for), a supersession, a
    // sequential fork, and a converged zero-op re-file.
    let mut h = memory_record("r-h", "the fact, first telling", None);
    h.observed_at = None;
    let h_subject = memory_record_subject(graph_id, &h);
    assert!(file_through_spine(&app, graph_id, &h));
    let a = memory_record("r-a", "the fact, per witness A", Some(h_subject.clone()));
    assert!(file_through_spine(&app, graph_id, &a));
    let b = memory_record("r-b", "the fact, per witness B", Some(h_subject.clone()));
    assert!(file_through_spine(&app, graph_id, &b));
    assert!(
        file_through_spine(&app, graph_id, &a),
        "converged re-file still applies ok"
    );

    // ── the log recorded every accepted intent, in order ──
    let events = read_memory_events(&app, graph_id).expect("read event log");
    assert_eq!(events.len(), 4, "four accepted ingests, four events");
    for (i, e) in events.iter().enumerate() {
        assert_eq!(e.seq, i as u64, "seq is dense and ordered");
        assert!(e.at_ms > 0, "each event carries the plan's clock");
    }

    // ── replay the log through the pure planner into a fresh store ──
    let projected = project_memory_events(graph_id, &events).expect("replay projects");
    let mem_graph = memory_projection_graph_iri(graph_id);
    let replayed = projected
        .get(&mem_graph)
        .expect("the commons projection graph was replayed");

    // ── THE ORACLE: canon-set equality with the live projection ──
    let live_set: std::collections::BTreeSet<(String, String, String)> =
        all_triples(&app, graph_id, &mem_graph)
            .into_iter()
            .map(|(s, p, o)| (strip_uri(&s), strip_uri(&p), survey_parse_term(&o).as_nt()))
            .collect();
    let replayed_set: std::collections::BTreeSet<(String, String, String)> = replayed
        .iter()
        .map(|(s, p, o)| (s.clone(), p.clone(), o.as_nt()))
        .collect();
    assert!(
        !live_set.is_empty(),
        "the live projection is non-trivial (H, A, B + provenance)"
    );
    let only_live: Vec<_> = live_set.difference(&replayed_set).collect();
    let only_replay: Vec<_> = replayed_set.difference(&live_set).collect();
    assert!(
        only_live.is_empty() && only_replay.is_empty(),
        "DISPOSABILITY ORACLE FAILED\n  live-only ({}): {:?}\n  replay-only ({}): {:?}",
        only_live.len(),
        only_live,
        only_replay.len(),
        only_replay,
    );
    println!(
        "  ✓ projection(event log) == live store ({} triples, set-equal) — the",
        live_set.len()
    );
    println!("    :projection:memory graph is honestly a CACHE of the log.");
}

// ---------------------------------------------------------------------------
// FLOW: GENERIC MO CRUD — the object surface over a product vocab (bookmark):
// create → list (paginated) → read → update (with address integrity) → delete
// (journaled retraction; lifecycle vocabs refused with the supersession
// instruction). NO MOCKS: real store, real spine underneath every mutation.
// ---------------------------------------------------------------------------

#[test]
fn trace_generic_object_crud_lifecycle() {
    let _serial = env_serial().lock().unwrap_or_else(|p| p.into_inner());
    let profile = temp_profile("crud");
    std::env::set_var("GARDEN_PROFILE_DIR", &profile);
    let result = std::panic::catch_unwind(run_object_crud_trace);
    std::env::remove_var("GARDEN_PROFILE_DIR");
    let _ = std::fs::remove_dir_all(&profile);
    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}

fn run_object_crud_trace() {
    use crate::emporium::applied_journal::read_applied_records;
    use crate::emporium::objects::{
        create_objects, delete_object, list_objects, read_object, update_object, ObjectError,
    };

    let app = mock_app();
    let graph_id = "crud-lab";
    seed_graph(&app, graph_id);

    println!("\n================ FLOW: GENERIC MO CRUD (bookmark) ================");

    // ── CREATE two ──
    let created = crate::app_runtime::async_runtime::block_on(create_objects(
        &app,
        graph_id,
        "emporium-bookmark",
        json!([
            {"kind": "Bookmark", "localId": "rust-book",
             "url": "https://doc.rust-lang.org/book/", "title": "The Rust Book",
             "note": "the canonical intro"},
            {"kind": "Bookmark", "localId": "sparql-spec",
             "url": "https://www.w3.org/TR/sparql11-query/", "title": "SPARQL 1.1"}
        ]),
    ))
    .expect("create ok");
    assert_eq!(created["ok"], json!(true));
    assert_eq!(created["subjects"].as_array().map(Vec::len), Some(2));

    // ── LIST with pagination ──
    let page =
        list_objects(&app, graph_id, "emporium-bookmark", "Bookmark", 1, 1).expect("list ok");
    assert_eq!(page["subjects"].as_array().map(Vec::len), Some(1), "{page}");
    let all =
        list_objects(&app, graph_id, "emporium-bookmark", "Bookmark", 100, 0).expect("list ok");
    assert_eq!(all["subjects"].as_array().map(Vec::len), Some(2));

    // ── READ by localId ──
    let obj =
        read_object(&app, graph_id, "emporium-bookmark", "Bookmark", "rust-book").expect("read ok");
    let subject = obj["subject"].as_str().expect("subject").to_string();
    assert!(subject.ends_with("rust-book"));
    assert!(
        obj["predicates"]
            .as_object()
            .expect("predicates map")
            .iter()
            .any(|(p, v)| p.ends_with("title")
                && v.as_array().is_some_and(|a| a
                    .iter()
                    .any(|o| o.as_str().is_some_and(|s| s.contains("The Rust Book"))))),
        "title present: {obj}"
    );

    // ── UPDATE with address integrity ──
    let updated = crate::app_runtime::async_runtime::block_on(update_object(
        &app,
        graph_id,
        "emporium-bookmark",
        "Bookmark",
        "rust-book",
        json!({"kind": "Bookmark", "localId": "rust-book",
               "url": "https://doc.rust-lang.org/book/", "title": "The Rust Book, 2nd ed."}),
    ))
    .expect("update ok");
    assert_eq!(updated["ok"], json!(true));
    let obj = read_object(&app, graph_id, "emporium-bookmark", "Bookmark", "rust-book")
        .expect("re-read ok");
    let rendered = obj.to_string();
    assert!(rendered.contains("2nd ed."), "title updated: {obj}");
    assert!(
        !rendered.contains("canonical intro"),
        "dropped note reclaimed within the subject: {obj}"
    );

    // Address-integrity violation: a record minting a DIFFERENT subject → 400.
    let mismatch = crate::app_runtime::async_runtime::block_on(update_object(
        &app,
        graph_id,
        "emporium-bookmark",
        "Bookmark",
        "rust-book",
        json!({"kind": "Bookmark", "localId": "some-other-book",
               "url": "https://example.test/", "title": "Wrong door"}),
    ));
    assert!(
        matches!(mismatch, Err(ObjectError::BadRequest(_))),
        "mismatched record is a 400: {mismatch:?}"
    );

    // ── class-integrity: an unknown full-IRI subject is 404, not a cross-class
    // touch (codex close-out finding — full-IRI addresses bypass localId minting) ──
    let sparql_subject = read_object(
        &app,
        graph_id,
        "emporium-bookmark",
        "Bookmark",
        "sparql-spec",
    )
    .expect("read sibling")["subject"]
        .as_str()
        .expect("subject")
        .to_string();
    let wrong = read_object(
        &app,
        graph_id,
        "emporium-bookmark",
        "Bookmark",
        &format!("{sparql_subject}-not-a-real-subject"),
    );
    assert!(
        matches!(wrong, Err(ObjectError::Absent(_))),
        "an unknown full-IRI subject reads 404: {wrong:?}"
    );

    // ── DELETE is refused for lifecycle vocabs (supersede instead) ──
    let refused = crate::app_runtime::async_runtime::block_on(delete_object(
        &app,
        graph_id,
        "sophia-memory-core",
        "MemoryRecord",
        "anything",
    ));
    assert!(
        matches!(refused, Err(ObjectError::Conflict(_))),
        "lifecycle vocab DELETE is a 409: {refused:?}"
    );

    // ── DELETE a product object: journaled retraction, sibling intact ──
    let journal_before = read_applied_records(&app, graph_id)
        .expect("read journal")
        .len();
    let deleted = crate::app_runtime::async_runtime::block_on(delete_object(
        &app,
        graph_id,
        "emporium-bookmark",
        "Bookmark",
        "rust-book",
    ))
    .expect("delete ok");
    assert_eq!(deleted["ok"], json!(true));
    assert!(deleted["retractedTriples"].as_i64().unwrap_or(0) > 0);
    let gone = read_object(&app, graph_id, "emporium-bookmark", "Bookmark", "rust-book");
    assert!(
        matches!(gone, Err(ObjectError::Absent(_))),
        "deleted object reads 404: {gone:?}"
    );
    let sibling = read_object(
        &app,
        graph_id,
        "emporium-bookmark",
        "Bookmark",
        "sparql-spec",
    );
    assert!(sibling.is_ok(), "sibling survives the retraction");
    let journal_after = read_applied_records(&app, graph_id)
        .expect("read journal")
        .len();
    assert_eq!(
        journal_after,
        journal_before + 1,
        "the retraction is journaled"
    );
    println!("  ✓ create/list/read/update/delete over the real spine; lifecycle vocabs 409;");
    println!("    retraction journaled; siblings untouched.");
}

// ---------------------------------------------------------------------------
// FLOW: SUBJECT-SCOPED UPSERT vs replace_class — the generic ingest's apply
// semantics. Default = a partial batch touches ONLY its own subjects (sibling
// records of the same class survive); whole-class replace is the explicit,
// destructive opt-in. NO MOCKS: real cell, real store, real spine.
// ---------------------------------------------------------------------------

/// Drive one bookmark ingest with an explicit `replace_class` flag.
fn ingest_bookmarks_with(
    app: &AppHandle,
    graph_id: &str,
    records: Value,
    replace_class: bool,
) -> crate::emporium::applier::ApplyReport {
    let body = json!({
        "vocab": "emporium-bookmark",
        "dry_run": false,
        "replace_class": replace_class,
        "payload": { "kind": "generic", "records": records }
    });
    let request: IngestRequest =
        serde_json::from_value(body).expect("bookmark ingest request deserializes");
    let planned = gather_and_plan(app, graph_id, &request).expect("bookmark gather_and_plan");
    crate::app_runtime::async_runtime::block_on(apply_and_assert(
        app,
        graph_id,
        &planned.plan,
        planned.contract,
        &request,
    ))
}

#[test]
fn trace_partial_batch_preserves_siblings() {
    let _serial = env_serial().lock().unwrap_or_else(|p| p.into_inner());
    let profile = temp_profile("partial-batch");
    std::env::set_var("GARDEN_PROFILE_DIR", &profile);
    let result = std::panic::catch_unwind(run_partial_batch_trace);
    std::env::remove_var("GARDEN_PROFILE_DIR");
    let _ = std::fs::remove_dir_all(&profile);
    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}

fn run_partial_batch_trace() {
    let app = mock_app();
    let graph_id = "partial-lab";
    seed_graph(&app, graph_id);
    let sink = bookmark_sink(graph_id);

    println!("\n================ FLOW: SUBJECT-SCOPED UPSERT vs replace_class ================");

    // ── STAGE 0: publish two bookmarks (full batch) ──
    let ab = json!([
        {"kind": "Bookmark", "localId": "rust-book",
         "url": "https://doc.rust-lang.org/book/", "title": "The Rust Book",
         "note": "the canonical intro"},
        {"kind": "Bookmark", "localId": "sparql-spec",
         "url": "https://www.w3.org/TR/sparql11-query/", "title": "SPARQL 1.1"}
    ]);
    assert!(ingest_bookmarks(&app, graph_id, ab).ok, "seed ingest ok");
    assert_eq!(bookmark_count(&app, graph_id), 2);
    let b_before: Vec<_> = all_triples(&app, graph_id, &sink)
        .into_iter()
        .filter(|(s, _, _)| s.contains("sparql-spec"))
        .collect();

    // ── STAGE 1: a PARTIAL batch (one NEW bookmark) — siblings must survive ──
    // Before subject-scoped upsert this exact call silently DELETED rust-book
    // and sparql-spec (whole-class replace was the implicit default) and still
    // returned ok:true. That was the appraisal's #1 critical.
    let c = json!([
        {"kind": "Bookmark", "localId": "nushell-book",
         "url": "https://www.nushell.sh/book/", "title": "The Nushell Book"}
    ]);
    let r = ingest_bookmarks(&app, graph_id, c.clone());
    assert!(r.ok, "partial ingest ok");
    let (_, del) = report_delta(&r);
    assert_eq!(del, 0, "a partial batch of a NEW subject deletes NOTHING");
    assert_eq!(
        bookmark_count(&app, graph_id),
        3,
        "SIBLING SURVIVAL: the partial batch added C without deleting A/B"
    );

    // ── STAGE 2: partial UPDATE of one subject — its own stale props reclaim,
    // siblings byte-untouched ──
    let a2 = json!([
        {"kind": "Bookmark", "localId": "rust-book",
         "url": "https://doc.rust-lang.org/book/", "title": "The Rust Book, 2nd ed."}
    ]);
    let r = ingest_bookmarks(&app, graph_id, a2);
    assert!(r.ok, "partial update ok");
    let a_now: Vec<_> = all_triples(&app, graph_id, &sink)
        .into_iter()
        .filter(|(s, _, _)| s.contains("rust-book"))
        .collect();
    assert!(
        a_now.iter().any(|(_, _, o)| o.contains("2nd ed.")),
        "rust-book title updated in place"
    );
    assert!(
        !a_now.iter().any(|(_, _, o)| o.contains("canonical intro")),
        "rust-book's dropped note is reclaimed (stale prop within the subject)"
    );
    let b_after: Vec<_> = all_triples(&app, graph_id, &sink)
        .into_iter()
        .filter(|(s, _, _)| s.contains("sparql-spec"))
        .collect();
    assert_eq!(b_before, b_after, "untouched sibling is byte-identical");
    assert_eq!(bookmark_count(&app, graph_id), 3);

    // ── STAGE 3: converged partial re-ingest = zero ops ──
    let r = ingest_bookmarks(&app, graph_id, c.clone());
    let (ins, del) = report_delta(&r);
    assert_eq!(
        (ins, del),
        (0, 0),
        "converged partial re-ingest is zero ops"
    );

    // ── STAGE 4: replace_class=true is the EXPLICIT destructive form ──
    let r = ingest_bookmarks_with(&app, graph_id, c, true);
    assert!(r.ok, "replace_class ingest ok");
    assert_eq!(
        bookmark_count(&app, graph_id),
        1,
        "replace_class: the batch IS the class — absent siblings reclaimed"
    );
    println!("  ✓ subject-scoped default preserves siblings; replace_class is opt-in destruction.");
}

// ---------------------------------------------------------------------------
// FLOW: CHAMBER SUPERSESSION HISTORY SURVIVES v3 — before subject-scoped
// upsert, propose v3's batch [v3, demoted-v2] whole-class-replaced the
// chm:DomainOntology span and silently DELETED the v1 record (history
// self-destructed at the third version).
// ---------------------------------------------------------------------------

#[test]
fn trace_chamber_history_survives_third_version() {
    let _serial = env_serial().lock().unwrap_or_else(|p| p.into_inner());
    let profile = temp_profile("chamber-history");
    std::env::set_var("GARDEN_PROFILE_DIR", &profile);
    let result = std::panic::catch_unwind(run_chamber_history_trace);
    std::env::remove_var("GARDEN_PROFILE_DIR");
    let _ = std::fs::remove_dir_all(&profile);
    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}

fn run_chamber_history_trace() {
    let app = mock_app();
    let graph_id = "chamber-history-lab";
    seed_graph(&app, graph_id);
    let chamber = chamber_projection_graph_iri(graph_id);

    println!("\n================ FLOW: CHAMBER HISTORY SURVIVES v3 ================");

    let subject_of = |res: &Value| {
        res.get("ontologySubject")
            .and_then(Value::as_str)
            .expect("ontology subject")
            .to_string()
    };
    let v1 = subject_of(&propose(&app, graph_id, bench_contract_v1(), "v1").expect("propose v1"));
    let v2 = subject_of(&propose(&app, graph_id, bench_contract_v2(), "v2").expect("propose v2"));
    let mut contract_v3 = bench_contract_v2();
    contract_v3["version"] = json!("2.1.0");
    contract_v3["description"] = json!("refined again: the third generation");
    let v3 = subject_of(&propose(&app, graph_id, contract_v3, "v3").expect("propose v3"));
    assert_ne!(v1, v2);
    assert_ne!(v2, v3);

    // All THREE generations still exist as born-RDF records.
    let subjects: Vec<String> = select(
        &app,
        graph_id,
        &format!(
            "SELECT ?s WHERE {{ GRAPH <{chamber}> {{ ?s a <{CHM_NS}DomainOntology> }} }} ORDER BY ?s"
        ),
    )
    .into_iter()
    .filter_map(|row| row.get("s").map(|s| strip_uri(s)))
    .collect();
    assert_eq!(
        subjects.len(),
        3,
        "HISTORY SURVIVES: v1, v2, v3 all present (v1 was silently deleted here \
         before subject-scoped upsert); got {subjects:?}"
    );
    for v in [&v1, &v2, &v3] {
        assert!(subjects.contains(v), "missing generation {v}");
    }

    // Lifecycle chain: v1 and v2 superseded (with forward links), v3 active.
    let status_of = |subject: &str| -> Vec<(String, String)> {
        select(
            &app,
            graph_id,
            &format!("SELECT ?p ?o WHERE {{ GRAPH <{chamber}> {{ <{subject}> ?p ?o }} }}"),
        )
        .into_iter()
        .map(|r| {
            (
                strip_uri(&r.get("p").cloned().unwrap_or_default()),
                r.get("o").cloned().unwrap_or_default(),
            )
        })
        .collect()
    };
    let status_p = format!("{CHM_NS}status");
    let supby_p = format!("{CHM_NS}supersededBy");
    let v1_po = status_of(&v1);
    assert!(
        v1_po
            .iter()
            .any(|(p, o)| p == &status_p && o == "\"superseded\""),
        "v1 superseded; got {v1_po:?}"
    );
    assert!(
        v1_po
            .iter()
            .any(|(p, o)| p == &supby_p && o == &format!("<{v2}>")),
        "v1 supersededBy v2"
    );
    let v2_po = status_of(&v2);
    assert!(
        v2_po
            .iter()
            .any(|(p, o)| p == &status_p && o == "\"superseded\""),
        "v2 superseded; got {v2_po:?}"
    );
    assert!(
        v2_po
            .iter()
            .any(|(p, o)| p == &supby_p && o == &format!("<{v3}>")),
        "v2 supersededBy v3"
    );
    let v3_po = status_of(&v3);
    assert!(
        v3_po
            .iter()
            .any(|(p, o)| p == &status_p && o == "\"active\""),
        "v3 is the active generation; got {v3_po:?}"
    );
    println!("  ✓ three generations retained; v1→v2→v3 chain intact; v3 active.");
}

// ---------------------------------------------------------------------------
// FLOW: CHAMBER IMMUTABLE PUBLISHED VERSIONS (S6) — because the ontology subject is
// a versioned LOGICAL id ({name}-v{version}, identity_kind=logical-id), re-proposing
// the SAME (name, version) with DIFFERENT content would silently overwrite published
// testimony. That is now REJECTED loud; identical-content re-propose stays idempotent.
// NO MOCKS: real cell, real store, real propose_domain_ontology spine.
// ---------------------------------------------------------------------------

#[test]
fn trace_chamber_published_version_is_immutable() {
    let _serial = env_serial().lock().unwrap_or_else(|p| p.into_inner());
    let profile = temp_profile("chamber-immutable");
    std::env::set_var("GARDEN_PROFILE_DIR", &profile);
    let result = std::panic::catch_unwind(run_chamber_immutability_trace);
    std::env::remove_var("GARDEN_PROFILE_DIR");
    let _ = std::fs::remove_dir_all(&profile);
    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}

fn run_chamber_immutability_trace() {
    let app = mock_app();
    let graph_id = "chamber-immutable-lab";
    seed_graph(&app, graph_id);
    let chamber = chamber_projection_graph_iri(graph_id);

    println!("\n================ FLOW: CHAMBER PUBLISHED VERSION IS IMMUTABLE ================");

    // Publish v1.
    let v1 = propose(&app, graph_id, bench_contract_v1(), "v1").expect("propose v1");
    let v1_subject = v1
        .get("ontologySubject")
        .and_then(Value::as_str)
        .expect("v1 subject")
        .to_string();

    // Re-propose the IDENTICAL (name, version) content → idempotent OK, same subject,
    // and the store still holds exactly ONE record at that subject.
    let v1_again =
        propose(&app, graph_id, bench_contract_v1(), "v1-again").expect("identical re-propose OK");
    assert_eq!(
        v1_again.get("ontologySubject").and_then(Value::as_str),
        Some(v1_subject.as_str()),
        "identical re-propose converges to the same subject"
    );

    // Re-propose the SAME (name, version) with DIFFERENT content → LOUD REJECT, no write.
    let mut mutated_v1 = bench_contract_v1();
    mutated_v1["description"] = json!("SNEAKY in-place rewrite of a published version");
    mutated_v1["classes"]["Trial"]["predicates"]["bench:sneak"] =
        json!({"datatype": "string", "required": false, "multi": false});
    let err = propose(&app, graph_id, mutated_v1, "v1-mutated")
        .expect_err("same version + different content MUST be rejected");
    let msg = err.message_ref().to_string();
    assert!(
        msg.contains("IMMUTABLE") && msg.contains("1.0.0"),
        "the reject names the immutability wall + version; got: {msg}"
    );

    // The published v1 content is UNTOUCHED — the sneaky predicate never landed.
    let stored_json: Vec<String> = select(
        &app,
        graph_id,
        &format!(
            "SELECT ?j WHERE {{ GRAPH <{chamber}> {{ <{v1_subject}> <{CHM_NS}contractJson> ?j }} }}"
        ),
    )
    .into_iter()
    .filter_map(|row| row.get("j").cloned())
    .collect();
    assert_eq!(
        stored_json.len(),
        1,
        "exactly one contractJson at the subject"
    );
    assert!(
        !stored_json[0].contains("bench:sneak"),
        "REJECTED write left the published version byte-untouched; got: {}",
        stored_json[0]
    );
    println!("  ✓ identical re-propose idempotent; mutated same-version rejected; v1 untouched.");
}

// ---------------------------------------------------------------------------
// FLOW: RECOVERABILITY (S3) — the born-RDF :projection:memory sink is rebuildable
// from the applied-plan journal. File H then supersede with H'; snapshot the
// projection; CLEAR + replay the journal; assert the projection is SET-EQUAL to the
// snapshot (the store was disposable ONLY because the journal exists). NO MOCKS:
// real cell, real store, real spine, real journal on disk.
// ---------------------------------------------------------------------------

#[test]
fn trace_memory_rebuild_from_journal() {
    let _serial = env_serial().lock().unwrap_or_else(|p| p.into_inner());
    let profile = temp_profile("rebuild");
    std::env::set_var("GARDEN_PROFILE_DIR", &profile);
    let result = std::panic::catch_unwind(run_memory_rebuild_trace);
    std::env::remove_var("GARDEN_PROFILE_DIR");
    let _ = std::fs::remove_dir_all(&profile);
    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}

fn run_memory_rebuild_trace() {
    use crate::emporium::applied_journal::{
        read_applied_records, rebuild_memory_projection_from_journal,
    };
    use std::collections::BTreeSet;

    let app = mock_app();
    let graph_id = "rebuild-lab";
    seed_graph(&app, graph_id);
    let mem_graph = memory_projection_graph_iri(graph_id);
    let mem_ns = memory_core_vocabulary().primary_namespace();
    let status_p = format!("{mem_ns}status");

    println!("\n================ FLOW: MEMORY REBUILD FROM APPLIED JOURNAL ================");
    println!("  projection:memory graph=<{mem_graph}>");

    // ── STAGE 0: file head H, then supersede with H', THROUGH the real spine ──
    let h = memory_record("r-0", "vera prefers fish CLI", None);
    let h_subject = memory_record_subject(graph_id, &h);
    assert!(
        file_through_spine(&app, graph_id, &h),
        "filing H must succeed"
    );
    let hp = memory_record("r-1", "vera prefers zsh", Some(h_subject.clone()));
    let hp_subject = memory_record_subject(graph_id, &hp);
    assert!(
        file_through_spine(&app, graph_id, &hp),
        "filing H' must succeed"
    );

    // ── STAGE 1: the applied-plan journal recorded EXACTLY the two applies ──
    let records = read_applied_records(&app, graph_id).expect("read applied journal");
    let memory_applies: Vec<_> = records.iter().filter(|r| r.mode == "memory").collect();
    assert_eq!(
        memory_applies.len(),
        2,
        "journal recorded 2 memory applies (H, H'); got {memory_applies:#?}"
    );
    assert!(
        memory_applies[0].seq < memory_applies[1].seq,
        "applies are seq-ordered (unambiguous replay order)"
    );
    for r in &memory_applies {
        assert_eq!(
            r.vocab, "sophia-memory-core",
            "record carries the pack name"
        );
        assert_eq!(
            r.contract.name, "sophia-memory-core",
            "contract provenance carried"
        );
        assert!(
            r.plan.get("steps").and_then(Value::as_array).is_some(),
            "the full serialized plan is journaled"
        );
    }
    println!(
        "  journal has {} memory applies (seq {} then {})",
        memory_applies.len(),
        memory_applies[0].seq,
        memory_applies[1].seq
    );

    // ── STAGE 2: snapshot every :projection:memory triple BEFORE the rebuild ──
    let before: BTreeSet<(String, String, String)> = all_triples(&app, graph_id, &mem_graph)
        .into_iter()
        .collect();
    assert!(
        !before.is_empty(),
        "the memory projection has content before rebuild"
    );
    println!("  pre-rebuild <{mem_graph}> has {} triples", before.len());

    // ── STAGE 3: REBUILD — CLEAR the sink + replay the journaled plans in order ──
    let report =
        rebuild_memory_projection_from_journal(&app, graph_id).expect("rebuild from journal");
    assert_eq!(report.replayed, 2, "replayed both journaled memory plans");
    assert_eq!(report.sink, mem_graph, "rebuilt the commons memory sink");
    println!(
        "  rebuild replayed {} plans into <{}>",
        report.replayed, report.sink
    );

    // ── STAGE 4: the projection is SET-EQUAL to the pre-rebuild snapshot ──
    let after: BTreeSet<(String, String, String)> = all_triples(&app, graph_id, &mem_graph)
        .into_iter()
        .collect();
    assert_eq!(
        before, after,
        "post-rebuild :projection:memory is SET-EQUAL to the snapshot (replay is faithful)"
    );

    // The supersession lifecycle survived the CLEAR + replay round-trip.
    let h_po = subject_po(&app, graph_id, &h_subject);
    assert!(
        h_po.iter()
            .any(|(p, o)| p == &status_p && o == "\"superseded\""),
        "H stays superseded after rebuild; got {h_po:?}"
    );
    let hp_po = subject_po(&app, graph_id, &hp_subject);
    assert!(
        hp_po
            .iter()
            .any(|(p, o)| p == &status_p && o == "\"active\""),
        "H' stays active after rebuild; got {hp_po:?}"
    );
    println!("  ✓ rebuild is set-faithful; supersession lifecycle intact.");
}

// ---------------------------------------------------------------------------
// FLOW: SHACL HALT IS JOURNALED (§3.3 gap fix) — the ValidationGate::Halt arm was
// the one halt that did NOT journal its failure. Drive a memory apply whose desired
// set violates a golden SHACL constraint under the default Halt policy, and assert a
// failure-journal file appears (and NO success record). NO MOCKS: real store, real
// SHACL gate, real journal on disk.
// ---------------------------------------------------------------------------

#[test]
fn trace_memory_shacl_halt_is_journaled() {
    let _serial = env_serial().lock().unwrap_or_else(|p| p.into_inner());
    let profile = temp_profile("halt-journal");
    std::env::set_var("GARDEN_PROFILE_DIR", &profile);
    let result = std::panic::catch_unwind(run_halt_journal_trace);
    std::env::remove_var("GARDEN_PROFILE_DIR");
    let _ = std::fs::remove_dir_all(&profile);
    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}

fn run_halt_journal_trace() {
    use crate::emporium::terms::Term;
    use oxigraph::model::NamedNode;

    let app = mock_app();
    let graph_id = "halt-journal-lab";
    seed_graph(&app, graph_id); // a freshly-seeded graph defaults to ValidationPolicy::Halt

    println!("\n================ FLOW: SHACL HALT IS JOURNALED (§3.3 gap) ================");

    // A VALID record → a real memory plan (observer empty ⇒ the memory-core contract).
    let record = memory_record("r-halt", "vera prefers fish CLI", None);
    let request = memory_request(&record);
    let mut planned = gather_and_plan(&app, graph_id, &request).expect("gather_and_plan");
    assert_eq!(
        planned.plan.mode, "memory",
        "a memory plan (routes to the sink)"
    );

    // Inject a SHACL-VIOLATING desired-insert: a mem:SourceReference subject MISSING
    // its required mem:sourceKind (sh:minCount 1 — the shacl_validator teeth-check).
    // WHY at the plan's desired_inserts and not a malformed record: the planner is
    // conformant-by-construction and the request gate + planner I1–I4 guards reject
    // record-level faults FIRST, so a blocking SHACL Violation is genuinely
    // unreachable from a well-formed record through the fully public path. The
    // honest seam is desired_inserts — the EXACT triple set the gate validates.
    let mem = memory_core_vocabulary().primary_namespace();
    let rdf_type = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type".to_string();
    let bad = format!("urn:mnemosyne:local:graph:{graph_id}:projection:memory:src:halt-bad");
    let uri = |s: &str| Term::Uri(NamedNode::new(s).expect("valid IRI"));
    planned.plan.desired_inserts.push((
        bad.clone(),
        rdf_type.clone(),
        uri(&format!("{mem}SourceReference")),
    ));
    planned.plan.desired_inserts.push((
        bad.clone(),
        rdf_type,
        uri("http://www.w3.org/ns/prov#Entity"),
    ));
    // deliberately NO mem:sourceKind → sh:minCount 1 violation (severity Violation).

    // Drive the REAL apply path: memory sink → SHACL gate → Halt (default policy).
    let report = crate::app_runtime::async_runtime::block_on(apply_and_assert(
        &app,
        graph_id,
        &planned.plan,
        memory_core_vocabulary(),
        &request,
    ));
    assert!(!report.ok, "the SHACL Halt rejects the write (ok=false)");
    assert!(
        report.steps.iter().any(|s| s.op == "memory_validate"),
        "the report carries the validation halt: {report:#?}"
    );

    // §3.3 FIX: the Halt arm now JOURNALS the failure like every other halt.
    let graph_dir = crate::graph_paths::existing_graph_dir(&app, graph_id).expect("graph dir");
    let failures_dir = graph_dir.join("memory").join("failures");
    let failure_files: Vec<_> = std::fs::read_dir(&failures_dir)
        .map(|rd| {
            rd.flatten()
                .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("json"))
                .collect()
        })
        .unwrap_or_default();
    assert_eq!(
        failure_files.len(),
        1,
        "the SHACL Halt wrote exactly one failure-journal file; found {}",
        failure_files.len()
    );

    // The journaled failure carries the SHACL violation (halt string) + haltedAt=0.
    let body = std::fs::read_to_string(failure_files[0].path()).expect("read failure record");
    let v: Value = serde_json::from_str(&body).expect("parse failure record");
    assert!(
        v["error"].as_str().unwrap_or("").contains("SHACL"),
        "failure record carries the SHACL violation; error={:?}",
        v["error"]
    );
    assert_eq!(
        v["haltedAt"],
        json!(0),
        "halted at the gate, before any step"
    );

    // A HALTED write leaves NO applied-plan (success) journal — nothing landed.
    let applied_dir = graph_dir.join("emporium").join("applied");
    let applied_count = std::fs::read_dir(&applied_dir)
        .map(|rd| rd.flatten().count())
        .unwrap_or(0);
    assert_eq!(
        applied_count, 0,
        "a rejected write writes no success record (only the failure lane)"
    );

    println!("  ✓ SHACL Halt journaled to memory/failures/ (1 file); no success record.");
}

// ===========================================================================
// The MCP `emporium_write` APPLY handler (`source_sync::mcp_source_emporium_write`)
// over a CHAMBER-proposed vocabulary. Until 2026-09-15 it previewed the batch
// clean through the chamber-aware emporium lane and then refused the apply with
// "unknown embedded vocabulary '<name>'": the source authority it builds
// operations for knows only the embedded catalogue. Observed on the canary cell
// (2026-09-04 watch log) and on Sirin (cuentas, 2026-09-15). This is the
// failing-direction witness: on the old handler it fails with exactly that
// message; with the chamber branch it passes and the instance is materialized.
// ===========================================================================

#[test]
fn mcp_emporium_write_applies_a_chamber_proposed_vocabulary() {
    // Its own profile under the process-wide serial lock, like every other
    // case here: without it this case read whatever GARDEN_PROFILE_DIR a
    // concurrent case had set (box judge of 7d50d8b: "open omphalos store
    // .../garden-marks-order-flush-.../omphalos: lock hold by current process").
    let _serial = env_serial().lock().unwrap_or_else(|p| p.into_inner());
    let profile = temp_profile("chamber-write-lane");
    std::env::set_var("GARDEN_PROFILE_DIR", &profile);
    let result = std::panic::catch_unwind(run_chamber_write_lane);
    std::env::remove_var("GARDEN_PROFILE_DIR");
    let _ = std::fs::remove_dir_all(&profile);
    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}

fn run_chamber_write_lane() {
    let app = mock_app();
    let graph_id = "chamber-write-lane";
    seed_graph(&app, graph_id);
    propose(&app, graph_id, bench_contract_v1(), "chamber write-lane witness")
        .expect("propose bench-domain v1");
    assert_eq!(active_ontology_count(&app, graph_id, "bench-domain"), 1);
    assert_eq!(trial_count(&app, graph_id), 0, "no Trial before the write");

    let args = json!({
        "graphId": graph_id,
        "vocab": "bench-domain",
        "records": [
            { "kind": "Trial", "localId": "t-lane-1", "bench:label": "applied through the lane" }
        ],
    });
    let applied = crate::app_runtime::async_runtime::block_on(
        crate::source_sync::mcp_source_emporium_write(app.clone(), &args),
    )
    .expect("a chamber vocabulary that previews clean must also apply");

    assert_eq!(applied["ok"], json!(true), "apply ok: {applied}");
    assert_eq!(applied["dryRun"], json!(false), "this was the apply, not the preview");
    assert_eq!(
        applied["sourceLedgered"],
        json!(false),
        "chamber writes say on the wire that they are not source-ledgered"
    );
    let outcomes = applied["results"].as_array().expect("results array");
    assert_eq!(outcomes.len(), 1);
    assert_eq!(outcomes[0]["outcome"], json!("applied"));
    assert_eq!(trial_count(&app, graph_id), 1, "the Trial landed in its projection sink");

    // Idempotent re-apply of the same record: still ok, still one Trial.
    let again = crate::app_runtime::async_runtime::block_on(
        crate::source_sync::mcp_source_emporium_write(app.clone(), &args),
    )
    .expect("re-apply");
    assert_eq!(again["ok"], json!(true));
    assert_eq!(trial_count(&app, graph_id), 1, "re-apply converges, never duplicates");
}
