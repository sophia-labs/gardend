//! W6 lane C: the commit mode, transaction runs, the three W5c minors and the local origin file.
//!
//! Every test names, in its doc comment, the mutation that turns it red. All of them use the public
//! API only, on real directories.

use std::path::Path;

use garden_ledger::{
    read_or_mint_origin, Draft, HlcClock, Ledger, LedgerError, Mode, OriginId, PayloadState,
    RedactPayload, StreamError, TxnRun, ORIGIN_FILE,
};
use tempfile::TempDir;

fn o(s: &str) -> OriginId {
    OriginId(s.to_string())
}

fn open(dir: &Path, origin: &str) -> Ledger {
    Ledger::open(dir, o(origin)).expect("open ledger").0
}

fn assert_draft(txn: Option<&str>) -> Draft {
    Draft {
        witness: "urn:w".into(),
        vocab: "v".into(),
        subjects: Vec::new(),
        mode: Mode::Assert,
        payload: br#"[{"s":"urn:s","p":"urn:p","o":"x"}]"#.to_vec(),
        txn: txn.map(str::to_string),
    }
}

fn commit_draft(txn: &str) -> Draft {
    Draft::commit(txn).with_witness("urn:w").with_vocab("v")
}

/// Make everything `from` holds durable and apply what `to` lacks, returning frames newly written.
fn ship(from: &mut Ledger, to: &mut Ledger) -> usize {
    from.sync_full().unwrap();
    let mut n = 0;
    for (origin, chunk) in from.diff(&to.version(), 1 << 20).unwrap() {
        n += to.apply(&origin, &chunk.bytes).unwrap();
    }
    n
}

fn run(txn: &str, first_seq: u64, last_seq: u64, closed: bool) -> TxnRun {
    TxnRun {
        txn: txn.into(),
        first_seq,
        last_seq,
        closed,
    }
}

/// Mutation: let `Draft::commit` set a payload or leave `txn` None and the append is refused, so the
/// first block goes red; drop the payload or txn rule in `frame::encode` and the refusal blocks go red
/// (the ledger would write the frame and `version()` would move).
#[test]
fn a_commit_draft_appends_as_a_commit_frame_and_malformed_commits_write_nothing() {
    let td = TempDir::new().unwrap();
    let mut led = open(td.path(), "aaa");
    let mut clock = HlcClock::new();
    led.append(&mut clock, 1, assert_draft(Some("t1"))).unwrap();
    let ap = led.append(&mut clock, 2, commit_draft("t1")).unwrap();
    assert_eq!(ap.seq, 2);
    let frames: Vec<_> = led
        .scan(&o("aaa"), 0)
        .unwrap()
        .map(|f| f.unwrap().decoded)
        .collect();
    assert_eq!(frames.len(), 2);
    let c = &frames[1];
    assert_eq!(c.header.mode, Mode::Commit);
    assert_eq!(c.header.txn.as_deref(), Some("t1"));
    assert_eq!(c.header.witness, "urn:w");
    assert_eq!(c.header.vocab, "v");
    assert!(c.header.subjects.is_empty());
    assert!(c.payload.is_empty());
    assert_eq!(c.state, PayloadState::Intact);
    assert_eq!(c.id, ap.id);

    // A commit with a payload, and a commit with no txn, are refused and leave the stream alone.
    let mut with_payload = commit_draft("t1");
    with_payload.payload = b"[]".to_vec();
    let mut without_txn = commit_draft("t1");
    without_txn.txn = None;
    for bad in [with_payload, without_txn] {
        let err = led.append(&mut clock, 3, bad).unwrap_err();
        assert!(
            matches!(err, LedgerError::Stream(StreamError::Frame { .. })),
            "{err:?}"
        );
        assert_eq!(led.version().get(&o("aaa")), 2);
    }
    // And the stream still takes the next good frame at seq 3.
    assert_eq!(
        led.append(&mut clock, 4, assert_draft(None)).unwrap().seq,
        3
    );
}

