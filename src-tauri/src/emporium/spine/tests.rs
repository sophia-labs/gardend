//! Loopback integration test for the ingest spine (headless feature only).
//!
//! Proves the spine's READS against a real gardend cell: build a MockRuntime
//! app over a temp profile, create a graph, then drive
//! [`super::gather_and_plan`] end-to-end.
//!
//!   1. dry-run on the seeded (empty) graph → a "new" plan whose summary
//!      anatomy matches: docWrites=5 (workflow + 2 nodes + archetype + run),
//!      wiresCreate=3, rdfInsert>0, rdfDelete=0.
//!   2. apply the plan to the cell the way the applier would — folders/docs into
//!      the workspace snapshot, doc bodies to disk, RDF + emp: provenance into
//!      `GRAPH <{root}:user:rdf>` (read==write graph).
//!   3. a SECOND dry-run after convergence → an all-zero summary (zero-ops
//!      convergence: the planner re-reads the just-minted state and plans
//!      nothing). This exercises the spine's gather: snapshot folders/docs/wires,
//!      doc markdown, RDF triples, and emp: provenance, all from the cell.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use super::*;
use crate::app_runtime::AppHandle;
use crate::document_types::DocumentRecord;
use crate::emporium::contract::workflow_vocabulary;
use crate::emporium::planner::{Plan, Step};
use crate::emporium::schemas::IngestRequest;
use crate::emporium::terms::{canonical_md, sha256_text, EMPORIUM_NS, PLACEHOLDER_NS};
use crate::graph_service::{create_graph_service, CreateGraphInput};
use crate::rdf::{document_subject, graph_subject};
use crate::rdf_authority::user_rdf_graph_iri;
use crate::rdf_service::{
    run_sparql_query_service, run_sparql_update_service, SparqlInput, SparqlUpdateInput,
};
use crate::runtime_config::{DOCUMENT_SCHEMA_VERSION, LOCAL_GRAPH_ORIGIN, LOCAL_PROVIDER_ID};
use crate::storage::write_json;
use crate::ydoc_paths::workspace_snapshot_path;

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
    std::env::temp_dir().join(format!("garden-emporium-spine-{name}-{nanos}"))
}

/// Build a MockRuntime app handle (no webview / loopback server — we drive the
/// services directly). Funnels through the ONE crate-wide `generate_context!()`
/// call site (see `tauri_runtime::build_mock_app_for_tests`) so the embedded
/// Info.plist link symbol stays unique across all lib-test mock apps.
fn mock_app() -> AppHandle {
    crate::tauri_runtime::build_mock_app_for_tests(false)
}

// ── the fixture: the planner's own demo workflow def ──

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

// ── applying a plan to the real cell (the P3 applier is HELD; this test owns a
//    faithful stand-in that writes exactly what the planner's ops describe) ──

/// A workspace snapshot we accrete as the plan creates folders/docs/wires, then
/// persist to the cell snapshot file so the next survey reads it back.
#[derive(Default)]
struct SnapshotBuilder {
    folders: Vec<Value>,
    documents: Vec<Value>,
    wires: Vec<Value>,
    folder_of_doc: BTreeMap<String, String>,
    wire_seq: usize,
}

impl SnapshotBuilder {
    fn upsert_folder(&mut self, id: &str, label: &str, parent: Option<&str>) {
        self.folders
            .retain(|f| f.get("id").and_then(Value::as_str) != Some(id));
        self.folders.push(json!({
            "id": id,
            "name": label,
            "parentId": parent,
        }));
    }

    fn ensure_doc(&mut self, id: &str) {
        if self
            .documents
            .iter()
            .any(|d| d.get("id").and_then(Value::as_str) == Some(id))
        {
            return;
        }
        self.documents.push(json!({
            "id": id,
            "title": id,
            "parentId": self.folder_of_doc.get(id),
        }));
    }

    fn move_doc(&mut self, id: &str, folder: &str) {
        self.folder_of_doc
            .insert(id.to_string(), folder.to_string());
        self.ensure_doc(id);
        for d in &mut self.documents {
            if d.get("id").and_then(Value::as_str) == Some(id) {
                d["parentId"] = json!(folder);
            }
        }
    }

