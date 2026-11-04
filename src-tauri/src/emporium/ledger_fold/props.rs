//! Property tests of the fold (W7 section 3, properties 1 to 8) and the unit tests that pin its
//! corners. `cargo test --lib ledger_fold`.
//!
//! The generators build a global sequence of frames: frame `i` has HLC ms `i + 1`, so a parent
//! (always an earlier frame) has a smaller stamp than its child and, within one origin, the HLC
//! grows with the seq, which is what the ledger gives. A fold "order" is any interleaving of the
//! origins that keeps each origin's own order.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use oxigraph::model::{GraphNameRef, Literal, NamedNode};
use oxigraph::store::Store;
use proptest::prelude::*;

use super::apply::apply_delta_commit;
use super::heads::canon_set;
use super::*;
use crate::emporium::terms::{canon_value, Term, Triple};

const ORIGINS: [&str; 3] = ["oa", "ob", "oc"];
const SUBJECTS: [&str; 3] = ["urn:t:s1", "urn:t:s2", "urn:t:s3"];
const CLASS: &str = "urn:t:C";
const P1: &str = "urn:t:p1";
const P2: &str = "urn:t:p2";
const Q1: &str = "urn:t:q1";
const Q2: &str = "urn:t:q2";
const C1: &str = "urn:t:c1";
/// lww, lww, add-wins, add-wins, contested (index into this for the generators).
const PREDS: [&str; 5] = [P1, P2, Q1, Q2, C1];

fn policy() -> FoldPolicy {
    let mut by = BTreeMap::new();
    by.insert(Q1.to_string(), Merge::AddWins);
    by.insert(Q2.to_string(), Merge::AddWins);
    by.insert(C1.to_string(), Merge::Contested);
    FoldPolicy {
        default: Merge::Lww,
        by_predicate: by,
        by_op: false,
    }
}

fn typed(lexical: &str, local: &str) -> Term {
    Term::Lit(Literal::new_typed_literal(
        lexical,
        NamedNode::new(format!("http://www.w3.org/2001/XMLSchema#{local}")).unwrap(),
    ))
}

fn lit(s: &str) -> Term {
    Term::Lit(Literal::new_simple_literal(s))
}

fn iri(s: &str) -> Term {
    Term::Uri(NamedNode::new(s).unwrap())
}

/// Nine terms; 3, 4 and 5 are the same number in three lexical forms (equal by `canon_value`).
fn term(i: u8) -> Term {
    match i % 9 {
        0 => lit("x"),
        1 => lit("y"),
        2 => Term::Lit(Literal::new_language_tagged_literal("x", "es").unwrap()),
        3 => typed("1", "integer"),
        4 => typed("1", "long"),
        5 => typed("1.0", "double"),
        6 => typed("2", "integer"),
        7 => iri("urn:t:o1"),
        _ => iri("urn:t:o2"),
    }
}

fn set(subject: &str, pred: &str, v: Option<Term>) -> Op {
    Op::Set {
        subject: subject.into(),
        class: CLASS.into(),
        predicate: pred.into(),
        value: v,
    }
}

fn add(subject: &str, pred: &str, vs: Vec<Term>) -> Op {
    Op::Add {
        subject: subject.into(),
        class: CLASS.into(),
        predicate: pred.into(),
        values: vs,
    }
}

fn rem(subject: &str, pred: &str, vs: Vec<Term>) -> Op {
    Op::Remove {
        subject: subject.into(),
        class: CLASS.into(),
        predicate: pred.into(),
        values: vs,
    }
}

fn retract(subject: &str) -> Op {
    Op::Retract {
        subject: subject.into(),
        class: CLASS.into(),
    }
}

fn hlc(ms: u64) -> Hlc {
    Hlc { ms, count: 0 }
}

fn ev(origin: &str, seq: u64, ms: u64, parents: &[&str], ops: Vec<Op>) -> FoldEvent {
    let parents: Vec<FrameId> = parents.iter().map(|p| p.to_string()).collect();
    FoldEvent::synthetic(origin, seq, hlc(ms), &parents, ops)
}

fn fold(policy: &FoldPolicy, events: &[FoldEvent]) -> HeadTable {
    let mut t = HeadTable::new();
    for e in events {
        t.apply(policy, e);
    }
    t
}

