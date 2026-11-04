//! One origin's append-only file: `{ledger_dir}/{origin}.log`.
//!
//! A stream owns a single writer identity. Frames are appended with one `write_all` on a file opened
//! `O_APPEND`; nothing is cached except a sparse `seq -> offset` index (every 64th frame) rebuilt by the
//! open-time scan. The stream never syncs by itself after open: `sync_barrier` / `sync_full` belong to the
//! host's cadence, and two flags tell the ledger what each stream still owes (see
//! [`Stream::dirty_since_barrier`]).
//!
//! # One writer, enforced
//!
//! `Stream::open` takes an exclusive advisory `flock` on the stream file and the `Stream` holds it until it
//! is dropped. A second open of the same file, in this process or another, fails with
//! [`StreamError::Locked`] naming the path. The append path truncates a half-written frame by its in-memory
//! length, which is only sound with a single writer; the lock is what makes that true rather than hoped.
//!
//! # Shipped only once it cannot be lost
//!
//! A frame may be shipped to a peer only after a full flush (`sync_full`) covers it. A barrier orders
//! writes but on macOS does not make them durable; a peer that held a frame its origin then lost in a
//! crash would see the origin mint a different event at the same seq, and the replicas would disagree
//! forever. So the stream keeps [`Stream::durable_len`], advanced only by a successful `sync_full` (and
//! at open, by an fsync of the recovered file; until that fsync it is 0), and [`Stream::raw_after`] and
//! [`Stream::raw_after_with_tip`] never read past it. The writer's ack rides the barrier; the ship rides
//! the full flush.
//!
//! # Recovery on open
//!
//! The scan reads every frame once. On the first frame that cannot be accepted at offset `X` (short,
//! bad header crc, bad lengths, bad payload crc) it looks for evidence that the damage is not a torn
//! tail: a decodable frame of this origin anywhere in `(X, file_len)`. If one exists the stream refuses
//! to open with [`StreamError::Corrupt`] at `X` and the file is untouched; if none exists the file is cut
//! at `X`. A zero-filled tail therefore falls out as a cut. The resync scan reads the rest of the file,
//! so it is O(file) in the corrupt case only; the clean path is the single pass. A frame whose header is
//! good but whose payload crc fails is held as a *suspect*: it may be a redaction that tore, which the
//! ledger repairs from the durable `redact` frame before the stream is judged (see `ledger.rs`).
//!
//! A damaged length prefix on the very last frame is indistinguishable from a tear and is cut.
//!
//! # A failed full flush poisons the stream
//!
//! After `fsync` fails the kernel may have dropped the dirty pages and cleared the error, so a retry
//! can report success for bytes that never reached the disk. A `Stream` therefore never retries: the
//! first failed `sync_full` marks it poisoned ([`Stream::is_poisoned`]). From then on `append`,
//! `redact_at`, `sync_barrier` and `sync_full` refuse ([`StreamError::Poisoned`], surfaced inside an
//! `io::Error` by the two sync calls: see [`StreamError::io_is_poisoned`]), and [`Stream::durable_len`]
//! stays where it was, so nothing the failed flush may have lost is ever shipped. Reads still work.
//! Only reopening the ledger (which rescans the file and fsyncs what it finds) clears the poison.

use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{self, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::frame::{
    decode, encode, inspect, redact_body, Decoded, Event, FrameError, Header, Id, Mode, OriginId,
    RedactPayload, LEN_PREFIX, MAX_BODY,
};
use crate::sync::{
    sync_barrier as fs_sync_barrier, sync_dir, sync_full as fs_sync_full, try_lock_exclusive,
};

/// Every `INDEX_EVERY`th frame (seq 1, 65, 129, ...) is remembered in the sparse index.
const INDEX_EVERY: u64 = 64;
/// Bytes the resync scan reads per step.
const RESYNC_WINDOW: usize = 1 << 20;
/// Largest candidate the resync scan must see whole: prefix, `hdr_len`, a full header, `hdr_crc`.
const RESYNC_MARGIN: usize = LEN_PREFIX + 2 + u16::MAX as usize + 4;

/// Errors a stream can report.
#[derive(Debug)]
pub enum StreamError {
    /// An underlying I/O failure.
    Io(io::Error),
    /// A frame at `offset` failed to decode.
    Frame {
        /// Byte offset of the frame.
        offset: u64,
        /// The envelope error.
        source: FrameError,
    },
    /// A damaged frame at `offset` with decodable frames after it (or a length no writer produces):
    /// the stream refuses to open and nothing is cut.
    Corrupt {
        /// Byte offset of the damaged frame.
        offset: u64,
    },
    /// The frame at `offset` carries a seq that is not the previous one plus one.
    Gap {
        /// Byte offset of the frame.
        offset: u64,
        /// The seq the scan expected.
        expected: u64,
        /// The seq the frame carried.
        found: u64,
    },
    /// An append whose header seq is not `next_seq`.
    Seq {
        /// The stream's next seq.
        expected: u64,
        /// The seq the header carried.
        found: u64,
    },
    /// A frame whose header origin is not this stream's origin.
    Origin {
        /// This stream's origin.
        expected: String,
        /// The origin the header carried.
        found: String,
    },
    /// An offset that is past the end of the stream.
    BadOffset(u64),
    /// An earlier `sync_full` failed, so what reached the disk is unknown: the stream refuses writes
    /// and flushes until it is reopened (see the module docs).
    Poisoned,
    /// The stream file is already open for writing (a second `Stream` in this process, or another
    /// process): one writer per stream.
    Locked {
        /// The stream file that is locked.
        path: PathBuf,
    },
}

impl fmt::Display for StreamError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StreamError::Io(e) => write!(f, "stream i/o error: {e}"),
            StreamError::Frame { offset, source } => {
                write!(f, "bad frame at offset {offset}: {source:?}")
            }
            StreamError::Corrupt { offset } => write!(f, "corrupt frame at offset {offset}"),
            StreamError::Gap {
                offset,
                expected,
                found,
            } => write!(
                f,
                "seq gap at offset {offset}: expected {expected}, found {found}"
            ),
            StreamError::Seq { expected, found } => {
                write!(f, "append seq mismatch: expected {expected}, found {found}")
            }
            StreamError::Origin { expected, found } => {
                write!(f, "origin mismatch: expected {expected}, found {found}")
            }
            StreamError::BadOffset(o) => write!(f, "offset {o} is past the end of the stream"),
            StreamError::Poisoned => write!(
                f,
                "stream is poisoned by a failed full flush: reopen the ledger"
            ),
            StreamError::Locked { path } => write!(
                f,
                "stream {} is locked: it is already open for writing (one writer per stream)",
                path.display()
            ),
        }
    }
}

impl std::error::Error for StreamError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            StreamError::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl StreamError {
    /// True when `e` is the `io::Error` a poisoned stream's `sync_barrier` or `sync_full` returns
    /// (it wraps [`StreamError::Poisoned`]).
    pub fn io_is_poisoned(e: &io::Error) -> bool {
        e.get_ref()
            .and_then(|inner| inner.downcast_ref::<StreamError>())
            .is_some_and(|s| matches!(s, StreamError::Poisoned))
    }
}

/// The error a poisoned stream's sync calls return.
fn poisoned_io() -> io::Error {
    io::Error::other(StreamError::Poisoned)
}

impl From<io::Error> for StreamError {
    fn from(e: io::Error) -> Self {
        StreamError::Io(e)
    }
}

/// What an append produced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Appended {
    /// The frame's seq.
    pub seq: u64,
    /// Byte offset of the frame in the stream file.
    pub offset: u64,
    /// The frame's id (blake3 of its stored header bytes).
    pub id: Id,
}

/// What `Stream::open` did to the file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Recovery {
    /// Every byte belonged to a valid frame.
    Clean,
    /// A torn tail was cut: the file now ends at `from`.
    TruncatedTail {
        /// Offset the file was truncated to.
        from: u64,
        /// Bytes removed.
        dropped_bytes: u64,
    },
    /// `redactions` redact frames named a target whose payload was not yet zero (a redaction that
    /// tore, or crashed before it began); the targets were re-zeroed. Reported only when no tail was
    /// cut, which is reported instead.
    Repaired {
        /// Targets re-zeroed in this stream.
        redactions: usize,
    },
}

/// One decoded frame with the offset it sits at.
#[derive(Clone, Debug)]
pub struct FrameAt {
    /// Byte offset of the frame in the stream file.
    pub offset: u64,
    /// The decoded frame.
    pub decoded: Decoded,
}

