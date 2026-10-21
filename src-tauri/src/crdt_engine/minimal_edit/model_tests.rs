//! Tests for `minimal_edit::model` (owner U2; DESIGN §4.3).

use std::path::{Path, PathBuf};

use serde_json::{json, Value};
use yrs::updates::decoder::Decode;
use yrs::{Any, Doc, Text, Transact, Update, XmlElementPrelim, XmlFragment, XmlTextPrelim};

use super::attrs::normal_form;
use super::model::{
    block_id, current_child_to_desired, current_node_json, desired_from_tiptap_nodes, inline_hash,
    is_inline_tag, read_fragment, segments_cur, segments_des, units_cur, units_des,
};
use super::types::{
    CurrentChild, CurrentFragment, CurrentNode, DesiredChild, DesiredNode, IdPolicy, Seg,
    SourceFormat, UnitKey, UnitOrigin, WritePolicy,
};
use crate::crdt_engine::builder::ydoc_from_tiptap_json;
use crate::crdt_engine::projection::ydoc_to_tiptap_json;

fn policy(format: SourceFormat) -> WritePolicy {
    WritePolicy {
        format,
        ids: IdPolicy::Bound,
    }
}

fn doc_of(content: Vec<Value>) -> Doc {
    ydoc_from_tiptap_json(&json!({ "type": "doc", "content": content }))
}

fn read(doc: &Doc, policy: &WritePolicy) -> CurrentFragment {
    let fragment = doc.get_or_insert_xml_fragment("content");
    let txn = doc.transact();
    read_fragment(&txn, &fragment, policy)
}

fn element(child: &CurrentChild) -> &CurrentNode {
    match child {
        CurrentChild::Element(node) => node,
        CurrentChild::Text(_) => panic!("expected an element"),
    }
}

fn desired_element(child: &DesiredChild) -> &DesiredNode {
    match child {
        DesiredChild::Element(node) => node,
        DesiredChild::Text(_) => panic!("expected an element"),
    }
}

fn text(text: &str) -> Value {
    json!({ "type": "text", "text": text })
}

fn marked(text: &str, marks: Value) -> Value {
    json!({ "type": "text", "text": text, "marks": marks })
}

fn paragraph(content: Vec<Value>) -> Value {
    json!({ "type": "paragraph", "content": content })
}

fn desired_hash(node: &Value, policy: &WritePolicy) -> u64 {
    let desired = desired_from_tiptap_nodes(std::slice::from_ref(node), policy);
    desired_element(&desired.children[0]).hash
}

fn current_hash(node: &Value, policy: &WritePolicy) -> u64 {
    let doc = doc_of(vec![node.clone()]);
    let fragment = read(&doc, policy);
    element(&fragment.children[0]).hash
}

fn fixtures_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("yrs_wire_compat")
}

fn doc_from_fixture(dir: &Path) -> Doc {
    let bytes = std::fs::read(dir.join("update-v1.bin")).expect("read update-v1.bin");
    let doc = Doc::new();
    {
        let mut txn = doc.transact_mut();
        txn.apply_update(Update::decode_v1(&bytes).expect("decode update"))
            .expect("apply update");
    }
    doc
}

#[test]
fn read_matches_projection_on_the_wire_compat_fixtures() {
    let root = fixtures_root();
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(&root)
        .expect("fixtures dir")
        .map(|entry| entry.expect("entry").path())
        .filter(|path| path.is_dir())
        .collect();
    dirs.sort();
    assert_eq!(dirs.len(), 4, "the four captured documents");
    for dir in dirs {
        let doc = doc_from_fixture(&dir);
        let projected = ydoc_to_tiptap_json(&doc);
        let expected = projected["content"].as_array().expect("content");
        for format in [SourceFormat::TiptapJson, SourceFormat::Markdown] {
            let fragment = read(&doc, &policy(format));
            // every top-level child of these documents is a block element
            assert_eq!(fragment.children.len(), expected.len(), "{}", dir.display());
            for (index, child) in fragment.children.iter().enumerate() {
                assert_eq!(
                    &current_node_json(element(child)),
                    &expected[index],
                    "{} block {index}",
                    dir.display()
                );
            }
        }
    }
}

