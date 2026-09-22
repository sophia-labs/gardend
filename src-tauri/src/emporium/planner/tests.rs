//! Planner determinism + convergence tests — port of `tests/emporium/test_planner.py`.
//!
//! The load-bearing guarantee: a NEW workflow plans the folder/doc/wire ops; a
//! converged snapshot → ZERO ops; the RDF stage produces the exact desired-triple
//! set and re-running against the just-minted triples → rdfInsert==0 &&
//! rdfDelete==0.

use std::collections::BTreeMap;

use serde_json::{json, Value as Json};

use super::*;
use crate::emporium::contract::workflow_vocabulary;
use crate::emporium::schemas::{CampaignRecord, JudgmentInput, ParsedWorkflow};
use crate::emporium::survey::{DocEntry, EntityRow, FolderEntry, Live};
use crate::emporium::terms::{Term, Triple, PLACEHOLDER_NS};

const PREFIX: &str = "urn:mnemosyne:user:U:graph:lab";

fn parsed_json() -> Json {
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

fn parsed() -> ParsedWorkflow {
    serde_json::from_value(parsed_json()).unwrap()
}

fn judgment_json() -> Json {
    json!({
        "shortId": "wf-demo",
        "preamble": "Demo.",
        "rationale": "",
        "nodeArchetypes": {"a": "NEW:alpha", "b": "NEW:alpha"},
        "newArchetypes": [
            {"slug": "alpha", "title": "Alpha", "role": "r", "template": "t"}
        ]
    })
}

fn judgment() -> JudgmentInput {
    serde_json::from_value(judgment_json()).unwrap()
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

/// Run plan_compute against the PARSED/JUDGMENT fixtures with a given live state.
fn plan(
    p: &ParsedWorkflow,
    pj: &Json,
    j: Option<&JudgmentInput>,
    live: &Live,
    triples: &[Triple],
    wires: &[CurrentWire],
    docs_md: &BTreeMap<String, String>,
    prov: &BTreeMap<String, BTreeMap<String, String>>,
) -> Plan {
    let run_json = pj.get("run").filter(|v| !v.is_null());
    plan_compute(
        workflow_vocabulary(),
        p,
        pj,
        run_json,
        j,
        live,
        triples,
        wires,
        docs_md,
        prov,
        None,
    )
    .expect("plan_compute should succeed")
}

// ── N-Triples body parser (the applier's INSERT DATA read-back) ──

/// Parse a rendered INSERT DATA body back into typed triples, substituting
/// placeholders the way the applier would. Mirrors the Python `parse_update_triples`.
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
        // `<subject> <predicate> object`
        let s_end = stmt.find('>').unwrap();
        let subject = &stmt[1..s_end];
        let rest = stmt[s_end + 1..].trim_start();
        let p_end = rest.find('>').unwrap();
        let predicate = &rest[1..p_end];
        let obj = rest[p_end + 1..].trim();
        let term = crate::emporium::survey::parse_term(obj);
        out.push((subject.to_string(), predicate.to_string(), term));
    }
    out
}

/// Feed a plan's outputs back as live state (what a real apply produces).
/// Returns `(live2, triples, wires, docs_md, provenance)`. Mirrors `simulate_apply`.
fn simulate_apply(
    plan: &Plan,
    live: &Live,
    resolve: &BTreeMap<String, String>,
) -> (
    Live,
    Vec<Triple>,
    Vec<CurrentWire>,
    BTreeMap<String, String>,
    BTreeMap<String, BTreeMap<String, String>>,
) {
    let mut live2 = live.clone();
    let mut docs_md: BTreeMap<String, String> = BTreeMap::new();
    let mut provenance: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    let mut triples: Vec<Triple> = Vec::new();
    let mut wires: Vec<CurrentWire> = Vec::new();

    for st in &plan.steps {
        match st {
            Step::CreateFolder {
                folder_id,
                label,
                parent_id,
            } => {
                live2.folders.insert(
                    folder_id.clone(),
                    FolderEntry {
                        label: label.clone(),
                        parent_id: parent_id.clone(),
                    },
                );
            }
            Step::RenameFolder { folder_id, label } => {
                live2.folders.entry(folder_id.clone()).or_default().label = label.clone();
            }
            Step::WriteDoc {
                doc_id, content, ..
            } => {
                docs_md.insert(doc_id.clone(), content.clone());
                let h = crate::emporium::terms::sha256_text(&crate::emporium::terms::canonical_md(
                    content,
                ));
                let mut p = BTreeMap::new();
                p.insert("intentSha256".to_string(), h.clone());
                p.insert("renderSha256".to_string(), h);
                p.insert("managedBy".to_string(), "emporium:test".to_string());
                provenance.insert(doc_id.clone(), p);
                live2.docs.entry(doc_id.clone()).or_insert(DocEntry {
                    title: String::new(),
                    folder_id: None,
                });
            }
            Step::Move { doc_id, folder_id } => {
                live2
                    .docs
                    .entry(doc_id.clone())
                    .or_insert(DocEntry {
                        title: String::new(),
                        folder_id: None,
                    })
                    .folder_id = Some(folder_id.clone());
            }
            Step::CreateWires { wires: ws } => {
                for w in ws {
                    let id = format!("w{}", wires.len());
                    wires.push(CurrentWire {
                        id,
                        predicate: w.predicate.clone(),
                        source_document_id: w.source_document_id.clone(),
                        target_document_id: w.target_document_id.clone(),
                    });
                }
            }
            Step::SparqlUpdate { update } => {
                if update.starts_with("INSERT DATA") {
                    triples.extend(parse_update_triples(update, resolve));
                }
            }
            Step::DeleteWires { .. } => {}
        }
    }
    (live2, triples, wires, docs_md, provenance)
}

fn entity_row(uri: &str, doc_id: &str, name: &str, sha: Option<&str>) -> EntityRow {
    EntityRow {
        uri: uri.to_string(),
        doc_id: Some(doc_id.to_string()),
        name: name.to_string(),
        sha: sha.map(str::to_string),
    }
}

