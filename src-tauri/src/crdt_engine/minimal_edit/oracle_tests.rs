//! The tree-edit oracle (DESIGN §2.15): laws T1-T6 over the shared corpus
//! `prototypes/ontology-to-lean/differential/corpus/tree-edit.json`.
//!
//! The Lean model proves the triple-reconcile laws L1-L5; the tree engine gets the same shape of laws, checked
//! here by a Rust evaluator over a corpus a Lean side can read later (Q7). Every scenario builds `current` with
//! the real builder, derives the desired fragment from TipTap nodes (or from markdown through the real
//! write-path parser and block-contract normaliser), plans with the real planner, emits with the real emitter,
//! and checks:
//!
//! - T1 apply after plan reaches the desired document (normal form; markdown through one normalising round trip);
//! - T2 plan(x, x) is a no-op and emits zero Yjs operations;
//! - T3 an empty plan leaves the state vector unchanged;
//! - T4 the plan is a pure function of its inputs (and, with a fixed client id, so are the update bytes);
//! - T5 block-disjoint edits compose: an id-anchored block's `Patch` equals `plan_node` of that block alone;
//! - T6 identity: kept and patched nodes keep their Yjs identity (BranchID);
//! - I9 step streams consume every current child exactly once.
//!
//! The scenario's own `expect` block pins the counts the design promises (summary, operations, kept ids).
//! `emit_rust_tree_edit_delta` writes `out/rust-tree-edit.json` (git-ignored) in the tagged form of
//! `emporium::terms::diff_oracle`: a list of `{name, delta}`.
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use serde_json::{json, Value};
use yrs::updates::decoder::Decode;
use yrs::{
    Any, BranchID, Doc, Out, ReadTxn, Transact, Update, Xml, XmlFragment, XmlFragmentRef, XmlOut,
};

use super::attrs::normal_form;
use super::emit::emit_fragment;
use super::model::{desired_from_tiptap_nodes, read_fragment};
use super::plan::{plan_fragment, plan_node};
use super::types::{
    CurrentChild, CurrentFragment, DesiredChild, DesiredFragment, EmitStats, FragmentPlan,
    IdPolicy, SourceFormat, Step, WritePolicy,
};
use crate::crdt_engine::{block_contract, builder, content_parse, markdown_export, projection};

const CORPUS: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../prototypes/ontology-to-lean/differential/corpus/tree-edit.json"
));

/// A fixed writer id, so two builds of one scenario encode to the same bytes (T4).
const CLIENT_ID: u64 = 7;

fn corpus() -> Vec<Value> {
    let parsed: Value = serde_json::from_str(CORPUS).expect("tree-edit corpus parses");
    assert_eq!(
        parsed["schema"], "sophia.tree-edit-corpus.v1",
        "corpus schema tag changed"
    );
    parsed["scenarios"]
        .as_array()
        .cloned()
        .expect("corpus holds a scenarios array")
}

// ---------------------------------------------------------------------------
// Building documents and desired content
// ---------------------------------------------------------------------------

fn array_of(value: &Value) -> Vec<Value> {
    value.as_array().cloned().unwrap_or_default()
}

fn doc_of(nodes: &[Value]) -> Value {
    json!({"type": "doc", "content": nodes})
}

fn content_of(doc: &Value) -> Vec<Value> {
    array_of(&doc["content"])
}

/// A Doc built the way the builder builds one, with a fixed client id.
fn doc_from_nodes(nodes: &[Value]) -> Doc {
    let doc = Doc::with_client_id(CLIENT_ID);
    let fragment = doc.get_or_insert_xml_fragment("content");
    {
        let mut txn = doc.transact_mut();
        builder::append_nodes(&mut txn, &fragment, nodes);
    }
    doc
}

fn collect_id_strings(node: &Value, out: &mut BTreeSet<String>) {
    if let Some(attrs) = node.get("attrs") {
        for key in ["data-block-id", "blockId"] {
            if let Some(id) = attrs.get(key).and_then(Value::as_str) {
                if !id.is_empty() {
                    out.insert(id.to_string());
                }
            }
        }
    }
    for child in node["content"].as_array().into_iter().flatten() {
        collect_id_strings(child, out);
    }
}

