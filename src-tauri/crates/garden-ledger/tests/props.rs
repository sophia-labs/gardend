//! Property tests for the ledger crate (W5.md section 3.6) and three unit tests.
//!
//! Every property runs 32 cases (8 for the per-byte torn-tail and torn-redaction ones) from a fixed ChaCha seed, so a CI failure reproduces locally.
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
        mode: spec.mode.clone(),
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
        mode: spec.mode.clone(),
        payload_hash: blake3::hash(&spec.payload).to_hex().to_string(),
        payload_len: spec.payload.len() as u32,
        txn: spec.txn.clone(),
        sig: spec.sig.clone(),
        extra: BTreeMap::new(),
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

/// A deterministic runner: 32 cases (override with `LEDGER_PROPTEST_CASES`), ChaCha seeded from
/// `LEDGER_PROPTEST_SEED` (default fixed), no failure-persistence files.
/// Like `runner` with a different default case count (the env override still wins).
fn runner_n(default_cases: u32) -> TestRunner {
    let seed: u64 = std::env::var("LEDGER_PROPTEST_SEED")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0x001E_D6E4_2026_1103);
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
    check_n(32, strategy, test)
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
        Just(Mode::Other("commit".to_string())),
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
            prop_assert!(!d.is_redacted());
            prop_assert_eq!(d.frame_len, bytes.len());

            // The id is blake3 of the header bytes as stored, found by the length fields.
            let hlen = u16::from_le_bytes([bytes[4], bytes[5]]) as usize;
            prop_assert_eq!(d.id, *blake3::hash(&bytes[6..6 + hlen]).as_bytes());

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

            // Any byte of any frame but the last, the 4-byte length prefix included: valid frames
            // follow the damage, so it is corruption at that frame's offset, never a cut tail.
            let k = k_sel % (n - 1);
            let frame_len = (offsets[k + 1] - offsets[k]) as usize;
            let pos = offsets[k] as usize + pos_sel % frame_len;
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
                prop_assert!(!fa.decoded.is_redacted());
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
    // Only frames a barrier has covered are shipped.
    replicas[from].sync_barrier().map_err(fail)?;
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

            a.sync_barrier().map_err(fail)?;
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
            ledger.sync_barrier().map_err(fail)?;
            let path = log_path(&dir, &origin);
            let len_before = std::fs::metadata(&path).map_err(fail)?.len();

            let before: Vec<garden_ledger::FrameAt> = ledger
                .scan(&oid, 0)
                .map_err(fail)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(fail)?;
            prop_assert_eq!(before.len(), specs.len());
            let idx = pick % specs.len();
            let target_id = before[idx].decoded.id;

            let redact_frame = ledger
                .redact(
                    &mut clock,
                    1,
                    "urn:w:redactor".into(),
                    "urn:v:r".into(),
                    &oid,
                    idx as u64 + 1,
                )
                .map_err(fail)?;
            // The redaction is a frame appended after the others; the target keeps its length.
            prop_assert_eq!(redact_frame.seq, specs.len() as u64 + 1);
            prop_assert_eq!(redact_frame.offset, len_before);
            prop_assert!(std::fs::metadata(&path).map_err(fail)?.len() > len_before);

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
                        prop_assert_eq!(fa.decoded.is_redacted(), visible(i));
                        prop_assert_eq!(fa.decoded.frame_len, before[i].decoded.frame_len);
                        prop_assert!(fa.decoded.payload.iter().all(|b| *b == 0));
                    } else if i < specs.len() {
                        prop_assert!(!fa.decoded.is_redacted());
                        prop_assert_eq!(&fa.decoded.payload, &specs[i].payload);
                        prop_assert_eq!(fa.decoded.id, before[i].decoded.id);
                    } else if i == specs.len() {
                        prop_assert_eq!(&fa.decoded.header.mode, &Mode::Redact);
                        let r = garden_ledger::RedactPayload::from_bytes(&fa.decoded.payload)
                            .map_err(fail)?;
                        prop_assert_eq!(&r.target_origin, &origin);
                        prop_assert_eq!(r.target_seq, idx as u64 + 1);
                        prop_assert_eq!(r.id().map_err(fail)?, target_id);
                    }
                }
                Ok(())
            };
            check_scan(&ledger, specs.len() + 1)?;

            // A later append still scans clean, also after a reopen.
            ledger
                .append(&mut clock, extra.wall, draft(&extra))
                .map_err(fail)?;
            check_scan(&ledger, specs.len() + 2)?;
            drop(ledger);
            let (reopened, recoveries) = Ledger::open(&dir, oid.clone()).map_err(fail)?;
            prop_assert!(recoveries.iter().all(|(_, r)| *r == Recovery::Clean));
            check_scan(&reopened, specs.len() + 2)?;
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
// 9. a crash that cuts a tail a peer already pulled recovers without a fork
// ---------------------------------------------------------------------------------------------

