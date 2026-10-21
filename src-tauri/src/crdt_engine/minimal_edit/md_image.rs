//! Markdown images: what a node becomes after `tiptap_json_to_markdown` and the write path's parser.
//!
//! `read_document` serves markdown that renders some nodes in lossy forms (an objectEmbed as a quote with a
//! link, a wikilink as `[[label]]`, mathInline as `$src$`, tables as GFM). An agent that sends that markdown
//! back would replace every such node with what the markdown parses to. The planner asks for a current node's
//! image and keeps the node when the desired run equals it (DESIGN §2.4). Pure: nothing here touches a Doc.

use serde_json::{json, Value};

use super::attrs::{node_expressible, Fnv64};
use super::model::{desired_from_tiptap_nodes, inline_hash, units_des};
use super::types::{AtomImage, DesiredChild, Image, SourceFormat, UnitKey, WritePolicy};
use crate::crdt_engine::block_contract::normalise_doc_content;
use crate::crdt_engine::content_parse::parse_write_content_for_operation;
use crate::crdt_engine::markdown_export::tiptap_json_to_markdown;

/// The seed for ids the parser and the normaliser mint here. Hashes exclude ids, so it only has to be stable.
const IMAGE_SEED: &str = "md-image";

fn node_type(node: &Value) -> &str {
    node.get("type").and_then(Value::as_str).unwrap_or("")
}

/// One hash for a list of desired children: element hashes and inline-run hashes, in order.
fn combined_hash(children: &[DesiredChild], policy: &WritePolicy) -> u64 {
    let mut hasher = Fnv64::new();
    for (index, child) in children.iter().enumerate() {
        match child {
            DesiredChild::Element(node) => hasher.write_u64(node.hash),
            DesiredChild::Text(_) => {
                let units = units_des(children, index..index + 1, policy);
                hasher.write_u64(inline_hash(&units, policy));
            }
        }
    }
    hasher.finish()
}

/// Export `nodes` as a document, parse it back as a markdown write and normalise the result: the blocks a
/// markdown write of the exported text would store. `None` if the parser refuses (it degrades instead, so
/// this is a belt for the type).
fn round_trip(nodes: Vec<Value>) -> Option<Vec<Value>> {
    let markdown = tiptap_json_to_markdown(&json!({"type": "doc", "content": nodes}));
    let parsed =
        parse_write_content_for_operation(&markdown, Some("markdown"), IMAGE_SEED).ok()?;
    let blocks = parsed
        .tiptap_json
        .get("content")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    Some(normalise_doc_content(&blocks, IMAGE_SEED).0)
}

/// Export `node` alone (`markdown_export::tiptap_json_to_markdown`), parse it back
/// (`content_parse::parse_write_content_for_operation(md, Some("markdown"), "md-image")`), normalise it
/// (`block_contract::normalise_doc_content`) and hash the result with `policy`.
pub(crate) fn block_image(node: &Value, policy: &WritePolicy) -> Image {
    // Markdown cannot say these at all, whatever the write's own format: a markdown write omits them.
    if !node_expressible(SourceFormat::Markdown, node_type(node)) {
        return Image::Unexpressible;
    }
    let own = desired_from_tiptap_nodes(std::slice::from_ref(node), policy);
    let own_hash = combined_hash(&own.children, policy);
    let Some(back) = round_trip(vec![node.clone()]) else {
        // A hash no desired run can reach: the node is simply not matched by image.
        return Image::Nodes {
            hash: own_hash.wrapping_add(1),
            count: 1,
        };
    };
    let parsed = desired_from_tiptap_nodes(&back, policy);
    let hash = combined_hash(&parsed.children, policy);
    let count = parsed.children.len();
    if count == own.children.len() && hash == own_hash {
        Image::Same
    } else {
        Image::Nodes { hash, count }
    }
}

/// Sentinel characters around an atom, so the export is not a lone atom (a paragraph that is only a hard break
/// loses it: markdown has no trailing hard break) and the atom's own units can be cut out of the result.
const LEAD: char = 'a';
const TRAIL: char = 'b';

/// The keyed units of the first block of `blocks` with the sentinels cut off (kept whole if they are gone).
fn first_block_units(blocks: &[Value], policy: &WritePolicy) -> Vec<UnitKey> {
    let fragment = desired_from_tiptap_nodes(blocks, policy);
    let Some(DesiredChild::Element(node)) = fragment.children.first() else {
        return Vec::new();
    };
    if node.children.is_empty() {
        return Vec::new();
    }
    let keys: Vec<UnitKey> = units_des(&node.children, 0..node.children.len(), policy)
        .into_iter()
        .map(|unit| unit.key)
        .collect();
    let framed = keys.len() >= 2
        && keys.first() == Some(&UnitKey::Char(LEAD))
        && keys.last() == Some(&UnitKey::Char(TRAIL));
    if framed {
        keys[1..keys.len() - 1].to_vec()
    } else {
        keys
    }
}

/// The image of one inline atom (export a paragraph holding the atom between two sentinel letters; read the
/// parsed paragraph's units).
pub(crate) fn inline_atom_image(atom: &Value, policy: &WritePolicy) -> AtomImage {
    if !node_expressible(SourceFormat::Markdown, node_type(atom)) {
        return AtomImage::Unexpressible;
    }
    let holder = json!({"type": "paragraph", "content": [
        {"type": "text", "text": LEAD.to_string()},
        atom.clone(),
        {"type": "text", "text": TRAIL.to_string()},
    ]});
    let own = first_block_units(std::slice::from_ref(&holder), policy);
    let Some(back) = round_trip(vec![holder]) else {
        return AtomImage::Units(Vec::new());
    };
    let units = first_block_units(&back, policy);
    if units == own {
        AtomImage::Same
    } else {
        AtomImage::Units(units)
    }
}
