//! Markdown fidelity round trip for the cell's document write/read faces:
//! `markdown → TipTap JSON` (content_parse.rs, what `write_document` stores)
//! and `TipTap JSON → markdown` (markdown_export.rs, what
//! `read_document(format=markdown)` returns).
//!
//! Corpus: tests/fixtures/markdown_round_trip/*.md — the synthetic documents
//! from the hosted Hoja QA of 2026-09-23 (qa-*.md) plus `constructs.md`
//! (every construct the editor schema can hold). A `<name>.expected.md`
//! beside a document records an intentionally lossy read-back (schema limits:
//! heading levels 4–6 clamp to 3, ordered-list start numbers, inline images
//! hoisted to blocks); every other document must come back byte-identical
//! modulo trailing whitespace.
//!
//! The store leg (write_document/read_document through a real graph) is the
//! lib test `markdown_fidelity_store_tests`.

#[path = "../src/crdt_engine/content_parse.rs"]
mod content_parse;
#[path = "../src/crdt_engine/markdown_export.rs"]
mod markdown_export;

use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};

fn corpus_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("markdown_round_trip")
}

fn corpus() -> Vec<(String, String, String)> {
    let mut docs = Vec::new();
    for entry in fs::read_dir(corpus_dir()).expect("corpus dir") {
        let path = entry.expect("entry").path();
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        if !name.ends_with(".md") || name.ends_with(".expected.md") {
            continue;
        }
        let stem = name.trim_end_matches(".md").to_string();
        let authored = fs::read_to_string(&path).expect("read corpus doc");
        let expected_path = corpus_dir().join(format!("{stem}.expected.md"));
        let expected = fs::read_to_string(&expected_path).unwrap_or_else(|_| authored.clone());
        docs.push((stem, authored, expected));
    }
    docs.sort();
    assert!(docs.len() >= 12, "corpus went missing: {} docs", docs.len());
    docs
}

fn normalize(markdown: &str) -> String {
    markdown
        .lines()
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string()
}

fn import(markdown: &str) -> Value {
    content_parse::parse_write_content(markdown, Some("markdown"))
        .expect("parse")
        .tiptap_json
}

fn export(doc: &Value) -> String {
    markdown_export::tiptap_json_to_markdown(doc)
}

fn strip_block_ids(value: &mut Value) {
    match value {
        Value::Array(items) => items.iter_mut().for_each(strip_block_ids),
        Value::Object(map) => {
            if let Some(Value::Object(attrs)) = map.get_mut("attrs") {
                attrs.remove("data-block-id");
            }
            map.values_mut().for_each(strip_block_ids);
        }
        _ => {}
    }
}

fn find_all<'a>(node: &'a Value, node_type: &str, out: &mut Vec<&'a Value>) {
    if node.get("type").and_then(Value::as_str) == Some(node_type) {
        out.push(node);
    }
    if let Some(children) = node.get("content").and_then(Value::as_array) {
        for child in children {
            find_all(child, node_type, out);
        }
    }
}

fn nodes<'a>(doc: &'a Value, node_type: &str) -> Vec<&'a Value> {
    let mut out = Vec::new();
    find_all(doc, node_type, &mut out);
    out
}

