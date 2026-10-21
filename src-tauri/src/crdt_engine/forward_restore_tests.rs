//! Tests for `forward_restore` in pure yrs, no app (owner U9; DESIGN §4.3).
//! Documents built by the real builder, restored forward by the real planner and emitter, compared under
//! the oracle normal form. A snapshot and its room share a history here, as they do in the store.

use serde_json::{json, Value};
use yrs::types::xml::{XmlFragment, XmlOut};
use yrs::types::ToJson;
use yrs::updates::decoder::Decode;
use yrs::{Any, Doc, Map, MapPrelim, ReadTxn, StateVector, Text, Transact, Update, WriteTxn};

use super::builder::ydoc_from_tiptap_json;
use super::forward_restore::{apply_document_restore, doc_from_update_bytes, RestoreContext};
use super::minimal_edit::attrs::normal_form;
use super::minimal_edit::OpCounts;
use super::projection::tiptap_json_in;

const GOLDEN_BOARD: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/flow/golden-board.json"
));

fn ctx() -> RestoreContext {
    RestoreContext {
        restore_point_id: "rp-1".to_string(),
        operation_id: "op-1".to_string(),
        now_ms: 1_761_000_000_000.0,
    }
}

fn paragraph(id: &str, text: &str) -> Value {
    json!({
        "type": "paragraph",
        "attrs": {"data-block-id": id},
        "content": [{"type": "text", "text": text}],
    })
}

fn bold_paragraph(id: &str, plain: &str, bold: &str) -> Value {
    json!({
        "type": "paragraph",
        "attrs": {"data-block-id": id},
        "content": [
            {"type": "text", "text": plain},
            {"type": "text", "text": bold, "marks": [{"type": "bold"}]},
        ],
    })
}

fn doc_json(blocks: Vec<Value>) -> Value {
    json!({"type": "doc", "content": blocks})
}

/// Structurally equal to `doc_json`'s content through the editor-shape normal form.
fn nf(doc: &Doc) -> Value {
    normal_form(&tiptap_json_in(&doc.transact()), false)
}

/// A second replica with the same history (a snapshot, or a client that synced).
fn fork(doc: &Doc) -> Doc {
    let bytes = doc
        .transact()
        .encode_state_as_update_v1(&StateVector::default());
    doc_from_update_bytes(&bytes).expect("decode a state this test just encoded")
}

fn restore(current: &Doc, target: &Doc, kind: Option<&str>) -> OpCounts {
    let mut txn = current.transact_mut();
    apply_document_restore(&mut txn, target, kind, &ctx()).expect("restore")
}

fn set_comment(doc: &Doc, id: &str, text: &str) {
    let mut txn = doc.transact_mut();
    let comments = txn.get_or_insert_map("comments");
    let entry = comments.insert(&mut txn, id, MapPrelim::default());
    entry.insert(&mut txn, "text", text);
}

/// Append `extra` to the first text of the `index`-th block, the way a person typing would.
fn type_into_block(doc: &Doc, index: u32, extra: &str) {
    let mut txn = doc.transact_mut();
    let fragment = txn.get_or_insert_xml_fragment("content");
    let Some(XmlOut::Element(block)) = fragment.get(&txn, index) else {
        panic!("block {index} is an element");
    };
    let Some(XmlOut::Text(text)) = block.get(&txn, 0) else {
        panic!("block {index} starts with text");
    };
    let end = text.len(&txn);
    text.insert(&mut txn, end, extra);
}

fn map_json(doc: &Doc, name: &str) -> Value {
    let txn = doc.transact();
    match txn.get_map(name) {
        Some(map) => serde_json::to_value(map.to_json(&txn)).unwrap_or(Value::Null),
        None => json!({}),
    }
}

fn apply(doc: &Doc, bytes: &[u8]) {
    doc.transact_mut()
        .apply_update(Update::decode_v1(bytes).expect("decode"))
        .expect("apply");
}

#[test]
fn tiptap_restore_reaches_snapshot_normal_form() {
    let snapshot = doc_json(vec![
        paragraph("p1", "First paragraph"),
        bold_paragraph("p2", "Second ", "paragraph"),
        paragraph("p3", "Third paragraph"),
    ]);
    // After the capture: p1 edited, p2 lost its bold, p3 deleted, p4 added.
    let now = doc_json(vec![
        paragraph("p1", "First paragraph, rewritten after the capture"),
        paragraph("p2", "Second paragraph"),
        paragraph("p4", "Added after the capture"),
    ]);
    let target = ydoc_from_tiptap_json(&snapshot);
    let current = ydoc_from_tiptap_json(&now);
    assert_ne!(
        nf(&current),
        nf(&target),
        "the documents differ before the restore"
    );

    let counts = restore(&current, &target, None);

    assert!(!counts.is_zero(), "something had to change");
    assert_eq!(
        nf(&current),
        nf(&target),
        "the restored document is the snapshot"
    );
    // Block ids come back too (restore states everything and binds ids).
    let restored = normal_form(&tiptap_json_in(&current.transact()), true);
    let wanted = normal_form(&tiptap_json_in(&target.transact()), true);
    assert_eq!(restored, wanted);
}