/// Where a frame sits and what its id is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Located {
    /// The frame's seq.
    pub seq: u64,
    /// Byte offset of the frame in the stream file.
    pub offset: u64,
    /// Bytes the frame occupies.
    pub frame_len: u64,
    /// blake3 of the stored header bytes.
    pub id: Id,
}

/// A run of raw frames, what sync ships.
///
/// An empty chunk (nothing shippable after the requested seq) has empty `bytes`,
/// `from_seq == to_seq == seq` (the seq asked for) and `more == false`. A chunk made by
/// [`Stream::raw_after_with_tip`] starts at the peer's tip frame, so `from_seq` is the seq asked for
/// and the receiver compares that frame's id before it appends the rest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RawChunk {
    /// Concatenated raw frames.
    pub bytes: Vec<u8>,
    /// Seq of the first frame in `bytes`.
    pub from_seq: u64,
    /// Seq of the last frame in `bytes`.
    pub to_seq: u64,
    /// True when frames remain after `to_seq` (below the durable length).
    pub more: bool,
}

/// One origin's append-only file.
pub struct Stream {
    path: PathBuf,
    file: File,
    origin: OriginId,
    next_seq: u64,
    len: u64,
    last_offset: Option<u64>,
    /// `(seq, offset)` of every 64th frame, starting at seq 1.
    index: Vec<(u64, u64)>,
    /// Bytes appended or redacted since the last barrier or full flush.
    dirty_since_barrier: AtomicBool,
    /// A barrier has covered writes that no full flush has yet.
    full_owed: AtomicBool,
    /// Bytes of the file covered by the last successful barrier or full flush (or present at open).
    /// Barrier bookkeeping only: nothing ships on the strength of it.
    synced_len: AtomicU64,
    /// Bytes of the file a successful full flush has covered, the only bound sync ships below. 0 until
    /// open has fsynced the recovered file.
    durable_len: AtomicU64,
    /// A full flush failed: refuse writes and flushes until reopen (see the module docs).
    poisoned: AtomicBool,
}

enum Raw {
    Eof,
    Short,
    TooLarge,
    Frame(Vec<u8>),
}

/// Read the raw bytes of the frame starting at `offset`, never reading past `limit`.
fn read_raw<R: Read>(r: &mut R, offset: u64, limit: u64) -> io::Result<Raw> {
    if offset >= limit {
        return Ok(Raw::Eof);
    }
    let avail = limit - offset;
    if avail < LEN_PREFIX as u64 {
        return Ok(Raw::Short);
    }
    let mut prefix = [0u8; LEN_PREFIX];
    r.read_exact(&mut prefix)?;
    let body_len = u32::from_le_bytes(prefix) as u64;
    if body_len > MAX_BODY as u64 {
        return Ok(Raw::TooLarge);
    }
    let total = body_len + LEN_PREFIX as u64;
    if total > avail {
        return Ok(Raw::Short);
    }
    let mut buf = vec![0u8; total as usize];
    buf[..LEN_PREFIX].copy_from_slice(&prefix);
    r.read_exact(&mut buf[LEN_PREFIX..])?;
    Ok(Raw::Frame(buf))
}

fn valid_origin(origin: &OriginId) -> bool {
    origin.validate().is_ok()
}

#[cfg(unix)]
fn write_all_at(f: &File, buf: &[u8], offset: u64) -> io::Result<()> {
    use std::os::unix::fs::FileExt;
    f.write_all_at(buf, offset)
}

#[cfg(windows)]
fn write_all_at(f: &File, mut buf: &[u8], mut offset: u64) -> io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !buf.is_empty() {
        let n = f.seek_write(buf, offset)?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "seek_write wrote 0",
            ));
        }
        buf = &buf[n..];
        offset += n as u64;
    }
    Ok(())
}

/// Zero the payload of the frame in `bytes` (read from `offset`) and write the changed region
/// (payload through its crc) back with one write on a separate non-append handle.
///
/// Returns whether anything changed. The header and its crc are never written.
fn redact_in_place(path: &Path, offset: u64, bytes: &mut [u8]) -> Result<bool, StreamError> {
    match redact_body(bytes).map_err(|source| StreamError::Frame { offset, source })? {
        None => Ok(false),
        Some(range) => {
            let w = OpenOptions::new().write(true).open(path)?;
            write_all_at(&w, &bytes[range.clone()], offset + range.start as u64)?;
            Ok(true)
        }
    }
}

/// Find the frames numbered `first..=last` (all below `limit`) from the sparse index and a forward
/// walk that reads only the length prefixes of the frames it skips and the header of the ones it wants.
fn walk(
    path: &Path,
    index: &[(u64, u64)],
    limit: u64,
    first: u64,
    last: u64,
) -> Result<Vec<Located>, StreamError> {
    let mut out = Vec::new();
    if first == 0 || first > last {
        return Ok(out);
    }
    let pos = index.partition_point(|&(s, _)| s <= first);
    let (mut seq, mut offset) = if pos == 0 { (1, 0) } else { index[pos - 1] };
    let mut r = BufReader::new(File::open(path)?);
    r.seek(SeekFrom::Start(offset))?;
    while seq <= last {
        if offset + LEN_PREFIX as u64 > limit {
            break;
        }
        let mut prefix = [0u8; LEN_PREFIX];
        r.read_exact(&mut prefix)?;
        let body_len = u32::from_le_bytes(prefix) as u64;
        let frame_len = body_len + LEN_PREFIX as u64;
        if body_len > MAX_BODY as u64 || offset + frame_len > limit || body_len < 10 {
            return Err(StreamError::Corrupt { offset });
        }
        let mut skip = body_len;
        if seq >= first {
            let mut hl = [0u8; 2];
            r.read_exact(&mut hl)?;
            let hlen = u16::from_le_bytes(hl) as u64;
            if 10 + hlen > body_len {
                return Err(StreamError::Corrupt { offset });
            }
            let mut hb = vec![0u8; hlen as usize];
            r.read_exact(&mut hb)?;
            out.push(Located {
                seq,
                offset,
                frame_len,
                id: *blake3::hash(&hb).as_bytes(),
            });
            skip -= 2 + hlen;
        }
        r.seek_relative(skip as i64)?;
        offset += frame_len;
        seq += 1;
    }
    Ok(out)
}

/// True when some offset in `[from, file_len)` starts something that decodes as a frame header of
/// `origin`: a plausible `hdr_len`, a header whose crc holds, and `origin` equal. This is the evidence
/// that a damaged frame before `from` has company and is therefore corruption, not a torn tail.
fn resync_finds_frame(
    path: &Path,
    from: u64,
    file_len: u64,
    origin: &OriginId,
) -> io::Result<bool> {
    #[derive(serde::Deserialize)]
    struct OriginOnly {
        origin: OriginId,
    }
    let mut f = File::open(path)?;
    let mut pos = from;
    while pos < file_len {
        let want = ((file_len - pos) as usize).min(RESYNC_WINDOW + RESYNC_MARGIN);
        f.seek(SeekFrom::Start(pos))?;
        let mut buf = vec![0u8; want];
        f.read_exact(&mut buf)?;
        let at_eof = pos + want as u64 >= file_len;
        let starts = if at_eof { want } else { RESYNC_WINDOW };
        for y in 0..starts {
            if y + LEN_PREFIX + 2 > buf.len() {
                break;
            }
            let hlen = u16::from_le_bytes([buf[y + 4], buf[y + 5]]) as usize;
            let hdr_end = y + 6 + hlen;
            if hlen < 2 || hdr_end + 4 > buf.len() {
                continue;
            }
            // A cheap prefilter; the crc below decides. Whitespace around the object is allowed so a
            // writer that pads its header is still recognised as company for a damaged frame.
            let hdr = &buf[y + 6..hdr_end];
            if hdr.trim_ascii_start().first() != Some(&b'{')
                || hdr.trim_ascii_end().last() != Some(&b'}')
            {
                continue;
            }
            let stored = u32::from_le_bytes([
                buf[hdr_end],
                buf[hdr_end + 1],
                buf[hdr_end + 2],
                buf[hdr_end + 3],
            ]);
            if crc32c::crc32c(&buf[y + 4..hdr_end]) != stored {
                continue;
            }
            if let Ok(o) = serde_json::from_slice::<OriginOnly>(&buf[y + 6..hdr_end]) {
                if o.origin == *origin {
                    return Ok(true);
                }
            }
        }
        if at_eof {
            break;
        }
        pos += RESYNC_WINDOW as u64;
    }
    Ok(false)
}

