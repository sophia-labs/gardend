//! Tests for `minimal_edit::emit` (owner U5; DESIGN §4.3).
//!
//! Plans are written by hand as `Step` literals, so these tests do not depend on the planner. The current
//! model is read with `model::read_fragment` in the same transaction that emits, as the write path does.
//! Everything runs on real yrs Docs built by `builder::ydoc_from_tiptap_json`.

use std::collections::BTreeMap;

use serde_json::{json, Value};
use yrs::branch::{Branch, BranchID};
use yrs::updates::decoder::Decode;
use yrs::{
    Doc, ReadTxn, StateVector, Text, Transact, Update, XmlFragment, XmlFragmentRef, XmlOut,
    XmlTextRef,
};

use super::emit::{emit_fragment, validate_plan};
use super::model::read_fragment;
use super::types::{
    CurrentFragment, EmitStats, FragmentPlan, NodePatch, RawMarks, Step, TextOp, TextRun,
    WritePolicy,
};
use crate::crdt_engine::builder::{insert_node_at, ydoc_from_tiptap_json};
use crate::crdt_engine::projection::ydoc_to_tiptap_json;

// ---------------------------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------------------------

fn doc_of(nodes: Vec<Value>) -> Doc {
    ydoc_from_tiptap_json(&json!({"type": "doc", "content": nodes}))
}

fn text(t: &str) -> Value {
    json!({"type": "text", "text": t})
}

fn bold(t: &str) -> Value {
    json!({"type": "text", "text": t, "marks": [{"type": "bold"}]})
}

fn para(id: &str, children: Vec<Value>) -> Value {
    json!({"type": "paragraph", "attrs": {"data-block-id": id}, "content": children})
}

fn plan_of(steps: Vec<Step>) -> FragmentPlan {
    FragmentPlan {
        steps,
        summary: Default::default(),
    }
}

/// Read the current model, build a hand-made plan from it, validate and emit it in one transaction.
/// Returns the stats and the transaction's encoded update (taken before the commit).
fn emit_on(
    doc: &Doc,
    make: impl FnOnce(&CurrentFragment) -> Vec<Step>,
) -> Result<(EmitStats, Vec<u8>), String> {
    let fragment = doc.get_or_insert_xml_fragment("content");
    let policy = WritePolicy::restore();
    let mut txn = doc.transact_mut();
    let current = read_fragment(&txn, &fragment, &policy);
    let plan = plan_of(make(&current));
    let stats = emit_fragment(&mut txn, &fragment, &current, &plan)?;
    let update = txn.encode_update_v1();
    Ok((stats, update))
}

fn update_is_empty(bytes: &[u8]) -> bool {
    let update = Update::decode_v1(bytes).expect("decode update");
    update.insertions(true).is_empty() && update.delete_set().is_empty()
}

fn branch_id<T: AsRef<Branch>>(branch: &T) -> BranchID {
    branch.as_ref().id()
}

fn collect_ids<T: ReadTxn>(txn: &T, node: &XmlOut, out: &mut Vec<BranchID>) {
    match node {
        XmlOut::Element(element) => {
            out.push(branch_id(element));
            for child in element.children(txn) {
                collect_ids(txn, &child, out);
            }
        }
        XmlOut::Text(text) => out.push(branch_id(text)),
        XmlOut::Fragment(fragment) => {
            for child in fragment.children(txn) {
                collect_ids(txn, &child, out);
            }
        }
    }
}

/// Every Y.XmlElement and Y.XmlText branch id of the document, in document order.
fn all_ids(doc: &Doc) -> Vec<BranchID> {
    let fragment = doc.get_or_insert_xml_fragment("content");
    let txn = doc.transact();
    let mut out = Vec::new();
    for child in fragment.children(&txn) {
        collect_ids(&txn, &child, &mut out);
    }
    out
}

fn block(doc: &Doc, index: usize) -> Value {
    ydoc_to_tiptap_json(doc)["content"][index].clone()
}

/// The text nodes of a block as (text, mark type names).
fn runs(block: &Value) -> Vec<(String, Vec<String>)> {
    block["content"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|node| node["type"] == "text")
        .map(|node| {
            let marks = node["marks"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|mark| mark["type"].as_str().unwrap_or("").to_string())
                .collect();
            (node["text"].as_str().unwrap_or("").to_string(), marks)
        })
        .collect()
}

