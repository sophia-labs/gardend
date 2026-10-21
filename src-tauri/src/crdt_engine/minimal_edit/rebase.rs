//! Three-way rebase of an `expectedRevision` write onto live edits the agent did not see (DESIGN §2.8).
//!
//! The agent read the record at its revision (the base) and sent the document it wants. The room may hold
//! more: a person typed after that read. Sending the desired document two-way would revert them. So per
//! top-level block: what the agent left alone follows the live document, what the agent changed applies when
//! nobody else touched it, and where both changed the person wins (Vera 2026-10-21, Q2) and the block is
//! reported so the agent can re-read and redo it. Pure: nothing here touches a Doc.

use std::cell::OnceCell;
use std::collections::BTreeMap;

use super::attrs::node_expressible;
use super::model::current_child_to_desired;
use super::plan::{align, DesSide};
use super::types::{
    CurrentChild, CurrentFragment, DesiredChild, DesiredFragment, Image, LiveEdits, Pair, Seg,
    Side, SourceFormat, WritePolicy,
};
use super::{md_image, MARKDOWN_IMAGES};

/// The base side of the base<->desired alignment: a `DesSide` that also knows the markdown image of its
/// nodes, so a markdown write that sends back "> ⧉ Class — Title <iri>" for a base objectEmbed matches it.
struct BaseSide<'a> {
    inner: DesSide<'a>,
    policy: &'a WritePolicy,
    images: Vec<OnceCell<Image>>,
}

impl<'a> BaseSide<'a> {
    fn new(children: &'a [DesiredChild], policy: &'a WritePolicy) -> Self {
        let inner = DesSide::new(children, policy);
        let images = (0..inner.segs.len()).map(|_| OnceCell::new()).collect();
        BaseSide {
            inner,
            policy,
            images,
        }
    }

    fn node(&self, seg: usize) -> Option<&'a super::types::DesiredNode> {
        match self.inner.segs.get(seg) {
            Some(Seg::Node(index)) => match self.inner.children.get(*index) {
                Some(DesiredChild::Element(node)) => Some(node),
                _ => None,
            },
            _ => None,
        }
    }
}

impl Side for BaseSide<'_> {
    fn len(&self) -> usize {
        self.inner.len()
    }
    fn tag(&self, seg: usize) -> &str {
        self.inner.tag(seg)
    }
    fn id(&self, seg: usize) -> Option<&str> {
        self.inner.id(seg)
    }
    fn hash(&self, seg: usize) -> u64 {
        self.inner.hash(seg)
    }
    fn image(&self, seg: usize) -> Option<&Image> {
        if !MARKDOWN_IMAGES || self.policy.format != SourceFormat::Markdown {
            return None;
        }
        let node = self.node(seg)?;
        Some(
            self.images[seg].get_or_init(|| md_image::block_image(&node.json, self.policy)),
        )
    }
    fn unexpressible(&self, seg: usize) -> bool {
        // A base node the write's format cannot say, absent from the desired content, was not removed by
        // the agent: it could not have been sent back.
        self.node(seg)
            .is_some_and(|node| !node_expressible(self.policy.format, &node.tag))
    }
}

/// Where each top-level child of `base` lives in `current` (same lineage: block id; an id-less base block
/// falls back to the first unclaimed current block with the same tag and hash).
fn lineage(base: &[DesiredChild], current: &[CurrentChild]) -> (Vec<Option<usize>>, Vec<bool>) {
    let mut by_id: BTreeMap<&str, usize> = BTreeMap::new();
    for (index, child) in current.iter().enumerate() {
        if let CurrentChild::Element(node) = child {
            if let Some(id) = node.id.as_deref() {
                by_id.entry(id).or_insert(index);
            }
        }
    }
    let mut claimed = vec![false; current.len()];
    let mut found: Vec<Option<usize>> = vec![None; base.len()];
    for (index, child) in base.iter().enumerate() {
        if let DesiredChild::Element(node) = child {
            if let Some(&at) = node.id.as_deref().and_then(|id| by_id.get(id)) {
                if !claimed[at] {
                    claimed[at] = true;
                    found[index] = Some(at);
                }
            }
        }
    }
    for (index, child) in base.iter().enumerate() {
        let DesiredChild::Element(node) = child else {
            continue;
        };
        if found[index].is_some() || node.id.is_some() {
            continue;
        }
        let twin = current.iter().enumerate().position(|(at, other)| {
            !claimed[at]
                && matches!(other, CurrentChild::Element(cur)
                    if cur.id.is_none() && cur.tag == node.tag && cur.hash == node.hash)
        });
        if let Some(at) = twin {
            claimed[at] = true;
            found[index] = Some(at);
        }
    }
    (found, claimed)
}

/// The children of `desired` that segment `seg` covers.
fn seg_children<'a>(segs: &[Seg], children: &'a [DesiredChild], seg: usize) -> &'a [DesiredChild] {
    match segs.get(seg) {
        Some(Seg::Node(index)) => children.get(*index..*index + 1).unwrap_or(&[]),
        Some(Seg::Inline(range)) => children.get(range.clone()).unwrap_or(&[]),
        None => &[],
    }
}