#[test]
fn unchanged_document_is_zero_ops() {
    let content = doc_json(vec![
        paragraph("p1", "First paragraph"),
        bold_paragraph("p2", "Second ", "paragraph"),
    ]);
    // Two independent histories of the same content.
    let target = ydoc_from_tiptap_json(&content);
    let current = ydoc_from_tiptap_json(&content);
    set_comment(&current, "c1", "same");
    set_comment(&target, "c1", "same");
    let before = current.transact().state_vector();

    let mut txn = current.transact_mut();
    let counts = apply_document_restore(&mut txn, &target, None, &ctx()).expect("restore");
    let update = txn.encode_update_v1();
    drop(txn);

    assert!(
        counts.is_zero(),
        "equal documents change nothing: {counts:?}"
    );
    assert!(
        update.len() <= 3,
        "the transaction's update is empty ({} bytes)",
        update.len()
    );
    assert_eq!(current.transact().state_vector(), before);
}

#[test]
fn comments_map_restored() {
    let content = doc_json(vec![paragraph("p1", "Commented text")]);
    let target = ydoc_from_tiptap_json(&content);
    set_comment(&target, "c1", "the original comment");
    let current = fork(&target);
    set_comment(&current, "c1", "edited after the capture");
    set_comment(&current, "c2", "added after the capture");
    assert_ne!(
        map_json(&current, "comments"),
        map_json(&target, "comments")
    );

    let counts = restore(&current, &target, None);

    assert_eq!(
        map_json(&current, "comments"),
        map_json(&target, "comments")
    );
    assert!(counts.map_keys_removed >= 1, "c2 is removed: {counts:?}");
    assert_eq!(
        counts.nodes_inserted
            + counts.nodes_removed
            + counts.text_bytes_inserted
            + counts.text_bytes_deleted,
        0,
        "the content was already equal: {counts:?}"
    );
}

#[test]
fn operation_log_untouched() {
    let content = doc_json(vec![paragraph("p1", "Text"), paragraph("p2", "More")]);
    let target = ydoc_from_tiptap_json(&content);
    {
        let mut txn = target.transact_mut();
        let ledger = txn.get_or_insert_map("_operationLog");
        ledger.insert(&mut txn, "op-before-capture", Any::from("applied"));
    }
    let current = fork(&target);
    {
        // An operation applied after the capture, and a content change that the restore must undo.
        let mut txn = current.transact_mut();
        let ledger = txn.get_or_insert_map("_operationLog");
        ledger.insert(&mut txn, "op-after-capture", Any::from("applied"));
    }
    type_into_block(&current, 0, " (typed after the capture)");
    let ledger_before = map_json(&current, "_operationLog");
    assert!(ledger_before.get("op-after-capture").is_some());

    let counts = restore(&current, &target, None);

    assert!(!counts.is_zero());
    assert_eq!(nf(&current), nf(&target));
    assert_eq!(
        map_json(&current, "_operationLog"),
        ledger_before,
        "the replay ledger is never rewound: a retried block operation must not apply twice"
    );
}

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
fn flow_board_restored_by_ytree() {
    let target = crate::flow_board::board_from_json(GOLDEN_BOARD).expect("seed golden");
    let current = crate::flow_board::board_from_json(&edited_board_text()).expect("seed edited");
    let export = |doc: &Doc| crate::flow_board::board_export_json(doc, None).json;
    assert_ne!(export(&current), export(&target));

    let counts = restore(&current, &target, Some(crate::flow_board::FLOW_BOARD_KIND));

    assert!(!counts.is_zero());
    assert_eq!(export(&current), export(&target));
    assert!(
        current.transact().get_xml_fragment("content").is_none(),
        "a board never grows a TipTap content root"
    );
    let again = restore(&current, &target, Some(crate::flow_board::FLOW_BOARD_KIND));
    assert!(
        again.is_zero(),
        "restoring twice is the same as once: {again:?}"
    );
}