/// A appends `specs`, B pulls everything, A "crashes" and loses its last frame (the file is cut at the
/// frame boundary and reopened). Returns A (version n-1), B and the paths.
struct Crashed {
    _td: TempDir,
    a: Ledger,
    b: Ledger,
    a_id: OriginId,
    a_dir: PathBuf,
    b_dir: PathBuf,
    n: u64,
}

fn crash_after_pull(specs: &[Spec]) -> Result<Crashed, TestCaseError> {
    let td = TempDir::new().map_err(fail)?;
    let a_dir = td.path().join("a");
    let b_dir = td.path().join("b");
    let a_id = OriginId("aaa".to_string());
    let mut a = ledger_in(&a_dir, "aaa");
    let mut b = ledger_in(&b_dir, "bbb");
    let mut clock = HlcClock::new();
    for spec in specs {
        a.append(&mut clock, spec.wall, draft(spec)).map_err(fail)?;
    }
    let last_offset = a.stream(&a_id).and_then(|s| s.last_offset()).unwrap();
    let mut pair = [a, b];
    exchange(&mut pair, 0, 1)?;
    let [a, b_taken] = pair;
    b = b_taken;
    b.sync_barrier().map_err(fail)?;
    prop_assert_eq_ok(b.version().get(&a_id), specs.len() as u64)?;
    drop(a);
    let f = OpenOptions::new()
        .write(true)
        .open(log_path(&a_dir, "aaa"))
        .map_err(fail)?;
    f.set_len(last_offset).map_err(fail)?;
    drop(f);
    let (a, recs) = Ledger::open(&a_dir, a_id.clone()).map_err(fail)?;
    prop_assert_eq_ok(recs.iter().all(|(_, r)| *r == Recovery::Clean), true)?;
    prop_assert_eq_ok(a.version().get(&a_id), specs.len() as u64 - 1)?;
    Ok(Crashed {
        _td: td,
        a,
        b,
        a_id,
        a_dir,
        b_dir,
        n: specs.len() as u64,
    })
}

fn prop_assert_eq_ok<T: PartialEq + Debug>(l: T, r: T) -> Result<(), TestCaseError> {
    if l == r {
        Ok(())
    } else {
        Err(TestCaseError::fail(format!("{l:?} != {r:?}")))
    }
}

