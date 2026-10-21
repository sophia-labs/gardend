//! Tests for `minimal_edit::ytree` (owner U6; DESIGN §4.3).
//! Real yrs documents, restored forward: independent histories on both sides, as a snapshot and a live room are.

use serde_json::{json, Value};
use yrs::types::ToJson;
use yrs::updates::decoder::Decode;
use yrs::{Any, Array, ArrayPrelim, Doc, Map, MapPrelim, ReadTxn, StateVector, Transact, WriteTxn};

use super::ytree::{restore_array, restore_map, restore_map_roots};
use super::OpCounts;

const GOLDEN_BOARD: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/flow/golden-board.json"
));

/// A Doc whose root map `name` holds `entries` (scalars only).
fn doc_with_map(name: &str, entries: &[(&str, Any)]) -> Doc {
    let doc = Doc::new();
    {
        let mut txn = doc.transact_mut();
        let map = txn.get_or_insert_map(name);
        for (key, value) in entries {
            map.insert(&mut txn, *key, value.clone());
        }
    }
    doc
}

/// Restore `roots` of `current` toward `target` in one transaction; returns the counts.
fn restore(current: &Doc, target: &Doc, roots: &[&str]) -> OpCounts {
    let target_txn = target.transact();
    let mut txn = current.transact_mut();
    restore_map_roots(&mut txn, roots, &target_txn)
}

/// JSON with every number as an f64, so `1` and `1.0` compare equal whatever yrs's serializer chose.
fn normalize(value: Value) -> Value {
    match value {
        Value::Number(n) => n
            .as_f64()
            .and_then(serde_json::Number::from_f64)
            .map(Value::Number)
            .unwrap_or(Value::Null),
        Value::Array(items) => Value::Array(items.into_iter().map(normalize).collect()),
        Value::Object(entries) => {
            Value::Object(entries.into_iter().map(|(k, v)| (k, normalize(v))).collect())
        }
        other => other,
    }
}

fn root_json(doc: &Doc, name: &str) -> Value {
    let txn = doc.transact();
    match txn.get_map(name) {
        Some(map) => normalize(serde_json::to_value(map.to_json(&txn)).unwrap_or(Value::Null)),
        None => Value::Null,
    }
}

fn state_vector(doc: &Doc) -> StateVector {
    doc.transact().state_vector()
}

#[test]
fn equal_maps_are_zero_ops() {
    // Two independent histories with the same content.
    let entries = [
        ("title", Any::from("Dreams")),
        ("count", Any::Number(3.0)),
        ("flag", Any::Bool(true)),
    ];
    let current = doc_with_map("documents", &entries);
    let target = doc_with_map("documents", &entries);
    let before = state_vector(&current);

    let counts = restore(&current, &target, &["documents"]);

    assert!(counts.is_zero(), "equal trees must change nothing: {counts:?}");
    assert_eq!(state_vector(&current), before, "no item was created");
    assert_eq!(root_json(&current, "documents"), root_json(&target, "documents"));
}

#[test]
fn numbers_that_differ_only_in_representation_are_equal() {
    let current = doc_with_map("documents", &[("n", Any::BigInt(7))]);
    let target = doc_with_map("documents", &[("n", Any::Number(7.0))]);
    let counts = restore(&current, &target, &["documents"]);
    assert!(counts.is_zero(), "7 and 7.0 are the same value: {counts:?}");
}

#[test]
fn changed_any_value_is_one_set() {
    let current = doc_with_map("documents", &[("a", Any::Number(1.0)), ("b", Any::from("x"))]);
    let target = doc_with_map("documents", &[("a", Any::Number(1.0)), ("b", Any::from("y"))]);

    let counts = restore(&current, &target, &["documents"]);

    assert_eq!(
        counts,
        OpCounts {
            map_keys_set: 1,
            ..OpCounts::default()
        }
    );
    assert_eq!(root_json(&current, "documents"), json!({"a": 1.0, "b": "y"}));
}

#[test]
fn removed_key_is_one_remove() {
    let current = doc_with_map(
        "documents",
        &[("a", Any::Number(1.0)), ("gone", Any::from("post-capture"))],
    );
    let target = doc_with_map("documents", &[("a", Any::Number(1.0))]);

    let counts = restore(&current, &target, &["documents"]);

    assert_eq!(
        counts,
        OpCounts {
            map_keys_removed: 1,
            ..OpCounts::default()
        }
    );
    assert_eq!(root_json(&current, "documents"), json!({"a": 1.0}));
}

#[test]
fn key_added_after_capture_is_set_back_when_missing_from_current() {
    let current = doc_with_map("documents", &[]);
    let target = doc_with_map("documents", &[("a", Any::from("was here"))]);
    let counts = restore(&current, &target, &["documents"]);
    assert_eq!(counts.map_keys_set, 1);
    assert_eq!(root_json(&current, "documents"), json!({"a": "was here"}));
}

