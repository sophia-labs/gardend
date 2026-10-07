//! Block contract: the emitted document, parity with the web editor's
//! schema, and the normaliser's laws (canonical = byte-for-byte no-op,
//! idempotent, deterministic, never loses text). The store-level proofs over
//! every write path are in `block_contract_store_tests.rs`.
use super::block_contract::{self, AWAITING_EDITOR, CONTRACT_VERSION, MARKS, NODES};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};

const EDITOR_SCHEMA: &str =
    include_str!("../../tests/fixtures/block_contract/editor-kernel-9eece44.schema.json");
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
    assert_eq!(format!("{:x}", sha2::Sha256::digest(compact.as_bytes())), declared);
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
        assert_eq!(node["content"].as_str(), spec.content, "{}: content expression", spec.name);
        assert_eq!(node["group"].as_str(), spec.group, "{}: group", spec.name);
        assert_eq!(node["marks"].as_str(), spec.marks, "{}: marks", spec.name);
        assert_eq!(node["inline"].as_bool(), Some(spec.inline), "{}: inline", spec.name);
        assert_eq!(node["atom"].as_bool(), Some(spec.atom), "{}: atom", spec.name);
        assert_eq!(
            attr_set(&node["attrs"]),
            spec.attr_names().into_iter().map(str::to_string).collect::<BTreeSet<_>>(),
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
        MARKS.iter().map(|m| m.name.to_string()).collect::<BTreeSet<_>>(),
        editor_marks.keys().cloned().collect::<BTreeSet<_>>(),
        "contract mark set != editor-kernel document schema"
    );
    for spec in MARKS {
        let mark = editor_marks[spec.name];
        assert_eq!(mark["excludes"].as_str(), spec.excludes, "{}: excludes", spec.name);
        assert_eq!(
            attr_set(&mark["attrs"]),
            spec.attr_names().into_iter().map(str::to_string).collect::<BTreeSet<_>>(),
            "{}: attrs",
            spec.name
        );
    }
    // Exactly the nodes the editor still lacks, and they are render-only.
    for name in AWAITING_EDITOR {
        assert!(!editor_nodes.contains_key(*name), "{name} shipped in the editor: drop it from AWAITING_EDITOR");
        assert_eq!(block_contract::node_spec(name).unwrap().state, block_contract::State::RenderOnly);
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
            assert!(block_contract::is_known_node(t), "{id}: {t} survived normalisation: {doc}");
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
        assert_eq!(serde_json::to_string(&again).unwrap(), serde_json::to_string(&doc["content"]).unwrap(), "{id}: not idempotent");
        assert!(
            again_receipts.iter().all(|w| !block_contract::is_rewrite_warning(w)),
            "{id}: second pass rewrote: {again_receipts:?}"
        );
        // Deterministic: the same seed gives the same bytes and receipts.
        let (replay, replay_receipts) = block_contract::normalise_doc_content(&content, "battery");
        assert_eq!(replay, doc["content"].as_array().unwrap().clone(), "{id}: not deterministic");
        assert_eq!(replay_receipts, receipts, "{id}: receipts not deterministic");
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
    assert_eq!(attrs["originalJson"], serde_json::to_string(&callout).unwrap());
    assert_eq!(attrs["text"], "Mind the gap\ntwice");
    assert_eq!(attrs["contractVersion"], CONTRACT_VERSION);
    assert_eq!(attrs["data-block-id"], "c1");
    assert_eq!(receipts.len(), 1);
    assert!(receipts[0].starts_with("block-contract unknown content[0]: callout"), "{receipts:?}");
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
                n["content"][0]["content"][0]["text"].as_str().unwrap().to_string(),
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
    assert_eq!(out[0]["attrs"]["data-block-id"], "ol", "container id moves to its first item");
    assert_eq!(out[2]["attrs"]["data-block-id"], "two");
    let minted = out[1]["attrs"]["data-block-id"].as_str().unwrap();
    assert!(minted.starts_with("block-") && minted.len() == 14, "{minted}");
    assert!(receipts[0].starts_with("block-contract known-equivalent content[0]: orderedList flattened to 3"), "{receipts:?}");
    assert!(receipts.iter().any(|w| w.contains("content[0].content[0].content[1]: taskList flattened")), "{receipts:?}");
    assert!(receipts.iter().any(|w| w.contains("content[0].content[1].content[0]") && w.contains("wrapped in a paragraph")), "{receipts:?}");
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
    assert_ne!(out[2]["attrs"]["data-block-id"], "p", "the second half gets its own id");
    assert!(out[1]["attrs"]["data-block-id"].is_string(), "the hoisted block gets an id");
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
        vec!["block-contract preserved content[0].content[0]: unknown mark subscript kept verbatim".to_string()]
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
        receipts.iter().all(|w| !block_contract::is_rewrite_warning(w)),
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
    let canonical: Vec<String> = STACKED_MARKS_CANONICAL.iter().map(|m| m.to_string()).collect();
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
        let (renormalised, receipts) = block_contract::normalise_doc_content(&content, "marks-again");
        assert!(
            receipts.iter().all(|w| !block_contract::is_rewrite_warning(w)),
            "written as {order:?}: normalising a read-back rewrote it: {receipts:?}"
        );
        assert_eq!(renormalised, content, "written as {order:?}: normalising a read-back changed it");
        let second = written_and_read_back(&read["content"][0], "marks-again");
        assert_eq!(second, read, "written as {order:?}: writing a read-back back changed the document");
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
        assert_eq!(run_mark_names(&read), ["bold", "superscript"], "read #{read_number}");
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
    assert_eq!(listed, MARKS.iter().map(|m| m.name.to_string()).collect::<Vec<_>>());
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
            assert_eq!(by_canonical_order(a, b), by_canonical_order(b, a).reverse(), "{a} vs {b}");
            assert_eq!(by_canonical_order(a, b) == std::cmp::Ordering::Equal, a == b, "{a} vs {b}");
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
    assert_eq!(out, vec![json!({"type": "listItem", "attrs": {"listType": "ordered", "indent": 0, "data-block-id": "ol"},
                                "content": [{"type": "paragraph"}]})]);
}
