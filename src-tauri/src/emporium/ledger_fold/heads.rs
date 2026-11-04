//! The head table: per (subject, predicate) the winning write, folded incrementally.
//!
//! A *head* is what the ledger says about one (subject, predicate). Two layers share the key
//! space, told apart by `HeadKey::set`:
//!
//! * the **single** layer (`Merge::Lww`, `Merge::Contested`): one write wins, the highest
//!   [`Stamp`]. A clear (`set` with no value) is a write like any other and is kept as a head
//!   with `value: None`, so a late lower-stamped write cannot resurrect what a later clear
//!   removed;
//! * the **set** layer (`Merge::AddWins`): values with tags, minus what removals observed.
//!
//! Order independence is the design constraint (property 1): every rule reads only the stamps of
//! events, never their arrival order. Hence the tombstone state kept beside the visible values:
//! the kills of a removal are recorded even when its target has not arrived yet, and a subject's
//! retraction is a stamp threshold, not an erasure.
//!
//! Assumptions the rules rely on, which the ledger gives: stamps are totally ordered (HLC, then
//! origin id, then seq, then op index); within one origin the HLC does not go back as seq grows;
//! a parent frame has a smaller stamp than its child.

use std::collections::btree_map::Entry as MapEntry;
use std::collections::{BTreeMap, BTreeSet};

use oxigraph::model::NamedNode;

use super::policy::{FoldPolicy, Merge};
use super::{Delta, FoldEvent, FrameId, Hlc, Op, RDF_TYPE};
use crate::emporium::terms::{canon, canon_value, CanonValue, Term, Triple};

/// Total order of writes: (HLC, origin id, seq, index of the op inside the frame).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Stamp {
    pub hlc: Hlc,
    pub origin: String,
    pub seq: u64,
    pub idx: u32,
}

impl Stamp {
    /// Position inside the origin's own stream.
    fn pos(&self) -> (u64, u32) {
        (self.seq, self.idx)
    }
}

/// One head's address: the subject, the predicate, and which layer.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct HeadKey {
    pub subject: String,
    pub predicate: String,
    /// True for the set layer (`Merge::AddWins`).
    pub set: bool,
}

impl HeadKey {
    fn lower_bound(subject: &str) -> HeadKey {
        HeadKey {
            subject: subject.to_string(),
            predicate: String::new(),
            set: false,
        }
    }
}

/// Removals recorded against tags, kept even before the tags arrive.
///
/// A tag is killed when a removal named its frame as a parent, or when a removal of the same
/// origin sits later in that origin's stream.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Kills {
    /// Parent-named frames, with the greatest stamp of a removal that named them.
    frames: BTreeMap<FrameId, Stamp>,
    /// Per origin, the removal latest in that origin's stream.
    origins: BTreeMap<String, Stamp>,
}

impl Kills {
    fn kills(&self, tag: &Stamp, frame: &str) -> bool {
        self.frames.contains_key(frame)
            || self
                .origins
                .get(&tag.origin)
                .map_or(false, |k| k.pos() > tag.pos())
    }

    fn add(&mut self, parents: &[FrameId], by: &Stamp) {
        for p in parents {
            match self.frames.get_mut(p) {
                Some(s) => {
                    if *by > *s {
                        *s = by.clone();
                    }
                }
                None => {
                    self.frames.insert(p.clone(), by.clone());
                }
            }
        }
        match self.origins.get_mut(&by.origin) {
            Some(k) => {
                if by.pos() > k.pos() {
                    *k = by.clone();
                }
            }
            None => {
                self.origins.insert(by.origin.clone(), by.clone());
            }
        }
    }

    /// Drop the kills whose removal is older than a retraction: what they could kill is gone.
    fn purge_below(&mut self, r: &Stamp) {
        self.frames.retain(|_, s| *s >= *r);
        self.origins.retain(|_, s| *s >= *r);
    }

    fn is_empty(&self) -> bool {
        self.frames.is_empty() && self.origins.is_empty()
    }
}

