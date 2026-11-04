//! A room over a ledger of streams: one `(graph, vocab)` ledger at `graph_dir/ledger/{vocab}`.
//!
//! [`LedgerRoom`] implements [`Convergent`] with the wire encodings of W6.md section 3
//! (`ledger-vv/1` for SyncStep1, `ledger-bundle/1` for SyncStep2 and Update) and the cadence of
//! section 4: an append or apply marks the room dirty, the host's persist tick issues
//! [`Convergent::persist`] (a barrier), and when a peer waits the host issues [`Convergent::durable`]
//! after [`LEDGER_SHIP_WINDOW`] of coalescing, then fans out [`Convergent::shippable`]. The crate
//! refuses to ship anything not below a stream's `durable_len`, so the room only chooses *when* to
//! flush, never *whether* a frame is safe to send.

#![allow(dead_code)] // consumed by the generic room loop (lane R) and the write seam (W7)

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use garden_ledger::{
    Appended, Draft, HlcClock, Ledger, LedgerError, OriginId, StreamError, VersionVector,
};
use tokio::sync::{broadcast, Mutex};

use super::awareness::AwarenessTable;
use super::convergent::{Applied, ConvergeError, Convergent, RoomKind};
use super::rooms::{ConnectionId, RoomFrame};
use crate::app_error::PersistenceError;

/// How long the host coalesces dirty frames before the eager full flush that lets them ship.
pub(crate) const LEDGER_SHIP_WINDOW: Duration = Duration::from_millis(20);

/// Budget for one bundle when shipping newly durable frames.
const SHIP_CHUNK_BYTES: usize = 8 * 1024 * 1024;

/// Capacity of the room's fan-out channel.
const FRAME_CHANNEL_CAPACITY: usize = 256;

const VV_VERSION: u8 = 1;
const BUNDLE_VERSION: u8 = 1;

/// Cursors for shipping: what has already been offered, and what a full flush has covered.
#[derive(Default)]
struct Shipping {
    /// Per origin, the last seq already queued for fan-out.
    shipped: BTreeMap<OriginId, u64>,
    /// The version at the last successful full flush (frames at or below it are durable).
    durable_vv: VersionVector,
    /// Bundles waiting for [`Convergent::shippable`].
    outbox: Vec<Vec<u8>>,
}

/// A ledger hosted as a room. Share it as `Arc<LedgerRoom>`.
pub(crate) struct LedgerRoom {
    vocab: String,
    ledger: Mutex<Ledger>,
    clock: StdMutex<HlcClock>,
    shipping: StdMutex<Shipping>,
    /// Frames written since the last barrier.
    barrier_owed: AtomicBool,
    /// Frames written since the last full flush.
    ship_owed: AtomicBool,
    frames: broadcast::Sender<RoomFrame>,
    awareness: AwarenessTable,
}