fn run(text: &str, marks: &[&str]) -> (String, Vec<String>) {
    (
        text.to_string(),
        marks.iter().map(|m| m.to_string()).collect(),
    )
}

fn block_text(block: &Value) -> String {
    runs(block).into_iter().map(|(text, _)| text).collect()
}

fn text_run(text: &str, marks: RawMarks) -> TextRun {
    TextRun {
        text: text.to_string(),
        marks: BTreeMap::new(),
        raw_marks: marks,
    }
}

fn marks_of(names: &[&str]) -> RawMarks {
    names.iter().map(|n| (n.to_string(), json!({}))).collect()
}

fn patch_children(children: Vec<Step>) -> Step {
    Step::Patch(NodePatch {
        set: vec![],
        remove: vec![],
        children: Some(children),
    })
}

fn full_state(doc: &Doc) -> Vec<u8> {
    doc.transact()
        .encode_state_as_update_v1(&StateVector::default())
}

fn apply(doc: &Doc, bytes: &[u8]) {
    doc.transact_mut()
        .apply_update(Update::decode_v1(bytes).expect("decode"))
        .expect("apply update");
}

fn text_of_block(doc: &Doc, index: usize, child: usize) -> XmlTextRef {
    let fragment: XmlFragmentRef = doc.get_or_insert_xml_fragment("content");
    let txn = doc.transact();
    let Some(XmlOut::Element(element)) = fragment.get(&txn, index as u32) else {
        panic!("block {index} is not an element");
    };
    let Some(XmlOut::Text(text)) = element.get(&txn, child as u32) else {
        panic!("child {child} of block {index} is not a text");
    };
    text
}

// ---------------------------------------------------------------------------------------------
// U5 tests
// ---------------------------------------------------------------------------------------------

#[test]
fn keep_plan_emits_an_empty_update() {
    let doc = doc_of(vec![
        para("a", vec![text("one")]),
        para("b", vec![bold("two")]),
        para("c", vec![text("three")]),
    ]);
    let before = ydoc_to_tiptap_json(&doc);
    let (stats, update) = emit_on(&doc, |cur| vec![Step::Keep; cur.children.len()]).unwrap();
    assert!(stats.counts.is_zero(), "an all-Keep plan counts nothing");
    assert_eq!(stats.clock_before, stats.clock_after);
    assert!(update_is_empty(&update), "the update carries no insertion and no deletion");
    assert!(update.len() <= 3, "an empty update is the two zero counts");
    assert_eq!(ydoc_to_tiptap_json(&doc), before);

    // The same check must be able to fail: one Remove makes the update non-empty.
    let (stats, update) = emit_on(&doc, |cur| {
        let mut steps = vec![Step::Keep; cur.children.len() - 1];
        steps.push(Step::Remove(1));
        steps
    })
    .unwrap();
    assert_eq!(stats.counts.nodes_removed, 1);
    assert!(!update_is_empty(&update));
}