/// One value of a set: the live tags that assert it and the removals aimed at it.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Entry {
    /// The term as first written (equality is by `canon_value`; the lexical form is whichever
    /// asserted the value when it became visible).
    term: Term,
    /// Live tags: stamp to the asserting frame.
    tags: BTreeMap<Stamp, FrameId>,
    kills: Kills,
}

/// A visible-value change, reported by the set operations to the caller that keeps the counts.
#[derive(Debug)]
enum Change {
    Appear(Term),
    Vanish(Term),
}

/// The state of one add-wins key.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct SetHead {
    entries: BTreeMap<CanonValue, Entry>,
    /// Key-level removals: a `set` on a set key replaces what it observed.
    clears: Kills,
}

impl SetHead {
    fn is_empty(&self) -> bool {
        self.entries.is_empty() && self.clears.is_empty()
    }

    fn add(&mut self, value: &Term, stamp: &Stamp, frame: &str, out: &mut Vec<Change>) {
        let c = canon_value(value);
        let entry = self.entries.entry(c.clone()).or_insert_with(|| Entry {
            term: value.clone(),
            tags: BTreeMap::new(),
            kills: Kills::default(),
        });
        let dead = entry.kills.kills(stamp, frame) || self.clears.kills(stamp, frame);
        if !dead {
            let was_visible = !entry.tags.is_empty();
            entry.tags.insert(stamp.clone(), frame.to_string());
            if !was_visible {
                entry.term = value.clone();
                out.push(Change::Appear(value.clone()));
            }
        } else if entry.tags.is_empty() && entry.kills.is_empty() {
            self.entries.remove(&c);
        }
    }

    fn remove(&mut self, value: &Term, by: &Stamp, parents: &[FrameId], out: &mut Vec<Change>) {
        let entry = self
            .entries
            .entry(canon_value(value))
            .or_insert_with(|| Entry {
                term: value.clone(),
                tags: BTreeMap::new(),
                kills: Kills::default(),
            });
        entry.kills.add(parents, by);
        let was_visible = !entry.tags.is_empty();
        let kills = &entry.kills;
        entry.tags.retain(|st, fr| !kills.kills(st, fr.as_str()));
        if was_visible && entry.tags.is_empty() {
            out.push(Change::Vanish(entry.term.clone()));
        }
    }

    fn clear(&mut self, by: &Stamp, parents: &[FrameId], out: &mut Vec<Change>) {
        self.clears.add(parents, by);
        let clears = &self.clears;
        for entry in self.entries.values_mut() {
            let was_visible = !entry.tags.is_empty();
            entry.tags.retain(|st, fr| !clears.kills(st, fr.as_str()));
            if was_visible && entry.tags.is_empty() {
                out.push(Change::Vanish(entry.term.clone()));
            }
        }
        // An entry with no tag and no kill is what a fresh dead add leaves nothing of: drop it, so
        // the table does not depend on whether the clear or the add came first.
        self.entries
            .retain(|_, e| !e.tags.is_empty() || !e.kills.is_empty());
    }

    fn purge_below(&mut self, r: &Stamp, out: &mut Vec<Change>) {
        for entry in self.entries.values_mut() {
            let was_visible = !entry.tags.is_empty();
            entry.tags.retain(|st, _| *st >= *r);
            entry.kills.purge_below(r);
            if was_visible && entry.tags.is_empty() {
                out.push(Change::Vanish(entry.term.clone()));
            }
        }
        self.entries
            .retain(|_, e| !e.tags.is_empty() || !e.kills.is_empty());
        self.clears.purge_below(r);
    }

    /// The visible terms with the stamp and frame of their latest tag.
    fn visible(&self) -> impl Iterator<Item = (&Term, &Stamp, &FrameId)> {
        self.entries
            .values()
            .filter_map(|e| e.tags.iter().next_back().map(|(st, fr)| (&e.term, st, fr)))
    }
}

/// The value of one key.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum HeadValue {
    /// Single layer. `value: None` is a clear that won.
    One {
        value: Option<Term>,
        stamp: Stamp,
        frame: FrameId,
        parents: Vec<FrameId>,
        contested: bool,
    },
    /// Set layer.
    Many(SetHead),
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ClassHead {
    class: String,
    stamp: Stamp,
}

