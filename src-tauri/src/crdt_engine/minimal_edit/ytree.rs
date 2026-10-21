//! Forward restore of non-fragment roots (Y.Map/Y.Array/Any trees): comments, the workspace maps, the Flow
//! board's `resource` and `scene`. Changes only what differs from the target.
//!
//! Why this shape: a byte rewind cannot undo what a peer already holds, so restore has to be a forward edit.
//! Every value is compared by canonical JSON, nested maps and arrays are recursed into (so a row that kept
//! its fields keeps its Yjs identity), and only a value that really differs is replaced, by a deep copy of
//! the target's value (`AsPrelim`). An absent target root counts as empty.

use std::collections::BTreeMap;

use serde_json::Value;
use yrs::types::{AsPrelim, ToJson};
use yrs::{Any, Array, ArrayRef, Map, MapRef, Out, ReadTxn, TransactionMut, WriteTxn};

use super::seqdiff::{myers_by, Edit};
use super::types::OpCounts;
use super::MAX_CHILDREN_EDIT_DISTANCE;

/// An `Any` as canonical JSON: every number as an f64 (so `1` and `1.0` agree), undefined as null, buffers
/// as arrays of bytes. Object key order is irrelevant to `Value` equality.
fn any_value(any: &Any) -> Value {
    match any {
        Any::Null | Any::Undefined => Value::Null,
        Any::Bool(flag) => Value::Bool(*flag),
        Any::Number(number) => number_value(*number),
        Any::BigInt(number) => number_value(*number as f64),
        Any::String(text) => Value::String(text.to_string()),
        Any::Buffer(bytes) => Value::Array(bytes.iter().map(|b| Value::from(*b)).collect()),
        Any::Array(items) => Value::Array(items.iter().map(any_value).collect()),
        Any::Map(entries) => Value::Object(
            entries
                .iter()
                .map(|(key, value)| (key.clone(), any_value(value)))
                .collect(),
        ),
    }
}

fn number_value(number: f64) -> Value {
    serde_json::Number::from_f64(number)
        .map(Value::Number)
        .unwrap_or(Value::Null)
}

/// A value's canonical JSON, nested shared types included.
fn out_value<T: ReadTxn>(txn: &T, out: &Out) -> Value {
    any_value(&out.to_json(txn))
}

/// The kind of a value: a shared text and a plain string with the same characters are different things.
fn kind_tag(out: &Out) -> &'static str {
    match out {
        Out::Any(_) => "any",
        Out::YText(_) => "text",
        Out::YArray(_) => "array",
        Out::YMap(_) => "map",
        Out::YXmlElement(_) => "xml-element",
        Out::YXmlFragment(_) => "xml-fragment",
        Out::YXmlText(_) => "xml-text",
        Out::YDoc(_) => "doc",
        _ => "other",
    }
}

/// For each named root map: make the current root equal to the target's (an absent target root = empty).
pub(crate) fn restore_map_roots<T: ReadTxn>(
    txn: &mut TransactionMut<'_>,
    roots: &[&str],
    target: &T,
) -> OpCounts {
    let mut counts = OpCounts::default();
    for name in roots {
        let wanted = target.get_map(*name);
        let have = txn.get_map(*name);
        match (have, wanted) {
            (None, None) => {}
            (Some(have), None) => counts.add(&clear_map(txn, &have)),
            (None, Some(wanted)) => {
                let have = txn.get_or_insert_map(*name);
                counts.add(&restore_map(txn, &have, target, &wanted));
            }
            (Some(have), Some(wanted)) => counts.add(&restore_map(txn, &have, target, &wanted)),
        }
    }
    counts
}

/// Remove every key of `map` (an absent target root).
fn clear_map(txn: &mut TransactionMut<'_>, map: &MapRef) -> OpCounts {
    let mut counts = OpCounts::default();
    let mut keys: Vec<String> = map.keys(&*txn).map(str::to_string).collect();
    keys.sort();
    for key in keys {
        map.remove(txn, &key);
        counts.map_keys_removed += 1;
    }
    counts
}

/// Recursive map restore: equal Any values untouched; nested maps and arrays recursed into; differing
/// values replaced (deep copy through `yrs::types::AsPrelim::as_prelim`); keys absent from the target removed.
pub(crate) fn restore_map<T: ReadTxn>(
    txn: &mut TransactionMut<'_>,
    current: &MapRef,
    target_txn: &T,
    target: &MapRef,
) -> OpCounts {
    let mut counts = OpCounts::default();
    // BTreeMap and sorted keys: the order of operations is deterministic.
    let wanted: BTreeMap<String, Out> = target
        .iter(target_txn)
        .map(|(key, value)| (key.to_string(), value))
        .collect();
    let mut present: Vec<String> = current.keys(&*txn).map(str::to_string).collect();
    present.sort();

    for key in &present {
        if !wanted.contains_key(key) {
            current.remove(txn, key);
            counts.map_keys_removed += 1;
        }
    }
    for (key, want) in &wanted {
        let Some(have) = current.get(&*txn, key) else {
            current.insert(txn, key.as_str(), want.as_prelim(target_txn));
            counts.map_keys_set += 1;
            continue;
        };
        match (&have, want) {
            (Out::YMap(have_map), Out::YMap(want_map)) => {
                counts.add(&restore_map(txn, have_map, target_txn, want_map));
            }
            (Out::YArray(have_array), Out::YArray(want_array)) => {
                counts.add(&restore_array(txn, have_array, target_txn, want_array));
            }
            _ => {
                let equal = kind_tag(&have) == kind_tag(want)
                    && out_value(&*txn, &have) == out_value(target_txn, want);
                if !equal {
                    current.insert(txn, key.as_str(), want.as_prelim(target_txn));
                    counts.map_keys_set += 1;
                }
            }
        }
    }
    counts
}

/// Array restore by Myers over each element's canonical JSON.
pub(crate) fn restore_array<T: ReadTxn>(
    txn: &mut TransactionMut<'_>,
    current: &ArrayRef,
    target_txn: &T,
    target: &ArrayRef,
) -> OpCounts {
    let mut counts = OpCounts::default();
    let have: Vec<(&'static str, Value)> = current
        .iter(&*txn)
        .map(|out| (kind_tag(&out), out_value(&*txn, &out)))
        .collect();
    let want_outs: Vec<Out> = target.iter(target_txn).collect();
    let want: Vec<(&'static str, Value)> = want_outs
        .iter()
        .map(|out| (kind_tag(out), out_value(target_txn, out)))
        .collect();

    // Past the cap the array is replaced whole: still correct, no longer minimal.
    let script = myers_by(&have, &want, |a, b| a == b, MAX_CHILDREN_EDIT_DISTANCE)
        .unwrap_or_else(|| whole_replacement(have.len(), want.len()));

    let mut index: u32 = 0;
    for edit in script {
        match edit {
            Edit::Equal { len, .. } => index += len as u32,
            Edit::Delete { len, .. } => {
                current.remove_range(txn, index, len as u32);
                counts.array_items_removed += len as u32;
            }
            Edit::Insert { b, len } => {
                for want_index in b..b + len {
                    let _ = current.insert(txn, index, want_outs[want_index].as_prelim(target_txn));
                    index += 1;
                    counts.array_items_inserted += 1;
                }
            }
        }
    }
    counts
}

fn whole_replacement(have: usize, want: usize) -> Vec<Edit> {
    let mut script = Vec::new();
    if have > 0 {
        script.push(Edit::Delete { a: 0, len: have });
    }
    if want > 0 {
        script.push(Edit::Insert { b: 0, len: want });
    }
    script
}