#[test]
fn adjacent_xml_texts_hash_like_one_text() {
    for format in [SourceFormat::TiptapJson, SourceFormat::Markdown] {
        let policy = policy(format);
        let split = paragraph(vec![text("Hello "), text("world")]);
        let whole = paragraph(vec![text("Hello world")]);

        // the builder writes one Y.XmlText per TipTap text node, and the model records what is there
        let split_doc = doc_of(vec![split.clone()]);
        let whole_doc = doc_of(vec![whole.clone()]);
        let split_read = read(&split_doc, &policy);
        let whole_read = read(&whole_doc, &policy);
        assert_eq!(element(&split_read.children[0]).children.len(), 2);
        assert_eq!(element(&whole_read.children[0]).children.len(), 1);

        let hash = element(&split_read.children[0]).hash;
        assert_eq!(hash, element(&whole_read.children[0]).hash, "{format:?}");
        // and the desired side hashes the same content the same way
        assert_eq!(hash, desired_hash(&split, &policy));
        assert_eq!(hash, desired_hash(&whole, &policy));

        // the unit sequences agree too
        let split_children = &element(&split_read.children[0]).children;
        let whole_children = &element(&whole_read.children[0]).children;
        assert_eq!(
            inline_hash(&units_cur(split_children, 0..2, &policy), &policy),
            inline_hash(&units_cur(whole_children, 0..1, &policy), &policy)
        );

        // marks are part of the content, and segmentation of marked runs is not
        let bold_split = paragraph(vec![
            marked("Hel", json!([{ "type": "bold" }])),
            marked("lo", json!([{ "type": "bold" }])),
        ]);
        let bold_whole = paragraph(vec![marked("Hello", json!([{ "type": "bold" }]))]);
        assert_eq!(
            current_hash(&bold_split, &policy),
            current_hash(&bold_whole, &policy)
        );
        let plain = paragraph(vec![text("Hello")]);
        assert_ne!(
            current_hash(&bold_whole, &policy),
            current_hash(&plain, &policy)
        );
        // different characters differ
        let other = paragraph(vec![text("Hello "), text("w0rld")]);
        assert_ne!(hash, current_hash(&other, &policy));
        // the split point moves, the content does not
        let shifted = paragraph(vec![text("Hello w"), text("orld")]);
        assert_eq!(hash, current_hash(&shifted, &policy));
    }
}