fn id_strings(nodes: &[Value]) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for node in nodes {
        collect_id_strings(node, &mut out);
    }
    out
}

fn replace_block_text(node: &mut Value, id: &str, text: &str) -> bool {
    if node["attrs"]["data-block-id"].as_str() == Some(id) {
        node["content"] = json!([{"type": "text", "text": text}]);
        return true;
    }
    if let Some(children) = node.get_mut("content").and_then(Value::as_array_mut) {
        for child in children {
            if replace_block_text(child, id, text) {
                return true;
            }
        }
    }
    false
}

/// `export(parse(export(x)))`: the observable of a markdown write (DESIGN I8).
fn markdown_normalised(doc: &Value) -> Result<String, String> {
    let exported = markdown_export::tiptap_json_to_markdown(doc);
    let parsed = content_parse::parse_write_content_for_operation(
        &exported,
        Some("markdown"),
        "oracle-md-round-trip",
    )?;
    Ok(markdown_export::tiptap_json_to_markdown(&parsed.tiptap_json))
}

/// The desired nodes of a scenario: TipTap nodes as given, or markdown through the real parser and normaliser.
fn desired_nodes_of(sc: &Value, current: &[Value], name: &str) -> Result<Vec<Value>, String> {
    if sc.get("desired").is_some() {
        return Ok(array_of(&sc["desired"]));
    }
    let markdown = if let Some(markdown) = sc["desiredMarkdown"].as_str() {
        markdown.to_string()
    } else if let Some(edit) = sc.get("desiredMarkdownFromCurrent") {
        let block = edit["editBlock"]
            .as_str()
            .ok_or("desiredMarkdownFromCurrent.editBlock missing")?;
        let text = edit["newText"]
            .as_str()
            .ok_or("desiredMarkdownFromCurrent.newText missing")?;
        let mut doc = doc_of(current);
        if !replace_block_text(&mut doc, block, text) {
            return Err(format!("no block {block} to edit in the current nodes"));
        }
        markdown_export::tiptap_json_to_markdown(&doc)
    } else {
        return Err("scenario states no desired content".to_string());
    };
    let seed = format!("oracle-{name}");
    let parsed =
        content_parse::parse_write_content_for_operation(&markdown, Some("markdown"), &seed)?;
    let nodes = content_of(&parsed.tiptap_json);
    let (nodes, _receipts) = block_contract::normalise_doc_content(&nodes, &seed);
    Ok(nodes)
}

fn policy_of(sc: &Value, desired_nodes: &[Value]) -> Result<WritePolicy, String> {
    let format = match sc["format"].as_str() {
        Some("tiptapJson") => SourceFormat::TiptapJson,
        Some("markdown") => SourceFormat::Markdown,
        Some("restore") => SourceFormat::Restore,
        other => return Err(format!("unknown scenario format {other:?}")),
    };
    let ids = match sc["ids"].as_str() {
        Some("bound") => IdPolicy::Bound,
        Some("adopt") => IdPolicy::Adopt {
            minted: id_strings(desired_nodes),
        },
        other => return Err(format!("unknown scenario ids policy {other:?}")),
    };
    Ok(WritePolicy { format, ids })
}

// ---------------------------------------------------------------------------
// Observing a Doc
// ---------------------------------------------------------------------------

fn branch_id_of<T: AsRef<yrs::branch::Branch>>(item: &T) -> BranchID {
    let branch: &yrs::branch::Branch = item.as_ref();
    branch.id()
}

fn collect_ids<T: ReadTxn, P: XmlFragment>(
    txn: &T,
    parent: &P,
    out: &mut BTreeMap<String, BranchID>,
) {
    for child in parent.children(txn) {
        if let XmlOut::Element(element) = child {
            for key in ["data-block-id", "blockId"] {
                if let Some(Out::Any(Any::String(id))) = element.get_attribute(txn, key) {
                    out.entry(id.to_string())
                        .or_insert_with(|| branch_id_of(&element));
                }
            }
            collect_ids(txn, &element, out);
        }
    }
}

