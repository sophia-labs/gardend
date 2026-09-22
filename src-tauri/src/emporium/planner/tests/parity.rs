//! EXECUTED parity against the standalone Meaningful-Objects prototype's CLAIMED
//! reconciliation-algebra deltas (caveat #2: prove the algebra against the REAL
//! emporium engine, not by reading `planner.rs` line-for-line).
//!
//! These tests run the REAL planner (`plan_compute` / `plan_memory_compute`) on
//! the REAL contracts (`workflow_vocabulary()` / `memory_core_vocabulary()`) with
//! real fixture-derived inputs, parse the emitted `DELETE DATA` / `INSERT DATA`
//! SPARQL bodies back into typed triples, and assert the resulting (removes, adds)
//! canon-sets EQUAL the deltas the prototype's `tests/anchors.rs` asserts for the
//! same two anchors. NO mocks; the prototype crate is NOT linked — we only assert
//! the real planner reproduces the prototype's CLAIMED algebra.
//!
//! Parity targets (verbatim from prototypes/meaningful-objects/tests/anchors.rs):
//!   * anchor2_intent_change_rebuilds_managed_without_drift — change one managed
//!     `wf:description` value ⇒ dels == {(demo, wf:description, OLD)},
//!     ins ⊇ {(demo, wf:description, NEW)}, nothing else churns; converged re-plan
//!     ⇒ ZERO ops; a foreign-namespace triple on a managed subject is FENCED OUT
//!     of removes.
//!   * anchor3_supersession_is_ordinary_diff_over_augmented_desired — file head H,
//!     then H′ with supersedesRef=H ⇒ removes ⊇ {(H,status,active),(H,isCurrent,
//!     true)}, adds ⊇ {(H,status,superseded),(H,isCurrent,false),(H,supersededBy,
//!     H′),(H′,supersedes,H)}, every non-lifecycle triple of H in NEITHER set
//!     (append-only carry-forward); a re-file against the post-state ⇒ ZERO ops.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::json;

// `super` is the `tests` module; its `use super::*` glob does NOT re-export the
// names IT imported, so a child module must re-import the types/macros/items it
// needs. The plain functions/consts DEFINED in tests.rs (converged_state, plan,
// parsed*, judgment, mem_live, sample_record, parse_update_triples,
// collect_insert_triples, PREFIX, …) ARE visible to this child via `super::*`.
use super::*;
use crate::emporium::contract::{memory_core_vocabulary, workflow_vocabulary};
use crate::emporium::planner::{memory_record_subject, plan_memory_compute, Plan, Step};
use crate::emporium::schemas::{MemoryRecordIn, ParsedWorkflow};
use crate::emporium::terms::{canon, CanonValue, Triple};

type CanonKey = (String, String, CanonValue);

fn canon_set(triples: &[Triple]) -> BTreeSet<CanonKey> {
    triples.iter().map(canon).collect()
}

/// Split a real plan's steps into (delete-triples, insert-triples) by verb,
/// parsing the N-Triples bodies back with the module's `parse_update_triples`
/// (no placeholder substitution — these vocabs mint no placeholders into the
/// diff). Mirrors the prototype's `split_steps`.
fn split_steps(plan: &Plan) -> (Vec<Triple>, Vec<Triple>) {
    let empty = BTreeMap::new();
    let mut dels = Vec::new();
    let mut ins = Vec::new();
    for st in &plan.steps {
        if let Step::SparqlUpdate { update } = st {
            if update.starts_with("DELETE DATA") {
                dels.extend(parse_update_triples(update, &empty));
            } else if update.starts_with("INSERT DATA") {
                ins.extend(parse_update_triples(update, &empty));
            }
        }
    }
    (dels, ins)
}

/// The DELETE steps must all precede the INSERT steps (load-bearing for a
/// same-subject value-canonical flip). Mirrors the prototype's
/// `assert_delete_before_insert`.
fn assert_delete_before_insert(plan: &Plan) {
    let mut seen_insert = false;
    for st in &plan.steps {
        if let Step::SparqlUpdate { update } = st {
            if update.starts_with("INSERT DATA") {
                seen_insert = true;
            } else if update.starts_with("DELETE DATA") {
                assert!(!seen_insert, "a DELETE step followed an INSERT step");
            }
        }
    }
}

// ===========================================================================
// ANCHOR 2 — REAL workflow update-mode rebuild vs prototype's claimed delta.
// ===========================================================================