/// The result of reading a stream file front to back, before any repair or cut.
pub(crate) struct Scan {
    path: PathBuf,
    /// The append handle, locked exclusively for the life of the stream it becomes.
    file: File,
    origin: OriginId,
    file_len: u64,
    /// Seq the next frame must carry.
    next_seq: u64,
    /// End of the accepted frames.
    end: u64,
    last_offset: Option<u64>,
    prev_offset: Option<u64>,
    index: Vec<(u64, u64)>,
    /// Frames with a good header and a bad payload crc.
    suspects: Vec<u64>,
    /// Redact frames found, with their parsed targets.
    redactions: Vec<RedactPayload>,
    /// Where a torn tail begins, when the scan stopped on a failure with nothing after it.
    cut: Option<u64>,
    /// Targets re-zeroed in this file by `repair_redactions`.
    repaired: usize,
}

impl Scan {
    pub(crate) fn origin(&self) -> &OriginId {
        &self.origin
    }

    /// The redact frames found in this file.
    pub(crate) fn redactions(&self) -> &[RedactPayload] {
        &self.redactions
    }
}

/// Open (creating if missing), lock and read one stream file. Writes nothing but the empty file.
pub(crate) fn scan_file(dir: &Path, origin: &OriginId) -> Result<Scan, StreamError> {
    if !valid_origin(origin) {
        return Err(StreamError::Io(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid origin id {:?}", origin.0),
        )));
    }
    std::fs::create_dir_all(dir)?;
    let path = dir.join(format!("{}.log", origin.0));
    let existed = path.exists();
    let file = OpenOptions::new().create(true).append(true).open(&path)?;
    if !existed {
        sync_dir(dir)?;
    }
    if !try_lock_exclusive(&file)? {
        return Err(StreamError::Locked { path });
    }
    let file_len = std::fs::metadata(&path)?.len();
    let mut reader = BufReader::new(File::open(&path)?);
    let mut s = Scan {
        path,
        file,
        origin: origin.clone(),
        file_len,
        next_seq: 1,
        end: 0,
        last_offset: None,
        prev_offset: None,
        index: Vec::new(),
        suspects: Vec::new(),
        redactions: Vec::new(),
        cut: None,
        repaired: 0,
    };

    loop {
        let offset = s.end;
        let raw = match read_raw(&mut reader, offset, file_len)? {
            Raw::Eof => break,
            Raw::Short => {
                fail_at(&mut s, offset)?;
                break;
            }
            Raw::TooLarge => return Err(StreamError::Corrupt { offset }),
            Raw::Frame(raw) => raw,
        };
        let ins = match inspect(&raw) {
            Ok(i) => i,
            Err(
                FrameError::Short { .. }
                | FrameError::HeaderCrc
                | FrameError::PayloadCrc
                | FrameError::Geometry(_),
            ) => {
                fail_at(&mut s, offset)?;
                break;
            }
            Err(FrameError::Version(v)) => {
                return Err(StreamError::Frame {
                    offset,
                    source: FrameError::Version(v),
                })
            }
            // A frame whose crc holds but whose header means nothing is not a tear.
            Err(_) => return Err(StreamError::Corrupt { offset }),
        };
        if ins.header.origin != *origin {
            return Err(StreamError::Origin {
                expected: origin.0.clone(),
                found: ins.header.origin.0.clone(),
            });
        }
        if ins.header.seq != s.next_seq {
            return Err(StreamError::Gap {
                offset,
                expected: s.next_seq,
                found: ins.header.seq,
            });
        }
        if !ins.payload_crc_ok {
            s.suspects.push(offset);
        } else if ins.header.mode == Mode::Redact {
            if let Ok(r) = RedactPayload::from_bytes(ins.payload) {
                s.redactions.push(r);
            }
        }
        if s.next_seq % INDEX_EVERY == 1 {
            s.index.push((s.next_seq, offset));
        }
        s.prev_offset = s.last_offset;
        s.last_offset = Some(offset);
        s.next_seq += 1;
        s.end = offset + raw.len() as u64;
    }
    Ok(s)
}

/// The first unacceptable frame is at `offset`: corruption if a decodable frame follows, else a tail.
fn fail_at(s: &mut Scan, offset: u64) -> Result<(), StreamError> {
    if resync_finds_frame(&s.path, offset + 1, s.file_len, &s.origin)? {
        return Err(StreamError::Corrupt { offset });
    }
    s.cut = Some(offset);
    Ok(())
}

/// Re-zero every redact frame's target (in any of `scans`) whose payload is not already zero.
///
/// The redact frame is appended and barriered before the target is touched, so on open it is on disk
/// whenever a half-zeroed target is: the repair is idempotent and never needs to guess. A target in a
/// stream not among `scans`, past what was accepted, or whose id differs from the one named is left alone.
pub(crate) fn repair_redactions(scans: &mut [Scan]) -> Result<(), StreamError> {
    let work: Vec<RedactPayload> = scans
        .iter()
        .flat_map(|s| s.redactions.iter().cloned())
        .collect();
    for r in work {
        let Some(ti) = scans.iter().position(|s| s.origin.0 == r.target_origin) else {
            continue;
        };
        let Ok(id) = r.id() else { continue };
        let t = &mut scans[ti];
        if r.target_seq == 0 || r.target_seq >= t.next_seq {
            continue;
        }
        let found = walk(&t.path, &t.index, t.end, r.target_seq, r.target_seq)?;
        let Some(l) = found.first() else { continue };
        if l.id != id {
            continue;
        }
        if zero_target(&t.path, l)? {
            t.repaired += 1;
        }
    }
    Ok(())
}

/// Read the located frame and zero its payload if it is not already.
fn zero_target(path: &Path, l: &Located) -> Result<bool, StreamError> {
    let mut f = File::open(path)?;
    f.seek(SeekFrom::Start(l.offset))?;
    let mut bytes = vec![0u8; l.frame_len as usize];
    f.read_exact(&mut bytes)?;
    drop(f);
    redact_in_place(path, l.offset, &mut bytes)
}

impl Scan {
    /// Judge the suspects, cut what must be cut, fsync the recovered file, and open the stream for
    /// appending. Only after that fsync does the stream count its bytes as durable (shippable).
    pub(crate) fn finish(self) -> Result<(Stream, Recovery), StreamError> {
        let (stream, recovery) = self.finish_unsynced()?;
        stream.sync_full()?;
        Ok((stream, recovery))
    }

    /// `finish` without the closing fsync: `durable_len` is 0, so nothing ships until `sync_full`.
    pub(crate) fn finish_unsynced(mut self) -> Result<(Stream, Recovery), StreamError> {
        let mut bad: Vec<u64> = Vec::new();
        if !self.suspects.is_empty() {
            let mut f = File::open(&self.path)?;
            for &off in &self.suspects {
                f.seek(SeekFrom::Start(off))?;
                match read_raw(&mut f, off, self.end)? {
                    Raw::Frame(raw) => match inspect(&raw) {
                        Ok(i) if i.payload_crc_ok => {}
                        _ => bad.push(off),
                    },
                    _ => bad.push(off),
                }
            }
        }
        let mut cut = self.cut;
        if !bad.is_empty() {
            // Only the last accepted frame can be a tail; a bad payload anywhere earlier has company.
            if bad.len() > 1 || Some(bad[0]) != self.last_offset {
                return Err(StreamError::Corrupt { offset: bad[0] });
            }
            self.next_seq -= 1;
            self.end = bad[0];
            self.last_offset = self.prev_offset;
            if self.index.last().map(|&(s, _)| s) == Some(self.next_seq) {
                self.index.pop();
            }
            cut = Some(bad[0]);
        }
        let recovery = match cut {
            Some(from) => {
                let w = OpenOptions::new().write(true).open(&self.path)?;
                w.set_len(from)?;
                fs_sync_full(&w)?;
                Recovery::TruncatedTail {
                    from,
                    dropped_bytes: self.file_len - from,
                }
            }
            None if self.repaired > 0 => Recovery::Repaired {
                redactions: self.repaired,
            },
            None => Recovery::Clean,
        };
        Ok((
            Stream {
                path: self.path,
                file: self.file,
                origin: self.origin,
                next_seq: self.next_seq,
                len: self.end,
                last_offset: self.last_offset,
                index: self.index,
                dirty_since_barrier: AtomicBool::new(self.repaired > 0),
                full_owed: AtomicBool::new(false),
                synced_len: AtomicU64::new(self.end),
                durable_len: AtomicU64::new(0),
                poisoned: AtomicBool::new(false),
            },
            recovery,
        ))
    }
}

