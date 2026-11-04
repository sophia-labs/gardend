//! Envelope types and the on-disk frame of the ledger (frame format v1, final).
//!
//! ```text
//! frame = [u32 LE body_len][body]
//! body  = [u16 LE hdr_len][header JSON][u32 LE hdr_crc][payload][u32 LE payload_crc]
//! ```
//!
//! `body_len` counts everything after itself. `hdr_crc` is crc32c over `hdr_len || header` and
//! `payload_crc` is crc32c over the payload, so an in-place redaction rewrites the payload and its
//! crc and never touches the header or its crc. `body_len` has no crc of its own; it is checked
//! structurally against the header's `payload_len`.
//!
//! The header is written once, as RFC 8785 canonical JSON, by [`encode`]. The event id is
//! `blake3(stored header bytes)`: a reader hashes the bytes it finds and never re-canonicalizes,
//! so a future writer with a different canonicalizer, an unknown `mode`, or extra header fields
//! still yields frames this reader can open and identify. The header is `u16`-prefixed, which
//! bounds it to 65,535 bytes: about 900 parent ids (67 bytes each) beside a typical envelope. A
//! header past that is [`FrameError::TooLarge`]. Integers anywhere in the header above 2^53 are
//! refused by [`encode`] (JCS numbers are IEEE doubles, so they would not round-trip).

use serde::de::Deserializer;
use serde::ser::Serializer;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fmt;
use std::ops::Range;

/// blake3 of the stored header bytes.
pub type Id = [u8; 32];
/// Lowercase hex of an [`Id`] (or of a payload hash), 64 chars.
pub type IdHex = String;

/// Largest permitted frame body (64 MiB).
pub const MAX_BODY: usize = 64 * 1024 * 1024;
/// Largest header the `u16` length field can hold.
pub const MAX_HEADER: usize = u16::MAX as usize;
/// Largest integer magnitude a header may carry (2^53).
pub const MAX_JSON_INT: u64 = 1 << 53;
/// Current header version.
pub const VERSION: u8 = 1;
/// Bytes of the `u32` length prefix.
pub const LEN_PREFIX: usize = 4;
/// Smallest possible body: `hdr_len`, `hdr_crc`, `payload_crc`.
const BODY_FIXED: usize = 2 + 4 + 4;

/// Writer identity; never GARDEN_CELL_ID.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct OriginId(pub String);

impl OriginId {
    /// Checks `[A-Za-z0-9._-]{1,128}`.
    pub fn validate(&self) -> Result<(), FrameError> {
        let s = &self.0;
        if s.is_empty() || s.len() > 128 {
            return Err(FrameError::Header(format!(
                "origin length {} not in 1..=128",
                s.len()
            )));
        }
        if s == "." || s == ".." {
            return Err(FrameError::Header(format!(
                "origin {s:?} is a path component"
            )));
        }
        if !s
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'_' || b == b'-')
        {
            return Err(FrameError::Header(format!(
                "origin {s:?} has invalid characters"
            )));
        }
        Ok(())
    }
}

/// Hybrid logical clock stamp; `HlcClock` in hlc.rs implements the clock.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Hlc {
    /// Physical milliseconds.
    pub ms: u64,
    /// Logical counter within the millisecond.
    pub count: u32,
}

/// What an event does. Names this build does not know decode to [`Mode::Other`] and are written
/// back unchanged; a reader never refuses a frame over its mode.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Assert records.
    Assert,
    /// Retract records.
    Retract,
    /// Supersede earlier events.
    Supersede,
    /// Redact an earlier payload.
    Redact,
    /// Close a multi-frame transaction: a frame of the same origin and the same `txn` as the run
    /// of frames it closes. [`encode`] refuses a commit frame that carries a payload or has no
    /// `txn`; [`decode`] reads one like any other frame (a reader never refuses a frame over its
    /// mode). The fold applies a txn's buffered frames when the commit arrives.
    Commit,
    /// A mode written by a newer build than this one.
    Other(String),
}

impl Mode {
    /// The wire name.
    pub fn as_str(&self) -> &str {
        match self {
            Mode::Assert => "assert",
            Mode::Retract => "retract",
            Mode::Supersede => "supersede",
            Mode::Redact => "redact",
            Mode::Commit => "commit",
            Mode::Other(s) => s,
        }
    }