    fn add_wire(&mut self, predicate: &str, src: &str, tgt: &str) {
        let id = format!("wire-{}", self.wire_seq);
        self.wire_seq += 1;
        self.wires.push(json!({
            "id": id,
            "predicate": predicate,
            "sourceDocumentId": src,
            "targetDocumentId": tgt,
        }));
    }

    fn snapshot(&self) -> Value {
        json!({
            "folders": self.folders,
            "documents": self.documents,
            "wires": self.wires,
            "workspace": { "materialization": "workspace-snapshot" },
        })
    }
}

/// Apply a plan to the cell: doc bodies → disk, folders/docs/wires → workspace
/// snapshot, RDF + emp: provenance → `GRAPH <{root}:user:rdf>`. Resolves the
/// SCRIPT_BLOCK placeholder to a concrete block URI, exactly as the real applier
/// would after writing the workflow doc.
fn apply_plan(app: &AppHandle, graph_id: &str, plan: &Plan) {
    let mut snap = SnapshotBuilder::default();
    let prefix = graph_subject(graph_id);
    let user_rdf = user_rdf_graph_iri(graph_id);

    // SCRIPT_BLOCK resolves to a block in the workflow doc (block ids are stable
    // post-write; the value only needs to be a stable concrete URI).
    let script_block_uri = format!("{prefix}:doc:{}#block-1", plan.workflow_doc_id);

    let mut provenance_docs: Vec<(String, String)> = Vec::new();

    for step in &plan.steps {
        match step {
            Step::CreateFolder {
                folder_id,
                label,
                parent_id,
            } => {
                snap.upsert_folder(folder_id, label, parent_id.as_deref());
            }
            Step::RenameFolder { folder_id, label } => {
                // Re-render with same parent (unknown here → keep prior parent).
                let parent = snap
                    .folders
                    .iter()
                    .find(|f| f.get("id").and_then(Value::as_str) == Some(folder_id))
                    .and_then(|f| f.get("parentId").and_then(Value::as_str))
                    .map(str::to_string);
                snap.upsert_folder(folder_id, label, parent.as_deref());
            }
            Step::WriteDoc {
                doc_id, content, ..
            } => {
                write_doc_body(app, graph_id, doc_id, content);
                snap.ensure_doc(doc_id);
                provenance_docs.push((doc_id.clone(), content.clone()));
            }
            Step::Move { doc_id, folder_id } => {
                snap.move_doc(doc_id, folder_id);
            }
            Step::CreateWires { wires } => {
                for w in wires {
                    snap.add_wire(&w.predicate, &w.source_document_id, &w.target_document_id);
                }
            }
            Step::DeleteWires { .. } => {}
            Step::SparqlUpdate { update } => {
                let resolved =
                    update.replace(&format!("{PLACEHOLDER_NS}SCRIPT_BLOCK"), &script_block_uri);
                let wrapped = wrap_in_user_graph(&resolved, &user_rdf);
                run_sparql_update_service(
                    app.clone(),
                    SparqlUpdateInput {
                        graph_id: graph_id.to_string(),
                        update: wrapped,
                    },
                )
                .expect("apply RDF op");
            }
        }
    }

    // Persist the workspace snapshot (the cell's get_workspace source).
    let graph_dir = crate::paths::existing_graph_dir(app, graph_id).expect("graph dir");
    write_json(&workspace_snapshot_path(&graph_dir), &snap.snapshot())
        .expect("write workspace snapshot");

    // Write emp: provenance (intentSha256 / renderSha256 / managedBy) so the
    // planner's doc_converged sees the docs as converged — render==intent here
    // because we wrote the exact planned content as the doc body.
    write_provenance(app, graph_id, &user_rdf, &prefix, &provenance_docs);
}

fn wrap_in_user_graph(update: &str, user_rdf: &str) -> String {
    // `INSERT DATA {\n<body> .\n}` → `INSERT DATA { GRAPH <user_rdf> {\n<body> .\n} }`
    let open = update.find('{').expect("update has body");
    let close = update.rfind('}').expect("update has body");
    let verb = &update[..open];
    let body = &update[open + 1..close];
    format!("{verb}{{ GRAPH <{user_rdf}> {{{body}}} }}")
}

