//! Property tests for the ledger crate (W5.md section 3.6) and three unit tests.
//!
//! Every property runs 64 cases from a fixed ChaCha seed, so a CI failure reproduces locally.
//! To explore other inputs set `LEDGER_PROPTEST_SEED=<u64>` (and `LEDGER_PROPTEST_CASES=<n>` to
//! raise the case count); a failing run prints the minimal shrunk input.

use std::collections::BTreeMap;
use std::fmt::Debug;
use std::fs::OpenOptions;
use std::path::{Path, PathBuf};

use garden_ledger::frame::{decode, encode, FrameError};
use garden_ledger::{
    Draft, Event, Header, Hlc, HlcClock, Ledger, LedgerError, Mode, OriginId, Recovery, Stream,
    StreamError, Subject, VersionVector,
};
use proptest::prelude::*;
use proptest::test_runner::{Config, RngAlgorithm, TestCaseError, TestRng, TestRunner};
use tempfile::TempDir;

// ---------------------------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------------------------

/// Random but reproducible description of one event: everything a `Draft` carries plus the
/// optional signature slot and the wall clock handed to the HLC.
#[derive(Clone, Debug)]
struct Spec {
    witness: String,
    vocab: String,
    subjects: Vec<Subject>,
    mode: Mode,
    payload: Vec<u8>,
    txn: Option<String>,
    sig: Option<String>,
    wall: u64,
}

/// Build a `Draft` from a spec (the spec stays usable, `Draft` is not assumed to be `Clone`).
fn draft(spec: &Spec) -> Draft {
    Draft {
        witness: spec.witness.clone(),
        vocab: spec.vocab.clone(),
        subjects: spec.subjects.clone(),
        mode: spec.mode,
        payload: spec.payload.clone(),
        txn: spec.txn.clone(),
    }
}

/// Build a complete `Event` for `origin` at `seq` from a spec, with a correct payload hash.
fn event(spec: &Spec, origin: &str, seq: u64) -> Event {
    let header = Header {
        v: 1,
        origin: OriginId(origin.to_string()),
        seq,
        hlc: Hlc {
            ms: spec.wall,
            count: (seq % 1_000_000) as u32,
        },
        witness: spec.witness.clone(),
        vocab: spec.vocab.clone(),
        subjects: spec.subjects.clone(),
        mode: spec.mode,
        payload_hash: blake3::hash(&spec.payload).to_hex().to_string(),
        payload_len: spec.payload.len() as u32,
        txn: spec.txn.clone(),
        sig: spec.sig.clone(),
    };
    Event {
        header,
        payload: spec.payload.clone(),
    }
}

/// Open (creating if needed) a ledger in `dir` whose local origin is `origin`.
fn ledger_in(dir: &Path, origin: &str) -> Ledger {
    let (ledger, _recoveries) =
        Ledger::open(dir, OriginId(origin.to_string())).expect("open ledger");
    ledger
}

/// Path of an origin's stream file inside a ledger directory.
fn log_path(dir: &Path, origin: &str) -> PathBuf {
    dir.join(format!("{origin}.log"))
}

/// Wrap any debuggable error as a failing proptest case.
fn fail<E: Debug>(e: E) -> TestCaseError {
    TestCaseError::fail(format!("{e:?}"))
}

/// A deterministic runner: 64 cases (override with `LEDGER_PROPTEST_CASES`), ChaCha seeded from
/// `LEDGER_PROPTEST_SEED` (default fixed), no failure-persistence files.
/// Like `runner` with a different default case count (the env override still wins).
fn runner_n(default_cases: u32) -> TestRunner {
    let seed: u64 = std::env::var("LEDGER_PROPTEST_SEED")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0x1ED6_E420_2611_03);
    let cases: u32 = std::env::var("LEDGER_PROPTEST_CASES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default_cases);
    let mut bytes = [0u8; 32];
    bytes[..8].copy_from_slice(&seed.to_le_bytes());
    let config = Config {
        cases,
        failure_persistence: None,
        ..Config::default()
    };
    TestRunner::new_with_rng(config, TestRng::from_seed(RngAlgorithm::ChaCha, &bytes))
}