    /// Map a wire name to a mode; unknown names become [`Mode::Other`].
    pub fn from_name(s: &str) -> Mode {
        match s {
            "assert" => Mode::Assert,
            "retract" => Mode::Retract,
            "supersede" => Mode::Supersede,
            "redact" => Mode::Redact,
            "commit" => Mode::Commit,
            other => Mode::Other(other.to_string()),
        }
    }

    fn is_known_name(s: &str) -> bool {
        !matches!(Mode::from_name(s), Mode::Other(_))
    }
}

impl Serialize for Mode {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for Mode {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Mode, D::Error> {
        let s = String::deserialize(d)?;
        Ok(Mode::from_name(&s))
    }
}

/// A subject touched by an event.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Subject {
    /// Subject IRI.
    pub subject: String,
    /// Class IRI.
    pub class: String,
    /// Parent event ids (hex).
    #[serde(default)]
    pub parents: Vec<IdHex>,
}

/// Header keys this build knows; an `extra` entry may not reuse one.
const KNOWN_KEYS: [&str; 12] = [
    "v",
    "origin",
    "seq",
    "hlc",
    "witness",
    "vocab",
    "subjects",
    "mode",
    "payload_hash",
    "payload_len",
    "txn",
    "sig",
];

/// Everything the id covers. `payload_hash` and `payload_len` stand in for the payload.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Header {
    /// Version, 1.
    pub v: u8,
    /// Writer.
    pub origin: OriginId,
    /// Dense per origin, from 1.
    pub seq: u64,
    /// Clock stamp.
    pub hlc: Hlc,
    /// IRI of the observer.
    pub witness: String,
    /// Vocabulary.
    pub vocab: String,
    /// Subjects.
    pub subjects: Vec<Subject>,
    /// Mode.
    pub mode: Mode,
    /// blake3 of the payload bytes.
    pub payload_hash: IdHex,
    /// Payload length in bytes.
    pub payload_len: u32,
    /// Cross-vocab write marker.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub txn: Option<String>,
    /// Signature slot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sig: Option<String>,
    /// Header fields this build does not know, kept verbatim and written back in canonical order.
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

/// A header and its payload (the JSON records, opaque here).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Event {
    /// Header.
    pub header: Header,
    /// Payload bytes.
    pub payload: Vec<u8>,
}

/// What a decoded frame's payload bytes are.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PayloadState {
    /// The payload matches its hash.
    Intact,
    /// The payload is all zero bytes (and non-empty) and does not match its hash: redacted in place.
    Redacted,
    /// The payload fails its hash and is not all zero. The crcs held, so the header and id are
    /// good; the bytes are what a buggy writer or a hand edit left.
    Damaged,
}

/// Result of decoding one frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Decoded {
    /// Header.
    pub header: Header,
    /// Its id: blake3 of the stored header bytes.
    pub id: Id,
    /// Payload bytes (zeros when redacted).
    pub payload: Vec<u8>,
    /// What the payload is.
    pub state: PayloadState,
    /// Bytes this frame occupies.
    pub frame_len: usize,
}

impl Decoded {
    /// True when the payload was zeroed by a redaction.
    pub fn is_redacted(&self) -> bool {
        self.state == PayloadState::Redacted
    }
}

/// A frame whose structure and header crc hold; the payload crc is reported, not required.
///
/// This is what the open-time scan uses: a frame with a good header and a bad payload crc is a
/// candidate torn redaction, which the ledger may still repair.
#[derive(Clone, Debug)]
pub struct Inspected<'a> {
    /// Header.
    pub header: Header,
    /// Its id: blake3 of the stored header bytes.
    pub id: Id,
    /// The stored header bytes.
    pub header_bytes: &'a [u8],
    /// The payload bytes as stored.
    pub payload: &'a [u8],
    /// Whether `payload_crc` matched.
    pub payload_crc_ok: bool,
    /// Bytes this frame occupies.
    pub frame_len: usize,
}