fn all_triples(t: &HeadTable) -> Vec<Triple> {
    t.owned_subjects()
        .iter()
        .flat_map(|s| t.triples_of(s))
        .collect()
}

fn nt(t: &Triple) -> String {
    format!("<{}> <{}> {}", t.0, t.1, t.2.as_nt())
}

fn nt_set(ts: &[Triple]) -> BTreeSet<String> {
    ts.iter().map(nt).collect()
}

fn has(t: &HeadTable, subject: &str, pred: &str, v: &Term) -> bool {
    let want = canon_value(v);
    t.triples_of(subject)
        .iter()
        .any(|(_, p, o)| p == pred && canon_value(o) == want)
}

// ---------------------------------------------------------------------------
// Generators
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct RawOp {
    kind: u8,
    subj: u8,
    pred: u8,
    vals: Vec<u8>,
}

#[derive(Clone, Debug)]
struct RawFrame {
    origin: u8,
    ops: Vec<RawOp>,
    picks: Vec<u8>,
}

fn raw_op(npreds: u8) -> impl Strategy<Value = RawOp> {
    (
        0u8..11,
        0u8..3,
        0u8..npreds,
        prop::collection::vec(0u8..9, 0..3),
    )
        .prop_map(|(kind, subj, pred, vals)| RawOp {
            kind,
            subj,
            pred,
            vals,
        })
}

fn raw_frame(npreds: u8) -> impl Strategy<Value = RawFrame> {
    (
        0u8..3,
        prop::collection::vec(raw_op(npreds), 1..4),
        prop::collection::vec(any::<u8>(), 0..3),
    )
        .prop_map(|(origin, ops, picks)| RawFrame { origin, ops, picks })
}

fn to_op(r: &RawOp) -> Op {
    let subject = SUBJECTS[(r.subj % 3) as usize];
    let pred = PREDS[r.pred as usize % PREDS.len()];
    let vals: Vec<Term> = r.vals.iter().map(|v| term(*v)).collect();
    match r.kind {
        0..=3 | 9 => set(
            subject,
            pred,
            Some(term(r.vals.first().copied().unwrap_or(0))),
        ),
        4 => set(subject, pred, None),
        5 | 6 => add(subject, pred, vals),
        7 | 8 => rem(subject, pred, vals),
        _ => retract(subject),
    }
}

/// The global sequence: frame `i` is `ORIGINS[origin]` at the next seq of that origin, HLC ms
/// `i + 1`, parents drawn from earlier frames.
fn build_events(raw: &[RawFrame]) -> Vec<FoldEvent> {
    let mut seqs = [0u64; 3];
    let mut ids: Vec<FrameId> = Vec::new();
    let mut out = Vec::new();
    for (i, f) in raw.iter().enumerate() {
        let o = (f.origin % 3) as usize;
        seqs[o] += 1;
        let seq = seqs[o];
        let id = synthetic_id(ORIGINS[o], seq);
        let mut parents: Vec<FrameId> = Vec::new();
        if i > 0 {
            for p in &f.picks {
                let pid = ids[*p as usize % i].clone();
                if pid != id && !parents.contains(&pid) {
                    parents.push(pid);
                }
            }
        }
        let ops: Vec<Op> = f.ops.iter().map(to_op).collect();
        out.push(FoldEvent::synthetic(
            ORIGINS[o],
            seq,
            hlc(i as u64 + 1),
            &parents,
            ops,
        ));
        ids.push(id);
    }
    out
}

fn interleave(events: &[FoldEvent], picks: &[u8]) -> Vec<FoldEvent> {
    let mut q: Vec<VecDeque<FoldEvent>> = vec![VecDeque::new(); 3];
    for e in events {
        let o = ORIGINS.iter().position(|x| *x == e.origin).unwrap();
        q[o].push_back(e.clone());
    }
    let mut out = Vec::new();
    let mut k = 0usize;
    loop {
        let live: Vec<usize> = (0..3).filter(|i| !q[*i].is_empty()).collect();
        if live.is_empty() {
            break;
        }
        let pick = picks.get(k % picks.len().max(1)).copied().unwrap_or(0) as usize;
        k += 1;
        let o = live[pick % live.len()];
        out.push(q[o].pop_front().unwrap());
    }
    out
}