/// One visible value of a subject, for the heads route.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct HeadRow {
    pub predicate: String,
    pub value: Term,
    pub hlc: Hlc,
    pub frame: FrameId,
    pub origin: String,
    pub contested: bool,
}

/// The head table, with the small amount of per-subject state the rules need.
#[derive(Clone, Debug, Default)]
pub(crate) struct HeadTable {
    pub(super) heads: BTreeMap<HeadKey, HeadValue>,
    /// Class of the subject: from the latest live write that named one.
    class_of: BTreeMap<String, ClassHead>,
    /// Per subject, the stamp of its latest retraction: writes at or below it are dead.
    retracted_at: BTreeMap<String, Stamp>,
    /// Every subject an op ever named (the fold owns these in the sink, retracted or not).
    owned: BTreeSet<String>,
    /// Per subject, the number of visible values. A subject with none emits no triples.
    visible_count: BTreeMap<String, u64>,
}

/// Terms replaced by their canonical value, so two tables compare equal whenever they fold to
/// the same thing (the lexical form kept for a value is whichever write got there first).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CanonView {
    heads: BTreeMap<HeadKey, CanonHead>,
    class_of: BTreeMap<String, (String, Stamp)>,
    retracted_at: BTreeMap<String, Stamp>,
    owned: BTreeSet<String>,
    visible_count: BTreeMap<String, u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum CanonHead {
    One {
        value: Option<CanonValue>,
        stamp: Stamp,
        frame: FrameId,
        parents: Vec<FrameId>,
        contested: bool,
    },
    Many {
        entries: BTreeMap<CanonValue, (BTreeMap<Stamp, FrameId>, Kills)>,
        clears: Kills,
    },
}

/// Per-event scratch: what changed, netted at the end.
struct Cx {
    want: bool,
    /// nt-key to (triple, present before the event, present now).
    tracker: BTreeMap<String, (Triple, bool, bool)>,
    /// Subject to (had visible values, class) before the event touched it.
    pre: BTreeMap<String, (bool, Option<String>)>,
    touched: BTreeSet<String>,
    contested: Vec<HeadKey>,
}

impl Cx {
    fn new(want: bool) -> Cx {
        Cx {
            want,
            tracker: BTreeMap::new(),
            pre: BTreeMap::new(),
            touched: BTreeSet::new(),
            contested: Vec::new(),
        }
    }

    fn mark(&mut self, s: &str, p: &str, t: Term, now: bool) {
        let key = format!("{s}\u{0}{p}\u{0}{}", t.as_nt());
        match self.tracker.get_mut(&key) {
            Some(e) => e.2 = now,
            None => {
                self.tracker
                    .insert(key, ((s.to_string(), p.to_string(), t), !now, now));
            }
        }
    }

    fn snapshot(
        &mut self,
        subject: &str,
        counts: &BTreeMap<String, u64>,
        class_of: &BTreeMap<String, ClassHead>,
    ) {
        if !self.want || self.pre.contains_key(subject) {
            return;
        }
        self.pre.insert(
            subject.to_string(),
            (
                counts.contains_key(subject),
                class_of.get(subject).map(|c| c.class.clone()),
            ),
        );
    }
}

fn bump(counts: &mut BTreeMap<String, u64>, subject: &str, up: bool) {
    if up {
        match counts.get_mut(subject) {
            Some(c) => *c += 1,
            None => {
                counts.insert(subject.to_string(), 1);
            }
        }
    } else if let Some(c) = counts.get_mut(subject) {
        if *c <= 1 {
            counts.remove(subject);
        } else {
            *c -= 1;
        }
    }
}

fn record(
    counts: &mut BTreeMap<String, u64>,
    cx: &mut Cx,
    subject: &str,
    predicate: &str,
    ch: Change,
) {
    match ch {
        Change::Appear(t) => {
            bump(counts, subject, true);
            if cx.want {
                cx.mark(subject, predicate, t, true);
            }
        }
        Change::Vanish(t) => {
            bump(counts, subject, false);
            if cx.want {
                cx.mark(subject, predicate, t, false);
            }
        }
    }
}