/// Run `test` over `strategy` with the deterministic runner, panicking with the shrunk failure.
fn check<S: Strategy>(strategy: S, test: impl Fn(S::Value) -> Result<(), TestCaseError>) {
    check_n(64, strategy, test)
}

/// `check` with a default case count.
fn check_n<S: Strategy>(
    cases: u32,
    strategy: S,
    test: impl Fn(S::Value) -> Result<(), TestCaseError>,
) {
    let mut runner = runner_n(cases);
    if let Err(e) = runner.run(&strategy, test) {
        panic!("{e}");
    }
}

/// Origin ids: `[a-z0-9]{1,8}`.
fn origin_strategy() -> impl Strategy<Value = String> {
    "[a-z0-9]{1,8}"
}

fn mode_strategy() -> impl Strategy<Value = Mode> {
    prop_oneof![
        Just(Mode::Assert),
        Just(Mode::Retract),
        Just(Mode::Supersede),
        Just(Mode::Redact),
    ]
}

/// Subjects: 1..=`max_subjects`, each with 0..=`max_parents` random 64-hex parents.
fn subjects_strategy(
    max_subjects: usize,
    max_parents: usize,
) -> impl Strategy<Value = Vec<Subject>> {
    let one = (
        "[a-z0-9]{1,8}",
        "[A-Za-z]{1,8}",
        prop::collection::vec("[0-9a-f]{64}", 0..=max_parents),
    )
        .prop_map(|(s, c, parents)| Subject {
            subject: format!("urn:s:{s}"),
            class: format!("urn:c:{c}"),
            parents,
        });
    prop::collection::vec(one, 1..=max_subjects)
}

/// A random event spec with bounded payload, subject and parent counts.
fn spec_strategy(
    max_payload: usize,
    max_subjects: usize,
    max_parents: usize,
) -> impl Strategy<Value = Spec> {
    (
        "[a-z0-9]{1,8}",
        "[a-z]{1,6}",
        subjects_strategy(max_subjects, max_parents),
        mode_strategy(),
        prop::collection::vec(any::<u8>(), 0..=max_payload),
        prop::option::of("[a-z0-9]{1,8}"),
        prop::option::of("[a-z0-9]{8}"),
        0u64..(1u64 << 40),
    )
        .prop_map(|(w, v, subjects, mode, payload, txn, sig, wall)| Spec {
            witness: format!("https://w.example/{w}"),
            vocab: format!("urn:v:{v}"),
            subjects,
            mode,
            payload,
            txn,
            sig,
            wall,
        })
}

/// The full-size spec used by most properties: payload 0..4096, 1..4 subjects, 0..2 parents.
fn any_spec() -> impl Strategy<Value = Spec> {
    spec_strategy(4096, 4, 2)
}

/// True when `err` is the gap refusal, whichever way the ledger wraps the stream's `Gap`.
fn is_gap(err: &LedgerError) -> bool {
    format!("{err:?}").contains("Gap")
}

/// Open a stream in `dir`, mapping the error (a `Stream` is not assumed `Debug`).
fn open_stream(dir: &Path, origin: &str) -> Result<(Stream, Recovery), TestCaseError> {
    match Stream::open(dir, &OriginId(origin.to_string())) {
        Ok(pair) => Ok(pair),
        Err(e) => Err(fail(e)),
    }
}

/// Collect every frame of a stream from offset 0.
fn scan_all(stream: &Stream) -> Result<Vec<garden_ledger::FrameAt>, TestCaseError> {
    stream
        .scan_from(0)
        .collect::<Result<Vec<_>, _>>()
        .map_err(fail)
}