#[test]
fn prop9_crash_cut_after_peer_pull_recovers_without_fork() {
    check(
        (prop::collection::vec(any_spec(), 2..6), any_spec()),
        |(specs, alt)| {
            // Mirror: A pulls its own lost frame back from B, accepted, byte-equal to B's copy.
            {
                let mut c = crash_after_pull(&specs)?;
                let chunks = c.b.diff(&c.a.version(), 1 << 20).map_err(fail)?;
                prop_assert_eq_ok(chunks.len(), 1)?;
                prop_assert_eq_ok(&chunks[0].0, &c.a_id)?;
                prop_assert_eq_ok(chunks[0].1.from_seq, c.n)?;
                let applied = c.a.apply(&c.a_id, &chunks[0].1.bytes).map_err(fail)?;
                prop_assert_eq_ok(applied, 1)?;
                prop_assert_eq_ok(c.a.version().get(&c.a_id), c.n)?;
                let mine = std::fs::read(log_path(&c.a_dir, "aaa")).map_err(fail)?;
                let theirs = std::fs::read(log_path(&c.b_dir, "aaa")).map_err(fail)?;
                prop_assert_eq_ok(mine, theirs)?;
                // The recovered origin carries on from where it was.
                let mut clock = HlcClock::new();
                let next =
                    c.a.append(&mut clock, alt.wall, draft(&alt))
                        .map_err(fail)?;
                prop_assert_eq_ok(next.seq, c.n + 1)?;
                // Pulling it again is a duplicate, not a fork.
                let again = c.b.diff(&c.a.version(), 1 << 20).map_err(fail)?;
                prop_assert_eq_ok(again.len(), 0)?;
            }
            // Fork: A re-appends a different event at the lost seq; B refuses it and writes nothing.
            {
                let mut c = crash_after_pull(&specs)?;
                let mut other = alt.clone();
                other.witness = format!("{}/fork", specs[specs.len() - 1].witness);
                let mut clock = HlcClock::new();
                c.a.append(&mut clock, other.wall, draft(&other))
                    .map_err(fail)?;
                c.a.sync_barrier().map_err(fail)?;
                let b_file = std::fs::read(log_path(&c.b_dir, "aaa")).map_err(fail)?;
                // B holds seq n already (its version says n, so a plain diff offers nothing): a chunk
                // from further back overlaps B's frames and is compared by id.
                let offered =
                    c.a.diff(&VersionVector(BTreeMap::new()), 1 << 24)
                        .map_err(fail)?;
                match c.b.apply(&c.a_id, &offered[0].1.bytes) {
                    Err(LedgerError::Fork {
                        origin,
                        seq,
                        ours,
                        theirs,
                    }) => {
                        prop_assert_eq_ok(origin, c.a_id.clone())?;
                        prop_assert_eq_ok(seq, c.n)?;
                        prop_assert!(ours != theirs);
                    }
                    other => {
                        return Err(TestCaseError::fail(format!("expected Fork, got {other:?}")))
                    }
                }
                prop_assert_eq_ok(
                    std::fs::read(log_path(&c.b_dir, "aaa")).map_err(fail)?,
                    b_file,
                )?;
                prop_assert_eq_ok(c.b.version().get(&c.a_id), c.n)?;
            }
            Ok(())
        },
    );
}

// ---------------------------------------------------------------------------------------------
// 10. a redaction that tears at any byte is repaired on open
// ---------------------------------------------------------------------------------------------

/// With the files as a finished redaction left them, restore the target's payload region byte by byte
/// from `region_before` (the torn states of an in-place write that applied only its first `j` bytes),
/// open the ledger at each, and require the repair to reproduce the finished files exactly.
fn torn_redaction_loop(
    dir: &Path,
    local: &OriginId,
    target: &OriginId,
    target_offset: u64,
    region_before: &[u8],
) -> Result<(), TestCaseError> {
    let path = log_path(dir, &target.0);
    let after = std::fs::read(&path).map_err(fail)?;
    let off = target_offset as usize;
    let hlen = u16::from_le_bytes([after[off + 4], after[off + 5]]) as usize;
    let rs = off + 6 + hlen + 4;
    let frame_len =
        4 + u32::from_le_bytes([after[off], after[off + 1], after[off + 2], after[off + 3]])
            as usize;
    let re = off + frame_len;
    prop_assert_eq_ok(re - rs, region_before.len())?;
    for j in 0..=(re - rs) {
        let mut torn = after.clone();
        torn[rs + j..re].copy_from_slice(&region_before[j..]);
        std::fs::write(&path, &torn).map_err(fail)?;
        let (ledger, recs) = Ledger::open(dir, local.clone()).map_err(fail)?;
        let rec = recs
            .iter()
            .find(|(o, _)| o == target)
            .map(|(_, r)| r.clone());
        let expected = if torn == after {
            Recovery::Clean
        } else {
            Recovery::Repaired { redactions: 1 }
        };
        prop_assert_eq_ok(rec, Some(expected))?;
        let frames: Vec<_> = ledger
            .scan(target, 0)
            .map_err(fail)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(fail)?;
        let hit = frames.iter().find(|f| f.offset == target_offset).unwrap();
        prop_assert!(hit.decoded.is_redacted(), "j={}", j);
        prop_assert_eq_ok(hit.decoded.frame_len, frame_len)?;
        drop(ledger);
        prop_assert!(std::fs::read(&path).map_err(fail)? == after, "j={}", j);
    }
    Ok(())
}