/// A root map "documents" holding one nested map under "doc" and, optionally, an array under "tags".
fn nested_doc(y: f64, tags: &[&str]) -> Doc {
    let doc = Doc::new();
    {
        let mut txn = doc.transact_mut();
        let root = txn.get_or_insert_map("documents");
        let inner = root.insert(&mut txn, "doc", MapPrelim::default());
        inner.insert(&mut txn, "x", Any::Number(1.0));
        inner.insert(&mut txn, "y", Any::Number(y));
        root.insert(&mut txn, "other", Any::Number(5.0));
        let items: Vec<Any> = tags.iter().map(|tag| Any::from(*tag)).collect();
        root.insert(&mut txn, "tags", ArrayPrelim::from(items));
    }
    doc
}

#[test]
fn nested_map_recurses_and_keeps_identity() {
    let current = nested_doc(2.0, &["a"]);
    let target = nested_doc(3.0, &["a"]);
    let inner_before = {
        let txn = current.transact();
        let root = txn.get_map("documents").expect("root");
        match root.get(&txn, "doc") {
            Some(yrs::Out::YMap(inner)) => inner.as_ref().id(),
            other => panic!("nested map expected, got {other:?}"),
        }
    };

    let counts = restore(&current, &target, &["documents"]);

    assert_eq!(
        counts,
        OpCounts {
            map_keys_set: 1,
            ..OpCounts::default()
        },
        "only y changed; the nested map was recursed into, not replaced"
    );
    let inner_after = {
        let txn = current.transact();
        let root = txn.get_map("documents").expect("root");
        match root.get(&txn, "doc") {
            Some(yrs::Out::YMap(inner)) => inner.as_ref().id(),
            other => panic!("nested map expected, got {other:?}"),
        }
    };
    assert_eq!(inner_before, inner_after, "the nested map keeps its Yjs identity");
    assert_eq!(root_json(&current, "documents"), root_json(&target, "documents"));
}

fn array_len_and_items(doc: &Doc) -> Vec<Value> {
    let txn = doc.transact();
    let root = txn.get_map("documents").expect("root");
    match root.get(&txn, "tags") {
        Some(yrs::Out::YArray(array)) => array
            .iter(&txn)
            .map(|out| normalize(serde_json::to_value(out.to_json(&txn)).unwrap_or(Value::Null)))
            .collect(),
        other => panic!("array expected, got {other:?}"),
    }
}

#[test]
fn array_restore_is_minimal() {
    // One element replaced in the middle: one removal, one insertion, nothing else.
    let current = nested_doc(1.0, &["a", "b", "c", "d", "e"]);
    let target = nested_doc(1.0, &["a", "b", "X", "d", "e"]);
    let counts = restore(&current, &target, &["documents"]);
    assert_eq!(
        counts,
        OpCounts {
            array_items_inserted: 1,
            array_items_removed: 1,
            ..OpCounts::default()
        }
    );
    assert_eq!(
        array_len_and_items(&current),
        vec![json!("a"), json!("b"), json!("X"), json!("d"), json!("e")]
    );

    // A pure append is one insertion, no removal.
    let current = nested_doc(1.0, &["a", "b"]);
    let target = nested_doc(1.0, &["a", "b", "c"]);
    let counts = restore(&current, &target, &["documents"]);
    assert_eq!(
        counts,
        OpCounts {
            array_items_inserted: 1,
            ..OpCounts::default()
        }
    );
    assert_eq!(array_len_and_items(&current), vec![json!("a"), json!("b"), json!("c")]);

    // Items added after the capture are removed; the rest stay.
    let current = nested_doc(1.0, &["a", "post", "b"]);
    let target = nested_doc(1.0, &["a", "b"]);
    let counts = restore(&current, &target, &["documents"]);
    assert_eq!(
        counts,
        OpCounts {
            array_items_removed: 1,
            ..OpCounts::default()
        }
    );
    assert_eq!(array_len_and_items(&current), vec![json!("a"), json!("b")]);
}

#[test]
fn array_restore_function_reaches_the_target_directly() {
    let current = Doc::new();
    let target = Doc::new();
    for (doc, items) in [(&current, vec![3, 1, 2, 9]), (&target, vec![1, 2, 3])] {
        let mut txn = doc.transact_mut();
        let array = txn.get_or_insert_array("list");
        for item in items {
            array.push_back(&mut txn, Any::Number(item as f64));
        }
    }
    let target_txn = target.transact();
    let target_array = target_txn.get_array("list").expect("target list");
    let mut txn = current.transact_mut();
    let current_array = txn.get_or_insert_array("list");
    restore_array(&mut txn, &current_array, &target_txn, &target_array);
    let items: Vec<f64> = current_array
        .iter(&txn)
        .map(|out| match out {
            yrs::Out::Any(Any::Number(n)) => n,
            other => panic!("number expected, got {other:?}"),
        })
        .collect();
    assert_eq!(items, vec![1.0, 2.0, 3.0]);
}

