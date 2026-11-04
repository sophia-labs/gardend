//! Golden fixtures of frame format v1 (`tests/fixtures/v1/*.frame`, each with a `.json` sidecar).
//!
//! The frames are checked-in bytes. The normal test decodes every one and compares the result with its
//! sidecar (expected id, header JSON, payload state and payload bytes), so a later build that cannot
//! read yesterday's frames fails here and not on someone's disk. A fixture with an unknown mode and an
//! unknown header field proves the reader is lenient; one with a non-canonical stored header proves the
//! id is the hash of the stored bytes, not of a re-canonicalization.
//!
//! To regenerate after a deliberate change to the format (which is not allowed once frames exist):
//! `GARDEN_LEDGER_WRITE_FIXTURES=1 cargo test -p garden-ledger --test fixtures write_fixtures`.

use std::collections::BTreeMap;
use std::path::PathBuf;

use garden_ledger::frame::{
    assemble, canonical_header_bytes, decode, encode, inspect, payload_hash, redact_body, to_hex,
};
use garden_ledger::{Event, Header, Hlc, Mode, OriginId, PayloadState, RedactPayload, Subject};
use serde_json::{json, Value};

fn dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/v1")
}

fn subject(s: &str, c: &str, parents: &[&str]) -> Subject {
    Subject {
        subject: s.into(),
        class: c.into(),
        parents: parents.iter().map(|p| p.to_string()).collect(),
    }
}

#[allow(clippy::too_many_arguments)]
fn make(
    origin: &str,
    seq: u64,
    hlc: (u64, u32),
    mode: Mode,
    subjects: Vec<Subject>,
    payload: &[u8],
) -> Event {
    let mut header = Header::for_payload(
        OriginId(origin.into()),
        seq,
        Hlc {
            ms: hlc.0,
            count: hlc.1,
        },
        "https://witness.example/vera".into(),
        "urn:vocab:fixtures".into(),
        subjects,
        mode,
        payload,
    )
    .unwrap();
    header.extra = BTreeMap::new();
    Event {
        header,
        payload: payload.to_vec(),
    }
}

struct Fixture {
    name: &'static str,
    note: &'static str,
    bytes: Vec<u8>,
    /// True when `encode(decode(bytes).header)` must reproduce `bytes` (a canonicalizer canary).
    canonical: bool,
}

