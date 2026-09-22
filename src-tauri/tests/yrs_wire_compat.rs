//! Phase 0 spike: prove yrs can read frontend-written Y.Doc binaries
//! (`update-v1.bin`) and re-materialize the same TipTap JSON the frontend
//! wrote (`tiptap.json`). Fixtures captured from a real garden profile.
//!
//! Wire format: yjs update v1, root type XmlFragment("content").
//! Reference TS: frontend/src/crdt/document-roundtrip.ts (yDocToTipTapJson).

use serde_json::{json, Map, Value};
use std::fs;
use std::path::{Path, PathBuf};
use yrs::types::xml::{XmlFragment, XmlOut};
use yrs::types::Attrs;
use yrs::updates::decoder::Decode;
use yrs::{Any, Doc, ReadTxn, Text, Transact, Update, Xml};

fn fixtures_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("yrs_wire_compat")
}

fn any_to_json(any: &Any) -> Value {
    match any {
        Any::Null | Any::Undefined => Value::Null,
        Any::Bool(b) => json!(b),
        Any::Number(n) => json!(n),
        Any::BigInt(n) => json!(n),
        Any::String(s) => json!(s.as_ref()),
        Any::Buffer(b) => json!(b.as_ref()),
        Any::Array(items) => Value::Array(items.iter().map(any_to_json).collect()),
        Any::Map(entries) => Value::Object(
            entries
                .iter()
                .map(|(k, v)| (k.to_string(), any_to_json(v)))
                .collect(),
        ),
    }
}

fn marks_from_attrs(attrs: &Attrs) -> Vec<Value> {
    let mut marks: Vec<Value> = Vec::new();
    for (name, value) in attrs.iter() {
        let mut mark = Map::new();
        mark.insert("type".into(), json!(name.to_string()));
        let attrs_json = any_to_json(value);
        if let Value::Object(ref obj) = attrs_json {
            if !obj.is_empty() {
                mark.insert("attrs".into(), attrs_json);
            }
        }
        marks.push(Value::Object(mark));
    }
    marks
}

fn xml_text_to_json<T: ReadTxn>(txn: &T, text: &yrs::XmlTextRef) -> Vec<Value> {
    let mut out = Vec::new();
    for diff in text.diff(txn, yrs::types::text::YChange::identity) {
        let chunk = match diff.insert {
            yrs::Out::Any(Any::String(s)) => s.to_string(),
            other => {
                // Embedded non-text content inside an XmlText is not expected
                // in garden documents; surface loudly if encountered.
                panic!("unexpected XmlText embed: {other:?}");
            }
        };
        if chunk.is_empty() {
            continue;
        }
        let mut node = Map::new();
        node.insert("type".into(), json!("text"));
        node.insert("text".into(), json!(chunk));
        if let Some(attrs) = diff.attributes.as_deref() {
            let marks = marks_from_attrs(attrs);
            if !marks.is_empty() {
                node.insert("marks".into(), Value::Array(marks));
            }
        }
        out.push(Value::Object(node));
    }
    out
}

fn xml_node_to_json<T: ReadTxn>(txn: &T, node: &XmlOut) -> Vec<Value> {
    match node {
        XmlOut::Text(text) => xml_text_to_json(txn, text),
        XmlOut::Element(element) => {
            let mut obj = Map::new();
            obj.insert("type".into(), json!(element.tag().to_string()));
            let mut attrs = Map::new();
            for (key, value) in element.attributes(txn) {
                attrs.insert(key.to_string(), out_to_json(txn, &value));
            }
            if !attrs.is_empty() {
                obj.insert("attrs".into(), Value::Object(attrs));
            }
            let mut content = Vec::new();
            for child in element.children(txn) {
                content.extend(xml_node_to_json(txn, &child));
            }
            if !content.is_empty() {
                obj.insert("content".into(), Value::Array(content));
            }
            vec![Value::Object(obj)]
        }
        XmlOut::Fragment(fragment) => {
            let mut content = Vec::new();
            for child in fragment.children(txn) {
                content.extend(xml_node_to_json(txn, &child));
            }
            content
        }
    }
}

