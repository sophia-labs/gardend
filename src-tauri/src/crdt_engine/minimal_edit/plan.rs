//! The planner: aligns current and desired children (block id, then content hash or markdown image, then
//! position), recurses into matched nodes, and diffs inline content at character level. Pure.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::ops::Range;
use std::sync::Arc;

use serde_json::Value;

use super::seqdiff::{common_affixes, longest_increasing_pairs, myers_by, Edit};
use super::types::{
    AtomImage, CurrentChild, CurrentFragment, CurrentNode, DesiredChild, DesiredFragment,
    DesiredNode, FragmentPlan, Image, MarkSet, NodePatch, Pair, PlanSummary, RawMarks, Seg, Side,
    SourceFormat, Step, TextOp, TextRun, Unit, UnitKey, UnitOrigin, WritePolicy,
};
use super::{
    attrs, md_image, model, MARKDOWN_IMAGES, MAX_CHILDREN_EDIT_DISTANCE, MAX_INLINE_EDIT_DISTANCE,
    MAX_INLINE_UNITS,
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
        let segs = model::segments_cur(children);
        let inline_hashes = segs
            .iter()
            .map(|seg| match seg {
                Seg::Inline(range) => {
                    model::inline_hash(&model::units_cur(children, range.clone(), policy), policy)
                }
                Seg::Node(_) => 0,
            })
            .collect();
        CurSide {
            children,
            segs,
            policy,
            inline_hashes,
        }
    }

    fn node(&self, seg: usize) -> Option<&CurrentNode> {
        match &self.segs[seg] {
            Seg::Node(index) => match &self.children[*index] {
                CurrentChild::Element(node) => Some(node),
                CurrentChild::Text(_) => None,
            },
            Seg::Inline(_) => None,
        }
    }
}

impl Side for CurSide<'_> {
    fn len(&self) -> usize {
        self.segs.len()
    }
    fn tag(&self, seg: usize) -> &str {
        match self.node(seg) {
            Some(node) => node.tag.as_str(),
            None => "#inline",
        }
    }
    fn id(&self, seg: usize) -> Option<&str> {
        self.node(seg).and_then(|node| node.id.as_deref())
    }
    fn hash(&self, seg: usize) -> u64 {
        match self.node(seg) {
            Some(node) => node.hash,
            None => self.inline_hashes[seg],
        }
    }
    fn image(&self, seg: usize) -> Option<&super::types::Image> {
        if self.policy.format != SourceFormat::Markdown || !MARKDOWN_IMAGES {
            return None;
        }
        let node = self.node(seg)?;
        Some(
            node.image.get_or_init(|| {
                md_image::block_image(&model::current_node_json(node), self.policy)
            }),
        )
    }
    fn unexpressible(&self, seg: usize) -> bool {
        match self.node(seg) {
            Some(node) => !attrs::node_expressible(self.policy.format, &node.tag),
            None => false,
        }
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
        let segs = model::segments_des(children);
        let inline_hashes = segs
            .iter()
            .map(|seg| match seg {
                Seg::Inline(range) => {
                    model::inline_hash(&model::units_des(children, range.clone(), policy), policy)
                }
                Seg::Node(_) => 0,
            })
            .collect();
        DesSide {
            children,
            segs,
            policy,
            inline_hashes,
        }
    }

    fn node(&self, seg: usize) -> Option<&DesiredNode> {
        match &self.segs[seg] {
            Seg::Node(index) => match &self.children[*index] {
                DesiredChild::Element(node) => Some(node),
                DesiredChild::Text(_) => None,
            },
            Seg::Inline(_) => None,
        }
    }
}

impl Side for DesSide<'_> {
    fn len(&self) -> usize {
        self.segs.len()
    }
    fn tag(&self, seg: usize) -> &str {
        match self.node(seg) {
            Some(node) => node.tag.as_str(),
            None => "#inline",
        }
    }
    fn id(&self, seg: usize) -> Option<&str> {
        self.node(seg).and_then(|node| node.id.as_deref())
    }
    fn hash(&self, seg: usize) -> u64 {
        match self.node(seg) {
            Some(node) => node.hash,
            None => self.inline_hashes[seg],
        }
    }
    fn image(&self, _seg: usize) -> Option<&super::types::Image> {
        None
    }
    fn unexpressible(&self, _seg: usize) -> bool {
        false
    }
}

// ---------------------------------------------------------------------------------------------
// Alignment
// ---------------------------------------------------------------------------------------------

/// Under Bound (or for an id the parser did not mint) a present desired id must equal the current one.
fn id_ok(cur_id: Option<&str>, des_id: Option<&str>, policy: &WritePolicy) -> bool {
    match des_id {
        Some(id) if !policy.id_is_minted(id) => cur_id == Some(id),
        _ => true,
    }
}

#[derive(Clone, Copy)]
enum AnchorKind {
    Equal,
    Modified,
    Image(usize),
}

#[derive(Clone, Copy)]
struct Anchor {
    a: usize,
    b: usize,
    kind: AnchorKind,
}

/// Does the combined hash of `count` desired segments from `start` equal the image's hash? A single node's
/// image hash is its own hash; several are folded with FNV in order. Pinned definition (U7 must follow it):
/// hash = `Fnv64` over `write_u64(node_hash)` of each parsed node in order, or the node's own hash when
/// count == 1.
fn image_hash_matches(side: &dyn Side, start: usize, count: usize, want: u64) -> bool {
    if count == 1 && side.hash(start) == want {
        return true;
    }
    let mut folded = attrs::Fnv64::new();
    for seg in start..start + count {
        folded.write_u64(side.hash(seg));
    }
    folded.finish() == want
}