/// Frame errors.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FrameError {
    /// Not enough bytes; `needed` more are required at least (torn-tail signal).
    Short {
        /// Additional bytes needed.
        needed: usize,
    },
    /// The header crc did not match.
    HeaderCrc,
    /// The payload crc did not match.
    PayloadCrc,
    /// The lengths in the frame disagree with each other (a damaged length prefix or `hdr_len`).
    Geometry(String),
    /// Header JSON or structure invalid although its crc held.
    Header(String),
    /// Unsupported header version.
    Version(u8),
    /// Body over the 64 MiB limit, or a header over the `u16` limit.
    TooLarge(usize),
    /// Canonicalization failed.
    Canonical(String),
    /// An integer above 2^53 in the header.
    IntegerRange(String),
}

impl fmt::Display for FrameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FrameError::Short { needed } => write!(f, "short frame, need {needed} more bytes"),
            FrameError::HeaderCrc => write!(f, "header crc mismatch"),
            FrameError::PayloadCrc => write!(f, "payload crc mismatch"),
            FrameError::Geometry(m) => write!(f, "frame geometry: {m}"),
            FrameError::Header(m) => write!(f, "bad header: {m}"),
            FrameError::Version(v) => write!(f, "unsupported version {v}"),
            FrameError::TooLarge(n) => write!(f, "{n} bytes exceeds a frame limit"),
            FrameError::Canonical(m) => write!(f, "canonical form: {m}"),
            FrameError::IntegerRange(m) => write!(f, "integer above 2^53: {m}"),
        }
    }
}

impl std::error::Error for FrameError {}

/// Lowercase hex of an id or hash.
pub fn to_hex(id: &Id) -> IdHex {
    hex_of(id)
}

/// Lowercase hex of arbitrary bytes.
pub fn hex_of(bytes: &[u8]) -> String {
    const H: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(H[(b >> 4) as usize] as char);
        s.push(H[(b & 15) as usize] as char);
    }
    s
}

/// Parse 64 lowercase hex chars into an [`Id`].
pub fn from_hex(s: &str) -> Result<Id, FrameError> {
    let b = s.as_bytes();
    if b.len() != 64 {
        return Err(FrameError::Header(format!("hex id length {}", b.len())));
    }
    fn nib(c: u8) -> Option<u8> {
        match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            _ => None,
        }
    }
    let mut out = [0u8; 32];
    for i in 0..32 {
        match (nib(b[2 * i]), nib(b[2 * i + 1])) {
            (Some(h), Some(l)) => out[i] = (h << 4) | l,
            _ => {
                return Err(FrameError::Header(
                    "hex id has non-lowercase-hex char".into(),
                ))
            }
        }
    }
    Ok(out)
}

/// Walk a header value and refuse any integer whose magnitude exceeds 2^53.
fn check_integers(v: &Value, path: &mut Vec<String>) -> Result<(), FrameError> {
    match v {
        Value::Number(n) => {
            let bad = if let Some(u) = n.as_u64() {
                u > MAX_JSON_INT
            } else if let Some(i) = n.as_i64() {
                i.unsigned_abs() > MAX_JSON_INT
            } else {
                false
            };
            if bad {
                return Err(FrameError::IntegerRange(format!(
                    "{n} at /{}",
                    path.join("/")
                )));
            }
        }
        Value::Array(a) => {
            for (i, x) in a.iter().enumerate() {
                path.push(i.to_string());
                check_integers(x, path)?;
                path.pop();
            }
        }
        Value::Object(m) => {
            for (k, x) in m {
                path.push(k.clone());
                check_integers(x, path)?;
                path.pop();
            }
        }
        _ => {}
    }
    Ok(())
}

/// RFC 8785 canonical JSON bytes of the header, as [`encode`] writes them.
///
/// Refuses an `extra` key that shadows a known field, an `Other` mode that spells a known one,
/// and any integer above 2^53 (checked on the `serde_json::Value`, before canonicalizing).
pub fn canonical_header_bytes(h: &Header) -> Result<Vec<u8>, FrameError> {
    for k in h.extra.keys() {
        if KNOWN_KEYS.contains(&k.as_str()) {
            return Err(FrameError::Header(format!(
                "extra header field {k:?} shadows a known field"
            )));
        }
    }
    if let Mode::Other(s) = &h.mode {
        if Mode::is_known_name(s) {
            return Err(FrameError::Header(format!(
                "Mode::Other({s:?}) spells a known mode"
            )));
        }
    }
    let value = serde_json::to_value(h).map_err(|e| FrameError::Header(e.to_string()))?;
    check_integers(&value, &mut Vec::new())?;
    serde_json_canonicalizer::to_vec(&value).map_err(|e| FrameError::Canonical(e.to_string()))
}