/// Plan onto empty, simulate the apply, dress the live snapshot the way a real
/// survey would see it afterwards. Mirrors `converged_state`.
fn converged_state() -> (
    Live,
    Vec<Triple>,
    Vec<CurrentWire>,
    BTreeMap<String, String>,
    BTreeMap<String, BTreeMap<String, String>>,
) {
    let p = parsed();
    let pj = parsed_json();
    let j = judgment();
    let p1 = plan(
        &p,
        &pj,
        Some(&j),
        &empty_live(),
        &[],
        &[],
        &BTreeMap::new(),
        &BTreeMap::new(),
    );
    let mut resolve = BTreeMap::new();
    resolve.insert(
        "SCRIPT_BLOCK".to_string(),
        format!("{PREFIX}:doc:wf-demo#block-1"),
    );
    let (mut live2, triples, wires, docs_md, prov) = simulate_apply(&p1, &empty_live(), &resolve);
    live2.workflows.insert(
        "demo".to_string(),
        entity_row(
            &format!("{PREFIX}:doc:wf-demo"),
            "wf-demo",
            "demo",
            Some(&pj["scriptSha256"].as_str().unwrap()),
        ),
    );
    let mut nodes = BTreeMap::new();
    nodes.insert("a".to_string(), "wf-demo-n-a".to_string());
    nodes.insert("b".to_string(), "wf-demo-n-b".to_string());
    live2
        .nodes_by_workflow
        .insert(format!("{PREFIX}:doc:wf-demo"), nodes);
    live2.archetypes.insert(
        "alpha".to_string(),
        entity_row(
            &format!("{PREFIX}:doc:agent-alpha"),
            "agent-alpha",
            "alpha",
            None,
        ),
    );
    (live2, triples, wires, docs_md, prov)
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[test]
fn plan_deterministic() {
    let p = parsed();
    let pj = parsed_json();
    let j = judgment();
    let p1 = plan(
        &p,
        &pj,
        Some(&j),
        &empty_live(),
        &[],
        &[],
        &BTreeMap::new(),
        &BTreeMap::new(),
    );
    let p2 = plan(
        &p,
        &pj,
        Some(&j),
        &empty_live(),
        &[],
        &[],
        &BTreeMap::new(),
        &BTreeMap::new(),
    );
    assert_eq!(
        serde_json::to_string(&p1).unwrap(),
        serde_json::to_string(&p2).unwrap()
    );
}

#[test]
fn full_plan_on_empty_graph() {
    let p = parsed();
    let pj = parsed_json();
    let j = judgment();
    let plan = plan(
        &p,
        &pj,
        Some(&j),
        &empty_live(),
        &[],
        &[],
        &BTreeMap::new(),
        &BTreeMap::new(),
    );
    let s = &plan.summary;
    assert_eq!(plan.mode, "new");
    // 5 docs: workflow + 2 nodes + 1 archetype + 1 run record.
    assert_eq!(s.doc_writes, 5, "{s:?}");
    assert_eq!(s.wires_create, 3, "{s:?}"); // a->b flowsInto, a/b exemplifies alpha
    assert!(s.rdf_insert > 0 && s.rdf_delete == 0, "{s:?}");
}

#[test]
fn converged_plan_is_zero_ops() {
    let (live2, triples, wires, docs_md, prov) = converged_state();
    let p = parsed();
    let pj = parsed_json();
    let j = judgment();
    let p3 = plan(&p, &pj, Some(&j), &live2, &triples, &wires, &docs_md, &prov);
    let s3 = &p3.summary;
    assert_eq!(p3.mode, "update");
    assert_eq!(s3.doc_writes, 0, "{s3:?}");
    assert_eq!(s3.moves, 0, "{s3:?}");
    assert_eq!(s3.folders, 0, "{s3:?}");
    assert_eq!(s3.wires_create, 0, "{s3:?}");
    assert_eq!(s3.wires_delete, 0, "{s3:?}");
    assert_eq!(
        s3.rdf_insert,
        0,
        "rdfInsert must be zero — {:?}",
        p3.steps
            .iter()
            .filter_map(|st| match st {
                Step::SparqlUpdate { update } if update.starts_with("INSERT") =>
                    Some(update.clone()),
                _ => None,
            })
            .take(2)
            .collect::<Vec<_>>()
    );
    assert_eq!(
        s3.rdf_delete,
        0,
        "rdfDelete must be zero — {:?}",
        p3.steps
            .iter()
            .filter_map(|st| match st {
                Step::SparqlUpdate { update } if update.starts_with("DELETE") =>
                    Some(update.clone()),
                _ => None,
            })
            .take(2)
            .collect::<Vec<_>>()
    );
    assert!(!p3.warnings.iter().any(|w| w.contains("hand-edited")));
}

#[test]
fn wf_agent_binding_edges_survive_workflow_rematerialization() {
    let (live2, mut triples, wires, docs_md, prov) = converged_state();
    let p = parsed();
    let pj = parsed_json();
    let j = judgment();
    let wfns = workflow_vocabulary().primary_namespace();
    let rdf_type = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";
    let run_type = format!("{wfns}Run");
    let agent_run_type = format!("{wfns}AgentRun");
    let bound_to_agent = format!("{wfns}boundToAgent");

    let run_subject = triples
        .iter()
        .find(|(_, pred, obj)| pred == rdf_type && obj.as_nt() == format!("<{run_type}>"))
        .map(|(subject, _, _)| subject.clone())
        .expect("fixture includes a wf:Run subject");
    let agent_run_subject = triples
        .iter()
        .find(|(_, pred, obj)| pred == rdf_type && obj.as_nt() == format!("<{agent_run_type}>"))
        .map(|(subject, _, _)| subject.clone())
        .expect("fixture includes a wf:AgentRun subject");

    triples.push((
        run_subject,
        bound_to_agent.clone(),
        crate::emporium::survey::parse_term(
            "<urn:sophia:agent:agent-0123456789abcdef:session:wf_test-1>",
        ),
    ));
    triples.push((
        agent_run_subject,
        bound_to_agent.clone(),
        crate::emporium::survey::parse_term(
            "<urn:sophia:agent:agent-0123456789abcdef:session:wf_test-1:turn:1>",
        ),
    ));

    let replan = plan(&p, &pj, Some(&j), &live2, &triples, &wires, &docs_md, &prov);
    let mut deletes = Vec::new();
    let empty = BTreeMap::new();
    for step in &replan.steps {
        if let Step::SparqlUpdate { update } = step {
            if update.starts_with("DELETE DATA") {
                deletes.extend(parse_update_triples(update, &empty));
            }
        }
    }

    assert_eq!(
        replan.summary.rdf_delete, 0,
        "wf-agent-binding edges are external to the workflow pack's managed delete sweep: {deletes:?}"
    );
    assert!(
        !deletes.iter().any(|(_, pred, _)| pred == &bound_to_agent),
        "wf:boundToAgent must survive workflow re-materialization, got deletes={deletes:?}"
    );
}

#[test]
fn intent_change_triggers_rewrite_without_drift_warning() {
    let (live2, triples, wires, docs_md, prov) = converged_state();
    let mut pj = parsed_json();
    pj["nodes"][0]["prompt"] = json!("a brand new prompt");
    let p: ParsedWorkflow = serde_json::from_value(pj.clone()).unwrap();
    let j = judgment();
    let plan = plan(&p, &pj, Some(&j), &live2, &triples, &wires, &docs_md, &prov);
    assert!(
        plan.steps.iter().any(|st| matches!(
            st,
            Step::WriteDoc { doc_id, .. } if doc_id == "wf-demo-n-a"
        )),
        "{:?}",
        plan.summary
    );
    assert!(!plan.warnings.iter().any(|w| w.contains("hand-edited")));
}

#[test]
fn hand_edit_triggers_rewrite_with_warning() {
    let (live2, triples, wires, mut docs_md, prov) = converged_state();
    let edited = format!("{}\nA stray human sentence.\n", docs_md["wf-demo"]);
    docs_md.insert("wf-demo".to_string(), edited);
    let p = parsed();
    let pj = parsed_json();
    let j = judgment();
    let plan = plan(&p, &pj, Some(&j), &live2, &triples, &wires, &docs_md, &prov);
    assert!(plan.steps.iter().any(|st| matches!(
        st,
        Step::WriteDoc { doc_id, .. } if doc_id == "wf-demo"
    )));
    assert!(
        plan.warnings
            .iter()
            .any(|w| w.contains("wf-demo hand-edited")),
        "{:?}",
        plan.warnings
    );
}

#[test]
fn missing_provenance_triggers_rewrite() {
    let (live2, triples, wires, docs_md, mut prov) = converged_state();
    prov.remove("wf-demo");
    let p = parsed();
    let pj = parsed_json();
    let j = judgment();
    let plan = plan(&p, &pj, Some(&j), &live2, &triples, &wires, &docs_md, &prov);
    assert!(plan.steps.iter().any(|st| matches!(
        st,
        Step::WriteDoc { doc_id, .. } if doc_id == "wf-demo"
    )));
    assert!(!plan.warnings.iter().any(|w| w.contains("hand-edited")));
}

#[test]
fn judgment_unknown_archetype_rejected() {
    let mut jj = judgment_json();
    jj["nodeArchetypes"] = json!({"a": "agent-nonexistent", "b": "NEW:alpha"});
    let bad: JudgmentInput = serde_json::from_value(jj).unwrap();
    let p = parsed();
    let pj = parsed_json();
    let run_json = pj.get("run").filter(|v| !v.is_null());
    let err = plan_compute(
        workflow_vocabulary(),
        &p,
        &pj,
        run_json,
        Some(&bad),
        &empty_live(),
        &[],
        &[],
        &BTreeMap::new(),
        &BTreeMap::new(),
        None,
    )
    .unwrap_err();
    assert!(err.0.contains("unknown archetype"), "{}", err.0);
}

#[test]
fn new_workflow_without_judgment_rejected() {
    let p = parsed();
    let pj = parsed_json();
    let run_json = pj.get("run").filter(|v| !v.is_null());
    let err = plan_compute(
        workflow_vocabulary(),
        &p,
        &pj,
        run_json,
        None,
        &empty_live(),
        &[],
        &[],
        &BTreeMap::new(),
        &BTreeMap::new(),
        None,
    )
    .unwrap_err();
    assert!(err.0.contains("judgment.shortId required"), "{}", err.0);
}

#[test]
fn runs_only_mode_on_sha_mismatch() {
    let mut live = empty_live();
    live.workflows.insert(
        "demo".to_string(),
        entity_row(
            &format!("{PREFIX}:doc:wf-demo"),
            "wf-demo",
            "demo",
            Some(&"f".repeat(64)),
        ),
    );
    live.docs.insert(
        "wf-demo".to_string(),
        DocEntry {
            title: "demo".to_string(),
            folder_id: Some("wf-demo-folder".to_string()),
        },
    );
    live.folders.insert(
        "wf-demo-folder".to_string(),
        FolderEntry {
            label: "demo".to_string(),
            parent_id: Some("workflows".to_string()),
        },
    );
    let p = parsed();
    let pj = parsed_json();
    let run_json = pj.get("run").filter(|v| !v.is_null());
    let plan = plan_compute(
        workflow_vocabulary(),
        &p,
        &pj,
        run_json,
        None,
        &live,
        &[],
        &[],
        &BTreeMap::new(),
        &BTreeMap::new(),
        None,
    )
    .unwrap();
    assert_eq!(plan.mode, "runs-only");
    assert!(plan.warnings.iter().any(|w| w.contains("filing run only")));
    // run doc + provenance folder, but no node/workflow doc writes.
    assert_eq!(plan.summary.doc_writes, 1);
    // the run carries wf:scriptSource marking what actually ran.
    let updates: String = plan
        .steps
        .iter()
        .filter_map(|st| match st {
            Step::SparqlUpdate { update } => Some(update.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(" ");
    assert!(updates.contains("scriptSource"));
}

#[test]
fn spaced_workflow_name_yields_valid_iris() {
    let mut pj = parsed_json();
    pj["name"] = json!("Demo Research and Synthesis");
    let p: ParsedWorkflow = serde_json::from_value(pj.clone()).unwrap();
    let j = judgment();
    let plan = plan(
        &p,
        &pj,
        Some(&j),
        &empty_live(),
        &[],
        &[],
        &BTreeMap::new(),
        &BTreeMap::new(),
    );
    let re = regex::Regex::new("<([^>]*)>").unwrap();
    for st in &plan.steps {
        if let Step::SparqlUpdate { update } = st {
            for cap in re.captures_iter(update) {
                assert!(!cap[1].contains(' '), "IRI with space in plan: {}", &cap[1]);
            }
        }
    }
    let joined = serde_json::to_string(&plan).unwrap();
    assert!(joined.contains("urn:sophia:wf:Demo-Research-and-Synthesis:phase:1"));
}

#[test]
fn iso_only_end_time_renders_ended_at() {
    let mut pj = parsed_json();
    pj["run"]["endTimeMs"] = Json::Null;
    pj["run"]["endTimeIso"] = json!("2026-06-11T01:21:53.305Z");
    let p: ParsedWorkflow = serde_json::from_value(pj.clone()).unwrap();
    let j = judgment();
    let plan = plan(
        &p,
        &pj,
        Some(&j),
        &empty_live(),
        &[],
        &[],
        &BTreeMap::new(),
        &BTreeMap::new(),
    );
    let joined = serde_json::to_string(&plan).unwrap();
    assert!(joined.contains("2026-06-11T01:21:53.305Z"));
    assert!(joined.contains("endedAtTime"));
}

// ── campaign ──

fn campaign_json() -> Json {
    let raw = std::fs::read_to_string(campaign_fixture_path()).expect("read campaign fixture");
    serde_json::from_str(&raw).expect("parse campaign fixture")
}

fn campaign_fixture_path() -> String {
    // The canonical GEPA campaign fixture, copied INTO this repo at
    // src/emporium/fixtures/campaign.json (was an absolute path into the sibling
    // mnemosyne-platform-emporium repo — non-portable, broke `cargo test` for
    // anyone without that checkout). Resolved repo-relative from CARGO_MANIFEST_DIR
    // (= src-tauri/) so `cargo test` is portable. Regen: see fixtures/README.md.
    format!(
        "{}/src/emporium/fixtures/campaign.json",
        env!("CARGO_MANIFEST_DIR")
    )
}

fn campaign() -> CampaignRecord {
    serde_json::from_value(campaign_json()).unwrap()
}

fn campaign_live() -> Live {
    let mut live = empty_live();
    live.archetypes.insert(
        "manifest-judge".to_string(),
        entity_row(
            &format!("{PREFIX}:doc:agent-manifest-judge"),
            "agent-manifest-judge",
            "manifest-judge",
            None,
        ),
    );
    live.folders.insert(
        "agent-library".to_string(),
        FolderEntry {
            label: "Agent Library".to_string(),
            parent_id: None,
        },
    );
    live.docs.insert(
        "agent-manifest-judge".to_string(),
        DocEntry {
            title: "manifest-judge".to_string(),
            folder_id: Some("agent-library".to_string()),
        },
    );
    live
}

#[test]
fn campaign_full_plan_and_convergence() {
    let c = campaign();
    let cj = campaign_json();
    let p1 = plan_campaign_compute(
        workflow_vocabulary(),
        &c,
        &cj,
        &campaign_live(),
        &[],
        &[],
        &BTreeMap::new(),
        &BTreeMap::new(),
    )
    .unwrap();
    let s1 = &p1.summary;
    let n = cj["candidates"].as_array().unwrap().len();
    assert_eq!(p1.mode, "campaign");
    assert_eq!(s1.doc_writes, n + 1, "{s1:?}"); // variants + record doc
    assert!(s1.rdf_insert > 0 && s1.rdf_delete == 0);

    // simulate apply, resolving every TB placeholder to a block URI.
    let mut resolve = BTreeMap::new();
    for st in &p1.steps {
        if let Step::WriteDoc { doc_id, .. } = st {
            resolve.insert(
                format!("TB:{doc_id}"),
                format!("{PREFIX}:doc:{doc_id}#block-1"),
            );
        }
    }
    let (live2, triples, wires, docs_md, prov) = simulate_apply(&p1, &campaign_live(), &resolve);
    let p2 = plan_campaign_compute(
        workflow_vocabulary(),
        &c,
        &cj,
        &live2,
        &triples,
        &wires,
        &docs_md,
        &prov,
    )
    .unwrap();
    let s2 = &p2.summary;
    assert_eq!(s2.folders, 0, "{s2:?}");
    assert_eq!(s2.doc_writes, 0, "{s2:?}");
    assert_eq!(s2.moves, 0, "{s2:?}");
    assert_eq!(s2.wires_create, 0, "{s2:?}");
    assert_eq!(s2.wires_delete, 0, "{s2:?}");
    assert_eq!(
        s2.rdf_delete,
        0,
        "rdfDelete must be zero — {:?}",
        p2.steps
            .iter()
            .filter_map(|st| match st {
                Step::SparqlUpdate { update } if update.starts_with("DELETE") =>
                    Some(update.clone()),
                _ => None,
            })
            .take(2)
            .collect::<Vec<_>>()
    );
    assert_eq!(
        s2.rdf_insert,
        0,
        "rdfInsert must be zero — {:?}",
        p2.steps
            .iter()
            .filter_map(|st| match st {
                Step::SparqlUpdate { update } if update.starts_with("INSERT") =>
                    Some(update.clone()),
                _ => None,
            })
            .take(2)
            .collect::<Vec<_>>()
    );
}

#[test]
fn campaign_missing_archetype_rejected() {
    let c = campaign();
    let cj = campaign_json();
    let err = plan_campaign_compute(
        workflow_vocabulary(),
        &c,
        &cj,
        &empty_live(),
        &[],
        &[],
        &BTreeMap::new(),
        &BTreeMap::new(),
    )
    .unwrap_err();
    assert!(err.0.contains("not found in graph"), "{}", err.0);
}

#[test]
fn plan_is_json_serializable() {
    let p = parsed();
    let pj = parsed_json();
    let j = judgment();
    let plan = plan(
        &p,
        &pj,
        Some(&j),
        &empty_live(),
        &[],
        &[],
        &BTreeMap::new(),
        &BTreeMap::new(),
    );
    let joined = serde_json::to_string(&plan).unwrap();
    // no mustache placeholders in the new grammar — urn placeholders only.
    assert!(!joined.contains("{{"));
    // placeholders render inside SPARQL strings, not as objects.
    for st in &plan.steps {
        if let Step::WriteDoc { content, .. } = st {
            assert!(!content.contains("{{"));
        }
    }
}

/// Sanity: the RDF stage produces the exact desired-triple set we expect for the
/// minted Workflow subject (the keystone "exact desired-triple set" assertion).
#[test]
fn rdf_stage_mints_workflow_with_value_canonical_zero_ops() {
    let (live2, triples, wires, docs_md, prov) = converged_state();
    let p = parsed();
    let pj = parsed_json();
    let j = judgment();
    // The minted triple set is in `triples` (from the first apply). A re-plan
    // against just those minted triples yields zero rdf ops.
    let p3 = plan(&p, &pj, Some(&j), &live2, &triples, &wires, &docs_md, &prov);
    assert_eq!(p3.summary.rdf_insert, 0);
    assert_eq!(p3.summary.rdf_delete, 0);
    // The Workflow subject is present in the minted triples (wf:name "demo").
    let wf_uri = format!("{PREFIX}:doc:wf-demo");
    let wfns = workflow_vocabulary().primary_namespace();
    let has_name = triples.iter().any(|(s, p, o)| {
        s == &wf_uri
            && p == &format!("{wfns}name")
            && matches!(o, Term::Lit(l) if l.value() == "demo")
    });
    assert!(has_name, "minted Workflow must carry wf:name \"demo\"");
}

// ---------------------------------------------------------------------------
// Memory pack — content-hash determinism + zero-ops idempotency (pure planner).
// ---------------------------------------------------------------------------

use crate::emporium::contract::memory_core_vocabulary;
use crate::emporium::schemas::{MemoryRecordIn, SourceRefIn};

fn mem_live() -> Live {
    Live {
        graph: "lab".to_string(),
        prefix: "urn:mnemosyne:local:graph:lab".to_string(),
        read_graph: "urn:mnemosyne:local:graph:lab:projection:memory".to_string(),
        ..Live::default()
    }
}

fn sample_record() -> MemoryRecordIn {
    MemoryRecordIn {
        client_ref: Some("r-0".to_string()),
        scope: "agent".to_string(),
        kind: "ClaimMemory".to_string(),
        content_orientation: "knowledge".to_string(),
        visibility: "private".to_string(),
        status: "active".to_string(),
        content: "vera prefers fish CLI".to_string(),
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

#[test]
fn memory_subject_is_content_addressed_and_deterministic() {
    let r = sample_record();
    // Empty observer = the shared commons (byte-identical to the pre-per-agent hash).
    let sr = source_ref_id("lab", "", &r.source_refs[0]);
    let id_a = memory_record_id("lab", "", &r, &[sr.clone()]);
    let id_b = memory_record_id("lab", "", &r, &[sr.clone()]);
    assert_eq!(id_a, id_b, "same inputs → same memId");
    assert!(id_a.starts_with("urn:mnemosyne:local:graph:lab:projection:memory:record:"));

    // Changed content → different subject (a new version).
    let mut r2 = sample_record();
    r2.content = "vera prefers zsh".to_string();
    let id_c = memory_record_id("lab", "", &r2, &[sr]);
    assert_ne!(id_a, id_c, "changed content must re-mint the subject");
}

#[test]
fn memory_plan_mints_then_refiles_to_zero_ops() {
    let contract = memory_core_vocabulary();
    let live = mem_live();
    let records = vec![sample_record()];

    // First file on a clean graph: self-creates the folder + inserts triples.
    let plan = plan_memory_compute(contract, "lab", &records, &live, &[]).unwrap();
    assert_eq!(plan.vocab, "sophia-memory-core");
    assert_eq!(plan.mode, "memory");
    assert!(plan.summary.rdf_insert > 0, "first file inserts triples");
    assert_eq!(plan.summary.rdf_delete, 0);
    assert!(
        plan.steps.iter().any(|s| s.op_name() == "create_folder"),
        "clean-graph first write self-creates the memory folder"
    );

    // Reconstruct the minted triples as the live memory set, then re-file the
    // identical record → zero ops (Case B idempotency).
    let minted = collect_insert_triples(&plan);
    let mut live2 = mem_live();
    // Pretend the folder now exists so the folder op also converges.
    live2.folders.insert(
        "memory".to_string(),
        crate::emporium::survey::FolderEntry {
            label: "Memory".to_string(),
            parent_id: None,
        },
    );
    let replan = plan_memory_compute(contract, "lab", &records, &live2, &minted).unwrap();
    assert_eq!(replan.summary.rdf_insert, 0, "re-file inserts nothing");
    assert_eq!(replan.summary.rdf_delete, 0, "re-file deletes nothing");
    assert!(
        !replan.steps.iter().any(|s| s.op_name() == "create_folder"),
        "folder op suppressed once the folder exists"
    );
}

#[test]
fn memory_plan_rejects_provenanceless_record() {
    let contract = memory_core_vocabulary();
    let live = mem_live();
    let mut r = sample_record();
    r.source_refs.clear();
    let err = plan_memory_compute(contract, "lab", &[r], &live, &[]).unwrap_err();
    assert!(err.0.contains("PROVENANCE"), "{}", err.0);
}

/// REGRESSION (apply-dispatch routing): a memory plan must route to the
/// direct-on-store memory materializer (`:projection:memory`), and that routing
/// MUST key on `mode`, not `vocab`. The ratified pack name is "sophia-memory-core"
/// — which does NOT start with "mem" — so the old `vocab.starts_with("mem")` gate
/// silently fell through to the user:rdf applier, leaking typed memory into
/// `:user:rdf` and leaving `:projection:memory` empty (so the survey saw no live
/// heads and supersession never fired). This pins both halves of that trap.
#[test]
fn memory_plan_routes_to_memory_sink_via_mode_not_vocab() {
    let contract = memory_core_vocabulary();
    let plan = plan_memory_compute(contract, "lab", &[sample_record()], &mem_live(), &[]).unwrap();
    assert!(
        plan.routes_to_memory_sink(),
        "memory plan must route to the direct-on-store memory sink"
    );
    assert_eq!(plan.mode, "memory", "mode is the routing signal");
    assert!(
        !plan.vocab.starts_with("mem"),
        "TRAP: pack name {:?} does not start with 'mem' — routing on vocab would \
         silently send memory to the user:rdf applier",
        plan.vocab
    );
}

/// LOAD-BEARING PROOF (T5.22, distinguishes honest wiring from a decorative
/// read): `plan.mode` — and therefore `routes_to_memory_sink()` — is DERIVED
/// from the contract's declared `MemoryRecord` signature (via
/// `class_dispatch::resolve`), not a bare hardcoded `"memory"` literal. Flip
/// the declared `store_target` on a synthetic memory-shaped contract and the
/// mode moves off "memory" with it; the real `memory_core_vocabulary()`
/// (asserted above) is the only contract production ever passes here, so
/// nothing observable changes today — but the read is now real, not
/// decorative.
#[test]
fn memory_mode_is_derived_from_the_declared_signature_not_hardcoded() {
    fn synthetic_memory_contract(store_target: &str) -> VocabularyContract {
        serde_json::from_value(json!({
            "name": "demo-memory-core",
            "version": "1.0.0",
            "title": "Demo Memory Core",
            "description": "demo",
            "namespaces": {
                "mem": "http://example.test/mem#",
                "rdf": "http://www.w3.org/1999/02/22-rdf-syntax-ns#",
                "xsd": "http://www.w3.org/2001/XMLSchema#"
            },
            "primary_prefix": "mem",
            "classes": {
                "MemoryRecord": {
                    "rdf_types": ["mem:MemoryRecord"],
                    "subject_rule": "descriptive:memory-record",
                    "source_kind": "current-state",
                    "identity_kind": "content-hash",
                    "store_mode": "materialize",
                    "store_target": store_target,
                    "enforcement": "halt",
                    "reconciliation_strategy": "codeBacked",
                    "dispatch_mode": "current-state-materialize",
                    "predicates": {}
                }
            }
        }))
        .expect("synthetic memory contract parses")
    }

    let live = mem_live();

    let memory_shaped = synthetic_memory_contract("projection:memory");
    let plan = plan_memory_compute(&memory_shaped, "lab", &[], &live, &[]).unwrap();
    assert_eq!(plan.mode, "memory");
    assert!(plan.routes_to_memory_sink());

    let flipped = synthetic_memory_contract("user:rdf");
    let plan2 = plan_memory_compute(&flipped, "lab", &[], &live, &[]).unwrap();
    assert_ne!(
        plan2.mode, "memory",
        "the declared signature moved off projection:memory — the mode must follow, \
         not stay hardcoded"
    );
    assert!(!plan2.routes_to_memory_sink());
}

/// Parse every INSERT DATA step in a plan back into typed triples (reuses the
/// module's N-Triples body parser).
fn collect_insert_triples(plan: &Plan) -> Vec<Triple> {
    let mut out = Vec::new();
    let empty = BTreeMap::new();
    for step in &plan.steps {
        if let Step::SparqlUpdate { update } = step {
            if update.starts_with("INSERT DATA") {
                out.extend(parse_update_triples(update, &empty));
            }
        }
    }
    out
}

// ═══════════════════════════════════════════════════════════════════════════
// EA-3 — the GENERIC simple-projection planner (B5 subject minting + B4 plan).
//
// Pure-unit tests (no store): prove `plan_generic_compute` parses the Template
// subject_rule, mints the deterministic subject, maps the flat record fields onto
// the class predicates, and renders the desired triples through the SAME
// frozen-vocab guard — including the loud rejections (missing required, rogue
// predicate). The store-level convergence oracle lives in `state_trace_tests.rs`.
// ═══════════════════════════════════════════════════════════════════════════
mod generic_planner_tests {
    use super::*;
    use crate::emporium::contract::get_vocabulary;
    use crate::emporium::schemas::GenericRecordIn;

    const GRAPH: &str = "lab";
    const BM_NS: &str = "http://mnemosyne.dev/bookmark#";

    fn record(json: Json) -> GenericRecordIn {
        serde_json::from_value(json).expect("generic record deserializes")
    }

    fn rust_bookmark() -> GenericRecordIn {
        record(json!({
            "kind": "Bookmark",
            "localId": "rust-book",
            "url": "https://doc.rust-lang.org/book/",
            "title": "The Rust Book",
            "note": "intro",
            "tag": ["rust", "ref"]
        }))
    }

    fn object_nt<'a>(triples: &'a [Triple], predicate: &str) -> Vec<String> {
        triples
            .iter()
            .filter(|(_, p, _)| p == predicate)
            .map(|(_, _, o)| o.as_nt())
            .collect()
    }

    #[test]
    fn generic_plan_mints_subject_and_renders_class_triples() {
        let contract = get_vocabulary("emporium-bookmark").unwrap();
        let plan = plan_generic_compute(contract, GRAPH, &[rust_bookmark()]).expect("plan");

        assert_eq!(plan.mode, "simple-projection");
        assert!(plan.routes_to_simple_projection());
        assert_eq!(plan.vocab, "emporium-bookmark");
        // No CRDT/SparqlUpdate steps — the reconcile applier owns the diff/apply.
        assert!(plan.steps.is_empty(), "generic plan emits no steps");

        let d = &plan.desired_inserts;
        // The B5 Template subject: {graph_subject}:projection:bookmark:bookmark:{localId}.
        let expected_subject =
            "urn:mnemosyne:local:graph:lab:projection:bookmark:bookmark:rust-book";
        assert!(
            d.iter().all(|(s, _, _)| s == expected_subject),
            "every desired triple is on the minted subject"
        );
        // rdf:type bm:Bookmark.
        assert_eq!(
            object_nt(d, "http://www.w3.org/1999/02/22-rdf-syntax-ns#type"),
            vec![format!("<{BM_NS}Bookmark>")]
        );
        // bm:url is a uri object; bm:title a plain literal.
        assert_eq!(
            object_nt(d, &format!("{BM_NS}url")),
            vec!["<https://doc.rust-lang.org/book/>"]
        );
        assert_eq!(
            object_nt(d, &format!("{BM_NS}title")),
            vec!["\"The Rust Book\""]
        );
        assert_eq!(object_nt(d, &format!("{BM_NS}note")), vec!["\"intro\""]);
        // bm:tag is multi → two triples.
        assert_eq!(object_nt(d, &format!("{BM_NS}tag")).len(), 2);
        // rdfInsert summary counts the desired set.
        assert_eq!(plan.summary.rdf_insert, d.len());
    }

    #[test]
    fn generic_plan_binds_authoritative_graph_id_in_subject_rules() {
        let contract = get_vocabulary("workflow").unwrap();
        let event = record(json!({
            "kind": "CompositionEvent",
            "localId": "source-event-1",
            "workflowName": "offline-audit",
            "sessionId": "offline-session",
            "eventOrder": 1,
            "generatedAtTime": "2026-07-29T00:00:00Z",
            "definitionSubject": "urn:offline-audit:workflow",
            "gestureKind": "insert",
            "partOfAuthoringSession": "urn:offline-audit:session"
        }));

        let plan = plan_generic_compute(contract, GRAPH, &[event]).expect("plan");
        let expected = "urn:sophia:wf:composition-event:lab:offline-audit:offline-session:1";
        assert!(
            plan.desired_inserts
                .iter()
                .all(|(subject, _, _)| subject == expected),
            "the graphId identity component must come from the graph authority"
        );
    }

    #[test]
    fn generic_plan_is_deterministic() {
        let contract = get_vocabulary("emporium-bookmark").unwrap();
        let a = plan_generic_compute(contract, GRAPH, &[rust_bookmark()]).unwrap();
        let b = plan_generic_compute(contract, GRAPH, &[rust_bookmark()]).unwrap();
        let canon = |p: &Plan| -> Vec<(String, String, String)> {
            let mut v: Vec<_> = p
                .desired_inserts
                .iter()
                .map(|(s, pr, o)| (s.clone(), pr.clone(), o.as_nt()))
                .collect();
            v.sort();
            v
        };
        assert_eq!(canon(&a), canon(&b), "same record → same desired triples");
    }

    #[test]
    fn missing_required_predicate_is_a_plan_error() {
        let contract = get_vocabulary("emporium-bookmark").unwrap();
        // A Bookmark missing the required bm:url.
        let bad = record(json!({"kind": "Bookmark", "localId": "x", "title": "No URL"}));
        let err = plan_generic_compute(contract, GRAPH, &[bad]).expect_err("missing required url");
        assert!(
            err.0.contains("url") || err.0.contains("required"),
            "{}",
            err.0
        );
    }

    #[test]
    fn rogue_field_is_rejected_by_the_no_silent_drop_guard() {
        let contract = get_vocabulary("emporium-bookmark").unwrap();
        // `bm:bogus` is not a declared predicate → the no-silent-drop guard rejects
        // it loud (a record field outside {localId, declared predicates, rule
        // tokens} cannot be quietly ignored — the exact class of the sourceRef typo).
        let bad = record(json!({
            "kind": "Bookmark", "localId": "x",
            "url": "https://x", "title": "t", "bm:bogus": "nope"
        }));
        let err = plan_generic_compute(contract, GRAPH, &[bad]).expect_err("rogue field");
        assert!(
            err.0.contains("unknown field") && err.0.contains("bogus"),
            "{}",
            err.0
        );
    }

    #[test]
    fn misspelled_known_field_is_rejected_not_silently_dropped() {
        let contract = get_vocabulary("emporium-bookmark").unwrap();
        // `titl` (typo for `title`) would silently strip the title if not guarded —
        // and then the required-predicate check would ALSO fire, but the loud
        // unknown-field error is the right, actionable one.
        let typo = record(json!({
            "kind": "Bookmark", "localId": "x", "url": "https://x", "titl": "t"
        }));
        let err = plan_generic_compute(contract, GRAPH, &[typo]).expect_err("typo'd field");
        assert!(
            err.0.contains("unknown field") && err.0.contains("titl"),
            "{}",
            err.0
        );
    }

    #[test]
    fn local_name_and_curie_field_keys_both_resolve() {
        let contract = get_vocabulary("emporium-bookmark").unwrap();
        // CURIE-keyed fields (`bm:url`) resolve the same as local-name (`url`).
        let curie_keyed = record(json!({
            "kind": "Bookmark", "localId": "x",
            "bm:url": "https://x", "bm:title": "t"
        }));
        let plan = plan_generic_compute(contract, GRAPH, &[curie_keyed]).expect("plan");
        assert_eq!(
            object_nt(&plan.desired_inserts, &format!("{BM_NS}url")),
            vec!["<https://x>"]
        );
    }

    #[test]
    fn descriptive_subject_rule_is_rejected_in_the_generic_path() {
        // The memory pack's Claim uses a descriptive subject_rule; the generic
        // planner has no code-mint default for it → a clear PlanError.
        let contract = get_vocabulary("sophia-memory-core").unwrap();
        let claim = record(json!({"kind": "Claim", "localId": "x"}));
        let err = plan_generic_compute(contract, GRAPH, &[claim])
            .expect_err("descriptive subject cannot be minted generically");
        assert!(err.0.contains("descriptive"), "{}", err.0);
    }

    #[test]
    fn virtual_class_records_are_rejected_before_subject_minting() {
        let contract = get_vocabulary("workflow").unwrap();
        let live = record(json!({
            "kind": "LiveRecommendedAction",
            "localId": "live-recommendation",
            "intent": "analysis-scout",
            "recommendedChoice": "adventure-trail",
            "choiceLabel": "Inspect retained PageViews",
            "recommendationSource": "garden-intent-rule",
            "rationale": "derived opinion"
        }));
        let err = plan_generic_compute(contract, GRAPH, &[live])
            .expect_err("virtual classes are read-resolved, not materialized");
        assert!(
            err.0.contains("LiveRecommendedAction")
                && err.0.contains("store_mode=virtual")
                && err.0.contains("cannot be materialized"),
            "{}",
            err.0
        );
    }

    /// LOAD-BEARING PROOF for THIS fork (T5.22): `plan_generic_compute`'s
    /// virtual-rejection is derived from `class_dispatch::resolve`, not a bare
    /// `store_mode` field compare duplicated from `materialized_class_
    /// partitions`. The SAME class name, only the synthetic contract's declared
    /// `store_mode` differing, flips from accepted to rejected.
    #[test]
    fn plan_generic_compute_virtual_rejection_is_derived_from_the_declared_signature() {
        fn synthetic_contract(store_mode: &str) -> VocabularyContract {
            let (identity_kind, source_kind, dispatch_mode, extra) = if store_mode == "virtual" {
                (
                    "resolve-by-query",
                    "derived",
                    "derived-virtual",
                    json!({"derived_from_query": "demo.widget(test slice)"}),
                )
            } else {
                (
                    "urn-template",
                    "current-state",
                    "current-state-materialize",
                    json!({}),
                )
            };
            let mut class = json!({
                "rdf_types": ["demo:Widget"],
                "subject_rule": "{graph_subject}:projection:demo:{localId}",
                "source_kind": source_kind,
                "identity_kind": identity_kind,
                "store_mode": store_mode,
                "store_target": if store_mode == "virtual" { "none" } else { "projection:demo" },
                "enforcement": "halt",
                "reconciliation_strategy": "codeBacked",
                "dispatch_mode": dispatch_mode,
                "predicates": {
                    "demo:label": {"datatype": "string", "required": false}
                }
            });
            class
                .as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            serde_json::from_value(json!({
                "name": "demo-generic",
                "version": "1.0.0",
                "title": "Demo Generic",
                "description": "demo",
                "namespaces": {
                    "demo": "http://example.test/demo#",
                    "rdf": "http://www.w3.org/1999/02/22-rdf-syntax-ns#",
                    "xsd": "http://www.w3.org/2001/XMLSchema#"
                },
                "primary_prefix": "demo",
                "write_target": "projection:demo",
                "classes": { "Widget": class }
            }))
            .expect("synthetic contract parses")
        }

        let widget = record(json!({"kind": "Widget", "localId": "w1", "label": "hi"}));

        let materialize_contract = synthetic_contract("materialize");
        let plan = plan_generic_compute(&materialize_contract, GRAPH, &[widget.clone()])
            .expect("materialize-shaped class is accepted");
        assert_eq!(plan.mode, "simple-projection");

        let virtual_contract = synthetic_contract("virtual");
        let err = plan_generic_compute(&virtual_contract, GRAPH, &[widget])
            .expect_err("the SAME class, now declared virtual, must be rejected");
        assert!(
            err.0.contains("Widget") && err.0.contains("store_mode=virtual"),
            "{}",
            err.0
        );
    }

    #[test]
    fn wf_agent_session_projection_generic_plan_carries_realization_anchors() {
        const RDF_TYPE: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";
        const AGT: &str = "http://mnemosyne.dev/agent#";
        const WF: &str = "http://mnemosyne.dev/workflow#";

        fn has(triples: &[Triple], subject: &str, predicate: &str, object_nt: &str) -> bool {
            triples
                .iter()
                .any(|(s, p, o)| s == subject && p == predicate && o.as_nt() == object_nt)
        }

        let contract = get_vocabulary("wf-agent-session-projection").unwrap();
        let agent = "urn:sophia:agent:agent-1a2b3c4d5e6f7a8b";
        let voice = "urn:sophia:agent:agent-1a2b3c4d5e6f7a8b:voice:monovocal";
        // §WS1 relevel: the wf:Run anchor is realizedBy the agt:Run (was the Session);
        // the agt:Run nests under the session, the turn nests under the agt:Run.
        let wf_run = "urn:sophia:wf-run:wfr-project";
        let wf_agent_run = "urn:sophia:wf-run:wfr-project:agent:map-d1";
        let session = "urn:sophia:agent:agent-1a2b3c4d5e6f7a8b:session:s-proj";
        let run_episode = "urn:sophia:agent:agent-1a2b3c4d5e6f7a8b:session:s-proj:run:wfr-project";
        let turn = "urn:sophia:agent:agent-1a2b3c4d5e6f7a8b:session:s-proj:run:wfr-project:turn:1";
        let tool = "urn:sophia:agent:agent-1a2b3c4d5e6f7a8b:tool:remember";
        let records = vec![
            record(json!({"kind": "WorkflowRunAnchor", "localId": wf_run})),
            record(json!({"kind": "WorkflowAgentRunAnchor", "localId": wf_agent_run})),
            record(json!({
                "kind": "Agent", "localId": agent, "agentId": "agent-1a2b3c4d5e6f7a8b",
                "voicing": format!("{AGT}Monovocal"), "hasVoice": [voice], "model": "cheap"
            })),
            record(
                json!({"kind": "Voice", "localId": voice, "voiceOf": agent, "voiceId": "monovocal"}),
            ),
            // §WS1: the run-grain Session carries NO realizedBy now.
            record(json!({
                "kind": "Session", "localId": session, "ofAgent": agent,
                "sessionId": "s-proj", "ownerUserId": "owner-1", "graphId": "lab",
                "model": "cheap", "turnCount": 1
            })),
            // the agt:Run owns the realizedBy → wf:Run edge + the inference binding
            // (generic-record field names: provider/model, NOT inferenceProvider/Model).
            record(json!({
                "kind": "Run", "localId": run_episode, "ofAgent": agent, "ofSession": session,
                "realizedBy": wf_run, "runId": "wfr-project", "ownerUserId": "owner-1",
                "graphId": "lab", "provider": "deepseek", "model": "cheap", "turnCount": 1
            })),
            record(json!({
                "kind": "Turn", "localId": turn, "inSession": session, "inRun": run_episode,
                "realizedBy": wf_agent_run, "ordinal": 1, "role": "node", "turnGrain": "node",
                "nodeLabel": "map:d1", "phaseIndex": 1, "state": "done",
                "turnTime": 1782200000000_i64, "usedTool": [tool]
            })),
            record(json!({
                "kind": "Tool", "localId": tool, "ofAgent": agent,
                "toolName": "remember", "toolStatus": "used", "toolAccess": "write"
            })),
        ];

        let plan = plan_generic_compute(contract, GRAPH, &records).expect("plan");
        assert_eq!(plan.mode, "simple-projection");
        assert!(plan.routes_to_simple_projection());
        assert!(has(
            &plan.desired_inserts,
            wf_run,
            RDF_TYPE,
            &format!("<{WF}Run>")
        ));
        assert!(has(
            &plan.desired_inserts,
            wf_agent_run,
            RDF_TYPE,
            &format!("<{WF}AgentRun>")
        ));
        // The realizedBy edge moved Session→Run: the agt:Run realizes the wf:Run.
        assert!(has(
            &plan.desired_inserts,
            run_episode,
            RDF_TYPE,
            &format!("<{AGT}Run>")
        ));
        assert!(has(
            &plan.desired_inserts,
            run_episode,
            &format!("{AGT}realizedBy"),
            &format!("<{wf_run}>")
        ));
        // The generic-record `provider`/`model` fields materialize as agt:provider/model.
        assert!(has(
            &plan.desired_inserts,
            run_episode,
            &format!("{AGT}provider"),
            "\"deepseek\""
        ));
        // The Turn nests under its agt:Run via agt:inRun.
        assert!(has(
            &plan.desired_inserts,
            turn,
            &format!("{AGT}inRun"),
            &format!("<{run_episode}>")
        ));
        assert!(has(
            &plan.desired_inserts,
            turn,
            &format!("{AGT}realizedBy"),
            &format!("<{wf_agent_run}>")
        ));
        assert!(has(
            &plan.desired_inserts,
            turn,
            &format!("{AGT}turnGrain"),
            "\"node\""
        ));
        assert!(has(
            &plan.desired_inserts,
            agent,
            &format!("{AGT}hasVoice"),
            &format!("<{voice}>")
        ));
    }
}

#[cfg(test)]
mod parity;