#[test]
fn patch_keeps_branch_ids() {
    let doc = doc_of(vec![
        para("a", vec![text("one")]),
        para("b", vec![text("two words")]),
        para("c", vec![text("three")]),
    ]);
    let ids_before = all_ids(&doc);
    let (stats, _) = emit_on(&doc, |_| {
        vec![
            Step::Keep,
            Step::Patch(NodePatch {
                set: vec![("data-block-id".into(), json!("b2")), ("indent".into(), json!(1))],
                remove: vec![],
                children: Some(vec![Step::Text(vec![
                    TextOp::Retain(4),
                    TextOp::Delete(3),
                    TextOp::Insert {
                        text: "W".into(),
                        marks: RawMarks::new(),
                    },
                ])]),
            }),
            Step::Keep,
        ]
    })
    .unwrap();
    assert_eq!(stats.counts.attrs_set, 2);
    assert_eq!(stats.counts.text_bytes_deleted, 3);
    assert_eq!(stats.counts.text_bytes_inserted, 1);
    assert_eq!(stats.counts.nodes_inserted, 0);
    assert_eq!(stats.counts.nodes_removed, 0);

    let ids_after = all_ids(&doc);
    assert!(
        ids_before == ids_after,
        "no block and no Y.XmlText changed identity"
    );
    let patched = block(&doc, 1);
    assert_eq!(patched["attrs"]["data-block-id"], "b2");
    assert_eq!(patched["attrs"]["indent"], 1.0);
    assert_eq!(block_text(&patched), "two Wds");
    assert_eq!(block_text(&block(&doc, 0)), "one");
    assert_eq!(block_text(&block(&doc, 2)), "three");

    // Removing an attribute is a patch too.
    let (stats, _) = emit_on(&doc, |_| {
        vec![
            Step::Keep,
            Step::Patch(NodePatch {
                set: vec![],
                remove: vec!["indent".into()],
                children: None,
            }),
            Step::Keep,
        ]
    })
    .unwrap();
    assert_eq!(stats.counts.attrs_removed, 1);
    assert!(block(&doc, 1)["attrs"].get("indent").is_none());
    assert!(ids_before == all_ids(&doc));
}

#[test]
fn text_delta_retain_format_delete_insert() {
    let doc = doc_of(vec![para("a", vec![text("Hello brave world")])]);
    let ids_before = all_ids(&doc);
    let (stats, _) = emit_on(&doc, |_| {
        vec![patch_children(vec![Step::Text(vec![
            TextOp::Retain(6),
            TextOp::Format {
                bytes: 5,
                set: vec![("bold".into(), json!({}))],
                remove: vec![],
            },
            TextOp::Delete(6),
            TextOp::Insert {
                text: " there".into(),
                marks: RawMarks::new(),
            },
        ])])]
    })
    .unwrap();
    assert_eq!(stats.counts.format_ranges, 1);
    assert_eq!(stats.counts.text_bytes_deleted, 6);
    assert_eq!(stats.counts.text_bytes_inserted, 6);
    assert_eq!(
        runs(&block(&doc, 0)),
        vec![
            run("Hello ", &[]),
            run("brave", &["bold"]),
            run(" there", &[]),
        ]
    );
    assert!(ids_before == all_ids(&doc), "the Y.XmlText kept its identity");
}

#[test]
fn insert_has_exact_marks_no_inheritance() {
    // "ab" is bold. An insert inside it with no marks must not be bold; an insert at its end with only
    // italic must be italic and not bold. Plain `Text::insert` would inherit bold in both places.
    let doc = doc_of(vec![para("a", vec![bold("ab")])]);
    emit_on(&doc, |_| {
        vec![patch_children(vec![Step::Text(vec![
            TextOp::Retain(1),
            TextOp::Insert {
                text: "X".into(),
                marks: RawMarks::new(),
            },
            TextOp::Retain(1),
            TextOp::Insert {
                text: "Y".into(),
                marks: marks_of(&["italic"]),
            },
        ])])]
    })
    .unwrap();
    assert_eq!(
        runs(&block(&doc, 0)),
        vec![
            run("a", &["bold"]),
            run("X", &[]),
            run("b", &["bold"]),
            run("Y", &["italic"]),
        ]
    );
}

#[test]
fn format_remove_sets_null() {
    let doc = doc_of(vec![para("a", vec![bold("abc")])]);
    let ids_before = all_ids(&doc);
    emit_on(&doc, |_| {
        vec![patch_children(vec![Step::Text(vec![TextOp::Format {
            bytes: 2,
            set: vec![],
            remove: vec!["bold".into()],
        }])])]
    })
    .unwrap();
    assert_eq!(
        runs(&block(&doc, 0)),
        vec![run("ab", &[]), run("c", &["bold"])]
    );

    // A mark with attributes is set verbatim.
    emit_on(&doc, |_| {
        vec![patch_children(vec![Step::Text(vec![
            TextOp::Retain(2),
            TextOp::Format {
                bytes: 1,
                set: vec![("link".into(), json!({"href": "https://example.org"}))],
                remove: vec!["bold".into()],
            },
        ])])]
    })
    .unwrap();
    let after = block(&doc, 0);
    assert_eq!(runs(&after), vec![run("ab", &[]), run("c", &["link"])]);
    assert_eq!(
        after["content"][1]["marks"][0]["attrs"]["href"],
        "https://example.org"
    );
    assert!(ids_before == all_ids(&doc));
}