fn write_doc_body(app: &AppHandle, graph_id: &str, doc_id: &str, content: &str) {
    let graph_dir = crate::paths::existing_graph_dir(app, graph_id).expect("graph dir");
    let local_path = crate::paths::documents_dir(&graph_dir).join(doc_id);
    let document = DocumentRecord {
        document_id: doc_id.to_string(),
        graph_id: graph_id.to_string(),
        title: doc_id.to_string(),
        revision: 1,
        body: content.to_string(),
        origin: LOCAL_GRAPH_ORIGIN.to_string(),
        provider_id: LOCAL_PROVIDER_ID.to_string(),
        local_path: crate::storage::display_path(&local_path),
        rdf_subject: document_subject(doc_id),
        created_at: "1000".to_string(),
        updated_at: "2000".to_string(),
        capabilities: Vec::new(),
        schema_version: DOCUMENT_SCHEMA_VERSION,
        tiptap_xml: String::new(),
        tiptap_json: None,
        ydoc_update_base64: String::new(),
        ydoc_state_path: String::new(),
        tree: None,
        blocks: Vec::new(),
        rdf_triple_count: 0,
        document_kind: None,
    };
    crate::document_record_store::write_document_record(&graph_dir, &document)
        .expect("write doc body");
}

fn write_provenance(
    app: &AppHandle,
    graph_id: &str,
    user_rdf: &str,
    prefix: &str,
    docs: &[(String, String)],
) {
    if docs.is_empty() {
        return;
    }
    let mut body = String::new();
    for (doc_id, content) in docs {
        let sha = sha256_text(&canonical_md(content));
        let subject = format!("{prefix}:doc:{doc_id}");
        body.push_str(&format!(
            "<{subject}> <{EMPORIUM_NS}intentSha256> \"{sha}\" .\n\
             <{subject}> <{EMPORIUM_NS}renderSha256> \"{sha}\" .\n\
             <{subject}> <{EMPORIUM_NS}managedBy> \"emporium:test\" .\n"
        ));
    }
    let update = format!("INSERT DATA {{ GRAPH <{user_rdf}> {{\n{body}}} }}");
    run_sparql_update_service(
        app.clone(),
        SparqlUpdateInput {
            graph_id: graph_id.to_string(),
            update,
        },
    )
    .expect("write provenance");
}

// ---------------------------------------------------------------------------
// the test
// ---------------------------------------------------------------------------

#[test]
fn spine_dry_run_plans_then_converges_to_zero_ops() {
    let _serial = env_serial().lock().unwrap_or_else(|p| p.into_inner());
    let profile = temp_profile("converge");
    std::env::set_var("GARDEN_PROFILE_DIR", &profile);

    let result = std::panic::catch_unwind(|| run_scenario());
    std::env::remove_var("GARDEN_PROFILE_DIR");
    let _ = std::fs::remove_dir_all(&profile);
    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}

