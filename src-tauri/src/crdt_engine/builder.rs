//! TipTap JSON → Y.Doc construction (write-path primitive).
//!
//! Port of createYDocFromTipTapJsonFallback / tipTapNodeToYXml
//! (frontend/src/crdt/document-roundtrip.ts), with one deliberate
//! difference: attribute values keep their native JSON types (the editor's
//! y-prosemirror binding stores Any values; the TS fallback stringified).
//! Our materializer (projection.rs) reads Any values, so construction +
//! materialization roundtrips losslessly — verified against fixtures.

use serde_json::Value;
use yrs::types::xml::XmlFragment;
use yrs::types::Attrs;
use yrs::{Any, Doc, Text, Transact, TransactionMut, Xml, XmlElementPrelim, XmlTextPrelim};

/// Build a fresh Y.Doc whose XmlFragment("content") mirrors `tiptap_json`.
pub fn ydoc_from_tiptap_json(tiptap_json: &Value) -> Doc {
    let doc = Doc::new();
    let fragment = doc.get_or_insert_xml_fragment("content");
    {
        let mut txn = doc.transact_mut();
        if let Some(content) = tiptap_json.get("content").and_then(Value::as_array) {
            append_nodes(&mut txn, &fragment, content);
        }
    }
    doc
}

pub(crate) fn append_nodes<F: XmlFragment>(
    txn: &mut TransactionMut<'_>,
    parent: &F,
    nodes: &[Value],
) {
    for node in nodes {
        append_node(txn, parent, node);
    }
}

fn append_node<F: XmlFragment>(txn: &mut TransactionMut<'_>, parent: &F, node: &Value) {
    let node_type = node.get("type").and_then(Value::as_str);
    if node_type == Some("text") {
        let text = node.get("text").and_then(Value::as_str).unwrap_or("");
        if text.is_empty() {
            return;
        }
        let index = parent.len(txn);
        let text_ref = parent.insert(txn, index, XmlTextPrelim::new(""));
        match marks_to_attrs(node.get("marks")) {
            Some(attrs) => text_ref.insert_with_attributes(txn, 0, text, attrs),
            None => text_ref.insert(txn, 0, text),
        }
        return;
    }

    let tag = node_type.unwrap_or("paragraph");
    let index = parent.len(txn);
    let element = parent.insert(txn, index, XmlElementPrelim::empty(tag));
    if let Some(attrs) = node.get("attrs").and_then(Value::as_object) {
        for (key, value) in attrs {
            if value.is_null() {
                continue;
            }
            element.insert_attribute(txn, key.clone(), json_to_any(value));
        }
    }
    if let Some(content) = node.get("content").and_then(Value::as_array) {
        append_nodes(txn, &element, content);
    }
}

fn marks_to_attrs(marks: Option<&Value>) -> Option<Attrs> {
    let marks = marks?.as_array()?;
    if marks.is_empty() {
        return None;
    }
    let mut attrs = Attrs::new();
    for mark in marks {
        let Some(mark_type) = mark.get("type").and_then(Value::as_str) else {
            continue;
        };
        let mark_attrs = mark
            .get("attrs")
            .map(json_to_any)
            .unwrap_or_else(|| Any::Map(std::sync::Arc::new(Default::default())));
        attrs.insert(mark_type.into(), mark_attrs);
    }
    Some(attrs)
}

pub(crate) fn json_to_any(value: &Value) -> Any {
    match value {
        Value::Null => Any::Null,
        Value::Bool(b) => Any::Bool(*b),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                // y-prosemirror stores JS numbers; mirror as f64 for wire parity
                Any::Number(i as f64)
            } else {
                Any::Number(n.as_f64().unwrap_or(0.0))
            }
        }
        Value::String(s) => Any::from(s.as_str()),
        Value::Array(items) => Any::Array(items.iter().map(json_to_any).collect()),
        Value::Object(entries) => Any::Map(std::sync::Arc::new(
            entries
                .iter()
                .map(|(k, v)| (k.clone(), json_to_any(v)))
                .collect(),
        )),
    }
}
