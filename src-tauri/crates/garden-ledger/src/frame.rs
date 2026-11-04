//! Envelope types and the on-disk frame of the ledger.
//!
//! ```text
//! frame = [u32 LE body_len][body][u32 LE crc32c(body)]
//! body  = [u16 LE header_len][header canonical JSON][payload bytes]
//! ```

use serde::{Deserialize, Serialize};
use std::fmt;

/// blake3 of the canonical header bytes.
pub type Id = [u8; 32];
/// Lowercase hex of an [`Id`] (or of a payload hash), 64 chars.
pub type IdHex = String;

/// Largest permitted frame body (64 MiB).
pub const MAX_BODY: usize = 64 * 1024 * 1024;
/// Current header version.
pub const VERSION: u8 = 1;

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

/// What an event does.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Assert records.
    Assert,
    /// Retract records.
    Retract,
    /// Supersede earlier events.
    Supersede,
    /// Redact an earlier payload.
    Redact,
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
}

/// A header and its payload (the JSON records, opaque here).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Event {
    /// Header.
    pub header: Header,
    /// Payload bytes.
    pub payload: Vec<u8>,
}

/// Result of decoding one frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Decoded {
    /// Header.
    pub header: Header,
    /// Its id.
    pub id: Id,
    /// Payload (zeros when redacted).
    pub payload: Vec<u8>,
    /// True when the payload is zeroed and no longer matches its hash.
    pub redacted: bool,
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
    /// crc32c mismatch.
    Crc,
    /// Header JSON or structure invalid.
    Header(String),
    /// Payload does not match its hash and is not redacted.
    PayloadHash,
    /// Unsupported header version.
    Version(u8),
    /// Body over the 64 MiB limit.
    TooLarge(usize),
    /// Canonicalization failed or stored header is not canonical.
    Canonical(String),
}

impl fmt::Display for FrameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FrameError::Short { needed } => write!(f, "short frame, need {needed} more bytes"),
            FrameError::Crc => write!(f, "frame crc mismatch"),
            FrameError::Header(m) => write!(f, "bad header: {m}"),
            FrameError::PayloadHash => write!(f, "payload hash mismatch"),
            FrameError::Version(v) => write!(f, "unsupported version {v}"),
            FrameError::TooLarge(n) => write!(f, "body of {n} bytes exceeds limit"),
            FrameError::Canonical(m) => write!(f, "canonical form: {m}"),
        }
    }
}

impl std::error::Error for FrameError {}

/// Lowercase hex of an id or hash.
pub fn to_hex(id: &Id) -> IdHex {
    const H: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(64);
    for b in id {
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

/// RFC 8785 canonical JSON bytes of the header.
pub fn canonical_header_bytes(h: &Header) -> Result<Vec<u8>, FrameError> {
    serde_json_canonicalizer::to_vec(h).map_err(|e| FrameError::Canonical(e.to_string()))
}

/// blake3 of the canonical header bytes.
pub fn id_of(h: &Header) -> Result<Id, FrameError> {
    Ok(*blake3::hash(&canonical_header_bytes(h)?).as_bytes())
}

/// Hex blake3 of payload bytes.
pub fn payload_hash(payload: &[u8]) -> IdHex {
    to_hex(blake3::hash(payload).as_bytes())
}

impl Header {
    /// Build a version-1 header for `payload`, filling `payload_hash` and
    /// `payload_len`; `txn` and `sig` are `None`.
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
        })
    }
}

/// Encode an event as one frame. The payload length must match the header.
pub fn encode(ev: &Event) -> Result<Vec<u8>, FrameError> {
    if ev.header.v != VERSION {
        return Err(FrameError::Version(ev.header.v));
    }
    if ev.payload.len() != ev.header.payload_len as usize {
        return Err(FrameError::Header(format!(
            "payload_len {} != payload {}",
            ev.header.payload_len,
            ev.payload.len()
        )));
    }
    let hb = canonical_header_bytes(&ev.header)?;
    let hlen = u16::try_from(hb.len())
        .map_err(|_| FrameError::Header(format!("header of {} bytes exceeds u16", hb.len())))?;
    let body_len = 2 + hb.len() + ev.payload.len();
    if body_len > MAX_BODY {
        return Err(FrameError::TooLarge(body_len));
    }
    let mut out = Vec::with_capacity(8 + body_len);
    out.extend_from_slice(&(body_len as u32).to_le_bytes());
    out.extend_from_slice(&hlen.to_le_bytes());
    out.extend_from_slice(&hb);
    out.extend_from_slice(&ev.payload);
    let crc = crc32c::crc32c(&out[4..4 + body_len]);
    out.extend_from_slice(&crc.to_le_bytes());
    Ok(out)
}