// ---------------------------------------------------------------------------------------------
// 1. codec round trip
// ---------------------------------------------------------------------------------------------

#[test]
fn prop1_codec_round_trip() {
    check(
        (any_spec(), origin_strategy(), 1u64..1_000_000),
        |(spec, origin, seq)| {
            let ev = event(&spec, &origin, seq);
            let bytes = encode(&ev).map_err(fail)?;
            let d = decode(&bytes).map_err(fail)?;
            prop_assert_eq!(&d.header, &ev.header);
            prop_assert_eq!(&d.payload, &ev.payload);
            prop_assert!(!d.redacted);
            prop_assert_eq!(d.frame_len, bytes.len());

            // The id is a function of the header alone: a re-encode of the decoded event keeps it.
            let again = Event {
                header: d.header.clone(),
                payload: d.payload.clone(),
            };
            let bytes2 = encode(&again).map_err(fail)?;
            prop_assert_eq!(&bytes, &bytes2);
            let d2 = decode(&bytes2).map_err(fail)?;
            prop_assert_eq!(d2.id, d.id);
            Ok(())
        },
    );
}

// ---------------------------------------------------------------------------------------------
// 2. torn tail at every byte of the last frame
// ---------------------------------------------------------------------------------------------

#[test]
fn prop2_torn_tail_at_every_byte() {
    // The last frame is kept small (about 600 bytes) so the per-byte loop stays fast.
    check_n(
        8,
        (
            prop::collection::vec(spec_strategy(512, 3, 1), 0..4),
            spec_strategy(32, 1, 1),
            origin_strategy(),
        ),
        |(head, last, origin)| {
            let mut specs = head;
            specs.push(last);
            let n = specs.len() as u64;

            let td = TempDir::new().map_err(fail)?;
            let dir = td.path();
            let path = log_path(dir, &origin);
            let last_offset;
            let total;
            {
                let (mut stream, rec) = open_stream(dir, &origin)?;
                prop_assert_eq!(rec, Recovery::Clean);
                let mut off = 0;
                for (i, spec) in specs.iter().enumerate() {
                    let ap = stream
                        .append_event(&event(spec, &origin, i as u64 + 1))
                        .map_err(fail)?;
                    off = ap.offset;
                }
                last_offset = off;
                total = stream.len();
            } // stream dropped before the file is cut
            let original = std::fs::read(&path).map_err(fail)?;
            prop_assert_eq!(original.len() as u64, total);

            // Cutting exactly at the frame boundary leaves a clean n-1 frame file.
            // Cutting at every byte strictly inside the last frame is a torn tail.
            for cut in last_offset..total {
                std::fs::write(&path, &original).map_err(fail)?;
                let f = OpenOptions::new().write(true).open(&path).map_err(fail)?;
                f.set_len(cut).map_err(fail)?;
                drop(f);

                {
                    let (stream, rec) = open_stream(dir, &origin)?;
                    if cut == last_offset {
                        prop_assert_eq!(rec, Recovery::Clean, "cut {}", cut);
                    } else {
                        prop_assert_eq!(
                            rec,
                            Recovery::TruncatedTail {
                                from: last_offset,
                                dropped_bytes: cut - last_offset
                            },
                            "cut {}",
                            cut
                        );
                    }
                    prop_assert_eq!(scan_all(&stream)?.len() as u64, n - 1, "cut {}", cut);
                    prop_assert_eq!(stream.next_seq(), n, "cut {}", cut);
                    prop_assert_eq!(stream.len(), last_offset, "cut {}", cut);
                }

                // An append after recovery yields a file that scans clean with n frames.
                {
                    let (mut stream, _) = open_stream(dir, &origin)?;
                    stream
                        .append_event(&event(&specs[specs.len() - 1], &origin, n))
                        .map_err(fail)?;
                }
                let (stream, rec) = open_stream(dir, &origin)?;
                prop_assert_eq!(rec, Recovery::Clean, "cut {}", cut);
                let frames = scan_all(&stream)?;
                prop_assert_eq!(frames.len() as u64, n, "cut {}", cut);
                for (i, fa) in frames.iter().enumerate() {
                    prop_assert_eq!(fa.decoded.header.seq, i as u64 + 1);
                }
            }
            Ok(())
        },
    );
}