#[test]
fn desired_groups_consecutive_text_nodes() {
    let policy = policy(SourceFormat::TiptapJson);
    let nodes = vec![
        text("tail"),
        json!({
            "type": "paragraph",
            "attrs": { "data-block-id": "p1", "indent": 0 },
            "content": [
                text("a"),
                marked("b", json!([{ "type": "bold" }, { "type": "link", "attrs": { "href": "h" } }])),
                text(""),
                text("c"),
                { "type": "hardBreak" },
                text("d")
            ]
        }),
        text(""),
    ];
    let desired = desired_from_tiptap_nodes(&nodes, &policy);
    // a top-level text node is a Text child; the trailing empty one is dropped
    assert_eq!(desired.children.len(), 2);
    let DesiredChild::Text(top) = &desired.children[0] else {
        panic!("expected text");
    };
    assert_eq!(top.len(), 1);
    assert_eq!(top[0].text, "tail");

    let node = desired_element(&desired.children[1]);
    assert_eq!(node.tag, "paragraph");
    assert_eq!(node.id.as_deref(), Some("p1"));
    assert_eq!(node.json, nodes[1], "the node's JSON is kept verbatim");
    // a, b, c are one group (the empty node between b and c is dropped); the hardBreak splits; d is alone
    assert_eq!(node.children.len(), 3);
    let DesiredChild::Text(group) = &node.children[0] else {
        panic!("expected a text group");
    };
    assert_eq!(
        group
            .iter()
            .map(|run| run.text.as_str())
            .collect::<Vec<_>>(),
        vec!["a", "b", "c"]
    );
    assert!(group[0].marks.is_empty() && group[0].raw_marks.is_empty());
    assert!(group[1].marks.contains_key("bold") && group[1].marks.contains_key("link"));
    assert_eq!(group[1].raw_marks["bold"], json!({}));
    assert_eq!(group[1].raw_marks["link"], json!({ "href": "h" }));
    assert_eq!(desired_element(&node.children[1]).tag, "hardBreak");
    let DesiredChild::Text(last) = &node.children[2] else {
        panic!("expected text");
    };
    assert_eq!(last.len(), 1);
    assert_eq!(last[0].text, "d");

    // ids: data-block-id wins, blockId is the fallback, empty strings are none
    assert_eq!(
        block_id(json!({ "blockId": "x" }).as_object().unwrap()),
        Some("x".into())
    );
    assert_eq!(
        block_id(
            json!({ "data-block-id": "a", "blockId": "b" })
                .as_object()
                .unwrap()
        ),
        Some("a".into())
    );
    assert_eq!(
        block_id(
            json!({ "data-block-id": null, "blockId": "b" })
                .as_object()
                .unwrap()
        ),
        Some("b".into())
    );
    assert_eq!(
        block_id(json!({ "data-block-id": "" }).as_object().unwrap()),
        None
    );
    assert_eq!(block_id(json!({}).as_object().unwrap()), None);

    // an element with only empty text has no children at all
    let empty = desired_from_tiptap_nodes(&[paragraph(vec![text("")])], &policy);
    assert!(desired_element(&empty.children[0]).children.is_empty());
}

