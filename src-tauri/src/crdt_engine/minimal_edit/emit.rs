//! The emitter: executes a FragmentPlan as yrs operations inside the caller's transaction. Decides nothing.
//!
//! Every plan is validated against the model it was made from before the first yrs call: an `Err` from
//! inside `Room::update_doc` evicts the room (rooms.rs:1062-1071), and a yrs call with a bad index or length
//! panics, so a malformed plan must never reach a mutation. A plan that is all `Keep` performs no yrs call.

use serde_json::Value;
use yrs::types::{Attrs, Delta};
use yrs::{
    Any, In, ReadTxn, Text, TransactionMut, Xml, XmlFragment, XmlFragmentRef, XmlTextPrelim,
};

use super::types::{
    CurrentChild, CurrentFragment, CurrentText, EmitStats, FragmentPlan, NodePatch, OpCounts,
    RawMarks, Step, TextOp, TextRun,
};
use crate::crdt_engine::builder::{insert_node_at, json_to_any};

/// Check a plan against the model it was made from before any mutation: step streams consume every current
/// child exactly once, Text steps address Y.XmlText children, byte counts fit and land on char boundaries.
pub(crate) fn validate_plan(current: &CurrentFragment, plan: &FragmentPlan) -> Result<(), String> {
    validate_steps(&current.children, &plan.steps, "fragment")
}

/// Validate, then execute. A plan that is all Keep performs no yrs call at all.
pub(crate) fn emit_fragment(
    txn: &mut TransactionMut<'_>,
    fragment: &XmlFragmentRef,
    current: &CurrentFragment,
    plan: &FragmentPlan,
) -> Result<EmitStats, String> {
    validate_plan(current, plan).map_err(|e| format!("emit_fragment: invalid plan: {e}"))?;

    let client = txn.doc().client_id();
    let clock_before = txn.state_vector().get(&client);
    let mut counts = OpCounts::default();
    if !plan.is_noop() {
        emit_steps(txn, fragment, &current.children, &plan.steps, &mut counts);
    }
    let clock_after = txn.state_vector().get(&client);
    Ok(EmitStats {
        counts,
        client_id: client.get(),
        clock_before,
        clock_after,
    })
}

// ---------------------------------------------------------------------------------------------
// Validation (pure: reads the model and the plan only)
// ---------------------------------------------------------------------------------------------

fn validate_steps(children: &[CurrentChild], steps: &[Step], at: &str) -> Result<(), String> {
    let mut idx = 0usize;
    for (n, step) in steps.iter().enumerate() {
        match step {
            Step::Keep => {
                child_at(children, idx, at, n)?;
                idx += 1;
            }
            Step::Patch(patch) => {
                let child = child_at(children, idx, at, n)?;
                let CurrentChild::Element(node) = child else {
                    return Err(format!("{at} step {n}: Patch addresses a Y.XmlText child"));
                };
                if let Some(sub) = &patch.children {
                    validate_steps(&node.children, sub, &format!("{at}/{}", node.tag))?;
                }
                idx += 1;
            }
            Step::Text(ops) => {
                let child = child_at(children, idx, at, n)?;
                let CurrentChild::Text(text) = child else {
                    return Err(format!("{at} step {n}: Text addresses an element child"));
                };
                validate_text_ops(text, ops).map_err(|e| format!("{at} step {n}: {e}"))?;
                idx += 1;
            }
            Step::Remove(count) => {
                let count = *count as usize;
                if idx + count > children.len() {
                    return Err(format!(
                        "{at} step {n}: Remove({count}) at child {idx} overruns {} children",
                        children.len()
                    ));
                }
                idx += count;
            }
            Step::InsertNode(json) => {
                if !json.is_object() {
                    return Err(format!("{at} step {n}: InsertNode is not a JSON object"));
                }
            }
            Step::InsertText(_) => {}
        }
    }
    if idx != children.len() {
        return Err(format!(
            "{at}: the steps consume {idx} of {} children",
            children.len()
        ));
    }
    Ok(())
}