fn fixtures() -> Vec<Fixture> {
    let parent_a = "a".repeat(64);
    let parent_b = "0123456789abcdef".repeat(4);
    let mut out = Vec::new();

    out.push(Fixture {
        name: "assert_minimal",
        note: "one subject, JSON records payload",
        bytes: encode(&make(
            "cell-a.1",
            1,
            (1_700_000_000_000, 0),
            Mode::Assert,
            vec![subject("urn:s:1", "urn:c:Thing", &[])],
            br#"[{"s":"urn:s:1","p":"urn:p:name","o":"one"}]"#,
        ))
        .unwrap(),
        canonical: true,
    });

    let mut ev = make(
        "cell-a.1",
        2,
        (1_700_000_000_001, 3),
        Mode::Supersede,
        vec![
            subject("urn:s:1", "urn:c:Thing", &[&parent_a, &parent_b]),
            subject("urn:s:2", "urn:c:Other", &[&parent_a]),
        ],
        br#"[{"s":"urn:s:1","p":"urn:p:name","o":"uno"}]"#,
    );
    ev.header.txn = Some("txn-7".into());
    ev.header.sig = Some("sig:ed25519:abcd".into());
    out.push(Fixture {
        name: "supersede_parents_txn_sig",
        note: "two subjects, three parents, txn and sig slots set",
        bytes: encode(&ev).unwrap(),
        canonical: true,
    });

    out.push(Fixture {
        name: "retract_unicode",
        note: "non-ASCII and escaped characters in header strings (JCS escaping)",
        bytes: encode(&make(
            "cell-e",
            3,
            (1_700_000_000_002, 0),
            Mode::Retract,
            vec![subject(
                "urn:s:caf\u{e9}\u{2603}\u{1F600}\"q\"\\\n",
                "urn:c:T\u{fc}",
                &[],
            )],
            "[{\"o\":\"\u{e9}\u{2603}\"}]".as_bytes(),
        ))
        .unwrap(),
        canonical: true,
    });

    out.push(Fixture {
        name: "assert_empty_payload_edge_ints",
        note: "empty payload; seq and hlc.ms at 2^53 - 1 and 2^53",
        bytes: encode(&make(
            "edge",
            9_007_199_254_740_991,
            (9_007_199_254_740_992, u32::MAX),
            Mode::Assert,
            vec![subject("urn:s:e", "urn:c:E", &[])],
            b"",
        ))
        .unwrap(),
        canonical: true,
    });

    let redact_payload = RedactPayload {
        target_origin: "cell-a.1".into(),
        target_seq: 1,
        target_id: "f".repeat(64),
    }
    .to_bytes();
    out.push(Fixture {
        name: "redact_frame",
        note: "a redact frame naming (cell-a.1, 1)",
        bytes: encode(&make(
            "cell-b.2",
            1,
            (1_700_000_000_100, 0),
            Mode::Redact,
            vec![subject("urn:s:1", "urn:c:Thing", &[&parent_a])],
            &redact_payload,
        ))
        .unwrap(),
        canonical: true,
    });

    let mut redacted = encode(&make(
        "cell-a.1",
        4,
        (1_700_000_000_003, 0),
        Mode::Assert,
        vec![subject("urn:s:4", "urn:c:Thing", &[])],
        b"secret secret secret",
    ))
    .unwrap();
    redact_body(&mut redacted).unwrap();
    out.push(Fixture {
        name: "assert_redacted_payload",
        note: "payload zeroed in place; id unchanged; reads Redacted",
        bytes: redacted,
        canonical: true,
    });

    let mut ev = make(
        "cell-a.1",
        5,
        (1_700_000_000_004, 0),
        Mode::Other("commit".into()),
        vec![subject("urn:s:5", "urn:c:Thing", &[])],
        b"",
    );
    ev.header.extra.insert("future".into(), json!(1));
    out.push(Fixture {
        name: "future_mode_commit_extra_field",
        note: "mode commit (unknown to v1 readers) and an unknown header field future:1",
        bytes: encode(&ev).unwrap(),
        canonical: true,
    });

    let mut ev = make(
        "cell-a.1",
        6,
        (1_700_000_000_005, 0),
        Mode::Other("tombstone-v9".into()),
        vec![subject("urn:s:6", "urn:c:Thing", &[])],
        b"{}",
    );
    ev.header
        .extra
        .insert("zz".into(), json!([1, 2, {"a": true, "b": null}]));
    ev.header.extra.insert("aa".into(), json!("x"));
    ev.header.extra.insert("pi".into(), json!(3.5));
    out.push(Fixture {
        name: "unknown_mode_nested_extras",
        note: "unknown mode with array, object and float extras, kept in canonical order",
        bytes: encode(&ev).unwrap(),
        canonical: true,
    });

    // A header stored pretty-printed with keys out of order: legal to read, id over the stored bytes.
    let ev = make(
        "cell-a.1",
        7,
        (1_700_000_000_006, 0),
        Mode::Assert,
        vec![subject("urn:s:7", "urn:c:Thing", &[])],
        b"[]",
    );
    let canon: Value =
        serde_json::from_slice(&canonical_header_bytes(&ev.header).unwrap()).unwrap();
    let mut reversed = serde_json::Map::new();
    for (k, v) in canon.as_object().unwrap().iter().rev() {
        reversed.insert(k.clone(), v.clone());
    }
    let pretty = serde_json::to_vec_pretty(&Value::Object(reversed)).unwrap();
    out.push(Fixture {
        name: "noncanonical_stored_header",
        note: "header stored pretty-printed, keys in reverse order; id = blake3 of these bytes",
        bytes: assemble(&pretty, b"[]").unwrap(),
        canonical: false,
    });

    // Intact crcs, payload that fails its hash and is not zero: a damaged payload, still readable.
    let ev = make(
        "cell-a.1",
        8,
        (1_700_000_000_007, 0),
        Mode::Assert,
        vec![subject("urn:s:8", "urn:c:Thing", &[])],
        b"original",
    );
    let mut hdr = ev.header.clone();
    hdr.payload_hash = payload_hash(b"not what was stored");
    out.push(Fixture {
        name: "damaged_payload_hash_mismatch",
        note: "crcs hold, hash does not, payload not all zero: reads Damaged",
        bytes: assemble(&canonical_header_bytes(&hdr).unwrap(), b"original").unwrap(),
        canonical: false,
    });

    out
}

