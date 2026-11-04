//! The ledger: a directory of per-origin streams, a version vector, diff and apply.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};

use crate::frame::{self, Event, FrameError, Header, Id, Mode, OriginId, Subject};
use crate::hlc::HlcClock;
use crate::stream::{Appended, FrameAt, RawChunk, Recovery, Stream, StreamError};

/// Errors from ledger operations.
#[derive(Debug)]
pub enum LedgerError {
    /// Filesystem failure outside a stream.
    Io(io::Error),
    /// A stream refused the operation.
    Stream(StreamError),
    /// A frame failed to encode or decode.
    Frame(FrameError),
    /// `apply` was asked to write the local origin's stream; only this process appends there.
    LocalOrigin,
    /// An origin id that is invalid, mismatched with its frames, or unknown to this ledger.
    BadOrigin(String),
    /// A peer chunk starts beyond our last seq + 1.
    Gap {
        /// The origin whose stream has the gap.
        origin: OriginId,
        /// The seq we needed next.
        expected: u64,
        /// The seq the chunk offered.
        found: u64,
    },
}

impl fmt::Display for LedgerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LedgerError::Io(e) => write!(f, "ledger io: {e}"),
            LedgerError::Stream(e) => write!(f, "ledger stream: {e}"),
            LedgerError::Frame(e) => write!(f, "ledger frame: {e}"),
            LedgerError::LocalOrigin => write!(f, "refusing to apply frames to the local origin"),
            LedgerError::BadOrigin(s) => write!(f, "bad origin: {s}"),
            LedgerError::Gap {
                origin,
                expected,
                found,
            } => write!(
                f,
                "gap in origin {}: expected seq {expected}, found {found}",
                origin.0
            ),
        }
    }
}

impl std::error::Error for LedgerError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            LedgerError::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for LedgerError {
    fn from(e: io::Error) -> Self {
        LedgerError::Io(e)
    }
}
impl From<StreamError> for LedgerError {
    fn from(e: StreamError) -> Self {
        LedgerError::Stream(e)
    }
}
impl From<FrameError> for LedgerError {
    fn from(e: FrameError) -> Self {
        LedgerError::Frame(e)
    }
}

/// Origin to last seq seen; an absent origin means 0.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VersionVector(pub BTreeMap<OriginId, u64>);

impl VersionVector {
    /// Last seq for `o` (0 if absent).
    pub fn get(&self, o: &OriginId) -> u64 {
        self.0.get(o).copied().unwrap_or(0)
    }

    /// True when for every origin `other` knows, we know at least as much.
    pub fn dominates(&self, other: &Self) -> bool {
        other.0.iter().all(|(o, s)| self.get(o) >= *s)
    }

    /// Per-origin maximum of the two vectors, into `self`.
    pub fn merge(&mut self, other: &Self) {
        for (o, s) in &other.0 {
            let e = self.0.entry(o.clone()).or_insert(0);
            if *s > *e {
                *e = *s;
            }
        }
    }
}

/// What a caller supplies for one local event; the ledger fills in origin, seq, hlc and hashes.
#[derive(Clone, Debug)]
pub struct Draft {
    /// IRI of the observer.
    pub witness: String,
    /// Vocabulary the records belong to.
    pub vocab: String,
    /// Subjects the event is about.
    pub subjects: Vec<Subject>,
    /// How the event bears on its subjects.
    pub mode: Mode,
    /// Opaque payload bytes (the JSON records).
    pub payload: Vec<u8>,
    /// Cross-vocab write marker.
    pub txn: Option<String>,
}

/// A directory of per-origin streams with one local (writable) origin.
pub struct Ledger {
    dir: PathBuf,
    streams: BTreeMap<OriginId, Stream>,
    local: OriginId,
}

/// True when `s` matches `[A-Za-z0-9._-]{1,128}`.
pub fn origin_is_valid(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && s != "."
        && s != ".."
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'_' || b == b'-')
}

/// Fsync a directory so a newly created file's entry is durable.
pub fn sync_dir_once(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        File::open(dir)?.sync_all()
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
        Ok(())
    }
}

fn hex(bytes: &[u8]) -> String {
    const T: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(T[(b >> 4) as usize] as char);
        s.push(T[(b & 15) as usize] as char);
    }
    s
}