fn lock<T>(m: &StdMutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

fn wall_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn persistence(e: impl std::fmt::Display, context: &str) -> PersistenceError {
    PersistenceError::Transient(format!("{context}: {e}"))
}

fn from_ledger(e: LedgerError) -> ConvergeError {
    match e {
        LedgerError::Fork { origin, seq, .. } => ConvergeError::Fork {
            origin: origin.0,
            seq,
        },
        LedgerError::Io(io) => ConvergeError::Persistence(PersistenceError::io(io, "ledger")),
        LedgerError::Stream(StreamError::Io(io)) => {
            ConvergeError::Persistence(PersistenceError::io(io, "ledger stream"))
        }
        other => ConvergeError::Refused(other.to_string()),
    }
}

/// Encode a version vector as `ledger-vv/1`: `[u8 1][u32 n]{[u16 len][origin utf8][u64 seq]}*`, little-endian.
pub(crate) fn encode_vv(vv: &VersionVector) -> Vec<u8> {
    let mut out = Vec::with_capacity(5 + vv.0.len() * 24);
    out.push(VV_VERSION);
    out.extend_from_slice(&(vv.0.len() as u32).to_le_bytes());
    for (origin, seq) in &vv.0 {
        let name = origin.0.as_bytes();
        out.extend_from_slice(&(name.len() as u16).to_le_bytes());
        out.extend_from_slice(name);
        out.extend_from_slice(&seq.to_le_bytes());
    }
    out
}

/// A cursor over a payload that fails with `Decode` instead of panicking.
struct Reader<'a> {
    buf: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize, what: &str) -> Result<&'a [u8], ConvergeError> {
        let end = self
            .at
            .checked_add(n)
            .filter(|e| *e <= self.buf.len())
            .ok_or_else(|| ConvergeError::Decode(format!("short read at {} ({what})", self.at)))?;
        let s = &self.buf[self.at..end];
        self.at = end;
        Ok(s)
    }
    fn u16(&mut self, what: &str) -> Result<u16, ConvergeError> {
        let b = self.take(2, what)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }
    fn u32(&mut self, what: &str) -> Result<u32, ConvergeError> {
        let b = self.take(4, what)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }
    fn u64(&mut self, what: &str) -> Result<u64, ConvergeError> {
        let b = self.take(8, what)?;
        let mut a = [0u8; 8];
        a.copy_from_slice(b);
        Ok(u64::from_le_bytes(a))
    }
    fn origin(&mut self) -> Result<OriginId, ConvergeError> {
        let len = self.u16("origin length")? as usize;
        let name = std::str::from_utf8(self.take(len, "origin")?)
            .map_err(|_| ConvergeError::Decode("origin is not utf8".into()))?;
        if !garden_ledger::ledger::origin_is_valid(name) {
            return Err(ConvergeError::Decode(format!("invalid origin {name:?}")));
        }
        Ok(OriginId(name.to_string()))
    }
    fn done(&self) -> bool {
        self.at == self.buf.len()
    }
}

/// Decode `ledger-vv/1`. Rejects an unknown version, a short read, a bad origin or trailing bytes.
pub(crate) fn decode_vv(bytes: &[u8]) -> Result<VersionVector, ConvergeError> {
    let mut r = Reader { buf: bytes, at: 0 };
    let v = r.take(1, "version")?[0];
    if v != VV_VERSION {
        return Err(ConvergeError::Decode(format!("ledger-vv version {v}")));
    }
    let n = r.u32("entry count")?;
    let mut map = BTreeMap::new();
    for _ in 0..n {
        let origin = r.origin()?;
        let seq = r.u64("seq")?;
        map.insert(origin, seq);
    }
    if !r.done() {
        return Err(ConvergeError::Decode(
            "trailing bytes after ledger-vv".into(),
        ));
    }
    Ok(VersionVector(map))
}

/// Encode `ledger-bundle/1`: `[u8 1]{[u16 len][origin][u32 nbytes][raw frames]}*`.
pub(crate) fn encode_bundle(entries: &[(&OriginId, &[u8])]) -> Result<Vec<u8>, ConvergeError> {
    let mut out = Vec::with_capacity(
        1 + entries
            .iter()
            .map(|(o, r)| 6 + o.0.len() + r.len())
            .sum::<usize>(),
    );
    out.push(BUNDLE_VERSION);
    for (origin, raw) in entries {
        let name = origin.0.as_bytes();
        let nlen = u16::try_from(name.len())
            .map_err(|_| ConvergeError::Refused("origin name too long".into()))?;
        let blen = u32::try_from(raw.len())
            .map_err(|_| ConvergeError::Refused("bundle entry over 4 GiB".into()))?;
        out.extend_from_slice(&nlen.to_le_bytes());
        out.extend_from_slice(name);
        out.extend_from_slice(&blen.to_le_bytes());
        out.extend_from_slice(raw);
    }
    Ok(out)
}

/// Decode `ledger-bundle/1` into `(origin, raw frames)` entries borrowed from `bytes`.
pub(crate) fn decode_bundle(bytes: &[u8]) -> Result<Vec<(OriginId, &[u8])>, ConvergeError> {
    let mut r = Reader { buf: bytes, at: 0 };
    let v = r.take(1, "version")?[0];
    if v != BUNDLE_VERSION {
        return Err(ConvergeError::Decode(format!("ledger-bundle version {v}")));
    }
    let mut out = Vec::new();
    while !r.done() {
        let origin = r.origin()?;
        let n = r.u32("frame bytes")? as usize;
        let raw = r.take(n, "frames")?;
        out.push((origin, raw));
    }
    Ok(out)
}