fn by_origin(events: &[FoldEvent]) -> Vec<FoldEvent> {
    let mut out = Vec::new();
    for o in ORIGINS {
        out.extend(events.iter().filter(|e| e.origin == o).cloned());
    }
    out
}

// ---------------------------------------------------------------------------
// The eight properties
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(96))]

    /// 1. Convergence: any interleaving that keeps per-origin order folds to the same table and
    /// the same triple set. (Contested keys are left out: their flag reads the head at arrival and
    /// a transitive ancestor is not a parent; see `contested_flag_*`.)
    #[test]
    fn p1_convergence(
        frames in prop::collection::vec(raw_frame(4), 1..14),
        picks in prop::collection::vec(any::<u8>(), 1..24),
    ) {
        let policy = policy();
        let events = build_events(&frames);
        let base = fold(&policy, &events);
        for alt in [interleave(&events, &picks), by_origin(&events)] {
            let t = fold(&policy, &alt);
            prop_assert_eq!(base.canonical(), t.canonical());
            prop_assert_eq!(canon_set(&all_triples(&base)), canon_set(&all_triples(&t)));
        }
    }

    /// 2. Idempotence: applying an event twice, the second Delta is empty and the table is
    /// untouched (contested keys included: an event never conflicts with itself).
    #[test]
    fn p2_idempotence_immediate(frames in prop::collection::vec(raw_frame(5), 1..14)) {
        let policy = policy();
        let mut t = HeadTable::new();
        for e in &build_events(&frames) {
            t.apply(&policy, e);
            let before = t.canonical();
            let d = t.apply(&policy, e);
            prop_assert!(d.is_empty(), "second application changed triples: {:?}", d);
            prop_assert!(d.contested.is_empty());
            prop_assert_eq!(&before, &t.canonical());
        }
    }

    /// 2b. Replaying the whole stream after folding it changes nothing.
    #[test]
    fn p2_idempotence_full_replay(frames in prop::collection::vec(raw_frame(4), 1..14)) {
        let policy = policy();
        let events = build_events(&frames);
        let mut t = fold(&policy, &events);
        let before = t.canonical();
        for e in &events {
            let d = t.apply(&policy, e);
            prop_assert!(d.is_empty(), "replay changed triples: {:?}", d);
        }
        prop_assert_eq!(before, t.canonical());
    }

    /// 3. No-op re-assert: `set` with a value equal to the head's (by canon_value, in any lexical
    /// form) is an empty Delta and leaves the rendered triples exactly as they were.
    #[test]
    fn p3_noop_reassert(
        pair in prop::sample::select(vec![(3u8, 4u8), (3, 5), (4, 5), (0, 0), (2, 2), (7, 7), (6, 6)]),
        o1 in 0u8..3,
        o2 in 0u8..3,
        pred in 0u8..2,
        late_arrival in any::<bool>(),
    ) {
        let policy = policy();
        let s = SUBJECTS[0];
        let p = PREDS[pred as usize];
        let seq2 = if o1 == o2 { 2 } else { 1 };
        let e1 = ev(ORIGINS[o1 as usize], 1, 10, &[], vec![set(s, p, Some(term(pair.0)))]);
        let e2 = ev(ORIGINS[o2 as usize], seq2, 20, &[], vec![set(s, p, Some(term(pair.1)))]);
        let mut t = HeadTable::new();
        // the head holds e1's value; e2 re-asserts it, later in HLC order
        let (first, second) = if late_arrival || o1 == o2 { (&e1, &e2) } else { (&e2, &e1) };
        t.apply(&policy, first);
        let rendered = nt_set(&t.triples_of(s));
        let d = t.apply(&policy, second);
        prop_assert!(d.is_empty(), "value-equal re-assert changed triples: {:?}", d);
        prop_assert_eq!(rendered, nt_set(&t.triples_of(s)));
        let rows = t.rows_of(s);
        prop_assert_eq!(rows.len(), 1);
        prop_assert_eq!(canon_value(&rows[0].value), canon_value(&term(pair.0)));
    }

    /// 4. Lww by HLC, ties by origin id: the same winner in either arrival order.
    #[test]
    fn p4_lww_winner(
        o1 in 0u8..3,
        o2 in 0u8..3,
        h1 in 1u64..6,
        h2 in 1u64..6,
        c1 in 0u32..2,
        c2 in 0u32..2,
        pred in 0u8..2,
    ) {
        let policy = policy();
        let s = SUBJECTS[0];
        let p = PREDS[pred as usize];
        let hlc1 = Hlc { ms: h1, count: c1 };
        // within one origin the clock does not go back as seq grows
        let hlc2 = if o1 == o2 { Hlc { ms: h1 + h2, count: c2 } } else { Hlc { ms: h2, count: c2 } };
        let seq2 = if o1 == o2 { 2 } else { 1 };
        let e1 = FoldEvent::synthetic(ORIGINS[o1 as usize], 1, hlc1, &[], vec![set(s, p, Some(lit("x")))]);
        let e2 = FoldEvent::synthetic(ORIGINS[o2 as usize], seq2, hlc2, &[], vec![set(s, p, Some(lit("y")))]);
        let second_wins = (hlc2, ORIGINS[o2 as usize], seq2) > (hlc1, ORIGINS[o1 as usize], 1u64);
        let expected = if second_wins { lit("y") } else { lit("x") };
        let orders: Vec<[&FoldEvent; 2]> = if o1 == o2 { vec![[&e1, &e2]] } else { vec![[&e1, &e2], [&e2, &e1]] };
        for order in orders {
            let mut t = HeadTable::new();
            for e in order {
                t.apply(&policy, e);
            }
            let got = nt_set(&t.triples_of(s));
            let want: BTreeSet<String> = [
                format!("<{s}> <{RDF_TYPE}> <{CLASS}>"),
                format!("<{s}> <{p}> {}", expected.as_nt()),
            ].into_iter().collect();
            prop_assert_eq!(got, want);
        }
    }

    /// 5a. Add-wins: a removal that names the asserting frame as parent (or comes later in the
    /// same origin) removes the value, whichever arrives first; a variant lexical form of the
    /// value is the same value.
    #[test]
    fn p5a_observed_removal(
        pair in prop::sample::select(vec![(3u8, 4u8), (3, 5), (0, 0), (2, 2), (7, 7)]),
        oa in 0u8..3,
        ob in 0u8..3,
        q in 0u8..2,
    ) {
        let policy = policy();
        let s = SUBJECTS[1];
        let p = PREDS[2 + q as usize];
        let a = ORIGINS[oa as usize];
        let b = ORIGINS[ob as usize];
        // by parent, two origins
        if a != b {
            let add_ev = ev(a, 1, 10, &[], vec![add(s, p, vec![term(pair.0)])]);
            let parent_id = synthetic_id(a, 1);
            let rem_ev = ev(b, 1, 20, &[parent_id.as_str()], vec![rem(s, p, vec![term(pair.1)])]);
            for order in [[&add_ev, &rem_ev], [&rem_ev, &add_ev]] {
                let mut t = HeadTable::new();
                for e in order { t.apply(&policy, e); }
                prop_assert!(t.triples_of(s).is_empty(), "parent-named removal left {:?}", t.triples_of(s));
            }
        }
        // same origin, later in its stream, no parents
        let add_ev = ev(a, 1, 10, &[], vec![add(s, p, vec![term(pair.0)])]);
        let rem_ev = ev(a, 2, 20, &[], vec![rem(s, p, vec![term(pair.1)])]);
        let t = fold(&policy, &[add_ev, rem_ev]);
        prop_assert!(t.triples_of(s).is_empty());
        // remove first, add later (same origin): the add survives
        let rem_ev = ev(a, 1, 10, &[], vec![rem(s, p, vec![term(pair.1)])]);
        let add_ev = ev(a, 2, 20, &[], vec![add(s, p, vec![term(pair.0)])]);
        let t = fold(&policy, &[rem_ev, add_ev]);
        prop_assert!(has(&t, s, p, &term(pair.0)));
    }

    /// 5b. A concurrent add and remove (the remove names no parent that is the add) keep the value,
    /// in both arrival orders, whichever has the later HLC.
    #[test]
    fn p5b_concurrent_add_remove_keeps_value(
        v in 0u8..9,
        oa in 0u8..3,
        ob in 0u8..3,
        ha in 1u64..8,
        hb in 1u64..8,
        with_unrelated_parent in any::<bool>(),
        q in 0u8..2,
    ) {
        prop_assume!(oa != ob);
        let policy = policy();
        let s = SUBJECTS[1];
        let p = PREDS[2 + q as usize];
        let a = ORIGINS[oa as usize];
        let b = ORIGINS[ob as usize];
        let add_ev = ev(a, 1, ha, &[], vec![add(s, p, vec![term(v)])]);
        let unrelated = ["zz#9"];
        let parents: &[&str] = if with_unrelated_parent { &unrelated } else { &[] };
        let rem_ev = ev(b, 1, hb, parents, vec![rem(s, p, vec![term(v)])]);
        for order in [[&add_ev, &rem_ev], [&rem_ev, &add_ev]] {
            let mut t = HeadTable::new();
            for e in order { t.apply(&policy, e); }
            prop_assert!(has(&t, s, p, &term(v)), "concurrent remove removed the value");
        }
    }

    /// 5c. Removing a value never asserted is a no-op; the values present stay.
    #[test]
    fn p5c_remove_never_asserted_is_noop(
        have in 0u8..9,
        miss in 0u8..9,
        oa in 0u8..3,
        ob in 0u8..3,
    ) {
        prop_assume!(canon_value(&term(have)) != canon_value(&term(miss)));
        let policy = policy();
        let s = SUBJECTS[2];
        let mut t = HeadTable::new();
        t.apply(&policy, &ev(ORIGINS[oa as usize], 1, 5, &[], vec![add(s, Q1, vec![term(have)])]));
        let before = nt_set(&t.triples_of(s));
        let d = t.apply(&policy, &ev(ORIGINS[ob as usize], if oa == ob { 2 } else { 1 }, 6, &[], vec![rem(s, Q1, vec![term(miss)])]));
        prop_assert!(d.is_empty(), "removal of an unasserted value changed triples: {:?}", d);
        prop_assert_eq!(before, nt_set(&t.triples_of(s)));
    }

    /// 6. Retract: after a retraction later than everything, no head of the subject remains and
    /// `triples_of` is empty, whatever the arrival order; a later `set` revives it with only the
    /// new head; other subjects are untouched.
    #[test]
    fn p6_retract_and_revive(
        frames in prop::collection::vec(raw_frame(5), 1..10),
        picks in prop::collection::vec(any::<u8>(), 1..16),
        subj in 0u8..3,
        o in 0u8..3,
    ) {
        let policy = policy();
        let s = SUBJECTS[subj as usize];
        let mut events = build_events(&frames);
        let n = events.len() as u64;
        let seq = events.iter().filter(|e| e.origin == ORIGINS[o as usize]).count() as u64 + 1;
        let without = events.clone();
        let retract_ev = ev(ORIGINS[o as usize], seq, n + 1, &[], vec![retract(s)]);
        let revive_ev = ev(ORIGINS[o as usize], seq + 1, n + 2, &[], vec![set(s, P1, Some(lit("x")))]);
        events.push(retract_ev);
        let reference = fold(&policy, &without);
        for order in [events.clone(), interleave(&events, &picks)] {
            let mut t = fold(&policy, &order);
            prop_assert!(t.triples_of(s).is_empty());
            prop_assert!(t.rows_of(s).is_empty());
            prop_assert!(!t.heads.keys().any(|k| k.subject == s), "a head of the subject survived");
            prop_assert!(t.retracted_subjects().contains(&s));
            for other in SUBJECTS.iter().filter(|x| **x != s) {
                prop_assert_eq!(canon_set(&t.triples_of(other)), canon_set(&reference.triples_of(other)));
            }
            t.apply(&policy, &revive_ev);
            let want: BTreeSet<String> = [
                format!("<{s}> <{RDF_TYPE}> <{CLASS}>"),
                format!("<{s}> <{P1}> {}", lit("x").as_nt()),
            ].into_iter().collect();
            prop_assert_eq!(nt_set(&t.triples_of(s)), want);
            prop_assert!(!t.retracted_subjects().contains(&s));
        }
    }

    /// 7. Contested: two concurrent sets flag the key (in either arrival order); a set that names
    /// the other as parent clears the flag; no parents means no flag.
    #[test]
    fn p7_contested_flag(
        oa in 0u8..3,
        ob in 0u8..3,
        ha in 1u64..8,
        hb in 1u64..8,
        swap in any::<bool>(),
    ) {
        prop_assume!(oa != ob);
        prop_assume!(ha != hb);
        let policy = policy();
        let s = SUBJECTS[0];
        let a = ORIGINS[oa as usize];
        let b = ORIGINS[ob as usize];
        let ea = ev(a, 1, ha, &["root#0"], vec![set(s, C1, Some(lit("x")))]);
        let eb = ev(b, 1, hb, &["root#0"], vec![set(s, C1, Some(lit("y")))]);
        let (first, second) = if swap { (&eb, &ea) } else { (&ea, &eb) };
        let mut t = HeadTable::new();
        let d1 = t.apply(&policy, first);
        prop_assert!(d1.contested.is_empty());
        let d2 = t.apply(&policy, second);
        prop_assert_eq!(d2.contested.len(), 1);
        prop_assert_eq!(t.contested_count(), 1);
        prop_assert!(t.rows_of(s).iter().all(|r| r.contested));
        let winner = if (hlc(hb), b) > (hlc(ha), a) { &eb } else { &ea };
        // a set naming the winner as parent clears the flag
        let c = ORIGINS[(3 - oa - ob) as usize];
        let e3 = ev(c, 1, 20, &[winner.id.as_str()], vec![set(s, C1, Some(lit("z")))]);
        t.apply(&policy, &e3);
        prop_assert_eq!(t.contested_count(), 0);
        // without parents there is no flag, only lww
        let ea = ev(a, 1, ha, &[], vec![set(s, C1, Some(lit("x")))]);
        let eb = ev(b, 1, hb, &[], vec![set(s, C1, Some(lit("y")))]);
        let (f2, s2) = if swap { (&eb, &ea) } else { (&ea, &eb) };
        let mut t = HeadTable::new();
        t.apply(&policy, f2);
        t.apply(&policy, s2);
        prop_assert_eq!(t.contested_count(), 0);
    }

    /// 8. Incremental equals rebuild: the Deltas, replayed on a triple set, are exactly the
    /// rebuilt set; every removal removes a triple that is there and every insert adds one that
    /// is not.
    #[test]
    fn p8_incremental_equals_rebuild(frames in prop::collection::vec(raw_frame(5), 1..14)) {
        let policy = policy();
        let events = build_events(&frames);
        let mut t = HeadTable::new();
        let mut live: BTreeSet<String> = BTreeSet::new();
        for e in &events {
            let d = t.apply(&policy, e);
            for r in &d.remove {
                prop_assert!(live.remove(&nt(r)), "Delta removes a triple the sink does not hold: {}", nt(r));
            }
            for i in &d.insert {
                prop_assert!(live.insert(nt(i)), "Delta inserts a triple already in the sink: {}", nt(i));
            }
        }
        let (rebuilt_table, rebuilt) = HeadTable::rebuild(&policy, events.iter());
        prop_assert_eq!(&nt_set(&rebuilt), &live);
        prop_assert_eq!(&nt_set(&all_triples(&t)), &live);
        prop_assert_eq!(rebuilt_table.canonical(), t.canonical());
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// 8b. The same through a real store: each Delta committed in its own transaction leaves the
    /// sink graph holding exactly the rebuilt triples.
    #[test]
    fn p8_deltas_through_a_real_store(frames in prop::collection::vec(raw_frame(5), 1..12)) {
        let policy = policy();
        let events = build_events(&frames);
        let store = Store::new().unwrap();
        let sink = NamedNode::new("urn:t:graph:user:rdf").unwrap();
        let mut t = HeadTable::new();
        for e in &events {
            let d = t.apply(&policy, e);
            apply_delta_commit(&store, &sink, &d).unwrap();
        }
        let dump = |store: &Store| -> BTreeSet<String> {
            store
                .quads_for_pattern(None, None, None, Some(GraphNameRef::NamedNode(sink.as_ref())))
                .map(|q| {
                    let q = q.unwrap();
                    format!("{} {} {}", q.subject, q.predicate, q.object)
                })
                .collect()
        };
        // oxigraph normalises xsd:long/int/short to xsd:integer, so the rebuilt triples go
        // through a store of their own and the two stores are compared as stored.
        let (_, rebuilt) = HeadTable::rebuild(&policy, events.iter());
        let fresh = Store::new().unwrap();
        let all = Delta {
            insert: rebuilt,
            ..Delta::default()
        };
        apply_delta_commit(&fresh, &sink, &all).unwrap();
        prop_assert_eq!(dump(&store), dump(&fresh));
    }
}