/// The id [`encode`] gives this header: blake3 of its canonical bytes. A header decoded from
/// bytes a different writer produced may hash differently; the authoritative id of a stored
/// frame is [`Decoded::id`].
pub fn id_of(h: &Header) -> Result<Id, FrameError> {
    Ok(*blake3::hash(&canonical_header_bytes(h)?).as_bytes())
}

/// Hex blake3 of payload bytes.
pub fn payload_hash(payload: &[u8]) -> IdHex {
    to_hex(blake3::hash(payload).as_bytes())
}

impl Header {
    /// Build a version-1 header for `payload`, filling `payload_hash` and
    /// `payload_len`; `txn`, `sig` and `extra` are empty.
    #[allow(clippy::too_many_arguments)]
    pub fn for_payload(
        origin: OriginId,
        seq: u64,
        hlc: Hlc,
        witness: String,
        vocab: String,
        subjects: Vec<Subject>,
        mode: Mode,
        payload: &[u8],
    ) -> Result<Header, FrameError> {
        let payload_len =
            u32::try_from(payload.len()).map_err(|_| FrameError::TooLarge(payload.len()))?;
        Ok(Header {
            v: VERSION,
            origin,
            seq,
            hlc,
            witness,
            vocab,
            subjects,
            mode,
            payload_hash: payload_hash(payload),
            payload_len,
            txn: None,
            sig: None,
            extra: BTreeMap::new(),
        })
    }
}

/// The payload of a `redact` frame: which event's payload the redaction names.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RedactPayload {
    /// Origin of the target frame (may differ from the redact frame's own origin).
    pub target_origin: String,
    /// Seq of the target frame in that origin's stream.
    pub target_seq: u64,
    /// Id of the target frame, lowercase hex.
    pub target_id: IdHex,
}

impl RedactPayload {
    /// The JSON bytes of this payload.
    pub fn to_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("a struct of strings and an integer serializes")
    }

    /// Parse a redact frame's payload.
    pub fn from_bytes(b: &[u8]) -> Result<RedactPayload, FrameError> {
        serde_json::from_slice(b).map_err(|e| FrameError::Header(format!("redact payload: {e}")))
    }

    /// The target id as bytes.
    pub fn id(&self) -> Result<Id, FrameError> {
        from_hex(&self.target_id)
    }
}

/// Put already-serialized header bytes and a payload into a frame: lengths and both crcs.
///
/// [`encode`] canonicalizes and calls this. It is public so tests and tools can store a header
/// that is not canonical and prove a reader hashes what is stored.
pub fn assemble(header_bytes: &[u8], payload: &[u8]) -> Result<Vec<u8>, FrameError> {
    let hlen =
        u16::try_from(header_bytes.len()).map_err(|_| FrameError::TooLarge(header_bytes.len()))?;
    let body_len = BODY_FIXED + header_bytes.len() + payload.len();
    if body_len > MAX_BODY {
        return Err(FrameError::TooLarge(body_len));
    }
    let mut out = Vec::with_capacity(LEN_PREFIX + body_len);
    out.extend_from_slice(&(body_len as u32).to_le_bytes());
    out.extend_from_slice(&hlen.to_le_bytes());
    out.extend_from_slice(header_bytes);
    let hcrc = crc32c::crc32c(&out[LEN_PREFIX..]);
    out.extend_from_slice(&hcrc.to_le_bytes());
    out.extend_from_slice(payload);
    out.extend_from_slice(&crc32c::crc32c(payload).to_le_bytes());
    Ok(out)
}

/// Encode an event as one frame. The payload length must match the header.
///
/// A [`Mode::Commit`] event must carry no payload and must name its `txn`; otherwise this is
/// [`FrameError::Header`]. (Decoding a commit frame is ordinary: those rules bind writers only.)
pub fn encode(ev: &Event) -> Result<Vec<u8>, FrameError> {
    if ev.header.v != VERSION {
        return Err(FrameError::Version(ev.header.v));
    }
    if ev.header.mode == Mode::Commit {
        if !ev.payload.is_empty() {
            return Err(FrameError::Header(format!(
                "commit frame carries a payload of {} bytes",
                ev.payload.len()
            )));
        }
        if ev.header.txn.is_none() {
            return Err(FrameError::Header("commit frame without a txn".into()));
        }
    }
    if ev.payload.len() != ev.header.payload_len as usize {
        return Err(FrameError::Header(format!(
            "payload_len {} != payload {}",
            ev.header.payload_len,
            ev.payload.len()
        )));
    }
    let hb = canonical_header_bytes(&ev.header)?;
    assemble(&hb, &ev.payload)
}