// ---------------------------------------------------------------------------------------------
// 3. corruption mid-file is not a tail
// ---------------------------------------------------------------------------------------------

#[test]
fn prop3_mid_file_corruption_is_corrupt() {
    check(
        (
            prop::collection::vec(any_spec(), 2..6),
            origin_strategy(),
            any::<usize>(),
            any::<usize>(),
            1u8..=255,
        ),
        |(specs, origin, k_sel, pos_sel, mask)| {
            let n = specs.len();
            let td = TempDir::new().map_err(fail)?;
            let dir = td.path();
            let path = log_path(dir, &origin);
            let mut offsets = Vec::new();
            {
                let (mut stream, _) = open_stream(dir, &origin)?;
                for (i, spec) in specs.iter().enumerate() {
                    let ap = stream
                        .append_event(&event(spec, &origin, i as u64 + 1))
                        .map_err(fail)?;
                    offsets.push(ap.offset);
                }
                offsets.push(stream.len());
            }

            // Any frame but the last, so valid bytes follow the damage. Skip the 4-byte length
            // prefix: damage there can make the frame look like an overrun (a tail), not a crc fault.
            let k = k_sel % (n - 1);
            let frame_len = (offsets[k + 1] - offsets[k]) as usize;
            let pos = offsets[k] as usize + 4 + pos_sel % (frame_len - 4);
            let mut bytes = std::fs::read(&path).map_err(fail)?;
            bytes[pos] ^= mask;
            std::fs::write(&path, &bytes).map_err(fail)?;

            match Stream::open(dir, &OriginId(origin.clone())) {
                Err(StreamError::Corrupt { offset, .. }) => {
                    prop_assert_eq!(offset, offsets[k]);
                }
                Err(other) => return Err(fail(other)),
                Ok(_) => return Err(TestCaseError::fail("corrupt file opened")),
            }
            // Refusing to open is not a truncation: the bytes on disk are untouched.
            prop_assert_eq!(std::fs::read(&path).map_err(fail)?, bytes);
            Ok(())
        },
    );
}

// ---------------------------------------------------------------------------------------------
// 4. replay equals
// ---------------------------------------------------------------------------------------------

#[test]
fn prop4_replay_equals() {
    check(
        (prop::collection::vec(any_spec(), 0..8), origin_strategy()),
        |(specs, origin)| {
            let td = TempDir::new().map_err(fail)?;
            let dir = td.path();
            let events: Vec<Event> = specs
                .iter()
                .enumerate()
                .map(|(i, s)| event(s, &origin, i as u64 + 1))
                .collect();
            {
                let (mut stream, _) = open_stream(dir, &origin)?;
                for ev in &events {
                    stream.append_event(ev).map_err(fail)?;
                }
            }
            let (stream, rec) = open_stream(dir, &origin)?;
            prop_assert_eq!(rec, Recovery::Clean);
            prop_assert_eq!(stream.next_seq(), events.len() as u64 + 1);
            let frames = scan_all(&stream)?;
            prop_assert_eq!(frames.len(), events.len());
            for (fa, ev) in frames.iter().zip(&events) {
                prop_assert_eq!(&fa.decoded.header, &ev.header);
                prop_assert_eq!(&fa.decoded.payload, &ev.payload);
                prop_assert!(!fa.decoded.redacted);
            }
            Ok(())
        },
    );
}

// ---------------------------------------------------------------------------------------------
// 5. union commutes
// ---------------------------------------------------------------------------------------------