fn run_scenario() {
    let app = mock_app();
    let graph_id = "lab";
    create_graph_service(
        &app,
        CreateGraphInput {
            graph_id: Some(graph_id.to_string()),
            title: "Lab".to_string(),
            description: None,
            operation_id: None,
        },
    )
    .expect("create graph");

    let request = ingest_request();

    // ── dry-run #1: full plan on the empty seeded graph ──
    let planned1 = gather_and_plan(&app, graph_id, &request).expect("first gather_and_plan");
    let p1 = &planned1.plan;
    let s1 = &p1.summary;
    assert_eq!(p1.mode, "new", "first plan mode");
    assert_eq!(p1.graph, graph_id);
    assert_eq!(p1.workflow, "demo");
    assert_eq!(p1.short_id, "wf-demo");
    assert_eq!(p1.workflow_doc_id, "wf-demo");
    // 5 docs: workflow + 2 nodes + 1 archetype + 1 run record.
    assert_eq!(s1.doc_writes, 5, "docWrites: {s1:?}");
    // a->b flowsInto, a/b exemplifies alpha.
    assert_eq!(s1.wires_create, 3, "wiresCreate: {s1:?}");
    assert!(s1.rdf_insert > 0, "rdfInsert>0: {s1:?}");
    assert_eq!(s1.rdf_delete, 0, "rdfDelete==0: {s1:?}");
    // dry-run NEVER touches the apply seam: no folders/docs exist yet.
    assert!(
        s1.folders >= 1,
        "creates at least the Workflows root: {s1:?}"
    );

    // The PlanOut shape is JSON-serializable with the documented keys.
    let plan_json = serde_json::to_value(p1).expect("plan serializes");
    for key in [
        "graph",
        "workflow",
        "mode",
        "shortId",
        "workflowDocId",
        "steps",
        "summary",
        "warnings",
    ] {
        assert!(
            plan_json.get(key).is_some(),
            "PlanOut missing {key}: {plan_json}"
        );
    }
    for key in [
        "folders",
        "docWrites",
        "moves",
        "wiresCreate",
        "wiresDelete",
        "rdfDelete",
        "rdfInsert",
    ] {
        assert!(
            plan_json["summary"].get(key).is_some(),
            "summary missing {key}"
        );
    }

    // ── converge: apply the plan to the cell ──
    apply_plan(&app, graph_id, p1);

    // ── dry-run #2: re-plan against the just-minted state → ZERO ops ──
    let planned2 = gather_and_plan(&app, graph_id, &request).expect("second gather_and_plan");
    let p2 = &planned2.plan;
    let s2 = &p2.summary;
    assert_eq!(p2.mode, "update", "second plan mode (workflow now exists)");
    assert_eq!(s2.folders, 0, "folders: {s2:?}");
    assert_eq!(s2.doc_writes, 0, "docWrites: {s2:?}");
    assert_eq!(s2.moves, 0, "moves: {s2:?}");
    assert_eq!(s2.wires_create, 0, "wiresCreate: {s2:?}");
    assert_eq!(s2.wires_delete, 0, "wiresDelete: {s2:?}");
    assert_eq!(
        s2.rdf_insert,
        0,
        "rdfInsert must be zero — {:?}",
        first_updates(p2, "INSERT")
    );
    assert_eq!(
        s2.rdf_delete,
        0,
        "rdfDelete must be zero — {:?}",
        first_updates(p2, "DELETE")
    );
    assert!(
        !p2.warnings.iter().any(|w| w.contains("hand-edited")),
        "no hand-edit warning on converged re-plan: {:?}",
        p2.warnings
    );
}

fn first_updates(plan: &Plan, verb: &str) -> Vec<String> {
    plan.steps
        .iter()
        .filter_map(|s| match s {
            Step::SparqlUpdate { update } if update.starts_with(verb) => Some(update.clone()),
            _ => None,
        })
        .take(2)
        .collect()
}

// ---------------------------------------------------------------------------
// End-to-end: apply_and_assert writes the anatomy, re-survey confirms it, and a
// second apply converges to an all-zero summary with a passing assertion.
//
// This is the P3-WP3 acceptance proof: the REAL write path (apply_plan over the
// in-process CRDT surface + RDF into `GRAPH <{root}:user:rdf>`) followed by the
// post-apply re-survey + assert_workflow, driven exactly as the dry_run=false
// route drives it (`apply_and_assert`).
// ---------------------------------------------------------------------------

/// MockRuntime app WITH the CRDT queue + room registry managed — the real applier
/// enqueues CRDT ops that drain through the in-process headless executor.
fn mock_app_with_state() -> AppHandle {
    crate::tauri_runtime::build_mock_app_for_tests(true)
}

/// Count distinct subjects of `rdf:type <{wfns}{class}>` in the user:rdf graph —
/// the SAME graph the survey reads (read==write). This is the follow-up
/// `sparql_query` the task asks for, run over `GRAPH <{root}:user:rdf>`. We
/// select the distinct subjects and count rows (no COUNT aggregate term to
/// re-parse).
fn count_class_instances(app: &AppHandle, graph_id: &str, class_local: &str) -> usize {
    let user_rdf = user_rdf_graph_iri(graph_id);
    let wfns = workflow_vocabulary().primary_namespace();
    let query = format!(
        "SELECT DISTINCT ?s WHERE {{ GRAPH <{user_rdf}> {{ ?s a <{wfns}{class_local}> }} }}"
    );
    let result = run_sparql_query_service(
        app.clone(),
        SparqlInput {
            graph_id: graph_id.to_string(),
            query,
        },
    )
    .expect("sparql class query");
    result.rows.len()
}

#[test]
fn apply_and_assert_writes_anatomy_then_converges() {
    let _serial = env_serial().lock().unwrap_or_else(|p| p.into_inner());
    let profile = temp_profile("apply-assert");
    std::env::set_var("GARDEN_PROFILE_DIR", &profile);
    let result = std::panic::catch_unwind(run_apply_and_assert_scenario);
    std::env::remove_var("GARDEN_PROFILE_DIR");
    let _ = std::fs::remove_dir_all(&profile);
    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}