impl LedgerRoom {
    /// Open (creating if missing) the ledger at `graph_dir/ledger/{vocab}` with `local` as this
    /// cell's writer id. `vocab` must be a single safe path component. Every frame found on open is
    /// durable (the crate fsyncs recovered files), so it counts as already shipped.
    pub(crate) fn open(
        graph_dir: &Path,
        vocab: &str,
        local: OriginId,
    ) -> Result<Arc<Self>, ConvergeError> {
        let safe = !vocab.is_empty()
            && vocab != "."
            && vocab != ".."
            && !vocab.contains(['/', '\\', '\0']);
        if !safe {
            return Err(ConvergeError::Refused(format!("bad vocab {vocab:?}")));
        }
        let dir = graph_dir.join("ledger").join(vocab);
        let (ledger, _recoveries) = Ledger::open(&dir, local).map_err(from_ledger)?;
        let at_open = ledger.version();
        let shipping = Shipping {
            shipped: at_open.0.clone(),
            durable_vv: at_open,
            outbox: Vec::new(),
        };
        let (frames, _) = broadcast::channel(FRAME_CHANNEL_CAPACITY);
        Ok(Arc::new(LedgerRoom {
            vocab: vocab.to_string(),
            ledger: Mutex::new(ledger),
            clock: StdMutex::new(HlcClock::new()),
            shipping: StdMutex::new(shipping),
            barrier_owed: AtomicBool::new(false),
            ship_owed: AtomicBool::new(false),
            frames,
            awareness: AwarenessTable::new(),
        }))
    }

    /// The vocabulary this ledger holds.
    pub(crate) fn vocab(&self) -> &str {
        &self.vocab
    }

    /// True when frames were written since the last full flush (the host's cue to schedule `durable`).
    pub(crate) fn ship_owed(&self) -> bool {
        self.ship_owed.load(Ordering::Acquire)
    }

    /// True when frames were written since the last barrier (the host's cue to schedule `persist`).
    pub(crate) fn barrier_owed(&self) -> bool {
        self.barrier_owed.load(Ordering::Acquire)
    }

    fn mark_dirty(&self) {
        self.barrier_owed.store(true, Ordering::Release);
        self.ship_owed.store(true, Ordering::Release);
    }

    /// Append a local event (server-side writers: the W7 write seam). The ack the caller gets rides
    /// `persist`; the event ships after the next `durable`.
    pub(crate) async fn append(&self, draft: Draft) -> Result<Appended, ConvergeError> {
        let mut ledger = self.ledger.lock().await;
        let appended = {
            let mut clock = lock(&self.clock);
            ledger
                .append(&mut clock, wall_ms(), draft)
                .map_err(from_ledger)?
        };
        self.mark_dirty();
        Ok(appended)
    }

    /// `{origins: {origin: seq}, durable: {origin: seq}, local: origin}` for the stats route.
    pub(crate) async fn version_json(&self) -> serde_json::Value {
        let ledger = self.ledger.lock().await;
        let origins: serde_json::Map<String, serde_json::Value> = ledger
            .version()
            .0
            .into_iter()
            .map(|(o, s)| (o.0, serde_json::Value::from(s)))
            .collect();
        let durable: serde_json::Map<String, serde_json::Value> = lock(&self.shipping)
            .durable_vv
            .0
            .iter()
            .map(|(o, s)| (o.0.clone(), serde_json::Value::from(*s)))
            .collect();
        serde_json::json!({
            "origins": origins,
            "durable": durable,
            "local": ledger.local().0.clone(),
        })
    }

