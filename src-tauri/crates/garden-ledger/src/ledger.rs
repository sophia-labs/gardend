//! The ledger: a directory of per-origin streams, a version vector, diff and apply.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};

use crate::frame::{
    self, Event, FrameError, Header, Id, Mode, OriginId, PayloadState, RedactPayload, Subject,
};
use crate::hlc::HlcClock;
use crate::stream::{
    repair_redactions, scan_file, Appended, FrameAt, RawChunk, Recovery, Stream, StreamError,
};

/// Errors from ledger operations.
#[derive(Debug)]
pub enum LedgerError {
    /// Filesystem failure outside a stream.
    Io(io::Error),
    /// A stream refused the operation.
    Stream(StreamError),
    /// A frame failed to encode or decode.
    Frame(FrameError),
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
    /// A peer's frame at `seq` has a different id from ours: the origin wrote two different events
    /// at one position. Nothing was written.
    Fork {
        /// The origin that forked.
        origin: OriginId,
        /// The position both frames claim.
        seq: u64,
        /// The id we hold at that position.
        ours: Id,
        /// The id the peer sent.
        theirs: Id,
    },
    /// No frame at that position.
    NoSuchFrame {
        /// The origin asked about.
        origin: OriginId,
        /// The missing position.
        seq: u64,
    },
}

impl fmt::Display for LedgerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LedgerError::Io(e) => write!(f, "ledger io: {e}"),
            LedgerError::Stream(e) => write!(f, "ledger stream: {e}"),
            LedgerError::Frame(e) => write!(f, "ledger frame: {e}"),
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
            LedgerError::Fork {
                origin,
                seq,
                ours,
                theirs,
            } => write!(
                f,
                "fork in origin {} at seq {seq}: ours {}, theirs {}",
                origin.0,
                frame::to_hex(ours),
                frame::to_hex(theirs)
            ),
            LedgerError::NoSuchFrame { origin, seq } => {
                write!(f, "no frame at seq {seq} in origin {}", origin.0)
            }
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

