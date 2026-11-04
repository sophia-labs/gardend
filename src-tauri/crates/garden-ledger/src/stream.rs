//! One origin's append-only file: `{ledger_dir}/{origin}.log`.
//!
//! A stream owns a single writer identity. Frames are appended with one `write_all` on a file opened
//! `O_APPEND`; nothing is cached except a sparse `seq -> offset` index (every 64th frame) rebuilt by the
//! open-time scan. The stream never syncs by itself: `sync_barrier` / `sync_full` belong to the host's
//! cadence, and a dirty flag tells the ledger which streams were appended to since the last sync.

use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{self, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use crate::frame::{decode, encode, redact_body, Decoded, Event, FrameError, Header, Id, OriginId};
use crate::sync::{sync_barrier as fs_sync_barrier, sync_dir, sync_full as fs_sync_full};

/// Every `INDEX_EVERY`th frame (seq 1, 65, 129, ...) is remembered in the sparse index.
const INDEX_EVERY: u64 = 64;
/// Bytes of the `u32` length prefix plus the `u32` crc trailer around a frame body.
const FRAME_OVERHEAD: u64 = 8;

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
    /// A damaged frame at `offset` that is followed by more bytes: the stream refuses to open.
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
    /// The frame's id (blake3 of its canonical header).
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
}

/// One decoded frame with the offset it sits at.
#[derive(Clone, Debug)]
pub struct FrameAt {
    /// Byte offset of the frame in the stream file.
    pub offset: u64,
    /// The decoded frame.
    pub decoded: Decoded,
}

/// A run of raw frames, what sync ships.
///
/// An empty chunk (nothing after the requested seq) has empty `bytes`, `from_seq == to_seq == seq`
/// (the seq asked for) and `more == false`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RawChunk {
    /// Concatenated raw frames.
    pub bytes: Vec<u8>,
    /// Seq of the first frame in `bytes`.
    pub from_seq: u64,
    /// Seq of the last frame in `bytes`.
    pub to_seq: u64,
    /// True when frames remain after `to_seq`.
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
    dirty: AtomicBool,
}

enum Raw {
    Eof,
    Short,
    Frame(Vec<u8>),
}