#[test]
fn unit_byte_offsets_are_utf8() {
    let policy = policy(SourceFormat::TiptapJson);
    let sample = "aé—😀中b";
    let doc = doc_of(vec![paragraph(vec![text(sample), text("é!")])]);
    let fragment = read(&doc, &policy);
    let node = element(&fragment.children[0]);
    let CurrentChild::Text(first) = &node.children[0] else {
        panic!("expected text");
    };
    // the Doc counts UTF-8 bytes: 1 + 2 + 3 + 4 + 3 + 1
    assert_eq!(first.byte_len, 14);
    assert_eq!(sample.len(), 14);

    let units = units_cur(&node.children, 0..2, &policy);
    assert_eq!(units.len(), sample.chars().count() + 2);
    let mut expected = Vec::new();
    for (offset, c) in sample.char_indices() {
        expected.push((0usize, offset as u32, c));
    }
    expected.push((1, 0, 'é'));
    expected.push((1, 2, '!'));
    for (unit, (child, byte, c)) in units.iter().zip(expected) {
        assert_eq!(unit.key, UnitKey::Char(c));
        assert_eq!(unit.origin, UnitOrigin::CurText { child, byte }, "{c}");
    }
    // each offset lands on a char boundary of its text
    for unit in &units {
        let UnitOrigin::CurText { child, byte } = unit.origin else {
            panic!("expected a text origin");
        };
        let CurrentChild::Text(owner) = &node.children[child] else {
            panic!("expected text");
        };
        let joined: String = owner.runs.iter().map(|run| run.text.as_str()).collect();
        assert!(joined.is_char_boundary(byte as usize));
        assert!(joined[byte as usize..].starts_with(match unit.key {
            UnitKey::Char(c) => c,
            UnitKey::Atom(_) => unreachable!(),
        }));
    }

    // offsets run across the formatting runs of one Y.XmlText (the editor's shape: one text, several runs)
    let editor_doc = Doc::new();
    {
        let fragment = editor_doc.get_or_insert_xml_fragment("content");
        let mut txn = editor_doc.transact_mut();
        let block = fragment.insert(&mut txn, 0, XmlElementPrelim::empty("paragraph"));
        let run_text = block.insert(&mut txn, 0, XmlTextPrelim::new(""));
        run_text.insert(&mut txn, 0, "é");
        let mut bold = yrs::types::Attrs::new();
        bold.insert(
            "bold".into(),
            Any::Map(std::sync::Arc::new(Default::default())),
        );
        run_text.insert_with_attributes(&mut txn, 2, "—x", bold);
    }
    let fragment = read(&editor_doc, &policy);
    let node = element(&fragment.children[0]);
    let CurrentChild::Text(one_text) = &node.children[0] else {
        panic!("expected text");
    };
    assert_eq!(one_text.runs.len(), 2);
    assert_eq!(one_text.byte_len, 6);
    let units = units_cur(&node.children, 0..1, &policy);
    let origins: Vec<_> = units.iter().map(|unit| unit.origin).collect();
    assert_eq!(
        origins,
        vec![
            UnitOrigin::CurText { child: 0, byte: 0 },
            UnitOrigin::CurText { child: 0, byte: 2 },
            UnitOrigin::CurText { child: 0, byte: 5 },
        ]
    );
    assert!(units[0].marks.is_empty() && units[1].marks.contains_key("bold"));

    // desired units know their child and run, not bytes
    let desired = desired_from_tiptap_nodes(
        &[paragraph(vec![
            text("é"),
            marked("😀", json!([{ "type": "bold" }])),
            json!({ "type": "hardBreak" }),
            text("x"),
        ])],
        &policy,
    );
    let children = &desired_element(&desired.children[0]).children;
    let units = units_des(children, 0..3, &policy);
    assert_eq!(units.len(), 4);
    assert_eq!(units[0].origin, UnitOrigin::DesText { child: 0, run: 0 });
    assert_eq!(units[1].origin, UnitOrigin::DesText { child: 0, run: 1 });
    assert!(units[1].marks.contains_key("bold") && units[1].raw_marks.contains_key("bold"));
    assert_eq!(units[2].origin, UnitOrigin::DesAtom { child: 1 });
    assert!(matches!(units[2].key, UnitKey::Atom(_)));
    assert_eq!(units[3].origin, UnitOrigin::DesText { child: 2, run: 0 });
}

#[test]
fn segments_split_blocks_from_inline_runs() {
    let policy = policy(SourceFormat::TiptapJson);
    let quote = json!({
        "type": "blockquote",
        "content": [
            text("x"),
            paragraph(vec![text("p")]),
            text("y"),
            { "type": "mathInline", "attrs": { "src": "e" } },
            text("z"),
            paragraph(vec![text("q")]),
            { "type": "hardBreak" }
        ]
    });
    let expected = vec![
        Seg::Inline(0..1),
        Seg::Node(1),
        Seg::Inline(2..5),
        Seg::Node(5),
        Seg::Inline(6..7),
    ];

    let doc = doc_of(vec![quote.clone()]);
    let fragment = read(&doc, &policy);
    assert_eq!(
        segments_cur(&element(&fragment.children[0]).children),
        expected
    );

    let desired = desired_from_tiptap_nodes(&[quote], &policy);
    assert_eq!(
        segments_des(&desired_element(&desired.children[0]).children),
        expected
    );

    // block children alone are one Node each; a lone text run is one Inline
    let blocks = doc_of(vec![paragraph(vec![text("a")]), paragraph(vec![text("b")])]);
    let fragment = read(&blocks, &policy);
    assert_eq!(
        segments_cur(&fragment.children),
        vec![Seg::Node(0), Seg::Node(1)]
    );
    assert!(segments_cur(&[]).is_empty());
    assert!(segments_des(&[]).is_empty());

    for tag in [
        "hardBreak",
        "mathInline",
        "footnote",
        "wikilink",
        "citation",
        "tagChip",
    ] {
        assert!(is_inline_tag(tag), "{tag}");
    }
    for tag in ["paragraph", "image", "mathBlock", "objectEmbed", "listItem"] {
        assert!(!is_inline_tag(tag), "{tag}");
    }
}

