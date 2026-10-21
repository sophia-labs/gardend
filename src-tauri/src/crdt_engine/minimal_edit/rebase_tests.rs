//! Tests for `minimal_edit::rebase` (owner U8; DESIGN §4.3).
//! Real yrs documents (built by the production builder, read by the production reader), real markdown
//! exporter and parser. The rebase itself is pure: no Doc is mutated.

use std::collections::BTreeSet;

use serde_json::{json, Value};
use yrs::{Doc, Transact};

use super::model::{desired_from_tiptap_nodes, read_fragment};
use super::rebase::rebase_on_live;
use super::types::{
    CurrentFragment, DesiredChild, DesiredFragment, IdPolicy, SourceFormat, WritePolicy,
};
use crate::crdt_engine::block_contract::normalise_doc_content;
use crate::crdt_engine::builder::ydoc_from_tiptap_json;
use crate::crdt_engine::content_parse::parse_write_content_for_operation;

fn json_policy() -> WritePolicy {
    WritePolicy {
        format: SourceFormat::TiptapJson,
        ids: IdPolicy::Bound,
    }
}

fn para(id: &str, text: &str) -> Value {
    json!({"type": "paragraph", "attrs": {"data-block-id": id},
           "content": [{"type": "text", "text": text}]})
}

fn embed(id: &str) -> Value {
    json!({"type": "objectEmbed", "attrs": {
        "data-block-id": id,
        "iri": "https://example.com/o/1",
        "classLabel": "Person",
        "label": "Ada",
    }})
}

fn desired_of(nodes: &[Value], policy: &WritePolicy) -> DesiredFragment {
    desired_from_tiptap_nodes(nodes, policy)
}

/// The room's live fragment for `nodes`, read the way the write path reads it.
fn current_of(nodes: &[Value], policy: &WritePolicy) -> CurrentFragment {
    let doc: Doc = ydoc_from_tiptap_json(&json!({"type": "doc", "content": nodes}));
    let fragment = doc.get_or_insert_xml_fragment("content");
    let txn = doc.transact();
    read_fragment(&txn, &fragment, policy)
}

fn hashes(fragment: &DesiredFragment) -> Vec<u64> {
    fragment
        .children
        .iter()
        .map(|child| match child {
            DesiredChild::Element(node) => node.hash,
            DesiredChild::Text(_) => panic!("top-level text"),
        })
        .collect()
}

fn element_json(fragment: &DesiredFragment, index: usize) -> String {
    match &fragment.children[index] {
        DesiredChild::Element(node) => node.json.to_string(),
        DesiredChild::Text(_) => panic!("top-level text"),
    }
}

fn tag(fragment: &DesiredFragment, index: usize) -> &str {
    match &fragment.children[index] {
        DesiredChild::Element(node) => &node.tag,
        DesiredChild::Text(_) => panic!("top-level text"),
    }
}

#[test]
fn untouched_block_keeps_live_edit() {
    let policy = json_policy();
    let base_nodes = vec![para("p1", "alpha"), para("p2", "beta")];
    let live_nodes = vec![para("p1", "alpha typed by a person"), para("p2", "beta")];
    let (result, edits) = rebase_on_live(
        &desired_of(&base_nodes, &policy),
        &current_of(&live_nodes, &policy),
        desired_of(&base_nodes, &policy), // the agent sends back what it read
        &policy,
    );
    assert_eq!(result.children.len(), 2);
    assert!(element_json(&result, 0).contains("typed by a person"));
    assert_eq!(edits.kept, vec!["p1".to_string()]);
    assert!(edits.conflicts.is_empty());
}

#[test]
fn agent_change_applies_when_live_unchanged() {
    let policy = json_policy();
    let base_nodes = vec![para("p1", "alpha"), para("p2", "beta")];
    let wanted = vec![para("p1", "alpha"), para("p2", "beta rewritten by the agent")];
    let (result, edits) = rebase_on_live(
        &desired_of(&base_nodes, &policy),
        &current_of(&base_nodes, &policy),
        desired_of(&wanted, &policy),
        &policy,
    );
    assert_eq!(result.children.len(), 2);
    assert!(element_json(&result, 1).contains("rewritten by the agent"));
    assert!(edits.kept.is_empty());
    assert!(edits.conflicts.is_empty());
}