#[test]
fn insert_node_at_matches_append_nodes_shape() {
    let nodes = vec![
        para("p", vec![text("plain "), bold("bold"), text(" tail")]),
        json!({"type": "heading", "attrs": {"data-block-id": "h", "level": 2, "gone": null},
               "content": [text("Title")]}),
        json!({"type": "listItem", "attrs": {"data-block-id": "li", "listType": "bullet", "indent": 0},
               "content": [para("lip", vec![text("item"), json!({"type": "hardBreak"}), text("next")])]}),
        json!({"type": "objectEmbed", "attrs": {"iri": "urn:x", "label": "Embed"}}),
        json!({"type": "paragraph", "content": [text(""), text("after empty")]}),
    ];
    let reference = doc_of(nodes.clone());

    let built = Doc::new();
    let fragment = built.get_or_insert_xml_fragment("content");
    {
        let mut txn = built.transact_mut();
        for (index, node) in nodes.iter().enumerate() {
            insert_node_at(&mut txn, &fragment, index as u32, node);
        }
    }
    assert_eq!(
        ydoc_to_tiptap_json(&built),
        ydoc_to_tiptap_json(&reference),
        "inserting at 0,1,2… builds the shape append_nodes builds"
    );
    assert_eq!(all_ids(&built).len(), all_ids(&reference).len());

    // Insert in the middle and at the front: order follows the index.
    {
        let mut txn = built.transact_mut();
        insert_node_at(&mut txn, &fragment, 1, &para("mid", vec![text("MID")]));
        insert_node_at(&mut txn, &fragment, 0, &para("front", vec![text("FRONT")]));
    }
    let after = ydoc_to_tiptap_json(&built);
    let order: Vec<&str> = after["content"]
        .as_array()
        .unwrap()
        .iter()
        .take(4)
        .map(|b| b["attrs"]["data-block-id"].as_str().unwrap_or("-"))
        .collect();
    assert_eq!(order, vec!["front", "p", "mid", "h"]);
}

#[test]
fn insert_text_child_between_atoms() {
    let br = json!({"type": "hardBreak"});
    let doc = doc_of(vec![para("a", vec![br.clone(), br.clone()])]);
    let ids_before = all_ids(&doc);
    let (stats, _) = emit_on(&doc, |_| {
        vec![patch_children(vec![
            Step::Keep,
            Step::InsertText(vec![
                text_run("X", marks_of(&["bold"])),
                text_run("Y", RawMarks::new()),
            ]),
            Step::Keep,
        ])]
    })
    .unwrap();
    assert_eq!(stats.counts.nodes_inserted, 1);
    assert_eq!(stats.counts.text_bytes_inserted, 2);
    let content = block(&doc, 0)["content"].clone();
    assert_eq!(content[0]["type"], "hardBreak");
    assert_eq!(content[1]["text"], "X");
    assert_eq!(content[1]["marks"][0]["type"], "bold");
    assert_eq!(content[2]["text"], "Y");
    assert!(content[2].get("marks").is_none());
    assert_eq!(content[3]["type"], "hardBreak");
    // The atoms are the same Y types as before.
    let after = all_ids(&doc);
    assert!(after.contains(&ids_before[1]) && after.contains(&ids_before[2]));
}

#[test]
fn split_text_around_inserted_atom() {
    let doc = doc_of(vec![para("a", vec![bold("headtail")])]);
    let head = text_of_block(&doc, 0, 0);
    let head_id = branch_id(&head);
    let (stats, _) = emit_on(&doc, |_| {
        vec![patch_children(vec![
            Step::Text(vec![TextOp::Retain(4), TextOp::Delete(4)]),
            Step::InsertNode(json!({"type": "hardBreak"})),
            Step::InsertText(vec![text_run("tail", marks_of(&["bold"]))]),
        ])]
    })
    .unwrap();
    assert_eq!(stats.counts.text_bytes_deleted, 4);
    assert_eq!(stats.counts.text_bytes_inserted, 4);
    let content = block(&doc, 0)["content"].clone();
    assert_eq!(content[0]["text"], "head");
    assert_eq!(content[0]["marks"][0]["type"], "bold");
    assert_eq!(content[1]["type"], "hardBreak");
    assert_eq!(content[2]["text"], "tail");
    assert_eq!(content[2]["marks"][0]["type"], "bold");
    assert!(
        branch_id(&text_of_block(&doc, 0, 0)) == head_id,
        "only the tail lost identity; the head is the original Y.XmlText"
    );
}

