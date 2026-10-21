//! Tests for `minimal_edit::md_image` (owner U7; DESIGN §4.3).
//! Pure: real exporter, real parser, real normaliser; no Doc is touched.

use std::collections::BTreeSet;

use serde_json::{json, Value};

use super::md_image::{block_image, inline_atom_image};
use super::model::desired_from_tiptap_nodes;
use super::types::{
    AtomImage, DesiredChild, IdPolicy, Image, SourceFormat, UnitKey, WritePolicy,
};
use crate::crdt_engine::block_contract::normalise_doc_content;
use crate::crdt_engine::content_parse::parse_write_content_for_operation;

fn markdown_policy() -> WritePolicy {
    WritePolicy {
        format: SourceFormat::Markdown,
        ids: IdPolicy::Adopt {
            minted: BTreeSet::new(),
        },
    }
}

fn text(t: &str) -> Value {
    json!({"type": "text", "text": t})
}

fn embed(id: &str) -> Value {
    json!({"type": "objectEmbed", "attrs": {
        "data-block-id": id,
        "iri": "https://example.com/o/1",
        "classLabel": "Person",
        "label": "Ada",
    }})
}

fn chars(s: &str) -> Vec<UnitKey> {
    s.chars().map(UnitKey::Char).collect()
}

/// The hash of the single block a markdown write of `markdown` stores.
fn parsed_block_hash(markdown: &str, policy: &WritePolicy) -> u64 {
    let parsed =
        parse_write_content_for_operation(markdown, Some("markdown"), "md-image-test").unwrap();
    let blocks = parsed.tiptap_json["content"].as_array().unwrap().clone();
    let blocks = normalise_doc_content(&blocks, "md-image-test").0;
    let fragment = desired_from_tiptap_nodes(&blocks, policy);
    assert_eq!(fragment.children.len(), 1, "one block expected");
    match &fragment.children[0] {
        DesiredChild::Element(node) => node.hash,
        DesiredChild::Text(_) => panic!("a block, not text"),
    }
}

#[test]
fn paragraph_image_is_same() {
    let paragraph = json!({"type": "paragraph", "attrs": {"data-block-id": "p1"}, "content": [
        text("plain and "),
        {"type": "text", "text": "bold", "marks": [{"type": "bold"}]},
    ]});
    assert_eq!(block_image(&paragraph, &markdown_policy()), Image::Same);
}

#[test]
fn object_embed_image_is_a_quote_with_a_link() {
    let policy = markdown_policy();
    let image = block_image(&embed("e1"), &policy);
    let Image::Nodes { hash, count } = image else {
        panic!("an objectEmbed reads back as other nodes, got {image:?}");
    };
    assert_eq!(count, 1);
    // The same hash the parser gives the line the exporter documents: `> ⧉ Class — Title <iri>`.
    let expected = parsed_block_hash("> ⧉ Person — Ada <https://example.com/o/1>", &policy);
    assert_eq!(hash, expected);
}

#[test]
fn query_block_is_unexpressible() {
    let query = json!({"type": "queryBlock", "attrs": {
        "data-block-id": "q1", "query": "SELECT * WHERE { ?s ?p ?o }"}});
    assert_eq!(
        block_image(&query, &markdown_policy()),
        Image::Unexpressible
    );
}

#[test]
fn math_inline_atom_image_is_dollar_text() {
    let atom = json!({"type": "mathInline", "attrs": {"src": "x^2"}});
    assert_eq!(
        inline_atom_image(&atom, &markdown_policy()),
        AtomImage::Units(chars("$x^2$"))
    );
}

#[test]
fn wikilink_image_is_bracket_label() {
    let atom = json!({"type": "wikilink", "attrs": {"label": "Ada", "targetDocId": "doc-1"}});
    assert_eq!(
        inline_atom_image(&atom, &markdown_policy()),
        AtomImage::Units(chars("[[Ada]]"))
    );
}

#[test]
fn hard_break_image_is_same() {
    let policy = markdown_policy();
    assert_eq!(
        inline_atom_image(&json!({"type": "hardBreak"}), &policy),
        AtomImage::Same
    );
    let paragraph = json!({"type": "paragraph", "attrs": {"data-block-id": "p1"}, "content": [
        text("first"), {"type": "hardBreak"}, text("second"),
    ]});
    assert_eq!(block_image(&paragraph, &policy), Image::Same);
}

#[test]
fn table_image_is_nodes() {
    // A first row of plain cells: GFM needs a header row, so the round trip promotes it to one.
    let cell = |t: &str| {
        json!({"type": "tableCell", "content": [
            {"type": "paragraph", "content": [text(t)]}]})
    };
    let table = json!({"type": "table", "attrs": {"data-block-id": "t1"}, "content": [
        {"type": "tableRow", "content": [cell("a"), cell("b")]},
        {"type": "tableRow", "content": [cell("c"), cell("d")]},
    ]});
    let image = block_image(&table, &markdown_policy());
    assert!(
        matches!(image, Image::Nodes { count: 1, .. }),
        "a promoted header row reads back as a different table, got {image:?}"
    );
}

#[test]
fn image_ignores_minted_ids() {
    let policy = WritePolicy {
        format: SourceFormat::Markdown,
        ids: IdPolicy::Adopt {
            minted: ["first".to_string()].into_iter().collect(),
        },
    };
    let a = block_image(&embed("first"), &policy);
    let b = block_image(&embed("second"), &policy);
    assert!(matches!(a, Image::Nodes { .. }));
    assert_eq!(a, b);
}
