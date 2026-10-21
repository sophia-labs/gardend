//! Tests for `whiteboard::restore` (owner U12; DESIGN §4.3).
//! Pure yrs over the shared fixture (`tests/fixtures/whiteboard/whiteboard-fixture-v1.json`): the restore
//! target and the 'live' board share a history, as a snapshot and its room do, and the live board is then
//! edited the way a person or an agent would.

use serde_json::{json, Value};
use yrs::types::ToJson;
use yrs::updates::decoder::Decode;
use yrs::{Doc, Map, ReadTxn, StateVector, Transact, WriteTxn};

use super::ops::{ensure_meta, scene_of_doc};
use super::restore::{restore_elements, scenes_equivalent};
use super::scene::Element;
use crate::crdt_engine::forward_restore::RestoreContext;
use crate::crdt_engine::minimal_edit::OpCounts;

const FIXTURE: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/whiteboard/whiteboard-fixture-v1.json"
));
const NOW_MS: f64 = 1_761_000_000_000.0;

fn fixture_elements() -> Vec<Element> {
    let fixture: Value = serde_json::from_str(FIXTURE).expect("fixture parses");
    fixture["elements"]
        .as_array()
        .expect("elements")
        .iter()
        .map(|e| e.as_object().expect("element object").clone())
        .collect()
}

fn id_of(el: &Element) -> String {
    el["id"].as_str().expect("id").to_string()
}

fn write_element(doc: &Doc, el: &Element) {
    let mut txn = doc.transact_mut();
    let elements = txn.get_or_insert_map("elements");
    elements.insert(
        &mut txn,
        id_of(el),
        serde_json::to_string(el).expect("serialize"),
    );
}

fn doc_of(elements: &[Element]) -> Doc {
    let doc = Doc::new();
    for el in elements {
        write_element(&doc, el);
    }
    ensure_meta(&mut doc.transact_mut());
    doc
}

/// A second replica holding the same history.
fn fork(doc: &Doc) -> Doc {
    let bytes = doc
        .transact()
        .encode_state_as_update_v1(&StateVector::default());
    let copy = Doc::new();
    copy.transact_mut()
        .apply_update(yrs::Update::decode_v1(&bytes).expect("decode"))
        .expect("apply");
    copy
}

fn element_of(doc: &Doc, id: &str) -> Element {
    scene_of_doc(doc)
        .elements
        .get(id)
        .cloned()
        .unwrap_or_else(|| panic!("element {id} present"))
}

fn ctx() -> RestoreContext {
    RestoreContext {
        restore_point_id: "rp-1".to_string(),
        operation_id: "op-1".to_string(),
        now_ms: NOW_MS,
    }
}

fn run(current: &Doc, target: &Doc) -> OpCounts {
    let mut txn = current.transact_mut();
    restore_elements(&mut txn, target, &ctx()).expect("restore")
}

fn number(el: &Element, key: &str) -> f64 {
    el[key].as_f64().unwrap_or_else(|| panic!("{key} is a number"))
}

fn first_live_rectangle() -> Element {
    fixture_elements()
        .into_iter()
        .find(|el| el["type"] == "rectangle" && el["isDeleted"] == false)
        .expect("a live rectangle in the fixture")
}

/// `el` as a later write by someone else: moved, version bumped, own nonce.
fn edited(el: &Element, x: f64, version: i64, nonce: i64) -> Element {
    let mut out = el.clone();
    out.insert("x".into(), json!(x));
    out.insert("version".into(), json!(version));
    out.insert("versionNonce".into(), json!(nonce));
    out.insert("updated".into(), json!(1_760_999_999_999_i64));
    out.insert(
        "customData".into(),
        json!({"sophia": {"by": "person", "at": 1_760_999_999_999_i64}}),
    );
    out
}

#[test]
fn restored_element_outranks_both_versions() {
    let rect = first_live_rectangle();
    let id = id_of(&rect);
    let target_version = number(&rect, "version") as i64;
    let target_nonce = number(&rect, "versionNonce") as i64;
    let target = doc_of(&fixture_elements());

    // The live copy was edited after the capture and sits at a HIGHER version than the target's.
    let current = fork(&target);
    let live_version = target_version + 2;
    write_element(&current, &edited(&rect, 500.0, live_version, 999));
    let counts = run(&current, &target);

    assert_eq!(counts.map_keys_set, 1, "only the edited element is rewritten");
    let restored = element_of(&current, &id);
    assert_eq!(number(&restored, "x"), number(&rect, "x"), "the target's geometry is back");
    assert_eq!(
        number(&restored, "version") as i64,
        live_version + 1,
        "above both the live copy and the target"
    );
    assert!(number(&restored, "version") as i64 > target_version);
    assert_ne!(number(&restored, "versionNonce") as i64, 999);
    assert_ne!(number(&restored, "versionNonce") as i64, target_nonce);
    assert_eq!(number(&restored, "updated") as i64, NOW_MS as i64);
    assert_eq!(restored["customData"]["sophia"]["by"], "restore");
    assert_eq!(restored["customData"]["sophia"]["at"], NOW_MS as i64);

    // The other direction: the live copy sits BELOW the target (a stale edit); the target still wins.
    let high_target = {
        let mut el = rect.clone();
        el.insert("version".into(), json!(40));
        doc_of(&[el])
    };
    let low_current = fork(&high_target);
    write_element(&low_current, &edited(&rect, 7.0, 2, 5));
    run(&low_current, &high_target);
    assert_eq!(
        number(&element_of(&low_current, &id), "version") as i64,
        41,
        "above the target's version too"
    );
}

