//! Tests for `minimal_edit::plan` (owner U4; DESIGN §4.3).
//!
//! Real Y.Docs built by `builder::ydoc_from_tiptap_json`, read by `model::read_fragment`, planned against
//! desired fragments from `model::desired_from_tiptap_nodes`. Nothing is mocked and nothing is applied: the
//! planner is pure, so these tests read the step streams.

use std::collections::BTreeSet;

use serde_json::{json, Value};
use yrs::{Doc, Transact};

use super::model;
use super::plan::{plan_fragment, plan_node};
use super::types::{
    CurrentChild, CurrentFragment, DesiredChild, DesiredFragment, FragmentPlan, IdPolicy, MarkSet, NodePatch,
    RawMarks, SourceFormat, Step, TextOp, TextRun, WritePolicy,
};
use crate::crdt_engine::builder::ydoc_from_tiptap_json;

const BOOK: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/comment_anchoring/freud-ch1-blocks.json"
));

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

fn tiptap_policy() -> WritePolicy {
    WritePolicy {
        format: SourceFormat::TiptapJson,
        ids: IdPolicy::Bound,
    }
}

fn markdown_policy(minted: &[&str]) -> WritePolicy {
    WritePolicy {
        format: SourceFormat::Markdown,
        ids: IdPolicy::Adopt {
            minted: minted.iter().map(|id| id.to_string()).collect::<BTreeSet<_>>(),
        },
    }
}

fn text(content: &str) -> Value {
    json!({"type": "text", "text": content})
}

fn marked(content: &str, marks: Value) -> Value {
    json!({"type": "text", "text": content, "marks": marks})
}

fn para(id: &str, nodes: Vec<Value>) -> Value {
    json!({"type": "paragraph", "attrs": {"data-block-id": id}, "content": nodes})
}

fn p(id: &str, content: &str) -> Value {
    para(id, vec![text(content)])
}

fn p_noid(content: &str) -> Value {
    json!({"type": "paragraph", "content": [text(content)]})
}

/// Build the current doc and the desired model, plan, and hand all three to `check`.
fn with_plan<R>(
    cur: &[Value],
    des: &[Value],
    policy: &WritePolicy,
    check: impl FnOnce(&CurrentFragment, &DesiredFragment, &FragmentPlan) -> R,
) -> R {
    let doc: Doc = ydoc_from_tiptap_json(&json!({"type": "doc", "content": cur}));
    let fragment = doc.get_or_insert_xml_fragment("content");
    let txn = doc.transact();
    let current = model::read_fragment(&txn, &fragment, policy);
    let desired = model::desired_from_tiptap_nodes(des, policy);
    let plan = plan_fragment(&current, &desired, policy);
    check(&current, &desired, &plan)
}

fn plan_of(cur: &[Value], des: &[Value], policy: &WritePolicy) -> FragmentPlan {
    with_plan(cur, des, policy, |_, _, plan| plan.clone())
}

fn kinds(steps: &[Step]) -> Vec<&'static str> {
    steps
        .iter()
        .map(|step| match step {
            Step::Keep => "keep",
            Step::Patch(_) => "patch",
            Step::Text(_) => "text",
            Step::Remove(_) => "remove",
            Step::InsertNode(_) => "node",
            Step::InsertText(_) => "newtext",
        })
        .collect()
}

fn patch_at(plan: &FragmentPlan, index: usize) -> &NodePatch {
    match &plan.steps[index] {
        Step::Patch(patch) => patch,
        other => panic!("step {index} is not a patch: {other:?}"),
    }
}

fn children_of(patch: &NodePatch) -> &Vec<Step> {
    patch
        .children
        .as_ref()
        .unwrap_or_else(|| panic!("patch has no children stream: {patch:?}"))
}

/// Every text op below `steps`, in document order.
fn text_ops(steps: &[Step]) -> Vec<&TextOp> {
    let mut out = Vec::new();
    for step in steps {
        match step {
            Step::Text(ops) => out.extend(ops.iter()),
            Step::Patch(patch) => {
                if let Some(children) = &patch.children {
                    out.extend(text_ops(children));
                }
            }
            _ => {}
        }
    }
    out
}