// ---------------------------------------------------------------------------
// Corners
// ---------------------------------------------------------------------------

#[test]
fn a_language_tag_is_part_of_the_value() {
    let policy = policy();
    let s = SUBJECTS[0];
    let mut t = HeadTable::new();
    t.apply(
        &policy,
        &ev("oa", 1, 1, &[], vec![set(s, P1, Some(lit("x")))]),
    );
    // the same text under a tag is a different value: the head changes
    let d = t.apply(
        &policy,
        &ev("oa", 2, 2, &[], vec![set(s, P1, Some(term(2)))]),
    );
    assert_eq!(d.remove.len(), 1);
    assert_eq!(d.insert.len(), 1);
    // and back
    let d = t.apply(
        &policy,
        &ev("oa", 3, 3, &[], vec![set(s, P1, Some(lit("x")))]),
    );
    assert_eq!(d.remove.len(), 1);
    assert_eq!(d.insert.len(), 1);
}

#[test]
fn add_wins_survives_a_remove_by_another_origin_that_never_saw_the_add_in_every_order() {
    let policy = policy();
    let s = SUBJECTS[0];
    let v = lit("x");
    let a = ev("oa", 1, 10, &[], vec![add(s, Q1, vec![v.clone()])]);
    let b = ev(
        "ob",
        1,
        20,
        &[synthetic_id("oa", 1).as_str()],
        vec![rem(s, Q1, vec![v.clone()])],
    );
    let c = ev("oc", 1, 30, &[], vec![add(s, Q1, vec![v.clone()])]);
    let all = [&a, &b, &c];
    let perms = [
        [0, 1, 2],
        [0, 2, 1],
        [1, 0, 2],
        [1, 2, 0],
        [2, 0, 1],
        [2, 1, 0],
    ];
    let mut views = Vec::new();
    for perm in perms {
        let mut t = HeadTable::new();
        for i in perm {
            t.apply(&policy, all[i]);
        }
        // b removed a's add; c's add is concurrent with b's removal and stays
        assert!(has(&t, s, Q1, &v), "order {perm:?} lost the concurrent add");
        views.push(t.canonical());
    }
    assert!(views.windows(2).all(|w| w[0] == w[1]));
}

