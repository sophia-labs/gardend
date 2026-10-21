//! Tests for `minimal_edit::attrs` (owner U1; DESIGN §4.3).

use std::collections::BTreeSet;

use serde_json::{json, Map, Value};

use super::attrs::{
    mark_stated, marks_from_tiptap, marks_from_y, node_attr_stated, node_expressible, normal_form,
    normal_form_stated, raw_marks_to_y, sem_mark_attrs, sem_node_attrs, sem_value, Fnv64, ID_ATTRS,
};
use super::types::{RawMarks, SourceFormat};

const SCHEMA_FIXTURE: &str =
    include_str!("../../../tests/fixtures/block_contract/editor-kernel-319e0253.schema.json");
const CONTRACT: &str = include_str!("../block-contract.v1.json");

fn object(value: Value) -> Map<String, Value> {
    match value {
        Value::Object(map) => map,
        other => panic!("not an object: {other}"),
    }
}

fn fixture() -> Value {
    serde_json::from_str(SCHEMA_FIXTURE).expect("schema fixture parses")
}

fn contract() -> Value {
    serde_json::from_str(CONTRACT).expect("block contract parses")
}

#[test]
fn defaults_table_matches_the_editor_schema_fixture() {
    let schema = fixture();
    let mut checked = 0;
    let mut non_null = 0;
    for (kind, node) in [("nodes", true), ("marks", false)] {
        for entry in schema[kind].as_array().expect("entries") {
            let name = entry["name"].as_str().expect("name");
            let mut expected = std::collections::BTreeMap::new();
            for (attr, spec) in entry["attrs"].as_object().expect("attrs") {
                if let Some(default) = sem_value(&spec["default"]) {
                    expected.insert(attr.clone(), default);
                }
            }
            non_null += expected.len();
            // the table must give exactly the fixture's non-null defaults: nothing missing, nothing extra
            let actual = if node {
                sem_node_attrs(name, &Map::new())
            } else {
                sem_mark_attrs(name, None)
            };
            assert_eq!(actual, expected, "{kind} {name}");
            checked += 1;
        }
    }
    assert!(checked > 30, "walked the whole fixture");
    assert_eq!(non_null, 52, "DESIGN Appendix B counts 52 defaults");
}

#[test]
fn markdown_stated_marks_are_the_contracts_lossless_marks() {
    let contract = contract();
    let mut lossless = BTreeSet::new();
    let mut all = Vec::new();
    for mark in contract["marks"].as_array().expect("marks") {
        let name = mark["name"].as_str().expect("name").to_string();
        if mark["markdown"]["lossless"].as_bool() == Some(true) {
            lossless.insert(name.clone());
        }
        all.push(name);
    }
    let stated: BTreeSet<String> = all
        .iter()
        .filter(|name| mark_stated(SourceFormat::Markdown, name))
        .cloned()
        .collect();
    assert_eq!(stated, lossless);
    assert!(!lossless.is_empty());
    // a mark the contract does not know is not something markdown can state
    assert!(!mark_stated(SourceFormat::Markdown, "wireMark"));
    // formats that state everything, and plain that states nothing
    assert!(mark_stated(SourceFormat::TiptapJson, "commentMark"));
    assert!(mark_stated(SourceFormat::Restore, "commentMark"));
    assert!(!mark_stated(SourceFormat::Plain, "bold"));
    assert!(mark_stated(SourceFormat::Blocks, "commentMark"));
    assert!(mark_stated(SourceFormat::Html, "underline"));
    assert!(!mark_stated(SourceFormat::Markdown, "underline"));
}

#[test]
fn markdown_unexpressible_nodes_are_the_not_exported_nodes() {
    let contract = contract();
    let mut not_exported = BTreeSet::new();
    let mut all = Vec::new();
    for node in contract["nodes"].as_array().expect("nodes") {
        let name = node["name"].as_str().expect("name").to_string();
        let form = node["markdown"]["form"].as_str().expect("form");
        if form.starts_with("(not exported)") || form.starts_with("(never stored") {
            not_exported.insert(name.clone());
        }
        all.push(name);
    }
    for format in [SourceFormat::Markdown, SourceFormat::Html] {
        let unexpressible: BTreeSet<String> = all
            .iter()
            .filter(|name| !node_expressible(format, name))
            .cloned()
            .collect();
        assert_eq!(unexpressible, not_exported, "{format:?}");
    }
    assert!(!not_exported.is_empty());
    for format in [
        SourceFormat::TiptapJson,
        SourceFormat::Blocks,
        SourceFormat::Xml,
        SourceFormat::Plain,
        SourceFormat::Restore,
    ] {
        assert!(node_expressible(format, "queryBlock"), "{format:?}");
    }
}