#[test]
fn both_changed_keeps_current_and_reports_conflict() {
    let policy = json_policy();
    let base_nodes = vec![para("p1", "alpha"), para("p2", "beta")];
    let live_nodes = vec![para("p1", "alpha from the person"), para("p2", "beta")];
    let wanted = vec![para("p1", "alpha from the agent"), para("p2", "beta")];
    let (result, edits) = rebase_on_live(
        &desired_of(&base_nodes, &policy),
        &current_of(&live_nodes, &policy),
        desired_of(&wanted, &policy),
        &policy,
    );
    let first = element_json(&result, 0);
    assert!(first.contains("from the person"), "{first}");
    assert!(!first.contains("from the agent"), "{first}");
    assert_eq!(edits.conflicts, vec!["p1".to_string()]);
    assert!(edits.kept.is_empty());
}

#[test]
fn base_equal_current_is_identity() {
    let policy = json_policy();
    let base_nodes = vec![para("p1", "alpha"), para("p2", "beta")];
    let wanted = vec![
        para("p1", "alpha"),
        para("p2", "beta changed"),
        para("p3", "gamma added"),
    ];
    let desired = desired_of(&wanted, &policy);
    let expected = hashes(&desired);
    let (result, edits) = rebase_on_live(
        &desired_of(&base_nodes, &policy),
        &current_of(&base_nodes, &policy),
        desired,
        &policy,
    );
    assert_eq!(hashes(&result), expected);
    assert!(edits.kept.is_empty());
    assert!(edits.conflicts.is_empty());
}

#[test]
fn live_added_block_survives_in_place() {
    let policy = json_policy();
    let base_nodes = vec![para("p1", "alpha"), para("p2", "beta")];
    let live_nodes = vec![
        para("p1", "alpha"),
        para("p9", "typed in between"),
        para("p2", "beta"),
    ];
    let (result, edits) = rebase_on_live(
        &desired_of(&base_nodes, &policy),
        &current_of(&live_nodes, &policy),
        desired_of(&base_nodes, &policy),
        &policy,
    );
    assert_eq!(result.children.len(), 3);
    assert!(element_json(&result, 1).contains("typed in between"));
    assert_eq!(edits.kept, vec!["p9".to_string()]);
    assert!(edits.conflicts.is_empty());
}

#[test]
fn markdown_desired_against_json_base_uses_images() {
    // The agent read JSON-backed blocks as markdown, edited the last paragraph and sent markdown back. The
    // embed comes back as the quote the exporter renders for it; the base must match it by image.
    let policy_md_ids = |nodes: &[Value]| {
        let minted: BTreeSet<String> = nodes
            .iter()
            .filter_map(|n| n["attrs"]["data-block-id"].as_str().map(str::to_string))
            .collect();
        WritePolicy {
            format: SourceFormat::Markdown,
            ids: IdPolicy::Adopt { minted },
        }
    };
    let markdown = "intro\n\n> ⧉ Person — Ada <https://example.com/o/1>\n\noutro changed by the agent";
    let parsed = parse_write_content_for_operation(markdown, Some("markdown"), "rebase-test").unwrap();
    let blocks = parsed.tiptap_json["content"].as_array().unwrap().clone();
    let blocks = normalise_doc_content(&blocks, "rebase-test").0;
    let policy = policy_md_ids(&blocks);

    let base_nodes = vec![para("p1", "intro"), embed("e1"), para("p3", "outro")];
    let live_nodes = vec![
        para("p1", "intro, and a person kept typing"),
        embed("e1"),
        para("p3", "outro"),
    ];
    let (result, edits) = rebase_on_live(
        &desired_of(&base_nodes, &policy),
        &current_of(&live_nodes, &policy),
        desired_of(&blocks, &policy),
        &policy,
    );
    assert_eq!(result.children.len(), 3);
    assert!(element_json(&result, 0).contains("a person kept typing"));
    // Matched by image: the live objectEmbed stays, not the quote the markdown parsed to.
    assert_eq!(tag(&result, 1), "objectEmbed");
    assert!(element_json(&result, 2).contains("changed by the agent"));
    assert_eq!(edits.kept, vec!["p1".to_string()]);
    assert!(edits.conflicts.is_empty());
}