#[test]
fn a_clear_beats_a_late_lower_write_and_a_retract_beats_a_late_lower_set() {
    let policy = policy();
    let s = SUBJECTS[0];
    let clear = ev("ob", 1, 20, &[], vec![set(s, P1, None)]);
    let late = ev("oa", 1, 10, &[], vec![set(s, P1, Some(lit("x")))]);
    let mut t = HeadTable::new();
    t.apply(&policy, &clear);
    let d = t.apply(&policy, &late);
    assert!(d.is_empty());
    assert!(t.triples_of(s).is_empty());

    let gone = ev("ob", 2, 30, &[], vec![retract(s)]);
    let lower = ev("oc", 1, 25, &[], vec![set(s, P2, Some(lit("y")))]);
    let mut t = HeadTable::new();
    t.apply(&policy, &gone);
    let d = t.apply(&policy, &lower);
    assert!(d.is_empty());
    assert!(t.triples_of(s).is_empty());
    assert_eq!(t.owned_subjects().len(), 1);
}

#[test]
fn set_on_a_set_key_replaces_what_it_observed() {
    let policy = policy();
    let s = SUBJECTS[0];
    let mut t = HeadTable::new();
    t.apply(
        &policy,
        &ev("oa", 1, 1, &[], vec![add(s, Q1, vec![lit("x"), lit("y")])]),
    );
    t.apply(
        &policy,
        &ev("ob", 1, 2, &[], vec![add(s, Q1, vec![lit("z")])]),
    );
    // oa replaces: its own earlier values go, ob's concurrent value stays
    t.apply(
        &policy,
        &ev("oa", 2, 3, &[], vec![set(s, Q1, Some(lit("w")))]),
    );
    assert!(!has(&t, s, Q1, &lit("x")));
    assert!(!has(&t, s, Q1, &lit("y")));
    assert!(has(&t, s, Q1, &lit("z")));
    assert!(has(&t, s, Q1, &lit("w")));
    // clearing with no value
    t.apply(&policy, &ev("oa", 3, 4, &[], vec![set(s, Q1, None)]));
    assert!(!has(&t, s, Q1, &lit("w")));
    assert!(has(&t, s, Q1, &lit("z")));
}