/// Block id -> the Yjs identity of the element holding it, at every depth (first holder wins).
fn ids_of(doc: &Doc, fragment: &XmlFragmentRef) -> BTreeMap<String, BranchID> {
    let txn = doc.transact();
    let mut out = BTreeMap::new();
    collect_ids(&txn, fragment, &mut out);
    out
}

fn top_level_branch_ids(doc: &Doc, fragment: &XmlFragmentRef) -> Vec<BranchID> {
    let txn = doc.transact();
    fragment
        .children(&txn)
        .map(|child| branch_id_of(&child))
        .collect()
}

fn find_block<'a>(node: &'a Value, id: &str) -> Option<&'a Value> {
    if node["attrs"]["data-block-id"].as_str() == Some(id) || node["attrs"]["blockId"].as_str() == Some(id) {
        return Some(node);
    }
    node["content"]
        .as_array()
        .into_iter()
        .flatten()
        .find_map(|child| find_block(child, id))
}

fn node_text(node: &Value) -> String {
    if node["type"] == "text" {
        return node["text"].as_str().unwrap_or("").to_string();
    }
    node["content"]
        .as_array()
        .into_iter()
        .flatten()
        .map(node_text)
        .collect()
}

/// Text carrying `commentMark` with `comment_id`, in document order.
fn comment_marked_text(node: &Value, comment_id: &str, out: &mut String) {
    if node["type"] == "text" {
        let marked = node["marks"].as_array().into_iter().flatten().any(|mark| {
            mark["type"] == "commentMark" && mark["attrs"]["commentId"] == comment_id
        });
        if marked {
            out.push_str(node["text"].as_str().unwrap_or(""));
        }
    }
    for child in node["content"].as_array().into_iter().flatten() {
        comment_marked_text(child, comment_id, out);
    }
}

// ---------------------------------------------------------------------------
// One application of the engine
// ---------------------------------------------------------------------------

struct Applied {
    /// The model the plan was made from (handles stay valid for identity checks).
    current: CurrentFragment,
    plan: FragmentPlan,
    /// A second plan from the same inputs (T4).
    replan: FragmentPlan,
    stats: EmitStats,
    /// The transaction's encoded update.
    update: Vec<u8>,
}

fn apply_once(
    doc: &Doc,
    fragment: &XmlFragmentRef,
    desired: &DesiredFragment,
    policy: &WritePolicy,
) -> Result<Applied, String> {
    let mut txn = doc.transact_mut();
    let current = read_fragment(&txn, fragment, policy);
    let plan = plan_fragment(&current, desired, policy);
    let replan = plan_fragment(&current, desired, policy);
    let stats = emit_fragment(&mut txn, fragment, &current, &plan)?;
    let update = txn.encode_update_v1();
    Ok(Applied {
        current,
        plan,
        replan,
        stats,
        update,
    })
}

/// True when the encoded update carries no insertion and no deletion (DESIGN I1).
fn update_is_empty(update: &[u8]) -> Result<bool, String> {
    let decoded = Update::decode_v1(update).map_err(|error| format!("decode update: {error}"))?;
    Ok(decoded.insertions(true).is_empty() && decoded.delete_set().is_empty() && update.len() <= 3)
}

/// T2 + T3: planning a document against its own projection is a no-op, emits nothing and leaves the state
/// vector as it was.
fn check_identity(
    doc: &Doc,
    fragment: &XmlFragmentRef,
    policy: &WritePolicy,
    label: &str,
) -> Vec<String> {
    let mut problems = Vec::new();
    let nodes = content_of(&projection::ydoc_to_tiptap_json(doc));
    let desired = desired_from_tiptap_nodes(&nodes, policy);
    let mut txn = doc.transact_mut();
    let current = read_fragment(&txn, fragment, policy);
    let plan = plan_fragment(&current, &desired, policy);
    if !plan.is_noop() {
        problems.push(format!(
            "T2 {label}: plan(x, x) is not a no-op: {:?}",
            plan.steps
        ));
        return problems;
    }
    let before = txn.state_vector();
    match emit_fragment(&mut txn, fragment, &current, &plan) {
        Err(error) => problems.push(format!("T2 {label}: emitting a no-op plan failed: {error}")),
        Ok(stats) => {
            if !stats.counts.is_zero() {
                problems.push(format!(
                    "T2 {label}: a no-op plan counted operations: {:?}",
                    stats.counts
                ));
            }
        }
    }
    match update_is_empty(&txn.encode_update_v1()) {
        Ok(true) => {}
        Ok(false) => problems.push(format!(
            "T2 {label}: a no-op plan produced a non-empty update"
        )),
        Err(error) => problems.push(format!("T2 {label}: {error}")),
    }
    if txn.state_vector() != before {
        problems.push(format!(
            "T3 {label}: an empty plan changed the state vector"
        ));
    }
    problems
}