#[test]
fn multibyte_offsets_land_on_char_boundaries() {
    // a(1) ñ(2) —(3) 😀(4) z(1)
    let doc = doc_of(vec![para("a", vec![text("añ—😀z")])]);
    let ids_before = all_ids(&doc);
    let (stats, _) = emit_on(&doc, |_| {
        vec![patch_children(vec![Step::Text(vec![
            TextOp::Retain(3),
            TextOp::Delete(7),
            TextOp::Insert {
                text: "→é".into(),
                marks: RawMarks::new(),
            },
        ])])]
    })
    .unwrap();
    assert_eq!(stats.counts.text_bytes_deleted, 7);
    assert_eq!(stats.counts.text_bytes_inserted, "→é".len() as u32);
    assert_eq!(block_text(&block(&doc, 0)), "añ→éz");
    assert!(ids_before == all_ids(&doc));

    // A length that ends inside a character is refused, and nothing changes.
    let before = ydoc_to_tiptap_json(&doc);
    for bad in [
        vec![TextOp::Retain(2)],               // inside ñ
        vec![TextOp::Retain(1), TextOp::Delete(1)], // inside ñ
        vec![TextOp::Retain(4), TextOp::Delete(2)], // inside → (3 bytes)
    ] {
        let result = emit_on(&doc, |_| vec![patch_children(vec![Step::Text(bad.clone())])]);
        assert!(result.is_err(), "{bad:?} must be refused");
    }
    assert_eq!(ydoc_to_tiptap_json(&doc), before);
}

#[test]
fn invalid_plan_is_refused_before_mutation() {
    let doc = doc_of(vec![
        para("a", vec![text("one"), json!({"type": "hardBreak"})]),
        para("b", vec![text("two")]),
    ]);
    let before = ydoc_to_tiptap_json(&doc);
    let state_before = doc.transact().state_vector();

    let attr_patch = || {
        Step::Patch(NodePatch {
            set: vec![("data-block-id".into(), json!("changed"))],
            remove: vec![],
            children: None,
        })
    };
    let cases: Vec<(&str, Vec<Step>)> = vec![
        // A valid first step must not be executed when a later one is invalid.
        ("remove overruns", vec![attr_patch(), Step::Remove(5)]),
        ("stream consumes too few", vec![attr_patch()]),
        (
            "stream consumes too many",
            vec![Step::Keep, Step::Keep, Step::Keep],
        ),
        (
            "text step on an element",
            vec![Step::Text(vec![TextOp::Retain(1)]), Step::Keep],
        ),
        (
            "delta past the end",
            vec![
                attr_patch(),
                patch_children(vec![Step::Text(vec![TextOp::Delete(99)])]),
            ],
        ),
        (
            "nested stream too short",
            vec![patch_children(vec![Step::Keep]), attr_patch()],
        ),
        (
            "patch on a text child",
            vec![
                attr_patch(),
                patch_children(vec![Step::Patch(NodePatch::default())]),
            ],
        ),
        (
            "insert node is not an object",
            vec![attr_patch(), Step::InsertNode(json!("x")), Step::Keep],
        ),
    ];
    for (name, steps) in cases {
        let result = emit_on(&doc, |_| steps.clone());
        let err = result.expect_err(name);
        assert!(!err.is_empty(), "{name}: the error says what is wrong");
        assert_eq!(
            ydoc_to_tiptap_json(&doc),
            before,
            "{name}: nothing was written"
        );
        assert!(
            doc.transact().state_vector() == state_before,
            "{name}: the Doc's state vector did not move"
        );
    }

    // validate_plan is the same check, callable alone.
    let fragment = doc.get_or_insert_xml_fragment("content");
    let current = {
        let txn = doc.transact();
        read_fragment(&txn, &fragment, &WritePolicy::restore())
    };
    assert!(validate_plan(&current, &plan_of(vec![Step::Keep, Step::Keep])).is_ok());
    assert!(validate_plan(&current, &plan_of(vec![Step::Keep])).is_err());
}