fn same_value(a: &Option<Term>, b: &Option<Term>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(x), Some(y)) => canon_value(x) == canon_value(y),
        _ => false,
    }
}

/// Two writes are concurrent when at least one declares parents and neither names the other.
/// With no parents on either side nothing is known about their causality, so nothing is flagged.
fn concurrent(a_parents: &[FrameId], a_frame: &str, b_parents: &[FrameId], b_frame: &str) -> bool {
    a_frame != b_frame
        && (!a_parents.is_empty() || !b_parents.is_empty())
        && !a_parents.iter().any(|p| p == b_frame)
        && !b_parents.iter().any(|p| p == a_frame)
}

fn class_triple(subject: &str, class: &str) -> Option<Triple> {
    NamedNode::new(class)
        .ok()
        .map(|c| (subject.to_string(), RDF_TYPE.to_string(), Term::Uri(c)))
}

impl HeadTable {
    pub(crate) fn new() -> HeadTable {
        HeadTable::default()
    }

    /// Fold one event. O(ops), plus the size of a set that a `set` op on a set key replaces.
    ///
    /// Re-applying an event, or asserting a value equal (by `canon_value`) to the one already at
    /// the head, returns a Delta with no triples. A value-equal write with a later stamp keeps
    /// the stored term and advances the head's stamp, frame and parents, so that the head table
    /// does not depend on arrival order.
    pub(crate) fn apply(&mut self, policy: &FoldPolicy, ev: &FoldEvent) -> Delta {
        self.apply_inner(policy, ev, true)
    }