#[test]
fn corpus_markdown_round_trips_through_tiptap_json() {
    let mut failures = Vec::new();
    for (name, authored, expected) in corpus() {
        let doc = import(&authored);
        let read_back = export(&doc);
        if normalize(&read_back) != normalize(&expected) {
            failures.push(format!(
                "--- {name}: expected ---\n{}\n--- {name}: read back ---\n{}\n",
                normalize(&expected),
                normalize(&read_back)
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "markdown round trip lost content:\n{}",
        failures.join("\n")
    );
}

#[test]
fn corpus_round_trip_is_a_fixed_point() {
    for (name, authored, _) in corpus() {
        let first = import(&authored);
        let once = export(&first);
        let mut second = import(&once);
        let twice = export(&second);
        assert_eq!(
            normalize(&once),
            normalize(&twice),
            "{name}: export is not idempotent"
        );
        let mut first = first;
        strip_block_ids(&mut first);
        strip_block_ids(&mut second);
        assert_eq!(
            first, second,
            "{name}: re-importing the export changed the TipTap document"
        );
    }
}

#[test]
fn gfm_table_imports_as_editor_table_nodes() {
    let doc = import(&fs::read_to_string(corpus_dir().join("qa-table.md")).unwrap());
    let tables = nodes(&doc, "table");
    assert_eq!(tables.len(), 1, "table dropped at import: {doc}");
    let rows = nodes(tables[0], "tableRow");
    assert_eq!(rows.len(), 4);
    let headers = nodes(rows[0], "tableHeader");
    assert_eq!(headers.len(), 3);
    assert_eq!(headers[0]["content"][0]["content"][0]["text"], "Name");
    assert_eq!(headers[1]["content"][0]["attrs"]["textAlign"], "right");
    let cells = nodes(rows[2], "tableCell");
    assert_eq!(cells.len(), 3);
    assert_eq!(cells[1]["content"][0]["content"][0]["text"], "12");
    assert_eq!(cells[0]["attrs"]["colspan"], 1);
    // The empty trailing cell still holds the paragraph tableCell requires.
    let last = nodes(rows[3], "tableCell");
    assert_eq!(last[2]["content"][0]["type"], "paragraph");
}

#[test]
fn markdown_image_imports_as_image_block() {
    let doc = import(&fs::read_to_string(corpus_dir().join("qa-image.md")).unwrap());
    let content = doc["content"].as_array().unwrap();
    let image = content
        .iter()
        .find(|n| n["type"] == "image")
        .expect("image node");
    assert_eq!(
        image["attrs"]["src"],
        "https://garden.sophia-labs.com/hoja/hoja-mark.svg"
    );
    assert_eq!(image["attrs"]["alt"], "hoja mark");
    assert!(
        image["attrs"]["data-block-id"].is_string(),
        "image blocks get block ids"
    );
    // No paragraph carries the alt text as prose any more.
    assert!(!content
        .iter()
        .any(|n| n["type"] == "paragraph" && n.to_string().contains("hoja mark")));

    let titled = import("![An image](https://example.com/img.png \"Image title\")");
    assert_eq!(titled["content"][0]["attrs"]["title"], "Image title");
}

#[test]
fn heading_levels_clamp_to_the_editor_schema() {
    // Both editor schemas configure heading levels [1, 2, 3]; h4–h6 clamp to 3
    // (TipTap would otherwise render an unknown level as h1).
    let doc = import("# a\n\n## b\n\n### c\n\n#### d\n\n###### f");
    let levels: Vec<f64> = nodes(&doc, "heading")
        .iter()
        .map(|h| h["attrs"]["level"].as_f64().unwrap())
        .collect();
    assert_eq!(levels, vec![1.0, 2.0, 3.0, 3.0, 3.0]);
}

#[test]
fn inline_code_is_stored_verbatim() {
    let doc = import("Compare `a<b && c>\"d\"` here.");
    let code = &doc["content"][0]["content"][1];
    assert_eq!(code["text"], "a<b && c>\"d\"");
    assert_eq!(code["marks"], json!([{"type": "code"}]));
}

#[test]
fn export_preserves_marks_links_lists_and_quotes() {
    let doc = json!({"type": "doc", "content": [
        {"type": "paragraph", "content": [
            {"type": "text", "text": "Go "},
            {"type": "text", "text": "here", "marks": [
                {"type": "link", "attrs": {"href": "https://e.test", "target": "_blank"}},
                {"type": "bold"}
            ]},
            {"type": "text", "text": " now ", "marks": [{"type": "italic"}]},
            {"type": "text", "text": "x", "marks": [{"type": "highlight"}]}
        ]},
        {"type": "listItem", "attrs": {"listType": "ordered", "indent": 0.0},
         "content": [{"type": "paragraph", "content": [{"type": "text", "text": "a"}]}]},
        {"type": "listItem", "attrs": {"listType": "ordered", "indent": 1.0},
         "content": [{"type": "paragraph", "content": [{"type": "text", "text": "a.1"}]}]},
        {"type": "listItem", "attrs": {"listType": "ordered", "indent": 0.0},
         "content": [{"type": "paragraph", "content": [{"type": "text", "text": "b"}]}]},
        {"type": "blockquote", "content": [
            {"type": "paragraph", "content": [{"type": "text", "text": "q1\nq2"}]}
        ]},
        {"type": "codeBlock", "attrs": {"language": "py"},
         "content": [{"type": "text", "text": "print('```')"}]}
    ]});
    assert_eq!(
        export(&doc),
        "Go [**here**](https://e.test) *now* x\n\n1. a\n   1. a.1\n2. b\n\n> q1\n> q2\n\n````py\nprint('```')\n````"
    );
}