/// Align two segment lists (DESIGN §2.5). Pairs come in document order and cover every segment of both
/// sides exactly once (`EqualImage` covers `count` b segments).
pub(crate) fn align(a: &dyn Side, b: &dyn Side, policy: &WritePolicy) -> Vec<Pair> {
    let na = a.len();
    let nb = b.len();
    let equal = |i: usize, j: usize| -> bool {
        a.tag(i) == b.tag(j) && a.hash(i) == b.hash(j) && id_ok(a.id(i), b.id(j), policy)
    };

    // 1. Affixes: equal segments at both ends cost nothing.
    let all_a: Vec<usize> = (0..na).collect();
    let all_b: Vec<usize> = (0..nb).collect();
    let (prefix, suffix) = common_affixes(&all_a, &all_b, |i: &usize, j: &usize| equal(*i, *j));
    let a_lo = prefix;
    let a_hi = na - suffix;
    let b_lo = prefix;
    let b_hi = nb - suffix;

    let mut a_used = vec![false; na];
    let mut b_used = vec![false; nb];
    let mut anchors: Vec<Anchor> = Vec::new();

    // 2. Id anchors: a desired id (not minted) that occurs exactly once among the current middle segments.
    let mut by_id: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
    for i in a_lo..a_hi {
        if let Some(id) = a.id(i) {
            by_id.entry(id).or_default().push(i);
        }
    }
    for j in b_lo..b_hi {
        let Some(id) = b.id(j) else { continue };
        if policy.id_is_minted(id) {
            continue;
        }
        let Some(found) = by_id.get(id) else { continue };
        if found.len() != 1 {
            continue;
        }
        let i = found[0];
        if a_used[i] || b_used[j] {
            continue;
        }
        a_used[i] = true;
        b_used[j] = true;
        let kind = if equal(i, j) {
            AnchorKind::Equal
        } else {
            AnchorKind::Modified
        };
        anchors.push(Anchor { a: i, b: j, kind });
    }

    // 3. Content anchors: the k-th occurrence of a (tag, hash) on one side with the k-th on the other.
    let mut by_key: BTreeMap<(&str, u64), VecDeque<usize>> = BTreeMap::new();
    for i in a_lo..a_hi {
        if !a_used[i] {
            by_key
                .entry((a.tag(i), a.hash(i)))
                .or_default()
                .push_back(i);
        }
    }
    for j in b_lo..b_hi {
        if b_used[j] {
            continue;
        }
        let Some(queue) = by_key.get_mut(&(b.tag(j), b.hash(j))) else {
            continue;
        };
        let Some(i) = queue.pop_front() else { continue };
        a_used[i] = true;
        b_used[j] = true;
        let kind = if equal(i, j) {
            AnchorKind::Equal
        } else {
            AnchorKind::Modified
        };
        anchors.push(Anchor { a: i, b: j, kind });
    }

    // 3b. Markdown images: a current node whose markdown image equals the next desired segments.
    if policy.format == SourceFormat::Markdown && MARKDOWN_IMAGES {
        let mut search_from = b_lo;
        for i in a_lo..a_hi {
            if a_used[i] || a.unexpressible(i) || a.tag(i) == "#inline" {
                continue;
            }
            let (want, count) = match a.image(i) {
                Some(Image::Nodes { hash, count }) if *count > 0 => (*hash, *count),
                _ => continue,
            };
            let free_run = |j: usize| -> bool {
                j + count <= b_hi
                    && (j..j + count).all(|x| !b_used[x])
                    && image_hash_matches(b, j, count, want)
            };
            let found = (search_from..b_hi)
                .chain(b_lo..search_from)
                .find(|&j| free_run(j));
            if let Some(j) = found {
                for x in j..j + count {
                    b_used[x] = true;
                }
                a_used[i] = true;
                anchors.push(Anchor {
                    a: i,
                    b: j,
                    kind: AnchorKind::Image(count),
                });
                search_from = j + count;
            }
        }
    }

    // 4. Order: the longest chain increasing on both sides; the rest are moves (removed, re-inserted).
    let pairs: Vec<(usize, usize)> = anchors.iter().map(|anchor| (anchor.a, anchor.b)).collect();
    let chain = longest_increasing_pairs(&pairs);
    let by_a: BTreeMap<usize, Anchor> = anchors.iter().map(|anchor| (anchor.a, *anchor)).collect();

    let mut out: Vec<Pair> = Vec::with_capacity(na.max(nb));
    for k in 0..prefix {
        out.push(Pair::Equal(k, k));
    }
    let mut ai = a_lo;
    let mut bj = b_lo;
    for (i, _) in &chain {
        let Some(anchor) = by_a.get(i) else { continue };
        align_gap(a, b, ai..anchor.a, bj..anchor.b, &a_used, &b_used, &mut out);
        match anchor.kind {
            AnchorKind::Equal => {
                out.push(Pair::Equal(anchor.a, anchor.b));
                bj = anchor.b + 1;
            }
            AnchorKind::Modified => {
                out.push(Pair::Modified(anchor.a, anchor.b));
                bj = anchor.b + 1;
            }
            AnchorKind::Image(count) => {
                out.push(Pair::EqualImage {
                    a: anchor.a,
                    b: anchor.b,
                    count,
                });
                bj = anchor.b + count;
            }
        }
        ai = anchor.a + 1;
    }
    align_gap(a, b, ai..a_hi, bj..b_hi, &a_used, &b_used, &mut out);
    for k in 0..suffix {
        out.push(Pair::Equal(a_hi + k, b_hi + k));
    }
    out
}