/// Frame geometry after the length and header-crc checks.
struct Geometry {
    /// Body length from the prefix.
    body_len: usize,
    /// Header length from the body.
    hlen: usize,
}

impl Geometry {
    fn header(&self) -> Range<usize> {
        LEN_PREFIX + 2..LEN_PREFIX + 2 + self.hlen
    }
    fn payload(&self) -> Range<usize> {
        let start = LEN_PREFIX + 2 + self.hlen + 4;
        start..LEN_PREFIX + self.body_len - 4
    }
    fn payload_crc(&self) -> Range<usize> {
        LEN_PREFIX + self.body_len - 4..LEN_PREFIX + self.body_len
    }
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

/// Length, bounds and header-crc checks, in the order that keeps a torn tail `Short`.
fn geometry(bytes: &[u8]) -> Result<Geometry, FrameError> {
    if bytes.len() < LEN_PREFIX {
        return Err(FrameError::Short {
            needed: LEN_PREFIX - bytes.len(),
        });
    }
    let body_len = u32_at(bytes, 0) as usize;
    if body_len > MAX_BODY {
        return Err(FrameError::TooLarge(body_len));
    }
    let total = LEN_PREFIX + body_len;
    if bytes.len() < total {
        return Err(FrameError::Short {
            needed: total - bytes.len(),
        });
    }
    if body_len < BODY_FIXED {
        return Err(FrameError::Geometry(format!(
            "body of {body_len} bytes is shorter than its fixed fields"
        )));
    }
    let hlen = u16::from_le_bytes([bytes[4], bytes[5]]) as usize;
    if BODY_FIXED + hlen > body_len {
        return Err(FrameError::Geometry(format!(
            "header length {hlen} exceeds body of {body_len}"
        )));
    }
    let g = Geometry { body_len, hlen };
    let hcrc_at = LEN_PREFIX + 2 + hlen;
    if crc32c::crc32c(&bytes[LEN_PREFIX..hcrc_at]) != u32_at(bytes, hcrc_at) {
        return Err(FrameError::HeaderCrc);
    }
    Ok(g)
}

/// Check structure and the header crc and parse the header, without requiring the payload crc.
///
/// The id is `blake3(stored header bytes)`; nothing is re-canonicalized.
pub fn inspect(bytes: &[u8]) -> Result<Inspected<'_>, FrameError> {
    let g = geometry(bytes)?;
    let hb = &bytes[g.header()];
    let header: Header =
        serde_json::from_slice(hb).map_err(|e| FrameError::Header(e.to_string()))?;
    if header.v != VERSION {
        return Err(FrameError::Version(header.v));
    }
    let payload = &bytes[g.payload()];
    if payload.len() != header.payload_len as usize {
        return Err(FrameError::Geometry(format!(
            "payload_len {} != {} payload bytes in the body",
            header.payload_len,
            payload.len()
        )));
    }
    let payload_crc_ok = crc32c::crc32c(payload) == u32_at(bytes, g.payload_crc().start);
    Ok(Inspected {
        id: *blake3::hash(hb).as_bytes(),
        header,
        header_bytes: hb,
        payload,
        payload_crc_ok,
        frame_len: LEN_PREFIX + g.body_len,
    })
}

/// Classify payload bytes against the header's hash.
pub fn payload_state(payload: &[u8], header: &Header) -> PayloadState {
    if payload_hash(payload) == header.payload_hash {
        PayloadState::Intact
    } else if !payload.is_empty() && payload.iter().all(|&b| b == 0) {
        PayloadState::Redacted
    } else {
        PayloadState::Damaged
    }
}