#[test]
fn carry_rules_state_what_each_format_can_say() {
    // ids are never stated by any format
    for format in [
        SourceFormat::TiptapJson,
        SourceFormat::Blocks,
        SourceFormat::Markdown,
        SourceFormat::Html,
        SourceFormat::Xml,
        SourceFormat::Plain,
        SourceFormat::Restore,
    ] {
        for id in ID_ATTRS {
            assert!(!node_attr_stated(format, "paragraph", id), "{format:?} {id}");
        }
    }
    assert!(node_attr_stated(SourceFormat::TiptapJson, "paragraph", "textAlign"));
    assert!(node_attr_stated(SourceFormat::Restore, "paragraph", "data-tags"));
    assert!(node_attr_stated(SourceFormat::Markdown, "heading", "level"));
    assert!(node_attr_stated(SourceFormat::Markdown, "listItem", "indent"));
    assert!(node_attr_stated(SourceFormat::Markdown, "codeBlock", "language"));
    assert!(node_attr_stated(SourceFormat::Markdown, "image", "src"));
    assert!(!node_attr_stated(SourceFormat::Markdown, "paragraph", "textAlign"));
    assert!(!node_attr_stated(SourceFormat::Markdown, "paragraph", "collapsed"));
    assert!(!node_attr_stated(SourceFormat::Markdown, "paragraph", "data-source-id"));
    assert!(node_attr_stated(SourceFormat::Html, "paragraph", "textAlign"));
    assert!(node_attr_stated(SourceFormat::Html, "paragraph", "data-pdf-page"));
    assert!(!node_attr_stated(SourceFormat::Html, "paragraph", "data-tags"));
    assert!(node_attr_stated(SourceFormat::Blocks, "listItem", "checked"));
    assert!(!node_attr_stated(SourceFormat::Blocks, "listItem", "indent"));
    assert!(!node_attr_stated(SourceFormat::Plain, "heading", "level"));
}

#[test]
fn link_defaults_make_editor_and_agent_links_equal() {
    let editor = json!([{
        "type": "link",
        "attrs": {
            "href": "https://example.com",
            "target": "_blank",
            "rel": "noopener noreferrer nofollow",
            "class": null
        }
    }]);
    let agent = json!([{ "type": "link", "attrs": { "href": "https://example.com" } }]);
    let (editor_sem, editor_raw) = marks_from_tiptap(Some(&editor));
    let (agent_sem, agent_raw) = marks_from_tiptap(Some(&agent));
    assert_eq!(editor_sem, agent_sem);
    // the raw values stay what each side wrote: only the comparison is semantic
    assert_ne!(editor_raw, agent_raw);

    let other = json!([{ "type": "link", "attrs": { "href": "https://example.org" } }]);
    assert_ne!(marks_from_tiptap(Some(&other)).0, agent_sem);

    // node side: indent 0 and collapsed false equal their absence
    let stated = object(json!({ "indent": 0, "collapsed": false }));
    assert_eq!(
        sem_node_attrs("paragraph", &stated),
        sem_node_attrs("paragraph", &Map::new())
    );
    let moved = object(json!({ "indent": 1 }));
    assert_ne!(
        sem_node_attrs("paragraph", &moved),
        sem_node_attrs("paragraph", &Map::new())
    );
}