/// Frame geometry after the length and crc checks: (body_len, header_len).
fn check_frame(bytes: &[u8]) -> Result<(usize, usize), FrameError> {
    if bytes.len() < 4 {
        return Err(FrameError::Short {
            needed: 4 - bytes.len(),
        });
    }
    let body_len = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;
    if body_len > MAX_BODY {
        return Err(FrameError::TooLarge(body_len));
    }
    let total = 4 + body_len + 4;
    if bytes.len() < total {
        return Err(FrameError::Short {
            needed: total - bytes.len(),
        });
    }
    let body = &bytes[4..4 + body_len];
    let stored = u32::from_le_bytes([
        bytes[4 + body_len],
        bytes[5 + body_len],
        bytes[6 + body_len],
        bytes[7 + body_len],
    ]);
    if crc32c::crc32c(body) != stored {
        return Err(FrameError::Crc);
    }
    if body_len < 2 {
        return Err(FrameError::Header(
            "body shorter than header length field".into(),
        ));
    }
    let hlen = u16::from_le_bytes([body[0], body[1]]) as usize;
    if 2 + hlen > body_len {
        return Err(FrameError::Header("header length exceeds body".into()));
    }
    Ok((body_len, hlen))
}

/// Decode the first frame in `bytes`; trailing bytes are ignored (see `frame_len`).
pub fn decode(bytes: &[u8]) -> Result<Decoded, FrameError> {
    let (body_len, hlen) = check_frame(bytes)?;
    let body = &bytes[4..4 + body_len];
    let hb = &body[2..2 + hlen];
    let payload = &body[2 + hlen..];
    let header: Header =
        serde_json::from_slice(hb).map_err(|e| FrameError::Header(e.to_string()))?;
    if header.v != VERSION {
        return Err(FrameError::Version(header.v));
    }
    let canon = canonical_header_bytes(&header)?;
    if canon != hb {
        return Err(FrameError::Canonical(
            "stored header is not canonical".into(),
        ));
    }
    if payload.len() != header.payload_len as usize {
        return Err(FrameError::Header(format!(
            "payload_len {} != payload {}",
            header.payload_len,
            payload.len()
        )));
    }
    let redacted = if payload_hash(payload) == header.payload_hash {
        false
    } else if !payload.is_empty() && payload.iter().all(|&b| b == 0) {
        true
    } else {
        return Err(FrameError::PayloadHash);
    };
    let id = *blake3::hash(&canon).as_bytes();
    Ok(Decoded {
        header,
        id,
        payload: payload.to_vec(),
        redacted,
        frame_len: 8 + body_len,
    })
}

/// Zero the payload region of a complete frame at the start of `bytes` and
/// rewrite its crc; the header (hence the id) is untouched. Only the first
/// frame's `frame_len` bytes are modified. Fails like [`decode`] on a short or
/// corrupt frame (it checks length, crc and header bounds first).
pub fn redact_body(bytes: &mut [u8]) -> Result<(), FrameError> {
    let (body_len, hlen) = check_frame(bytes)?;
    let start = 4 + 2 + hlen;
    let end = 4 + body_len;
    for b in &mut bytes[start..end] {
        *b = 0;
    }
    let crc = crc32c::crc32c(&bytes[4..end]);
    bytes[end..end + 4].copy_from_slice(&crc.to_le_bytes());
    Ok(())
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
        assert!(!d.redacted);
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
    fn crc_detects_flip() {
        let mut f = encode(&sample(b"hello")).unwrap();
        let n = f.len();
        f[n - 6] ^= 1;
        assert_eq!(decode(&f), Err(FrameError::Crc));
    }

    #[test]
    fn redaction_keeps_id() {
        let ev = sample(b"secret secret");
        let mut f = encode(&ev).unwrap();
        let before = decode(&f).unwrap();
        redact_body(&mut f).unwrap();
        let after = decode(&f).unwrap();
        assert!(after.redacted);
        assert_eq!(after.id, before.id);
        assert_eq!(after.header, before.header);
        assert!(after.payload.iter().all(|&b| b == 0));
        assert_eq!(after.payload.len(), ev.payload.len());
    }

    #[test]
    fn tampered_payload_is_hash_error() {
        let ev = sample(b"abcdef");
        let mut f = encode(&ev).unwrap();
        let n = f.len();
        f[n - 5] ^= 0x55;
        let crc = crc32c::crc32c(&f[4..n - 4]);
        f[n - 4..].copy_from_slice(&crc.to_le_bytes());
        assert_eq!(decode(&f), Err(FrameError::PayloadHash));
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
}