/// T5 and I9 over the top-level step stream.
fn check_composition(
    current: &CurrentFragment,
    desired: &DesiredFragment,
    plan: &FragmentPlan,
    policy: &WritePolicy,
) -> Vec<String> {
    let mut problems = Vec::new();
    let mut cursor = 0usize;
    for step in &plan.steps {
        match step {
            Step::Keep | Step::Patch(_) => {
                if let Some(CurrentChild::Element(node)) = current.children.get(cursor) {
                    if let Some(id) = &node.id {
                        let counterpart = desired.children.iter().find_map(|child| match child {
                            DesiredChild::Element(d)
                                if d.id.as_ref() == Some(id) && d.tag == node.tag =>
                            {
                                Some(d)
                            }
                            _ => None,
                        });
                        if let Some(des) = counterpart {
                            let alone = plan_node(node, des, policy);
                            match step {
                                Step::Patch(patch) if alone.as_ref() != Some(patch) => {
                                    problems.push(format!(
                                        "T5: block {id}: the fragment's Patch {patch:?} differs from plan_node alone {alone:?}"
                                    ));
                                }
                                Step::Keep if alone.is_some() => {
                                    problems.push(format!(
                                        "T5: block {id} is kept in the fragment plan but plan_node alone patches it: {alone:?}"
                                    ));
                                }
                                _ => {}
                            }
                        }
                    }
                }
                cursor += 1;
            }
            Step::Text(_) => cursor += 1,
            Step::Remove(n) => cursor += *n as usize,
            Step::InsertNode(_) | Step::InsertText(_) => {}
        }
    }
    if cursor != current.children.len() {
        problems.push(format!(
            "I9: the top-level step stream consumes {cursor} children, the fragment holds {}",
            current.children.len()
        ));
    }
    problems
}

/// T6: every child a step keeps, patches or text-edits is still in the fragment, as itself; every removed one
/// is gone.
fn check_identity_kept(
    doc: &Doc,
    fragment: &XmlFragmentRef,
    current: &CurrentFragment,
    plan: &FragmentPlan,
) -> Vec<String> {
    let mut problems = Vec::new();
    let after = top_level_branch_ids(doc, fragment);
    let child_id = |index: usize| -> Option<BranchID> {
        current.children.get(index).map(|child| match child {
            CurrentChild::Element(node) => branch_id_of(&node.handle),
            CurrentChild::Text(text) => branch_id_of(&text.handle),
        })
    };
    let mut cursor = 0usize;
    for step in &plan.steps {
        match step {
            Step::Keep | Step::Patch(_) | Step::Text(_) => {
                match child_id(cursor) {
                    Some(id) if after.contains(&id) => {}
                    Some(_) => problems.push(format!(
                        "T6: top-level child {cursor} was kept by the plan but is no longer in the fragment"
                    )),
                    None => {}
                }
                cursor += 1;
            }
            Step::Remove(n) => {
                for offset in 0..*n as usize {
                    if let Some(id) = child_id(cursor + offset) {
                        if after.contains(&id) {
                            problems.push(format!(
                                "T6: top-level child {} was removed by the plan but is still in the fragment",
                                cursor + offset
                            ));
                        }
                    }
                }
                cursor += *n as usize;
            }
            Step::InsertNode(_) | Step::InsertText(_) => {}
        }
    }
    problems
}

// ---------------------------------------------------------------------------
// The scenario's own expectations
// ---------------------------------------------------------------------------