#[test]
fn whiteboard_kind_restores_elements() {
    let element = |id: &str, version: i64, x: i64| -> Value {
        json!({"id": id, "type": "rectangle", "x": x, "y": 0, "version": version, "versionNonce": 1,
               "isDeleted": false, "updated": 1})
    };
    let put = |doc: &Doc, el: &Value| {
        let mut txn = doc.transact_mut();
        let elements = txn.get_or_insert_map("elements");
        elements.insert(
            &mut txn,
            el["id"].as_str().expect("id").to_string(),
            el.to_string(),
        );
    };
    let target = Doc::new();
    put(&target, &element("a", 2, 10));
    let current = fork(&target);
    put(&current, &element("a", 5, 99));
    put(&current, &element("b", 1, 0));

    let counts = restore(&current, &target, Some(crate::whiteboard::WHITEBOARD_KIND));

    assert_eq!(counts.map_keys_set, 2, "a restored, b tombstoned");
    assert!(crate::whiteboard::restore::scenes_equivalent(
        &current, &target
    ));
}

#[test]
fn doc_from_empty_bytes_is_empty() {
    // Zero bytes, and the canonical encoding of an empty Y.Doc: both are a document with no content.
    let empty = Doc::new();
    let canonical = empty
        .transact()
        .encode_state_as_update_v1(&StateVector::default());
    for bytes in [Vec::new(), canonical] {
        let doc = doc_from_update_bytes(&bytes).expect("empty input decodes");
        let txn = doc.transact();
        assert_eq!(txn.root_refs().count(), 0, "no roots in an empty document");
        assert_eq!(tiptap_json_in(&txn), json!({"type": "doc", "content": []}));
    }
}

#[test]
fn reconnecting_client_converges() {
    let base = doc_json(vec![
        paragraph("p1", "First paragraph"),
        paragraph("p2", "Second paragraph"),
    ]);
    let server = ydoc_from_tiptap_json(&base);
    set_comment(&server, "c1", "kept");
    let snapshot = fork(&server);
    let client_a = fork(&server);

    // After the capture the server changes, and client A syncs that newer state, then goes offline.
    type_into_block(&server, 0, " AFTER-CAPTURE");
    set_comment(&server, "c2", "after the capture");
    let to_a = server
        .transact()
        .encode_diff_v1(&client_a.transact().state_vector());
    apply(&client_a, &to_a);
    assert_ne!(
        nf(&client_a),
        nf(&snapshot),
        "A holds the post-capture state"
    );

    // The restore is a forward transaction on the server.
    let counts = restore(&server, &snapshot, None);
    assert!(!counts.is_zero());
    assert_eq!(nf(&server), nf(&snapshot));
    assert_eq!(
        map_json(&server, "comments"),
        map_json(&snapshot, "comments")
    );

    // A reconnects: the server asks with its state vector, A answers with what the server lacks.
    let server_sv = server.transact().state_vector();
    let from_a = client_a.transact().encode_diff_v1(&server_sv);
    let nf_before = nf(&server);
    apply(&server, &from_a);
    assert_eq!(
        server.transact().state_vector(),
        server_sv,
        "A knew nothing the server did not: its SyncStep2 changes nothing"
    );
    assert_eq!(
        nf(&server),
        nf_before,
        "the post-capture content does not come back"
    );

    // The server answers with the restore, and A converges on the snapshot.
    let to_a = server
        .transact()
        .encode_diff_v1(&client_a.transact().state_vector());
    apply(&client_a, &to_a);
    assert_eq!(nf(&client_a), nf(&snapshot));
    assert_eq!(
        map_json(&client_a, "comments"),
        map_json(&snapshot, "comments")
    );
}

#[test]
fn offline_edit_merges_on_top_of_the_restore() {
    let base = doc_json(vec![
        paragraph("p1", "First paragraph"),
        paragraph("p2", "Second paragraph"),
    ]);
    let server = ydoc_from_tiptap_json(&base);
    let snapshot = fork(&server);
    let client_b = fork(&server);

    type_into_block(&server, 0, " AFTER-CAPTURE");
    // B is offline and edits another block.
    type_into_block(&client_b, 1, " OFFLINE-B");

    restore(&server, &snapshot, None);

    // Exchange in both directions.
    let from_b = client_b
        .transact()
        .encode_diff_v1(&server.transact().state_vector());
    apply(&server, &from_b);
    let to_b = server
        .transact()
        .encode_diff_v1(&client_b.transact().state_vector());
    apply(&client_b, &to_b);

    assert_eq!(nf(&server), nf(&client_b), "server and B converge");
    let text = serde_json::to_string(&nf(&server)).expect("json");
    assert!(
        text.contains("OFFLINE-B"),
        "the offline edit survives as an edit: {text}"
    );
    assert!(
        !text.contains("AFTER-CAPTURE"),
        "the restored text stays restored: {text}"
    );
}