fn first_text_step(steps: &[Step]) -> Option<&Vec<TextOp>> {
    for step in steps {
        match step {
            Step::Text(ops) => return Some(ops),
            Step::Patch(patch) => {
                if let Some(children) = &patch.children {
                    if let Some(found) = first_text_step(children) {
                        return Some(found);
                    }
                }
            }
            _ => {}
        }
    }
    None
}

fn patch_depth(step: &Step) -> usize {
    match step {
        Step::Patch(patch) => {
            1 + patch
                .children
                .as_ref()
                .map(|children| children.iter().map(patch_depth).max().unwrap_or(0))
                .unwrap_or(0)
        }
        _ => 0,
    }
}

fn consumed(steps: &[Step]) -> usize {
    steps
        .iter()
        .map(|step| match step {
            Step::Keep | Step::Patch(_) | Step::Text(_) => 1,
            Step::Remove(count) => *count as usize,
            Step::InsertNode(_) | Step::InsertText(_) => 0,
        })
        .sum()
}

/// I9 at every level, plus: Text steps address Y.XmlText children and stay inside them on char boundaries.
fn assert_conserves(current: &[CurrentChild], steps: &[Step]) {
    assert_eq!(
        consumed(steps),
        current.len(),
        "steps do not consume the children exactly once: {steps:?}"
    );
    let mut cursor = 0usize;
    for step in steps {
        match step {
            Step::Keep => cursor += 1,
            Step::Text(ops) => {
                let CurrentChild::Text(target) = &current[cursor] else {
                    panic!("Text step on an element: {step:?}");
                };
                let whole: String = target.runs.iter().map(|run| run.text.as_str()).collect();
                let mut at = 0usize;
                for op in ops {
                    match op {
                        TextOp::Retain(bytes) | TextOp::Delete(bytes) => at += *bytes as usize,
                        TextOp::Format { bytes, .. } => at += *bytes as usize,
                        TextOp::Insert { .. } => {}
                    }
                    assert!(at <= whole.len(), "op runs past the text: {ops:?} on {whole:?}");
                    assert!(
                        whole.is_char_boundary(at),
                        "op ends inside a character: {ops:?} on {whole:?}"
                    );
                }
                cursor += 1;
            }
            Step::Patch(patch) => {
                let CurrentChild::Element(node) = &current[cursor] else {
                    panic!("Patch step on a text: {step:?}");
                };
                if let Some(children) = &patch.children {
                    assert_conserves(&node.children, children);
                }
                cursor += 1;
            }
            Step::Remove(count) => cursor += *count as usize,
            Step::InsertNode(_) | Step::InsertText(_) => {}
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The shared scenarios (also the inputs of the conservation and determinism tests)
// ---------------------------------------------------------------------------------------------

struct Case {
    cur: Vec<Value>,
    des: Vec<Value>,
    policy: WritePolicy,
}

const ALL_CASES: &[&str] = &[
    "identical",
    "changed",
    "insert-top",
    "move",
    "bound-id",
    "adopt-same",
    "adopt-append",
    "adopt-shrink",
    "adopt-skip",
    "adopt-legacy",
    "adopt-none",
    "bold",
    "md-comment",
    "md-query",
    "md-provenance",
    "restore-attr",
    "link",
    "link-href",
    "table",
    "list-indent",
    "embed",
    "hard-break",
    "interior",
    "edge",
    "front",
];

fn comment_mark() -> Value {
    json!([{"type": "commentMark", "attrs": {"commentId": "c1"}}])
}

fn case(name: &str) -> Case {
    let tiptap = tiptap_policy();
    match name {
        "identical" => Case {
            cur: vec![
                p("a", "Hello"),
                json!({"type": "heading", "attrs": {"level": 2, "data-block-id": "h"},
                       "content": [text("Title")]}),
                para("b", vec![text("plain "), marked("bold", json!([{"type": "bold"}]))]),
                para("c", vec![text("Hello "), text("world")]),
            ],
            // The last block is builder-shaped on one side only: the segmentation must not matter.
            des: vec![
                p("a", "Hello"),
                json!({"type": "heading", "attrs": {"level": 2, "data-block-id": "h"},
                       "content": [text("Title")]}),
                para("b", vec![text("plain "), marked("bold", json!([{"type": "bold"}]))]),
                para("c", vec![text("Hello world")]),
            ],
            policy: tiptap,
        },
        "changed" => Case {
            cur: vec![p("a", "one"), p("b", "two"), p("c", "three")],
            des: vec![p("a", "one"), p("b", "two!"), p("c", "three")],
            policy: tiptap,
        },
        "insert-top" => Case {
            cur: vec![p("a", "one"), p("b", "two")],
            des: vec![p("new", "zero"), p("a", "one"), p("b", "two")],
            policy: tiptap,
        },
        "move" => Case {
            cur: vec![p("A", "alpha"), p("B", "beta"), p("C", "gamma")],
            des: vec![p("B", "beta"), p("C", "gamma"), p("A", "alpha")],
            policy: tiptap,
        },
        "bound-id" => Case {
            cur: vec![p("old", "same")],
            des: vec![p("new", "same")],
            policy: tiptap,
        },
        "adopt-same" => Case {
            cur: vec![p("OLD0", "zero"), p("OLD1", "one"), p("OLD2", "two")],
            des: vec![
                p("fresh0", "zero v2"),
                p("fresh1", "one edited"),
                p("fresh2", "two again"),
            ],
            policy: markdown_policy(&["fresh0", "fresh1", "fresh2"]),
        },
        "adopt-append" => Case {
            cur: vec![p("OLD0", "one"), p("OLD1", "two")],
            des: vec![
                p("fresh0", "one"),
                p("fresh1", "two"),
                p("fresh2", "three (new)"),
                p("fresh3", "four (new)"),
            ],
            policy: markdown_policy(&["fresh0", "fresh1", "fresh2", "fresh3"]),
        },
        "adopt-shrink" => Case {
            cur: vec![
                p("OLD0", "zero"),
                p("OLD1", "one"),
                p("OLD2", "two"),
                p("OLD3", "three"),
            ],
            des: vec![p("fresh0", "only one now")],
            policy: markdown_policy(&["fresh0"]),
        },
        "adopt-skip" => Case {
            cur: vec![p("OLD0", "zero"), p("OLD1", "one")],
            // A stray text node between blocks is not a block: it consumes no id slot.
            des: vec![
                p("fresh0", "para 0"),
                json!({"type": "text", "text": "stray inline"}),
                p("fresh1", "para 1"),
            ],
            policy: markdown_policy(&["fresh0", "fresh1"]),
        },
        "adopt-legacy" => Case {
            cur: vec![json!({"type": "paragraph",
                             "attrs": {"data-block-id": "OLD0", "blockId": "OLD0"},
                             "content": [text("before")]})],
            des: vec![json!({"type": "paragraph",
                             "attrs": {"data-block-id": "fresh0", "blockId": "fresh0"},
                             "content": [text("after")]})],
            policy: markdown_policy(&["fresh0"]),
        },
        "adopt-none" => Case {
            cur: vec![],
            des: vec![p("fresh0", "hello")],
            policy: markdown_policy(&["fresh0"]),
        },
        "bold" => Case {
            cur: vec![p("p", "hello world")],
            des: vec![para(
                "p",
                vec![text("hello "), marked("world", json!([{"type": "bold"}]))],
            )],
            policy: tiptap,
        },
        "md-comment" => Case {
            cur: vec![para(
                "p",
                vec![text("Hello "), marked("world", comment_mark())],
            )],
            des: vec![para("fresh", vec![text("Hello world!")])],
            policy: markdown_policy(&["fresh"]),
        },
        "md-query" => Case {
            cur: vec![
                p("a", "first"),
                json!({"type": "queryBlock",
                       "attrs": {"data-block-id": "q", "query": "SELECT 1"}}),
                p("b", "last"),
            ],
            des: vec![p_noid("first"), p_noid("last")],
            policy: markdown_policy(&[]),
        },
        "md-provenance" => Case {
            cur: vec![json!({"type": "paragraph",
                             "attrs": {"data-block-id": "b1", "data-source-id": "src-1",
                                       "data-pdf-page": 3},
                             "content": [text("old words")]})],
            des: vec![p("fresh", "new words")],
            policy: markdown_policy(&["fresh"]),
        },
        "restore-attr" => Case {
            cur: vec![json!({"type": "paragraph",
                             "attrs": {"data-block-id": "x", "data-source-id": "src-1"},
                             "content": [text("same")]})],
            des: vec![p("x", "same")],
            policy: WritePolicy::restore(),
        },
        "link" => Case {
            // The editor's link carries defaults; the agent's does not. They are the same link.
            cur: vec![para(
                "p",
                vec![marked(
                    "go",
                    json!([{"type": "link", "attrs": {
                        "href": "https://a.example", "target": "_blank",
                        "rel": "noopener noreferrer nofollow", "class": null}}]),
                )],
            )],
            des: vec![para(
                "p",
                vec![marked(
                    "go",
                    json!([{"type": "link", "attrs": {"href": "https://a.example"}}]),
                )],
            )],
            policy: tiptap,
        },
        "link-href" => Case {
            cur: vec![para(
                "p",
                vec![marked(
                    "go",
                    json!([{"type": "link", "attrs": {
                        "href": "https://a.example", "target": "_blank",
                        "rel": "noopener noreferrer nofollow", "class": null}}]),
                )],
            )],
            des: vec![para(
                "p",
                vec![marked(
                    "go",
                    json!([{"type": "link", "attrs": {"href": "https://b.example"}}]),
                )],
            )],
            policy: tiptap,
        },
        "table" => {
            let table = |cell_text: &str| {
                json!({"type": "table", "attrs": {"data-block-id": "t"}, "content": [
                    {"type": "tableRow", "content": [
                        {"type": "tableCell", "content": [
                            {"type": "paragraph", "content": [text(cell_text)]}]}]}]})
            };
            Case {
                cur: vec![table("x")],
                des: vec![table("y")],
                policy: tiptap,
            }
        }
        "list-indent" => {
            let item = |indent: u32| {
                json!({"type": "listItem",
                       "attrs": {"data-block-id": "li", "listType": "bullet", "indent": indent},
                       "content": [{"type": "paragraph", "content": [text("item")]}]})
            };
            Case {
                cur: vec![item(0)],
                des: vec![item(1)],
                policy: tiptap,
            }
        }
        "embed" => {
            let embed = |label: &str| {
                json!({"type": "objectEmbed",
                       "attrs": {"data-block-id": "e1", "iri": "http://example.org/thing",
                                 "label": label, "classLabel": "Thing", "slot": "card"}})
            };
            Case {
                cur: vec![embed("A")],
                des: vec![embed("B")],
                policy: tiptap,
            }
        }
        "hard-break" => Case {
            cur: vec![p("h", "ab")],
            des: vec![para(
                "h",
                vec![text("a"), json!({"type": "hardBreak"}), text("b")],
            )],
            policy: tiptap,
        },
        "interior" => Case {
            cur: vec![para("p", vec![marked("abcd", comment_mark())])],
            des: vec![para("fresh", vec![text("abXcd")])],
            policy: markdown_policy(&["fresh"]),
        },
        "edge" => Case {
            cur: vec![para("p", vec![marked("abcd", comment_mark())])],
            des: vec![para("fresh", vec![text("abcdX")])],
            policy: markdown_policy(&["fresh"]),
        },
        "front" => Case {
            cur: vec![para("p", vec![marked("abcd", comment_mark())])],
            des: vec![para("fresh", vec![text("Xabcd")])],
            policy: markdown_policy(&["fresh"]),
        },
        other => panic!("unknown case {other}"),
    }
}

fn plan_case(name: &str) -> FragmentPlan {
    let case = case(name);
    plan_of(&case.cur, &case.des, &case.policy)
}

fn raw_marks(entries: &[(&str, Value)]) -> RawMarks {
    entries
        .iter()
        .map(|(name, value)| (name.to_string(), value.clone()))
        .collect()
}

// ---------------------------------------------------------------------------------------------
// Alignment and identity
// ---------------------------------------------------------------------------------------------

#[test]
fn identical_is_all_keep() {
    let plan = plan_case("identical");
    assert!(plan.is_noop(), "{:?}", plan.steps);
    assert_eq!(kinds(&plan.steps), vec!["keep"; 4]);
    assert_eq!(plan.summary.kept, 4);
    assert_eq!(plan.summary.patched + plan.summary.inserted + plan.summary.removed, 0);
}

#[test]
fn one_changed_paragraph_is_one_patch() {
    let plan = plan_case("changed");
    assert_eq!(kinds(&plan.steps), vec!["keep", "patch", "keep"]);
    let patch = patch_at(&plan, 1);
    assert!(patch.set.is_empty() && patch.remove.is_empty(), "{patch:?}");
    assert_eq!(
        patch.children,
        Some(vec![Step::Text(vec![
            TextOp::Retain(3),
            TextOp::Insert {
                text: "!".into(),
                marks: RawMarks::new()
            }
        ])])
    );
    assert_eq!(plan.summary.kept, 2);
    assert_eq!(plan.summary.patched, 1);
}

#[test]
fn insert_at_top_is_one_insert() {
    let plan = plan_case("insert-top");
    assert_eq!(kinds(&plan.steps), vec!["node", "keep", "keep"]);
    let Step::InsertNode(json) = &plan.steps[0] else {
        panic!("{:?}", plan.steps[0]);
    };
    assert_eq!(json["attrs"]["data-block-id"], "new");
    assert_eq!((plan.summary.inserted, plan.summary.kept), (1, 2));
}

#[test]
fn move_is_remove_plus_insert() {
    let plan = plan_case("move");
    // Yjs has no XmlFragment move: A leaves at the top and comes back at the bottom; B and C keep identity.
    assert_eq!(kinds(&plan.steps), vec!["remove", "keep", "keep", "node"]);
    assert_eq!(plan.steps[0], Step::Remove(1));
    let Step::InsertNode(json) = &plan.steps[3] else {
        panic!("{:?}", plan.steps[3]);
    };
    assert_eq!(json["attrs"]["data-block-id"], "A");
}

#[test]
fn bound_id_change_is_an_attr_patch() {
    let plan = plan_case("bound-id");
    assert_eq!(
        plan.steps,
        vec![Step::Patch(NodePatch {
            set: vec![("data-block-id".to_string(), json!("new"))],
            remove: vec![],
            children: None,
        })]
    );

    // A legacy blockId on the current node follows, so the two faces stay consistent.
    let cur = vec![json!({"type": "paragraph",
                          "attrs": {"data-block-id": "old", "blockId": "old"},
                          "content": [text("same")]})];
    let des = vec![p("new", "same")];
    let plan = plan_of(&cur, &des, &tiptap_policy());
    let patch = patch_at(&plan, 0);
    assert_eq!(
        patch.set,
        vec![
            ("blockId".to_string(), json!("new")),
            ("data-block-id".to_string(), json!("new"))
        ]
    );
    assert!(patch.remove.is_empty() && patch.children.is_none());
}

// ---------------------------------------------------------------------------------------------
// Adopt (the six cases ported from reuse_block_ids_positionally)
// ---------------------------------------------------------------------------------------------

fn id_keys(patch: &NodePatch) -> Vec<&String> {
    patch
        .set
        .iter()
        .map(|(key, _)| key)
        .chain(patch.remove.iter())
        .filter(|key| key.as_str() == "data-block-id" || key.as_str() == "blockId")
        .collect()
}

#[test]
fn adopt_keeps_current_ids_same_structure() {
    let plan = plan_case("adopt-same");
    assert_eq!(kinds(&plan.steps), vec!["patch", "patch", "patch"]);
    for index in 0..3 {
        // The same positions, so the same blocks: no id is written and nothing is replaced.
        assert!(id_keys(patch_at(&plan, index)).is_empty());
    }
}

#[test]
fn adopt_appended_blocks_keep_minted_ids() {
    let plan = plan_case("adopt-append");
    assert_eq!(kinds(&plan.steps), vec!["keep", "keep", "node", "node"]);
    for (index, id) in [(2, "fresh2"), (3, "fresh3")] {
        let Step::InsertNode(json) = &plan.steps[index] else {
            panic!("{:?}", plan.steps[index]);
        };
        assert_eq!(json["attrs"]["data-block-id"], id);
    }
}

#[test]
fn adopt_shrink_keeps_prefix_ids() {
    let plan = plan_case("adopt-shrink");
    assert_eq!(kinds(&plan.steps), vec!["patch", "remove"]);
    assert!(id_keys(patch_at(&plan, 0)).is_empty());
    assert_eq!(plan.steps[1], Step::Remove(3));
}

#[test]
fn adopt_skips_non_block_types() {
    let plan = plan_case("adopt-skip");
    assert_eq!(kinds(&plan.steps), vec!["patch", "newtext", "patch"]);
    assert!(id_keys(patch_at(&plan, 0)).is_empty());
    assert!(id_keys(patch_at(&plan, 2)).is_empty());
    let Step::InsertText(runs) = &plan.steps[1] else {
        panic!("{:?}", plan.steps[1]);
    };
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].text, "stray inline");
}

#[test]
fn adopt_keeps_legacy_block_id_consistent() {
    let plan = plan_case("adopt-legacy");
    assert_eq!(kinds(&plan.steps), vec!["patch"]);
    // Neither face is rewritten, so data-block-id and blockId both stay OLD0.
    assert!(id_keys(patch_at(&plan, 0)).is_empty());
}

#[test]
fn adopt_no_existing_ids_is_noop() {
    let plan = plan_case("adopt-none");
    assert_eq!(kinds(&plan.steps), vec!["node"]);
    let Step::InsertNode(json) = &plan.steps[0] else {
        panic!("{:?}", plan.steps[0]);
    };
    assert_eq!(json["attrs"]["data-block-id"], "fresh0");
}

// ---------------------------------------------------------------------------------------------
// Inline marks and carry rules
// ---------------------------------------------------------------------------------------------

#[test]
fn bold_range_is_one_format_op() {
    let plan = plan_case("bold");
    assert_eq!(kinds(&plan.steps), vec!["patch"]);
    assert_eq!(
        patch_at(&plan, 0).children,
        Some(vec![Step::Text(vec![
            TextOp::Retain(6),
            TextOp::Format {
                bytes: 5,
                set: vec![("bold".to_string(), json!({}))],
                remove: vec![],
            },
        ])])
    );
}

#[test]
fn markdown_keeps_comment_mark() {
    let plan = plan_case("md-comment");
    assert_eq!(kinds(&plan.steps), vec!["patch"]);
    let children = children_of(patch_at(&plan, 0));
    // "Hello " is untouched; "world" (commented) is retained as is and "!" is appended after it.
    assert_eq!(
        children,
        &vec![
            Step::Keep,
            Step::Text(vec![
                TextOp::Retain(5),
                TextOp::Insert {
                    text: "!".into(),
                    marks: RawMarks::new()
                }
            ])
        ]
    );
    for op in text_ops(&plan.steps) {
        match op {
            TextOp::Format { remove, .. } => {
                assert!(!remove.iter().any(|name| name == "commentMark"), "{op:?}")
            }
            TextOp::Delete(_) => panic!("markdown deleted text: {op:?}"),
            _ => {}
        }
    }
}

#[test]
fn markdown_keeps_unexpressible_query_block() {
    let plan = plan_case("md-query");
    // Markdown has no query block: omitting it from the write leaves it in place.
    assert!(plan.is_noop(), "{:?}", plan.steps);
    assert_eq!(kinds(&plan.steps), vec!["keep", "keep", "keep"]);

    // An edit elsewhere still leaves it alone.
    let cur = case("md-query").cur;
    let plan = plan_of(
        &cur,
        &[p_noid("first, edited"), p_noid("last")],
        &markdown_policy(&[]),
    );
    assert_eq!(kinds(&plan.steps), vec!["patch", "keep", "keep"]);
}

#[test]
fn markdown_keeps_provenance_attrs() {
    let plan = plan_case("md-provenance");
    assert_eq!(kinds(&plan.steps), vec!["patch"]);
    let patch = patch_at(&plan, 0);
    assert!(patch.set.is_empty(), "{patch:?}");
    assert!(patch.remove.is_empty(), "{patch:?}");
    assert!(patch.children.is_some());
}

#[test]
fn restore_policy_removes_unstated_attrs() {
    let plan = plan_case("restore-attr");
    assert_eq!(
        plan.steps,
        vec![Step::Patch(NodePatch {
            set: vec![],
            remove: vec!["data-source-id".to_string()],
            children: None,
        })]
    );
}

#[test]
fn link_defaults_are_keep() {
    let plan = plan_case("link");
    assert!(plan.is_noop(), "{:?}", plan.steps);

    // A different href is a real change: one format op that sets the desired link.
    let plan = plan_case("link-href");
    assert_eq!(kinds(&plan.steps), vec!["patch"]);
    let ops = first_text_step(&plan.steps).expect("a text step");
    assert_eq!(
        ops,
        &vec![TextOp::Format {
            bytes: 2,
            set: vec![("link".to_string(), json!({"href": "https://b.example"}))],
            remove: vec![],
        }]
    );
}

// ---------------------------------------------------------------------------------------------
// Nested structure
// ---------------------------------------------------------------------------------------------

#[test]
fn table_cell_edit_is_nested_patch() {
    let plan = plan_case("table");
    assert_eq!(kinds(&plan.steps), vec!["patch"]);
    // table > row > cell > paragraph, then the text: the table's identity and every wrapper stay.
    assert_eq!(patch_depth(&plan.steps[0]), 4);
    assert_eq!(
        first_text_step(&plan.steps),
        Some(&vec![
            TextOp::Delete(1),
            TextOp::Insert {
                text: "y".into(),
                marks: RawMarks::new()
            }
        ])
    );
}

#[test]
fn list_indent_is_attr_patch() {
    let plan = plan_case("list-indent");
    assert_eq!(
        plan.steps,
        vec![Step::Patch(NodePatch {
            set: vec![("indent".to_string(), json!(1))],
            remove: vec![],
            children: None,
        })]
    );
}

#[test]
fn object_embed_label_is_attr_patch() {
    let plan = plan_case("embed");
    // The embed keeps its identity (and so its mounted NodeView); only the label changes.
    assert_eq!(
        plan.steps,
        vec![Step::Patch(NodePatch {
            set: vec![("label".to_string(), json!("B"))],
            remove: vec![],
            children: None,
        })]
    );
}

#[test]
fn hard_break_insert_splits_text() {
    let plan = plan_case("hard-break");
    assert_eq!(kinds(&plan.steps), vec!["patch"]);
    let children = children_of(patch_at(&plan, 0));
    assert_eq!(children.len(), 3, "{children:?}");
    // The head stays in the original text; only the tail loses identity.
    assert_eq!(
        children[0],
        Step::Text(vec![TextOp::Retain(1), TextOp::Delete(1)])
    );
    let Step::InsertNode(atom) = &children[1] else {
        panic!("{:?}", children[1]);
    };
    assert_eq!(atom["type"], "hardBreak");
    assert_eq!(
        children[2],
        Step::InsertText(vec![TextRun {
            text: "b".into(),
            marks: MarkSet::new(),
            raw_marks: RawMarks::new(),
        }])
    );
}

#[test]
fn interior_insert_inherits_unstated_marks_edge_insert_does_not() {
    let comment = raw_marks(&[("commentMark", json!({"commentId": "c1"}))]);

    // Inside the commented phrase: "X" stays commented (ProseMirror's interior rule).
    let plan = plan_case("interior");
    assert_eq!(
        first_text_step(&plan.steps),
        Some(&vec![
            TextOp::Retain(2),
            TextOp::Insert {
                text: "X".into(),
                marks: comment
            }
        ])
    );

    // At the end of it: no right neighbour carries the mark, so "X" is plain.
    let plan = plan_case("edge");
    assert_eq!(
        first_text_step(&plan.steps),
        Some(&vec![
            TextOp::Retain(4),
            TextOp::Insert {
                text: "X".into(),
                marks: RawMarks::new()
            }
        ])
    );

    // At the start: no left neighbour.
    let plan = plan_case("front");
    assert_eq!(
        first_text_step(&plan.steps),
        Some(&vec![TextOp::Insert {
            text: "X".into(),
            marks: RawMarks::new()
        }])
    );
}

#[test]
fn inline_cap_falls_back_to_replace() {
    fn leaf(plan: &FragmentPlan) -> &Vec<Step> {
        children_of(patch_at(plan, 0))
    }
    let policy = tiptap_policy();

    // Edit distance past MAX_INLINE_EDIT_DISTANCE: two disjoint 2,100-character runs.
    let plan = plan_of(
        &[p("p", &"a".repeat(2_100))],
        &[p("p", &"b".repeat(2_100))],
        &policy,
    );
    let steps = leaf(&plan);
    assert_eq!(steps.len(), 2, "{:?}", kinds(steps));
    assert_eq!(steps[0], Step::Remove(1));
    let Step::InsertText(runs) = &steps[1] else {
        panic!("{:?}", steps[1]);
    };
    assert_eq!(runs[0].text.len(), 2_100);

    // More units than MAX_INLINE_UNITS: Myers is not even tried.
    let plan = plan_of(
        &[p("p", &"a".repeat(100_001))],
        &[p("p", &"b".repeat(100_001))],
        &policy,
    );
    let steps = leaf(&plan);
    assert_eq!(steps[0], Step::Remove(1));
    assert!(matches!(steps[1], Step::InsertText(_)));

    // Under the cap the same shape is a text delta, not a replacement.
    let plan = plan_of(&[p("p", "aaaa")], &[p("p", "bbbb")], &policy);
    assert_eq!(kinds(leaf(&plan)), vec!["text"]);
}

// ---------------------------------------------------------------------------------------------
// Properties
// ---------------------------------------------------------------------------------------------

fn freud_blocks() -> Vec<Value> {
    let fixture: Value = serde_json::from_str(BOOK).expect("fixture parses");
    fixture["tiptapJson"]["content"]
        .as_array()
        .expect("fixture content")
        .clone()
}

/// A seeded xorshift, so the "random" edits are the same on every run.
struct Seed(u64);

impl Seed {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, bound: usize) -> usize {
        (self.next() % bound as u64) as usize
    }
}