/// One exchange: `from` ships what `to` lacks (its `diff(to.version())`), `to` applies it.
fn exchange(replicas: &mut [Ledger], from: usize, to: usize) -> Result<(), TestCaseError> {
    let since = replicas[to].version();
    let chunks = replicas[from].diff(&since, 1 << 20).map_err(fail)?;
    for (origin, chunk) in chunks {
        replicas[to].apply(&origin, &chunk.bytes).map_err(fail)?;
    }
    Ok(())
}

#[test]
fn prop5_union_commutes() {
    check(
        (
            2usize..=4,
            prop::collection::vec((any::<usize>(), spec_strategy(1024, 3, 2)), 0..16),
            prop::collection::vec((any::<usize>(), any::<usize>()), 0..12),
        ),
        |(k, appends, pairs)| {
            let td = TempDir::new().map_err(fail)?;
            let names: Vec<String> = (0..k).map(|i| format!("r{i}x")).collect();
            let dirs: Vec<PathBuf> = names
                .iter()
                .map(|n| td.path().join(format!("dir-{n}")))
                .collect();
            let mut replicas: Vec<Ledger> = names
                .iter()
                .zip(&dirs)
                .map(|(n, d)| ledger_in(d, n))
                .collect();
            let mut clocks: Vec<HlcClock> = (0..k).map(|_| HlcClock::new()).collect();

            for (who, spec) in &appends {
                let i = who % k;
                replicas[i]
                    .append(&mut clocks[i], spec.wall, draft(spec))
                    .map_err(fail)?;
            }

            // A random run of exchanges, then top up until every ordered pair met at least twice.
            let mut counts: BTreeMap<(usize, usize), usize> = BTreeMap::new();
            for (a, b) in &pairs {
                let from = a % k;
                let mut to = b % k;
                if to == from {
                    to = (to + 1) % k;
                }
                exchange(&mut replicas, from, to)?;
                *counts.entry((from, to)).or_insert(0) += 1;
            }
            for from in 0..k {
                for to in 0..k {
                    if from == to {
                        continue;
                    }
                    let done = counts.get(&(from, to)).copied().unwrap_or(0);
                    for _ in done..2 {
                        exchange(&mut replicas, from, to)?;
                    }
                }
            }

            let v0 = replicas[0].version();
            for r in &replicas[1..] {
                prop_assert_eq!(&r.version(), &v0);
            }
            for origin in v0.0.keys() {
                let reference = std::fs::read(log_path(&dirs[0], &origin.0)).map_err(fail)?;
                for d in &dirs[1..] {
                    let other = std::fs::read(log_path(d, &origin.0)).map_err(fail)?;
                    prop_assert_eq!(&other, &reference, "origin {}", origin.0);
                }
            }
            Ok(())
        },
    );
}

// ---------------------------------------------------------------------------------------------
// 6. dedup by position
// ---------------------------------------------------------------------------------------------

#[test]
fn prop6_dedup_and_gap() {
    check(
        (
            prop::collection::vec(any_spec(), 3..8),
            origin_strategy(),
            1u64..=2,
        ),
        |(specs, origin, skip)| {
            let td = TempDir::new().map_err(fail)?;
            let origin_a = format!("{origin}a");
            let a_id = OriginId(origin_a.clone());
            let mut a = ledger_in(&td.path().join("a"), &origin_a);
            let mut clock = HlcClock::new();
            for spec in &specs {
                a.append(&mut clock, spec.wall, draft(spec)).map_err(fail)?;
            }
            let n = specs.len();

            let mut b = ledger_in(&td.path().join("b"), "bee");
            let empty = VersionVector(BTreeMap::new());
            let chunks = a.diff(&empty, 1 << 24).map_err(fail)?;
            prop_assert_eq!(chunks.len(), 1);
            prop_assert_eq!(&chunks[0].0, &a_id);
            let bytes = &chunks[0].1.bytes;
            prop_assert_eq!(b.apply(&a_id, bytes).map_err(fail)?, n);
            prop_assert_eq!(b.apply(&a_id, bytes).map_err(fail)?, 0);
            prop_assert_eq!(b.version().get(&a_id), n as u64);

            // A chunk that starts beyond our seq + 1 is a gap, and writes nothing.
            let mut c = ledger_in(&td.path().join("c"), "sea");
            let mut since = BTreeMap::new();
            since.insert(a_id.clone(), skip); // frames from seq skip+1 onward; c holds none
            let late = a.diff(&VersionVector(since), 1 << 24).map_err(fail)?;
            prop_assert_eq!(late.len(), 1);
            match c.apply(&a_id, &late[0].1.bytes) {
                Err(e) => prop_assert!(is_gap(&e), "expected Gap, got {:?}", e),
                Ok(m) => return Err(TestCaseError::fail(format!("gap applied {m} frames"))),
            }
            prop_assert_eq!(c.version().get(&a_id), 0);
            Ok(())
        },
    );
}