/// The event id: blake3 of the canonical (RFC 8785) header bytes.
fn header_id(h: &Header) -> Result<Id, LedgerError> {
    let bytes = serde_json_canonicalizer::to_vec(h)
        .map_err(|e| LedgerError::Io(io::Error::new(io::ErrorKind::InvalidData, e.to_string())))?;
    Ok(*blake3::hash(&bytes).as_bytes())
}

impl Ledger {
    /// Open every `*.log` in `dir` (create dir if missing); recover each; report recoveries.
    pub fn open(
        dir: &Path,
        local: OriginId,
    ) -> Result<(Ledger, Vec<(OriginId, Recovery)>), LedgerError> {
        if !origin_is_valid(&local.0) {
            return Err(LedgerError::BadOrigin(local.0));
        }
        let mut created = false;
        if !dir.is_dir() {
            std::fs::create_dir_all(dir)?;
            created = true;
        }
        let mut names: BTreeSet<String> = BTreeSet::new();
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            if !entry.file_type()?.is_file() {
                continue;
            }
            let name = entry.file_name();
            let name = match name.to_str() {
                Some(n) => n,
                None => continue,
            };
            if let Some(stem) = name.strip_suffix(".log") {
                if !origin_is_valid(stem) {
                    return Err(LedgerError::BadOrigin(stem.to_string()));
                }
                names.insert(stem.to_string());
            }
        }
        if !names.contains(&local.0) {
            created = true;
            names.insert(local.0.clone());
        }
        let mut streams = BTreeMap::new();
        let mut recoveries = Vec::new();
        for name in names {
            let origin = OriginId(name);
            let (stream, rec) = Stream::open(dir, &origin)?;
            recoveries.push((origin.clone(), rec));
            streams.insert(origin, stream);
        }
        if created {
            sync_dir_once(dir)?;
        }
        Ok((
            Ledger {
                dir: dir.to_path_buf(),
                streams,
                local,
            },
            recoveries,
        ))
    }

    /// Last seq per origin (origins with no frames are omitted).
    pub fn version(&self) -> VersionVector {
        let mut m = BTreeMap::new();
        for (o, s) in &self.streams {
            let last = s.next_seq().saturating_sub(1);
            if last > 0 {
                m.insert(o.clone(), last);
            }
        }
        VersionVector(m)
    }

    /// The local origin.
    pub fn local(&self) -> &OriginId {
        &self.local
    }

    /// Build the header (seq = local next, origin = local, hlc from the clock), encode, append to
    /// the local stream.
    pub fn append(
        &mut self,
        clock: &mut HlcClock,
        wall_ms: u64,
        draft: Draft,
    ) -> Result<Appended, LedgerError> {
        let payload_len = u32::try_from(draft.payload.len())
            .map_err(|_| LedgerError::Frame(FrameError::TooLarge(draft.payload.len())))?;
        let payload_hash = hex(blake3::hash(&draft.payload).as_bytes());
        let local = self.local.clone();
        let stream = self
            .streams
            .get_mut(&local)
            .ok_or_else(|| LedgerError::BadOrigin(format!("local stream {} missing", local.0)))?;
        let header = Header {
            v: 1,
            origin: local,
            seq: stream.next_seq(),
            hlc: clock.now(wall_ms),
            witness: draft.witness,
            vocab: draft.vocab,
            subjects: draft.subjects,
            mode: draft.mode,
            payload_hash,
            payload_len,
            txn: draft.txn,
            sig: None,
        };
        let event = Event {
            header,
            payload: draft.payload,
        };
        Ok(stream.append_event(&event)?)
    }

    /// Frames another replica lacks: for each origin where ours > theirs, raw chunks after their seq.
    pub fn diff(
        &self,
        since: &VersionVector,
        max_bytes: usize,
    ) -> Result<Vec<(OriginId, RawChunk)>, LedgerError> {
        let mut out = Vec::new();
        for (o, s) in &self.streams {
            let ours = s.next_seq().saturating_sub(1);
            let theirs = since.get(o);
            if ours > theirs {
                let chunk = s.raw_after(theirs, max_bytes)?;
                if !chunk.bytes.is_empty() {
                    out.push((o.clone(), chunk));
                }
            }
        }
        Ok(out)
    }

    /// Apply raw frames from a peer for one origin: validates, skips seq <= ours (dedup by
    /// position), refuses a gap, appends the rest to that origin's stream. Returns how many frames
    /// were new. Nothing is written unless every frame in `raw` validates.
    pub fn apply(&mut self, origin: &OriginId, raw: &[u8]) -> Result<usize, LedgerError> {
        if *origin == self.local {
            return Err(LedgerError::LocalOrigin);
        }
        if !origin_is_valid(&origin.0) {
            return Err(LedgerError::BadOrigin(origin.0.clone()));
        }
        let ours = self
            .streams
            .get(origin)
            .map(|s| s.next_seq().saturating_sub(1))
            .unwrap_or(0);

        // Pass 1: decode and validate every frame, simulate the seq rule.
        let mut fresh: Vec<(std::ops::Range<usize>, Header)> = Vec::new();
        let mut pos = 0usize;
        let mut expected = ours + 1;
        while pos < raw.len() {
            let d = frame::decode(&raw[pos..])?;
            let end = pos + d.frame_len;
            if d.header.origin != *origin {
                return Err(LedgerError::BadOrigin(format!(
                    "frame origin {} in chunk for {}",
                    d.header.origin.0, origin.0
                )));
            }
            if header_id(&d.header)? != d.id {
                return Err(LedgerError::Frame(FrameError::Header("id mismatch".into())));
            }
            if d.header.seq > ours {
                if d.header.seq != expected {
                    return Err(LedgerError::Gap {
                        origin: origin.clone(),
                        expected,
                        found: d.header.seq,
                    });
                }
                expected += 1;
                fresh.push((pos..end, d.header));
            }
            pos = end;
        }
        if fresh.is_empty() {
            return Ok(0);
        }

        // Pass 2: write.
        let mut created = false;
        if !self.streams.contains_key(origin) {
            let (stream, _rec) = Stream::open(&self.dir, origin)?;
            self.streams.insert(origin.clone(), stream);
            created = true;
        }
        let stream = self
            .streams
            .get_mut(origin)
            .ok_or_else(|| LedgerError::BadOrigin(origin.0.clone()))?;
        for (range, header) in &fresh {
            stream.append(&raw[range.clone()], header)?;
        }
        if created {
            sync_dir_once(&self.dir)?;
        }
        Ok(fresh.len())
    }

    /// Iterate one origin's frames from a byte offset (0 = all).
    pub fn scan(
        &self,
        origin: &OriginId,
        from_offset: u64,
    ) -> Result<impl Iterator<Item = Result<FrameAt, StreamError>> + '_, LedgerError> {
        let s = self
            .streams
            .get(origin)
            .ok_or_else(|| LedgerError::BadOrigin(format!("unknown origin {}", origin.0)))?;
        Ok(s.scan_from(from_offset))
    }

    /// Redact the frame at `offset` in `origin`'s stream (payload zeroed in place).
    pub fn redact(&mut self, origin: &OriginId, offset: u64) -> Result<(), LedgerError> {
        let s = self
            .streams
            .get_mut(origin)
            .ok_or_else(|| LedgerError::BadOrigin(format!("unknown origin {}", origin.0)))?;
        Ok(s.redact_at(offset)?)
    }

    /// Ordered-durability barrier on every stream appended since the last sync.
    pub fn sync_barrier(&self) -> io::Result<()> {
        for s in self.streams.values() {
            if s.is_dirty() {
                s.sync_barrier()?;
            }
        }
        Ok(())
    }

    /// Full flush on every stream appended since the last sync.
    pub fn sync_full(&self) -> io::Result<()> {
        for s in self.streams.values() {
            if s.is_dirty() {
                s.sync_full()?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn o(s: &str) -> OriginId {
        OriginId(s.to_string())
    }

    #[test]
    fn origin_validation() {
        assert!(origin_is_valid("a.b_c-9"));
        assert!(!origin_is_valid(""));
        assert!(!origin_is_valid("a/b"));
        assert!(!origin_is_valid(&"x".repeat(129)));
    }

    #[test]
    fn version_vector_dominates_and_merge() {
        let mut a = VersionVector::default();
        a.0.insert(o("x"), 3);
        let mut b = VersionVector::default();
        b.0.insert(o("x"), 2);
        b.0.insert(o("y"), 1);
        assert!(a.dominates(&VersionVector::default()));
        assert!(!a.dominates(&b));
        a.merge(&b);
        assert!(a.dominates(&b));
        assert_eq!(a.get(&o("x")), 3);
        assert_eq!(a.get(&o("y")), 1);
        assert_eq!(a.get(&o("z")), 0);
    }
}