impl Ledger {
    /// Open every `*.log` in `dir` (create dir if missing); recover each; report recoveries.
    ///
    /// Recovery is two-phase because a redaction can name a frame in another origin's stream. Every
    /// file is read first (no writes); then every `redact` frame's target is re-zeroed if it is not
    /// already (the redact frame is durable before its target is ever touched, so a half-zeroed
    /// target always has its redact frame on disk); only then is each stream judged, so a target that
    /// tore at the tail of a peer's file is repaired rather than cut.
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
        let mut scans = Vec::with_capacity(names.len());
        for name in names {
            scans.push(scan_file(dir, &OriginId(name))?);
        }
        repair_redactions(&mut scans)?;
        let mut streams = BTreeMap::new();
        let mut recoveries = Vec::new();
        for scan in scans {
            let origin = scan.origin().clone();
            let (stream, rec) = scan.finish()?;
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

    /// Read-only view of one origin's stream (its durability flags, lengths, lookups).
    pub fn stream(&self, origin: &OriginId) -> Option<&Stream> {
        self.streams.get(origin)
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
        let payload_hash = frame::payload_hash(&draft.payload);
        let local = self.local.clone();
        let stream = self
            .streams
            .get_mut(&local)
            .ok_or_else(|| LedgerError::BadOrigin(format!("local stream {} missing", local.0)))?;
        let header = Header {
            v: frame::VERSION,
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
            extra: BTreeMap::new(),
        };
        let event = Event {
            header,
            payload: draft.payload,
        };
        Ok(stream.append_event(&event)?)
    }

    /// Frames another replica lacks: for each origin where ours > theirs, raw chunks after their seq.
    /// Only frames a barrier has covered are shipped (see [`Stream::raw_after`]), so call
    /// `sync_barrier` first if freshly appended frames should go.
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

    /// Apply raw frames from a peer for one origin.
    ///
    /// Every frame must decode with both crcs and carry `origin`, and the chunk's seqs must be
    /// consecutive and start at or below our next seq (else [`LedgerError::Gap`]). Frames at positions
    /// we already hold are compared by id: a different id is a [`LedgerError::Fork`] and nothing is
    /// written; the same id is a duplicate and is skipped. The rest are appended. Nothing is written
    /// unless every frame validates. Returns how many frames were new.
    ///
    /// `origin` may be the local origin: that is how a replica gets back its own frames that a crash cut
    /// from its tail but a peer kept (they extend our tail contiguously and match on any overlap).
    /// New `redact` frames zero the local copy of their target, after a barrier on the stream they were
    /// appended to.
    pub fn apply(&mut self, origin: &OriginId, raw: &[u8]) -> Result<usize, LedgerError> {
        if !origin_is_valid(&origin.0) {
            return Err(LedgerError::BadOrigin(origin.0.clone()));
        }
        let ours = self
            .streams
            .get(origin)
            .map(|s| s.next_seq().saturating_sub(1))
            .unwrap_or(0);

        // Pass 1: decode and validate every frame, check the seq chain, collect what overlaps.
        let mut overlap: Vec<(u64, Id)> = Vec::new();
        let mut fresh: Vec<(std::ops::Range<usize>, Header)> = Vec::new();
        let mut redactions: Vec<RedactPayload> = Vec::new();
        let mut pos = 0usize;
        let mut prev: Option<u64> = None;
        while pos < raw.len() {
            let d = frame::decode(&raw[pos..])?;
            let end = pos + d.frame_len;
            if d.header.origin != *origin {
                return Err(LedgerError::BadOrigin(format!(
                    "frame origin {} in chunk for {}",
                    d.header.origin.0, origin.0
                )));
            }
            let expected = prev.map_or(ours + 1, |p| p + 1);
            let seq = d.header.seq;
            let in_chain = match prev {
                Some(_) => seq == expected,
                None => seq <= expected,
            };
            if !in_chain {
                return Err(LedgerError::Gap {
                    origin: origin.clone(),
                    expected,
                    found: seq,
                });
            }
            prev = Some(seq);
            if seq <= ours {
                overlap.push((seq, d.id));
            } else {
                if d.header.mode == Mode::Redact && d.state == PayloadState::Intact {
                    if let Ok(r) = RedactPayload::from_bytes(&d.payload) {
                        redactions.push(r);
                    }
                }
                fresh.push((pos..end, d.header));
            }
            pos = end;
        }

        // Overlap: same position must mean the same event.
        if let (Some(&(first, _)), Some(&(last, _))) = (overlap.first(), overlap.last()) {
            let stream = self
                .streams
                .get(origin)
                .ok_or_else(|| LedgerError::BadOrigin(origin.0.clone()))?;
            let held = stream.ids_range(first, last)?;
            for ((seq, theirs), mine) in overlap.iter().zip(&held) {
                if mine != theirs {
                    return Err(LedgerError::Fork {
                        origin: origin.clone(),
                        seq: *seq,
                        ours: *mine,
                        theirs: *theirs,
                    });
                }
            }
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
        if !redactions.is_empty() {
            // The redact frames must be durable before any target is touched.
            self.streams[origin].sync_barrier()?;
            for r in &redactions {
                self.zero_target(r)?;
            }
        }
        Ok(fresh.len())
    }

    /// Zero the payload of the frame a `redact` payload names, when we hold it and its id matches.
    fn zero_target(&mut self, r: &RedactPayload) -> Result<(), LedgerError> {
        let Ok(id) = r.id() else { return Ok(()) };
        let Some(stream) = self.streams.get_mut(&OriginId(r.target_origin.clone())) else {
            return Ok(());
        };
        if let Some(loc) = stream.locate(r.target_seq)? {
            if loc.id == id {
                stream.redact_at(loc.offset)?;
            }
        }
        Ok(())
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

    /// Redact the payload of frame `(target_origin, target_seq)`, durably, in two steps.
    ///
    /// 1. Append a `redact` frame to the local stream (payload `{target_origin, target_seq,
    ///    target_id}`, subjects copied from the target with the target as parent) and barrier it.
    /// 2. Zero the target's payload and rewrite its payload crc in place (one write; the header, the id
    ///    and the file length are unchanged).
    ///
    /// A crash between or during the steps is repaired by `Ledger::open`, which re-zeroes every redact
    /// frame's target. The target may be in another origin's stream. Returns the redact frame.
    pub fn redact(
        &mut self,
        clock: &mut HlcClock,
        wall_ms: u64,
        witness: String,
        vocab: String,
        target_origin: &OriginId,
        target_seq: u64,
    ) -> Result<Appended, LedgerError> {
        let target = self
            .streams
            .get(target_origin)
            .ok_or_else(|| LedgerError::BadOrigin(format!("unknown origin {}", target_origin.0)))?;
        let loc = target
            .locate(target_seq)?
            .ok_or_else(|| LedgerError::NoSuchFrame {
                origin: target_origin.clone(),
                seq: target_seq,
            })?;
        let target_header = target.header_at(&loc)?;
        let id_hex = frame::to_hex(&loc.id);
        let subjects = target_header
            .subjects
            .into_iter()
            .map(|s| Subject {
                subject: s.subject,
                class: s.class,
                parents: vec![id_hex.clone()],
            })
            .collect();
        let payload = RedactPayload {
            target_origin: target_origin.0.clone(),
            target_seq,
            target_id: id_hex,
        }
        .to_bytes();
        let appended = self.append(
            clock,
            wall_ms,
            Draft {
                witness,
                vocab,
                subjects,
                mode: Mode::Redact,
                payload,
                txn: None,
            },
        )?;
        let local = self.streams.get(&self.local).ok_or_else(|| {
            LedgerError::BadOrigin(format!("local stream {} missing", self.local.0))
        })?;
        local.sync_barrier()?;
        let target = self
            .streams
            .get_mut(target_origin)
            .ok_or_else(|| LedgerError::BadOrigin(format!("unknown origin {}", target_origin.0)))?;
        target.redact_at(loc.offset)?;
        Ok(appended)
    }

    /// Ordered-durability barrier on every stream appended to since the last barrier. Each stream that
    /// had writes then owes a full flush (see [`Stream::full_owed`]).
    pub fn sync_barrier(&self) -> io::Result<()> {
        for s in self.streams.values() {
            if s.dirty_since_barrier() {
                s.sync_barrier()?;
            }
        }
        Ok(())
    }

    /// Full flush on every stream that appended or redacted since its last full flush: the ones that
    /// owe one after a barrier, and the ones never barriered at all.
    pub fn sync_full(&self) -> io::Result<()> {
        for s in self.streams.values() {
            if s.full_owed() || s.dirty_since_barrier() {
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