fn child_at<'a>(
    children: &'a [CurrentChild],
    idx: usize,
    at: &str,
    step: usize,
) -> Result<&'a CurrentChild, String> {
    children.get(idx).ok_or_else(|| {
        format!(
            "{at} step {step}: no child at {idx} (the fragment has {})",
            children.len()
        )
    })
}

fn validate_text_ops(text: &CurrentText, ops: &[TextOp]) -> Result<(), String> {
    let mut pos: u64 = 0;
    for op in ops {
        let advance = match op {
            TextOp::Retain(n) => *n,
            TextOp::Format { bytes, .. } => *bytes,
            TextOp::Delete(n) => *n,
            TextOp::Insert { .. } => continue,
        };
        pos += u64::from(advance);
        if pos > u64::from(text.byte_len) {
            return Err(format!(
                "text delta reaches byte {pos} of a {}-byte text",
                text.byte_len
            ));
        }
        if !on_char_boundary(text, pos as u32) {
            return Err(format!("text delta ends inside a character at byte {pos}"));
        }
    }
    Ok(())
}

/// Is byte offset `pos` of the text a char boundary? The runs hold the string chunks in order; a run
/// edge is always a boundary, so only the run that contains `pos` needs a look.
fn on_char_boundary(text: &CurrentText, pos: u32) -> bool {
    let mut start = 0u32;
    for run in &text.runs {
        let end = start + run.text.len() as u32;
        if pos < end {
            return run.text.is_char_boundary((pos - start) as usize);
        }
        start = end;
    }
    // At or past the end of every run: the end is a boundary (the length check already ran).
    true
}

// ---------------------------------------------------------------------------------------------
// Execution
// ---------------------------------------------------------------------------------------------

/// Walk one parent's step stream with a live child cursor `at`. `idx` follows the *current* children list
/// (what the plan was made from); `at` follows the parent's live children, which inserts and removes move.
fn emit_steps<P: XmlFragment>(
    txn: &mut TransactionMut<'_>,
    parent: &P,
    children: &[CurrentChild],
    steps: &[Step],
    counts: &mut OpCounts,
) {
    let mut at = 0u32;
    let mut idx = 0usize;
    for step in steps {
        match step {
            Step::Keep => {
                at += 1;
                idx += 1;
            }
            Step::Patch(patch) => {
                if let Some(CurrentChild::Element(node)) = children.get(idx) {
                    emit_patch(txn, &node.handle, &node.children, patch, counts);
                }
                at += 1;
                idx += 1;
            }
            Step::Text(ops) => {
                if let Some(CurrentChild::Text(text)) = children.get(idx) {
                    emit_text_ops(txn, text, ops, counts);
                }
                at += 1;
                idx += 1;
            }
            Step::Remove(count) => {
                if *count > 0 {
                    parent.remove_range(txn, at, *count);
                    counts.nodes_removed += *count;
                    idx += *count as usize;
                }
            }
            Step::InsertNode(json) => {
                insert_node_at(txn, parent, at, json);
                let created = count_json_nodes(json, counts);
                counts.nodes_inserted += created;
                at += 1;
            }
            Step::InsertText(runs) => {
                if emit_insert_text(txn, parent, at, runs, counts) {
                    at += 1;
                }
            }
        }
    }
}

fn emit_patch(
    txn: &mut TransactionMut<'_>,
    handle: &yrs::XmlElementRef,
    children: &[CurrentChild],
    patch: &NodePatch,
    counts: &mut OpCounts,
) {
    for (key, value) in &patch.set {
        if value.is_null() {
            // A null is an absent attribute everywhere in this engine.
            handle.remove_attribute(txn, key);
            counts.attrs_removed += 1;
        } else {
            handle.insert_attribute(txn, key.clone(), json_to_any(value));
            counts.attrs_set += 1;
        }
    }
    for key in &patch.remove {
        handle.remove_attribute(txn, key);
        counts.attrs_removed += 1;
    }
    if let Some(sub) = &patch.children {
        emit_steps(txn, handle, children, sub, counts);
    }
}