/// Mutation: ignore `Mode::Commit` when computing `closed` (always false) and every closed run below
/// goes red; forget to break a run on a frame without a txn and `B 5..6` swallows seq 8; let a commit
/// not end its run and the final `A` run merges with `A 2..4`; start `first_seq` at the run's true
/// start instead of `from_seq` and the `from_seq = 3` case goes red.
#[test]
fn txn_runs_group_consecutive_frames_of_one_txn_and_close_on_a_commit() {
    let td = TempDir::new().unwrap();
    let mut a = open(&td.path().join("a"), "aaa");
    let mut clock = HlcClock::new();
    let mut wall = 0u64;
    let mut put = |a: &mut Ledger, d: Draft| {
        wall += 1;
        a.append(&mut clock, wall, d).unwrap().seq
    };
    assert_eq!(put(&mut a, assert_draft(None)), 1);
    assert_eq!(put(&mut a, assert_draft(Some("A"))), 2);
    assert_eq!(put(&mut a, assert_draft(Some("A"))), 3);
    assert_eq!(put(&mut a, commit_draft("A")), 4);
    assert_eq!(put(&mut a, assert_draft(Some("B"))), 5);
    assert_eq!(put(&mut a, assert_draft(Some("B"))), 6);
    assert_eq!(put(&mut a, assert_draft(None)), 7);
    assert_eq!(put(&mut a, assert_draft(Some("B"))), 8);
    assert_eq!(put(&mut a, commit_draft("C")), 9);
    assert_eq!(put(&mut a, assert_draft(Some("A"))), 10);

    let full = vec![
        run("A", 2, 4, true),
        run("B", 5, 6, false),
        run("B", 8, 8, false),
        run("C", 9, 9, true),
        run("A", 10, 10, false),
    ];
    let aaa = o("aaa");
    assert_eq!(a.txn_runs(&aaa, 0).unwrap(), full);
    assert_eq!(a.txn_runs(&aaa, 1).unwrap(), full);
    // From inside a run: the run starts at the first frame at or after from_seq.
    let mut from3 = full.clone();
    from3[0] = run("A", 3, 4, true);
    assert_eq!(a.txn_runs(&aaa, 3).unwrap(), from3);
    assert_eq!(a.txn_runs(&aaa, 4).unwrap()[0], run("A", 4, 4, true));
    assert_eq!(a.txn_runs(&aaa, 5).unwrap(), full[1..].to_vec());
    let mut from6 = full[1..].to_vec();
    from6[0] = run("B", 6, 6, false);
    assert_eq!(a.txn_runs(&aaa, 6).unwrap(), from6);
    assert_eq!(a.txn_runs(&aaa, 10).unwrap(), vec![run("A", 10, 10, false)]);
    // Past the end, and an origin we do not hold.
    assert!(a.txn_runs(&aaa, 11).unwrap().is_empty());
    assert!(a.txn_runs(&aaa, 500).unwrap().is_empty());
    assert!(matches!(
        a.txn_runs(&o("zzz"), 0),
        Err(LedgerError::BadOrigin(_))
    ));

    // The last run closes when its commit lands.
    assert_eq!(put(&mut a, commit_draft("A")), 11);
    let mut closed = full.clone();
    closed[4] = run("A", 10, 11, true);
    assert_eq!(a.txn_runs(&aaa, 0).unwrap(), closed);

    // A replica that received the frames reads the same runs: a partial sync leaves a txn open.
    let mut b = open(&td.path().join("b"), "bbb");
    assert_eq!(ship(&mut a, &mut b), 11);
    assert_eq!(b.txn_runs(&aaa, 0).unwrap(), closed);
}

/// Mutation: a stream that has only the frames and not the commit reads open, never closed: make
/// `txn_runs` treat any run as closed and the open-run assertion goes red.
#[test]
fn a_txn_whose_commit_has_not_arrived_is_open_on_the_replica() {
    let td = TempDir::new().unwrap();
    let mut a = open(&td.path().join("a"), "aaa");
    let mut clock = HlcClock::new();
    a.append(&mut clock, 1, assert_draft(Some("T"))).unwrap();
    a.append(&mut clock, 2, assert_draft(Some("T"))).unwrap();
    let mut b = open(&td.path().join("b"), "bbb");
    assert_eq!(ship(&mut a, &mut b), 2);
    assert_eq!(
        b.txn_runs(&o("aaa"), 0).unwrap(),
        vec![run("T", 1, 2, false)]
    );
    a.append(&mut clock, 3, commit_draft("T")).unwrap();
    assert_eq!(ship(&mut a, &mut b), 1);
    assert_eq!(
        b.txn_runs(&o("aaa"), 0).unwrap(),
        vec![run("T", 1, 3, true)]
    );
}