#[test]
fn step_streams_conserve_children() {
    // Every scenario above.
    for name in ALL_CASES {
        let case = case(name);
        with_plan(&case.cur, &case.des, &case.policy, |current, _, plan| {
            assert_conserves(&current.children, &plan.steps);
        });
    }

    // The Freud fixture with seeded single-block edits: one text node of one block changes; every other
    // block must be a Keep and the edited one a Patch.
    let blocks = freud_blocks();
    let policy = tiptap_policy();
    let mut seed = Seed(0x9E37_79B9_7F4A_7C15);
    for round in 0..60 {
        let mut des = blocks.clone();
        let block = seed.below(des.len());
        let texts: Vec<usize> = des[block]["content"]
            .as_array()
            .map(|nodes| {
                nodes
                    .iter()
                    .enumerate()
                    .filter(|(_, node)| node["text"].as_str().map_or(false, |t| t.chars().count() > 4))
                    .map(|(index, _)| index)
                    .collect()
            })
            .unwrap_or_default();
        if texts.is_empty() {
            continue;
        }
        let node = texts[seed.below(texts.len())];
        let original = des[block]["content"][node]["text"].as_str().unwrap().to_string();
        let mut chars: Vec<char> = original.chars().collect();
        let at = seed.below(chars.len());
        match seed.below(3) {
            0 => {
                // Insert a multibyte character.
                chars.insert(at, 'Ω');
            }
            1 => {
                // Delete a short range.
                let end = (at + 1 + seed.below(3)).min(chars.len());
                chars.drain(at..end);
            }
            _ => {
                // Replace a short range with other characters.
                let end = (at + 1 + seed.below(3)).min(chars.len());
                drop(chars.splice(at..end, "é—Ωz".chars()));
            }
        }
        let edited: String = chars.into_iter().collect();
        if edited == original {
            continue;
        }
        des[block]["content"][node]["text"] = Value::String(edited);

        with_plan(&blocks, &des, &policy, |current, _, plan| {
            assert_conserves(&current.children, &plan.steps);
            let expected: Vec<&str> = (0..blocks.len())
                .map(|index| if index == block { "patch" } else { "keep" })
                .collect();
            assert_eq!(kinds(&plan.steps), expected, "round {round}, block {block}");
        });
    }
}

#[test]
fn plan_is_deterministic() {
    for name in ALL_CASES {
        let first = plan_case(name);
        let second = plan_case(name);
        assert_eq!(first, second, "case {name}");

        // And planning twice against the same read model gives the same stream.
        let case = case(name);
        with_plan(&case.cur, &case.des, &case.policy, |current, desired, plan| {
            let again = plan_fragment(current, desired, &case.policy);
            assert_eq!(&again, plan, "case {name}");
        });
    }

    // plan_node alone agrees with the Patch the fragment plan holds for the same pair (the T5 shape).
    let case = case("changed");
    with_plan(&case.cur, &case.des, &case.policy, |current, desired, plan| {
        let (CurrentChild::Element(cur), DesiredChild::Element(des)) =
            (&current.children[1], &desired.children[1])
        else {
            panic!("blocks expected");
        };
        let alone = plan_node(cur, des, &case.policy).expect("the block changed");
        assert_eq!(Step::Patch(alone), plan.steps[1]);
    });
}