impl Stream {
    /// Open (create if missing), lock, scan every frame, recover (see the module docs), fsync the
    /// recovered file, and leave it positioned for `O_APPEND`. Redact frames in this file repair
    /// targets in this file; a ledger repairs targets across its streams in `Ledger::open`.
    ///
    /// The caller must be the only writer: the file is locked exclusively until the `Stream` is dropped,
    /// and a second open fails with [`StreamError::Locked`].
    pub fn open(dir: &Path, origin: &OriginId) -> Result<(Stream, Recovery), StreamError> {
        let mut scans = [scan_file(dir, origin)?];
        repair_redactions(&mut scans)?;
        let [scan] = scans;
        scan.finish()
    }

    /// The writer identity of this stream.
    pub fn origin(&self) -> &OriginId {
        &self.origin
    }

    /// The seq the next append must carry (1 on an empty stream).
    pub fn next_seq(&self) -> u64 {
        self.next_seq
    }

    /// Length of the file in bytes (the offset the next frame lands at).
    pub fn len(&self) -> u64 {
        self.len
    }

    /// True when the stream holds no frames.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Offset of the most recent frame, or `None` on an empty stream.
    pub fn last_offset(&self) -> Option<u64> {
        self.last_offset
    }

    /// True when frames were appended or redacted since the last barrier or full flush.
    pub fn dirty_since_barrier(&self) -> bool {
        self.dirty_since_barrier.load(Ordering::Acquire)
    }

    /// True when a barrier has covered writes that no full flush has covered yet: the host still owes
    /// this stream one `sync_full` for the window.
    pub fn full_owed(&self) -> bool {
        self.full_owed.load(Ordering::Acquire)
    }

    /// Bytes of the file covered by the last successful barrier or full flush (everything that was on
    /// disk at open). Ordering bookkeeping only: a barrier does not make bytes durable on every
    /// platform, so sync does not ship on this; see [`Stream::durable_len`].
    pub fn synced_len(&self) -> u64 {
        self.synced_len.load(Ordering::Acquire)
    }

    /// Bytes of the file a successful full flush has covered (at open: after the recovered file was
    /// fsynced). Sync ships only frames below this, so a peer never holds a frame its origin can lose.
    pub fn durable_len(&self) -> u64 {
        self.durable_len.load(Ordering::Acquire)
    }

    /// True once a `sync_full` has failed on this stream: it refuses writes and flushes until reopened
    /// and `durable_len` is frozen (see the module docs).
    pub fn is_poisoned(&self) -> bool {
        self.poisoned.load(Ordering::Acquire)
    }

    /// Path of the stream file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Append one already-encoded frame whose `header.seq` must equal `next_seq` (else `StreamError::Seq`).
    pub fn append(&mut self, frame: &[u8], header: &Header) -> Result<Appended, StreamError> {
        if self.is_poisoned() {
            return Err(StreamError::Poisoned);
        }
        if header.origin != self.origin {
            return Err(StreamError::Origin {
                expected: self.origin.0.clone(),
                found: header.origin.0.clone(),
            });
        }
        if header.seq != self.next_seq {
            return Err(StreamError::Seq {
                expected: self.next_seq,
                found: header.seq,
            });
        }
        let offset = self.len;
        let ins = inspect(frame).map_err(|source| StreamError::Frame { offset, source })?;
        if !ins.payload_crc_ok {
            return Err(StreamError::Frame {
                offset,
                source: FrameError::PayloadCrc,
            });
        }
        if ins.frame_len != frame.len() {
            return Err(StreamError::Io(io::Error::new(
                io::ErrorKind::InvalidInput,
                "append: buffer holds more than one frame",
            )));
        }
        if ins.header.origin != self.origin {
            return Err(StreamError::Origin {
                expected: self.origin.0.clone(),
                found: ins.header.origin.0.clone(),
            });
        }
        if ins.header.seq != self.next_seq {
            return Err(StreamError::Seq {
                expected: self.next_seq,
                found: ins.header.seq,
            });
        }
        let id = ins.id;
        if let Err(e) = self.file.write_all(frame) {
            // Best effort: do not leave half a frame behind for the next append to follow.
            let _ = self.file.set_len(offset);
            return Err(StreamError::Io(e));
        }
        let seq = self.next_seq;
        if seq % INDEX_EVERY == 1 {
            self.index.push((seq, offset));
        }
        self.last_offset = Some(offset);
        self.len = offset + frame.len() as u64;
        self.next_seq += 1;
        self.dirty_since_barrier.store(true, Ordering::Release);
        Ok(Appended { seq, offset, id })
    }

    /// Encode and append an event (sets nothing; the caller built the header with the right seq).
    pub fn append_event(&mut self, event: &Event) -> Result<Appended, StreamError> {
        let frame = encode(event).map_err(|source| StreamError::Frame {
            offset: self.len,
            source,
        })?;
        self.append(&frame, &event.header)
    }

    /// Iterate frames from a byte offset (0 = all). Each item is `Ok(FrameAt { offset, decoded })` or a hard error.
    ///
    /// `offset` must sit on a frame boundary; the iterator covers the stream as it was at the call.
    pub fn scan_from(&self, offset: u64) -> impl Iterator<Item = Result<FrameAt, StreamError>> {
        ScanIter::new(&self.path, self.origin.clone(), offset, self.len)
    }

    /// Where frame `seq` sits and its id, via the sparse index and a forward walk that never loads
    /// payloads. `None` when `seq` is 0 or past the end.
    pub fn locate(&self, seq: u64) -> Result<Option<Located>, StreamError> {
        if seq == 0 || seq >= self.next_seq {
            return Ok(None);
        }
        Ok(walk(&self.path, &self.index, self.len, seq, seq)?.pop())
    }

    /// The header of a located frame, read without its payload.
    pub fn header_at(&self, loc: &Located) -> Result<Header, StreamError> {
        let mut f = File::open(&self.path)?;
        f.seek(SeekFrom::Start(loc.offset + LEN_PREFIX as u64))?;
        let mut hl = [0u8; 2];
        f.read_exact(&mut hl)?;
        let mut hb = vec![0u8; u16::from_le_bytes(hl) as usize];
        f.read_exact(&mut hb)?;
        serde_json::from_slice(&hb).map_err(|e| StreamError::Frame {
            offset: loc.offset,
            source: FrameError::Header(e.to_string()),
        })
    }

    /// Ids of frames `first..=last` (clipped to the stream), in order, in one forward walk.
    pub fn ids_range(&self, first: u64, last: u64) -> Result<Vec<Id>, StreamError> {
        let last = last.min(self.next_seq.saturating_sub(1));
        Ok(walk(&self.path, &self.index, self.len, first, last)?
            .into_iter()
            .map(|l| l.id)
            .collect())
    }

    /// Raw frame bytes from a seq (exclusive) onward: what sync ships. Chunked by `max_bytes`.
    ///
    /// Only frames below [`Stream::durable_len`] are shipped: a frame no full flush has covered may yet
    /// be lost in a crash, and a peer that held it would see the origin reuse its seq (a fork).
    /// At least one frame is returned when any shippable ones exist after `seq`, even if it alone
    /// exceeds `max_bytes`, so a caller looping on `more` always makes progress.
    pub fn raw_after(&self, seq: u64, max_bytes: usize) -> Result<RawChunk, StreamError> {
        self.chunk(seq, false, max_bytes)
    }

    /// Like [`Stream::raw_after`], but when `seq > 0` the chunk also starts with frame `seq` itself, the
    /// peer's tip, so the receiver compares its id before appending anything (`Ledger::apply` turns a
    /// mismatch into `Fork`). The tip does not count against `max_bytes`, and it is shipped only when it
    /// is itself durable. With nothing new to ship the chunk is the tip alone (`from_seq == to_seq ==
    /// seq`, `more == false`): that is how two replicas that are in step still check each other.
    pub fn raw_after_with_tip(&self, seq: u64, max_bytes: usize) -> Result<RawChunk, StreamError> {
        self.chunk(seq, true, max_bytes)
    }