/// A payload with at least one non-zero byte, so redaction is observable.
fn visible_spec(max_payload: usize) -> impl Strategy<Value = Spec> {
    spec_strategy(max_payload, 2, 1).prop_map(|mut s| {
        s.payload.push(0xA5);
        s
    })
}

#[test]
fn prop10_torn_redaction_at_every_byte_repairs_on_open() {
    check_n(
        8,
        (
            prop::collection::vec(visible_spec(64), 1..5),
            any::<usize>(),
            origin_strategy(),
        ),
        |(specs, pick, origin)| {
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
            ledger.sync_full().map_err(fail)?;
            let idx = pick % specs.len();
            let loc = ledger
                .stream(&oid)
                .unwrap()
                .locate(idx as u64 + 1)
                .map_err(fail)?
                .unwrap();
            let before = std::fs::read(log_path(&dir, &origin)).map_err(fail)?;
            ledger
                .redact(
                    &mut clock,
                    9,
                    "urn:w:r".into(),
                    "urn:v:r".into(),
                    &oid,
                    idx as u64 + 1,
                )
                .map_err(fail)?;
            drop(ledger);
            let hlen = u16::from_le_bytes([
                before[loc.offset as usize + 4],
                before[loc.offset as usize + 5],
            ]) as usize;
            let rs = loc.offset as usize + 6 + hlen + 4;
            let re = (loc.offset + loc.frame_len) as usize;
            torn_redaction_loop(&dir, &oid, &oid, loc.offset, &before[rs..re])
        },
    );
}

#[test]
fn torn_redaction_of_a_peers_last_frame_is_repaired_not_cut() {
    // The redact frame lives in the local stream; its target is the tail frame of a peer's stream.
    // A torn zeroing there fails the peer file's payload crc at its very end, which a plain scan
    // would cut. The ledger repairs it first.
    check_n(
        4,
        (
            prop::collection::vec(visible_spec(48), 1..4),
            origin_strategy(),
        ),
        |(specs, origin)| {
            let td = TempDir::new().map_err(fail)?;
            let peer = format!("{origin}p");
            let peer_id = OriginId(peer.clone());
            let local_id = OriginId(format!("{origin}l"));
            let mut p = ledger_in(&td.path().join("p"), &peer);
            let mut pclock = HlcClock::new();
            for spec in &specs {
                p.append(&mut pclock, spec.wall, draft(spec))
                    .map_err(fail)?;
            }
            p.sync_barrier().map_err(fail)?;
            let dir = td.path().join("l");
            let mut l = ledger_in(&dir, &local_id.0);
            for (o, chunk) in p
                .diff(&VersionVector(BTreeMap::new()), 1 << 24)
                .map_err(fail)?
            {
                l.apply(&o, &chunk.bytes).map_err(fail)?;
            }
            l.sync_full().map_err(fail)?;
            let seq = specs.len() as u64;
            let loc = l
                .stream(&peer_id)
                .unwrap()
                .locate(seq)
                .map_err(fail)?
                .unwrap();
            prop_assert_eq_ok(
                loc.offset + loc.frame_len,
                l.stream(&peer_id).unwrap().len(),
            )?;
            let before = std::fs::read(log_path(&dir, &peer)).map_err(fail)?;
            let mut lclock = HlcClock::new();
            l.redact(
                &mut lclock,
                5,
                "urn:w:r".into(),
                "urn:v:r".into(),
                &peer_id,
                seq,
            )
            .map_err(fail)?;
            drop(l);
            let hlen = u16::from_le_bytes([
                before[loc.offset as usize + 4],
                before[loc.offset as usize + 5],
            ]) as usize;
            let rs = loc.offset as usize + 6 + hlen + 4;
            let re = (loc.offset + loc.frame_len) as usize;
            torn_redaction_loop(&dir, &local_id, &peer_id, loc.offset, &before[rs..re])
        },
    );
}