#[test]
fn nulls_and_numbers_are_semantic() {
    assert_eq!(sem_value(&Value::Null), None);
    assert_eq!(sem_value(&json!(1)), sem_value(&json!(1.0)));
    assert_eq!(sem_value(&json!(2)), Some(json!(2.0)));
    assert_ne!(sem_value(&json!(1)), sem_value(&json!(2)));
    assert_eq!(sem_value(&json!("x")), Some(json!("x")));
    // nested: object nulls are absences, array nulls keep their position
    assert_eq!(
        sem_value(&json!({ "a": null, "b": [1, null] })),
        Some(json!({ "b": [1.0, null] }))
    );

    let int_level = object(json!({ "level": 2 }));
    let float_level = object(json!({ "level": 2.0 }));
    assert_eq!(
        sem_node_attrs("heading", &int_level),
        sem_node_attrs("heading", &float_level)
    );
    // an explicit null is an absent attribute: the default applies
    let null_level = object(json!({ "level": null, "data-tags": null }));
    let attrs = sem_node_attrs("heading", &null_level);
    assert_eq!(attrs.get("level"), Some(&json!(1.0)));
    assert!(!attrs.contains_key("data-tags"));
    // ids are kept: callers exclude them where needed
    let with_id = object(json!({ "data-block-id": "b1" }));
    assert_eq!(
        sem_node_attrs("paragraph", &with_id).get("data-block-id"),
        Some(&json!("b1"))
    );
    // a tag the schema does not know has no defaults
    assert!(sem_node_attrs("mystery", &Map::new()).is_empty());
}

#[test]
fn fnv_is_stable() {
    // the published FNV-1a 64 test vectors
    assert_eq!(Fnv64::new().finish(), 0xcbf2_9ce4_8422_2325);
    let mut a = Fnv64::new();
    a.write(b"a");
    assert_eq!(a.finish(), 0xaf63_dc4c_8601_ec8c);
    let mut foobar = Fnv64::new();
    foobar.write(b"foobar");
    assert_eq!(foobar.finish(), 0x8594_4171_f739_67e8);

    // the framing is part of the contract: little-endian length prefix
    let mut one = Fnv64::new();
    one.write_u64(1);
    assert_eq!(one.finish(), 0x89cd_3129_1d2a_efa4);
    let mut ab = Fnv64::new();
    ab.write_str("ab");
    assert_eq!(ab.finish(), 0x9c60_01d3_28a9_4690);
    let mut split_left = Fnv64::new();
    split_left.write_str("ab");
    split_left.write_str("c");
    let mut split_right = Fnv64::new();
    split_right.write_str("a");
    split_right.write_str("bc");
    assert_eq!(split_left.finish(), 0x7e60_470b_f599_cad6);
    assert_eq!(split_right.finish(), 0xba1e_1f0e_0704_d8ea);

    let mut value = Fnv64::new();
    value.write_value(&json!({ "b": 1, "a": [true, null, "x"] }));
    assert_eq!(value.finish(), 0x1c90_c354_99b8_9511);
}

fn value_hash(value: &Value) -> u64 {
    let mut hasher = Fnv64::new();
    hasher.write_value(value);
    hasher.finish()
}