#[test]
fn from_ops_policy_reads_the_nidra_shape() {
    // the contractless vocabulary: set is lww, add and remove are add-wins
    let policy = FoldPolicy::from_ops();
    let s = "urn:ao:term/id=1";
    let name = "urn:ao:term#name";
    let tag = "urn:ao:term#tag";
    let class = "urn:ao:term";
    let set_name = |v: &str| Op::Set {
        subject: s.into(),
        class: class.into(),
        predicate: name.into(),
        value: Some(lit(v)),
    };
    let mut t = HeadTable::new();
    t.apply(&policy, &ev("oa", 1, 1, &[], vec![set_name("Customer")]));
    t.apply(&policy, &ev("ob", 1, 2, &[], vec![set_name("Client")]));
    t.apply(
        &policy,
        &ev(
            "oa",
            2,
            3,
            &[],
            vec![Op::Add {
                subject: s.into(),
                class: class.into(),
                predicate: tag.into(),
                values: vec![iri("urn:ao:tag/id=7"), iri("urn:ao:tag/id=8")],
            }],
        ),
    );
    t.apply(
        &policy,
        &ev(
            "oa",
            3,
            4,
            &[],
            vec![Op::Remove {
                subject: s.into(),
                class: class.into(),
                predicate: tag.into(),
                values: vec![iri("urn:ao:tag/id=7")],
            }],
        ),
    );
    let got = nt_set(&t.triples_of(s));
    let want: BTreeSet<String> = [
        format!("<{s}> <{RDF_TYPE}> <{class}>"),
        format!("<{s}> <{name}> {}", lit("Client").as_nt()),
        format!("<{s}> <{tag}> <urn:ao:tag/id=8>"),
    ]
    .into_iter()
    .collect();
    assert_eq!(got, want);
}