/// Read the raw bytes of the frame starting at `offset`, never reading past `limit`.
fn read_raw<R: Read>(r: &mut R, offset: u64, limit: u64) -> io::Result<Raw> {
    if offset >= limit {
        return Ok(Raw::Eof);
    }
    let avail = limit - offset;
    if avail < 4 {
        return Ok(Raw::Short);
    }
    let mut prefix = [0u8; 4];
    r.read_exact(&mut prefix)?;
    let body_len = u32::from_le_bytes(prefix) as u64;
    let total = body_len + FRAME_OVERHEAD;
    if total > avail {
        return Ok(Raw::Short);
    }
    let mut buf = vec![0u8; total as usize];
    buf[..4].copy_from_slice(&prefix);
    r.read_exact(&mut buf[4..])?;
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

impl Stream {
    /// Open (create if missing), scan every frame, cut a torn tail, leave the file positioned for `O_APPEND`.
    pub fn open(dir: &Path, origin: &OriginId) -> Result<(Stream, Recovery), StreamError> {
        if !valid_origin(origin) {
            return Err(StreamError::Io(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("invalid origin id {:?}", origin.0),
            )));
        }
        std::fs::create_dir_all(dir)?;
        let path = dir.join(format!("{}.log", origin.0));
        let existed = path.exists();
        if !existed {
            OpenOptions::new().create(true).write(true).open(&path)?;
            sync_dir(dir)?;
        }

        let file_len = std::fs::metadata(&path)?.len();
        let mut reader = BufReader::new(File::open(&path)?);
        let mut offset: u64 = 0;
        let mut next_seq: u64 = 1;
        let mut last_offset: Option<u64> = None;
        let mut index: Vec<(u64, u64)> = Vec::new();
        let mut cut: Option<u64> = None;

        loop {
            let raw = match read_raw(&mut reader, offset, file_len)? {
                Raw::Eof => break,
                Raw::Short => {
                    cut = Some(offset);
                    break;
                }
                Raw::Frame(raw) => raw,
            };
            let end = offset + raw.len() as u64;
            let d = match decode(&raw) {
                Ok(d) => d,
                Err(FrameError::Short { .. }) => {
                    cut = Some(offset);
                    break;
                }
                Err(FrameError::Crc { .. }) => {
                    if end == file_len {
                        cut = Some(offset);
                        break;
                    }
                    return Err(StreamError::Corrupt { offset });
                }
                Err(FrameError::Header { .. }) | Err(FrameError::PayloadHash { .. }) => {
                    return Err(StreamError::Corrupt { offset });
                }
                Err(source) => return Err(StreamError::Frame { offset, source }),
            };
            if d.header.origin != *origin {
                return Err(StreamError::Origin {
                    expected: origin.0.clone(),
                    found: d.header.origin.0.clone(),
                });
            }
            if d.header.seq != next_seq {
                return Err(StreamError::Gap {
                    offset,
                    expected: next_seq,
                    found: d.header.seq,
                });
            }
            if next_seq % INDEX_EVERY == 1 {
                index.push((next_seq, offset));
            }
            last_offset = Some(offset);
            next_seq += 1;
            offset = end;
        }
        drop(reader);

        let recovery = match cut {
            None => Recovery::Clean,
            Some(from) => {
                let w = OpenOptions::new().write(true).open(&path)?;
                w.set_len(from)?;
                fs_sync_full(&w)?;
                Recovery::TruncatedTail {
                    from,
                    dropped_bytes: file_len - from,
                }
            }
        };
        let len = cut.unwrap_or(offset);

        let file = OpenOptions::new().append(true).open(&path)?;
        Ok((
            Stream {
                path,
                file,
                origin: origin.clone(),
                next_seq,
                len,
                last_offset,
                index,
                dirty: AtomicBool::new(false),
            },
            recovery,
        ))
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

    /// True when frames were appended or redacted since the last sync.
    pub fn is_dirty(&self) -> bool {
        self.dirty.load(Ordering::Acquire)
    }

    /// Path of the stream file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Append one already-encoded frame whose `header.seq` must equal `next_seq` (else `StreamError::Seq`).
    pub fn append(&mut self, frame: &[u8], header: &Header) -> Result<Appended, StreamError> {
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
        let d = decode(frame).map_err(|source| StreamError::Frame { offset, source })?;
        if d.frame_len != frame.len() {
            return Err(StreamError::Io(io::Error::new(
                io::ErrorKind::InvalidInput,
                "append: buffer holds more than one frame",
            )));
        }
        if d.header.origin != self.origin {
            return Err(StreamError::Origin {
                expected: self.origin.0.clone(),
                found: d.header.origin.0.clone(),
            });
        }
        if d.header.seq != self.next_seq {
            return Err(StreamError::Seq {
                expected: self.next_seq,
                found: d.header.seq,
            });
        }
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
        self.dirty.store(true, Ordering::Release);
        Ok(Appended {
            seq,
            offset,
            id: d.id,
        })
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

    /// Raw frame bytes from a seq (exclusive) onward: what sync ships. Chunked by `max_bytes`.
    ///
    /// At least one frame is returned when any exist after `seq`, even if it alone exceeds `max_bytes`,
    /// so a caller looping on `more` always makes progress.
    pub fn raw_after(&self, seq: u64, max_bytes: usize) -> Result<RawChunk, StreamError> {
        let empty = RawChunk {
            bytes: Vec::new(),
            from_seq: seq,
            to_seq: seq,
            more: false,
        };
        if seq.saturating_add(1) >= self.next_seq {
            return Ok(empty);
        }
        let want = seq + 1;
        let pos = self.index.partition_point(|&(s, _)| s <= want);
        let (mut cur_seq, start) = if pos == 0 {
            (1, 0)
        } else {
            self.index[pos - 1]
        };

        let mut reader = BufReader::new(File::open(&self.path)?);
        reader.seek(SeekFrom::Start(start))?;
        let mut offset = start;
        let mut bytes: Vec<u8> = Vec::new();
        let mut from_seq = 0u64;
        let mut to_seq = 0u64;
        let mut more = false;

        loop {
            let raw = match read_raw(&mut reader, offset, self.len)? {
                Raw::Eof => break,
                Raw::Short => return Err(StreamError::Corrupt { offset }),
                Raw::Frame(raw) => raw,
            };
            let flen = raw.len() as u64;
            if cur_seq > seq {
                if !bytes.is_empty() && bytes.len().saturating_add(raw.len()) > max_bytes {
                    more = true;
                    break;
                }
                if bytes.is_empty() {
                    from_seq = cur_seq;
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

    /// Zero the payload of the frame at `offset` and rewrite its crc: the one in-place write.
    ///
    /// The file length never changes and the frame's id stays verifiable. The write goes through a
    /// separate non-append handle because `pwrite` on an `O_APPEND` descriptor appends on Linux.
    pub fn redact_at(&mut self, offset: u64) -> Result<(), StreamError> {
        if offset >= self.len {
            return Err(StreamError::BadOffset(offset));
        }
        let mut reader = File::open(&self.path)?;
        reader.seek(SeekFrom::Start(offset))?;
        let mut bytes = match read_raw(&mut reader, offset, self.len)? {
            Raw::Frame(raw) => raw,
            Raw::Eof => return Err(StreamError::BadOffset(offset)),
            Raw::Short => return Err(StreamError::Corrupt { offset }),
        };
        drop(reader);
        let original_len = bytes.len();
        let before = decode(&bytes).map_err(|source| StreamError::Frame { offset, source })?;
        let _ = redact_body(&mut bytes);
        let after = decode(&bytes).map_err(|source| StreamError::Frame { offset, source })?;
        if bytes.len() != original_len || after.frame_len != original_len || after.id != before.id {
            return Err(StreamError::Corrupt { offset });
        }
        let w = OpenOptions::new().write(true).open(&self.path)?;
        write_all_at(&w, &bytes, offset)?;
        self.dirty.store(true, Ordering::Release);
        Ok(())
    }

    /// Ordered durability barrier (the host's cadence); clears the dirty flag on success.
    pub fn sync_barrier(&self) -> io::Result<()> {
        self.dirty.store(false, Ordering::Release);
        let r = fs_sync_barrier(&self.file);
        if r.is_err() {
            self.dirty.store(true, Ordering::Release);
        }
        r
    }

    /// Full flush to stable storage (the host's cadence); clears the dirty flag on success.
    ///
    /// The redact path writes through its own handle, so the barrier/full here is issued on the append
    /// handle of the same inode, which covers those pages as well on the platforms we target.
    pub fn sync_full(&self) -> io::Result<()> {
        self.dirty.store(false, Ordering::Release);
        let r = fs_sync_full(&self.file);
        if r.is_err() {
            self.dirty.store(true, Ordering::Release);
        }
        r
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
            Raw::Short => return Err(StreamError::Corrupt { offset }),
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

    #[test]
    fn empty_open() {
        let dir = tempfile::tempdir().unwrap();
        let (s, rec) = Stream::open(dir.path(), &origin("a")).unwrap();
        assert_eq!(rec, Recovery::Clean);
        assert_eq!(s.next_seq(), 1);
        assert_eq!(s.len(), 0);
        assert_eq!(s.last_offset(), None);
        assert!(!s.is_dirty());
        assert_eq!(s.scan_from(0).count(), 0);
        assert!(dir.path().join("a.log").exists());
    }

    #[test]
    fn append_then_reopen_counts() {
        let dir = tempfile::tempdir().unwrap();
        let o = origin("a");
        let (mut s, _) = Stream::open(dir.path(), &o).unwrap();
        let ap = fill(&mut s, 3);
        assert!(s.is_dirty());
        s.sync_full().unwrap();
        assert!(!s.is_dirty());
        assert_eq!(ap[0].offset, 0);
        assert_eq!(s.next_seq(), 4);
        let len = s.len();
        let last = s.last_offset();
        drop(s);

        let (s2, rec) = Stream::open(dir.path(), &o).unwrap();
        assert_eq!(rec, Recovery::Clean);
        assert_eq!(s2.next_seq(), 4);
        assert_eq!(s2.len(), len);
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
    fn raw_after_chunking_boundaries() {
        let dir = tempfile::tempdir().unwrap();
        let o = origin("a");
        let (mut s, _) = Stream::open(dir.path(), &o).unwrap();
        fill(&mut s, 130);

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
    fn redact_at_keeps_length() {
        let dir = tempfile::tempdir().unwrap();
        let o = origin("a");
        let (mut s, _) = Stream::open(dir.path(), &o).unwrap();
        let ap = fill(&mut s, 3);
        s.sync_barrier().unwrap();
        let len = s.len();
        let before = s.scan_from(ap[1].offset).next().unwrap().unwrap();
        assert!(!before.decoded.redacted);

        s.redact_at(ap[1].offset).unwrap();
        assert!(s.is_dirty());
        assert_eq!(s.len(), len);
        assert_eq!(
            std::fs::metadata(dir.path().join("a.log")).unwrap().len(),
            len
        );

        let after = s.scan_from(ap[1].offset).next().unwrap().unwrap();
        assert!(after.decoded.redacted);
        assert_eq!(after.decoded.id, before.decoded.id);
        assert_eq!(after.decoded.frame_len, before.decoded.frame_len);
        assert!(after.decoded.payload.iter().all(|b| *b == 0));

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