/// Change exactly one managed value (`wf:description` on the wf-demo Workflow
/// subject) and assert the REAL `plan_compute` delta is EXACTLY:
///   dels == { (wf-demo, wf:description, "d") }   (old value, managed wf: pred)
///   ins  ⊇  { (wf-demo, wf:description, "RENAMED") }
/// with NOTHING ELSE churning — the prototype's anchor2 claim.
#[test]
fn anchor2_real_planner_intent_change_rebuilds_only_managed_value() {
    let (live2, triples, wires, docs_md, prov) = converged_state();

    // Mutate the parsed def's description ("d" → "RENAMED"). This is the managed
    // wf:description value on the Workflow subject.
    let mut pj = parsed_json();
    pj["description"] = json!("RENAMED");
    let p: ParsedWorkflow = serde_json::from_value(pj.clone()).unwrap();
    let j = judgment();

    let plan = plan(&p, &pj, Some(&j), &live2, &triples, &wires, &docs_md, &prov);
    assert_eq!(
        plan.mode, "update",
        "an existing workflow re-plans in update-mode"
    );
    assert_delete_before_insert(&plan);

    let (dels, ins) = split_steps(&plan);

    let demo = format!("{PREFIX}:doc:wf-demo");
    let wfns = workflow_vocabulary().primary_namespace();
    let desc_p = format!("{wfns}description");

    // The OLD wf:description "d" is the ONLY stale triple (managed wf: predicate →
    // fence passes; nothing else changed).
    assert_eq!(
        dels.len(),
        1,
        "exactly the changed managed triple is stale, got dels={dels:?}"
    );
    assert_eq!(dels[0].0, demo, "stale triple is on the wf-demo subject");
    assert_eq!(dels[0].1, desc_p, "stale triple is wf:description");
    assert_eq!(
        canon(&dels[0]).2,
        CanonValue::Lit("d".into()),
        "the OLD description value 'd' is deleted"
    );

    // The new value is inserted (and is the ONLY insert).
    assert!(
        ins.iter().any(|t| t.0 == demo
            && t.1 == desc_p
            && canon(t).2 == CanonValue::Lit("RENAMED".into())),
        "the new wf:description 'RENAMED' must be inserted, got ins={ins:?}"
    );
    assert_eq!(
        ins.len(),
        1,
        "only the changed triple is missing → exactly one insert, got ins={ins:?}"
    );
}

/// A converged re-plan (live == desired) yields ZERO rdf ops — the prototype's
/// `anchor2_converged_state_is_zero_ops`, against the REAL planner.
#[test]
fn anchor2_real_planner_converged_state_is_zero_ops() {
    let (live2, triples, wires, docs_md, prov) = converged_state();
    let p = parsed();
    let pj = parsed_json();
    let j = judgment();
    let plan = plan(&p, &pj, Some(&j), &live2, &triples, &wires, &docs_md, &prov);
    let (dels, ins) = split_steps(&plan);
    assert!(
        dels.is_empty() && ins.is_empty(),
        "converged (live==desired) → zero rdf ops, got dels={dels:?} ins={ins:?}"
    );
    assert_eq!(plan.summary.rdf_delete, 0);
    assert_eq!(plan.summary.rdf_insert, 0);
}

/// The fence: a FOREIGN-namespace triple (nfo:) on a managed, NON-urn doc subject
/// must be FENCED OUT of removes — the prototype's
/// `anchor2_fence_excludes_foreign_namespace_on_managed_subject`, against the REAL
/// `is_managed_pred` fence in `plan_compute` (planner.rs ~1037-1054).
#[test]
fn anchor2_real_planner_fence_excludes_foreign_namespace_on_managed_subject() {
    let (live2, mut triples, wires, docs_md, prov) = converged_state();

    // Seed live with a foreign nfo: triple on the managed wf-demo doc subject
    // (NOT a urn:sophia:wf subject; nfo: is neither wf: nor prov:).
    let demo = format!("{PREFIX}:doc:wf-demo");
    let foreign_pred =
        "http://www.semanticdesktop.org/ontologies/2007/03/22/nfo#fileName".to_string();
    triples.push((
        demo.clone(),
        foreign_pred.clone(),
        crate::emporium::survey::parse_term("\"foreign.txt\""),
    ));

    // Re-plan the unchanged (converged) def: the only live/desired difference is
    // the foreign triple, which the fence must drop from removes.
    let p = parsed();
    let pj = parsed_json();
    let j = judgment();
    let plan = plan(&p, &pj, Some(&j), &live2, &triples, &wires, &docs_md, &prov);
    let (dels, _ins) = split_steps(&plan);

    assert!(
        !dels.iter().any(|t| t.1 == foreign_pred),
        "the foreign nfo: triple on a managed subject must be FENCED OUT of removes, got dels={dels:?}"
    );
    assert!(
        dels.is_empty(),
        "only the fenced foreign triple differed → no deletes at all, got dels={dels:?}"
    );
}