    fn chunk(&self, seq: u64, tip: bool, max_bytes: usize) -> Result<RawChunk, StreamError> {
        let tip = tip && seq > 0;
        let empty = RawChunk {
            bytes: Vec::new(),
            from_seq: seq,
            to_seq: seq,
            more: false,
        };
        // First seq the chunk may contain.
        let first = if tip { seq } else { seq.saturating_add(1) };
        if first >= self.next_seq {
            return Ok(empty);
        }
        let limit = self.durable_len();
        let pos = self.index.partition_point(|&(s, _)| s <= first);
        let (mut cur_seq, start) = if pos == 0 {
            (1, 0)
        } else {
            self.index[pos - 1]
        };

        let mut reader = BufReader::new(File::open(&self.path)?);
        reader.seek(SeekFrom::Start(start))?;
        let mut offset = start;
        let mut bytes: Vec<u8> = Vec::new();
        let mut tip_bytes = 0usize;
        let mut from_seq = 0u64;
        let mut to_seq = 0u64;
        let mut more = false;

        loop {
            let raw = match read_raw(&mut reader, offset, limit)? {
                Raw::Eof => break,
                Raw::Short | Raw::TooLarge => return Err(StreamError::Corrupt { offset }),
                Raw::Frame(raw) => raw,
            };
            let flen = raw.len() as u64;
            if cur_seq >= first {
                let is_tip = cur_seq <= seq;
                let new_bytes = bytes.len() - tip_bytes;
                if !is_tip && new_bytes > 0 && new_bytes.saturating_add(raw.len()) > max_bytes {
                    more = true;
                    break;
                }
                if bytes.is_empty() {
                    from_seq = cur_seq;
                }
                if is_tip {
                    tip_bytes = raw.len();
                }
                to_seq = cur_seq;
                bytes.extend_from_slice(&raw);
            }
            cur_seq += 1;
            offset += flen;
        }
        if bytes.is_empty() {
            return Ok(empty);
        }
        Ok(RawChunk {
            bytes,
            from_seq,
            to_seq,
            more,
        })
    }

    /// Zero the payload of the frame at `offset` and rewrite its payload crc: the one in-place write.
    ///
    /// The file length never changes, the header and its crc are not written, and the frame's id stays
    /// verifiable. The payload and its crc go out in one contiguous write through a separate non-append
    /// handle (`pwrite` on an `O_APPEND` descriptor appends on Linux). A frame that is already redacted
    /// is left alone. This is the second step of a redaction: `Ledger::redact` appends and barriers the
    /// `redact` frame first.
    pub fn redact_at(&mut self, offset: u64) -> Result<(), StreamError> {
        if self.is_poisoned() {
            return Err(StreamError::Poisoned);
        }
        if offset >= self.len {
            return Err(StreamError::BadOffset(offset));
        }
        let mut reader = File::open(&self.path)?;
        reader.seek(SeekFrom::Start(offset))?;
        let mut bytes = match read_raw(&mut reader, offset, self.len)? {
            Raw::Frame(raw) => raw,
            Raw::Eof => return Err(StreamError::BadOffset(offset)),
            Raw::Short | Raw::TooLarge => return Err(StreamError::Corrupt { offset }),
        };
        drop(reader);
        if redact_in_place(&self.path, offset, &mut bytes)? {
            self.dirty_since_barrier.store(true, Ordering::Release);
        }
        Ok(())
    }

    /// Ordered durability barrier (the host's cadence). On success the stream is no longer dirty
    /// since a barrier, a full flush is owed if there was anything to cover, and everything written so
    /// far counts as synced.
    pub fn sync_barrier(&self) -> io::Result<()> {
        if self.is_poisoned() {
            return Err(poisoned_io());
        }
        let covered = self.len;
        let was_dirty = self.dirty_since_barrier.swap(false, Ordering::AcqRel);
        match fs_sync_barrier(&self.file) {
            Ok(()) => {
                if was_dirty {
                    self.full_owed.store(true, Ordering::Release);
                }
                self.synced_len.fetch_max(covered, Ordering::AcqRel);
                Ok(())
            }
            Err(e) => {
                if was_dirty {
                    self.dirty_since_barrier.store(true, Ordering::Release);
                }
                Err(e)
            }
        }
    }

    /// Full flush to stable storage (the host's cadence): clears both flags on success and is the only
    /// thing that advances [`Stream::durable_len`], the bound sync ships below.
    ///
    /// The redact path writes through its own handle, so the barrier/full here is issued on the append
    /// handle of the same inode, which covers those pages as well on the platforms we target.
    ///
    /// A failure poisons the stream (see the module docs): this and every later write or flush
    /// refuses, and `durable_len` does not move.
    pub fn sync_full(&self) -> io::Result<()> {
        self.sync_full_with(fs_sync_full)
    }

    /// `sync_full` with the flush syscall injected, so tests can make a real stream see a real
    /// `fsync` failure without a failing disk.
    pub(crate) fn sync_full_with(
        &self,
        flush: impl FnOnce(&File) -> io::Result<()>,
    ) -> io::Result<()> {
        if self.is_poisoned() {
            return Err(poisoned_io());
        }
        let covered = self.len;
        let was_dirty = self.dirty_since_barrier.swap(false, Ordering::AcqRel);
        let was_owed = self.full_owed.swap(false, Ordering::AcqRel);
        match flush(&self.file) {
            Ok(()) => {
                self.synced_len.fetch_max(covered, Ordering::AcqRel);
                self.durable_len.fetch_max(covered, Ordering::AcqRel);
                Ok(())
            }
            Err(e) => {
                self.poisoned.store(true, Ordering::Release);
                if was_dirty {
                    self.dirty_since_barrier.store(true, Ordering::Release);
                }
                if was_owed {
                    self.full_owed.store(true, Ordering::Release);
                }
                Err(e)
            }
        }
    }
}

struct ScanIter {
    reader: Option<BufReader<File>>,
    origin: OriginId,
    pos: u64,
    limit: u64,
    prev_seq: Option<u64>,
    pending: Option<StreamError>,
    done: bool,
}

impl ScanIter {
    fn new(path: &Path, origin: OriginId, offset: u64, limit: u64) -> ScanIter {
        let mut it = ScanIter {
            reader: None,
            origin,
            pos: offset,
            limit,
            prev_seq: None,
            pending: None,
            done: false,
        };
        if offset > limit {
            it.pending = Some(StreamError::BadOffset(offset));
            return it;
        }
        if offset == limit {
            it.done = true;
            return it;
        }
        let opened = File::open(path).and_then(|f| {
            let mut r = BufReader::new(f);
            r.seek(SeekFrom::Start(offset))?;
            Ok(r)
        });
        match opened {
            Ok(r) => it.reader = Some(r),
            Err(e) => it.pending = Some(StreamError::Io(e)),
        }
        it
    }

    fn step(&mut self) -> Result<Option<FrameAt>, StreamError> {
        let offset = self.pos;
        let reader = match self.reader.as_mut() {
            Some(r) => r,
            None => return Ok(None),
        };
        let raw = match read_raw(reader, offset, self.limit)? {
            Raw::Eof => return Ok(None),
            Raw::Short | Raw::TooLarge => return Err(StreamError::Corrupt { offset }),
            Raw::Frame(raw) => raw,
        };
        let decoded = decode(&raw).map_err(|source| StreamError::Frame { offset, source })?;
        if decoded.header.origin != self.origin {
            return Err(StreamError::Origin {
                expected: self.origin.0.clone(),
                found: decoded.header.origin.0.clone(),
            });
        }
        if let Some(prev) = self.prev_seq {
            if decoded.header.seq != prev + 1 {
                return Err(StreamError::Gap {
                    offset,
                    expected: prev + 1,
                    found: decoded.header.seq,
                });
            }
        }
        self.prev_seq = Some(decoded.header.seq);
        self.pos = offset + raw.len() as u64;
        Ok(Some(FrameAt { offset, decoded }))
    }
}

impl Iterator for ScanIter {
    type Item = Result<FrameAt, StreamError>;