fn state_name(s: PayloadState) -> &'static str {
    match s {
        PayloadState::Intact => "intact",
        PayloadState::Redacted => "redacted",
        PayloadState::Damaged => "damaged",
    }
}

#[test]
fn write_fixtures() {
    if std::env::var("GARDEN_LEDGER_WRITE_FIXTURES").as_deref() != Ok("1") {
        return;
    }
    std::fs::create_dir_all(dir()).unwrap();
    for f in fixtures() {
        let d = decode(&f.bytes).unwrap();
        let ins = inspect(&f.bytes).unwrap();
        let header_json: Value = serde_json::from_slice(ins.header_bytes).unwrap();
        let sidecar = json!({
            "note": f.note,
            "id": to_hex(&d.id),
            "canonical": f.canonical,
            "payload_state": state_name(d.state),
            "payload_hex": garden_ledger::frame::hex_of(&d.payload),
            "header": header_json,
        });
        std::fs::write(dir().join(format!("{}.frame", f.name)), &f.bytes).unwrap();
        std::fs::write(
            dir().join(format!("{}.json", f.name)),
            serde_json::to_string_pretty(&sidecar).unwrap() + "\n",
        )
        .unwrap();
    }
}

#[test]
fn every_golden_fixture_decodes_to_its_sidecar() {
    let mut names: Vec<String> = std::fs::read_dir(dir())
        .expect("tests/fixtures/v1 is checked in")
        .filter_map(|e| {
            let n = e.unwrap().file_name().into_string().unwrap();
            n.strip_suffix(".frame").map(str::to_string)
        })
        .collect();
    names.sort();
    assert!(names.len() >= 10, "fixtures went missing: {names:?}");
    let mut saw_future = false;
    let mut saw_noncanonical = false;
    for name in &names {
        let bytes = std::fs::read(dir().join(format!("{name}.frame"))).unwrap();
        let side: Value =
            serde_json::from_slice(&std::fs::read(dir().join(format!("{name}.json"))).unwrap())
                .unwrap();
        let d = decode(&bytes).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(d.frame_len, bytes.len(), "{name}");
        assert_eq!(to_hex(&d.id), side["id"].as_str().unwrap(), "{name}: id");
        assert_eq!(
            serde_json::to_value(&d.header).unwrap(),
            side["header"],
            "{name}: header"
        );
        assert_eq!(
            state_name(d.state),
            side["payload_state"].as_str().unwrap(),
            "{name}: state"
        );
        assert_eq!(
            garden_ledger::frame::hex_of(&d.payload),
            side["payload_hex"].as_str().unwrap(),
            "{name}: payload"
        );
        // The id is the hash of the stored header bytes, whatever they are.
        let ins = inspect(&bytes).unwrap();
        assert_eq!(d.id, *blake3::hash(ins.header_bytes).as_bytes(), "{name}");
        if side["canonical"].as_bool().unwrap() {
            // Canary for canonicalizer drift: today's encoder reproduces the stored bytes. If this ever
            // fails the fixtures above still decode (readers never re-canonicalize); fix the encoder pin.
            let again = Event {
                header: d.header.clone(),
                payload: d.payload.clone(),
            };
            if d.state == PayloadState::Intact {
                assert_eq!(encode(&again).unwrap(), bytes, "{name}: re-encode");
            }
        } else {
            saw_noncanonical = true;
        }
        if name == "future_mode_commit_extra_field" {
            saw_future = true;
            assert_eq!(d.header.mode, Mode::Other("commit".into()));
            assert_eq!(d.header.extra["future"], json!(1));
        }
    }
    assert!(saw_future && saw_noncanonical);
}