// ---------------------------------------------------------------------------------------------
// 11. a zero-filled tail is cut
// ---------------------------------------------------------------------------------------------

#[test]
fn prop11_zero_filled_tail_is_cut() {
    check(
        (
            prop::collection::vec(any_spec(), 0..5),
            origin_strategy(),
            1usize..300,
        ),
        |(specs, origin, zeros)| {
            let td = TempDir::new().map_err(fail)?;
            let dir = td.path();
            let path = log_path(dir, &origin);
            {
                let (mut stream, _) = open_stream(dir, &origin)?;
                for (i, spec) in specs.iter().enumerate() {
                    stream
                        .append_event(&event(spec, &origin, i as u64 + 1))
                        .map_err(fail)?;
                }
            }
            let good = std::fs::read(&path).map_err(fail)?;
            let mut padded = good.clone();
            padded.resize(padded.len() + zeros, 0);
            std::fs::write(&path, &padded).map_err(fail)?;
            let (stream, rec) = open_stream(dir, &origin)?;
            prop_assert_eq!(
                rec,
                Recovery::TruncatedTail {
                    from: good.len() as u64,
                    dropped_bytes: zeros as u64
                }
            );
            prop_assert_eq!(stream.next_seq(), specs.len() as u64 + 1);
            prop_assert_eq!(std::fs::read(&path).map_err(fail)?, good);
            Ok(())
        },
    );
}

// ---------------------------------------------------------------------------------------------
// 12. a barrier owes a full flush, and sync_full issues it on every stream that appended
// ---------------------------------------------------------------------------------------------