    fn next(&mut self) -> Option<Self::Item> {
        if let Some(e) = self.pending.take() {
            self.done = true;
            return Some(Err(e));
        }
        if self.done {
            return None;
        }
        match self.step() {
            Ok(Some(f)) => Some(Ok(f)),
            Ok(None) => {
                self.done = true;
                None
            }
            Err(e) => {
                self.done = true;
                Some(Err(e))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::{Hlc, Mode};
    use std::collections::BTreeMap;

    fn origin(name: &str) -> OriginId {
        OriginId(name.to_string())
    }

    fn event(o: &OriginId, seq: u64, payload: &[u8]) -> Event {
        Event {
            header: Header {
                v: 1,
                origin: o.clone(),
                seq,
                hlc: Hlc {
                    ms: 1_000 + seq,
                    count: 0,
                },
                witness: "urn:test:witness".to_string(),
                vocab: "test".to_string(),
                subjects: Vec::new(),
                mode: Mode::Assert,
                payload_hash: blake3::hash(payload).to_hex().to_string(),
                payload_len: payload.len() as u32,
                txn: None,
                sig: None,
                extra: BTreeMap::new(),
            },
            payload: payload.to_vec(),
        }
    }

    fn fill(s: &mut Stream, n: u64) -> Vec<Appended> {
        let o = s.origin().clone();
        (1..=n)
            .map(|i| {
                s.append_event(&event(&o, i, format!("payload-{i}").as_bytes()))
                    .unwrap()
            })
            .collect()
    }

    /// Mutation: drop the `poisoned.store(true)` in `sync_full_with` and the stream stays writable, so
    /// the `Poisoned` assertions go red; let the failure path `fetch_max` durable_len and the frozen
    /// length assertion goes red; clear the flag anywhere but open and the pre-reopen checks go red.
    #[test]
    fn a_failed_full_flush_poisons_the_stream_and_freezes_durable_len_until_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let o = origin("p");
        let (mut s, _) = Stream::open(dir.path(), &o).unwrap();
        fill(&mut s, 2);
        s.sync_full().unwrap();
        let frozen = s.durable_len();
        assert_eq!(frozen, s.len());
        assert!(!s.is_poisoned());
        s.append_event(&event(&o, 3, b"third")).unwrap();
        let len3 = s.len();
        let err = s
            .sync_full_with(|_| Err(io::Error::other("injected fsync failure")))
            .unwrap_err();
        assert!(
            !StreamError::io_is_poisoned(&err),
            "the first failure is the raw one"
        );
        assert!(s.is_poisoned());
        assert_eq!(s.durable_len(), frozen);
        // A later flush that the OS would have accepted is still refused, and moves nothing.
        let again = s.sync_full().unwrap_err();
        assert!(StreamError::io_is_poisoned(&again));
        assert!(StreamError::io_is_poisoned(&s.sync_barrier().unwrap_err()));
        assert_eq!(s.durable_len(), frozen);
        // Nothing past the frozen length ships; what was durable still does.
        let chunk = s.raw_after(0, usize::MAX).unwrap();
        assert_eq!(chunk.to_seq, 2);
        // Writes refuse and write nothing.
        assert!(matches!(
            s.append_event(&event(&o, 4, b"fourth")),
            Err(StreamError::Poisoned)
        ));
        assert!(matches!(s.redact_at(0), Err(StreamError::Poisoned)));
        assert_eq!(s.len(), len3);
        assert_eq!(s.next_seq(), 4);
        // Reads still work.
        assert_eq!(s.scan_from(0).count(), 3);
        drop(s);
        // Reopen rescans and fsyncs: unpoisoned, and the third frame (on disk) counts and ships.
        let (mut s, rec) = Stream::open(dir.path(), &o).unwrap();
        assert_eq!(rec, Recovery::Clean);
        assert!(!s.is_poisoned());
        assert_eq!(s.durable_len(), len3);
        assert_eq!(s.raw_after(0, usize::MAX).unwrap().to_seq, 3);
        s.append_event(&event(&o, 4, b"fourth")).unwrap();
        s.sync_full().unwrap();
        assert_eq!(s.durable_len(), s.len());
    }

    #[test]
    fn empty_open() {
        let dir = tempfile::tempdir().unwrap();
        let (s, rec) = Stream::open(dir.path(), &origin("a")).unwrap();
        assert_eq!(rec, Recovery::Clean);
        assert_eq!(s.next_seq(), 1);
        assert_eq!(s.len(), 0);
        assert_eq!(s.last_offset(), None);
        assert!(!s.dirty_since_barrier() && !s.full_owed());
        assert_eq!(s.scan_from(0).count(), 0);
        assert!(dir.path().join("a.log").exists());
    }

    #[test]
    fn append_then_reopen_counts() {
        let dir = tempfile::tempdir().unwrap();
        let o = origin("a");
        let (mut s, _) = Stream::open(dir.path(), &o).unwrap();
        let ap = fill(&mut s, 3);
        assert!(s.dirty_since_barrier());
        s.sync_full().unwrap();
        assert!(!s.dirty_since_barrier());
        assert_eq!(ap[0].offset, 0);
        assert_eq!(s.next_seq(), 4);
        let len = s.len();
        let last = s.last_offset();
        drop(s);

        let (s2, rec) = Stream::open(dir.path(), &o).unwrap();
        assert_eq!(rec, Recovery::Clean);
        assert_eq!(s2.next_seq(), 4);
        assert_eq!(s2.len(), len);
        assert_eq!(s2.synced_len(), len);
        assert_eq!(s2.last_offset(), last);
        let seqs: Vec<u64> = s2
            .scan_from(0)
            .map(|r| r.unwrap().decoded.header.seq)
            .collect();
        assert_eq!(seqs, vec![1, 2, 3]);
        let from_second: Vec<u64> = s2
            .scan_from(ap[1].offset)
            .map(|r| r.unwrap().decoded.header.seq)
            .collect();
        assert_eq!(from_second, vec![2, 3]);
    }

    #[test]
    fn wrong_seq_and_origin_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let o = origin("a");
        let (mut s, _) = Stream::open(dir.path(), &o).unwrap();
        assert!(matches!(
            s.append_event(&event(&o, 2, b"x")),
            Err(StreamError::Seq {
                expected: 1,
                found: 2
            })
        ));
        assert!(matches!(
            s.append_event(&event(&origin("b"), 1, b"x")),
            Err(StreamError::Origin { .. })
        ));
        assert_eq!(s.len(), 0);
    }

    #[test]
    fn torn_tail_is_cut() {
        let dir = tempfile::tempdir().unwrap();
        let o = origin("a");
        let (mut s, _) = Stream::open(dir.path(), &o).unwrap();
        let ap = fill(&mut s, 3);
        let good = ap[2].offset;
        let full = s.len();
        drop(s);
        let f = OpenOptions::new()
            .write(true)
            .open(dir.path().join("a.log"))
            .unwrap();
        f.set_len(full - 3).unwrap();
        drop(f);

        let (s2, rec) = Stream::open(dir.path(), &o).unwrap();
        assert_eq!(
            rec,
            Recovery::TruncatedTail {
                from: good,
                dropped_bytes: full - 3 - good
            }
        );
        assert_eq!(s2.next_seq(), 3);
        assert_eq!(s2.len(), good);
        assert_eq!(
            std::fs::metadata(dir.path().join("a.log")).unwrap().len(),
            good
        );
    }

    #[test]
    fn zero_filled_tail_is_cut_and_a_flipped_prefix_is_not() {
        let dir = tempfile::tempdir().unwrap();
        let o = origin("a");
        let (mut s, _) = Stream::open(dir.path(), &o).unwrap();
        let ap = fill(&mut s, 3);
        let len = s.len();
        drop(s);
        let path = dir.path().join("a.log");
        let good = std::fs::read(&path).unwrap();

        for zeros in [1usize, 3, 4, 7, 40, 300] {
            let mut b = good.clone();
            b.resize(b.len() + zeros, 0);
            std::fs::write(&path, &b).unwrap();
            let (s2, rec) = Stream::open(dir.path(), &o).unwrap();
            assert_eq!(
                rec,
                Recovery::TruncatedTail {
                    from: len,
                    dropped_bytes: zeros as u64
                },
                "{zeros} zero bytes"
            );
            assert_eq!(s2.next_seq(), 4);
            assert_eq!(std::fs::metadata(&path).unwrap().len(), len);
        }

        // A flipped length prefix on frame 2 (valid frame 3 follows) is corruption: no cut.
        let mut b = good.clone();
        b[ap[1].offset as usize + 1] ^= 0x40;
        std::fs::write(&path, &b).unwrap();
        assert!(matches!(
            Stream::open(dir.path(), &o),
            Err(StreamError::Corrupt { offset }) if offset == ap[1].offset
        ));
        assert_eq!(std::fs::read(&path).unwrap(), b);
    }

    #[test]
    fn bad_payload_crc_is_cut_at_the_tail_and_corrupt_elsewhere() {
        let dir = tempfile::tempdir().unwrap();
        let o = origin("a");
        let (mut s, _) = Stream::open(dir.path(), &o).unwrap();
        let ap = fill(&mut s, 3);
        let len = s.len();
        drop(s);
        let path = dir.path().join("a.log");
        let good = std::fs::read(&path).unwrap();

        let mut b = good.clone();
        b[len as usize - 6] ^= 1; // payload byte of the last frame
        std::fs::write(&path, &b).unwrap();
        let (s2, rec) = Stream::open(dir.path(), &o).unwrap();
        assert_eq!(
            rec,
            Recovery::TruncatedTail {
                from: ap[2].offset,
                dropped_bytes: len - ap[2].offset
            }
        );
        assert_eq!(s2.next_seq(), 3);
        assert_eq!(s2.last_offset(), Some(ap[1].offset));
        drop(s2);

        let mut b = good.clone();
        b[ap[1].offset as usize + 20] ^= 1; // inside frame 2
        std::fs::write(&path, &b).unwrap();
        assert!(matches!(
            Stream::open(dir.path(), &o),
            Err(StreamError::Corrupt { offset }) if offset == ap[1].offset
        ));
    }

    #[test]
    fn resync_looks_past_the_first_read_window() {
        // A damaged 1.5 MB frame followed by a small valid one: the evidence lies beyond the first
        // 1 MiB window of the resync scan, so the scan must keep reading.
        let dir = tempfile::tempdir().unwrap();
        let o = origin("a");
        let (mut s, _) = Stream::open(dir.path(), &o).unwrap();
        let big = vec![b'x'; 1_500_000];
        let first = s.append_event(&event(&o, 1, &big)).unwrap();
        let second = s.append_event(&event(&o, 2, b"after")).unwrap();
        drop(s);
        let path = dir.path().join("a.log");
        let good = std::fs::read(&path).unwrap();
        let mut b = good.clone();
        b[first.offset as usize + 1_000_000] ^= 1;
        std::fs::write(&path, &b).unwrap();
        assert!(matches!(
            Stream::open(dir.path(), &o),
            Err(StreamError::Corrupt { offset: 0 })
        ));
        // The same damage with nothing after it is a tail.
        std::fs::write(&path, &b[..second.offset as usize]).unwrap();
        let (s2, rec) = Stream::open(dir.path(), &o).unwrap();
        assert!(matches!(rec, Recovery::TruncatedTail { from: 0, .. }));
        assert_eq!(s2.next_seq(), 1);
    }

    #[test]
    fn raw_after_chunking_boundaries() {
        let dir = tempfile::tempdir().unwrap();
        let o = origin("a");
        let (mut s, _) = Stream::open(dir.path(), &o).unwrap();
        fill(&mut s, 130);
        s.sync_full().unwrap();

        let all = s.raw_after(0, usize::MAX).unwrap();
        assert_eq!((all.from_seq, all.to_seq, all.more), (1, 130, false));
        assert_eq!(all.bytes.len() as u64, s.len());

        let tail = s.raw_after(100, usize::MAX).unwrap();
        assert_eq!((tail.from_seq, tail.to_seq, tail.more), (101, 130, false));

        for after in [63u64, 64, 65, 128, 129] {
            let c = s.raw_after(after, usize::MAX).unwrap();
            assert_eq!(c.from_seq, after + 1);
            assert_eq!(c.to_seq, 130);
        }

        let end = s.raw_after(130, 10).unwrap();
        assert!(end.bytes.is_empty() && !end.more);
        let beyond = s.raw_after(500, 10).unwrap();
        assert!(beyond.bytes.is_empty() && !beyond.more);

        // A zero budget still makes progress, one frame at a time.
        let one = s.raw_after(0, 0).unwrap();
        assert_eq!((one.from_seq, one.to_seq, one.more), (1, 1, true));

        // Looping on `more` reassembles the whole stream.
        let mut got = Vec::new();
        let mut after = 0u64;
        loop {
            let c = s.raw_after(after, 300).unwrap();
            assert_eq!(c.from_seq, after + 1);
            got.extend_from_slice(&c.bytes);
            after = c.to_seq;
            if !c.more {
                break;
            }
        }
        assert_eq!(after, 130);
        assert_eq!(got, all.bytes);
    }

    #[test]
    fn raw_after_ships_only_what_a_full_flush_covered() {
        let dir = tempfile::tempdir().unwrap();
        let o = origin("a");
        let (mut s, _) = Stream::open(dir.path(), &o).unwrap();
        fill(&mut s, 3);
        assert_eq!(s.durable_len(), 0);
        assert!(s.raw_after(0, usize::MAX).unwrap().bytes.is_empty());
        // A barrier orders the writes (the writer's ack) but does not make them shippable.
        s.sync_barrier().unwrap();
        assert_eq!(s.synced_len(), s.len());
        assert_eq!(s.durable_len(), 0);
        assert!(s.raw_after(0, usize::MAX).unwrap().bytes.is_empty());
        assert!(s
            .raw_after_with_tip(0, usize::MAX)
            .unwrap()
            .bytes
            .is_empty());
        s.sync_full().unwrap();
        let third_end = s.len();
        assert_eq!(s.durable_len(), third_end);
        let fourth = s.append_event(&event(&o, 4, b"unsynced")).unwrap();
        s.sync_barrier().unwrap();
        let chunk = s.raw_after(0, usize::MAX).unwrap();
        assert_eq!((chunk.from_seq, chunk.to_seq, chunk.more), (1, 3, false));
        assert_eq!(chunk.bytes.len() as u64, fourth.offset);
        assert_eq!(s.durable_len(), fourth.offset);
        s.sync_full().unwrap();
        assert_eq!(s.raw_after(0, usize::MAX).unwrap().to_seq, 4);
    }

    #[test]
    fn open_counts_nothing_as_durable_until_the_recovered_file_is_fsynced() {
        let dir = tempfile::tempdir().unwrap();
        let o = origin("a");
        let (mut s, _) = Stream::open(dir.path(), &o).unwrap();
        fill(&mut s, 3);
        drop(s);

        // Recovery done, fsync not yet: nothing ships, and a barrier does not change that.
        let (s2, rec) = scan_file(dir.path(), &o)
            .unwrap()
            .finish_unsynced()
            .unwrap();
        assert_eq!(rec, Recovery::Clean);
        assert_eq!(s2.durable_len(), 0);
        assert!(s2.raw_after(0, usize::MAX).unwrap().bytes.is_empty());
        assert!(s2
            .raw_after_with_tip(2, usize::MAX)
            .unwrap()
            .bytes
            .is_empty());
        s2.sync_barrier().unwrap();
        assert!(s2.raw_after(0, usize::MAX).unwrap().bytes.is_empty());
        s2.sync_full().unwrap();
        assert_eq!(s2.raw_after(0, usize::MAX).unwrap().to_seq, 3);
        drop(s2);

        // The real open does the fsync itself: what was recovered is shippable at once, what is
        // appended after it is not until the next full flush.
        let (mut s3, _) = Stream::open(dir.path(), &o).unwrap();
        assert_eq!(s3.durable_len(), s3.len());
        assert_eq!(s3.raw_after(0, usize::MAX).unwrap().to_seq, 3);
        let fourth = s3.append_event(&event(&o, 4, b"after open")).unwrap();
        s3.sync_barrier().unwrap();
        assert_eq!(s3.raw_after(0, usize::MAX).unwrap().to_seq, 3);
        assert_eq!(s3.durable_len(), fourth.offset);
    }

    #[test]
    fn the_tip_rides_free_and_ships_only_when_durable() {
        let dir = tempfile::tempdir().unwrap();
        let o = origin("a");
        let (mut s, _) = Stream::open(dir.path(), &o).unwrap();
        let ap = fill(&mut s, 10);
        s.sync_full().unwrap();

        // In step: the tip alone, one frame, nothing more.
        let tip = s.raw_after_with_tip(10, usize::MAX).unwrap();
        assert_eq!((tip.from_seq, tip.to_seq, tip.more), (10, 10, false));
        assert_eq!(tip.bytes.len() as u64, s.len() - ap[9].offset);
        // Peer ahead of us: nothing. Peer at zero: the plain stream, no tip.
        assert!(s
            .raw_after_with_tip(11, usize::MAX)
            .unwrap()
            .bytes
            .is_empty());
        let all = s.raw_after_with_tip(0, usize::MAX).unwrap();
        assert_eq!((all.from_seq, all.to_seq), (1, 10));
        assert_eq!(all, s.raw_after(0, usize::MAX).unwrap());
        // Behind: the tip, then the rest.
        let rest = s.raw_after_with_tip(7, usize::MAX).unwrap();
        assert_eq!((rest.from_seq, rest.to_seq, rest.more), (7, 10, false));
        assert_eq!(rest.bytes.len() as u64, s.len() - ap[6].offset);

        // The budget counts new frames only, and at least one new frame always rides with the tip,
        // so a loop that applies each chunk and asks again always advances.
        let zero = s.raw_after_with_tip(7, 0).unwrap();
        assert_eq!((zero.from_seq, zero.to_seq, zero.more), (7, 8, true));
        let two = s
            .raw_after_with_tip(7, (ap[9].offset - ap[7].offset) as usize)
            .unwrap();
        assert_eq!((two.from_seq, two.to_seq, two.more), (7, 9, true));

        // A tip beyond what is durable is not shipped.
        let eleventh = s.append_event(&event(&o, 11, b"unflushed")).unwrap();
        s.sync_barrier().unwrap();
        assert!(s
            .raw_after_with_tip(11, usize::MAX)
            .unwrap()
            .bytes
            .is_empty());
        let still = s.raw_after_with_tip(9, usize::MAX).unwrap();
        assert_eq!((still.from_seq, still.to_seq), (9, 10));
        assert_eq!(still.bytes.len() as u64, eleventh.offset - ap[8].offset);
        s.sync_full().unwrap();
        let now = s.raw_after_with_tip(10, usize::MAX).unwrap();
        assert_eq!((now.from_seq, now.to_seq), (10, 11));
    }

    #[test]
    fn a_second_open_of_the_same_stream_is_locked_and_names_the_path() {
        let dir = tempfile::tempdir().unwrap();
        let o = origin("a");
        let (mut first, _) = Stream::open(dir.path(), &o).unwrap();
        fill(&mut first, 2);
        let path = dir.path().join("a.log");
        match Stream::open(dir.path(), &o) {
            Err(e @ StreamError::Locked { .. }) => {
                let StreamError::Locked { path: named } = &e else {
                    unreachable!()
                };
                assert_eq!(named, &path);
                assert!(e.to_string().contains(&path.display().to_string()), "{e}");
            }
            Err(other) => panic!("expected Locked, got {other}"),
            Ok(_) => panic!("a second writer was admitted"),
        }
        // The refusal touched nothing: the first stream still appends where it was.
        let third = first.append_event(&event(&o, 3, b"still mine")).unwrap();
        assert_eq!(third.seq, 3);
        // Another origin in the same directory is its own stream.
        assert!(Stream::open(dir.path(), &origin("b")).is_ok());
        // Dropping the holder frees the file, and the frames are all there.
        drop(first);
        let (again, rec) = Stream::open(dir.path(), &o).unwrap();
        assert_eq!(rec, Recovery::Clean);
        assert_eq!(again.next_seq(), 4);
    }

    #[test]
    fn a_failed_open_releases_its_lock() {
        let dir = tempfile::tempdir().unwrap();
        let o = origin("a");
        let (mut s, _) = Stream::open(dir.path(), &o).unwrap();
        let ap = fill(&mut s, 3);
        drop(s);
        let path = dir.path().join("a.log");
        let mut b = std::fs::read(&path).unwrap();
        b[ap[1].offset as usize + 20] ^= 1;
        std::fs::write(&path, &b).unwrap();
        assert!(matches!(
            Stream::open(dir.path(), &o),
            Err(StreamError::Corrupt { .. })
        ));
        // Corrupt is reported again, not Locked: the failed open let go.
        assert!(matches!(
            Stream::open(dir.path(), &o),
            Err(StreamError::Corrupt { .. })
        ));
    }

    #[test]
    fn resync_recognises_a_whitespace_padded_header_as_company() {
        use crate::frame::{assemble, canonical_header_bytes};
        let dir = tempfile::tempdir().unwrap();
        let o = origin("a");
        let (mut s, _) = Stream::open(dir.path(), &o).unwrap();
        let ap = fill(&mut s, 2);
        let len = s.len();
        drop(s);
        // Frame 3 as a future writer might store it: a valid header with spaces around the object.
        let ev = event(&o, 3, b"padded");
        let canon = canonical_header_bytes(&ev.header).unwrap();
        let mut padded = b"  ".to_vec();
        padded.extend_from_slice(&canon);
        padded.extend_from_slice(b"\n ");
        let third = assemble(&padded, &ev.payload).unwrap();
        let path = dir.path().join("a.log");
        let mut bytes = std::fs::read(&path).unwrap();
        // Damage frame 2's header, so the first unacceptable frame has the padded one after it.
        bytes[ap[1].offset as usize + 8] ^= 1;
        assert!(ap[1].offset + 8 < len);
        bytes.extend_from_slice(&third);
        std::fs::write(&path, &bytes).unwrap();
        assert!(matches!(
            Stream::open(dir.path(), &o),
            Err(StreamError::Corrupt { offset }) if offset == ap[1].offset
        ));
        // Refusing is not cutting: every byte is still there.
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
    }

    #[test]
    fn flags_follow_the_barrier_then_full_protocol() {
        let dir = tempfile::tempdir().unwrap();
        let (mut s, _) = Stream::open(dir.path(), &origin("a")).unwrap();
        assert!(!s.dirty_since_barrier() && !s.full_owed());
        // A barrier on a stream with nothing written owes nothing.
        s.sync_barrier().unwrap();
        assert!(!s.full_owed());
        fill(&mut s, 2);
        assert!(s.dirty_since_barrier() && !s.full_owed());
        s.sync_barrier().unwrap();
        assert!(!s.dirty_since_barrier() && s.full_owed());
        assert_eq!(s.synced_len(), s.len());
        s.sync_barrier().unwrap();
        assert!(s.full_owed(), "a second barrier keeps the debt");
        s.sync_full().unwrap();
        assert!(!s.dirty_since_barrier() && !s.full_owed());
    }

    #[test]
    fn locate_and_ids_range_find_frames_without_loading_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let o = origin("a");
        let (mut s, _) = Stream::open(dir.path(), &o).unwrap();
        let ap = fill(&mut s, 200);
        for seq in [1u64, 2, 63, 64, 65, 66, 129, 130, 200] {
            let l = s.locate(seq).unwrap().unwrap();
            assert_eq!(l.seq, seq);
            assert_eq!(l.offset, ap[seq as usize - 1].offset);
            assert_eq!(l.id, ap[seq as usize - 1].id);
        }
        assert_eq!(s.locate(0).unwrap(), None);
        assert_eq!(s.locate(201).unwrap(), None);
        let ids = s.ids_range(60, 70).unwrap();
        assert_eq!(ids.len(), 11);
        for (i, id) in ids.iter().enumerate() {
            assert_eq!(*id, ap[59 + i].id);
        }
        assert_eq!(s.ids_range(195, 999).unwrap().len(), 6);
    }