/// Mutation: delete the `NoOverlap` check in `Ledger::apply` and the tipless chunk is accepted
/// (`Ok(2)`), so the `unwrap_err` panics; make it fire when `ours == 0` and the first-ever apply
/// below is refused; make it fire on an empty chunk and the `Ok(0)` line goes red.
#[test]
fn apply_requires_an_overlap_when_we_already_hold_the_origin() {
    let td = TempDir::new().unwrap();
    let mut a = open(&td.path().join("a"), "aaa");
    let mut clock = HlcClock::new();
    for i in 1..=3 {
        a.append(&mut clock, i, assert_draft(None)).unwrap();
    }
    let mut b = open(&td.path().join("b"), "bbb");
    // Nothing held: no overlap is needed, and a chunk that starts at 1 applies.
    assert_eq!(ship(&mut a, &mut b), 3);
    for i in 4..=5 {
        a.append(&mut clock, i, assert_draft(None)).unwrap();
    }
    a.sync_full().unwrap();
    let tipless = a
        .stream(&o("aaa"))
        .unwrap()
        .raw_after(3, usize::MAX)
        .unwrap();
    assert_eq!((tipless.from_seq, tipless.to_seq), (4, 5));
    let err = b.apply(&o("aaa"), &tipless.bytes).unwrap_err();
    assert!(err.to_string().contains("repeats none"), "{err}");
    match err {
        LedgerError::NoOverlap {
            origin,
            ours,
            first,
        } => assert_eq!((origin, ours, first), (o("aaa"), 3, 4)),
        other => panic!("expected NoOverlap, got {other:?}"),
    }
    assert_eq!(b.version().get(&o("aaa")), 3, "nothing was written");
    // An empty chunk is nothing to apply, not a missing overlap.
    assert_eq!(b.apply(&o("aaa"), &[]).unwrap(), 0);
    // The diff chunk carries the tip and applies.
    assert_eq!(ship(&mut a, &mut b), 2);
    assert_eq!(b.version().get(&o("aaa")), 5);
    // A later in-step sync (tip only, 0 new) still passes.
    assert_eq!(ship(&mut a, &mut b), 0);
}

/// Build the redact frames `(genuine, bogus)` that both name `(aaa, 1)`: the genuine one from a
/// replica that holds the target, the bogus one with a made-up target id.
fn redact_makers(td: &Path, a: &mut Ledger) -> (Ledger, Ledger) {
    let mut clock = HlcClock::new();
    let mut c = open(&td.join("c"), "ccc");
    assert_eq!(ship(a, &mut c), 1);
    c.redact(&mut clock, 10, "urn:w".into(), "v".into(), &o("aaa"), 1)
        .unwrap();
    c.sync_full().unwrap();

    let mut b = open(&td.join("b"), "bbb");
    let bogus = RedactPayload {
        target_origin: "aaa".into(),
        target_seq: 1,
        target_id: "ab".repeat(32),
    }
    .to_bytes();
    b.append(
        &mut clock,
        11,
        Draft {
            witness: "urn:w".into(),
            vocab: "v".into(),
            subjects: Vec::new(),
            mode: Mode::Redact,
            payload: bogus,
            txn: None,
        },
    )
    .unwrap();
    b.sync_full().unwrap();
    (c, b)
}

/// Apply only `origin`'s chunk from `from` into `to`.
fn ship_origin(from: &Ledger, to: &mut Ledger, origin: &str) {
    for (org, chunk) in from.diff(&to.version(), 1 << 20).unwrap() {
        if org == o(origin) {
            to.apply(&org, &chunk.bytes).unwrap();
        }
    }
}

fn first_frame_state(l: &Ledger) -> (PayloadState, Vec<u8>) {
    let f = l.scan(&o("aaa"), 0).unwrap().next().unwrap().unwrap();
    (f.decoded.state, f.decoded.payload)
}

/// Mutation: key `pending_redactions` by `(origin, seq)` again (value = id) and the genuine redaction
/// is overwritten by the bogus one that arrives after it, so the target lands un-zeroed and the
/// `Redacted` assertion in the first loop iteration goes red; skip the id comparison when the target
/// lands and the bogus-only case zeroes a frame it should not.
#[test]
fn a_bogus_redact_frame_cannot_displace_the_genuine_pending_redaction() {
    for genuine_first in [true, false] {
        let td = TempDir::new().unwrap();
        let mut a = open(&td.path().join("a"), "aaa");
        a.append(&mut HlcClock::new(), 1, assert_draft(None))
            .unwrap();
        let (c, b) = redact_makers(td.path(), &mut a);

        // The redact frames reach a replica that does not hold the target yet.
        let mut d = open(&td.path().join("d"), "ddd");
        if genuine_first {
            ship_origin(&c, &mut d, "ccc");
            ship_origin(&b, &mut d, "bbb");
        } else {
            ship_origin(&b, &mut d, "bbb");
            ship_origin(&c, &mut d, "ccc");
        }
        assert_eq!(d.version().get(&o("aaa")), 0);
        // Then the target lands: it is zeroed on arrival.
        ship_origin(&a, &mut d, "aaa");
        let (state, payload) = first_frame_state(&d);
        assert_eq!(
            state,
            PayloadState::Redacted,
            "genuine_first={genuine_first}"
        );
        assert!(payload.iter().all(|&x| x == 0) && !payload.is_empty());

        // Reopening re-reads the redact frames and keeps it zero.
        drop(d);
        let d = open(&td.path().join("d"), "ddd");
        assert_eq!(first_frame_state(&d).0, PayloadState::Redacted);
    }

    // The bogus frame alone names a position with the wrong id: it must not zero anything.
    let td = TempDir::new().unwrap();
    let mut a = open(&td.path().join("a"), "aaa");
    a.append(&mut HlcClock::new(), 1, assert_draft(None))
        .unwrap();
    let (_c, b) = redact_makers(td.path(), &mut a);
    let mut d = open(&td.path().join("d"), "ddd");
    ship_origin(&b, &mut d, "bbb");
    ship_origin(&a, &mut d, "aaa");
    assert_eq!(first_frame_state(&d).0, PayloadState::Intact);
}