#[test]
fn hash_ignores_block_ids() {
    for format in [
        SourceFormat::TiptapJson,
        SourceFormat::Markdown,
        SourceFormat::Restore,
    ] {
        let policy = policy(format);
        let with = |id_attrs: Value| {
            json!({
                "type": "blockquote",
                "attrs": id_attrs,
                "content": [{
                    "type": "paragraph",
                    "attrs": { "data-block-id": "inner" },
                    "content": [text("same")]
                }]
            })
        };
        let none = with(json!({}));
        let one = with(json!({ "data-block-id": "b1" }));
        let other = with(json!({ "data-block-id": "b2", "blockId": "legacy" }));
        for (a, b) in [(&none, &one), (&one, &other)] {
            assert_eq!(
                current_hash(a, &policy),
                current_hash(b, &policy),
                "{format:?}"
            );
            assert_eq!(
                desired_hash(a, &policy),
                desired_hash(b, &policy),
                "{format:?}"
            );
        }
        assert_eq!(current_hash(&none, &policy), desired_hash(&one, &policy));
        // a real difference still shows
        let changed = json!({
            "type": "blockquote",
            "content": [{ "type": "paragraph", "content": [text("different")] }]
        });
        assert_ne!(
            current_hash(&none, &policy),
            current_hash(&changed, &policy)
        );
    }
    // the id itself is still read
    let doc = doc_of(vec![json!({
        "type": "paragraph",
        "attrs": { "data-block-id": "b9" },
        "content": [text("x")]
    })]);
    let fragment = read(&doc, &policy(SourceFormat::Restore));
    assert_eq!(element(&fragment.children[0]).id.as_deref(), Some("b9"));
}

#[test]
fn hash_ignores_unstated_attrs_under_markdown_only() {
    let markdown = policy(SourceFormat::Markdown);
    let tiptap = policy(SourceFormat::TiptapJson);
    let plain = paragraph(vec![text("same")]);
    let aligned = json!({
        "type": "paragraph",
        "attrs": { "textAlign": "center", "collapsed": true, "data-source-id": "s" },
        "content": [text("same")]
    });
    assert_eq!(
        current_hash(&plain, &markdown),
        current_hash(&aligned, &markdown)
    );
    assert_eq!(
        desired_hash(&plain, &markdown),
        desired_hash(&aligned, &markdown)
    );
    assert_ne!(
        current_hash(&plain, &tiptap),
        current_hash(&aligned, &tiptap)
    );
    assert_ne!(
        desired_hash(&plain, &tiptap),
        desired_hash(&aligned, &tiptap)
    );

    // a stated attribute still counts under markdown
    let heading = |level: u32| json!({ "type": "heading", "attrs": { "level": level }, "content": [text("h")] });
    assert_ne!(
        current_hash(&heading(1), &markdown),
        current_hash(&heading(2), &markdown)
    );
    // and the schema default is no difference: a missing level is level 1
    let bare = json!({ "type": "heading", "content": [text("h")] });
    assert_eq!(
        current_hash(&bare, &markdown),
        current_hash(&heading(1), &markdown)
    );

    // unstated marks: a comment mark is invisible to markdown, visible to tiptapJson
    let commented = paragraph(vec![marked(
        "same",
        json!([{ "type": "commentMark", "attrs": { "commentId": "c1" } }]),
    )]);
    assert_eq!(
        current_hash(&plain, &markdown),
        current_hash(&commented, &markdown)
    );
    assert_eq!(
        desired_hash(&plain, &markdown),
        desired_hash(&commented, &markdown)
    );
    assert_ne!(
        current_hash(&plain, &tiptap),
        current_hash(&commented, &tiptap)
    );
    // stated marks count under both
    let bold = paragraph(vec![marked("same", json!([{ "type": "bold" }]))]);
    assert_ne!(
        current_hash(&plain, &markdown),
        current_hash(&bold, &markdown)
    );

    // a link the editor wrote equals one the agent wrote
    let editor_link = paragraph(vec![marked(
        "l",
        json!([{ "type": "link", "attrs": {
            "href": "u", "target": "_blank", "rel": "noopener noreferrer nofollow", "class": null
        } }]),
    )]);
    let agent_link = paragraph(vec![marked(
        "l",
        json!([{ "type": "link", "attrs": { "href": "u" } }]),
    )]);
    assert_eq!(
        current_hash(&editor_link, &tiptap),
        desired_hash(&agent_link, &tiptap)
    );
}