/// Pair the segments between two anchors. Segments that were anchored but fell out of the chain (moves) never
/// pair again; equal tags pair positionally through Myers over tags; a current segment the format cannot
/// express and that nothing pairs with is kept.
fn align_gap(
    a: &dyn Side,
    b: &dyn Side,
    a_range: Range<usize>,
    b_range: Range<usize>,
    forced_a: &[bool],
    forced_b: &[bool],
    out: &mut Vec<Pair>,
) {
    let ai: Vec<usize> = a_range.collect();
    let bj: Vec<usize> = b_range.collect();
    if ai.is_empty() && bj.is_empty() {
        return;
    }
    let removed_or_kept = |i: usize, out: &mut Vec<Pair>| {
        let keep = !forced_a[i]
            && (a.unexpressible(i) || matches!(a.image(i), Some(Image::Unexpressible)));
        out.push(if keep {
            Pair::KeptUnstated(i)
        } else {
            Pair::Removed(i)
        });
    };
    let pairable = |i: usize, j: usize| -> bool {
        !forced_a[i]
            && !forced_b[j]
            && !a.unexpressible(i)
            && a.tag(i) == b.tag(j)
            && !matches!(a.image(i), Some(Image::Unexpressible))
    };
    let script = myers_by(
        &ai,
        &bj,
        |i: &usize, j: &usize| pairable(*i, *j),
        MAX_CHILDREN_EDIT_DISTANCE,
    );
    match script {
        Some(script) => {
            for edit in script {
                match edit {
                    Edit::Equal { a: x, b: y, len } => {
                        for t in 0..len {
                            out.push(Pair::Modified(ai[x + t], bj[y + t]));
                        }
                    }
                    Edit::Delete { a: x, len } => {
                        for t in 0..len {
                            removed_or_kept(ai[x + t], out);
                        }
                    }
                    Edit::Insert { b: y, len } => {
                        for t in 0..len {
                            out.push(Pair::Added(bj[y + t]));
                        }
                    }
                }
            }
        }
        None => {
            for &i in &ai {
                removed_or_kept(i, out);
            }
            for &j in &bj {
                out.push(Pair::Added(j));
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Fragment, children and node plans
// ---------------------------------------------------------------------------------------------

/// The whole plan for the `content` fragment.
pub(crate) fn plan_fragment(
    current: &CurrentFragment,
    desired: &DesiredFragment,
    policy: &WritePolicy,
) -> FragmentPlan {
    let steps = plan_children(&current.children, &desired.children, policy);
    let mut summary = PlanSummary::default();
    for step in &steps {
        match step {
            Step::Keep => summary.kept += 1,
            Step::Patch(_) | Step::Text(_) => summary.patched += 1,
            Step::InsertNode(_) | Step::InsertText(_) => summary.inserted += 1,
            Step::Remove(count) => summary.removed += *count,
        }
    }
    FragmentPlan { steps, summary }
}

fn seg_children(seg: &Seg) -> Range<usize> {
    match seg {
        Seg::Node(index) => *index..*index + 1,
        Seg::Inline(range) => range.clone(),
    }
}

/// Insert steps for the desired children in `range`.
fn insert_steps(desired: &[DesiredChild], range: Range<usize>) -> Vec<Step> {
    desired[range]
        .iter()
        .map(|child| match child {
            DesiredChild::Element(node) => Step::InsertNode(node.json.clone()),
            DesiredChild::Text(runs) => Step::InsertText(runs.clone()),
        })
        .collect()
}

/// Merge adjacent removals.
fn coalesce(steps: Vec<Step>) -> Vec<Step> {
    let mut out: Vec<Step> = Vec::with_capacity(steps.len());
    for step in steps {
        if let Step::Remove(more) = &step {
            if let Some(Step::Remove(count)) = out.last_mut() {
                *count += *more;
                continue;
            }
        }
        out.push(step);
    }
    out
}

/// The step stream for one parent's children.
pub(crate) fn plan_children(
    current: &[CurrentChild],
    desired: &[DesiredChild],
    policy: &WritePolicy,
) -> Vec<Step> {
    let cur_side = CurSide::new(current, policy);
    let des_side = DesSide::new(desired, policy);
    let pairs = align(&cur_side, &des_side, policy);
    let mut steps: Vec<Step> = Vec::new();
    for pair in pairs {
        match pair {
            Pair::Equal(a, _) | Pair::KeptUnstated(a) | Pair::EqualImage { a, .. } => {
                for _ in seg_children(&cur_side.segs[a]) {
                    steps.push(Step::Keep);
                }
            }
            Pair::Removed(a) => {
                let count = seg_children(&cur_side.segs[a]).len();
                steps.push(Step::Remove(count as u32));
            }
            Pair::Added(b) => {
                steps.extend(insert_steps(desired, seg_children(&des_side.segs[b])));
            }
            Pair::Modified(a, b) => {
                let seg_a = &cur_side.segs[a];
                let seg_b = &des_side.segs[b];
                match (seg_a, seg_b) {
                    (Seg::Node(i), Seg::Node(j)) => match (&current[*i], &desired[*j]) {
                        (CurrentChild::Element(cur), DesiredChild::Element(des))
                            if cur.tag == des.tag =>
                        {
                            match plan_node(cur, des, policy) {
                                Some(patch) => steps.push(Step::Patch(patch)),
                                None => steps.push(Step::Keep),
                            }
                        }
                        _ => {
                            // Y.XmlElement tags are immutable: replace.
                            steps.push(Step::Remove(1));
                            steps.extend(insert_steps(desired, *j..*j + 1));
                        }
                    },
                    (Seg::Inline(ra), Seg::Inline(rb)) => {
                        steps.extend(plan_inline(
                            &current[ra.clone()],
                            &desired[rb.clone()],
                            policy,
                        ));
                    }
                    _ => {
                        steps.push(Step::Remove(seg_children(seg_a).len() as u32));
                        steps.extend(insert_steps(desired, seg_children(seg_b)));
                    }
                }
            }
        }
    }
    coalesce(steps)
}

/// The patch of one matched element; `None` when nothing differs under the policy.
pub(crate) fn plan_node(
    current: &super::types::CurrentNode,
    desired: &super::types::DesiredNode,
    policy: &WritePolicy,
) -> Option<NodePatch> {
    let mut set: BTreeMap<String, Value> = BTreeMap::new();
    let mut remove: BTreeSet<String> = BTreeSet::new();

    let cur_sem = attrs::sem_node_attrs(&current.tag, &current.attrs);
    let des_sem = attrs::sem_node_attrs(&desired.tag, &desired.attrs);
    let keys: BTreeSet<&String> = current.attrs.keys().chain(desired.attrs.keys()).collect();
    for key in keys {
        if attrs::ID_ATTRS.contains(&key.as_str()) {
            continue;
        }
        let des_raw = desired.attrs.get(key).filter(|value| !value.is_null());
        let stated =
            des_raw.is_some() || attrs::node_attr_stated(policy.format, &desired.tag, key.as_str());
        if !stated {
            continue;
        }
        if cur_sem.get(key) == des_sem.get(key) {
            continue;
        }
        match des_raw {
            Some(value) => {
                set.insert(key.clone(), value.clone());
            }
            None => {
                let present = current
                    .attrs
                    .get(key)
                    .map(|value| !value.is_null())
                    .unwrap_or(false);
                if present {
                    remove.insert(key.clone());
                }
            }
        }
    }

    // Ids: a Bound desired id wins on a matched node; Adopt (minted) and absent ids keep the current one.
    if let Some(id) = desired.id.as_deref() {
        if !policy.id_is_minted(id) && current.id.as_deref() != Some(id) {
            set.insert("data-block-id".to_string(), Value::String(id.to_string()));
            if current.attrs.contains_key("blockId") {
                set.insert("blockId".to_string(), Value::String(id.to_string()));
            }
        }
    }

    let children = if current.children.is_empty() && desired.children.is_empty() {
        None
    } else {
        let steps = plan_children(&current.children, &desired.children, policy);
        if steps.iter().all(|step| matches!(step, Step::Keep)) {
            None
        } else {
            Some(steps)
        }
    };

    if set.is_empty() && remove.is_empty() && children.is_none() {
        return None;
    }
    Some(NodePatch {
        set: set.into_iter().collect(),
        remove: remove.into_iter().collect(),
        children,
    })
}

// ---------------------------------------------------------------------------------------------
// Inline content
// ---------------------------------------------------------------------------------------------

/// Marks equal as far as the format states them.
fn stated_marks_equal(a: &MarkSet, b: &MarkSet, format: SourceFormat) -> bool {
    a.iter()
        .filter(|(name, _)| attrs::mark_stated(format, name.as_str()))
        .eq(b
            .iter()
            .filter(|(name, _)| attrs::mark_stated(format, name.as_str())))
}

fn units_equal(cur: &[Unit], des: &[Unit], format: SourceFormat) -> bool {
    cur.len() == des.len()
        && cur
            .iter()
            .zip(des.iter())
            .all(|(c, d)| c.key == d.key && stated_marks_equal(&c.marks, &d.marks, format))
}

/// `(set, remove)` turning the current unit's marks into the desired unit's, for the marks the format states.
fn format_diff(
    cur: &Unit,
    des: &Unit,
    format: SourceFormat,
) -> (Vec<(String, Value)>, Vec<String>) {
    let mut set = Vec::new();
    let mut remove = Vec::new();
    for (name, raw) in des.raw_marks.iter() {
        if attrs::mark_stated(format, name) && cur.marks.get(name) != des.marks.get(name) {
            set.push((name.clone(), raw.clone()));
        }
    }
    for name in cur.raw_marks.keys() {
        if attrs::mark_stated(format, name) && !des.raw_marks.contains_key(name) {
            remove.push(name.clone());
        }
    }
    (set, remove)
}

/// The marks an equal unit has once its stated marks follow the desired unit (unstated ones stay).
fn merged_marks(cur: &Unit, des: &Unit, format: SourceFormat) -> RawMarks {
    let mut out = RawMarks::new();
    for (name, raw) in cur.raw_marks.iter() {
        if !attrs::mark_stated(format, name) {
            out.insert(name.clone(), raw.clone());
        }
    }
    for (name, raw) in des.raw_marks.iter() {
        if attrs::mark_stated(format, name) {
            out.insert(name.clone(), raw.clone());
        }
    }
    out
}

fn sem_of_raw(raw: &RawMarks) -> MarkSet {
    raw.iter()
        .map(|(name, value)| (name.clone(), attrs::sem_mark_attrs(name, Some(value))))
        .collect()
}

fn push_char_run(runs: &mut Vec<TextRun>, ch: char, raw: &RawMarks) {
    if let Some(last) = runs.last_mut() {
        if last.raw_marks == *raw {
            last.text.push(ch);
            return;
        }
    }
    runs.push(TextRun {
        text: ch.to_string(),
        marks: sem_of_raw(raw),
        raw_marks: raw.clone(),
    });
}

fn runs_of(units: &[(char, Arc<RawMarks>)]) -> Vec<TextRun> {
    let mut runs = Vec::new();
    for (ch, marks) in units {
        push_char_run(&mut runs, *ch, marks);
    }
    runs
}

fn push_retain(ops: &mut Vec<TextOp>, bytes: u32) {
    if let Some(TextOp::Retain(count)) = ops.last_mut() {
        *count += bytes;
        return;
    }
    ops.push(TextOp::Retain(bytes));
}

fn push_format(ops: &mut Vec<TextOp>, bytes: u32, set: Vec<(String, Value)>, remove: Vec<String>) {
    if let Some(TextOp::Format {
        bytes: count,
        set: last_set,
        remove: last_remove,
    }) = ops.last_mut()
    {
        if *last_set == set && *last_remove == remove {
            *count += bytes;
            return;
        }
    }
    ops.push(TextOp::Format { bytes, set, remove });
}

fn push_delete(ops: &mut Vec<TextOp>, bytes: u32) {
    if let Some(TextOp::Delete(count)) = ops.last_mut() {
        *count += bytes;
        return;
    }
    ops.push(TextOp::Delete(bytes));
}

fn push_insert_char(ops: &mut Vec<TextOp>, ch: char, marks: &Arc<RawMarks>) {
    if let Some(TextOp::Insert {
        text,
        marks: last_marks,
    }) = ops.last_mut()
    {
        if *last_marks == **marks {
            text.push(ch);
            return;
        }
    }
    ops.push(TextOp::Insert {
        text: ch.to_string(),
        marks: (**marks).clone(),
    });
}

/// One operation of the unit-level script, over indices into the current and desired unit lists.
#[derive(Clone, Copy, Debug)]
enum UOp {
    /// Current unit `.0` is desired unit `.1`.
    Eq(usize, usize),
    /// Current unit removed.
    Del(usize),
    /// Desired unit inserted.
    Ins(usize),
    /// Current unit the format cannot express and the desired content omits: stays.
    Kept(usize),
}

/// Replace a whole inline run: remove its current children, insert the desired ones.
fn replace_run(cur_len: usize, des: &[DesiredChild]) -> Vec<Step> {
    let mut steps = Vec::new();
    if cur_len > 0 {
        steps.push(Step::Remove(cur_len as u32));
    }
    steps.extend(insert_steps(des, 0..des.len()));
    steps
}

/// The step stream for one inline run: one step per current child plus inserts (DESIGN §2.5, plan_inline).
fn plan_inline(cur: &[CurrentChild], des: &[DesiredChild], policy: &WritePolicy) -> Vec<Step> {
    let format = policy.format;
    let cu = model::units_cur(cur, 0..cur.len(), policy);
    let du = model::units_des(des, 0..des.len(), policy);

    if units_equal(&cu, &du, format) {
        return vec![Step::Keep; cur.len()];
    }
    if cu.len() + du.len() > MAX_INLINE_UNITS {
        return replace_run(cur.len(), des);
    }

    // Keys for Myers. Under markdown a current atom that reads back as characters is expanded to them.
    let expand = format == SourceFormat::Markdown && MARKDOWN_IMAGES;
    let mut ckeys: Vec<UnitKey> = Vec::with_capacity(cu.len());
    let mut cmap: Vec<usize> = Vec::with_capacity(cu.len());
    let mut cimg: Vec<bool> = Vec::with_capacity(cu.len());
    let mut image_len: BTreeMap<usize, usize> = BTreeMap::new();
    let mut unexpressible: BTreeSet<usize> = BTreeSet::new();
    for (u, unit) in cu.iter().enumerate() {
        let mut expanded = false;
        if let (UnitKey::Atom(_), UnitOrigin::CurAtom { child }) = (&unit.key, unit.origin) {
            if let CurrentChild::Element(node) = &cur[child] {
                if !attrs::node_expressible(format, &node.tag) {
                    unexpressible.insert(u);
                } else if expand && node.tag != "hardBreak" {
                    match md_image::inline_atom_image(&model::current_node_json(node), policy) {
                        AtomImage::Units(keys) if !keys.is_empty() => {
                            image_len.insert(u, keys.len());
                            for key in keys {
                                ckeys.push(key);
                                cmap.push(u);
                                cimg.push(true);
                            }
                            expanded = true;
                        }
                        AtomImage::Unexpressible => {
                            unexpressible.insert(u);
                        }
                        _ => {}
                    }
                }
            }
        }
        if !expanded {
            ckeys.push(unit.key.clone());
            cmap.push(u);
            cimg.push(false);
        }
    }
    let dkeys: Vec<UnitKey> = du.iter().map(|unit| unit.key.clone()).collect();

    // Myers over the keys between the common affixes.
    let key_eq = |x: &UnitKey, y: &UnitKey| x == y;
    let (prefix, suffix) = common_affixes(&ckeys, &dkeys, key_eq);
    let mid_c = &ckeys[prefix..ckeys.len() - suffix];
    let mid_d = &dkeys[prefix..dkeys.len() - suffix];
    let Some(script) = myers_by(mid_c, mid_d, key_eq, MAX_INLINE_EDIT_DISTANCE) else {
        return replace_run(cur.len(), des);
    };

    #[derive(Clone, Copy)]
    enum EOp {
        Eq(usize, usize),
        Del(usize),
        Ins(usize),
    }
    let mut eops: Vec<EOp> = Vec::with_capacity(ckeys.len().max(dkeys.len()));
    for t in 0..prefix {
        eops.push(EOp::Eq(t, t));
    }
    for edit in &script {
        match *edit {
            Edit::Equal { a, b, len } => {
                for t in 0..len {
                    eops.push(EOp::Eq(prefix + a + t, prefix + b + t));
                }
            }
            Edit::Delete { a, len } => {
                for t in 0..len {
                    eops.push(EOp::Del(prefix + a + t));
                }
            }
            Edit::Insert { b, len } => {
                for t in 0..len {
                    eops.push(EOp::Ins(prefix + b + t));
                }
            }
        }
    }
    for t in 0..suffix {
        eops.push(EOp::Eq(ckeys.len() - suffix + t, dkeys.len() - suffix + t));
    }

    // An expanded atom stays only if all its image units are equal, contiguous and carry no marks.
    let mut span: BTreeMap<usize, (usize, usize, usize)> = BTreeMap::new(); // atom -> (first, last, eq count)
    let mut broken: BTreeSet<usize> = BTreeSet::new();
    for (position, op) in eops.iter().enumerate() {
        match *op {
            EOp::Eq(c, d) if cimg[c] => {
                let atom = cmap[c];
                let entry = span.entry(atom).or_insert((position, position, 0));
                entry.1 = position;
                entry.2 += 1;
                if !du[d].marks.is_empty() {
                    broken.insert(atom);
                }
            }
            EOp::Del(c) if cimg[c] => {
                broken.insert(cmap[c]);
            }
            _ => {}
        }
    }
    let atom_stays = |atom: usize| -> bool {
        let Some(len) = image_len.get(&atom) else {
            return false;
        };
        match span.get(&atom) {
            Some((first, last, count)) => {
                !broken.contains(&atom) && *count == *len && last - first + 1 == *len
            }
            None => false,
        }
    };

    let mut ops: Vec<UOp> = Vec::with_capacity(eops.len());
    let mut started: BTreeSet<usize> = BTreeSet::new();
    let push_del = |ops: &mut Vec<UOp>, u: usize| {
        if unexpressible.contains(&u) {
            ops.push(UOp::Kept(u));
        } else {
            ops.push(UOp::Del(u));
        }
    };
    for op in &eops {
        match *op {
            EOp::Eq(c, d) => {
                if cimg[c] {
                    let atom = cmap[c];
                    if atom_stays(atom) {
                        if started.insert(atom) {
                            ops.push(UOp::Eq(atom, d));
                        }
                    } else {
                        if started.insert(atom) {
                            push_del(&mut ops, atom);
                        }
                        ops.push(UOp::Ins(d));
                    }
                } else {
                    ops.push(UOp::Eq(cmap[c], d));
                }
            }
            EOp::Del(c) => {
                if cimg[c] {
                    let atom = cmap[c];
                    if started.insert(atom) {
                        push_del(&mut ops, atom);
                    }
                } else {
                    push_del(&mut ops, cmap[c]);
                }
            }
            EOp::Ins(d) => ops.push(UOp::Ins(d)),
        }
    }

    let mut scan = Scan::new(cur, des, &cu, &du, policy);
    scan.run(&ops);
    scan.finish()
}

enum Item {
    Text(Vec<(char, Arc<RawMarks>)>),
    Atom(usize),
}

/// Per current Y.XmlText child: the delta being built and what happened to it.
#[derive(Default)]
struct TextAcc {
    ops: Vec<TextOp>,
    byte_len: u32,
    /// Some unit stayed (equal).
    kept: bool,
    /// Some text was inserted into it.
    inserted: bool,
    /// Some unit was deleted.
    deleted: bool,
    /// An atom was inserted inside it: its tail was deleted and re-created as a new text after the atom.
    split: bool,
}

/// Walks the unit script and builds one step per current child plus the inserts between them.
struct Scan<'a> {
    cur: &'a [CurrentChild],
    des: &'a [DesiredChild],
    cu: &'a [Unit],
    du: &'a [Unit],
    policy: &'a WritePolicy,
    acc: Vec<TextAcc>,
    atom_step: Vec<Option<Step>>,
    /// `gaps[c]`: new children inserted before current child `c` (`gaps[len]`: after the last).
    gaps: Vec<Vec<Step>>,
}

impl<'a> Scan<'a> {
    fn new(
        cur: &'a [CurrentChild],
        des: &'a [DesiredChild],
        cu: &'a [Unit],
        du: &'a [Unit],
        policy: &'a WritePolicy,
    ) -> Self {
        let acc = cur
            .iter()
            .map(|child| TextAcc {
                byte_len: match child {
                    CurrentChild::Text(text) => text.byte_len,
                    CurrentChild::Element(_) => 0,
                },
                ..TextAcc::default()
            })
            .collect();
        Scan {
            cur,
            des,
            cu,
            du,
            policy,
            acc,
            atom_step: vec![None; cur.len()],
            gaps: vec![Vec::new(); cur.len() + 1],
        }
    }

    fn child_of(&self, unit: usize) -> usize {
        match self.cu[unit].origin {
            UnitOrigin::CurText { child, .. }
            | UnitOrigin::CurAtom { child }
            | UnitOrigin::CurAtomImage { child } => child,
            _ => 0,
        }
    }

    fn char_of(&self, unit: usize) -> Option<char> {
        match self.cu[unit].key {
            UnitKey::Char(ch) => Some(ch),
            UnitKey::Atom(_) => None,
        }
    }

    fn byte_of(&self, unit: usize) -> u32 {
        match self.cu[unit].origin {
            UnitOrigin::CurText { byte, .. } => byte,
            _ => 0,
        }
    }

    fn run(&mut self, ops: &[UOp]) {
        // next_cur[k]: the current unit of the first op at or after k that carries one.
        let mut next_cur: Vec<Option<usize>> = vec![None; ops.len() + 1];
        for k in (0..ops.len()).rev() {
            next_cur[k] = match ops[k] {
                UOp::Eq(c, _) | UOp::Del(c) | UOp::Kept(c) => Some(c),
                UOp::Ins(_) => next_cur[k + 1],
            };
        }
        let mut last: Option<usize> = None;
        let mut k = 0;
        while k < ops.len() {
            match ops[k] {
                UOp::Eq(c, d) => {
                    self.equal_unit(c, d);
                    last = Some(c);
                    k += 1;
                }
                UOp::Kept(c) => {
                    last = Some(c);
                    k += 1;
                }
                UOp::Del(_) | UOp::Ins(_) => {
                    let mut dels: Vec<usize> = Vec::new();
                    let mut inss: Vec<usize> = Vec::new();
                    let mut end = k;
                    while end < ops.len() {
                        match ops[end] {
                            UOp::Del(c) => dels.push(c),
                            UOp::Ins(d) => inss.push(d),
                            _ => break,
                        }
                        end += 1;
                    }
                    let left = self.group(dels, inss, last, next_cur[end]);
                    if left.is_some() {
                        last = left;
                    }
                    k = end;
                }
            }
        }
    }

    fn equal_unit(&mut self, c: usize, d: usize) {
        let Some(ch) = self.char_of(c) else {
            return; // an atom that stays: Keep
        };
        let child = self.child_of(c);
        if self.acc[child].split {
            let raw = merged_marks(&self.cu[c], &self.du[d], self.policy.format);
            self.tail_push(child, &[(ch, Arc::new(raw))]);
            return;
        }
        let bytes = ch.len_utf8() as u32;
        self.acc[child].kept = true;
        let (set, remove) = format_diff(&self.cu[c], &self.du[d], self.policy.format);
        if set.is_empty() && remove.is_empty() {
            push_retain(&mut self.acc[child].ops, bytes);
        } else {
            push_format(&mut self.acc[child].ops, bytes, set, remove);
        }
    }

    fn delete_unit(&mut self, c: usize) {
        let child = self.child_of(c);
        match self.char_of(c) {
            Some(ch) => {
                if !self.acc[child].split {
                    push_delete(&mut self.acc[child].ops, ch.len_utf8() as u32);
                    self.acc[child].deleted = true;
                }
            }
            None => self.atom_step[child] = Some(Step::Remove(1)),
        }
    }

    /// Append characters to the new text that follows split child `child` (the last piece of its gap).
    fn tail_push(&mut self, child: usize, units: &[(char, Arc<RawMarks>)]) {
        let gap = &mut self.gaps[child + 1];
        if !matches!(gap.last(), Some(Step::InsertText(_))) {
            gap.push(Step::InsertText(Vec::new()));
        }
        if let Some(Step::InsertText(runs)) = gap.last_mut() {
            for (ch, marks) in units {
                push_char_run(runs, *ch, marks);
            }
        }
    }

    /// Insert after the last processed unit of text child `child`.
    fn attach_left(&mut self, child: usize, units: &[(char, Arc<RawMarks>)]) {
        if self.acc[child].split {
            self.tail_push(child, units);
            return;
        }
        self.acc[child].inserted = true;
        for (ch, marks) in units {
            push_insert_char(&mut self.acc[child].ops, *ch, marks);
        }
    }

    /// Insert at offset 0 of text child `child`, none of whose units has been processed yet.
    fn attach_front(&mut self, child: usize, units: &[(char, Arc<RawMarks>)]) {
        self.acc[child].inserted = true;
        for (ch, marks) in units {
            push_insert_char(&mut self.acc[child].ops, *ch, marks);
        }
    }

    /// An atom goes inside text child `child` before its unit `right`: delete the tail of the child (once);
    /// the tail's equal units are re-created after the atom.
    fn ensure_split(&mut self, child: usize, right: usize) {
        if self.acc[child].split {
            return;
        }
        let offset = self.byte_of(right);
        let total = self.acc[child].byte_len;
        push_delete(&mut self.acc[child].ops, total.saturating_sub(offset));
        self.acc[child].split = true;
        self.acc[child].deleted = true;
    }

    fn inherited(&self, left: Option<usize>, right: Option<usize>) -> Vec<(String, Value)> {
        let (Some(l), Some(r)) = (left, right) else {
            return Vec::new();
        };
        let (lu, ru) = (&self.cu[l], &self.cu[r]);
        let mut out = Vec::new();
        for (name, raw) in lu.raw_marks.iter() {
            if attrs::mark_stated(self.policy.format, name) {
                continue;
            }
            if ru.raw_marks.contains_key(name) && lu.marks.get(name) == ru.marks.get(name) {
                out.push((name.clone(), raw.clone()));
            }
        }
        out
    }

    fn insert_marks(&self, d: usize, inherited: &[(String, Value)]) -> Arc<RawMarks> {
        let base = &self.du[d].raw_marks;
        if inherited.iter().all(|(name, _)| base.contains_key(name)) {
            return base.clone();
        }
        let mut marks: RawMarks = (**base).clone();
        for (name, value) in inherited {
            marks.entry(name.clone()).or_insert_with(|| value.clone());
        }
        Arc::new(marks)
    }

    fn atom_json(&self, des_child: usize) -> Step {
        match &self.des[des_child] {
            DesiredChild::Element(node) => Step::InsertNode(node.json.clone()),
            DesiredChild::Text(runs) => Step::InsertText(runs.clone()),
        }
    }

    /// One run of deletes and inserts between two equal stretches. Returns the last current unit it consumed.
    fn group(
        &mut self,
        mut dels: Vec<usize>,
        mut inss: Vec<usize>,
        prev: Option<usize>,
        right: Option<usize>,
    ) -> Option<usize> {
        let consumed = dels.last().copied();

        // A lone deleted atom met by an inserted atom of the same tag becomes an attribute patch.
        if dels.len() == 1 && self.char_of(dels[0]).is_none() {
            if let Some(&first) = inss.first() {
                let cur_child = self.child_of(dels[0]);
                let des_child = match self.du[first].origin {
                    UnitOrigin::DesAtom { child } => Some(child),
                    _ => None,
                };
                if let (Some(des_child), CurrentChild::Element(cur_node)) =
                    (des_child, &self.cur[cur_child])
                {
                    if let DesiredChild::Element(des_node) = &self.des[des_child] {
                        if cur_node.tag == des_node.tag {
                            self.atom_step[cur_child] =
                                plan_node(cur_node, des_node, self.policy).map(Step::Patch);
                            dels.clear();
                            inss.remove(0);
                        }
                    }
                }
            }
        }
        for &c in &dels {
            self.delete_unit(c);
        }
        if inss.is_empty() {
            return consumed;
        }

        let left = consumed.or(prev);
        let left_text = left
            .filter(|&u| self.char_of(u).is_some())
            .map(|u| self.child_of(u));
        let right_text = right
            .filter(|&u| self.char_of(u).is_some())
            .map(|u| self.child_of(u));
        let gap = left.map(|u| self.child_of(u) + 1).unwrap_or(0);

        let has_atom = inss
            .iter()
            .any(|&d| matches!(self.du[d].key, UnitKey::Atom(_)));
        if !has_atom {
            let inherited = self.inherited(left, right);
            let units: Vec<(char, Arc<RawMarks>)> = inss
                .iter()
                .filter_map(|&d| match self.du[d].key {
                    UnitKey::Char(ch) => Some((ch, self.insert_marks(d, &inherited))),
                    UnitKey::Atom(_) => None,
                })
                .collect();
            if let Some(child) = left_text {
                self.attach_left(child, &units);
            } else if let Some(child) = right_text {
                self.attach_front(child, &units);
            } else {
                self.gaps[gap].push(Step::InsertText(runs_of(&units)));
            }
            return consumed;
        }

        // Items: runs of text and atoms in desired order.
        let mut items: Vec<Item> = Vec::new();
        for &d in &inss {
            match self.du[d].key {
                UnitKey::Char(ch) => {
                    let unit = (ch, self.du[d].raw_marks.clone());
                    match items.last_mut() {
                        Some(Item::Text(units)) => units.push(unit),
                        _ => items.push(Item::Text(vec![unit])),
                    }
                }
                UnitKey::Atom(_) => {
                    if let UnitOrigin::DesAtom { child } = self.du[d].origin {
                        items.push(Item::Atom(child));
                    }
                }
            }
        }
        let first_atom = items.iter().position(|item| matches!(item, Item::Atom(_)));
        let last_atom = items.iter().rposition(|item| matches!(item, Item::Atom(_)));
        let (Some(first_atom), Some(last_atom)) = (first_atom, last_atom) else {
            return consumed;
        };
        let interior_child = match (left_text, right_text) {
            (Some(l), Some(r)) if l == r => Some(l),
            _ => None,
        };
        for (index, item) in items.into_iter().enumerate() {
            match item {
                Item::Text(units) => {
                    if index < first_atom {
                        match left_text {
                            Some(child) => self.attach_left(child, &units),
                            None => self.gaps[gap].push(Step::InsertText(runs_of(&units))),
                        }
                    } else if index > last_atom {
                        if let Some(child) = interior_child {
                            self.tail_push(child, &units);
                        } else if let Some(child) = right_text {
                            self.attach_front(child, &units);
                        } else {
                            self.gaps[gap].push(Step::InsertText(runs_of(&units)));
                        }
                    } else {
                        self.gaps[gap].push(Step::InsertText(runs_of(&units)));
                    }
                }
                Item::Atom(des_child) => {
                    if let (Some(child), Some(r)) = (interior_child, right) {
                        self.ensure_split(child, r);
                    }
                    let step = self.atom_json(des_child);
                    self.gaps[gap].push(step);
                    if interior_child.is_some() && index == last_atom {
                        // The text that was cut off continues here.
                        self.gaps[gap].push(Step::InsertText(Vec::new()));
                    }
                }
            }
        }
        consumed
    }

    fn finish(mut self) -> Vec<Step> {
        fn live(step: &Step) -> bool {
            !matches!(step, Step::InsertText(runs) if runs.is_empty())
        }
        let mut steps: Vec<Step> = Vec::new();
        for child in 0..self.cur.len() {
            let gap = std::mem::take(&mut self.gaps[child]);
            steps.extend(gap.into_iter().filter(live));
            match &self.cur[child] {
                CurrentChild::Element(_) => {
                    steps.push(self.atom_step[child].take().unwrap_or(Step::Keep));
                }
                CurrentChild::Text(_) => {
                    let acc = &mut self.acc[child];
                    while matches!(acc.ops.last(), Some(TextOp::Retain(_))) {
                        acc.ops.pop();
                    }
                    if acc.ops.is_empty() {
                        steps.push(Step::Keep);
                    } else if !acc.kept && !acc.inserted && acc.deleted {
                        steps.push(Step::Remove(1));
                    } else {
                        steps.push(Step::Text(std::mem::take(&mut acc.ops)));
                    }
                }
            }
        }
        let gap = std::mem::take(&mut self.gaps[self.cur.len()]);
        steps.extend(gap.into_iter().filter(live));
        steps
    }
}