#[test]
fn contested_flag_is_local_to_the_head_and_its_challenger() {
    // The flag compares an arriving write with the head only. A chain a <- b <- c is not
    // concurrent when folded in order; this pins that order.
    let policy = policy();
    let s = SUBJECTS[0];
    let a = ev("oa", 1, 1, &["root#0"], vec![set(s, C1, Some(lit("x")))]);
    let b = ev(
        "ob",
        1,
        2,
        &[synthetic_id("oa", 1).as_str()],
        vec![set(s, C1, Some(lit("y")))],
    );
    let c = ev(
        "oc",
        1,
        3,
        &[synthetic_id("ob", 1).as_str()],
        vec![set(s, C1, Some(lit("z")))],
    );
    let t = fold(&policy, &[a, b, c]);
    assert_eq!(t.contested_count(), 0);
}

#[test]
fn equal_values_are_never_contested() {
    let policy = policy();
    let s = SUBJECTS[0];
    let a = ev("oa", 1, 1, &["root#0"], vec![set(s, C1, Some(term(3)))]);
    let b = ev("ob", 1, 2, &["root#0"], vec![set(s, C1, Some(term(4)))]);
    let t = fold(&policy, &[a, b]);
    assert_eq!(t.contested_count(), 0);
}

#[test]
fn delta_names_the_touched_subjects_even_when_nothing_changed() {
    let policy = policy();
    let s = SUBJECTS[0];
    let mut t = HeadTable::new();
    let e = ev("oa", 1, 1, &[], vec![set(s, P1, Some(lit("x")))]);
    t.apply(&policy, &e);
    let d = t.apply(&policy, &e);
    assert!(d.is_empty());
    assert!(d.touched.contains(s));
}

#[test]
fn rebuild_returns_the_triples_of_every_owned_subject() {
    let policy = policy();
    let events = vec![
        ev("oa", 1, 1, &[], vec![set(SUBJECTS[0], P1, Some(lit("x")))]),
        ev("oa", 2, 2, &[], vec![set(SUBJECTS[1], P1, Some(lit("y")))]),
        ev("ob", 1, 3, &[], vec![retract(SUBJECTS[1])]),
    ];
    let (table, triples) = HeadTable::rebuild(&policy, events.iter());
    assert_eq!(table.owned_subjects().len(), 2);
    // subject 0: type + p1; subject 1 retracted: nothing
    assert_eq!(triples.len(), 2);
}