// ---------------------------------------------------------------------------------------------
// 7. redaction
// ---------------------------------------------------------------------------------------------

#[test]
fn prop7_redaction() {
    check(
        (
            prop::collection::vec(any_spec(), 1..6),
            any::<usize>(),
            any_spec(),
            origin_strategy(),
        ),
        |(specs, pick, extra, origin)| {
            let td = TempDir::new().map_err(fail)?;
            let dir = td.path().join("led");
            let oid = OriginId(origin.clone());
            let mut ledger = ledger_in(&dir, &origin);
            let mut clock = HlcClock::new();
            for spec in &specs {
                ledger
                    .append(&mut clock, spec.wall, draft(spec))
                    .map_err(fail)?;
            }
            let path = log_path(&dir, &origin);
            let len_before = std::fs::metadata(&path).map_err(fail)?.len();

            let before: Vec<garden_ledger::FrameAt> = ledger
                .scan(&oid, 0)
                .map_err(fail)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(fail)?;
            prop_assert_eq!(before.len(), specs.len());
            let idx = pick % specs.len();
            let target_offset = before[idx].offset;
            let target_id = before[idx].decoded.id;

            ledger.redact(&oid, target_offset).map_err(fail)?;
            prop_assert_eq!(std::fs::metadata(&path).map_err(fail)?.len(), len_before);

            // A payload that is empty or already all zero is unchanged by zeroing, so its hash
            // still matches and it does not read as redacted.
            let visible = |i: usize| specs[i].payload.iter().any(|b| *b != 0);
            let check_scan = |ledger: &Ledger, total: usize| -> Result<(), TestCaseError> {
                let frames = ledger
                    .scan(&oid, 0)
                    .map_err(fail)?
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(fail)?;
                prop_assert_eq!(frames.len(), total);
                for (i, fa) in frames.iter().enumerate() {
                    prop_assert_eq!(fa.decoded.header.seq, i as u64 + 1);
                    if i == idx {
                        prop_assert_eq!(fa.decoded.id, target_id);
                        prop_assert_eq!(fa.decoded.redacted, visible(i));
                        prop_assert!(fa.decoded.payload.iter().all(|b| *b == 0));
                    } else if i < specs.len() {
                        prop_assert!(!fa.decoded.redacted);
                        prop_assert_eq!(&fa.decoded.payload, &specs[i].payload);
                        prop_assert_eq!(fa.decoded.id, before[i].decoded.id);
                    }
                }
                Ok(())
            };
            check_scan(&ledger, specs.len())?;

            // A later append still scans clean, also after a reopen.
            ledger
                .append(&mut clock, extra.wall, draft(&extra))
                .map_err(fail)?;
            check_scan(&ledger, specs.len() + 1)?;
            drop(ledger);
            let (reopened, recoveries) = Ledger::open(&dir, oid.clone()).map_err(fail)?;
            prop_assert!(recoveries.iter().all(|(_, r)| *r == Recovery::Clean));
            check_scan(&reopened, specs.len() + 1)?;
            Ok(())
        },
    );
}

