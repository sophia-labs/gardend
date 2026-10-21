//! The planner: aligns current and desired children (block id, then content hash or markdown image, then
//! position), recurses into matched nodes, and diffs inline content at character level. Pure.

use super::types::{
    CurrentChild, CurrentFragment, DesiredChild, DesiredFragment, FragmentPlan, NodePatch, Pair,
    Seg, Side, Step, WritePolicy,
};

/// The current side of an alignment over a children list.
pub(crate) struct CurSide<'a> {
    pub(crate) children: &'a [CurrentChild],
    pub(crate) segs: Vec<Seg>,
    pub(crate) policy: &'a WritePolicy,
    pub(crate) inline_hashes: Vec<u64>,
}

impl<'a> CurSide<'a> {
    pub(crate) fn new(children: &'a [CurrentChild], policy: &'a WritePolicy) -> Self {
        unimplemented!("U4")
    }
}

impl Side for CurSide<'_> {
    fn len(&self) -> usize {
        unimplemented!("U4")
    }
    fn tag(&self, seg: usize) -> &str {
        unimplemented!("U4")
    }
    fn id(&self, seg: usize) -> Option<&str> {
        unimplemented!("U4")
    }
    fn hash(&self, seg: usize) -> u64 {
        unimplemented!("U4")
    }
    fn image(&self, seg: usize) -> Option<&super::types::Image> {
        unimplemented!("U4")
    }
    fn unexpressible(&self, seg: usize) -> bool {
        unimplemented!("U4")
    }
}

/// The desired (or base) side.
pub(crate) struct DesSide<'a> {
    pub(crate) children: &'a [DesiredChild],
    pub(crate) segs: Vec<Seg>,
    pub(crate) policy: &'a WritePolicy,
    pub(crate) inline_hashes: Vec<u64>,
}

impl<'a> DesSide<'a> {
    pub(crate) fn new(children: &'a [DesiredChild], policy: &'a WritePolicy) -> Self {
        unimplemented!("U4")
    }
}

impl Side for DesSide<'_> {
    fn len(&self) -> usize {
        unimplemented!("U4")
    }
    fn tag(&self, seg: usize) -> &str {
        unimplemented!("U4")
    }
    fn id(&self, seg: usize) -> Option<&str> {
        unimplemented!("U4")
    }
    fn hash(&self, seg: usize) -> u64 {
        unimplemented!("U4")
    }
    fn image(&self, _seg: usize) -> Option<&super::types::Image> {
        unimplemented!("U4")
    }
    fn unexpressible(&self, _seg: usize) -> bool {
        unimplemented!("U4")
    }
}

/// Align two segment lists (DESIGN §2.5). Pairs come in document order and cover every segment of both
/// sides exactly once (`EqualImage` covers `count` b segments).
pub(crate) fn align(a: &dyn Side, b: &dyn Side, policy: &WritePolicy) -> Vec<Pair> {
    unimplemented!("U4")
}

/// The whole plan for the `content` fragment.
pub(crate) fn plan_fragment(
    current: &CurrentFragment,
    desired: &DesiredFragment,
    policy: &WritePolicy,
) -> FragmentPlan {
    unimplemented!("U4")
}

/// The step stream for one parent's children.
pub(crate) fn plan_children(
    current: &[CurrentChild],
    desired: &[DesiredChild],
    policy: &WritePolicy,
) -> Vec<Step> {
    unimplemented!("U4")
}

/// The patch of one matched element; `None` when nothing differs under the policy.
pub(crate) fn plan_node(
    current: &super::types::CurrentNode,
    desired: &super::types::DesiredNode,
    policy: &WritePolicy,
) -> Option<NodePatch> {
    unimplemented!("U4")
}