/// `base`: the record's `tiptapJson` at the expected revision (what `read_document` served). Per top-level
/// block: agent left it as in base -> take current (the person's edit survives); current still equals base
/// -> take desired; both changed -> take current (the person wins, Q2) and name the block in `conflicts`.
pub(crate) fn rebase_on_live(
    base: &DesiredFragment,
    current: &CurrentFragment,
    desired: DesiredFragment,
    policy: &WritePolicy,
) -> (DesiredFragment, LiveEdits) {
    let base_side = BaseSide::new(&base.children, policy);
    let des_side = DesSide::new(&desired.children, policy);
    let pairs = align(&base_side, &des_side, policy);
    let (found, mut claimed) = lineage(&base.children, &current.children);

    let mut out: Vec<(Option<usize>, DesiredChild)> = Vec::new();
    let mut edits = LiveEdits::default();

    // The state of base segment `a` in the live document: its base node, its current index, and whether the
    // person changed it since the base.
    struct Live {
        id: Option<String>,
        at: Option<usize>,
        changed: bool,
    }
    let live_of = |a: usize| -> Option<Live> {
        let Some(Seg::Node(index)) = base_side.inner.segs.get(a) else {
            return None;
        };
        let DesiredChild::Element(node) = base.children.get(*index)? else {
            return None;
        };
        let at = found[*index];
        let changed = match at.map(|i| &current.children[i]) {
            Some(CurrentChild::Element(cur)) => cur.hash != node.hash,
            _ => true,
        };
        Some(Live {
            id: node.id.clone(),
            at,
            changed,
        })
    };
    let push_current = |out: &mut Vec<(Option<usize>, DesiredChild)>, at: usize| {
        out.push((
            Some(at),
            current_child_to_desired(&current.children[at], policy),
        ));
    };
    let note = |list: &mut Vec<String>, id: &Option<String>| {
        if let Some(id) = id {
            list.push(id.clone());
        }
    };
    // The agent's own children for desired segments `b..b + count`, tagged with the live block they replace.
    let push_desired = |out: &mut Vec<(Option<usize>, DesiredChild)>,
                        b: usize,
                        count: usize,
                        at: Option<usize>| {
        for seg in b..b + count {
            for child in seg_children(&des_side.segs, &desired.children, seg) {
                out.push((at, child.clone()));
            }
        }
    };

    for pair in &pairs {
        match pair {
            // The agent left the block as it read it: the live document decides.
            Pair::Equal(a, b) | Pair::EqualImage { a, b, .. } => {
                let count = match pair {
                    Pair::EqualImage { count, .. } => *count,
                    _ => 1,
                };
                match live_of(*a) {
                    Some(live) => match live.at {
                        Some(at) => {
                            push_current(&mut out, at);
                            if live.changed {
                                note(&mut edits.kept, &live.id);
                            }
                        }
                        // The person deleted it: their deletion stands.
                        None => note(&mut edits.kept, &live.id),
                    },
                    // A top-level text run: no lineage to rebase, the agent's copy stands.
                    None => push_desired(&mut out, *b, count, None),
                }
            }
            // Omitted because the format cannot say it: the live block stays, as it is.
            Pair::KeptUnstated(a) => {
                if let Some(live) = live_of(*a) {
                    match live.at {
                        Some(at) => {
                            push_current(&mut out, at);
                            if live.changed {
                                note(&mut edits.kept, &live.id);
                            }
                        }
                        None => note(&mut edits.kept, &live.id),
                    }
                }
            }
            Pair::Modified(a, b) => match live_of(*a) {
                Some(live) => match (live.at, live.changed) {
                    (Some(at), true) => {
                        push_current(&mut out, at);
                        note(&mut edits.conflicts, &live.id);
                    }
                    (Some(at), false) => push_desired(&mut out, *b, 1, Some(at)),
                    // The person deleted what the agent changed: the deletion stands, the agent redoes it.
                    (None, _) => note(&mut edits.conflicts, &live.id),
                },
                None => push_desired(&mut out, *b, 1, None),
            },
            Pair::Removed(a) => {
                if let Some(live) = live_of(*a) {
                    if let (Some(at), true) = (live.at, live.changed) {
                        push_current(&mut out, at);
                        note(&mut edits.conflicts, &live.id);
                    }
                }
            }
            Pair::Added(b) => push_desired(&mut out, *b, 1, None),
        }
    }

    // Blocks the person added live have no base counterpart: they stay, after the nearest earlier live
    // block that is still in the result.
    for (index, child) in current.children.iter().enumerate() {
        if claimed[index] {
            continue;
        }
        claimed[index] = true;
        let CurrentChild::Element(node) = child else {
            continue;
        };
        let anchor = out
            .iter()
            .enumerate()
            .filter_map(|(position, (at, _))| (*at).filter(|at| *at < index).map(|at| (at, position)))
            .max();
        let position = anchor.map_or(0, |(_, position)| position + 1);
        out.insert(
            position,
            (
                Some(index),
                current_child_to_desired(child, policy),
            ),
        );
        note(&mut edits.kept, &node.id);
    }

    let children = out.into_iter().map(|(_, child)| child).collect();
    (DesiredFragment { children }, edits)
}
