//! The two models the planner compares: the desired fragment (from TipTap JSON) and the current fragment
//! (read from the room's Y.Doc inside the write's transaction). Hashing and inline units live here.

use std::ops::Range;

use serde_json::{Map, Value};
use yrs::{ReadTxn, XmlFragment, XmlFragmentRef};

use super::types::{
    CurrentChild, CurrentFragment, CurrentNode, DesiredChild, DesiredFragment, Seg, Unit,
    WritePolicy,
};

/// Inline node tags: children of a textblock that are Y.XmlElements beside Y.XmlText (projection.rs:57
/// plus the contract's other inline atoms): hardBreak, mathInline, footnote, wikilink, citation, tagChip.
pub(crate) fn is_inline_tag(tag: &str) -> bool {
    unimplemented!("U2")
}

/// `data-block-id`, else `blockId`; non-empty strings only (block_ops.rs:523-533).
pub(crate) fn block_id(attrs: &Map<String, Value>) -> Option<String> {
    unimplemented!("U2")
}

/// Read the whole `content` fragment. Never mutates. O(elements + chars).
pub(crate) fn read_fragment<T: ReadTxn>(
    txn: &T,
    fragment: &XmlFragmentRef,
    policy: &WritePolicy,
) -> CurrentFragment {
    unimplemented!("U2")
}

/// Read one parent's children (the fragment or an element).
pub(crate) fn read_children<T: ReadTxn, P: XmlFragment>(
    txn: &T,
    parent: &P,
    policy: &WritePolicy,
) -> Vec<CurrentChild> {
    unimplemented!("U2")
}

/// Desired model of a TipTap `content` array. Consecutive text nodes become one `DesiredChild::Text`;
/// empty text nodes are dropped (builder.rs:42-44 skips them too).
pub(crate) fn desired_from_tiptap_nodes(nodes: &[Value], policy: &WritePolicy) -> DesiredFragment {
    unimplemented!("U2")
}

/// The TipTap JSON of a current node (tag, raw attrs, children; each text run one text node with its raw
/// marks). Equal to what `projection::ydoc_to_tiptap_json` would give for that node.
pub(crate) fn current_node_json(node: &CurrentNode) -> Value {
    unimplemented!("U2")
}

/// A current child as a desired child with the same content (used by the rebase to keep a live edit).
pub(crate) fn current_child_to_desired(child: &CurrentChild, policy: &WritePolicy) -> DesiredChild {
    unimplemented!("U2")
}

/// Segment a current children list: each block element is a Node; each maximal run of Text children and
/// inline-tag elements is one Inline segment.
pub(crate) fn segments_cur(children: &[CurrentChild]) -> Vec<Seg> {
    unimplemented!("U2")
}

pub(crate) fn segments_des(children: &[DesiredChild]) -> Vec<Seg> {
    unimplemented!("U2")
}

/// Units of current children `range` (an Inline segment): every char of every Y.XmlText (CurText origin,
/// byte offsets), every inline atom one CurAtom unit. Atoms are not expanded to images here.
pub(crate) fn units_cur(
    children: &[CurrentChild],
    range: Range<usize>,
    policy: &WritePolicy,
) -> Vec<Unit> {
    unimplemented!("U2")
}

pub(crate) fn units_des(
    children: &[DesiredChild],
    range: Range<usize>,
    policy: &WritePolicy,
) -> Vec<Unit> {
    unimplemented!("U2")
}

/// Hash of an inline unit sequence under the policy: keys plus stated marks. Equal for equal content
/// whatever the Y.XmlText segmentation (DESIGN I1 on builder-shaped documents).
pub(crate) fn inline_hash(units: &[Unit], policy: &WritePolicy) -> u64 {
    unimplemented!("U2")
}