fn run_apply_and_assert_scenario() {
    let app = mock_app_with_state();
    let graph_id = "lab";
    create_graph_service(
        &app,
        CreateGraphInput {
            graph_id: Some(graph_id.to_string()),
            title: "Lab".to_string(),
            description: None,
            operation_id: None,
        },
    )
    .expect("create graph");

    let contract = workflow_vocabulary();
    // dry_run=false: the route's write path.
    let mut request = ingest_request();
    request.dry_run = false;

    // ── apply #1: writes the full anatomy + asserts ──
    let planned1 = gather_and_plan(&app, graph_id, &request).expect("first gather_and_plan");
    assert_eq!(planned1.plan.mode, "new", "first plan is a fresh mint");
    let report1 = crate::app_runtime::async_runtime::block_on(apply_and_assert(
        &app,
        graph_id,
        &planned1.plan,
        contract,
        &request,
    ));
    assert!(report1.ok, "first apply ok: {report1:?}");
    assert!(report1.halted_at.is_none(), "no halt: {report1:?}");

    // The assertion was folded in (workflow kind) and PASSES.
    let assertion1 = report1
        .assertion
        .as_ref()
        .expect("assertion present (workflow kind)");
    assert!(
        assertion1.passed,
        "assertion.pass must be true after a fresh mint: failures={:?}",
        assertion1.failures
    );
    assert_eq!(assertion1.stats.nodes, Some(2), "2 AgentNodes asserted");
    assert_eq!(assertion1.stats.phases, Some(1), "1 Phase asserted");
    assert_eq!(assertion1.stats.agent_runs, Some(2), "2 AgentRuns asserted");

    // ── follow-up sparql_query over GRAPH <{root}:user:rdf> (read==write) ──
    // The anatomy landed: wf:Workflow (1) + N wf:Phase + M wf:AgentNode +
    // archetypes — read from the SAME graph the survey reads.
    assert_eq!(
        count_class_instances(&app, graph_id, "Workflow"),
        1,
        "exactly one wf:Workflow written to user:rdf"
    );
    assert_eq!(
        count_class_instances(&app, graph_id, "Phase"),
        1,
        "one wf:Phase (the 'Go' phase) written to user:rdf"
    );
    assert_eq!(
        count_class_instances(&app, graph_id, "AgentNode"),
        2,
        "two wf:AgentNode (a, b) written to user:rdf"
    );
    assert_eq!(
        count_class_instances(&app, graph_id, "Archetype"),
        1,
        "one wf:Archetype (alpha) written to user:rdf"
    );

    // ── apply #2: re-apply the SAME def → converged (ALL-ZERO summary) ──
    let planned2 = gather_and_plan(&app, graph_id, &request).expect("second gather_and_plan");
    assert_eq!(
        planned2.plan.mode, "update",
        "second plan is an update (the workflow now exists)"
    );
    let report2 = crate::app_runtime::async_runtime::block_on(apply_and_assert(
        &app,
        graph_id,
        &planned2.plan,
        contract,
        &request,
    ));
    assert!(report2.ok, "second apply ok=true (converged): {report2:?}");

    // The summary on the report is the plan's summary — it must be ALL-ZERO
    // across every counter (the camelCase keys the planner serializes).
    let summary = &report2.summary;
    for key in [
        "folders",
        "docWrites",
        "moves",
        "wiresCreate",
        "wiresDelete",
        "rdfInsert",
        "rdfDelete",
    ] {
        assert_eq!(
            summary.get(key).and_then(Value::as_u64),
            Some(0),
            "converged summary.{key} must be 0: {summary}"
        );
    }

    // The converged re-apply STILL re-asserts → assertion.pass == true.
    let assertion2 = report2
        .assertion
        .as_ref()
        .expect("assertion present on re-apply");
    assert!(
        assertion2.passed,
        "assertion.pass must be true on the converged re-apply: failures={:?}",
        assertion2.failures
    );
    assert!(
        !assertion2.warnings.iter().any(|w| w.contains("drifted")),
        "no managed-doc drift on a clean converged re-apply: {:?}",
        assertion2.warnings
    );
}