#[test]
fn remove_and_insert_node_steps_keep_the_cursor_straight() {
    let doc = doc_of(vec![
        para("a", vec![text("A")]),
        para("b", vec![text("B")]),
        para("c", vec![text("C")]),
        para("d", vec![text("D")]),
    ]);
    let ids_before = all_ids(&doc);
    let (stats, _) = emit_on(&doc, |_| {
        vec![
            Step::Keep,
            Step::Remove(2),
            Step::InsertNode(para("n", vec![text("N")])),
            Step::Keep,
            Step::InsertNode(para("z", vec![text("Z")])),
        ]
    })
    .unwrap();
    assert_eq!(stats.counts.nodes_removed, 2);
    assert_eq!(stats.counts.nodes_inserted, 4); // two paragraphs, two Y.XmlTexts
    let order: Vec<String> = (0..4).map(|i| block_text(&block(&doc, i))).collect();
    assert_eq!(order, vec!["A", "N", "D", "Z"]);
    let after = all_ids(&doc);
    assert!(after.contains(&ids_before[0]) && after.contains(&ids_before[6]));
    assert!(stats.clock_after > stats.clock_before);
}

#[test]
fn concurrent_person_insert_in_kept_block_survives() {
    let agent = doc_of(vec![
        para("a", vec![text("Block one text")]),
        para("b", vec![text("Block two text")]),
        para("c", vec![text("Block three text")]),
    ]);
    let person = Doc::new();
    person.get_or_insert_xml_fragment("content");
    apply(&person, &full_state(&agent));

    // The person types in block 3 while the agent patches block 1.
    let person_update = {
        let text = text_of_block(&person, 2, 0);
        let mut txn = person.transact_mut();
        text.insert(&mut txn, 6, "PERSON ");
        txn.encode_update_v1()
    };
    let (_, agent_update) = emit_on(&agent, |_| {
        vec![
            patch_children(vec![Step::Text(vec![
                TextOp::Retain(6),
                TextOp::Delete(3),
                TextOp::Insert {
                    text: "ONE".into(),
                    marks: RawMarks::new(),
                },
            ])]),
            Step::Keep,
            Step::Keep,
        ]
    })
    .unwrap();

    apply(&agent, &person_update);
    apply(&person, &agent_update);
    for doc in [&agent, &person] {
        assert_eq!(block_text(&block(doc, 0)), "Block ONE text");
        assert_eq!(block_text(&block(doc, 2)), "Block PERSON three text");
    }
    assert_eq!(ydoc_to_tiptap_json(&agent), ydoc_to_tiptap_json(&person));
}

#[test]
fn concurrent_person_insert_in_untouched_range_of_patched_block_survives() {
    let agent = doc_of(vec![para("a", vec![text("Hello brave new world")])]);
    let person = Doc::new();
    person.get_or_insert_xml_fragment("content");
    apply(&person, &full_state(&agent));

    // The person edits the same paragraph, outside the range the agent changes.
    let person_update = {
        let text = text_of_block(&person, 0, 0);
        let mut txn = person.transact_mut();
        text.insert(&mut txn, 21, "!");
        text.insert(&mut txn, 11, " NEW");
        text.insert(&mut txn, 0, "PRE ");
        txn.encode_update_v1()
    };
    let (_, agent_update) = emit_on(&agent, |_| {
        vec![patch_children(vec![Step::Text(vec![
            TextOp::Retain(6),
            TextOp::Delete(5),
            TextOp::Insert {
                text: "bold".into(),
                marks: RawMarks::new(),
            },
        ])])]
    })
    .unwrap();

    apply(&agent, &person_update);
    apply(&person, &agent_update);
    for doc in [&agent, &person] {
        assert_eq!(
            block_text(&block(doc, 0)),
            "PRE Hello bold NEW new world!",
            "the agent's change and all three of the person's inserts"
        );
    }
}