    /// Fold `events`, in the order given (per-origin order must be kept; any interleaving of
    /// origins gives the same table), and return the table with every triple it holds.
    pub(crate) fn rebuild<'a>(
        policy: &FoldPolicy,
        events: impl Iterator<Item = &'a FoldEvent>,
    ) -> (HeadTable, Vec<Triple>) {
        let mut table = HeadTable::default();
        for ev in events {
            table.apply_inner(policy, ev, false);
        }
        let mut triples = Vec::new();
        for subject in &table.owned {
            triples.extend(table.triples_of(subject));
        }
        (table, triples)
    }

    /// `rdf:type` plus every visible value of `subject`, in predicate order.
    pub(crate) fn triples_of(&self, subject: &str) -> Vec<Triple> {
        let mut out = Vec::new();
        if !self.visible_count.contains_key(subject) {
            return out;
        }
        if let Some(c) = self.class_of.get(subject) {
            out.extend(class_triple(subject, &c.class));
        }
        for (k, hv) in self.range_of(subject) {
            match hv {
                HeadValue::One { value: Some(t), .. } => {
                    out.push((subject.to_string(), k.predicate.clone(), t.clone()))
                }
                HeadValue::One { value: None, .. } => {}
                HeadValue::Many(sh) => {
                    for (t, _, _) in sh.visible() {
                        out.push((subject.to_string(), k.predicate.clone(), t.clone()));
                    }
                }
            }
        }
        out
    }

    /// The visible values of `subject` with their winning write, for the heads route.
    pub(crate) fn rows_of(&self, subject: &str) -> Vec<HeadRow> {
        let mut out = Vec::new();
        for (k, hv) in self.range_of(subject) {
            match hv {
                HeadValue::One {
                    value: Some(t),
                    stamp,
                    frame,
                    contested,
                    ..
                } => out.push(HeadRow {
                    predicate: k.predicate.clone(),
                    value: t.clone(),
                    hlc: stamp.hlc,
                    frame: frame.clone(),
                    origin: stamp.origin.clone(),
                    contested: *contested,
                }),
                HeadValue::One { value: None, .. } => {}
                HeadValue::Many(sh) => {
                    for (t, st, fr) in sh.visible() {
                        out.push(HeadRow {
                            predicate: k.predicate.clone(),
                            value: t.clone(),
                            hlc: st.hlc,
                            frame: fr.clone(),
                            origin: st.origin.clone(),
                            contested: false,
                        });
                    }
                }
            }
        }
        out
    }

    /// Subjects any op ever named: what the fold owns in the sink, retracted ones included.
    pub(crate) fn owned_subjects(&self) -> &BTreeSet<String> {
        &self.owned
    }

    /// Subjects whose latest retraction is still in force (no later write revived them).
    pub(crate) fn retracted_subjects(&self) -> Vec<&str> {
        self.retracted_at
            .keys()
            .filter(|s| !self.visible_count.contains_key(s.as_str()))
            .map(String::as_str)
            .collect()
    }

    /// Number of keys flagged contested.
    pub(crate) fn contested_count(&self) -> usize {
        self.heads
            .values()
            .filter(|h| {
                matches!(
                    h,
                    HeadValue::One {
                        contested: true,
                        ..
                    }
                )
            })
            .count()
    }

    /// The keys flagged contested.
    pub(crate) fn contested_keys(&self) -> Vec<HeadKey> {
        self.heads
            .iter()
            .filter(|(_, h)| {
                matches!(
                    h,
                    HeadValue::One {
                        contested: true,
                        ..
                    }
                )
            })
            .map(|(k, _)| k.clone())
            .collect()
    }

    /// Number of keys held (live or tombstoned).
    pub(crate) fn len(&self) -> usize {
        self.heads.len()
    }

    /// The table with terms replaced by canonical values; equal views mean equal folds.
    pub(crate) fn canonical(&self) -> CanonView {
        let heads = self
            .heads
            .iter()
            .map(|(k, h)| {
                let c = match h {
                    HeadValue::One {
                        value,
                        stamp,
                        frame,
                        parents,
                        contested,
                    } => CanonHead::One {
                        value: value.as_ref().map(canon_value),
                        stamp: stamp.clone(),
                        frame: frame.clone(),
                        parents: parents.clone(),
                        contested: *contested,
                    },
                    HeadValue::Many(sh) => CanonHead::Many {
                        entries: sh
                            .entries
                            .iter()
                            .map(|(c, e)| (c.clone(), (e.tags.clone(), e.kills.clone())))
                            .collect(),
                        clears: sh.clears.clone(),
                    },
                };
                (k.clone(), c)
            })
            .collect();
        CanonView {
            heads,
            class_of: self
                .class_of
                .iter()
                .map(|(s, c)| (s.clone(), (c.class.clone(), c.stamp.clone())))
                .collect(),
            retracted_at: self.retracted_at.clone(),
            owned: self.owned.clone(),
            visible_count: self.visible_count.clone(),
        }
    }

    fn range_of<'a>(
        &'a self,
        subject: &'a str,
    ) -> impl Iterator<Item = (&'a HeadKey, &'a HeadValue)> {
        self.heads
            .range(HeadKey::lower_bound(subject)..)
            .take_while(move |(k, _)| k.subject == subject)
    }

    fn apply_inner(&mut self, policy: &FoldPolicy, ev: &FoldEvent, want: bool) -> Delta {
        let mut cx = Cx::new(want);
        let parents_by_subject: BTreeMap<&str, &[FrameId]> = ev
            .subjects
            .iter()
            .filter(|s| !s.parents.is_empty())
            .map(|s| (s.subject.as_str(), s.parents.as_slice()))
            .collect();
        for (i, op) in ev.ops.iter().enumerate() {
            let parents: &[FrameId] = parents_by_subject.get(op.subject()).copied().unwrap_or(&[]);
            self.apply_op(policy, ev, i as u32, op, parents, &mut cx);
        }
        self.finish(cx)
    }

    fn apply_op(
        &mut self,
        policy: &FoldPolicy,
        ev: &FoldEvent,
        idx: u32,
        op: &Op,
        parents: &[FrameId],
        cx: &mut Cx,
    ) {
        let subject = op.subject();
        if !self.owned.contains(subject) {
            self.owned.insert(subject.to_string());
        }
        if cx.want && !cx.touched.contains(subject) {
            cx.touched.insert(subject.to_string());
        }
        let stamp = Stamp {
            hlc: ev.hlc,
            origin: ev.origin.clone(),
            seq: ev.seq,
            idx,
        };
        if let Some(r) = self.retracted_at.get(subject) {
            if stamp <= *r {
                return;
            }
        }
        cx.snapshot(subject, &self.visible_count, &self.class_of);
        let predicate = match op.predicate() {
            None => {
                self.do_retract(subject, stamp, cx);
                return;
            }
            Some(p) => p,
        };
        match self.class_of.get_mut(subject) {
            Some(c) => {
                if stamp > c.stamp {
                    if c.class != op.class() {
                        c.class = op.class().to_string();
                    }
                    c.stamp = stamp.clone();
                }
            }
            None => {
                self.class_of.insert(
                    subject.to_string(),
                    ClassHead {
                        class: op.class().to_string(),
                        stamp: stamp.clone(),
                    },
                );
            }
        }
        let merge = policy.merge_for(predicate, op);
        if merge.is_set() {
            self.apply_set(op, subject, predicate, stamp, &ev.id, parents, cx);
        } else {
            // Under lww and contested: set writes its value; add writes the last value; remove
            // clears. An add or remove with no values writes nothing.
            let value: Option<Option<Term>> = match op {
                Op::Set { value, .. } => Some(value.clone()),
                Op::Add { values, .. } => values.last().map(|v| Some(v.clone())),
                Op::Remove { values, .. } => {
                    if values.is_empty() {
                        None
                    } else {
                        Some(None)
                    }
                }
                Op::Retract { .. } => None,
            };
            if let Some(value) = value {
                self.apply_single(
                    subject,
                    predicate,
                    value,
                    stamp,
                    &ev.id,
                    parents,
                    merge == Merge::Contested,
                    cx,
                );
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn apply_single(
        &mut self,
        subject: &str,
        predicate: &str,
        value: Option<Term>,
        stamp: Stamp,
        frame: &str,
        parents: &[FrameId],
        contested_mode: bool,
        cx: &mut Cx,
    ) {
        let HeadTable {
            heads,
            visible_count,
            ..
        } = self;
        let key = HeadKey {
            subject: subject.to_string(),
            predicate: predicate.to_string(),
            set: false,
        };
        match heads.entry(key.clone()) {
            MapEntry::Vacant(v) => {
                if let Some(t) = &value {
                    record(
                        visible_count,
                        cx,
                        subject,
                        predicate,
                        Change::Appear(t.clone()),
                    );
                }
                v.insert(HeadValue::One {
                    value,
                    stamp,
                    frame: frame.to_string(),
                    parents: parents.to_vec(),
                    contested: false,
                });
            }
            MapEntry::Occupied(mut o) => {
                let HeadValue::One {
                    value: hv,
                    stamp: hs,
                    frame: hf,
                    parents: hp,
                    contested: hc,
                } = o.get_mut()
                else {
                    return;
                };
                if stamp == *hs {
                    return;
                }
                let same = same_value(hv, &value);
                let cc = contested_mode
                    && !same
                    && concurrent(parents, frame, hp.as_slice(), hf.as_str());
                if stamp > *hs {
                    if !same {
                        if let Some(old) = hv.take() {
                            record(visible_count, cx, subject, predicate, Change::Vanish(old));
                        }
                        if let Some(new) = &value {
                            record(
                                visible_count,
                                cx,
                                subject,
                                predicate,
                                Change::Appear(new.clone()),
                            );
                        }
                        *hv = value;
                    }
                    *hs = stamp;
                    *hf = frame.to_string();
                    *hp = parents.to_vec();
                    *hc = cc;
                    if cc && cx.want {
                        cx.contested.push(key);
                    }
                } else if cc && !*hc {
                    *hc = true;
                    if cx.want {
                        cx.contested.push(key);
                    }
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn apply_set(
        &mut self,
        op: &Op,
        subject: &str,
        predicate: &str,
        stamp: Stamp,
        frame: &str,
        parents: &[FrameId],
        cx: &mut Cx,
    ) {
        let HeadTable {
            heads,
            visible_count,
            ..
        } = self;
        let key = HeadKey {
            subject: subject.to_string(),
            predicate: predicate.to_string(),
            set: true,
        };
        let mut sh = match heads.remove(&key) {
            Some(HeadValue::Many(s)) => s,
            _ => SetHead::default(),
        };
        let mut out: Vec<Change> = Vec::new();
        match op {
            Op::Add { values, .. } => {
                for v in values {
                    sh.add(v, &stamp, frame, &mut out);
                }
            }
            Op::Remove { values, .. } => {
                for v in values {
                    sh.remove(v, &stamp, parents, &mut out);
                }
            }
            Op::Set { value, .. } => {
                sh.clear(&stamp, parents, &mut out);
                if let Some(v) = value {
                    sh.add(v, &stamp, frame, &mut out);
                }
            }
            Op::Retract { .. } => {}
        }
        for ch in out {
            record(visible_count, cx, subject, predicate, ch);
        }
        if !sh.is_empty() {
            heads.insert(key, HeadValue::Many(sh));
        }
    }

    fn do_retract(&mut self, subject: &str, r: Stamp, cx: &mut Cx) {
        let HeadTable {
            heads,
            class_of,
            retracted_at,
            visible_count,
            ..
        } = self;
        retracted_at.insert(subject.to_string(), r.clone());
        if class_of.get(subject).map_or(false, |c| c.stamp < r) {
            class_of.remove(subject);
        }
        let keys: Vec<HeadKey> = heads
            .range(HeadKey::lower_bound(subject)..)
            .take_while(|(k, _)| k.subject == subject)
            .map(|(k, _)| k.clone())
            .collect();
        for key in keys {
            let Some(hv) = heads.remove(&key) else {
                continue;
            };
            match hv {
                HeadValue::One {
                    value,
                    stamp,
                    frame,
                    parents,
                    contested,
                } => {
                    if stamp < r {
                        if let Some(t) = value {
                            record(
                                visible_count,
                                cx,
                                subject,
                                &key.predicate,
                                Change::Vanish(t),
                            );
                        }
                    } else {
                        heads.insert(
                            key,
                            HeadValue::One {
                                value,
                                stamp,
                                frame,
                                parents,
                                contested,
                            },
                        );
                    }
                }
                HeadValue::Many(mut sh) => {
                    let mut out = Vec::new();
                    sh.purge_below(&r, &mut out);
                    for ch in out {
                        record(visible_count, cx, subject, &key.predicate, ch);
                    }
                    if !sh.is_empty() {
                        heads.insert(key, HeadValue::Many(sh));
                    }
                }
            }
        }
    }

    fn finish(&mut self, cx: Cx) -> Delta {
        let mut delta = Delta::default();
        if !cx.want {
            return delta;
        }
        for (subject, (pre_vis, pre_class)) in &cx.pre {
            let post_vis = self.visible_count.contains_key(subject.as_str());
            let post_class = self.class_of.get(subject).map(|c| c.class.clone());
            let pre_present = *pre_vis && pre_class.is_some();
            let post_present = post_vis && post_class.is_some();
            let changed = pre_present != post_present || (pre_present && *pre_class != post_class);
            if !changed {
                continue;
            }
            if pre_present {
                if let Some(t) = pre_class.as_deref().and_then(|c| class_triple(subject, c)) {
                    delta.remove.push(t);
                }
            }
            if post_present {
                if let Some(t) = post_class.as_deref().and_then(|c| class_triple(subject, c)) {
                    delta.insert.push(t);
                }
            }
        }
        for (_, (t, before, now)) in cx.tracker {
            if before && !now {
                delta.remove.push(t);
            } else if !before && now {
                delta.insert.push(t);
            }
        }
        delta.touched = cx.touched;
        let mut seen = BTreeSet::new();
        delta.contested = cx
            .contested
            .into_iter()
            .filter(|k| seen.insert(k.clone()))
            .collect();
        delta
    }
}

/// The value-canonical set of a triple list (what the oracle compares).
pub(crate) fn canon_set(triples: &[Triple]) -> BTreeSet<(String, String, CanonValue)> {
    triples.iter().map(canon).collect()
}