    #[test]
    fn redact_at_keeps_length() {
        let dir = tempfile::tempdir().unwrap();
        let o = origin("a");
        let (mut s, _) = Stream::open(dir.path(), &o).unwrap();
        let ap = fill(&mut s, 3);
        s.sync_barrier().unwrap();
        let len = s.len();
        let before = s.scan_from(ap[1].offset).next().unwrap().unwrap();
        assert!(!before.decoded.is_redacted());

        s.redact_at(ap[1].offset).unwrap();
        assert!(s.dirty_since_barrier());
        assert_eq!(s.len(), len);
        assert_eq!(
            std::fs::metadata(dir.path().join("a.log")).unwrap().len(),
            len
        );

        let after = s.scan_from(ap[1].offset).next().unwrap().unwrap();
        assert!(after.decoded.is_redacted());
        assert_eq!(after.decoded.id, before.decoded.id);
        assert_eq!(after.decoded.frame_len, before.decoded.frame_len);
        assert!(after.decoded.payload.iter().all(|b| *b == 0));

        // A second redaction of the same frame changes nothing and dirties nothing.
        s.sync_barrier().unwrap();
        s.redact_at(ap[1].offset).unwrap();
        assert!(!s.dirty_since_barrier());

        assert!(matches!(s.redact_at(len), Err(StreamError::BadOffset(_))));
        drop(s);
        let (s2, rec) = Stream::open(dir.path(), &o).unwrap();
        assert_eq!(rec, Recovery::Clean);
        assert_eq!(s2.next_seq(), 4);
        // Appending after a redaction still lands at the end.
        let mut s2 = s2;
        let a = s2.append_event(&event(&o, 4, b"after")).unwrap();
        assert_eq!(a.offset, len);
    }
}
