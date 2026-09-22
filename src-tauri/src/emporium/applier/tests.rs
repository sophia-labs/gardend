//! Loopback integration tests for the CRDT applier (headless feature only).
//!
//! These drive the REAL gardend cell: a MockRuntime app with the CRDT queue +
//! room registry managed, a seeded graph, and the spine producing real plans.
//! The applier then maps each plan verb to the in-process CRDT surface
//! (`enqueue_crdt_operation` → the headless executor → the Rust CRDT engine) and
//! folds RDF + emp: provenance into the user:rdf graph.
//!
//!   1. `apply_round_trips_and_loud_halts` — apply a plan with create_folder +
//!      write_doc (a ```javascript code block) + move + create_wires on a seeded
//!      graph. Assert: ok=true; read_document(markdown) round-trips the workflow
//!      doc; a captured wf:scriptBlock URI of the form
//!      `{prefix}:doc:{docId}#block-XXXX`; the move of a just-written doc is a
//!      recorded NO-OP (placedAtCreation); the user:rdf graph holds the RDF the
//!      sparql_update steps wrote (read==write). Then a SECOND plan with a doomed
//!      sparql_update step loud-halts with ok=false + haltedAt naming the step.
//!
//!   2. `block_id_stability_two_apply` — apply the demo plan, capture
//!      wf:scriptBlock, then re-plan (gather_and_plan) the SAME def and assert the
//!      re-plan is converged (write_doc dropped via `doc_converged`), re-apply,
//!      and the wf:scriptBlock is BYTE-IDENTICAL across both applies.

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use crate::app_runtime::AppHandle;
use crate::emporium::applier::apply_plan;
use crate::emporium::contract::workflow_vocabulary;
use crate::emporium::planner::{Plan, Step};
use crate::emporium::schemas::IngestRequest;
use crate::emporium::spine::gather_and_plan;
use crate::emporium::terms::sha256_text;
use crate::graph_service::{create_graph_service, CreateGraphInput};
use crate::rdf::graph_subject;
use crate::rdf_authority::user_rdf_graph_iri;
use crate::rdf_service::{run_sparql_query_service, SparqlInput};

/// `GARDEN_PROFILE_DIR` is process-global; serialize the headless tests that set
/// it (across ALL modules — spine + applier) so they cannot stomp each other.
fn env_serial() -> &'static Mutex<()> {
    crate::tauri_runtime::profile_env_serial()
}

fn temp_profile(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    std::env::temp_dir().join(format!("garden-emporium-applier-{name}-{nanos}"))
}

/// Build a MockRuntime app handle with the CRDT queue + room registry managed,
/// so `enqueue_crdt_operation` drains through the in-process headless executor.
/// Funnels through the ONE crate-wide `generate_context!()` call site.
fn mock_app() -> AppHandle {
    crate::tauri_runtime::build_mock_app_for_tests(true)
}

// ── the fixture: the planner's own demo workflow def (mirrors spine/tests.rs) ──

