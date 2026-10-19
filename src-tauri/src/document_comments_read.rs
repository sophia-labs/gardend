//! A document's comments, as `read_document` shows them to an agent
//! (2026-10-19). People's notes and highlights live in the Y.Doc `comments`
//! map; an agent that reads the document should read them too.
//!
//! Compact and stable: a fixed set of fields, absent ones left out, entries in
//! `createdAt` then `id` order.

use crate::document_types::DocumentRecord;
use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _};
use serde_json::{json, Map, Value};
use yrs::{types::ToJson, updates::decoder::Decode, Doc, ReadTxn, Transact, Update};

/// What an entry shows an agent, in output order. `id` and `text` are always
/// present; the rest only when the entry has them.
const FIELDS: [(&str, &str); 10] = [
    ("id", "id"),
    ("text", "text"),
    ("quotedText", "quotedText"),
    ("author", "author"),
    ("claimedAuthor", "claimedAuthor"),
    ("blockId", "blockId"),
    ("resolved", "resolved"),
    ("kind", "kind"),
    ("color", "color"),
    ("anchor", "anchor"),
];

pub(crate) fn comments_for_read(document: &DocumentRecord) -> Vec<Value> {
    let Ok(bytes) = BASE64_STANDARD.decode(&document.ydoc_update_base64) else {
        return Vec::new();
    };
    let Ok(update) = Update::decode_v1(&bytes) else {
        return Vec::new();
    };
    let doc = Doc::new();
    if doc.transact_mut().apply_update(update).is_err() {
        return Vec::new();
    }
    comments_in_doc(&doc)
}

pub(crate) fn comments_in_doc(doc: &Doc) -> Vec<Value> {
    let txn = doc.transact();
    let Some(map) = txn.get_map("comments") else {
        return Vec::new();
    };
    let mut text = String::new();
    map.to_json(&txn).to_json(&mut text);
    let Some(entries) = serde_json::from_str::<Value>(&text)
        .ok()
        .and_then(|value| value.as_object().cloned())
    else {
        return Vec::new();
    };
    let mut rows: Vec<(f64, String, Value)> = entries
        .iter()
        .map(|(key, entry)| (created_at(entry), key.clone(), compact(key, entry)))
        .collect();
    rows.sort_by(|a, b| {
        a.0.partial_cmp(&b.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.1.cmp(&b.1))
    });
    rows.into_iter().map(|(_, _, row)| row).collect()
}

fn created_at(entry: &Value) -> f64 {
    entry
        .get("createdAt")
        .and_then(Value::as_f64)
        .unwrap_or(f64::MAX)
}

fn compact(key: &str, entry: &Value) -> Value {
    let mut out = Map::new();
    let Some(entry) = entry.as_object() else {
        // An old bare-string entry is just the note.
        out.insert("id".into(), json!(key));
        out.insert("text".into(), entry.clone());
        return Value::Object(out);
    };
    for (field, source) in FIELDS {
        let value = match field {
            "id" => Some(json!(entry
                .get("id")
                .or_else(|| entry.get("commentId"))
                .and_then(Value::as_str)
                .unwrap_or(key))),
            "text" => Some(entry.get(source).cloned().unwrap_or_else(|| json!(""))),
            _ => entry.get(source).filter(|v| !v.is_null()).cloned(),
        };
        if let Some(value) = value {
            out.insert(field.to_string(), value);
        }
    }
    if let Some(created) = entry.get("createdAt").filter(|v| !v.is_null()) {
        out.insert("createdAt".into(), created.clone());
    }
    Value::Object(out)
}