    /// Queue bundles for every origin's frames past the shipped watermark. Caller holds the ledger
    /// lock and has just completed `sync_full`, so the crate will serve everything up to `upto`.
    fn queue_shippable(&self, ledger: &Ledger, upto: &VersionVector) -> Result<(), ConvergeError> {
        let mut ship = lock(&self.shipping);
        let mut queued = Vec::new();
        let mut marks = Vec::new();
        for (origin, last) in &upto.0 {
            let mut from = ship.shipped.get(origin).copied().unwrap_or(0);
            if from >= *last {
                continue;
            }
            let Some(stream) = ledger.stream(origin) else {
                continue;
            };
            loop {
                let chunk = stream
                    .raw_after_with_tip(from, SHIP_CHUNK_BYTES)
                    .map_err(|e| from_ledger(LedgerError::Stream(e)))?;
                // A tip-only chunk (`to_seq == from`) means nothing new is durable yet.
                if chunk.bytes.is_empty() || chunk.to_seq <= from {
                    break;
                }
                queued.push(encode_bundle(&[(origin, chunk.bytes.as_slice())])?);
                from = chunk.to_seq;
                if !chunk.more {
                    break;
                }
            }
            marks.push((origin.clone(), from));
        }
        ship.outbox.extend(queued);
        for (origin, seq) in marks {
            ship.shipped.insert(origin, seq);
        }
        Ok(())
    }
}

impl Convergent for LedgerRoom {
    fn kind(&self) -> RoomKind {
        RoomKind::Ledger
    }

    async fn version(&self) -> Vec<u8> {
        encode_vv(&self.ledger.lock().await.version())
    }

    async fn diff(&self, since: &[u8], max_bytes: usize) -> Result<Vec<Vec<u8>>, ConvergeError> {
        let mut vv = decode_vv(since)?;
        let ledger = self.ledger.lock().await;
        let mut out = Vec::new();
        // Ledger::diff returns one chunk per origin; an origin with `more` is asked again from
        // where its chunk ended. Origins already complete are not repeated on later passes.
        let mut only: Option<Vec<OriginId>> = None;
        loop {
            let chunks = ledger.diff(&vv, max_bytes).map_err(from_ledger)?;
            let mut again = Vec::new();
            for (origin, chunk) in chunks {
                if let Some(keep) = &only {
                    if !keep.contains(&origin) {
                        continue;
                    }
                }
                out.push(encode_bundle(&[(&origin, chunk.bytes.as_slice())])?);
                if chunk.more {
                    vv.0.insert(origin.clone(), chunk.to_seq);
                    again.push(origin);
                }
            }
            if again.is_empty() {
                break;
            }
            only = Some(again);
        }
        Ok(out)
    }

    fn empty_diff(&self) -> Option<Vec<u8>> {
        encode_bundle(&[]).ok()
    }

    async fn apply(&self, payload: &[u8], _from: ConnectionId) -> Result<Applied, ConvergeError> {
        let entries = decode_bundle(payload)?;
        let mut ledger = self.ledger.lock().await;
        // The server never appends a peer's frames to its own stream. A peer that is in step still sends our
        // origin back as the one-frame tip overlap `Ledger::diff` always carries, so own-origin frames we already
        // hold are an idempotent duplicate (or a loud Fork, if the bytes differ) and go through `Ledger::apply`;
        // one beyond our own seq would be a peer writing as us, and is refused.
        for (origin, raw) in &entries {
            if origin != ledger.local() {
                continue;
            }
            let held = ledger.version().get(origin);
            let mut offset = 0usize;
            while offset < raw.len() {
                let inspected = garden_ledger::frame::inspect(&raw[offset..])
                    .map_err(|e| ConvergeError::Decode(format!("ledger frame: {e:?}")))?;
                if inspected.header.seq > held {
                    return Err(ConvergeError::Refused(format!(
                        "origin {} is this cell's own writer",
                        origin.0
                    )));
                }
                offset += inspected.frame_len;
            }
        }
        let mut new_frames = 0usize;
        for (origin, raw) in entries {
            new_frames += ledger.apply(&origin, raw).map_err(from_ledger)?;
        }
        if new_frames > 0 {
            self.mark_dirty();
        }
        Ok(Applied {
            changed: new_frames > 0,
            reply: None,
        })
    }