fn expect_counts(
    label: &str,
    expected: &Value,
    actual: &Value,
    at_least: bool,
    problems: &mut Vec<String>,
) {
    let Some(map) = expected.as_object() else {
        return;
    };
    for (key, want) in map {
        let want = want.as_u64();
        let got = actual.get(key).and_then(Value::as_u64);
        let ok = match (want, got) {
            (Some(want), Some(got)) if at_least => got >= want,
            (Some(want), Some(got)) => got == want,
            _ => false,
        };
        if !ok {
            problems.push(format!(
                "expect.{label}.{key}: wanted {}{want:?}, got {got:?}",
                if at_least { "at least " } else { "" }
            ));
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn check_expect(
    sc: &Value,
    applied: &Applied,
    before: &BTreeMap<String, BranchID>,
    after: &BTreeMap<String, BranchID>,
    result: &Value,
    policy: &WritePolicy,
) -> Vec<String> {
    let mut problems = Vec::new();
    let expect = &sc["expect"];

    if expect["zeroOps"] == true {
        if !applied.plan.is_noop() {
            problems.push(format!(
                "expect.zeroOps: the plan is not all Keep: {:?}",
                applied.plan.steps
            ));
        }
        if !applied.stats.counts.is_zero() {
            problems.push(format!(
                "expect.zeroOps: operations were counted: {:?}",
                applied.stats.counts
            ));
        }
        match update_is_empty(&applied.update) {
            Ok(true) => {}
            Ok(false) => problems.push("expect.zeroOps: the update is not empty".to_string()),
            Err(error) => problems.push(format!("expect.zeroOps: {error}")),
        }
    }

    for id in array_of(&expect["keptIds"]).iter().filter_map(Value::as_str) {
        match (before.get(id), after.get(id)) {
            (Some(was), Some(is)) if was == is => {}
            (Some(_), Some(_)) => problems.push(format!(
                "expect.keptIds: block {id} now sits on a different Yjs element"
            )),
            (None, _) => problems.push(format!("expect.keptIds: block {id} is not in current")),
            (_, None) => problems.push(format!("expect.keptIds: block {id} is gone after the write")),
        }
    }
    for id in array_of(&expect["reinsertedIds"])
        .iter()
        .filter_map(Value::as_str)
    {
        match (before.get(id), after.get(id)) {
            (_, None) => problems.push(format!(
                "expect.reinsertedIds: block {id} is missing after the write"
            )),
            (Some(was), Some(is)) if was == is => problems.push(format!(
                "expect.reinsertedIds: block {id} kept its Yjs element (a move must re-insert)"
            )),
            _ => {}
        }
    }
    if let Some(pairs) = expect["sameElementAs"].as_object() {
        for (new_id, old_id) in pairs {
            let old_id = old_id.as_str().unwrap_or_default();
            match (after.get(new_id), before.get(old_id)) {
                (Some(is), Some(was)) if is == was => {}
                other => problems.push(format!(
                    "expect.sameElementAs: {new_id} should be the element that held {old_id}, got {other:?}"
                )),
            }
        }
    }
    if expect["insertedIdsAreMinted"] == true {
        let fresh: Vec<&String> = after.keys().filter(|id| !before.contains_key(*id)).collect();
        if fresh.is_empty() {
            problems.push("expect.insertedIdsAreMinted: no new block id appeared".to_string());
        }
        for id in fresh {
            if !policy.id_is_minted(id) {
                problems.push(format!(
                    "expect.insertedIdsAreMinted: new block id {id} was not minted by the parser"
                ));
            }
        }
    }

    let summary = serde_json::to_value(applied.plan.summary).unwrap_or(Value::Null);
    expect_counts("summary", &expect["summary"], &summary, false, &mut problems);
    let counts = serde_json::to_value(applied.stats.counts).unwrap_or(Value::Null);
    expect_counts("ops", &expect["ops"], &counts, false, &mut problems);
    expect_counts("opsAtLeast", &expect["opsAtLeast"], &counts, true, &mut problems);

    if let Some(marked) = expect["commentMarkedText"].as_object() {
        for (comment_id, want) in marked {
            let mut got = String::new();
            comment_marked_text(result, comment_id, &mut got);
            if Some(got.as_str()) != want.as_str() {
                problems.push(format!(
                    "expect.commentMarkedText: comment {comment_id} marks {got:?}, wanted {want}"
                ));
            }
        }
    }
    if let Some(texts) = expect["resultText"].as_object() {
        for (block_id, want) in texts {
            let got = find_block(result, block_id).map(node_text);
            if got.as_deref() != want.as_str() {
                problems.push(format!(
                    "expect.resultText: block {block_id} reads {got:?}, wanted {want}"
                ));
            }
        }
    }
    problems
}

// ---------------------------------------------------------------------------
// The evaluator
// ---------------------------------------------------------------------------

struct ScenarioRun {
    name: String,
    /// One line per violated law or expectation, prefixed with the scenario name.
    failures: Vec<String>,
    /// The tagged delta `emit_rust_tree_edit_delta` writes.
    delta: Value,
}

fn run_scenario(sc: &Value) -> ScenarioRun {
    let name = sc["name"].as_str().unwrap_or("?").to_string();
    let mut failures: Vec<String> = Vec::new();
    let mut note = |law_and_message: String| failures.push(format!("{name}: {law_and_message}"));

    let current_nodes = array_of(&sc["current"]);
    let desired_nodes = match desired_nodes_of(sc, &current_nodes, &name) {
        Ok(nodes) => nodes,
        Err(error) => {
            note(format!("setup: {error}"));
            return ScenarioRun {
                name,
                failures,
                delta: Value::Null,
            };
        }
    };
    let policy = match policy_of(sc, &desired_nodes) {
        Ok(policy) => policy,
        Err(error) => {
            note(format!("setup: {error}"));
            return ScenarioRun {
                name,
                failures,
                delta: Value::Null,
            };
        }
    };
    let desired = desired_from_tiptap_nodes(&desired_nodes, &policy);

    let doc = doc_from_nodes(&current_nodes);
    let fragment = doc.get_or_insert_xml_fragment("content");
    let before = ids_of(&doc, &fragment);

    // T2/T3 on the starting state: the current document planned against itself.
    for problem in check_identity(&doc, &fragment, &policy, "on the current document") {
        note(problem);
    }

    // T4 needs a second, identical document to compare update bytes.
    let twin = doc_from_nodes(&current_nodes);
    let twin_fragment = twin.get_or_insert_xml_fragment("content");

    let applied = match apply_once(&doc, &fragment, &desired, &policy) {
        Ok(applied) => applied,
        Err(error) => {
            note(format!("apply: {error}"));
            return ScenarioRun {
                name,
                failures,
                delta: Value::Null,
            };
        }
    };
    let after = ids_of(&doc, &fragment);
    let result = projection::ydoc_to_tiptap_json(&doc);

    // T1: the result is the desired document.
    if policy.format == SourceFormat::Markdown {
        let want = markdown_normalised(&doc_of(&desired_nodes));
        let got = markdown_normalised(&result);
        match (want, got) {
            (Ok(want), Ok(got)) if want == got => {}
            (Ok(want), Ok(got)) => note(format!(
                "T1: markdown of the result differs from the desired markdown\n  want: {want:?}\n  got:  {got:?}"
            )),
            (Err(error), _) | (_, Err(error)) => note(format!("T1: markdown round trip: {error}")),
        }
    } else {
        let want = normal_form(&doc_of(&desired_nodes), true);
        let got = normal_form(&result, true);
        if want != got {
            note(format!(
                "T1: the result is not the desired document\n  want: {want}\n  got:  {got}"
            ));
        }
    }

    // T4: same inputs, same plan; same client id, same update bytes.
    if applied.plan != applied.replan {
        note(format!(
            "T4: planning twice gave different plans\n  first:  {:?}\n  second: {:?}",
            applied.plan.steps, applied.replan.steps
        ));
    }
    match apply_once(&twin, &twin_fragment, &desired, &policy) {
        Ok(second) => {
            if second.plan != applied.plan {
                note("T4: an identical document planned differently".to_string());
            }
            if second.update != applied.update {
                note("T4: an identical document encoded different update bytes".to_string());
            }
        }
        Err(error) => note(format!("T4: applying to the twin document failed: {error}")),
    }

    // T5 and I9 on the step stream; T6 on the resulting fragment.
    for problem in check_composition(&applied.current, &desired, &applied.plan, &policy) {
        note(problem);
    }
    for problem in check_identity_kept(&doc, &fragment, &applied.current, &applied.plan) {
        note(problem);
    }

    // T2/T3 again on the result: what was just written is a fixed point.
    for problem in check_identity(&doc, &fragment, &policy, "on the result") {
        note(problem);
    }

    for problem in check_expect(sc, &applied, &before, &after, &result, &policy) {
        note(problem);
    }

    let kept: Vec<&String> = before
        .iter()
        .filter(|(id, was)| after.get(*id) == Some(*was))
        .map(|(id, _)| id)
        .collect();
    let observable = if policy.format == SourceFormat::Markdown {
        json!({"markdown": markdown_normalised(&result).unwrap_or_default()})
    } else {
        normal_form(&result, true)
    };
    let delta = json!({
        "summary": applied.plan.summary,
        "ops": applied.stats.counts,
        "zeroOps": applied.plan.is_noop(),
        "keptIds": kept,
        "result": observable,
    });
    ScenarioRun {
        name,
        failures,
        delta,
    }
}

fn delta_of_corpus(scenarios: &[Value]) -> Vec<Value> {
    scenarios
        .iter()
        .map(|sc| {
            let run = run_scenario(sc);
            json!({"name": run.name, "delta": run.delta})
        })
        .collect()
}

// ---------------------------------------------------------------------------
// The tests
// ---------------------------------------------------------------------------

/// Every scenario, every law, one failure line each.
#[test]
fn tree_edit_corpus_laws() {
    let scenarios = corpus();
    assert_eq!(
        scenarios.len(),
        22,
        "tree-edit corpus cardinality changed (it is shared with a future Lean side)"
    );
    let mut names = BTreeSet::new();
    let mut failures: Vec<String> = Vec::new();
    for sc in &scenarios {
        let run = run_scenario(sc);
        assert!(names.insert(run.name.clone()), "duplicate scenario {}", run.name);
        failures.extend(run.failures);
    }
    assert!(
        failures.is_empty(),
        "{} tree-edit law violation(s):\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// The oracle must be able to fail: a wrong expectation on a correct engine is reported.
#[test]
fn oracle_flags_a_wrong_expectation() {
    let scenarios = corpus();
    let mut sc = scenarios
        .iter()
        .find(|sc| sc["name"] == "one-paragraph-changed")
        .cloned()
        .expect("scenario one-paragraph-changed");
    sc["expect"]["zeroOps"] = json!(true);
    sc["expect"]["summary"]["kept"] = json!(2);
    let run = run_scenario(&sc);
    assert!(
        run.failures.iter().any(|f| f.contains("expect.zeroOps")),
        "a change declared zero-op passed: {:?}",
        run.failures
    );
    assert!(
        run.failures.iter().any(|f| f.contains("expect.summary.kept")),
        "a wrong summary passed: {:?}",
        run.failures
    );
}

/// Writes `differential/out/rust-tree-edit.json` (git-ignored) so a Lean side can compare later. The output
/// is a pure function of the corpus: two runs agree.
#[test]
fn emit_rust_tree_edit_delta() {
    let scenarios = corpus();
    let first = delta_of_corpus(&scenarios);
    let second = delta_of_corpus(&scenarios);
    assert_eq!(first, second, "the tree-edit delta is not deterministic");
    for entry in &first {
        assert!(
            entry["delta"].is_object(),
            "scenario {} produced no delta",
            entry["name"]
        );
    }

    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../prototypes/ontology-to-lean/differential/out");
    std::fs::create_dir_all(&dir).expect("mkdir differential/out");
    let path = dir.join("rust-tree-edit.json");
    let body = serde_json::to_vec_pretty(&json!(first)).expect("serialize delta");
    std::fs::write(&path, &body).unwrap_or_else(|error| panic!("write {path:?}: {error}"));
    eprintln!("[tree_edit_oracle] wrote {path:?}");

    let reread: Value = serde_json::from_slice(
        &std::fs::read(&path).unwrap_or_else(|error| panic!("read {path:?}: {error}")),
    )
    .expect("delta file parses");
    assert_eq!(reread.as_array().map(Vec::len), Some(scenarios.len()));
}
