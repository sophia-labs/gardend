//! Assertion-suite tests — port of `tests/emporium/test_asserts.py`.
//!
//! The proof: a converged plan's materialized outputs PASS assertion, and each
//! failure class is independently detectable. The snapshot is built the way the
//! source test does — plan against an empty graph, then read the plan's INSERT
//! DATA / write_doc / create_wires ops back into the (triples, wires, docs_md)
//! the suite consumes (resolving the SCRIPT_BLOCK placeholder to a concrete
//! block URI). The required-predicate sweep tests prune the materialized triples
//! and confirm the EXACT failure message.

use std::collections::BTreeMap;

use serde_json::{json, Value as Json};

use super::*;
use crate::emporium::contract::workflow_vocabulary;
use crate::emporium::planner::{plan_compute, Step};
use crate::emporium::schemas::{JudgmentInput, ParsedWorkflow};
use crate::emporium::survey::{parse_term, Live};
use crate::emporium::terms::{canonical_md, sha256_text, Term, Triple, PLACEHOLDER_NS};

const PREFIX: &str = "urn:mnemosyne:user:U:graph:lab";

fn parsed_json() -> Json {
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

fn parsed() -> ParsedWorkflow {
    serde_json::from_value(parsed_json()).unwrap()
}

fn judgment() -> JudgmentInput {
    serde_json::from_value(json!({
        "shortId": "wf-demo",
        "preamble": "Demo.",
        "rationale": "",
        "nodeArchetypes": {"a": "NEW:alpha", "b": "NEW:alpha"},
        "newArchetypes": [
            {"slug": "alpha", "title": "Alpha", "role": "r", "template": "t"}
        ]
    }))
    .unwrap()
}

fn empty_live() -> Live {
    Live {
        graph: "lab".to_string(),
        prefix: PREFIX.to_string(),
        read_graph: "urn:mnemosyne:user:U:graph:lab:user:rdf".to_string(),
        folders: BTreeMap::new(),
        docs: BTreeMap::new(),
        workflows: BTreeMap::new(),
        archetypes: BTreeMap::new(),
        contracts: BTreeMap::new(),
        runs: Vec::new(),
        nodes_by_workflow: BTreeMap::new(),
    }
}

/// Parse a rendered INSERT DATA body back into typed triples, substituting
/// placeholders the way the applier would. Mirrors the Python
/// `parse_update_triples`.
fn parse_update_triples(update: &str, resolve: &BTreeMap<String, String>) -> Vec<Triple> {
    let open = update.find('{').unwrap();
    let close = update.rfind('}').unwrap();
    let mut body = update[open + 1..close].to_string();
    for (name, uri) in resolve {
        body = body.replace(&format!("{PLACEHOLDER_NS}{name}"), uri);
    }
    let mut out = Vec::new();
    for stmt in body.split(" .\n") {
        let stmt = stmt.trim().trim_end_matches('.').trim();
        if stmt.is_empty() {
            continue;
        }
        let s_end = stmt.find('>').unwrap();
        let subject = &stmt[1..s_end];
        let rest = stmt[s_end + 1..].trim_start();
        let p_end = rest.find('>').unwrap();
        let predicate = &rest[1..p_end];
        let obj = rest[p_end + 1..].trim();
        out.push((subject.to_string(), predicate.to_string(), parse_term(obj)));
    }
    out
}

/// Plan against an empty graph and materialize its inserts/writes/wires as a
/// snapshot: `(triples, wires, docs_md)`. Mirrors the Python `applied_state()`.
fn applied_state() -> (Vec<Triple>, Vec<AssertWire>, BTreeMap<String, String>) {
    let p = parsed();
    let pj = parsed_json();
    let j = judgment();
    let run_json = pj.get("run").filter(|v| !v.is_null());
    let plan = plan_compute(
        workflow_vocabulary(),
        &p,
        &pj,
        run_json,
        Some(&j),
        &empty_live(),
        &[],
        &[],
        &BTreeMap::new(),
        &BTreeMap::new(),
        None,
    )
    .expect("plan_compute should succeed");

    let mut resolve = BTreeMap::new();
    resolve.insert(
        "SCRIPT_BLOCK".to_string(),
        format!("{PREFIX}:doc:wf-demo#block-1"),
    );

    let mut triples: Vec<Triple> = Vec::new();
    let mut docs_md: BTreeMap<String, String> = BTreeMap::new();
    let mut wires: Vec<AssertWire> = Vec::new();
    for st in &plan.steps {
        match st {
            Step::SparqlUpdate { update } if update.starts_with("INSERT DATA") => {
                triples.extend(parse_update_triples(update, &resolve));
            }
            Step::WriteDoc {
                doc_id, content, ..
            } => {
                docs_md.insert(doc_id.clone(), content.clone());
            }
            Step::CreateWires { wires: ws } => {
                for (i, w) in ws.iter().enumerate() {
                    wires.push(AssertWire {
                        id: format!("w{i}"),
                        predicate: Some(w.predicate.clone()),
                        source_document_id: Some(w.source_document_id.clone()),
                        target_document_id: Some(w.target_document_id.clone()),
                    });
                }
            }
            _ => {}
        }
    }
    (triples, wires, docs_md)
}

fn assert_demo(
    triples: &[Triple],
    wires: &[AssertWire],
    workflow_doc_md: Option<&str>,
) -> AssertReport {
    assert_workflow(
        workflow_vocabulary(),
        "demo",
        triples,
        wires,
        workflow_doc_md,
        None,
        None,
    )
}

// ---------------------------------------------------------------------------
// the complete-anatomy pass + each failure class
// ---------------------------------------------------------------------------

#[test]
fn applied_plan_passes_assertion() {
    let (triples, wires, docs_md) = applied_state();
    let report = assert_demo(&triples, &wires, docs_md.get("wf-demo").map(String::as_str));
    assert!(report.passed, "{:?}", report.failures);
    assert_eq!(report.stats.nodes, Some(2));
    assert_eq!(report.stats.agent_runs, Some(2));
    assert_eq!(report.stats.phases, Some(1));
}

#[test]
fn missing_workflow_fails() {
    let report = assert_workflow(workflow_vocabulary(), "nope", &[], &[], None, None, None);
    assert!(!report.passed);
    assert!(report.failures[0].contains("no wf:Workflow named 'nope'"));
}

#[test]
fn sha_mismatch_detected() {
    let (triples, wires, docs_md) = applied_state();
    let tampered = docs_md["wf-demo"].replace("export const meta", "export const META");
    let report = assert_demo(&triples, &wires, Some(&tampered));
    assert!(!report.passed);
    assert!(report.failures.iter().any(|f| f.contains("MISMATCH")));
}

#[test]
fn dropping_required_phase_fails_with_exact_message() {
    // Dropping the required wf:phase on the Workflow fails with the exact
    // generated message (the sweep follows the contract's `required` flag).
    let (triples, wires, docs_md) = applied_state();
    let wfns = workflow_vocabulary().primary_namespace();
    let phase_pred = format!("{wfns}phase");
    let pruned: Vec<Triple> = triples
        .iter()
        .filter(|(_, p, _)| p != &phase_pred)
        .cloned()
        .collect();
    let report = assert_demo(&pruned, &wires, docs_md.get("wf-demo").map(String::as_str));
    assert!(!report.passed);
    let wf_uri = format!("{PREFIX}:doc:wf-demo");
    let want = format!("Workflow <{wf_uri}> missing required wf:phase");
    assert!(
        report.failures.iter().any(|f| f == &want),
        "want exact `{want}`, got {:?}",
        report.failures
    );
}

#[test]
fn missing_required_script_sha_detected() {
    // Mirror of test_asserts.py::test_missing_required_predicate_detected — drop
    // wf:scriptSha256 and confirm the generated "missing required" message.
    let (triples, wires, docs_md) = applied_state();
    let wfns = workflow_vocabulary().primary_namespace();
    let sha_pred = format!("{wfns}scriptSha256");
    let pruned: Vec<Triple> = triples
        .iter()
        .filter(|(_, p, _)| p != &sha_pred)
        .cloned()
        .collect();
    let report = assert_demo(&pruned, &wires, docs_md.get("wf-demo").map(String::as_str));
    assert!(!report.passed);
    assert!(report
        .failures
        .iter()
        .any(|f| f.contains("missing required wf:scriptSha256")));
}

#[test]
fn agentrun_node_outside_workflow_fails() {
    // Rewire an AgentRun's wf:node to a doc that is NOT a node of this workflow.
    let (mut triples, wires, docs_md) = applied_state();
    let wfns = workflow_vocabulary().primary_namespace();
    let node_pred = format!("{wfns}node");
    let alien = format!("{PREFIX}:doc:agent-stranger");
    let mut rewired = false;
    for t in triples.iter_mut() {
        if t.1 == node_pred {
            t.2 = Term::Uri(oxigraph::model::NamedNode::new(&alien).unwrap());
            rewired = true;
            break;
        }
    }
    assert!(rewired, "fixture must carry at least one AgentRun wf:node");
    let report = assert_demo(&triples, &wires, docs_md.get("wf-demo").map(String::as_str));
    assert!(!report.passed);
    assert!(
        report.failures.iter().any(|f| {
            f.contains("wf:node -> not an AgentNode of this workflow") && f.contains(&alien)
        }),
        "{:?}",
        report.failures
    );
}

#[test]
fn rogue_predicate_fails() {
    // A primary-namespace predicate outside the contract (wf:improvised) trips
    // the rogue scan.
    let (mut triples, wires, docs_md) = applied_state();
    let wfns = workflow_vocabulary().primary_namespace();
    let subj = format!("{PREFIX}:doc:wf-demo");
    triples.push((
        subj,
        format!("{wfns}improvised"),
        Term::Lit(oxigraph::model::Literal::new_simple_literal("x")),
    ));
    let report = assert_demo(&triples, &wires, docs_md.get("wf-demo").map(String::as_str));
    assert!(!report.passed);
    assert!(
        report.failures.iter().any(|f| f.contains("improvised")),
        "{:?}",
        report.failures
    );
}

#[test]
fn duplicate_wires_detected() {
    let (triples, wires, docs_md) = applied_state();
    let mut dup = wires[0].clone();
    dup.id = "w-dup".to_string();
    let mut all = wires.clone();
    all.push(dup);
    let report = assert_demo(&triples, &all, docs_md.get("wf-demo").map(String::as_str));
    assert!(!report.passed);
    assert!(report
        .failures
        .iter()
        .any(|f| f.contains("duplicate wires")));
}

// ---------------------------------------------------------------------------
// managed-doc drift — non-fatal warning (NEVER a failure)
// ---------------------------------------------------------------------------

fn prov_for(docs_md: &BTreeMap<String, String>) -> BTreeMap<String, BTreeMap<String, String>> {
    docs_md
        .iter()
        .map(|(did, md)| {
            let sha = sha256_text(&canonical_md(md));
            let mut m = BTreeMap::new();
            m.insert("intentSha256".to_string(), sha.clone());
            m.insert("renderSha256".to_string(), sha);
            m.insert(
                "managedBy".to_string(),
                "emporium:workflow@test".to_string(),
            );
            (did.clone(), m)
        })
        .collect()
}

#[test]
fn drift_check_clean_when_provenance_matches() {
    let (triples, wires, docs_md) = applied_state();
    let prov = prov_for(&docs_md);
    let report = assert_workflow(
        workflow_vocabulary(),
        "demo",
        &triples,
        &wires,
        docs_md.get("wf-demo").map(String::as_str),
        Some(&docs_md),
        Some(&prov),
    );
    assert!(report.passed, "{:?}", report.failures);
    assert!(report.warnings.is_empty());
}

#[test]
fn hand_edited_doc_warns_but_does_not_fail() {
    let (triples, wires, docs_md) = applied_state();
    let prov = prov_for(&docs_md);
    // Hand-edit a node doc AFTER provenance was recorded — drift, not damage.
    let mut edited = docs_md.clone();
    let key = "wf-demo-n-a";
    let original = edited.get(key).cloned().expect("node-a doc must exist");
    edited.insert(key.to_string(), format!("{original}\nhuman note\n"));
    let report = assert_workflow(
        workflow_vocabulary(),
        "demo",
        &triples,
        &wires,
        edited.get("wf-demo").map(String::as_str),
        Some(&edited),
        Some(&prov),
    );
    // A hand-edit NEVER fails — it warns.
    assert!(report.passed, "{:?}", report.failures);
    assert!(
        report
            .warnings
            .iter()
            .any(|w| w.contains("wf-demo-n-a") && w.contains("drifted")),
        "{:?}",
        report.warnings
    );
}

#[test]
fn drift_check_silent_without_provenance() {
    let (triples, wires, docs_md) = applied_state();
    let report = assert_workflow(
        workflow_vocabulary(),
        "demo",
        &triples,
        &wires,
        docs_md.get("wf-demo").map(String::as_str),
        Some(&docs_md),
        None,
    );
    assert!(report.passed, "{:?}", report.failures);
    assert!(report.warnings.is_empty());
}