#[test]
fn current_to_desired_round_trips_hash() {
    let blocks = vec![
        json!({
            "type": "heading",
            "attrs": { "data-block-id": "h1", "level": 2, "indent": 0, "collapsed": false },
            "content": [text("Title "), marked("bold", json!([{ "type": "bold" }]))]
        }),
        json!({
            "type": "blockquote",
            "attrs": { "data-block-id": "q1" },
            "content": [
                {
                    "type": "paragraph",
                    "attrs": { "data-block-id": "q2" },
                    "content": [
                        text("quote é—😀 "),
                        { "type": "hardBreak" },
                        marked("link", json!([
                            { "type": "link", "attrs": { "href": "https://x.test" } },
                            { "type": "commentMark", "attrs": { "commentId": "c1" } }
                        ])),
                        { "type": "mathInline", "attrs": { "src": "x^2" } },
                        text(" end")
                    ]
                }
            ]
        }),
        json!({
            "type": "listItem",
            "attrs": { "listType": "ordered", "indent": 1, "checked": null },
            "content": [paragraph(vec![text("item")])]
        }),
        json!({ "type": "horizontalRule", "attrs": { "data-block-id": "hr" } }),
        paragraph(vec![]),
    ];
    for format in [
        SourceFormat::TiptapJson,
        SourceFormat::Markdown,
        SourceFormat::Html,
        SourceFormat::Blocks,
        SourceFormat::Plain,
    ] {
        let policy = policy(format);
        let doc = doc_of(blocks.clone());
        let fragment = read(&doc, &policy);
        assert_eq!(fragment.children.len(), blocks.len());
        let desired = desired_from_tiptap_nodes(&blocks, &policy);
        for (index, child) in fragment.children.iter().enumerate() {
            let node = element(child);
            // the current block, turned back into a desired block, hashes as it did
            let converted = current_child_to_desired(child, &policy);
            let converted_node = desired_element(&converted);
            assert_eq!(converted_node.hash, node.hash, "{format:?} block {index}");
            assert_eq!(converted_node.tag, node.tag);
            assert_eq!(converted_node.id, node.id);
            assert_eq!(converted_node.json, current_node_json(node));
            // and it hashes as the TipTap JSON the document was built from
            assert_eq!(
                desired_element(&desired.children[index]).hash,
                node.hash,
                "{format:?} block {index} from JSON"
            );
            // the JSON of the read node is the JSON that was written, up to the semantic normal form
            let wrap = |node: Value| json!({ "type": "doc", "content": [node] });
            assert_eq!(
                normal_form(&wrap(current_node_json(node)), true),
                normal_form(&wrap(blocks[index].clone()), true),
                "{format:?} block {index} JSON"
            );
        }
    }

    // reading never touches the document
    let doc = doc_of(blocks);
    let before = ydoc_to_tiptap_json(&doc);
    let _ = read(&doc, &policy(SourceFormat::TiptapJson));
    assert_eq!(ydoc_to_tiptap_json(&doc), before);
}