#[test]
fn absent_target_root_clears_current() {
    let current = doc_with_map("documents", &[("a", Any::Number(1.0)), ("b", Any::Number(2.0))]);
    let target = Doc::new(); // no `documents` root at all

    let counts = restore(&current, &target, &["documents"]);

    assert_eq!(
        counts,
        OpCounts {
            map_keys_removed: 2,
            ..OpCounts::default()
        }
    );
    assert_eq!(root_json(&current, "documents"), json!({}));
}

#[test]
fn absent_root_on_both_sides_is_zero_ops() {
    let current = Doc::new();
    let target = Doc::new();
    let counts = restore(&current, &target, &["documents", "wires"]);
    assert!(counts.is_zero());
}

#[test]
fn unnamed_roots_untouched() {
    let current = Doc::new();
    {
        let mut txn = current.transact_mut();
        let comments = txn.get_or_insert_map("comments");
        comments.insert(&mut txn, "c1", Any::from("old"));
        let ledger = txn.get_or_insert_map("_operationLog");
        ledger.insert(&mut txn, "op-sentinel", Any::from("applied after the capture"));
    }
    let target = doc_with_map("comments", &[("c1", Any::from("captured"))]);

    let counts = restore(&current, &target, &["comments"]);

    assert_eq!(counts.map_keys_set, 1);
    assert_eq!(root_json(&current, "comments"), json!({"c1": "captured"}));
    assert_eq!(
        root_json(&current, "_operationLog"),
        json!({"op-sentinel": "applied after the capture"}),
        "a root the caller did not name is never touched"
    );
}

#[test]
fn restore_map_works_on_nested_maps_directly() {
    let current = nested_doc(2.0, &[]);
    let target = nested_doc(9.0, &[]);
    let target_txn = target.transact();
    let target_root = target_txn.get_map("documents").expect("target root");
    let mut txn = current.transact_mut();
    let current_root = txn.get_or_insert_map("documents");
    let counts = restore_map(&mut txn, &current_root, &target_txn, &target_root);
    assert_eq!(counts.map_keys_set, 1);
}

/// The golden board with one requirement edited and one task dropped, so the two boards differ in a row
/// field and in an `order` array.
fn edited_board_text() -> String {
    let mut board: Value = serde_json::from_str(GOLDEN_BOARD).expect("golden board parses");
    let requirements = board["requirements"].as_array_mut().expect("requirements");
    let row = requirements[0].as_object_mut().expect("row");
    let field = row
        .iter()
        .find(|(key, value)| key.as_str() != "id" && value.is_string())
        .map(|(key, _)| key.clone())
        .expect("a string field to edit");
    let edited = format!("{}-edited", row[&field].as_str().unwrap_or_default());
    row.insert(field, Value::String(edited));
    board["tasks"].as_array_mut().expect("tasks").pop();
    serde_json::to_string(&board).expect("serialize")
}

#[test]
fn flow_board_seed_round_trip() {
    let golden = crate::flow_board::board_from_json(GOLDEN_BOARD).expect("seed golden");
    let edited = crate::flow_board::board_from_json(&edited_board_text()).expect("seed edited");
    let export = |doc: &Doc| crate::flow_board::board_export_json(doc, None).json;
    assert_ne!(
        export(&golden),
        export(&edited),
        "the two boards must differ before the restore"
    );

    // Restore A (the edited board, 'now') toward B (the golden board, the 'snapshot').
    let counts = restore(&edited, &golden, &["resource", "scene"]);
    assert!(!counts.is_zero(), "the boards differ, so something changed");
    assert_eq!(export(&edited), export(&golden), "the exported boards are equal");

    // And back again: now the other direction changes things, and a second restore is a no-op.
    let again = restore(&edited, &golden, &["resource", "scene"]);
    assert!(again.is_zero(), "restoring twice is the same as once: {again:?}");

    // A round trip through bytes: the restored board survives an encode/decode.
    let bytes = edited
        .transact()
        .encode_state_as_update_v1(&StateVector::default());
    let reloaded = Doc::new();
    reloaded
        .transact_mut()
        .apply_update(yrs::Update::decode_v1(&bytes).expect("decode"))
        .expect("apply");
    assert_eq!(export(&reloaded), export(&golden));
}
