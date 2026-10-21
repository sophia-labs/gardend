//! Block contract: the emitted document, parity with the web editor's
//! schema, and the normaliser's laws (canonical = byte-for-byte no-op,
//! idempotent, deterministic, never loses text). The store-level proofs over
//! every write path are in `block_contract_store_tests.rs`.
use super::block_contract::{self, AWAITING_EDITOR, CONTRACT_VERSION, MARKS, NODES};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};

/// Shrubbery's `packages/editor-kernel/schema/editor-kernel.schema.json` at
/// 319e0253 (feat/typed-faces-20261020), byte for byte.
const EDITOR_SCHEMA: &str =
    include_str!("../../tests/fixtures/block_contract/editor-kernel-319e0253.schema.json");
const BATTERY: &str = include_str!("../../tests/fixtures/block_contract/list-loss-battery.json");
const CONTRACT_FILE: &str = include_str!("block-contract.v1.json");

fn contract_path() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/crdt_engine/block-contract.v1.json")
}

#[test]
fn emitted_contract_file_is_current_and_content_addressed() {
    let generated = block_contract::contract_file_text();
    if std::env::var_os("BLOCK_CONTRACT_WRITE").is_some() {
        std::fs::write(contract_path(), &generated).unwrap();
        return;
    }
    assert_eq!(
        CONTRACT_FILE, generated,
        "block-contract.v1.json is stale: regenerate with BLOCK_CONTRACT_WRITE=1 cargo test … emitted_contract_file"
    );
    // sha256 is over the compact canonical JSON of everything but itself.
    let mut parsed: Value = serde_json::from_str(CONTRACT_FILE).unwrap();
    let declared = parsed["sha256"].as_str().unwrap().to_string();
    parsed.as_object_mut().unwrap().remove("sha256");
    let compact = serde_json::to_string(&block_contract::canonical_json(&parsed)).unwrap();
    use sha2::Digest;
    assert_eq!(
        format!("{:x}", sha2::Sha256::digest(compact.as_bytes())),
        declared
    );
    assert_eq!(parsed["contractVersion"], CONTRACT_VERSION);
    assert_eq!(block_contract::contract_status()["sha256"], declared);
}

fn attr_set(value: &Value) -> BTreeSet<String> {
    value.as_object().unwrap().keys().cloned().collect()
}

