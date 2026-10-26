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
    comments_in_update_bytes(&bytes)
}

/// The comments of a document whose sidecar is on disk, read from the raw
/// update bytes: no hydrating record, no base64 round trip. `None` when there
/// is no sidecar (a legacy document whose history is inline in the manifest;
/// the caller then reads the hydrating record).
pub(crate) fn comments_for_read_from_sidecar(
    graph_dir: &std::path::Path,
    document_id: &str,
) -> Option<Vec<Value>> {
    let state_path = crate::paths::document_ydoc_state_path(graph_dir, document_id);
    let bytes = crate::document_sidecar_store::read_ydoc_update_bytes(&state_path)
        .ok()
        .flatten()?;
    Some(comments_in_update_bytes(&bytes))
}

pub(crate) fn comments_in_update_bytes(bytes: &[u8]) -> Vec<Value> {
    let Ok(update) = Update::decode_v1(bytes) else {
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

/// The comments that belong to one `read_document` page (R6). Paging through
/// a long document used to repeat every comment on every page (54 comments were
/// 30 KB on a 4,000-character page). A comment is on the page when the block it
/// is anchored to OPENS in the visible window: its `data-block-id` shows in
/// `visible` (xml and json formats), or, where the format carries no ids
/// (markdown), the opening of the block's own text does. A comment with no
/// block, or whose block cannot be located anywhere in the whole `content`
/// (a deleted block, a format that hides it), is unanchored and comes on the
/// first page (`offset == 0`). Order is kept.
pub(crate) fn comments_for_page(
    all: Vec<Value>,
    content: &str,
    visible: &str,
    offset: usize,
    block_text: impl Fn(&str) -> Option<String>,
) -> Vec<Value> {
    all.into_iter()
        .filter(|comment| {
            let Some(block_id) = comment
                .get("blockId")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
            else {
                return offset == 0;
            };
            let probes = [
                format!("data-block-id=\"{block_id}\""),
                format!("\"data-block-id\": \"{block_id}\""),
            ];
            let probe = if probes.iter().any(|probe| content.contains(probe.as_str())) {
                Some(probes.to_vec())
            } else {
                block_text(block_id)
                    .map(|text| text.trim().chars().take(64).collect::<String>())
                    .filter(|text| !text.is_empty() && content.contains(text.as_str()))
                    .map(|text| vec![text])
            };
            match probe {
                Some(probes) => probes.iter().any(|probe| visible.contains(probe.as_str())),
                None => offset == 0,
            }
        })
        .collect()
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

#[cfg(test)]
mod tests {
    use super::*;

    fn comment(id: &str, block: Option<&str>) -> Value {
        match block {
            Some(block) => json!({"id": id, "text": "note", "blockId": block}),
            None => json!({"id": id, "text": "note"}),
        }
    }

    fn ids(comments: &[Value]) -> Vec<&str> {
        comments.iter().map(|c| c["id"].as_str().unwrap()).collect()
    }

    #[test]
    fn xml_pages_carry_only_the_comments_of_their_blocks() {
        let content = r#"<paragraph data-block-id="b1">one</paragraph><paragraph data-block-id="b2">two</paragraph><paragraph data-block-id="b3">three</paragraph>"#;
        let split = content.find("<paragraph data-block-id=\"b2\"").unwrap();
        let (first, second) = content.split_at(split);
        let all = vec![
            comment("c1", Some("b1")),
            comment("c2", Some("b3")),
            comment("loose", None),
        ];
        let none = |_: &str| None;
        let page1 = comments_for_page(all.clone(), content, first, 0, none);
        assert_eq!(
            ids(&page1),
            ["c1", "loose"],
            "block b1 and the unanchored one"
        );
        let page2 = comments_for_page(all, content, second, split, none);
        assert_eq!(
            ids(&page2),
            ["c2"],
            "block b3 only; unanchored not repeated"
        );
    }

    #[test]
    fn markdown_pages_locate_a_comment_by_its_blocks_text() {
        let content = "First paragraph here.\n\nSecond paragraph there.";
        let visible = "Second paragraph there.";
        let all = vec![
            comment("a", Some("b1")),
            comment("b", Some("b2")),
            comment("gone", Some("zz")),
        ];
        let text = |id: &str| match id {
            "b1" => Some("First paragraph here.".to_string()),
            "b2" => Some("Second paragraph there.".to_string()),
            _ => None,
        };
        let later = comments_for_page(all.clone(), content, visible, 23, text);
        assert_eq!(ids(&later), ["b"]);
        let first = comments_for_page(all, content, "First paragraph here.", 0, text);
        assert_eq!(
            ids(&first),
            ["a", "gone"],
            "a block that cannot be located is unanchored: first page"
        );
    }
}