#[test]
fn prop12_barrier_then_full_is_issued_on_every_stream_that_appended() {
    check(
        (
            prop::collection::vec(any_spec(), 1..4),
            prop::collection::vec(any_spec(), 1..4),
            prop::collection::vec(any_spec(), 1..3),
        ),
        |(local_specs, peer_specs, quiet_specs)| {
            let td = TempDir::new().map_err(fail)?;
            let l_id = OriginId("lll".to_string());
            let p_id = OriginId("ppp".to_string());
            let q_id = OriginId("qqq".to_string());
            let mut l = ledger_in(&td.path().join("l"), "lll");
            let mut clock = HlcClock::new();
            let flags = |l: &Ledger, o: &OriginId| {
                let s = l.stream(o).unwrap();
                (s.dirty_since_barrier(), s.full_owed())
            };

            // Two peers' frames arrive by apply; settle one of them completely.
            for (name, specs) in [("ppp", &peer_specs), ("qqq", &quiet_specs)] {
                let mut peer = ledger_in(&td.path().join(name), name);
                let mut pc = HlcClock::new();
                for spec in specs {
                    peer.append(&mut pc, spec.wall, draft(spec)).map_err(fail)?;
                }
                peer.sync_barrier().map_err(fail)?;
                for (o, chunk) in peer.diff(&l.version(), 1 << 24).map_err(fail)? {
                    l.apply(&o, &chunk.bytes).map_err(fail)?;
                }
            }
            l.sync_barrier().map_err(fail)?;
            l.sync_full().map_err(fail)?;
            for o in [&l_id, &p_id, &q_id] {
                prop_assert_eq!(flags(&l, o), (false, false));
            }

            // Appends to the local stream and to ppp (a second batch), none to qqq.
            for spec in &local_specs {
                l.append(&mut clock, spec.wall, draft(spec)).map_err(fail)?;
            }
            let mut peer = ledger_in(&td.path().join("ppp"), "ppp");
            let mut pc = HlcClock::new();
            for spec in &peer_specs {
                peer.append(&mut pc, spec.wall, draft(spec)).map_err(fail)?;
            }
            for spec in &local_specs {
                peer.append(&mut pc, spec.wall, draft(spec)).map_err(fail)?;
            }
            peer.sync_barrier().map_err(fail)?;
            for (o, chunk) in peer.diff(&l.version(), 1 << 24).map_err(fail)? {
                l.apply(&o, &chunk.bytes).map_err(fail)?;
            }
            prop_assert_eq!(flags(&l, &l_id), (true, false));
            prop_assert_eq!(flags(&l, &p_id), (true, false));
            prop_assert_eq!(flags(&l, &q_id), (false, false));

            l.sync_barrier().map_err(fail)?;
            prop_assert_eq!(flags(&l, &l_id), (false, true));
            prop_assert_eq!(flags(&l, &p_id), (false, true));
            prop_assert_eq!(flags(&l, &q_id), (false, false));
            for o in [&l_id, &p_id] {
                let s = l.stream(o).unwrap();
                prop_assert_eq!(s.synced_len(), s.len());
            }

            // The barrier did not discharge the debt: the full flush clears every stream that had it.
            l.sync_full().map_err(fail)?;
            for o in [&l_id, &p_id, &q_id] {
                prop_assert_eq!(flags(&l, o), (false, false));
            }
            Ok(())
        },
    );
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

fn simple_spec(tag: &str) -> Spec {
    Spec {
        witness: "https://w.example/unit".into(),
        vocab: "urn:v:unit".into(),
        subjects: vec![Subject {
            subject: format!("urn:s:{tag}"),
            class: "urn:c:Unit".into(),
            parents: vec![],
        }],
        mode: Mode::Assert,
        payload: format!("[{{\"tag\":\"{tag}\"}}]").into_bytes(),
        txn: None,
        sig: None,
        wall: 1_000,
    }
}

#[test]
fn unit_a_peers_redact_frame_zeroes_the_local_copy_after_a_barrier() {
    let td = TempDir::new().unwrap();
    let p_id = OriginId("ppp".to_string());
    let mut p = ledger_in(&td.path().join("p"), "ppp");
    let mut l = ledger_in(&td.path().join("l"), "lll");
    let mut pc = HlcClock::new();
    for tag in ["one", "two", "three"] {
        p.append(&mut pc, 1, draft(&simple_spec(tag))).unwrap();
    }
    p.sync_barrier().unwrap();
    for (o, c) in p.diff(&l.version(), 1 << 20).unwrap() {
        l.apply(&o, &c.bytes).unwrap();
    }
    let target = l.stream(&p_id).unwrap().locate(2).unwrap().unwrap();

    p.redact(&mut pc, 2, "urn:w:r".into(), "urn:v:unit".into(), &p_id, 2)
        .unwrap();
    p.sync_barrier().unwrap();
    for (o, c) in p.diff(&l.version(), 1 << 20).unwrap() {
        assert_eq!(l.apply(&o, &c.bytes).unwrap(), 1);
    }
    let frames: Vec<_> = l
        .scan(&p_id, 0)
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(frames.len(), 4);
    assert!(frames[1].decoded.is_redacted());
    assert_eq!(frames[1].decoded.id, target.id);
    assert!(!frames[0].decoded.is_redacted() && !frames[2].decoded.is_redacted());
    // The local copy now equals the origin's: byte for byte.
    let (a, b) = (
        std::fs::read(log_path(&td.path().join("p"), "ppp")).unwrap(),
        std::fs::read(log_path(&td.path().join("l"), "ppp")).unwrap(),
    );
    assert_eq!(a, b);
    // A redaction naming an event that is not there is an error and appends nothing.
    let before = p.version();
    assert!(matches!(
        p.redact(&mut pc, 3, "urn:w:r".into(), "urn:v:unit".into(), &p_id, 99),
        Err(LedgerError::NoSuchFrame { .. })
    ));
    assert_eq!(p.version(), before);
}

#[test]
fn unit_apply_refuses_a_chunk_that_does_not_validate_and_writes_nothing() {
    let td = TempDir::new().unwrap();
    let a_id = OriginId("aaa".to_string());
    let mut a = ledger_in(&td.path().join("a"), "aaa");
    let mut c = HlcClock::new();
    for tag in ["x", "y", "z"] {
        a.append(&mut c, 1, draft(&simple_spec(tag))).unwrap();
    }
    a.sync_barrier().unwrap();
    let chunk = a
        .diff(&VersionVector(BTreeMap::new()), 1 << 20)
        .unwrap()
        .remove(0)
        .1;
    let mut bad = chunk.bytes.clone();
    let n = bad.len();
    bad[n - 6] ^= 1; // payload of the last frame
    let mut b = ledger_in(&td.path().join("b"), "bbb");
    assert!(b.apply(&a_id, &bad).is_err());
    assert_eq!(b.version().get(&a_id), 0);
    assert!(!log_path(&td.path().join("b"), "aaa").exists());
    // A chunk with a hole in its seqs is a gap, wherever it starts.
    let first_len = 4 + u32::from_le_bytes(chunk.bytes[..4].try_into().unwrap()) as usize;
    let mut holed = chunk.bytes[..first_len].to_vec();
    let second_len =
        4 + u32::from_le_bytes(chunk.bytes[first_len..first_len + 4].try_into().unwrap()) as usize;
    holed.extend_from_slice(&chunk.bytes[first_len + second_len..]);
    assert!(is_gap(&b.apply(&a_id, &holed).unwrap_err()));
    assert_eq!(b.apply(&a_id, &chunk.bytes).unwrap(), 3);
}

#[test]
fn unit_unknown_mode_and_fields_survive_open_sync_and_reopen() {
    // A frame a newer build wrote (mode "commit", header field "future") must open, replicate and
    // reopen on this build, with its id unchanged.
    let td = TempDir::new().unwrap();
    let a_id = OriginId("aaa".to_string());
    let dir_a = td.path().join("a");
    let (mut s, _) = Stream::open(&dir_a, &a_id).unwrap();
    let mut spec = simple_spec("future");
    spec.mode = Mode::Other("commit".into());
    let mut ev = event(&spec, "aaa", 1);
    ev.header
        .extra
        .insert("future".into(), serde_json::json!(1));
    let ap = s.append_event(&ev).unwrap();
    s.sync_barrier().unwrap();
    drop(s);

    let (mut a, recs) = Ledger::open(&dir_a, a_id.clone()).unwrap();
    assert!(recs.iter().all(|(_, r)| *r == Recovery::Clean));
    let mut b = ledger_in(&td.path().join("b"), "bbb");
    for (o, c) in a.diff(&b.version(), 1 << 20).unwrap() {
        assert_eq!(b.apply(&o, &c.bytes).unwrap(), 1);
    }
    let f = b.scan(&a_id, 0).unwrap().next().unwrap().unwrap();
    assert_eq!(f.decoded.id, ap.id);
    assert_eq!(f.decoded.header.mode, Mode::Other("commit".into()));
    assert_eq!(f.decoded.header.extra["future"], serde_json::json!(1));
    // The local origin keeps appending after such a frame.
    a.append(&mut HlcClock::new(), 1, draft(&simple_spec("next")))
        .unwrap();
    assert_eq!(a.version().get(&a_id), 2);
}

#[test]
fn unit_header_past_the_u16_bound_is_too_large() {
    let mut spec = simple_spec("big");
    spec.subjects[0].parents = vec!["b".repeat(64); 1000];
    assert!(matches!(
        encode(&event(&spec, "big", 1)),
        Err(FrameError::TooLarge(_))
    ));
}