// ---------------------------------------------------------------------------------------------
// 8. HLC monotone
// ---------------------------------------------------------------------------------------------

#[test]
fn prop8_hlc_strictly_monotone() {
    // (is_observe, local wall clock, remote ms, remote count): wall clocks jump freely.
    let op = (
        any::<bool>(),
        0u64..(1u64 << 40),
        0u64..(1u64 << 40),
        0u32..(1u32 << 20),
    );
    check(prop::collection::vec(op, 1..64), |ops| {
        let mut clock = HlcClock::new();
        let mut prev: Option<Hlc> = None;
        for (observe, wall, rms, rcount) in ops {
            let h = if observe {
                clock.observe(
                    Hlc {
                        ms: rms,
                        count: rcount,
                    },
                    wall,
                )
            } else {
                clock.now(wall)
            };
            if let Some(p) = prev {
                prop_assert!(h > p, "{:?} not after {:?}", h, p);
            }
            prev = Some(h);
        }
        Ok(())
    });
}

// ---------------------------------------------------------------------------------------------
// unit tests
// ---------------------------------------------------------------------------------------------

#[test]
fn unit_origin_id_validation() {
    let td = TempDir::new().unwrap();
    let accepts = |name: &str| Stream::open(td.path(), &OriginId(name.to_string())).is_ok();
    assert!(accepts("a"));
    assert!(accepts("Node_1.v2-x"));
    assert!(accepts(&"z".repeat(128)));
    assert!(!accepts(""));
    assert!(!accepts(&"z".repeat(129)));
    assert!(!accepts("a/b"));
    assert!(!accepts(".."));
    assert!(!accepts("has space"));
    assert!(!accepts("nul\0"));
}

#[test]
fn unit_version_vector_dominates_and_merge() {
    let o = |s: &str| OriginId(s.to_string());
    let vv = |pairs: &[(&str, u64)]| {
        VersionVector(
            pairs
                .iter()
                .map(|(k, v)| (o(k), *v))
                .collect::<BTreeMap<_, _>>(),
        )
    };
    let a = vv(&[("a", 3), ("b", 1)]);
    let b = vv(&[("a", 2)]);
    let c = vv(&[("b", 5)]);
    assert_eq!(a.get(&o("a")), 3);
    assert_eq!(a.get(&o("missing")), 0);
    assert!(a.dominates(&b));
    assert!(!b.dominates(&a));
    assert!(a.dominates(&a));
    assert!(a.dominates(&vv(&[])));
    assert!(!a.dominates(&c));
    assert!(!c.dominates(&a));

    let mut m = a;
    m.merge(&c);
    m.merge(&b);
    assert_eq!(m.get(&o("a")), 3);
    assert_eq!(m.get(&o("b")), 5);
    assert!(m.dominates(&c));
    assert!(m.dominates(&b));
    let mut idem = vv(&[("a", 3), ("b", 5)]);
    idem.merge(&m);
    assert_eq!(idem, m);
}

#[test]
fn unit_too_large() {
    // A body over 64 MiB is refused by decode before anything else is believed.
    let spec = Spec {
        witness: "https://w.example/big".into(),
        vocab: "urn:v:big".into(),
        subjects: vec![Subject {
            subject: "urn:s:x".into(),
            class: "urn:c:X".into(),
            parents: vec![],
        }],
        mode: Mode::Assert,
        payload: vec![7u8; (64 << 20) + 1],
        txn: None,
        sig: None,
        wall: 1,
    };
    match encode(&event(&spec, "big", 1)).and_then(|b| decode(&b).map(|_| ())) {
        Err(FrameError::TooLarge(_)) => {}
        Err(other) => panic!("expected TooLarge, got {other:?}"),
        Ok(_) => panic!("oversize frame decoded"),
    }
}