#[test]
fn element_absent_in_target_is_tombstoned() {
    let target = doc_of(&fixture_elements());
    let current = fork(&target);

    let mut added = first_live_rectangle();
    added.insert("id".into(), json!("added-after-capture-0001"));
    added.insert("version".into(), json!(4));
    write_element(&current, &added);
    // A tombstone nobody has to touch again.
    let mut already_gone = added.clone();
    already_gone.insert("id".into(), json!("gone-before-0000000002"));
    already_gone.insert("isDeleted".into(), json!(true));
    already_gone.insert("version".into(), json!(9));
    write_element(&current, &already_gone);

    let counts = run(&current, &target);

    assert_eq!(counts.map_keys_set, 1, "one tombstone written, the old one left alone");
    let tombstone = element_of(&current, "added-after-capture-0001");
    assert_eq!(tombstone["isDeleted"], true, "the element stays as a tombstone, not a removed key");
    assert_eq!(number(&tombstone, "version") as i64, 5, "version bumped past the live copy's");
    assert_eq!(tombstone["type"], added["type"], "the rest of the element is kept");
    assert_eq!(
        number(&element_of(&current, "gone-before-0000000002"), "version") as i64,
        9,
        "an existing tombstone is not rewritten"
    );
    assert!(scenes_equivalent(&current, &target));
}

#[test]
fn element_missing_from_current_is_written_back_above_its_old_version() {
    let rect = first_live_rectangle();
    let id = id_of(&rect);
    let target = doc_of(&fixture_elements());
    let current = doc_of(
        &fixture_elements()
            .into_iter()
            .filter(|el| id_of(el) != id)
            .collect::<Vec<_>>(),
    );

    let counts = run(&current, &target);

    assert!(counts.map_keys_set >= 1);
    let back = element_of(&current, &id);
    assert_eq!(back["isDeleted"], false);
    assert_eq!(number(&back, "version") as i64, number(&rect, "version") as i64 + 1);
    assert!(scenes_equivalent(&current, &target));
}

#[test]
fn equal_scenes_are_zero_ops() {
    // The same drawing in an independent history, every element stamped by a later writer, and the
    // fixture's tombstone simply absent: still the same board.
    let target = doc_of(&fixture_elements());
    let restamped: Vec<Element> = fixture_elements()
        .into_iter()
        .filter(|el| el["isDeleted"] == false)
        .map(|el| {
            let version = number(&el, "version") as i64 + 10;
            let mut out = edited(&el, number(&el, "x"), version, 31337);
            out.insert("x".into(), el["x"].clone());
            out
        })
        .collect();
    let current = doc_of(&restamped);
    let before = current.transact().state_vector();

    let counts = run(&current, &target);

    assert!(counts.is_zero(), "equal scenes change nothing: {counts:?}");
    assert_eq!(current.transact().state_vector(), before, "no item was created");
    assert!(scenes_equivalent(&current, &target));
}

fn meta_json(doc: &Doc) -> Value {
    let txn = doc.transact();
    let map = txn.get_map("meta").expect("meta root");
    serde_json::to_value(map.to_json(&txn)).expect("meta json")
}

#[test]
fn fixture_round_trip() {
    let target = doc_of(&fixture_elements());
    let current = fork(&target);

    // A person moves the box, deletes the note, draws something new, and recolours the page.
    let rect = first_live_rectangle();
    let rect_id = id_of(&rect);
    write_element(&current, &edited(&rect, 640.0, number(&rect, "version") as i64 + 1, 77));
    let note = fixture_elements()
        .into_iter()
        .find(|el| id_of(el) != rect_id && el["type"] == "rectangle" && el["isDeleted"] == false)
        .expect("a second live rectangle");
    let note_id = id_of(&note);
    let mut deleted = note.clone();
    deleted.insert("isDeleted".into(), json!(true));
    deleted.insert("version".into(), json!(number(&note, "version") as i64 + 1));
    write_element(&current, &deleted);
    let mut drawn = rect.clone();
    drawn.insert("id".into(), json!("drawn-after-capture-00001"));
    write_element(&current, &drawn);
    {
        let mut txn = current.transact_mut();
        let meta = txn.get_or_insert_map("meta");
        meta.insert(&mut txn, "viewBackgroundColor", "#101010");
    }
    assert!(!scenes_equivalent(&current, &target), "the boards differ before the restore");
    let pre_versions: Vec<(String, f64)> = scene_of_doc(&current)
        .elements
        .iter()
        .map(|(id, el)| (id.clone(), number(el, "version")))
        .collect();

    let counts = run(&current, &target);

    assert!(counts.map_keys_set >= 3, "moved, undeleted and drawn elements were all rewritten");
    assert_eq!(counts.map_keys_removed, 0, "nothing is ever removed, only tombstoned");
    assert!(scenes_equivalent(&current, &target), "the restored board equals the snapshot");
    assert_eq!(meta_json(&current)["viewBackgroundColor"], "#ffffff", "meta restored through ytree");
    assert_eq!(element_of(&current, &note_id)["isDeleted"], false, "the deleted note is back");
    assert_eq!(
        element_of(&current, "drawn-after-capture-00001")["isDeleted"],
        true,
        "the new drawing is tombstoned"
    );
    let after = scene_of_doc(&current);
    for (id, before) in &pre_versions {
        let now = number(&after.elements[id], "version");
        assert!(
            now >= *before,
            "no element's version went backwards: {id} {before} -> {now}"
        );
    }
    assert!(
        number(&after.elements[&rect_id], "version") > number(&rect, "version") + 1.0,
        "the moved box outranks the live copy"
    );

    // Restoring again changes nothing, and the result survives an encode/decode.
    let again = run(&current, &target);
    assert!(again.is_zero(), "idempotent: {again:?}");
    assert!(scenes_equivalent(&fork(&current), &target));
}