/// Count the Y types an `InsertNode` creates (each element, each non-empty text node) and the bytes of
/// text it writes. Returns the node count; the text bytes go straight into `counts`.
fn count_json_nodes(node: &Value, counts: &mut OpCounts) -> u32 {
    if node.get("type").and_then(Value::as_str) == Some("text") {
        let text = node.get("text").and_then(Value::as_str).unwrap_or("");
        if text.is_empty() {
            return 0;
        }
        counts.text_bytes_inserted += text.len() as u32;
        return 1;
    }
    let mut total = 1;
    if let Some(content) = node.get("content").and_then(Value::as_array) {
        for child in content {
            total += count_json_nodes(child, counts);
        }
    }
    total
}

fn emit_text_ops(
    txn: &mut TransactionMut<'_>,
    text: &CurrentText,
    ops: &[TextOp],
    counts: &mut OpCounts,
) {
    let mut delta: Vec<Delta<In>> = Vec::with_capacity(ops.len());
    for op in ops {
        match op {
            TextOp::Retain(0) | TextOp::Delete(0) => {}
            TextOp::Retain(n) => delta.push(Delta::Retain(*n, None)),
            TextOp::Format { bytes, set, remove } => {
                if *bytes == 0 {
                    continue;
                }
                if set.is_empty() && remove.is_empty() {
                    delta.push(Delta::Retain(*bytes, None));
                    continue;
                }
                let mut attrs = Attrs::new();
                for (name, value) in set {
                    attrs.insert(name.as_str().into(), json_to_any(value));
                }
                for name in remove {
                    attrs.insert(name.as_str().into(), Any::Null);
                }
                delta.push(Delta::Retain(*bytes, Some(Box::new(attrs))));
                counts.format_ranges += 1;
            }
            TextOp::Delete(n) => {
                delta.push(Delta::Deleted(*n));
                counts.text_bytes_deleted += *n;
            }
            TextOp::Insert { text, marks } => {
                if text.is_empty() {
                    continue;
                }
                delta.push(insert_delta(text, marks));
                counts.text_bytes_inserted += text.len() as u32;
            }
        }
    }
    // A trailing plain Retain says nothing.
    while matches!(delta.last(), Some(Delta::Retain(_, None))) {
        delta.pop();
    }
    if delta.is_empty() {
        return;
    }
    text.handle.apply_delta(txn, delta);
}

/// An insert that carries exactly `marks`: `apply_delta` unsets every attribute not given
/// (yrs text.rs:703-724), so the text inherits nothing from its neighbours.
fn insert_delta(text: &str, marks: &RawMarks) -> Delta<In> {
    let mut attrs = Attrs::new();
    for (name, value) in marks {
        attrs.insert(name.as_str().into(), json_to_any(value));
    }
    Delta::Inserted(In::Any(Any::from(text)), Some(Box::new(attrs)))
}

/// Insert a new Y.XmlText at `at` holding `runs`. Returns false (and inserts nothing) when every run is empty.
fn emit_insert_text<P: XmlFragment>(
    txn: &mut TransactionMut<'_>,
    parent: &P,
    at: u32,
    runs: &[TextRun],
    counts: &mut OpCounts,
) -> bool {
    let delta: Vec<Delta<In>> = runs
        .iter()
        .filter(|run| !run.text.is_empty())
        .map(|run| insert_delta(&run.text, &run.raw_marks))
        .collect();
    if delta.is_empty() {
        return false;
    }
    let text_ref = parent.insert(txn, at, XmlTextPrelim::new(""));
    counts.nodes_inserted += 1;
    counts.text_bytes_inserted += runs.iter().map(|run| run.text.len() as u32).sum::<u32>();
    text_ref.apply_delta(txn, delta);
    true
}