fn parsed_json() -> Value {
    let script = "export const meta = {}\n";
    let sha = sha256_text(script);
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

fn ingest_request() -> IngestRequest {
    let body = json!({
        "vocab": "workflow",
        "dry_run": true,
        "payload": {
            "kind": "workflow",
            "parsed": parsed_json(),
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

/// Read a document's markdown back through the same read path the spine uses.
fn read_doc_markdown(app: &AppHandle, graph_id: &str, doc_id: &str) -> String {
    let graph_dir = crate::paths::existing_graph_dir(app, graph_id).expect("graph dir");
    let dir = crate::paths::document_dir(&graph_dir, doc_id).expect("doc dir");
    let manifest = dir.join("document.json");
    let record =
        crate::document_record_store::read_document_record(&graph_dir, &manifest).expect("record");
    crate::document_export_rendering::document_markdown(&record)
}

/// Count user:rdf-graph triples for a subject/predicate via SPARQL.
fn query_object(app: &AppHandle, graph_id: &str, subject: &str, predicate: &str) -> Option<String> {
    let user_rdf = user_rdf_graph_iri(graph_id);
    let query = format!(
        "SELECT ?o WHERE {{ GRAPH <{user_rdf}> {{ <{subject}> <{predicate}> ?o }} }} LIMIT 1"
    );
    let result = run_sparql_query_service(
        app.clone(),
        SparqlInput {
            graph_id: graph_id.to_string(),
            query,
        },
    )
    .expect("sparql query");
    result.rows.first().and_then(|r| r.get("o").cloned())
}

// ---------------------------------------------------------------------------
// Test 1: round-trip + loud halt
// ---------------------------------------------------------------------------

#[test]
fn apply_round_trips_and_loud_halts() {
    let _serial = env_serial().lock().unwrap_or_else(|p| p.into_inner());
    let profile = temp_profile("roundtrip");
    std::env::set_var("GARDEN_PROFILE_DIR", &profile);
    let result = std::panic::catch_unwind(run_round_trip_scenario);
    std::env::remove_var("GARDEN_PROFILE_DIR");
    let _ = std::fs::remove_dir_all(&profile);
    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}

fn run_round_trip_scenario() {
    let app = mock_app();
    let graph_id = "lab";
    seed_graph(&app, graph_id);
    let contract = workflow_vocabulary();
    let request = ingest_request();

    let planned = gather_and_plan(&app, graph_id, &request).expect("gather_and_plan");
    let plan = planned.plan;

    // Sanity: the plan must carry the verbs we are exercising.
    assert!(
        plan.steps
            .iter()
            .any(|s| matches!(s, Step::CreateFolder { .. })),
        "plan has a create_folder step"
    );
    assert!(
        plan.steps.iter().any(|s| matches!(
            s,
            Step::WriteDoc {
                capture_script_block: Some(true),
                ..
            }
        )),
        "plan has a write_doc with captureScriptBlock"
    );
    assert!(
        plan.steps.iter().any(|s| matches!(s, Step::Move { .. })),
        "plan has a move step"
    );
    assert!(
        plan.steps
            .iter()
            .any(|s| matches!(s, Step::CreateWires { .. })),
        "plan has a create_wires step"
    );

    let wf_doc_id = plan.workflow_doc_id.clone();
    let wf_content = plan
        .steps
        .iter()
        .find_map(|s| match s {
            Step::WriteDoc {
                doc_id, content, ..
            } if *doc_id == wf_doc_id => Some(content.clone()),
            _ => None,
        })
        .expect("workflow doc write step");
    assert!(
        wf_content.contains("```javascript"),
        "workflow doc body carries a ```javascript fence (the script-block anchor)"
    );

    // ── apply ──
    let report = crate::app_runtime::async_runtime::block_on(apply_plan(&app, graph_id, &plan, contract));
    assert!(report.ok, "apply ok; report: {report:?}");
    assert!(report.halted_at.is_none(), "no halt: {report:?}");

    // wf:scriptBlock captured, of the form {prefix}:doc:{docId}#block-XXXX.
    let prefix = graph_subject(graph_id);
    let script_block = report.script_block.clone().expect("scriptBlock captured");
    let expected_prefix = format!("{prefix}:doc:{wf_doc_id}#block-");
    assert!(
        script_block.starts_with(&expected_prefix),
        "scriptBlock URI shape: {script_block} (want prefix {expected_prefix})"
    );
    let block_id = script_block.rsplit('#').next().expect("fragment after #");
    assert!(
        block_id.starts_with("block-") && block_id.len() == "block-".len() + 8,
        "block id is block-<8hex>: {block_id}"
    );

    // The move of the just-written workflow doc is a recorded NO-OP.
    let move_entry = report
        .steps
        .iter()
        .find(|s| s.op == "move" && s.extra.get("docId") == Some(&json!(wf_doc_id)));
    if let Some(entry) = move_entry {
        assert_eq!(
            entry.extra.get("placedAtCreation"),
            Some(&json!(true)),
            "move of just-written doc is a recorded no-op: {entry:?}"
        );
    }

    // read_document(markdown) round-trips the workflow doc: the javascript fence
    // survives the document.write → TipTap → markdown round trip.
    let readback = read_doc_markdown(&app, graph_id, &wf_doc_id);
    assert!(
        readback.contains("```javascript"),
        "round-trip preserves the javascript fence: {readback}"
    );
    assert!(
        readback.contains("export const meta"),
        "round-trip preserves the script body: {readback}"
    );

    // read==write: the wf:scriptBlock triple landed in the user:rdf graph and
    // resolves to the captured concrete block URI (placeholder substituted).
    let wfns = contract.primary_namespace();
    let wf_uri = format!("{prefix}:doc:{wf_doc_id}");
    let stored = query_object(&app, graph_id, &wf_uri, &format!("{wfns}scriptBlock"))
        .expect("wf:scriptBlock triple present in user:rdf graph");
    assert_eq!(
        stored,
        format!("<{script_block}>"),
        "stored scriptBlock object is the captured concrete URI (no placeholder left)"
    );
    // The provenance managedBy triple landed too (emp: ledger in the same graph).
    let managed = query_object(
        &app,
        graph_id,
        &wf_uri,
        "http://mnemosyne.dev/emporium#managedBy",
    )
    .expect("emp:managedBy provenance present");
    let expected_managed = format!("\"emporium:{}@{}\"", contract.name, contract.version);
    assert_eq!(
        managed, expected_managed,
        "managedBy is emporium:{{name}}@{{version}}: {managed}"
    );

    // ── loud halt: a plan whose sole step is a doomed sparql_update ──
    let bad_plan = Plan {
        graph: graph_id.to_string(),
        workflow: "demo".to_string(),
        vocab: "workflow".to_string(),
        mode: "update".to_string(),
        short_id: "wf-demo".to_string(),
        workflow_doc_id: wf_doc_id.clone(),
        steps: vec![Step::SparqlUpdate {
            // Not an INSERT/DELETE DATA shape → graph_wrap rejects → loud halt.
            update: "THIS IS NOT SPARQL".to_string(),
        }],
        summary: plan.summary.clone(),
        warnings: Vec::new(),
        desired_inserts: Vec::new(),
        observer: String::new(),
        planned_at_ms: 0,
    };
    let halt = crate::app_runtime::async_runtime::block_on(apply_plan(&app, graph_id, &bad_plan, contract));
    assert!(!halt.ok, "doomed plan halts: {halt:?}");
    assert_eq!(halt.halted_at, Some(0), "halted at step 0: {halt:?}");
    let halted_step = &halt.steps[0];
    assert_eq!(halted_step.op, "sparql_update");
    assert_eq!(halted_step.extra.get("ok"), Some(&json!(false)));
    assert!(
        halted_step
            .extra
            .get("error")
            .and_then(Value::as_str)
            .map(|e| e.contains("unexpected update shape"))
            .unwrap_or(false),
        "halt error names the bad shape: {halted_step:?}"
    );
}

// ---------------------------------------------------------------------------
// Test 2: block-id stability across two applies of the same plan
// ---------------------------------------------------------------------------

#[test]
fn block_id_stability_two_apply() {
    let _serial = env_serial().lock().unwrap_or_else(|p| p.into_inner());
    let profile = temp_profile("stability");
    std::env::set_var("GARDEN_PROFILE_DIR", &profile);
    let result = std::panic::catch_unwind(run_stability_scenario);
    std::env::remove_var("GARDEN_PROFILE_DIR");
    let _ = std::fs::remove_dir_all(&profile);
    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}

fn run_stability_scenario() {
    let app = mock_app();
    let graph_id = "lab";
    seed_graph(&app, graph_id);
    let contract = workflow_vocabulary();
    let request = ingest_request();

    // ── apply #1: full plan, capture wf:scriptBlock ──
    let planned1 = gather_and_plan(&app, graph_id, &request).expect("first gather_and_plan");
    let plan1 = planned1.plan;
    assert_eq!(plan1.mode, "new", "first plan is a fresh mint");
    let report1 = crate::app_runtime::async_runtime::block_on(apply_plan(&app, graph_id, &plan1, contract));
    assert!(report1.ok, "first apply ok: {report1:?}");
    let script_block_1 = report1
        .script_block
        .clone()
        .expect("scriptBlock #1 captured");

    // ── re-plan: the SAME def must converge (write_doc dropped) ──
    let planned2 = gather_and_plan(&app, graph_id, &request).expect("second gather_and_plan");
    let plan2 = planned2.plan;
    assert_eq!(
        plan2.mode, "update",
        "second plan is an update (the workflow now exists)"
    );
    assert!(
        !plan2
            .steps
            .iter()
            .any(|s| matches!(s, Step::WriteDoc { .. })),
        "converged re-plan drops every write_doc (doc_converged): {:?}",
        plan2.steps.iter().map(Step::op_name).collect::<Vec<_>>()
    );
    assert!(
        !plan2.warnings.iter().any(|w| w.contains("hand-edited")),
        "no hand-edit warning on converged re-plan: {:?}",
        plan2.warnings
    );

    // The converged plan's wf:scriptBlock triple is no longer a placeholder — it
    // reuses the concrete value already in the graph (planner.rs:735-745). So the
    // re-apply's sparql_update carries the SAME block URI and the diff is empty.
    let prefix = graph_subject(graph_id);
    let wf_uri = format!("{prefix}:doc:{}", plan1.workflow_doc_id);
    let wfns = contract.primary_namespace();
    let stored_before = query_object(&app, graph_id, &wf_uri, &format!("{wfns}scriptBlock"))
        .expect("scriptBlock present before re-apply");

    // ── apply #2: re-apply the SAME (converged) plan ──
    let report2 = crate::app_runtime::async_runtime::block_on(apply_plan(&app, graph_id, &plan2, contract));
    assert!(report2.ok, "second apply ok: {report2:?}");
    assert!(
        !report2.steps.iter().any(|s| s.op == "write_doc"),
        "second apply performs NO write_doc (the rewrite was skipped): {:?}",
        report2
            .steps
            .iter()
            .map(|s| s.op.clone())
            .collect::<Vec<_>>()
    );

    // The wf:scriptBlock is BYTE-IDENTICAL across both applies.
    let stored_after = query_object(&app, graph_id, &wf_uri, &format!("{wfns}scriptBlock"))
        .expect("scriptBlock present after re-apply");
    assert_eq!(
        stored_before, stored_after,
        "wf:scriptBlock is byte-stable across two applies"
    );
    assert_eq!(
        stored_after,
        format!("<{script_block_1}>"),
        "the stored scriptBlock still equals the URI captured on apply #1"
    );
}