#[test]
fn contract_matches_the_editor_kernel_document_schema() {
    let editor: Value = serde_json::from_str(EDITOR_SCHEMA).unwrap();
    let editor_nodes: BTreeMap<String, &Value> = editor["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| (n["name"].as_str().unwrap().to_string(), n))
        .collect();
    let contract_nodes: BTreeSet<String> = NODES
        .iter()
        .map(|n| n.name.to_string())
        .filter(|n| !AWAITING_EDITOR.contains(&n.as_str()))
        .collect();
    assert_eq!(
        contract_nodes,
        editor_nodes.keys().cloned().collect::<BTreeSet<_>>(),
        "contract node set != editor-kernel document schema"
    );
    for spec in NODES.iter().filter(|n| !AWAITING_EDITOR.contains(&n.name)) {
        let node = editor_nodes[spec.name];
        assert_eq!(
            node["content"].as_str(),
            spec.content,
            "{}: content expression",
            spec.name
        );
        assert_eq!(node["group"].as_str(), spec.group, "{}: group", spec.name);
        assert_eq!(node["marks"].as_str(), spec.marks, "{}: marks", spec.name);
        assert_eq!(
            node["inline"].as_bool(),
            Some(spec.inline),
            "{}: inline",
            spec.name
        );
        assert_eq!(
            node["atom"].as_bool(),
            Some(spec.atom),
            "{}: atom",
            spec.name
        );
        assert_eq!(
            attr_set(&node["attrs"]),
            spec.attr_names()
                .into_iter()
                .map(str::to_string)
                .collect::<BTreeSet<_>>(),
            "{}: attrs",
            spec.name
        );
    }
    let editor_marks: BTreeMap<String, &Value> = editor["marks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| (m["name"].as_str().unwrap().to_string(), m))
        .collect();
    assert_eq!(
        MARKS
            .iter()
            .map(|m| m.name.to_string())
            .collect::<BTreeSet<_>>(),
        editor_marks.keys().cloned().collect::<BTreeSet<_>>(),
        "contract mark set != editor-kernel document schema"
    );
    for spec in MARKS {
        let mark = editor_marks[spec.name];
        assert_eq!(
            mark["excludes"].as_str(),
            spec.excludes,
            "{}: excludes",
            spec.name
        );
        assert_eq!(
            attr_set(&mark["attrs"]),
            spec.attr_names()
                .into_iter()
                .map(str::to_string)
                .collect::<BTreeSet<_>>(),
            "{}: attrs",
            spec.name
        );
    }
    // Exactly the nodes the editor still lacks, and they are render-only.
    for name in AWAITING_EDITOR {
        assert!(
            !editor_nodes.contains_key(*name),
            "{name} shipped in the editor: drop it from AWAITING_EDITOR"
        );
        assert_eq!(
            block_contract::node_spec(name).unwrap().state,
            block_contract::State::RenderOnly
        );
    }
}

/// Every tiptapJson of the list-loss battery, plus the store specimen shapes.
fn battery_docs() -> Vec<(String, Vec<Value>)> {
    let battery: Vec<Value> = serde_json::from_str(BATTERY).unwrap();
    battery
        .into_iter()
        .filter_map(|shape| {
            let content = shape["body"]["tiptapJson"]["content"].as_array()?.clone();
            Some((shape["id"].as_str().unwrap().to_string(), content))
        })
        .collect()
}

fn texts(node: &Value, out: &mut Vec<String>) {
    if let Some(t) = node["text"].as_str() {
        if !t.trim().is_empty() {
            out.push(t.to_string());
        }
    }
    for child in node["content"].as_array().into_iter().flatten() {
        texts(child, out);
    }
}

fn types(node: &Value, out: &mut BTreeSet<String>) {
    if let Some(t) = node["type"].as_str() {
        out.insert(t.to_string());
    }
    for child in node["content"].as_array().into_iter().flatten() {
        types(child, out);
    }
}

#[test]
fn battery_normalises_to_known_nodes_without_losing_text_and_is_idempotent() {
    for (id, content) in battery_docs() {
        let (normalised, receipts) = block_contract::normalise_doc_content(&content, "battery");
        let doc = json!({"type": "doc", "content": normalised});
        let mut seen = BTreeSet::new();
        types(&doc, &mut seen);
        for t in &seen {
            assert!(
                block_contract::is_known_node(t),
                "{id}: {t} survived normalisation: {doc}"
            );
        }
        let flat = serde_json::to_string(&doc).unwrap();
        let mut original = Vec::new();
        for node in &content {
            texts(node, &mut original);
        }
        for t in original {
            assert!(flat.contains(&t), "{id}: text {t:?} lost: {doc}");
        }
        // Idempotent: normalising the output rewrites nothing.
        let (again, again_receipts) = block_contract::normalise_doc_content(
            doc["content"].as_array().unwrap(),
            "battery-second-pass",
        );
        assert_eq!(
            serde_json::to_string(&again).unwrap(),
            serde_json::to_string(&doc["content"]).unwrap(),
            "{id}: not idempotent"
        );
        assert!(
            again_receipts
                .iter()
                .all(|w| !block_contract::is_rewrite_warning(w)),
            "{id}: second pass rewrote: {again_receipts:?}"
        );
        // Deterministic: the same seed gives the same bytes and receipts.
        let (replay, replay_receipts) = block_contract::normalise_doc_content(&content, "battery");
        assert_eq!(
            replay,
            doc["content"].as_array().unwrap().clone(),
            "{id}: not deterministic"
        );
        assert_eq!(
            replay_receipts, receipts,
            "{id}: receipts not deterministic"
        );
    }
}

#[test]
fn canonical_content_is_a_byte_for_byte_no_op() {
    let canonical = vec![
        json!({"type": "paragraph", "attrs": {"data-block-id": "p", "textAlign": "center"}, "content": [
            {"type": "text", "text": "a", "marks": [{"type": "link", "attrs": {"href": "https://x"}}]},
            {"type": "footnote", "attrs": {"content": "n"}},
            {"type": "tagChip", "attrs": {"name": "t"}},
            {"type": "citation", "attrs": {"citation": "c"}}]}),
        json!({"type": "listItem", "attrs": {"indent": 2, "data-block-id": "l", "listType": "ordered"},
               "content": [{"type": "paragraph", "content": [{"type": "text", "text": "x"}]},
                           {"type": "codeBlock", "attrs": {"language": "rust"}, "content": [{"type": "text", "text": "fn"}]}]}),
        json!({"type": "blockquote", "content": [{"type": "paragraph", "content": [{"type": "text", "text": "q"}]}]}),
        json!({"type": "horizontalRule"}),
        json!({"type": "calendarEvent", "attrs": {"title": "e"}}),
        json!({"type": "opaqueBlock", "attrs": {"data-block-id": "o", "originalType": "callout",
               "originalJson": "{\"type\":\"callout\"}", "text": "", "contractVersion": 1}}),
        json!({"type": "unsupportedBlock", "attrs": {"nodeName": "x"}}),
        json!({"type": "heading", "attrs": {"level": 5}, "content": [{"type": "text", "text": "h5 stays"}]}),
        object_embed(),
        json!({"type": "objectEmbed", "attrs": {"iri": "https://example.org/objects/7"}}),
    ];
    let (out, receipts) = block_contract::normalise_doc_content(&canonical, "canonical");
    assert!(receipts.is_empty(), "{receipts:?}");
    assert_eq!(
        serde_json::to_string(&out).unwrap(),
        serde_json::to_string(&canonical).unwrap()
    );
}

#[test]
fn unknown_block_is_wrapped_byte_exact_and_keeps_its_id() {
    let callout = json!({"type": "callout", "attrs": {"tone": "warn", "data-block-id": "c1"},
        "content": [{"type": "paragraph", "content": [{"type": "text", "text": "Mind the gap"}]},
                    {"type": "paragraph", "content": [{"type": "text", "text": "twice"}]}]});
    let (out, receipts) = block_contract::normalise_doc_content(&[callout.clone()], "op-1");
    assert_eq!(out.len(), 1);
    let attrs = &out[0]["attrs"];
    assert_eq!(out[0]["type"], "opaqueBlock");
    assert_eq!(attrs["originalType"], "callout");
    assert_eq!(
        attrs["originalJson"],
        serde_json::to_string(&callout).unwrap()
    );
    assert_eq!(attrs["text"], "Mind the gap\ntwice");
    assert_eq!(attrs["contractVersion"], CONTRACT_VERSION);
    assert_eq!(attrs["data-block-id"], "c1");
    assert_eq!(receipts.len(), 1);
    assert!(
        receipts[0].starts_with("block-contract unknown content[0]: callout"),
        "{receipts:?}"
    );
    // Once wrapped, the opaque object is canonical: no double wrapping.
    let (again, again_receipts) = block_contract::normalise_doc_content(&out, "op-2");
    assert_eq!(again, out);
    assert!(again_receipts.is_empty());
}

#[test]
fn nested_lists_flatten_with_indent_ids_and_receipts() {
    let list = json!({"type": "orderedList", "attrs": {"data-block-id": "ol"}, "content": [
        {"type": "listItem", "content": [
            {"type": "paragraph", "content": [{"type": "text", "text": "one"}]},
            {"type": "taskList", "content": [
                {"type": "taskItem", "attrs": {"checked": "true"}, "content": [
                    {"type": "paragraph", "content": [{"type": "text", "text": "one.a"}]}]}]}]},
        {"type": "listItem", "attrs": {"data-block-id": "two"}, "content": [{"type": "text", "text": "two"}]}]});
    let (out, receipts) = block_contract::normalise_doc_content(&[list], "op-lists");
    let summary: Vec<(String, String, i64, Value)> = out
        .iter()
        .map(|n| {
            (
                n["attrs"]["listType"].as_str().unwrap().to_string(),
                n["content"][0]["content"][0]["text"]
                    .as_str()
                    .unwrap()
                    .to_string(),
                n["attrs"]["indent"].as_i64().unwrap(),
                n["attrs"]["checked"].clone(),
            )
        })
        .collect();
    assert_eq!(
        summary,
        vec![
            ("ordered".into(), "one".into(), 0, Value::Null),
            ("task".into(), "one.a".into(), 1, json!(true)),
            ("ordered".into(), "two".into(), 0, Value::Null),
        ]
    );
    assert_eq!(
        out[0]["attrs"]["data-block-id"], "ol",
        "container id moves to its first item"
    );
    assert_eq!(out[2]["attrs"]["data-block-id"], "two");
    let minted = out[1]["attrs"]["data-block-id"].as_str().unwrap();
    assert!(
        minted.starts_with("block-") && minted.len() == 14,
        "{minted}"
    );
    assert!(
        receipts[0]
            .starts_with("block-contract known-equivalent content[0]: orderedList flattened to 3"),
        "{receipts:?}"
    );
    assert!(
        receipts
            .iter()
            .any(|w| w.contains("content[0].content[0].content[1]: taskList flattened")),
        "{receipts:?}"
    );
    assert!(
        receipts
            .iter()
            .any(|w| w.contains("content[0].content[1].content[0]")
                && w.contains("wrapped in a paragraph")),
        "{receipts:?}"
    );
}

#[test]
fn block_atoms_in_inline_content_are_hoisted_in_order() {
    let para = json!({"type": "paragraph", "attrs": {"data-block-id": "p"}, "content": [
        {"type": "text", "text": "before "},
        {"type": "mathBlock", "attrs": {"src": "E=mc^2"}},
        {"type": "text", "text": " after"}]});
    let (out, receipts) = block_contract::normalise_doc_content(&[para], "op-hoist");
    let kinds: Vec<&str> = out.iter().map(|n| n["type"].as_str().unwrap()).collect();
    assert_eq!(kinds, ["paragraph", "mathBlock", "paragraph"]);
    assert_eq!(out[0]["attrs"]["data-block-id"], "p");
    assert_eq!(out[0]["content"][0]["text"], "before ");
    assert_eq!(out[2]["content"][0]["text"], " after");
    assert_ne!(
        out[2]["attrs"]["data-block-id"], "p",
        "the second half gets its own id"
    );
    assert!(
        out[1]["attrs"]["data-block-id"].is_string(),
        "the hoisted block gets an id"
    );
    assert_eq!(receipts, vec!["block-contract known-equivalent content[0].content[1]: mathBlock hoisted out of paragraph (block content cannot sit inline)".to_string()]);
}

#[test]
fn unknown_marks_are_preserved_verbatim_and_reported() {
    let para = json!({"type": "paragraph", "content": [
        {"type": "text", "text": "H", "marks": [{"type": "subscript"}]},
        {"type": "text", "text": "2O", "marks": [{"type": "bold"}]}]});
    let (out, receipts) = block_contract::normalise_doc_content(&[para.clone()], "op-marks");
    assert_eq!(out, vec![para]);
    assert_eq!(
        receipts,
        vec![
            "block-contract preserved content[0].content[0]: unknown mark subscript kept verbatim"
                .to_string()
        ]
    );
}

/// The marks the mark-order tests stack on one text run, already in canonical
/// order: three the contract knows (link, bold, italic) and two it does not.
/// `abbr` sorts before every contract mark alphabetically and `superscript`
/// after them, so this sequence can only come from "contract marks in the
/// order of MARKS, then the rest by name", never from a plain alphabetical sort.
const STACKED_MARKS_CANONICAL: [&str; 5] = ["link", "bold", "italic", "abbr", "superscript"];

fn permutations(items: &[&'static str]) -> Vec<Vec<&'static str>> {
    if items.len() <= 1 {
        return vec![items.to_vec()];
    }
    let mut out = Vec::new();
    for (index, head) in items.iter().enumerate() {
        let mut rest = items.to_vec();
        rest.remove(index);
        for mut tail in permutations(&rest) {
            tail.insert(0, *head);
            out.push(tail);
        }
    }
    out
}

/// One paragraph whose single text run carries `marks`, in that order.
fn stacked_paragraph(marks: &[&str]) -> Value {
    let marks: Vec<Value> = marks
        .iter()
        .map(|name| match *name {
            "link" => json!({"type": "link", "attrs": {"href": "https://example.org/"}}),
            other => json!({"type": other}),
        })
        .collect();
    json!({"type": "paragraph", "attrs": {"data-block-id": "p"},
           "content": [{"type": "text", "text": "stacked", "marks": marks}]})
}

/// The normaliser, then the real store round trip every write takes: the marks
/// become Y text attributes (a set) and are read back out of the Y.Doc, the
/// way `materialize_ydoc` builds the stored `tiptapJson`.
fn written_and_read_back(paragraph: &Value, seed: &str) -> Value {
    let (nodes, receipts) =
        block_contract::normalise_doc_content(std::slice::from_ref(paragraph), seed);
    assert!(
        receipts
            .iter()
            .all(|w| !block_contract::is_rewrite_warning(w)),
        "marks are a set the normaliser keeps, not a rewrite: {receipts:?}"
    );
    let doc = super::builder::ydoc_from_tiptap_json(&json!({"type": "doc", "content": nodes}));
    super::projection::ydoc_to_tiptap_json(&doc)
}

/// The types of the marks on the first text run of the first paragraph, in the
/// order the read returned them.
fn run_mark_names(doc: &Value) -> Vec<String> {
    doc["content"][0]["content"][0]["marks"]
        .as_array()
        .unwrap_or_else(|| panic!("the run carries no marks: {doc}"))
        .iter()
        .map(|mark| mark["type"].as_str().unwrap().to_string())
        .collect()
}

#[test]
fn marks_read_back_in_one_canonical_order_whatever_order_they_were_written_in() {
    let canonical: Vec<String> = STACKED_MARKS_CANONICAL
        .iter()
        .map(|m| m.to_string())
        .collect();
    let orders = permutations(&STACKED_MARKS_CANONICAL);
    assert_eq!(orders.len(), 120);
    for order in orders {
        let paragraph = stacked_paragraph(&order);
        // Every projection is a fresh read, and yrs hands a run's formatting
        // back in a std HashMap whose iteration order differs per read; before
        // the canonical sort, the chance that all of these reads (over all 120
        // input orders) came back canonical by luck was nil.
        let read = written_and_read_back(&paragraph, "marks");
        assert_eq!(run_mark_names(&read), canonical, "written as {order:?}");
        for read_number in 1..4 {
            let again = written_and_read_back(&paragraph, "marks");
            assert_eq!(
                run_mark_names(&again),
                canonical,
                "written as {order:?}, read #{read_number}"
            );
        }
        // A fixed point: the read-back normalises to itself without a receipt,
        // and writing it back stores, and reads back, the same document.
        let content = read["content"].as_array().unwrap().clone();
        let (renormalised, receipts) =
            block_contract::normalise_doc_content(&content, "marks-again");
        assert!(
            receipts
                .iter()
                .all(|w| !block_contract::is_rewrite_warning(w)),
            "written as {order:?}: normalising a read-back rewrote it: {receipts:?}"
        );
        assert_eq!(
            renormalised, content,
            "written as {order:?}: normalising a read-back changed it"
        );
        let second = written_and_read_back(&read["content"][0], "marks-again");
        assert_eq!(
            second, read,
            "written as {order:?}: writing a read-back back changed the document"
        );
    }
}

/// The pair from CodeBuild b3236846 (2026-10-02T23:58Z): the store test wrote
/// `[superscript, bold]`, read it back as `[superscript, bold]`, wrote that
/// back and read `[bold, superscript]`. The same source (c60f057e2762, tree
/// 5c62c3125fe9) had passed in build 9d141d69 (2026-09-27T01:33Z): the order
/// was a coin flip per read, not a rule.
#[test]
fn the_pair_that_failed_on_the_box_reads_back_bold_then_superscript_every_time() {
    let paragraph = stacked_paragraph(&["superscript", "bold"]);
    for read_number in 0..64 {
        let read = written_and_read_back(&paragraph, "pair");
        assert_eq!(
            run_mark_names(&read),
            ["bold", "superscript"],
            "read #{read_number}"
        );
    }
}

fn by_canonical_order(a: &String, b: &String) -> std::cmp::Ordering {
    block_contract::canonical_mark_cmp(a, b)
}

#[test]
fn canonical_mark_order_is_the_contract_marks_list_then_other_marks_by_name() {
    // The committed contract states the order: its `marks` list, which is MARKS.
    let contract: Value = serde_json::from_str(CONTRACT_FILE).unwrap();
    let listed: Vec<String> = contract["marks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|mark| mark["name"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        listed,
        MARKS.iter().map(|m| m.name.to_string()).collect::<Vec<_>>()
    );
    let mut expected = listed.clone();
    expected.extend(["aaa", "kbd", "wireMark", "zzz"].map(String::from));
    // Every rotation, and the reverse, of the whole sequence sorts back to it.
    for shift in 0..expected.len() {
        let mut rotated = expected.clone();
        rotated.rotate_left(shift);
        rotated.sort_by(by_canonical_order);
        assert_eq!(rotated, expected, "rotated by {shift}");
    }
    let mut reversed: Vec<String> = expected.iter().rev().cloned().collect();
    reversed.sort_by(by_canonical_order);
    assert_eq!(reversed, expected);
    // A total order: antisymmetric, and equal only to itself.
    for a in &expected {
        for b in &expected {
            assert_eq!(
                by_canonical_order(a, b),
                by_canonical_order(b, a).reverse(),
                "{a} vs {b}"
            );
            assert_eq!(
                by_canonical_order(a, b) == std::cmp::Ordering::Equal,
                a == b,
                "{a} vs {b}"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// `canonical_mark_order`: the form in which a stored `tiptapJson` is compared
// with a fresh projection of its own Y.Doc (`projection_semantics_match`).
// ---------------------------------------------------------------------------

fn doc_of(content: Vec<Value>) -> Value {
    json!({"type": "doc", "content": content})
}

#[test]
fn a_tree_is_put_in_one_mark_order_whatever_order_its_marks_were_stored_in() {
    let canonical = doc_of(vec![stacked_paragraph(&STACKED_MARKS_CANONICAL)]);
    let orders = permutations(&STACKED_MARKS_CANONICAL);
    assert_eq!(orders.len(), 120);
    for order in orders {
        let stored = doc_of(vec![stacked_paragraph(&order)]);
        let put_in_order = block_contract::canonical_mark_order(&stored);
        assert_eq!(put_in_order, canonical, "stored as {order:?}");
        // Byte for byte, not only equal as JSON values.
        assert_eq!(
            serde_json::to_string(&put_in_order).unwrap(),
            serde_json::to_string(&canonical).unwrap(),
            "stored as {order:?}"
        );
    }
}

#[test]
fn mark_order_sorts_each_marks_array_in_the_node_tree_and_changes_nothing_else() {
    let stored = json!({
        "type": "doc",
        "content": [
            {"type": "paragraph", "attrs": {"data-block-id": "a"}, "content": [
                {"type": "text", "text": "one ", "marks": [{"type": "italic"}, {"type": "bold"}]},
                {"type": "text", "text": "two", "marks": [
                    {"type": "superscript"},
                    {"type": "link", "attrs": {"href": "https://example.org/", "title": "t"}},
                    {"type": "bold"}
                ]},
                {"type": "text", "text": " three"}
            ]},
            {"type": "blockquote", "attrs": {"data-block-id": "b"}, "content": [
                {"type": "paragraph", "attrs": {"data-block-id": "c"}, "content": [
                    {"type": "text", "text": "nested", "marks": [{"type": "underline"}, {"type": "code"}]}
                ]}
            ]},
            {"type": "listItem", "attrs": {"data-block-id": "d", "listType": "bullet", "indent": 0}, "content": [
                {"type": "paragraph", "attrs": {"data-block-id": "e"}, "content": [
                    {"type": "text", "text": "item", "marks": [
                        {"type": "strike"},
                        {"type": "textStyle", "attrs": {"fontSize": "14px"}}
                    ]}
                ]}
            ]}
        ]
    });
    // The same tree with exactly these four marks arrays replaced, and nothing else.
    let mut expected = stored.clone();
    expected["content"][0]["content"][0]["marks"] = json!([{"type": "bold"}, {"type": "italic"}]);
    expected["content"][0]["content"][1]["marks"] = json!([
        {"type": "link", "attrs": {"href": "https://example.org/", "title": "t"}},
        {"type": "bold"},
        {"type": "superscript"}
    ]);
    expected["content"][1]["content"][0]["content"][0]["marks"] =
        json!([{"type": "code"}, {"type": "underline"}]);
    expected["content"][2]["content"][0]["content"][0]["marks"] = json!([
        {"type": "textStyle", "attrs": {"fontSize": "14px"}},
        {"type": "strike"}
    ]);
    assert_ne!(stored, expected, "the fixture must not already be in order");

    let put_in_order = block_contract::canonical_mark_order(&stored);
    assert_eq!(put_in_order, expected);
    // Byte for byte: key order, text and attrs all as they were.
    assert_eq!(
        serde_json::to_string(&put_in_order).unwrap(),
        serde_json::to_string(&expected).unwrap()
    );
    // Idempotent, and a tree already in order comes back byte-identical.
    let again = block_contract::canonical_mark_order(&put_in_order);
    assert_eq!(
        serde_json::to_string(&again).unwrap(),
        serde_json::to_string(&put_in_order).unwrap()
    );
}

#[test]
fn mark_order_leaves_marks_inside_attrs_as_written() {
    let stored = json!({"type": "doc", "content": [
        {"type": "queryBlock", "attrs": {
            "data-block-id": "q",
            "spec": {"marks": [{"type": "italic"}, {"type": "bold"}]}
        }},
        {"type": "paragraph", "attrs": {
            "data-block-id": "p",
            "marks": [{"type": "italic"}, {"type": "bold"}]
        }, "content": [
            {"type": "text", "text": "x", "marks": [{"type": "italic"}, {"type": "bold"}]}
        ]}
    ]});
    let put_in_order = block_contract::canonical_mark_order(&stored);
    // The run's marks are sorted ...
    assert_eq!(
        put_in_order["content"][1]["content"][0]["marks"],
        json!([{"type": "bold"}, {"type": "italic"}])
    );
    // ... a `marks` key inside attrs is data, not a run's formatting, and stays as written.
    assert_eq!(
        put_in_order["content"][0]["attrs"]["spec"]["marks"],
        json!([{"type": "italic"}, {"type": "bold"}])
    );
    assert_eq!(
        put_in_order["content"][1]["attrs"]["marks"],
        json!([{"type": "italic"}, {"type": "bold"}])
    );
    // So the run is the one thing that changed.
    let mut expected = stored.clone();
    expected["content"][1]["content"][0]["marks"] = json!([{"type": "bold"}, {"type": "italic"}]);
    assert_eq!(put_in_order, expected);
}

#[test]
fn mark_order_is_stable_and_total_over_marks_without_a_known_type() {
    let run = |marks: Value| {
        doc_of(vec![json!({"type": "paragraph", "content": [
            {"type": "text", "text": "x", "marks": marks}
        ]})])
    };
    // A mark with no string `type` sorts as the empty name, first among the marks
    // the contract does not know; marks of one name keep the order they had (a
    // stable sort by name, never by attrs).
    let stored = run(json!([
        {"type": 5},
        {},
        {"type": "bold"},
        {"type": "zzz"},
        {"type": "link", "attrs": {"href": "b"}},
        {"type": "link", "attrs": {"href": "a"}}
    ]));
    let expected = run(json!([
        {"type": "link", "attrs": {"href": "b"}},
        {"type": "link", "attrs": {"href": "a"}},
        {"type": "bold"},
        {"type": 5},
        {},
        {"type": "zzz"}
    ]));
    assert_eq!(block_contract::canonical_mark_order(&stored), expected);
}

#[test]
fn mark_order_passes_through_whatever_is_not_a_node_tree() {
    for scalar in [json!(null), json!(true), json!(5), json!("marks")] {
        assert_eq!(block_contract::canonical_mark_order(&scalar), scalar);
    }
    // `marks` that is not an array, and `content` that is not an array, stay as written.
    let odd_content = json!({"type": "paragraph", "marks": null, "content": "not an array"});
    assert_eq!(
        block_contract::canonical_mark_order(&odd_content),
        odd_content
    );
    let odd_marks = json!({"type": "text", "text": "x", "marks": "bold"});
    assert_eq!(block_contract::canonical_mark_order(&odd_marks), odd_marks);
    // The empty cases.
    for empty in [json!({}), json!([]), json!({"type": "text", "marks": []})] {
        assert_eq!(block_contract::canonical_mark_order(&empty), empty);
    }
    // A bare list of nodes is walked like the content of one.
    let stored =
        json!([{"type": "text", "text": "x", "marks": [{"type": "italic"}, {"type": "bold"}]}]);
    assert_eq!(
        block_contract::canonical_mark_order(&stored),
        json!([{"type": "text", "text": "x", "marks": [{"type": "bold"}, {"type": "italic"}]}])
    );
}

#[test]
fn markdown_export_renders_opaque_blocks_as_fenced_json() {
    let doc = json!({"type": "doc", "content": [
        {"type": "opaqueBlock", "attrs": {"originalType": "callout", "originalJson": "{\"type\":\"callout\",\"x\":\"```\"}", "text": "t", "contractVersion": 1}}]});
    let md = super::markdown_export::tiptap_json_to_markdown(&doc);
    assert_eq!(md, "````json\n{\"type\":\"callout\",\"x\":\"```\"}\n````");
}

#[test]
fn container_ids_are_never_silently_dropped() {
    // The first item has its own id: the container's id cannot move, and the
    // receipt says so.
    let list = json!({"type": "bulletList", "attrs": {"data-block-id": "bl"}, "content": [
        {"type": "listItem", "attrs": {"data-block-id": "own"}, "content": [
            {"type": "paragraph", "content": [{"type": "text", "text": "a"}]}]}]});
    let (out, receipts) = block_contract::normalise_doc_content(&[list], "op-ids");
    assert_eq!(out[0]["attrs"]["data-block-id"], "own");
    assert!(receipts.iter().any(|w| w == "block-contract preserved content[0]: bulletList id bl not kept: its first item carries its own id"), "{receipts:?}");
    // An empty container keeps its place and id as one empty item.
    let empty = json!({"type": "orderedList", "attrs": {"data-block-id": "ol"}, "content": []});
    let (out, _) = block_contract::normalise_doc_content(&[empty], "op-empty");
    assert_eq!(
        out,
        vec![
            json!({"type": "listItem", "attrs": {"listType": "ordered", "indent": 0, "data-block-id": "ol"},
                                "content": [{"type": "paragraph"}]})
        ]
    );
}

// ---------------------------------------------------------------------------
// objectEmbed (contract 2): a block atom that references a typed object by IRI
// ---------------------------------------------------------------------------

const DREAM_IRI: &str = "urn:sophia:type:freud-dreams:workspace#DreamReport-meier";

/// The canonical embed, every attr the contract names (§4's "Embed block").
fn object_embed() -> Value {
    json!({"type": "objectEmbed", "attrs": {
        "data-block-id": "emb",
        "iri": DREAM_IRI,
        "graphId": "freud-dreams",
        "slot": "card",
        "label": "Meier: straw between the toes",
        "classLabel": "Dream report"}})
}

fn embed_with(attrs: Value) -> Value {
    json!({"type": "objectEmbed", "attrs": attrs})
}

#[test]
fn object_embed_is_a_minted_render_only_block_atom_the_editor_carries() {
    let spec = block_contract::node_spec("objectEmbed").expect("objectEmbed is in the contract");
    assert_eq!(spec.group, Some("block"));
    assert_eq!(spec.content, None);
    assert!(spec.atom && !spec.inline);
    assert_eq!(spec.state, block_contract::State::RenderOnly);
    assert_eq!(
        spec.attr_names(),
        [
            "classLabel",
            "data-block-id",
            "graphId",
            "iri",
            "label",
            "slot"
        ]
    );
    assert!(block_contract::is_block_id_minted("objectEmbed"));
    // The 319e0253 editor schema ships it (and opaqueBlock): nothing waits.
    assert!(
        AWAITING_EDITOR.is_empty(),
        "{AWAITING_EDITOR:?} still awaiting the editor"
    );
    assert_eq!(CONTRACT_VERSION, 2, "contract 2 learned objectEmbed");
    // The emitted contract says what the node means beyond its attr names.
    let contract: Value = serde_json::from_str(CONTRACT_FILE).unwrap();
    assert_eq!(
        contract["objectEmbed"]["rdfEdge"],
        "http://mnemosyne.dev/doc#embeds"
    );
    let node = contract["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["name"] == "objectEmbed")
        .unwrap();
    assert_eq!(node["blockId"], "minted");
}

#[test]
fn canonical_object_embed_passes_byte_for_byte_and_is_idempotent() {
    let content = vec![
        object_embed(),
        embed_with(json!({"iri": "https://example.org/objects/7", "slot": "inline"})),
        embed_with(json!({"iri": "mailto:vera@example.org", "slot": "pane", "label": ""})),
    ];
    let (out, receipts) = block_contract::normalise_doc_content(&content, "embed");
    assert!(receipts.is_empty(), "{receipts:?}");
    assert_eq!(
        serde_json::to_string(&out).unwrap(),
        serde_json::to_string(&content).unwrap()
    );
    let (again, again_receipts) = block_contract::normalise_doc_content(&out, "embed-again");
    assert_eq!(again, out);
    assert!(again_receipts.is_empty());
}

#[test]
fn an_object_embed_without_an_absolute_iri_is_wrapped_opaque_with_a_receipt() {
    for (case, attrs, why) in [
        (
            "missing",
            json!({"data-block-id": "e1", "label": "orphan"}),
            "has no iri",
        ),
        (
            "null",
            json!({"data-block-id": "e2", "iri": null}),
            "has no iri",
        ),
        (
            "empty",
            json!({"data-block-id": "e3", "iri": ""}),
            "has an iri that is not absolute (\"\")",
        ),
        (
            "relative",
            json!({"data-block-id": "e4", "iri": "dream-report-7", "label": "Meier"}),
            "has an iri that is not absolute (\"dream-report-7\")",
        ),
        (
            "path",
            json!({"data-block-id": "e5", "iri": "/objects/7"}),
            "has an iri that is not absolute (\"/objects/7\")",
        ),
        (
            "spaces",
            json!({"data-block-id": "e6", "iri": "urn:has spaces"}),
            "has an iri that is not absolute (\"urn:has spaces\")",
        ),
        (
            "number",
            json!({"data-block-id": "e7", "iri": 7}),
            "has an iri that is not a string",
        ),
    ] {
        let node = embed_with(attrs);
        let (out, receipts) = block_contract::normalise_doc_content(&[node.clone()], case);
        assert_eq!(out.len(), 1, "{case}");
        let opaque = &out[0];
        assert_eq!(opaque["type"], "opaqueBlock", "{case}: {opaque}");
        assert_eq!(opaque["attrs"]["originalType"], "objectEmbed", "{case}");
        assert_eq!(
            opaque["attrs"]["originalJson"],
            serde_json::to_string(&node).unwrap(),
            "{case}: the embed is kept byte-exact, never deleted"
        );
        assert_eq!(
            opaque["attrs"]["contractVersion"], CONTRACT_VERSION,
            "{case}"
        );
        assert_eq!(
            opaque["attrs"]["data-block-id"], node["attrs"]["data-block-id"],
            "{case}: the block keeps its id"
        );
        // The wrapper's display text is the embed's label, else its iri.
        let shown = match (&node["attrs"]["label"], &node["attrs"]["iri"]) {
            (Value::String(label), _) => label.clone(),
            (_, Value::String(iri)) => iri.clone(),
            (_, Value::Number(n)) => n.to_string(),
            _ => String::new(),
        };
        assert_eq!(opaque["attrs"]["text"], shown, "{case}");
        assert_eq!(
            receipts,
            vec![format!(
                "block-contract unknown content[0]: objectEmbed {why}; wrapped as opaqueBlock (originalJson kept byte-exact)"
            )],
            "{case}"
        );
        // The wrapper is canonical: a second pass is a no-op.
        let (again, again_receipts) = block_contract::normalise_doc_content(&out, "second");
        assert_eq!(again, out, "{case}");
        assert!(again_receipts.is_empty(), "{case}: {again_receipts:?}");
    }
}

#[test]
fn an_object_embed_carrying_content_is_wrapped_opaque_like_any_atom() {
    let node = json!({"type": "objectEmbed", "attrs": {"iri": DREAM_IRI},
        "content": [{"type": "paragraph", "content": [{"type": "text", "text": "smuggled"}]}]});
    let (out, receipts) = block_contract::normalise_doc_content(&[node], "leaf");
    assert_eq!(out[0]["type"], "opaqueBlock");
    assert_eq!(out[0]["attrs"]["text"], "smuggled", "text kept for display");
    assert!(
        receipts[0].contains("objectEmbed is a leaf node carrying content"),
        "{receipts:?}"
    );
}

#[test]
fn an_object_embed_inside_a_paragraph_is_hoisted_beside_it() {
    let para = json!({"type": "paragraph", "attrs": {"data-block-id": "p"}, "content": [
        {"type": "text", "text": "see "},
        object_embed(),
        {"type": "text", "text": " above"}]});
    let (out, receipts) = block_contract::normalise_doc_content(&[para], "hoist");
    let kinds: Vec<&str> = out.iter().map(|n| n["type"].as_str().unwrap()).collect();
    assert_eq!(kinds, ["paragraph", "objectEmbed", "paragraph"]);
    assert_eq!(out[1], object_embed(), "the embed itself is untouched");
    assert_eq!(receipts.len(), 1, "{receipts:?}");
    assert!(receipts[0].contains("objectEmbed hoisted out of paragraph"));
}

#[test]
fn an_object_embed_slot_outside_the_three_is_kept_with_a_note() {
    let node = embed_with(json!({"data-block-id": "s", "iri": DREAM_IRI, "slot": "billboard"}));
    let (out, receipts) = block_contract::normalise_doc_content(&[node.clone()], "slot");
    assert_eq!(out, vec![node], "kept as written");
    assert_eq!(
        receipts,
        vec!["block-contract preserved content[0]: objectEmbed slot \"billboard\" is not inline, card or pane; kept, read as card".to_string()]
    );
    assert!(receipts
        .iter()
        .all(|w| !block_contract::is_rewrite_warning(w)));
}

#[test]
fn markdown_export_renders_object_embeds_as_one_quoted_line() {
    let md = |attrs: Value| {
        super::markdown_export::tiptap_json_to_markdown(
            &json!({"type": "doc", "content": [embed_with(attrs)]}),
        )
    };
    assert_eq!(
        md(object_embed()["attrs"].clone()),
        format!("> ⧉ Dream report — Meier: straw between the toes <{DREAM_IRI}>")
    );
    // Fallbacks: a missing label drops its part, never the line.
    assert_eq!(
        md(json!({"iri": DREAM_IRI, "classLabel": "Dream report"})),
        format!("> ⧉ Dream report <{DREAM_IRI}>")
    );
    assert_eq!(
        md(json!({"iri": DREAM_IRI, "label": "Meier", "classLabel": "  "})),
        format!("> ⧉ Meier <{DREAM_IRI}>")
    );
    assert_eq!(md(json!({"iri": DREAM_IRI})), format!("> ⧉ <{DREAM_IRI}>"));
    // Labels are one line of escaped text.
    assert_eq!(
        md(json!({"iri": "https://example.org/x", "label": "a *b*\nc", "classLabel": "T"})),
        "> ⧉ T — a \\*b\\* c <https://example.org/x>"
    );
    // A value that is not an autolink (an update_blocks or Y-sync write) is a
    // code span; an embed with no IRI at all says so.
    assert_eq!(
        md(json!({"iri": "dream-7", "label": "Meier"})),
        "> ⧉ Meier `dream-7`"
    );
    assert_eq!(md(json!({"label": "Meier"})), "> ⧉ Meier (no iri)");
    // Between paragraphs, and inside a quote, it is its own block.
    let doc = json!({"type": "doc", "content": [
        {"type": "paragraph", "content": [{"type": "text", "text": "Before."}]},
        object_embed(),
        {"type": "blockquote", "content": [object_embed()]},
        {"type": "paragraph", "content": [{"type": "text", "text": "After."}]}]});
    let line = format!("⧉ Dream report — Meier: straw between the toes <{DREAM_IRI}>");
    assert_eq!(
        super::markdown_export::tiptap_json_to_markdown(&doc),
        format!("Before.\n\n> {line}\n\n> > {line}\n\nAfter.")
    );
    // In a table cell it is the same line, inline.
    let table = json!({"type": "doc", "content": [{"type": "table", "content": [
        {"type": "tableRow", "content": [
            {"type": "tableHeader", "content": [{"type": "paragraph", "content": [{"type": "text", "text": "Object"}]}]}]},
        {"type": "tableRow", "content": [
            {"type": "tableCell", "content": [object_embed()]}]}]}]});
    let rendered = super::markdown_export::tiptap_json_to_markdown(&table);
    assert!(rendered.contains(&format!("| {line} |")), "{rendered}");
}

/// What the block projection (read_blocks, get_block, the plain-text body)
/// and the tree (RDF) see of an embed.
#[test]
fn object_embed_projects_as_an_object_block_whose_text_is_its_label_else_its_iri() {
    let doc = json!({"type": "doc", "content": [
        {"type": "paragraph", "attrs": {"data-block-id": "p"}, "content": [{"type": "text", "text": "Lead."}]},
        object_embed(),
        embed_with(json!({"data-block-id": "bare", "iri": DREAM_IRI, "label": " "}))]});
    let snapshot = super::projection::materialize_tiptap_json(&doc, "doc-embed");
    let blocks = snapshot.blocks_json.as_array().unwrap();
    assert_eq!(blocks.len(), 3, "both embeds are blocks: {blocks:?}");
    assert_eq!(blocks[1]["id"], "emb");
    assert_eq!(blocks[1]["type"], "object");
    assert_eq!(blocks[1]["content"], "Meier: straw between the toes");
    assert_eq!(blocks[2]["type"], "object");
    assert_eq!(blocks[2]["content"], DREAM_IRI, "no label: the IRI");
    assert!(
        snapshot.body.contains("Meier: straw between the toes"),
        "{}",
        snapshot.body
    );
    let embed = &snapshot.tree_json["root"]["children"][1];
    assert_eq!(embed["tagName"], "objectEmbed");
    assert_eq!(embed["attributes"]["blockId"], "emb");
    assert_eq!(embed["attributes"]["extra"]["iri"], DREAM_IRI);
    assert_eq!(embed["attributes"]["extra"]["classLabel"], "Dream report");
}

/// Y.Doc ↔ TipTap JSON ↔ tiptapXml: the embed survives every face of the
/// store's own representation, attrs exact.
#[test]
fn object_embed_round_trips_through_the_ydoc_and_tiptap_xml() {
    let doc = json!({"type": "doc", "content": [
        {"type": "paragraph", "attrs": {"data-block-id": "p"}, "content": [{"type": "text", "text": "Lead."}]},
        object_embed(),
        // XML-hostile attribute values: quotes, angle brackets, ampersands.
        embed_with(json!({"data-block-id": "hostile", "iri": "https://example.org/o?a=1&b=2",
                          "label": "\"Straw\" <between> & toes", "classLabel": "Dream report", "slot": "inline"}))]});
    let ydoc = super::builder::ydoc_from_tiptap_json(&doc);
    let read = super::projection::ydoc_to_tiptap_json(&ydoc);
    assert_eq!(read, doc, "Y.Doc -> TipTap JSON keeps the embed exactly");

    let xml = super::projection::ydoc_to_tiptap_xml(&ydoc);
    assert!(
        xml.contains(&format!(
            "<objectEmbed classLabel=\"Dream report\" data-block-id=\"emb\" graphId=\"freud-dreams\" iri=\"{DREAM_IRI}\" label=\"Meier: straw between the toes\" slot=\"card\"></objectEmbed>"
        )),
        "{xml}"
    );
    let parsed = super::content_parse::tiptap_xml_to_tiptap_json(&xml).unwrap();
    assert_eq!(
        parsed, doc,
        "tiptapXml -> TipTap JSON keeps the embed exactly"
    );

    // And the parsed form is canonical: the normaliser leaves it alone.
    let (normalised, receipts) =
        block_contract::normalise_doc_content(parsed["content"].as_array().unwrap(), "xml");
    assert!(receipts.is_empty(), "{receipts:?}");
    assert_eq!(normalised, *doc["content"].as_array().unwrap());
    // A second trip through a fresh Y.Doc is the same XML (a fixed point).
    let again = super::builder::ydoc_from_tiptap_json(&parsed);
    assert_eq!(super::projection::ydoc_to_tiptap_xml(&again), xml);
}

/// The content-string XML path (`write_document(content, format: "xml")`)
/// mints the embed's block id, as it does for every minted type.
#[test]
fn the_xml_content_parser_mints_an_object_embed_block_id() {
    let parsed = super::content_parse::parse_write_content_for_operation(
        &format!("<paragraph>Lead.</paragraph><objectEmbed iri=\"{DREAM_IRI}\" slot=\"card\" label=\"Meier\"></objectEmbed>"),
        Some("xml"),
        "op-xml",
    )
    .unwrap();
    let embed = &parsed.tiptap_json["content"][1];
    assert_eq!(embed["type"], "objectEmbed", "{}", parsed.tiptap_json);
    assert_eq!(embed["attrs"]["iri"], DREAM_IRI);
    assert_eq!(embed["attrs"]["label"], "Meier");
    let id = embed["attrs"]["data-block-id"].as_str().unwrap();
    assert!(id.starts_with("block-"), "{id}");
}

/// The editor's attr defaults (Shrubbery 319e0253) read the way gardend reads
/// an embed: `slot` "card" is the reader's default, `graphId` null is "this
/// graph", empty labels are absent, and the default `iri` "" is no IRI at all,
/// so an embed inserted without one never becomes a canonical embed through a
/// JSON write.
#[test]
fn the_editor_object_embed_defaults_read_as_the_contract_reads_them() {
    let editor: Value = serde_json::from_str(EDITOR_SCHEMA).unwrap();
    let node = |name: &str| {
        editor["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|n| n["name"] == name)
            .unwrap_or_else(|| panic!("{name} not in the editor schema"))
            .clone()
    };
    let embed = node("objectEmbed");
    let defaults: serde_json::Map<String, Value> = embed["attrs"]
        .as_object()
        .unwrap()
        .iter()
        .map(|(key, attr)| (key.clone(), attr["default"].clone()))
        .collect();
    assert_eq!(
        Value::Object(defaults.clone()),
        json!({"classLabel": "", "data-block-id": null, "graphId": null, "iri": "", "label": "", "slot": "card"})
    );
    assert!(block_contract::OBJECT_EMBED_SLOTS.contains(&"card"));

    // All defaults, as the editor would serialize a bare insert: not an embed.
    let bare = json!({"type": "objectEmbed", "attrs": defaults});
    let (out, receipts) = block_contract::normalise_doc_content(&[bare], "defaults");
    assert_eq!(out[0]["type"], "opaqueBlock");
    assert!(receipts[0].contains("objectEmbed has an iri that is not absolute (\"\")"));

    // Defaults plus an IRI: canonical, and the face reads only the IRI.
    let mut attrs = defaults.clone();
    attrs.insert("iri".into(), json!(DREAM_IRI));
    attrs.insert("data-block-id".into(), json!("emb"));
    let with_iri = json!({"type": "objectEmbed", "attrs": attrs});
    let (out, receipts) = block_contract::normalise_doc_content(&[with_iri.clone()], "iri");
    assert!(receipts.is_empty(), "{receipts:?}");
    assert_eq!(out, vec![with_iri.clone()]);
    assert_eq!(
        super::markdown_export::tiptap_json_to_markdown(
            &json!({"type": "doc", "content": [with_iri]})
        ),
        format!("> ⧉ <{DREAM_IRI}>")
    );
}

/// `dataTable` (Hoja's tabular atom) mirrors the editor: canonical through a
/// JSON write (contract 1 wrapped it as opaque), and the markdown face keeps a
/// line for it instead of dropping it.
#[test]
fn data_table_is_canonical_and_never_dropped_from_markdown() {
    let spec = block_contract::node_spec("dataTable").expect("dataTable is in the contract");
    assert_eq!(spec.group, Some("block"));
    assert!(spec.atom && spec.content.is_none());
    assert_eq!(spec.attr_names(), ["caption", "data", "ref"]);
    assert!(!block_contract::is_block_id_minted("dataTable"));
    let tables = vec![
        json!({"type": "dataTable", "attrs": {"caption": "Sales by month", "ref": "data-table-1-x", "data": null}}),
        json!({"type": "dataTable", "attrs": {"caption": null, "data": {"sheets": [{"name": "S", "columns": ["a"], "rows": [["1"]]}]}}}),
        json!({"type": "dataTable"}),
    ];
    let (out, receipts) = block_contract::normalise_doc_content(&tables, "tables");
    assert!(receipts.is_empty(), "{receipts:?}");
    assert_eq!(out, tables);
    let md = super::markdown_export::tiptap_json_to_markdown(&json!({"type": "doc", "content": [
        {"type": "paragraph", "content": [{"type": "text", "text": "Before."}]},
        tables[0], tables[1],
        {"type": "dataTable", "attrs": {"caption": "# not *a* heading"}}]}));
    assert_eq!(
        md,
        "Before.\n\nSales by month (table)\n\n(table)\n\n\\# not \\*a\\* heading (table)"
    );
}