// ===========================================================================
// ANCHOR 3 — REAL memory supersession vs prototype's claimed delta.
// ===========================================================================

/// A second memory head that differs only in content (so it content-hashes to a
/// distinct subject) and explicitly supersedes a prior head. The producer-directed
/// `supersedes_ref` is what drives the demote pass.
fn corrected_record(supersedes: Option<String>) -> MemoryRecordIn {
    let mut r = sample_record();
    r.client_ref = Some("r-1".to_string());
    r.content = "vera prefers zsh".to_string();
    r.supersedes_ref = supersedes;
    r
}

/// File head H (active, isCurrent=true), then file H′ with supersedesRef=H against
/// a live projection containing H's minted triples. Assert the REAL
/// `plan_memory_compute` delta matches the prototype's anchor3 claim, term-for-term
/// at the canon boundary.
#[test]
fn anchor3_real_planner_supersession_is_ordinary_diff_over_augmented_desired() {
    let contract = memory_core_vocabulary();
    let graph = "lab";

    // ── file H on a clean graph; H's minted triples become the live projection ──
    let h_record = sample_record();
    let h_subject = memory_record_subject(graph, &h_record);
    let plan_h =
        plan_memory_compute(contract, graph, &[h_record.clone()], &mem_live(), &[]).unwrap();
    let live_mem = collect_insert_triples(&plan_h);

    // Sanity: H is filed active + current in the live projection.
    let mem = contract.primary_namespace();
    let status_p = format!("{mem}status");
    let iscur_p = format!("{mem}isCurrent");
    assert!(
        live_mem.iter().any(|t| t.0 == h_subject
            && t.1 == status_p
            && canon(t).2 == CanonValue::Lit("active".into())),
        "H must be filed mem:status=active"
    );
    assert!(
        live_mem
            .iter()
            .any(|t| t.0 == h_subject && t.1 == iscur_p && canon(t).2 == CanonValue::Bool(true)),
        "H must be filed mem:isCurrent=true"
    );

    // ── file H′ with supersedesRef = H against the live projection ──
    let hp_record = corrected_record(Some(h_subject.clone()));
    let hp_subject = memory_record_subject(graph, &hp_record);
    assert_ne!(
        h_subject, hp_subject,
        "changed content must re-mint a distinct subject"
    );

    // The folder already exists from H's file (suppress the folder op so the diff
    // is the pure supersession algebra).
    let mut live2 = mem_live();
    live2.folders.insert(
        MEMORY_FOLDER_ID_FOR_TEST.to_string(),
        crate::emporium::survey::FolderEntry {
            label: "Memory".to_string(),
            parent_id: None,
        },
    );

    let plan_hp =
        plan_memory_compute(contract, graph, &[hp_record.clone()], &live2, &live_mem).unwrap();
    assert!(
        plan_hp.warnings.is_empty(),
        "H is a live target → no dangling-supersedesRef warning, got {:?}",
        plan_hp.warnings
    );
    assert_eq!(plan_hp.mode, "memory");
    assert_delete_before_insert(&plan_hp);

    let (dels, ins) = split_steps(&plan_hp);
    let del = canon_set(&dels);
    let add = canon_set(&ins);

    let supby_p = format!("{mem}supersededBy");
    let sup_p = format!("{mem}supersedes");

    // removes ⊇ {(H,status,active),(H,isCurrent,true)}.
    assert!(
        del.contains(&(
            h_subject.clone(),
            status_p.clone(),
            CanonValue::Lit("active".into())
        )),
        "removes must contain (H, mem:status, active); del={del:?}"
    );
    assert!(
        del.contains(&(h_subject.clone(), iscur_p.clone(), CanonValue::Bool(true))),
        "removes must contain (H, mem:isCurrent, true); del={del:?}"
    );

    // adds ⊇ {(H,status,superseded),(H,isCurrent,false),(H,supersededBy,H′),(H′,supersedes,H)}.
    assert!(
        add.contains(&(
            h_subject.clone(),
            status_p.clone(),
            CanonValue::Lit("superseded".into())
        )),
        "adds must contain (H, mem:status, superseded); add={add:?}"
    );
    assert!(
        add.contains(&(h_subject.clone(), iscur_p.clone(), CanonValue::Bool(false))),
        "adds must contain (H, mem:isCurrent, false); add={add:?}"
    );
    assert!(
        add.contains(&(
            h_subject.clone(),
            supby_p.clone(),
            CanonValue::Uri(hp_subject.clone())
        )),
        "adds must contain (H, mem:supersededBy, H′); add={add:?}"
    );
    assert!(
        add.contains(&(
            hp_subject.clone(),
            sup_p.clone(),
            CanonValue::Uri(h_subject.clone())
        )),
        "adds must contain (H′, mem:supersedes, H); add={add:?}"
    );

    // APPEND-ONLY carry-forward proof: every NON-lifecycle triple of H is in
    // NEITHER set (carried forward → on both sides → zero ops on those (s,p)).
    for (s, p, _) in &live_mem {
        if s == &h_subject && p != &status_p && p != &iscur_p && p != &supby_p {
            assert!(
                !del.iter().any(|d| &d.0 == s && &d.1 == p),
                "non-lifecycle triple ({s}, {p}) of H must NOT be deleted; del={del:?}"
            );
            assert!(
                !add.iter().any(|a| &a.0 == s && &a.1 == p),
                "non-lifecycle triple ({s}, {p}) of H must NOT be re-inserted; add={add:?}"
            );
        }
    }
}