#[test]
fn write_value_is_order_independent_for_objects() {
    let one: Value = serde_json::from_str(r#"{"a":1,"b":{"x":true,"y":[1,2]},"c":"s"}"#).unwrap();
    let two: Value = serde_json::from_str(r#"{"c":"s","b":{"y":[1,2],"x":true},"a":1}"#).unwrap();
    assert_eq!(value_hash(&one), value_hash(&two));
    // arrays are ordered
    assert_ne!(value_hash(&json!([1, 2])), value_hash(&json!([2, 1])));
    // a type tag precedes each value: "1", 1 and true differ
    assert_ne!(value_hash(&json!("1")), value_hash(&json!(1)));
    assert_ne!(value_hash(&json!(1)), value_hash(&json!(true)));
    // numbers hash as f64
    assert_eq!(value_hash(&json!(1)), value_hash(&json!(1.0)));
    // content matters
    assert_ne!(value_hash(&json!({ "a": 1 })), value_hash(&json!({ "a": 2 })));
    assert_ne!(value_hash(&json!({ "a": 1 })), value_hash(&json!({ "b": 1 })));
}

fn rich_doc() -> Value {
    json!({
        "type": "doc",
        "content": [
            {
                "type": "paragraph",
                "attrs": { "data-block-id": "b1", "indent": 0, "textAlign": null },
                "content": [
                    { "type": "text", "text": "Hel", "marks": [
                        { "type": "italic" }, { "type": "bold" }
                    ] },
                    { "type": "text", "text": "" },
                    { "type": "text", "text": "lo", "marks": [
                        { "type": "bold" }, { "type": "italic", "attrs": {} }
                    ] },
                    { "type": "text", "text": " w" }
                ]
            },
            {
                "type": "heading",
                "attrs": { "level": 2, "data-block-id": "b2" },
                "content": [
                    { "type": "text", "text": "T", "marks": [
                        { "type": "link", "attrs": { "href": "https://x.test" } }
                    ] }
                ]
            },
            { "type": "paragraph", "content": [] }
        ]
    })
}

#[test]
fn normal_form_merges_adjacent_equal_text_and_sorts_marks() {
    let normal = normal_form(&rich_doc(), false);
    let paragraph = &normal["content"][0];
    assert_eq!(paragraph["type"], "paragraph");
    // ids dropped, defaults filled, the null textAlign gone, numbers f64
    assert_eq!(paragraph["attrs"], json!({ "collapsed": false, "indent": 0.0 }));
    // "Hel" + "" + "lo" merge (the same marks in any order); " w" stays apart; bold sorts before italic
    assert_eq!(
        paragraph["content"],
        json!([
            { "type": "text", "text": "Hello", "marks": [{ "type": "bold" }, { "type": "italic" }] },
            { "type": "text", "text": " w" }
        ])
    );
    // the link mark carries its schema defaults
    assert_eq!(
        normal["content"][1]["content"][0]["marks"],
        json!([{ "type": "link", "attrs": {
            "href": "https://x.test",
            "rel": "noopener noreferrer nofollow",
            "target": "_blank"
        } }])
    );
    // an empty content array is dropped
    assert!(normal["content"][2].get("content").is_none());

    // with ids the block ids stay
    let with_ids = normal_form(&rich_doc(), true);
    assert_eq!(with_ids["content"][0]["attrs"]["data-block-id"], "b1");
    assert_eq!(with_ids["content"][1]["attrs"]["data-block-id"], "b2");
    // text nodes with different marks do not merge
    let split = normal_form(
        &json!({ "type": "paragraph", "content": [
            { "type": "text", "text": "a", "marks": [{ "type": "bold" }] },
            { "type": "text", "text": "b" }
        ] }),
        false,
    );
    assert_eq!(split["content"].as_array().unwrap().len(), 2);
}

#[test]
fn normal_form_is_idempotent() {
    for with_ids in [false, true] {
        let once = normal_form(&rich_doc(), with_ids);
        assert_eq!(normal_form(&once, with_ids), once, "with_ids {with_ids}");
        for format in [SourceFormat::Markdown, SourceFormat::Html, SourceFormat::Blocks] {
            let once = normal_form_stated(&rich_doc(), format, with_ids);
            assert_eq!(
                normal_form_stated(&once, format, with_ids),
                once,
                "{format:?} with_ids {with_ids}"
            );
        }
    }
    // two spellings of one document meet in the same normal form
    let spelled_out = json!({
        "type": "doc",
        "content": [{
            "type": "paragraph",
            "attrs": { "collapsed": false, "indent": 0, "data-block-id": "zzz" },
            "content": [{ "type": "text", "text": "x" }]
        }]
    });
    let terse = json!({
        "type": "doc",
        "content": [{ "type": "paragraph", "content": [{ "type": "text", "text": "x" }] }]
    });
    assert_eq!(normal_form(&spelled_out, false), normal_form(&terse, false));
    assert_ne!(normal_form(&spelled_out, true), normal_form(&terse, true));
}

#[test]
fn normal_form_stated_drops_unstated_attrs_for_markdown() {
    let doc = json!({
        "type": "doc",
        "content": [
            {
                "type": "paragraph",
                "attrs": { "data-block-id": "p1", "textAlign": "center", "collapsed": true },
                "content": [
                    { "type": "text", "text": "a", "marks": [{ "type": "bold" }] },
                    { "type": "text", "text": "b", "marks": [
                        { "type": "bold" }, { "type": "commentMark", "attrs": { "commentId": "c1" } }
                    ] }
                ]
            },
            { "type": "heading", "attrs": { "level": 3, "indent": 2 }, "content": [
                { "type": "text", "text": "H" }
            ] },
            { "type": "listItem", "attrs": { "listType": "ordered", "indent": 1, "collapsed": true,
                "checked": true, "data-tags": "t" }, "content": [
                { "type": "paragraph", "content": [{ "type": "text", "text": "i" }] }
            ] }
        ]
    });
    let stated = normal_form_stated(&doc, SourceFormat::Markdown, false);
    let paragraph = &stated["content"][0];
    // no stated attribute: no attrs at all; the unstated comment mark is gone, so "a" and "b" merge
    assert!(paragraph.get("attrs").is_none());
    assert_eq!(
        paragraph["content"],
        json!([{ "type": "text", "text": "ab", "marks": [{ "type": "bold" }] }])
    );
    // heading keeps level, loses indent
    assert_eq!(stated["content"][1]["attrs"], json!({ "level": 3.0 }));
    // list item keeps listType, indent and checked, loses collapsed and tags
    assert_eq!(
        stated["content"][2]["attrs"],
        json!({ "listType": "ordered", "indent": 1.0, "checked": true })
    );

    // the unrestricted form keeps all of it
    let full = normal_form(&doc, false);
    assert_eq!(full["content"][0]["attrs"]["textAlign"], "center");
    assert_eq!(full["content"][0]["content"].as_array().unwrap().len(), 2);

    // ids survive with_ids even though no format states them
    let with_ids = normal_form_stated(&doc, SourceFormat::Markdown, true);
    assert_eq!(with_ids["content"][0]["attrs"], json!({ "data-block-id": "p1" }));

    // tiptapJson states everything
    assert_eq!(
        normal_form_stated(&doc, SourceFormat::TiptapJson, false),
        normal_form(&doc, false)
    );
}

#[test]
fn raw_marks_round_trip_through_y_attrs() {
    let raw: RawMarks = [
        ("bold".to_string(), json!({})),
        (
            "link".to_string(),
            json!({ "href": "https://x.test", "target": "_blank" }),
        ),
        ("commentMark".to_string(), json!({ "commentId": "c-1" })),
    ]
    .into_iter()
    .collect();
    let attrs = raw_marks_to_y(&raw);
    assert_eq!(attrs.len(), 3);
    let (sem, back) = marks_from_y(Some(&attrs));
    assert_eq!(back, raw);
    // the semantic marks equal what the TipTap spelling of the same marks gives
    let tiptap = json!([
        { "type": "commentMark", "attrs": { "commentId": "c-1" } },
        { "type": "link", "attrs": { "href": "https://x.test" } },
        { "type": "bold" }
    ]);
    let (tiptap_sem, tiptap_raw) = marks_from_tiptap(Some(&tiptap));
    assert_eq!(sem, tiptap_sem);
    assert_eq!(tiptap_raw["bold"], json!({}));

    // numbers survive as f64: the raw value equals its f64 spelling
    let numbered: RawMarks = [("textStyle".to_string(), json!({ "fontSize": 12.0 }))]
        .into_iter()
        .collect();
    assert_eq!(marks_from_y(Some(&raw_marks_to_y(&numbered))).1, numbered);

    // no formatting at all
    let (none_sem, none_raw) = marks_from_y(None);
    assert!(none_sem.is_empty() && none_raw.is_empty());
    let (empty_sem, empty_raw) = marks_from_tiptap(None);
    assert!(empty_sem.is_empty() && empty_raw.is_empty());

    // a legacy `true` mark reads as true, and equals the bare mark semantically
    let mut legacy = yrs::types::Attrs::new();
    legacy.insert("bold".into(), yrs::Any::Bool(true));
    let (legacy_sem, legacy_raw) = marks_from_y(Some(&legacy));
    assert_eq!(legacy_raw["bold"], json!(true));
    assert_eq!(
        legacy_sem,
        marks_from_tiptap(Some(&json!([{ "type": "bold" }]))).0
    );
}