    async fn persist(&self) -> Result<(), PersistenceError> {
        if !self.barrier_owed.swap(false, Ordering::AcqRel) {
            return Ok(());
        }
        let ledger = self.ledger.lock().await;
        ledger.sync_barrier().map_err(|e| {
            self.barrier_owed.store(true, Ordering::Release);
            PersistenceError::io(e, "ledger barrier")
        })
    }

    async fn durable(&self) -> Result<(), PersistenceError> {
        if !self.ship_owed.swap(false, Ordering::AcqRel) {
            return Ok(());
        }
        let ledger = self.ledger.lock().await;
        let covered = ledger.version();
        if let Err(e) = ledger.sync_full() {
            self.ship_owed.store(true, Ordering::Release);
            return Err(PersistenceError::io(e, "ledger full flush"));
        }
        // A full flush covers the barrier's bytes too.
        self.barrier_owed.store(false, Ordering::Release);
        if let Err(e) = self.queue_shippable(&ledger, &covered) {
            // The flush itself succeeded; shipping is retried by the next owed flush.
            self.ship_owed.store(true, Ordering::Release);
            return Err(persistence(e, "ledger shippable"));
        }
        lock(&self.shipping).durable_vv = covered;
        Ok(())
    }

    async fn shippable(&self) -> Vec<Vec<u8>> {
        std::mem::take(&mut lock(&self.shipping).outbox)
    }

    fn awareness(&self) -> &AwarenessTable {
        &self.awareness
    }