/// A re-file of H′ against the post-supersession state ⇒ ZERO ops — the
/// prototype's `anchor3_refile_against_post_state_is_zero_ops`, against the REAL
/// planner. The post-state is exactly what a real apply of the supersession plan
/// produces: H demoted + H′ filed.
#[test]
fn anchor3_real_planner_refile_against_post_state_is_zero_ops() {
    let contract = memory_core_vocabulary();
    let graph = "lab";

    // Build the post-supersession live projection by applying H's file, then H′'s
    // supersession plan, to a single accumulating triple store (the diff says: of
    // the H-scoped triples, delete the stale lifecycle ones and insert the demoted
    // ones; H′'s triples are all inserts).
    let h_record = sample_record();
    let h_subject = memory_record_subject(graph, &h_record);
    let plan_h = plan_memory_compute(contract, graph, &[h_record], &mem_live(), &[]).unwrap();
    let mut store = collect_insert_triples(&plan_h);

    let hp_record = corrected_record(Some(h_subject.clone()));
    let mut live2 = mem_live();
    live2.folders.insert(
        MEMORY_FOLDER_ID_FOR_TEST.to_string(),
        crate::emporium::survey::FolderEntry {
            label: "Memory".to_string(),
            parent_id: None,
        },
    );
    let plan_hp =
        plan_memory_compute(contract, graph, &[hp_record.clone()], &live2, &store).unwrap();
    apply_diff_to_store(&mut store, &plan_hp);

    // Re-file H′ identically against the post-state → ZERO ops (Case B convergence).
    let replan = plan_memory_compute(contract, graph, &[hp_record], &live2, &store).unwrap();
    let (dels, ins) = split_steps(&replan);
    assert!(
        dels.is_empty() && ins.is_empty(),
        "re-file of a correction must converge to ZERO ops, got dels={dels:?} ins={ins:?}"
    );
    assert_eq!(replan.summary.rdf_delete, 0);
    assert_eq!(replan.summary.rdf_insert, 0);
}

/// Apply a plan's DELETE/INSERT-DATA diff to an in-memory triple store the way a
/// real apply would: drop value-canonically-matching stale triples, then add the
/// missing ones. Keeps the store at the post-apply projection.
fn apply_diff_to_store(store: &mut Vec<Triple>, plan: &Plan) {
    let (dels, ins) = split_steps(plan);
    let del_keys: BTreeSet<CanonKey> = canon_set(&dels);
    store.retain(|t| !del_keys.contains(&canon(t)));
    let cur_keys: BTreeSet<CanonKey> = store.iter().map(canon).collect();
    for t in ins {
        if !cur_keys.contains(&canon(&t)) {
            store.push(t);
        }
    }
}

/// The memory registry folder id (mirrors the private `MEMORY_FOLDER_ID` in
/// `plan_memory_compute`); duplicated here so the post-file live state suppresses
/// the folder op without reaching the planner's private const.
const MEMORY_FOLDER_ID_FOR_TEST: &str = "memory";