/// Decode the first frame in `bytes`; trailing bytes are ignored (see `frame_len`). Both crcs
/// must hold; a payload that fails its hash with intact crcs decodes as [`PayloadState::Damaged`].
pub fn decode(bytes: &[u8]) -> Result<Decoded, FrameError> {
    let ins = inspect(bytes)?;
    if !ins.payload_crc_ok {
        return Err(FrameError::PayloadCrc);
    }
    let state = payload_state(ins.payload, &ins.header);
    Ok(Decoded {
        id: ins.id,
        payload: ins.payload.to_vec(),
        state,
        frame_len: ins.frame_len,
        header: ins.header,
    })
}

/// Zero the payload of the complete frame at the start of `bytes` and rewrite `payload_crc`; the
/// header, `hdr_crc` and id are untouched. Returns the byte range of the frame that changed
/// (payload through its crc, contiguous, so one write applies it), or `None` when the frame was
/// already redacted exactly. The payload crc is not required to hold: this is also the repair of
/// a torn redaction. Fails like [`decode`] on a short frame or a damaged header.
pub fn redact_body(bytes: &mut [u8]) -> Result<Option<Range<usize>>, FrameError> {
    let g = geometry(bytes)?;
    let pr = g.payload();
    let crc_at = g.payload_crc().start;
    let zero_crc = crc32c::crc32c(&vec![0u8; pr.len()]);
    let already = bytes[pr.clone()].iter().all(|&b| b == 0) && u32_at(bytes, crc_at) == zero_crc;
    if already {
        return Ok(None);
    }
    for b in &mut bytes[pr.clone()] {
        *b = 0;
    }
    bytes[crc_at..crc_at + 4].copy_from_slice(&zero_crc.to_le_bytes());
    Ok(Some(pr.start..crc_at + 4))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(payload: &[u8]) -> Event {
        let header = Header::for_payload(
            OriginId("cell-a.1".into()),
            1,
            Hlc {
                ms: 1_700_000_000_000,
                count: 2,
            },
            "urn:witness:x".into(),
            "vocab".into(),
            vec![Subject {
                subject: "s".into(),
                class: "c".into(),
                parents: vec![],
            }],
            Mode::Assert,
            payload,
        )
        .unwrap();
        Event {
            header,
            payload: payload.to_vec(),
        }
    }

    #[test]
    fn round_trip() {
        let ev = sample(br#"[{"subject":"s"}]"#);
        let f = encode(&ev).unwrap();
        let d = decode(&f).unwrap();
        assert_eq!(d.header, ev.header);
        assert_eq!(d.payload, ev.payload);
        assert_eq!(d.state, PayloadState::Intact);
        assert_eq!(d.frame_len, f.len());
        assert_eq!(d.id, id_of(&ev.header).unwrap());
        let mut two = f.clone();
        two.extend_from_slice(&f);
        assert_eq!(decode(&two).unwrap().frame_len, f.len());
    }

    #[test]
    fn short_on_every_prefix() {
        let f = encode(&sample(b"hello payload")).unwrap();
        for n in 0..f.len() {
            match decode(&f[..n]) {
                Err(FrameError::Short { needed }) => assert!(needed > 0 && n + needed <= f.len()),
                other => panic!("prefix {n}: {other:?}"),
            }
        }
    }

    #[test]
    fn each_crc_guards_its_region() {
        let f = encode(&sample(b"hello")).unwrap();
        let hlen = u16::from_le_bytes([f[4], f[5]]) as usize;
        let mut h = f.clone();
        h[6 + hlen / 2] ^= 1;
        assert_eq!(decode(&h), Err(FrameError::HeaderCrc));
        let mut p = f.clone();
        let n = p.len();
        p[n - 6] ^= 1;
        assert_eq!(decode(&p), Err(FrameError::PayloadCrc));
        // Inspect reports the payload crc instead of refusing, and the id is still the header's.
        let ins = inspect(&p).unwrap();
        assert!(!ins.payload_crc_ok);
        assert_eq!(ins.id, decode(&f).unwrap().id);
    }

    #[test]
    fn id_is_the_hash_of_the_stored_header_bytes() {
        // A header stored with spaces and in non-canonical key order still opens, and its id is
        // blake3 of exactly the stored bytes.
        let ev = sample(b"abc");
        let canon = canonical_header_bytes(&ev.header).unwrap();
        let v: Value = serde_json::from_slice(&canon).unwrap();
        let pretty = serde_json::to_vec_pretty(&v).unwrap();
        assert_ne!(pretty, canon);
        let f = assemble(&pretty, b"abc").unwrap();
        let d = decode(&f).unwrap();
        assert_eq!(d.id, *blake3::hash(&pretty).as_bytes());
        assert_ne!(d.id, id_of(&ev.header).unwrap());
        assert_eq!(d.header, ev.header);
    }

    #[test]
    fn redaction_keeps_id_and_header() {
        let ev = sample(b"secret secret");
        let mut f = encode(&ev).unwrap();
        let before = decode(&f).unwrap();
        let header_before = f[..6 + u16::from_le_bytes([f[4], f[5]]) as usize + 4].to_vec();
        let range = redact_body(&mut f).unwrap().unwrap();
        assert_eq!(range.end, f.len());
        let after = decode(&f).unwrap();
        assert_eq!(after.state, PayloadState::Redacted);
        assert_eq!(after.id, before.id);
        assert_eq!(after.header, before.header);
        assert!(after.payload.iter().all(|&b| b == 0));
        assert_eq!(after.payload.len(), ev.payload.len());
        assert_eq!(&f[..header_before.len()], &header_before[..]);
        // Idempotent: a second pass has nothing to change.
        assert_eq!(redact_body(&mut f).unwrap(), None);
    }

    #[test]
    fn hash_mismatch_with_good_crcs_is_damaged_not_refused() {
        let ev = sample(b"abcdef");
        let mut hdr = ev.header.clone();
        hdr.payload_hash = payload_hash(b"something else");
        let f = assemble(&canonical_header_bytes(&hdr).unwrap(), b"abcdef").unwrap();
        let d = decode(&f).unwrap();
        assert_eq!(d.state, PayloadState::Damaged);
    }

    #[test]
    fn mode_and_extra_are_lenient_and_round_trip() {
        let mut ev = sample(b"{}");
        ev.header.mode = Mode::Other("tombstone-v9".into());
        ev.header
            .extra
            .insert("future".into(), serde_json::json!(1));
        ev.header
            .extra
            .insert("nested".into(), serde_json::json!({"b": [1, 2], "a": null}));
        let f = encode(&ev).unwrap();
        let d = decode(&f).unwrap();
        assert_eq!(d.header.mode, Mode::Other("tombstone-v9".into()));
        assert_eq!(d.header.extra["future"], serde_json::json!(1));
        assert_eq!(d.header, ev.header);
        // The unknown fields are in the canonical bytes, in sorted order.
        let text = String::from_utf8(canonical_header_bytes(&ev.header).unwrap()).unwrap();
        assert!(text.contains(r#""future":1"#), "{text}");
        assert!(text.find("future").unwrap() < text.find("hlc").unwrap());
        // A known name cannot hide in Other, and an extra key cannot shadow a field.
        let mut bad = sample(b"{}");
        bad.header.mode = Mode::Other("assert".into());
        assert!(encode(&bad).is_err());
        let mut bad = sample(b"{}");
        bad.header.extra.insert("seq".into(), serde_json::json!(9));
        assert!(encode(&bad).is_err());
    }

    fn commit_event(payload: &[u8], txn: Option<&str>) -> Event {
        let mut ev = sample(payload);
        ev.header.mode = Mode::Commit;
        ev.header.txn = txn.map(str::to_string);
        ev
    }

    /// Mutation: drop the payload check in `encode` (or the `txn` check) and the matching
    /// assertion below goes red; make `Mode::from_name("commit")` return `Other` and the round trip
    /// and the `Other("commit")` refusal go red.
    #[test]
    fn commit_frames_encode_only_empty_with_a_txn() {
        assert_eq!(Mode::from_name("commit"), Mode::Commit);
        assert_eq!(Mode::Commit.as_str(), "commit");
        // A commit with a txn and no payload round-trips.
        let ev = commit_event(b"", Some("txn-1"));
        let f = encode(&ev).unwrap();
        let d = decode(&f).unwrap();
        assert_eq!(d.header, ev.header);
        assert_eq!(d.header.mode, Mode::Commit);
        assert_eq!(d.header.txn.as_deref(), Some("txn-1"));
        assert!(d.payload.is_empty());
        assert_eq!(d.state, PayloadState::Intact);
        // A payload is refused, whether or not there is a txn.
        for txn in [Some("txn-1"), None] {
            assert!(
                matches!(
                    encode(&commit_event(b"{}", txn)),
                    Err(FrameError::Header(_))
                ),
                "payload, txn {txn:?}"
            );
        }
        // No txn is refused.
        assert!(matches!(
            encode(&commit_event(b"", None)),
            Err(FrameError::Header(_))
        ));
        // `commit` is a known name now: it cannot hide in `Other`.
        let mut ev = sample(b"");
        ev.header.mode = Mode::Other("commit".into());
        assert!(encode(&ev).is_err());
    }

    /// Mutation: add the commit rules to `decode`/`inspect` and the frames below stop reading, so
    /// this goes red.
    #[test]
    fn decoding_a_commit_frame_is_ordinary_even_when_a_writer_would_have_been_refused() {
        // A commit that carries a payload, and a commit without a txn, as some other writer made them.
        for (payload, txn) in [(&b"{}"[..], Some("t")), (&b""[..], None)] {
            let ev = commit_event(payload, txn);
            let f = assemble(&canonical_header_bytes(&ev.header).unwrap(), payload).unwrap();
            let d = decode(&f).unwrap();
            assert_eq!(d.header.mode, Mode::Commit);
            assert_eq!(d.header.txn.as_deref(), txn);
            assert_eq!(d.payload, payload);
            assert_eq!(d.state, PayloadState::Intact);
        }
    }

    #[test]
    fn integers_above_2_pow_53_are_refused_anywhere() {
        let mut ev = sample(b"x");
        ev.header.seq = MAX_JSON_INT;
        assert!(encode(&ev).is_ok(), "2^53 itself is allowed");
        ev.header.seq = MAX_JSON_INT + 1;
        assert!(matches!(encode(&ev), Err(FrameError::IntegerRange(_))));
        let mut ev = sample(b"x");
        ev.header.hlc.ms = u64::MAX;
        assert!(matches!(encode(&ev), Err(FrameError::IntegerRange(_))));
        let mut ev = sample(b"x");
        ev.header.extra.insert(
            "deep".into(),
            serde_json::json!({"a": [1, {"b": 9007199254740993u64}]}),
        );
        assert!(matches!(encode(&ev), Err(FrameError::IntegerRange(_))));
        let mut ev = sample(b"x");
        ev.header
            .extra
            .insert("neg".into(), serde_json::json!(-9007199254740993i64));
        assert!(matches!(encode(&ev), Err(FrameError::IntegerRange(_))));
        let mut ev = sample(b"x");
        ev.header
            .extra
            .insert("float".into(), serde_json::json!(1.0e300));
        assert!(encode(&ev).is_ok(), "a float is not an integer");
    }

    #[test]
    fn header_over_u16_is_too_large() {
        let mut ev = sample(b"x");
        // Each parent costs 67 bytes in the header: 66 hex-ish chars, two quotes, a comma.
        ev.header.subjects[0].parents = vec!["a".repeat(64); 900];
        let f = encode(&ev).unwrap();
        assert!(u16::from_le_bytes([f[4], f[5]]) as usize > 60_000);
        ev.header.subjects[0].parents = vec!["a".repeat(64); 1000];
        assert!(matches!(encode(&ev), Err(FrameError::TooLarge(n)) if n > MAX_HEADER));
    }

    #[test]
    fn origin_validation_and_hex() {
        assert!(OriginId("a.b_c-9".into()).validate().is_ok());
        assert!(OriginId("".into()).validate().is_err());
        assert!(OriginId("a b".into()).validate().is_err());
        assert!(OriginId("x".repeat(129)).validate().is_err());
        let id = [0xabu8; 32];
        assert_eq!(from_hex(&to_hex(&id)).unwrap(), id);
        assert!(from_hex("AB").is_err());
    }

    #[test]
    fn redact_payload_round_trips() {
        let r = RedactPayload {
            target_origin: "peer".into(),
            target_seq: 7,
            target_id: to_hex(&[3u8; 32]),
        };
        assert_eq!(RedactPayload::from_bytes(&r.to_bytes()).unwrap(), r);
        assert_eq!(r.id().unwrap(), [3u8; 32]);
        assert!(RedactPayload::from_bytes(b"[]").is_err());
    }
}