    fn frames(&self) -> &broadcast::Sender<RoomFrame> {
        &self.frames
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use garden_ledger::{Mode, Subject};
    use std::path::PathBuf;
    use std::sync::atomic::AtomicU64;

    static NEXT: AtomicU64 = AtomicU64::new(0);

    /// A scratch directory removed on drop (no tempfile dependency in the cell crate).
    struct Scratch(PathBuf);
    impl Scratch {
        fn new() -> Self {
            let n = NEXT.fetch_add(1, Ordering::Relaxed);
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let p = std::env::temp_dir()
                .join(format!("ledger-room-{}-{nanos}-{n}", std::process::id()));
            std::fs::create_dir_all(&p).unwrap();
            Scratch(p)
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn o(s: &str) -> OriginId {
        OriginId(s.to_string())
    }

    fn draft(tag: &str) -> Draft {
        Draft {
            witness: "https://example.org/witness".into(),
            vocab: "test".into(),
            subjects: vec![Subject {
                subject: format!("https://example.org/s/{tag}"),
                class: "https://example.org/C".into(),
                parents: vec![],
            }],
            mode: Mode::Assert,
            payload: format!("{{\"tag\":\"{tag}\"}}").into_bytes(),
            txn: None,
        }
    }

    async fn relay(from: &LedgerRoom, to: &LedgerRoom) -> Result<bool, ConvergeError> {
        let since = to.version().await;
        let mut changed = false;
        for bundle in from.diff(&since, 8 * 1024 * 1024).await? {
            changed |= to.apply(&bundle, 1).await?.changed;
        }
        Ok(changed)
    }

    fn vv_of(room_version: &[u8]) -> VersionVector {
        decode_vv(room_version).unwrap()
    }

    #[test]
    fn vv_roundtrip_and_rejections() {
        let mut m = BTreeMap::new();
        m.insert(o("alpha"), 7u64);
        m.insert(o("b.c-9"), u64::MAX);
        let vv = VersionVector(m);
        let bytes = encode_vv(&vv);
        assert_eq!(bytes[0], 1);
        assert_eq!(decode_vv(&bytes).unwrap(), vv);
        assert_eq!(
            decode_vv(&encode_vv(&VersionVector::default())).unwrap(),
            VersionVector::default()
        );
        // Truncated at every length, wrong version, trailing bytes, bad origin.
        for cut in 0..bytes.len() {
            assert!(
                matches!(decode_vv(&bytes[..cut]), Err(ConvergeError::Decode(_))),
                "cut {cut}"
            );
        }
        let mut bad = bytes.clone();
        bad[0] = 2;
        assert!(matches!(decode_vv(&bad), Err(ConvergeError::Decode(_))));
        let mut extra = bytes.clone();
        extra.push(0);
        assert!(matches!(decode_vv(&extra), Err(ConvergeError::Decode(_))));
        let slash = [
            vec![1u8, 1, 0, 0, 0, 3, 0],
            b"a/b".to_vec(),
            1u64.to_le_bytes().to_vec(),
        ]
        .concat();
        assert!(matches!(decode_vv(&slash), Err(ConvergeError::Decode(_))));
    }

    #[test]
    fn bundle_roundtrip_and_rejections() {
        let a = o("a");
        let b = o("bee");
        let raw_a = vec![1u8, 2, 3];
        let raw_b: Vec<u8> = Vec::new();
        let bytes = encode_bundle(&[(&a, raw_a.as_slice()), (&b, raw_b.as_slice())]).unwrap();
        let back = decode_bundle(&bytes).unwrap();
        assert_eq!(back.len(), 2);
        assert_eq!(back[0], (a.clone(), raw_a.as_slice()));
        assert_eq!(back[1], (b.clone(), raw_b.as_slice()));
        assert!(decode_bundle(&[1]).unwrap().is_empty());
        for cut in 1..bytes.len() {
            // Cuts landing exactly between entries are valid shorter bundles; all others must fail.
            let r = decode_bundle(&bytes[..cut]);
            if cut != 1 && cut != 1 + 2 + 1 + 4 + 3 {
                assert!(matches!(r, Err(ConvergeError::Decode(_))), "cut {cut}");
            }
        }
        assert!(matches!(decode_bundle(&[9]), Err(ConvergeError::Decode(_))));
        assert!(matches!(decode_bundle(&[]), Err(ConvergeError::Decode(_))));
    }

    #[tokio::test]
    async fn two_rooms_converge_both_ways() {
        let (da, db) = (Scratch::new(), Scratch::new());
        let a = LedgerRoom::open(&da.0, "test", o("cell-a")).unwrap();
        let b = LedgerRoom::open(&db.0, "test", o("cell-b")).unwrap();
        for t in ["one", "two", "three"] {
            a.append(draft(t)).await.unwrap();
        }
        assert!(a.ship_owed());
        // Nothing ships before a full flush.
        assert!(a
            .diff(&b.version().await, 1 << 20)
            .await
            .unwrap()
            .is_empty());
        a.persist().await.unwrap();
        a.durable().await.unwrap();
        assert!(!a.ship_owed());

        assert!(relay(&a, &b).await.unwrap());
        assert_eq!(vv_of(&b.version().await).get(&o("cell-a")), 3);

        // The reverse direction.
        for t in ["x", "y"] {
            b.append(draft(t)).await.unwrap();
        }
        b.durable().await.unwrap();
        assert!(relay(&b, &a).await.unwrap());
        let va = vv_of(&a.version().await);
        assert_eq!(va.get(&o("cell-b")), 2);
        assert_eq!(va.get(&o("cell-a")), 3);
        // In step: only tip overlaps are exchanged, nothing changes.
        assert!(!relay(&a, &b).await.unwrap());
        assert!(!relay(&b, &a).await.unwrap());
    }

    /// y-sync answers every SyncStep1; a ledger peer takes its first SyncStep2 as "in step", so a diff with nothing to
    /// send is answered with an empty bundle. Mutation: return `None` and a fresh client never leaves "connecting".
    #[test]
    fn a_diff_with_nothing_to_send_is_answered_by_an_empty_bundle() {
        let d = Scratch::new();
        let a = LedgerRoom::open(&d.0, "test", o("cell-a")).unwrap();
        let empty = a.empty_diff().expect("a ledger answers an empty diff");
        assert!(decode_bundle(&empty).unwrap().is_empty());
    }

    #[tokio::test]
    async fn durable_queues_shippable_bundles_once() {
        let (da, db) = (Scratch::new(), Scratch::new());
        let a = LedgerRoom::open(&da.0, "test", o("cell-a")).unwrap();
        let b = LedgerRoom::open(&db.0, "test", o("cell-b")).unwrap();
        assert!(a.shippable().await.is_empty());
        a.append(draft("p")).await.unwrap();
        a.append(draft("q")).await.unwrap();
        assert!(
            a.shippable().await.is_empty(),
            "nothing is shippable before durable"
        );
        a.durable().await.unwrap();
        let out = a.shippable().await;
        assert_eq!(out.len(), 1);
        for bundle in &out {
            assert!(b.apply(bundle, 7).await.unwrap().changed);
        }
        assert_eq!(vv_of(&b.version().await).get(&o("cell-a")), 2);
        assert!(a.shippable().await.is_empty());
        // The next frame ships with a tip overlap and applies cleanly.
        a.append(draft("r")).await.unwrap();
        a.durable().await.unwrap();
        for bundle in a.shippable().await {
            assert!(b.apply(&bundle, 7).await.unwrap().changed);
        }
        assert_eq!(vv_of(&b.version().await).get(&o("cell-a")), 3);
        let j = a.version_json().await;
        assert_eq!(j["origins"]["cell-a"], 3);
        assert_eq!(j["durable"]["cell-a"], 3);
        assert_eq!(j["local"], "cell-a");
    }

    #[tokio::test]
    async fn a_forked_frame_is_refused_and_leaves_the_ledger_unchanged() {
        let (da, db, dc) = (Scratch::new(), Scratch::new(), Scratch::new());
        let a = LedgerRoom::open(&da.0, "test", o("cell-a")).unwrap();
        let b = LedgerRoom::open(&db.0, "test", o("cell-b")).unwrap();
        // An impostor that wrote different events under A's origin.
        let c = LedgerRoom::open(&dc.0, "test", o("cell-a")).unwrap();
        for t in ["one", "two", "three"] {
            a.append(draft(t)).await.unwrap();
        }
        a.durable().await.unwrap();
        relay(&a, &b).await.unwrap();
        for t in ["uno", "dos", "tres", "cuatro"] {
            c.append(draft(t)).await.unwrap();
        }
        c.durable().await.unwrap();

        let before = b.version().await;
        let bundles = c.diff(&before, 1 << 20).await.unwrap();
        assert!(!bundles.is_empty());
        let mut saw_fork = false;
        for bundle in &bundles {
            match b.apply(bundle, 1).await {
                Err(ConvergeError::Fork { origin, seq }) => {
                    assert_eq!(origin, "cell-a");
                    assert_eq!(seq, 3);
                    saw_fork = true;
                }
                other => panic!("expected a fork, got {other:?}"),
            }
        }
        assert!(saw_fork);
        assert_eq!(b.version().await, before);
    }

    #[tokio::test]
    async fn refuses_its_own_origin_and_bad_vocab() {
        let (da, db) = (Scratch::new(), Scratch::new());
        let a = LedgerRoom::open(&da.0, "test", o("cell-a")).unwrap();
        let b = LedgerRoom::open(&db.0, "test", o("cell-a")).unwrap();
        b.append(draft("z")).await.unwrap();
        b.durable().await.unwrap();
        let bundles = b.diff(&a.version().await, 1 << 20).await.unwrap();
        assert!(matches!(
            a.apply(&bundles[0], 1).await,
            Err(ConvergeError::Refused(_))
        ));
        assert!(LedgerRoom::open(&da.0, "../x", o("cell-a")).is_err());
        assert!(LedgerRoom::open(&da.0, "", o("cell-a")).is_err());
    }

    #[tokio::test]
    async fn reopen_counts_existing_frames_as_shipped() {
        let d = Scratch::new();
        {
            let a = LedgerRoom::open(&d.0, "test", o("cell-a")).unwrap();
            a.append(draft("kept")).await.unwrap();
            a.durable().await.unwrap();
        }
        let a = LedgerRoom::open(&d.0, "test", o("cell-a")).unwrap();
        assert_eq!(vv_of(&a.version().await).get(&o("cell-a")), 1);
        a.append(draft("more")).await.unwrap();
        a.durable().await.unwrap();
        assert_eq!(a.shippable().await.len(), 1);
    }
}