/// Mutation: write a fresh id on every open and the second open's id differs; read the id from
/// `GARDEN_CELL_ID` and the format assertion goes red; write the file in place (no link) and the
/// racing-mint test sees two ids; skip validation on read and the garbage case opens.
#[test]
fn open_dir_mints_the_cell_origin_once_and_reads_it_back() {
    let td = TempDir::new().unwrap();
    let dir = td.path().join("nested/ledger");
    let (mut led, rec) = Ledger::open_dir(&dir).unwrap();
    assert!(rec
        .iter()
        .all(|(_, r)| *r == garden_ledger::Recovery::Clean));
    let origin = led.local().clone();
    let id = &origin.0;
    assert!(id.starts_with("cell-"), "{id}");
    let hex = &id["cell-".len()..];
    assert_eq!(hex.len(), 16, "{id}");
    assert!(
        hex.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "{id}"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join(ORIGIN_FILE))
            .unwrap()
            .trim(),
        id
    );
    assert!(dir.join(format!("{id}.log")).is_file());
    led.append(&mut HlcClock::new(), 1, assert_draft(None))
        .unwrap();
    led.sync_barrier().unwrap();
    drop(led);

    // The second open reads the same id and sees the frame; no stray temp files remain.
    let (led, _) = Ledger::open_dir(&dir).unwrap();
    assert_eq!(led.local(), &origin);
    assert_eq!(led.version().get(&origin), 1);
    let mut names: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    let mut want = vec![ORIGIN_FILE.to_string(), format!("{id}.log")];
    want.sort();
    assert_eq!(names, want);
    drop(led);

    // `Ledger::open` with an explicit local origin still works in the same directory and wins.
    let (led, _) = Ledger::open(&dir, o("explicit")).unwrap();
    assert_eq!(led.local(), &o("explicit"));
    drop(led);
    assert_eq!(
        read_or_mint_origin(&dir).unwrap(),
        origin,
        "the file is untouched"
    );
}

/// Mutation: derive the id from the clock alone (drop `RandomState` and the counter) and fifty mints
/// in a tight loop collide.
#[test]
fn minted_origins_differ_between_ledgers() {
    let td = TempDir::new().unwrap();
    let mut seen = std::collections::BTreeSet::new();
    for i in 0..50 {
        let id = read_or_mint_origin(&td.path().join(format!("l{i}"))).unwrap();
        assert!(seen.insert(id.0), "duplicate origin at {i}");
    }
}

/// Mutation: replace the link-into-place with a plain `write` of the final file and two racing
/// threads can each mint and see different ids.
#[test]
fn racing_mints_in_one_directory_agree() {
    for round in 0..5 {
        let td = TempDir::new().unwrap();
        let dir = td.path().join(format!("race{round}"));
        let ids: Vec<OriginId> = std::thread::scope(|s| {
            let handles: Vec<_> = (0..8)
                .map(|_| s.spawn(|| read_or_mint_origin(&dir).unwrap()))
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        assert!(ids.windows(2).all(|w| w[0] == w[1]), "{ids:?}");
        assert_eq!(read_or_mint_origin(&dir).unwrap(), ids[0]);
    }
}

/// Mutation: trust whatever the file holds and a path-like id opens; overwrite a bad file with a fresh
/// mint and the "file is untouched" assertion goes red; ignore a valid hand-written id and the
/// `my-cell.1` case goes red.
#[test]
fn an_origin_file_is_honoured_or_refused_never_overwritten() {
    let td = TempDir::new().unwrap();

    let good = td.path().join("good");
    std::fs::create_dir_all(&good).unwrap();
    std::fs::write(good.join(ORIGIN_FILE), "my-cell.1\n").unwrap();
    let (led, _) = Ledger::open_dir(&good).unwrap();
    assert_eq!(led.local(), &o("my-cell.1"));

    for (i, bad) in ["../escape", "", "a b", "x/y"].iter().enumerate() {
        let dir = td.path().join(format!("bad{i}"));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(ORIGIN_FILE), bad).unwrap();
        assert!(
            matches!(Ledger::open_dir(&dir), Err(LedgerError::BadOrigin(_))),
            "{bad:?}"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join(ORIGIN_FILE)).unwrap(),
            *bad
        );
    }
}