fn out_to_json<T: ReadTxn>(txn: &T, out: &yrs::Out) -> Value {
    match out {
        yrs::Out::Any(any) => any_to_json(any),
        other => json!(other.clone().to_string(txn)),
    }
}

/// Port of yDocToTipTapJson: read XmlFragment("content") into
/// `{type: "doc", content: [...]}`.
fn materialize_tiptap_json(doc: &Doc) -> Value {
    let fragment = doc.get_or_insert_xml_fragment("content");
    let txn = doc.transact();
    let mut content = Vec::new();
    for child in fragment.children(&txn) {
        content.extend(xml_node_to_json(&txn, &child));
    }
    json!({ "type": "doc", "content": content })
}

/// Normalize for comparison: sort marks by type, drop empty attrs/content,
/// coerce numbers to f64 representation.
fn normalize(value: &Value) -> Value {
    match value {
        Value::Object(obj) => {
            let mut out = Map::new();
            for (k, v) in obj {
                match k.as_str() {
                    "attrs" => {
                        let normalized = normalize(v);
                        if let Value::Object(ref o) = normalized {
                            if o.is_empty() {
                                continue;
                            }
                        }
                        out.insert(k.clone(), normalized);
                    }
                    "content" => {
                        let normalized = normalize(v);
                        if let Value::Array(ref a) = normalized {
                            if a.is_empty() {
                                continue;
                            }
                        }
                        out.insert(k.clone(), normalized);
                    }
                    "marks" => {
                        let mut marks: Vec<Value> = match normalize(v) {
                            Value::Array(a) => a,
                            other => vec![other],
                        };
                        marks.sort_by_key(|m| {
                            m.get("type")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_string()
                        });
                        out.insert(k.clone(), Value::Array(marks));
                    }
                    _ => {
                        out.insert(k.clone(), normalize(v));
                    }
                }
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.iter().map(normalize).collect()),
        Value::Number(n) => json!(n.as_f64()),
        other => other.clone(),
    }
}

fn first_divergence(path: String, a: &Value, b: &Value, out: &mut Vec<String>) {
    if out.len() >= 5 {
        return;
    }
    match (a, b) {
        (Value::Object(ao), Value::Object(bo)) => {
            for key in ao.keys().chain(bo.keys()) {
                match (ao.get(key), bo.get(key)) {
                    (Some(av), Some(bv)) => first_divergence(format!("{path}.{key}"), av, bv, out),
                    (Some(_), None) => out.push(format!("{path}.{key}: only in rust output")),
                    (None, Some(_)) => out.push(format!("{path}.{key}: only in fixture")),
                    (None, None) => unreachable!(),
                }
            }
        }
        (Value::Array(aa), Value::Array(ba)) => {
            if aa.len() != ba.len() {
                out.push(format!("{path}: array len {} vs {}", aa.len(), ba.len()));
            }
            for (i, (av, bv)) in aa.iter().zip(ba.iter()).enumerate() {
                first_divergence(format!("{path}[{i}]"), av, bv, out);
            }
        }
        _ if a != b => out.push(format!("{path}: {a} != {b}")),
        _ => {}
    }
}

fn assert_fixture_roundtrip(doc_dir: &Path) {
    let bin = fs::read(doc_dir.join("update-v1.bin")).expect("read update-v1.bin");
    let expected: Value =
        serde_json::from_slice(&fs::read(doc_dir.join("tiptap.json")).expect("read tiptap.json"))
            .expect("parse tiptap.json");

    let doc = Doc::new();
    {
        let mut txn = doc.transact_mut();
        let update = Update::decode_v1(&bin).expect("decode yjs update v1");
        txn.apply_update(update).expect("apply update");
    }
    let actual = materialize_tiptap_json(&doc);

    let actual_n = normalize(&actual);
    let expected_n = normalize(&expected);
    if actual_n != expected_n {
        let mut diffs = Vec::new();
        first_divergence("$".into(), &actual_n, &expected_n, &mut diffs);
        panic!(
            "materialization diverges for {}:\n{}",
            doc_dir.display(),
            diffs.join("\n")
        );
    }
}

#[test]
fn yrs_rematerializes_frontend_written_documents() {
    let root = fixtures_root();
    let mut checked = 0;
    for entry in fs::read_dir(&root).expect("fixtures dir") {
        let dir = entry.expect("dir entry").path();
        if dir.is_dir() {
            assert_fixture_roundtrip(&dir);
            checked += 1;
        }
    }
    assert!(checked >= 3, "expected >=3 fixture docs, found {checked}");
}

/// Write-path roundtrip: build a Y.Doc from each fixture's tiptap.json via
/// the Rust builder, materialize it back, and require structural equality.
/// This is the primitive document.write relies on.
#[test]
fn builder_roundtrips_fixture_tiptap_json() {
    let root = fixtures_root();
    let mut checked = 0;
    for entry in fs::read_dir(&root).expect("fixtures dir") {
        let dir = entry.expect("dir entry").path();
        if !dir.is_dir() {
            continue;
        }
        let expected: Value =
            serde_json::from_slice(&fs::read(dir.join("tiptap.json")).expect("read tiptap.json"))
                .expect("parse tiptap.json");
        let doc = garden_lib::crdt_engine::builder::ydoc_from_tiptap_json(&expected);
        let actual = garden_lib::crdt_engine::projection::ydoc_to_tiptap_json(&doc);
        let actual_n = normalize(&actual);
        let expected_n = normalize(&expected);
        if actual_n != expected_n {
            let mut diffs = Vec::new();
            first_divergence("$".into(), &actual_n, &expected_n, &mut diffs);
            panic!(
                "builder roundtrip diverges for {}:\n{}",
                dir.display(),
                diffs.join("\n")
            );
        }
        checked += 1;
    }
    assert!(checked >= 3, "expected >=3 fixture docs, found {checked}");
}

/// Strip fields that are random per materialization (mark ids; block ids
/// only when the source node had no data-block-id) and null-valued keys
/// (the TS mark literals include different key sets per mark kind).
fn normalize_blocks(value: &Value) -> Value {
    match value {
        Value::Object(obj) => Value::Object(
            obj.iter()
                .filter(|(k, v)| k.as_str() != "id" && !v.is_null())
                .map(|(k, v)| (k.clone(), normalize_blocks(v)))
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(normalize_blocks).collect()),
        Value::Number(n) => json!(n.as_f64()),
        other => other.clone(),
    }
}

#[test]
fn projection_blocks_match_frontend_written_blocks_json() {
    let root = fixtures_root();
    let mut checked = 0;
    for entry in fs::read_dir(&root).expect("fixtures dir") {
        let dir = entry.expect("dir entry").path();
        if !dir.is_dir() {
            continue;
        }
        let bin = fs::read(dir.join("update-v1.bin")).expect("read update-v1.bin");
        let expected: Value =
            serde_json::from_slice(&fs::read(dir.join("blocks.json")).expect("read blocks.json"))
                .expect("parse blocks.json");

        let doc = Doc::new();
        {
            let mut txn = doc.transact_mut();
            txn.apply_update(Update::decode_v1(&bin).expect("decode"))
                .expect("apply");
        }
        let snapshot = garden_lib::crdt_engine::projection::materialize_ydoc(&doc, "fixture");
        let actual_n = normalize_blocks(&snapshot.blocks_json);
        let expected_n = normalize_blocks(&expected);
        if actual_n != expected_n {
            let mut diffs = Vec::new();
            first_divergence("$".into(), &actual_n, &expected_n, &mut diffs);
            panic!(
                "blocks projection diverges for {}:\n{}",
                dir.display(),
                diffs.join("\n")
            );
        }
        // `id` keys must still exist in our output even though normalized away.
        if let Value::Array(blocks) = &snapshot.blocks_json {
            for block in blocks {
                assert!(block.get("id").is_some(), "block missing id");
            }
        }
        checked += 1;
    }
    assert!(checked >= 3, "expected >=3 fixture docs, found {checked}");
}
