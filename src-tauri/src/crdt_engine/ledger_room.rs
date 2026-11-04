//! A room over a ledger of streams: one `(graph, vocab)` ledger at `graph_dir/ledger/{vocab}`.
//!
//! [`LedgerRoom`] implements [`Convergent`] with the wire encodings of W6.md section 3
//! (`ledger-vv/1` for SyncStep1, `ledger-bundle/1` for SyncStep2 and Update) and the cadence of
//! section 4, which the room itself keeps (no socket needs to be connected for it to hold): an
//! append or a changed apply marks the room dirty and arms two tasks. The persist task runs
//! [`Convergent::persist`] (a barrier) after [`ROOM_PERSIST_WINDOW`]. The ship task runs
//! [`ship_pending`] (barrier, full flush, fan-out of [`Convergent::shippable`]) after
//! [`LEDGER_SHIP_WINDOW`] when a peer is subscribed, and after [`LEDGER_IDLE_FLUSH_WINDOW`] when none
//! is. The crate refuses to ship anything not below a stream's `durable_len`, so the room only
//! chooses *when* to flush, never *whether* a frame is safe to send.
//!
//! The barrier and the full flush run on a blocking thread (`spawn_blocking`) holding the ledger's
//! owned mutex guard, so a 20 ms fsync never occupies an executor worker; appends still wait for it,
//! exactly as before.
//!
//! An evicted room (graph deleted, restored or duplicated over, or replaced in the registry) is
//! fenced: `apply`, `append` and `diff` refuse once [`LedgerRoom::evict`] has run, while the final
//! `persist` + `durable` that eviction spawns still go through.

#![allow(dead_code)] // the write seam (W7) is the remaining consumer of `append` / `version_json`

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex as StdMutex, Weak};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use garden_ledger::{
    Appended, Draft, HlcClock, Ledger, LedgerError, OriginId, StreamError, VersionVector,
};
use tokio::sync::{broadcast, Mutex};
use tracing::instrument::WithSubscriber;

use super::awareness::AwarenessTable;
use super::convergent::{Applied, ConvergeError, Convergent, RoomKind};
use super::rooms::{ship_pending, ConnectionId, FrameKind, RoomFrame, ROOM_PERSIST_WINDOW};
use crate::app_error::PersistenceError;

/// How long the room coalesces dirty frames before the eager full flush that lets them ship, when
/// at least one peer is subscribed.
pub(crate) const LEDGER_SHIP_WINDOW: Duration = Duration::from_millis(20);

/// How long dirty frames wait for their full flush when nobody is subscribed (the 1 s durability
/// window of `plans/2026-10-22-durability/DESIGN.md`).
pub(crate) const LEDGER_IDLE_FLUSH_WINDOW: Duration = Duration::from_secs(1);

/// The message a fenced room refuses with.
const EVICTED_REASON: &str = "ledger room was evicted";

/// Budget for one bundle when shipping newly durable frames.
const SHIP_CHUNK_BYTES: usize = 8 * 1024 * 1024;

/// Capacity of the room's fan-out channel.
const FRAME_CHANNEL_CAPACITY: usize = 256;

const VV_VERSION: u8 = 1;
const BUNDLE_VERSION: u8 = 1;

/// `ship_armed` states: no ship task pending, one pending at the idle window, one pending at the
/// eager window. A higher state is armed over a lower one (see [`LedgerRoom::arm_ship`]).
const SHIP_NONE: u8 = 0;
const SHIP_IDLE: u8 = 1;
const SHIP_EAGER: u8 = 2;

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
    /// In an `Arc` so a flush can take an owned guard onto a blocking thread.
    ledger: Arc<Mutex<Ledger>>,
    clock: StdMutex<HlcClock>,
    shipping: Arc<StdMutex<Shipping>>,
    /// Frames written since the last barrier.
    barrier_owed: AtomicBool,
    /// Frames written since the last full flush.
    ship_owed: AtomicBool,
    /// Set (once, first) by [`LedgerRoom::evict`]; fences `apply`, `append` and `diff`.
    evicted: AtomicBool,
    /// A persist task is waiting out its window.
    persist_armed: AtomicBool,
    /// A ship task is waiting out its window ([`SHIP_NONE`], [`SHIP_IDLE`] or [`SHIP_EAGER`]).
    ship_armed: AtomicU8,
    /// Back-reference for the tasks the room arms from `&self`. They hold it weakly: a room nobody
    /// else holds is gone, and its files are released.
    me: Weak<LedgerRoom>,
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
        LedgerError::Stream(StreamError::Locked { path }) => {
            ConvergeError::Locked(path.display().to_string())
        }
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
        return Err(ConvergeError::UnsupportedVersion {
            what: "ledger-vv",
            version: v,
        });
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
        return Err(ConvergeError::UnsupportedVersion {
            what: "ledger-bundle",
            version: v,
        });
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

/// The cell's background-work lease, held by a room's timer tasks so idle drain and shutdown wait
/// for them. Tests have no process lifecycle, so they hold none.
fn background_lease(name: &'static str) -> Option<crate::cell_lifecycle::BackgroundLease> {
    #[cfg(test)]
    {
        let _ = name;
        None
    }
    #[cfg(not(test))]
    {
        crate::cell_lifecycle::process_lifecycle()
            .and_then(|lifecycle| lifecycle.begin_background(name).ok())
    }
}

/// A version vector as a JSON object `{origin: seq}`.
fn vv_json(vv: &BTreeMap<OriginId, u64>) -> serde_json::Map<String, serde_json::Value> {
    vv.iter()
        .map(|(o, s)| (o.0.clone(), serde_json::Value::from(*s)))
        .collect()
}

/// What a blocking full flush ended in.
enum Flushed {
    /// Flushed, and everything below the flushed version is queued for shipping.
    Done,
    /// The full flush itself failed; nothing may ship.
    Full(std::io::Error),
    /// The flush succeeded but queuing the shippable bundles failed; retry on the next flush.
    Ship(ConvergeError),
}

/// Queue bundles for every origin's frames past the shipped watermark. The caller holds the ledger
/// and has just completed `sync_full`, so the crate will serve everything up to `upto`.
fn queue_shippable(
    ledger: &Ledger,
    shipping: &StdMutex<Shipping>,
    upto: &VersionVector,
) -> Result<(), ConvergeError> {
    let mut ship = lock(shipping);
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
        Ok(Arc::new_cyclic(|me| LedgerRoom {
            vocab: vocab.to_string(),
            ledger: Arc::new(Mutex::new(ledger)),
            clock: StdMutex::new(HlcClock::new()),
            shipping: Arc::new(StdMutex::new(shipping)),
            barrier_owed: AtomicBool::new(false),
            ship_owed: AtomicBool::new(false),
            evicted: AtomicBool::new(false),
            persist_armed: AtomicBool::new(false),
            ship_armed: AtomicU8::new(SHIP_NONE),
            me: me.clone(),
            frames,
            awareness: AwarenessTable::new(),
        }))
    }

    /// The version map of the ledger in `dir` when no room hosts it, without creating anything:
    /// no directory, no `origin` file, no stream file, no registry room. A directory with no
    /// `origin` file has no ledger yet (`local` is `null`); one with an `origin` and no streams
    /// reports that origin and nothing else; one with streams is opened the way a room opens it
    /// (so a torn tail is recovered, exactly as the next room open would) and dropped. The caller
    /// serializes this against room opens (the registry's opening slot), because the open takes the
    /// streams' locks.
    pub(crate) fn cold_version_json(dir: &Path) -> Result<serde_json::Value, ConvergeError> {
        let empty = |local: Option<String>| serde_json::json!({"origins": {}, "durable": {}, "local": local});
        if !dir.join(garden_ledger::ORIGIN_FILE).is_file() {
            return Ok(empty(None));
        }
        let has_streams = std::fs::read_dir(dir)
            .map(|entries| {
                entries.filter_map(Result::ok).any(|entry| {
                    entry
                        .file_name()
                        .to_str()
                        .is_some_and(|n| n.ends_with(".log"))
                })
            })
            .unwrap_or(false);
        if !has_streams {
            let local = garden_ledger::read_or_mint_origin(dir).map_err(from_ledger)?;
            return Ok(empty(Some(local.0)));
        }
        let (ledger, _recoveries) = Ledger::open_dir(dir).map_err(from_ledger)?;
        let origins = vv_json(&ledger.version().0);
        Ok(serde_json::json!({
            // Everything found on open is durable: the open fsynced it.
            "origins": origins.clone(),
            "durable": origins,
            "local": ledger.local().0.clone(),
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

    /// Whether [`Self::evict`] has run.
    pub(crate) fn is_evicted(&self) -> bool {
        self.evicted.load(Ordering::SeqCst)
    }

    fn check_live(&self) -> Result<(), ConvergeError> {
        if self.is_evicted() {
            return Err(ConvergeError::Refused(EVICTED_REASON.into()));
        }
        Ok(())
    }

    /// Fence the room and let go of it. The flag is set first (so nothing new is accepted), then
    /// every socket is told to go, then a spawned task makes what the room holds durable and keeps
    /// the room alive until it has finished, so the ledger's stream locks are released only after
    /// the last flush. Idempotent: only the first call does anything.
    pub(crate) fn evict(self: &Arc<Self>) {
        if self.evicted.swap(true, Ordering::SeqCst) {
            return;
        }
        let _ = self.frames.send(RoomFrame {
            origin: None,
            kind: FrameKind::Evicted,
            payload: Vec::new(),
        });
        let room = self.clone();
        crate::app_runtime::async_runtime::spawn(
            async move {
                if let Err(error) = room.persist().await {
                    log::warn!("ledger barrier at eviction failed: {error}");
                }
                if let Err(error) = room.durable().await {
                    log::warn!("ledger full flush at eviction failed: {error}");
                }
            }
            .with_current_subscriber(),
        );
    }

    fn mark_dirty(&self) {
        self.barrier_owed.store(true, Ordering::Release);
        self.ship_owed.store(true, Ordering::Release);
    }

    /// Arm the two background tasks a change owes (W6.md section 4). `peers_waiting`: at least one
    /// connection is subscribed, so shipping is eager ([`LEDGER_SHIP_WINDOW`]); with none it rides
    /// the idle cadence ([`LEDGER_IDLE_FLUSH_WINDOW`]).
    fn arm(&self, peers_waiting: bool) {
        self.arm_persist();
        self.arm_ship(peers_waiting);
    }

    /// At most one persist task waits at a time. It clears the flag *before* it flushes, so a change
    /// that lands after arms the next task and one that landed before is in this barrier.
    fn arm_persist(&self) {
        if self.is_evicted() || self.persist_armed.swap(true, Ordering::SeqCst) {
            return;
        }
        let lease = background_lease("ledger-persist");
        let me = self.me.clone();
        crate::app_runtime::async_runtime::spawn(
            async move {
                let _lease = lease;
                tokio::time::sleep(ROOM_PERSIST_WINDOW).await;
                let Some(room) = me.upgrade() else {
                    return;
                };
                room.persist_armed.store(false, Ordering::SeqCst);
                if let Err(error) = room.persist().await {
                    log::warn!("ledger barrier failed (retried with the next flush): {error}");
                }
            }
            .with_current_subscriber(),
        );
    }

    /// Arm a ship task: [`LEDGER_SHIP_WINDOW`] when a peer waits, [`LEDGER_IDLE_FLUSH_WINDOW`] when
    /// none does. A task already armed at the same or an earlier window is enough (it clears the
    /// state before it ships, so a change that lands after arms its own); a waiting peer arrives
    /// over an idle task and arms an eager one beside it.
    fn arm_ship(&self, peers_waiting: bool) {
        if self.is_evicted() {
            return;
        }
        let want = if peers_waiting { SHIP_EAGER } else { SHIP_IDLE };
        if self.ship_armed.fetch_max(want, Ordering::SeqCst) >= want {
            return;
        }
        let window = if peers_waiting {
            LEDGER_SHIP_WINDOW
        } else {
            LEDGER_IDLE_FLUSH_WINDOW
        };
        let lease = background_lease("ledger-ship");
        let me = self.me.clone();
        crate::app_runtime::async_runtime::spawn(
            async move {
                let _lease = lease;
                tokio::time::sleep(window).await;
                let Some(room) = me.upgrade() else {
                    return;
                };
                room.ship_armed.store(SHIP_NONE, Ordering::SeqCst);
                ship_pending(&*room).await;
            }
            .with_current_subscriber(),
        );
    }

    /// Append a local event (server-side writers: the W7 write seam). The ack the caller gets rides
    /// `persist` (armed here); the event ships after the room's next `durable`, which this arms too,
    /// so a connected peer receives it without sending a frame.
    pub(crate) async fn append(&self, draft: Draft) -> Result<Appended, ConvergeError> {
        self.check_live()?;
        let appended = {
            let mut ledger = self.ledger.lock().await;
            self.check_live()?;
            let mut clock = lock(&self.clock);
            ledger
                .append(&mut clock, wall_ms(), draft)
                .map_err(from_ledger)?
        };
        self.mark_dirty();
        self.arm(self.frames.receiver_count() > 0);
        Ok(appended)
    }

    /// `{origins: {origin: seq}, durable: {origin: seq}, local: origin}` for the stats route.
    pub(crate) async fn version_json(&self) -> serde_json::Value {
        let ledger = self.ledger.lock().await;
        let origins = vv_json(&ledger.version().0);
        let durable = vv_json(&lock(&self.shipping).durable_vv.0);
        serde_json::json!({
            "origins": origins,
            "durable": durable,
            "local": ledger.local().0.clone(),
        })
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
        self.check_live()?;
        let ledger = self.ledger.lock().await;
        self.check_live()?;
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
        self.check_live()?;
        let mut ledger = self.ledger.lock().await;
        self.check_live()?;
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
        drop(ledger);
        if new_frames > 0 {
            self.mark_dirty();
            // Section 4: eager as soon as any connection is subscribed (the writer's own socket
            // counts), idle cadence otherwise.
            self.arm(self.frames.receiver_count() > 0);
        }
        Ok(Applied {
            changed: new_frames > 0,
            reply: None,
        })
    }

    async fn persist(&self) -> Result<(), PersistenceError> {
        if !self.barrier_owed.load(Ordering::Acquire) {
            return Ok(());
        }
        let ledger = self.ledger.clone().lock_owned().await;
        if !self.barrier_owed.swap(false, Ordering::AcqRel) {
            return Ok(());
        }
        // The fsync runs on a blocking thread holding the owned guard: appends wait for it as they
        // always did, but no executor worker does.
        match tokio::task::spawn_blocking(move || ledger.sync_barrier()).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => {
                self.barrier_owed.store(true, Ordering::Release);
                Err(PersistenceError::io(e, "ledger barrier"))
            }
            Err(e) => {
                self.barrier_owed.store(true, Ordering::Release);
                Err(persistence(e, "ledger barrier task"))
            }
        }
    }

    async fn durable(&self) -> Result<(), PersistenceError> {
        if !self.ship_owed.load(Ordering::Acquire) {
            return Ok(());
        }
        let ledger = self.ledger.clone().lock_owned().await;
        if !self.ship_owed.swap(false, Ordering::AcqRel) {
            return Ok(());
        }
        // Holding the ledger, nothing is appended while this flushes, and the full flush covers the
        // barrier's bytes too: clear the barrier debt now so an append after it can set it again.
        self.barrier_owed.store(false, Ordering::Release);
        let shipping = self.shipping.clone();
        let flushed = tokio::task::spawn_blocking(move || {
            let covered = ledger.version();
            if let Err(e) = ledger.sync_full() {
                return Flushed::Full(e);
            }
            if let Err(e) = queue_shippable(&ledger, &shipping, &covered) {
                return Flushed::Ship(e);
            }
            lock(&shipping).durable_vv = covered;
            Flushed::Done
        })
        .await;
        match flushed {
            Ok(Flushed::Done) => Ok(()),
            Ok(Flushed::Full(e)) => {
                self.barrier_owed.store(true, Ordering::Release);
                self.ship_owed.store(true, Ordering::Release);
                Err(PersistenceError::io(e, "ledger full flush"))
            }
            Ok(Flushed::Ship(e)) => {
                // The flush itself succeeded; shipping is retried by the next owed flush.
                self.ship_owed.store(true, Ordering::Release);
                Err(persistence(e, "ledger shippable"))
            }
            Err(e) => {
                self.barrier_owed.store(true, Ordering::Release);
                self.ship_owed.store(true, Ordering::Release);
                Err(persistence(e, "ledger full flush task"))
            }
        }
    }

    async fn shippable(&self) -> Vec<Vec<u8>> {
        std::mem::take(&mut lock(&self.shipping).outbox)
    }

    fn is_evicted(&self) -> bool {
        LedgerRoom::is_evicted(self)
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
        // An unknown version is named, not folded into "undecodable": the loop closes on it.
        let mut bad = bytes.clone();
        bad[0] = 2;
        assert_eq!(
            decode_vv(&bad),
            Err(ConvergeError::UnsupportedVersion {
                what: "ledger-vv",
                version: 2
            })
        );
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
        assert_eq!(
            decode_bundle(&[9]).err(),
            Some(ConvergeError::UnsupportedVersion {
                what: "ledger-bundle",
                version: 9
            })
        );
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

    async fn frame_within(
        rx: &mut broadcast::Receiver<RoomFrame>,
        wait: Duration,
    ) -> Option<RoomFrame> {
        tokio::time::timeout(wait, async {
            loop {
                match rx.recv().await {
                    Ok(frame) if frame.kind == FrameKind::State => return frame,
                    Ok(_) => continue,
                    Err(_) => std::future::pending::<()>().await,
                }
            }
        })
        .await
        .ok()
    }

    /// B1: after `evict` the room refuses apply, append and diff, says `Evicted` once however many
    /// times it is evicted, and its final flush still goes through.
    #[tokio::test]
    async fn an_evicted_room_is_fenced_but_still_flushes() {
        let (da, db) = (Scratch::new(), Scratch::new());
        let a = LedgerRoom::open(&da.0, "test", o("cell-a")).unwrap();
        let b = LedgerRoom::open(&db.0, "test", o("cell-b")).unwrap();
        a.append(draft("before")).await.unwrap();
        b.append(draft("theirs")).await.unwrap();
        b.durable().await.unwrap();
        let from_b = b.diff(&a.version().await, 1 << 20).await.unwrap();
        let mut rx = a.frames().subscribe();
        assert!(!a.is_evicted());

        a.evict();
        a.evict();
        assert!(a.is_evicted());
        assert!(Convergent::is_evicted(&*a));
        let first = rx.recv().await.unwrap();
        assert_eq!(first.kind, FrameKind::Evicted);
        assert!(
            tokio::time::timeout(Duration::from_millis(100), async {
                loop {
                    if let Ok(frame) = rx.recv().await {
                        if frame.kind == FrameKind::Evicted {
                            return;
                        }
                    }
                }
            })
            .await
            .is_err(),
            "a second evict announced the eviction again"
        );

        let refused = |r: Result<_, ConvergeError>| match r {
            Err(ConvergeError::Refused(m)) => m == EVICTED_REASON,
            _ => false,
        };
        assert!(refused(a.append(draft("after")).await.map(|_| ())));
        assert!(refused(a.apply(&from_b[0], 1).await.map(|_| ())));
        assert!(refused(
            a.diff(&b.version().await, 1 << 20).await.map(|_| ())
        ));
        assert_eq!(vv_of(&a.version().await).get(&o("cell-a")), 1);
        assert_eq!(vv_of(&a.version().await).get(&o("cell-b")), 0);

        // The flush eviction owes is not fenced.
        a.persist().await.unwrap();
        a.durable().await.unwrap();
        assert!(!a.ship_owed());
        let j = a.version_json().await;
        assert_eq!(j["durable"]["cell-a"], 1);
    }

    /// M1: an append on a room with one subscribed peer reaches it as a `State` frame within 200 ms,
    /// with no client frame and no explicit flush call.
    #[tokio::test]
    async fn an_append_ships_to_a_subscribed_peer_without_any_client_frame() {
        let (da, db) = (Scratch::new(), Scratch::new());
        let a = LedgerRoom::open(&da.0, "test", o("cell-a")).unwrap();
        let b = LedgerRoom::open(&db.0, "test", o("cell-b")).unwrap();
        let mut rx = a.frames().subscribe();
        a.append(draft("pushed")).await.unwrap();
        let frame = frame_within(&mut rx, Duration::from_millis(200))
            .await
            .expect("the append shipped within 200 ms");
        // [0][2][varuint len][bundle]: an Update a peer applies.
        assert_eq!(&frame.payload[..2], &[0, 2]);
        let mut pos = 2usize;
        let len = crate::crdt_engine::rooms::read_var_u64(&frame.payload, &mut pos).unwrap();
        let bundle = &frame.payload[pos..pos + len as usize];
        assert!(b.apply(bundle, 9).await.unwrap().changed);
        assert_eq!(vv_of(&b.version().await).get(&o("cell-a")), 1);
        assert!(!a.ship_owed());
    }

    /// M1: a changed apply ships to every subscriber (the writer's own socket and a second one) within
    /// 200 ms, on the room's timer, with nothing else happening.
    #[tokio::test]
    async fn an_apply_ships_to_the_other_subscriber() {
        let (da, db) = (Scratch::new(), Scratch::new());
        let a = LedgerRoom::open(&da.0, "test", o("cell-a")).unwrap();
        let b = LedgerRoom::open(&db.0, "test", o("cell-b")).unwrap();
        b.append(draft("from b")).await.unwrap();
        b.durable().await.unwrap();
        let (mut writer, mut peer) = (a.frames().subscribe(), a.frames().subscribe());
        for bundle in b.diff(&a.version().await, 1 << 20).await.unwrap() {
            assert!(a.apply(&bundle, 3).await.unwrap().changed);
        }
        assert!(frame_within(&mut peer, Duration::from_millis(200))
            .await
            .is_some());
        assert!(frame_within(&mut writer, Duration::from_millis(50))
            .await
            .is_some());
        assert!(!a.ship_owed());
    }

    /// Section 4 literally: one subscribed connection (here the writer's own) is enough for the eager window.
    #[tokio::test]
    async fn an_apply_with_one_subscriber_ships_on_the_eager_window() {
        let (da, db) = (Scratch::new(), Scratch::new());
        let a = LedgerRoom::open(&da.0, "test", o("cell-a")).unwrap();
        let b = LedgerRoom::open(&db.0, "test", o("cell-b")).unwrap();
        b.append(draft("from b")).await.unwrap();
        b.durable().await.unwrap();
        let mut only = a.frames().subscribe();
        for bundle in b.diff(&a.version().await, 1 << 20).await.unwrap() {
            assert!(a.apply(&bundle, 3).await.unwrap().changed);
        }
        assert!(frame_within(&mut only, Duration::from_millis(200))
            .await
            .is_some());
        assert!(!a.ship_owed());
    }

    /// M1: with nobody subscribed the full flush rides the 1 s idle cadence, and the barrier its own
    /// 250 ms window: both debts clear on their own.
    #[tokio::test]
    async fn with_no_peer_the_room_flushes_itself_on_the_idle_cadence() {
        let d = Scratch::new();
        let a = LedgerRoom::open(&d.0, "test", o("cell-a")).unwrap();
        a.append(draft("quiet")).await.unwrap();
        assert!(a.ship_owed() && a.barrier_owed());
        tokio::time::sleep(ROOM_PERSIST_WINDOW + Duration::from_millis(250)).await;
        assert!(!a.barrier_owed(), "the barrier did not run on its window");
        assert!(a.ship_owed(), "the idle full flush came early");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while a.ship_owed() && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert!(!a.ship_owed(), "ship_owed did not clear within 2 s");
        assert_eq!(a.version_json().await["durable"]["cell-a"], 1);
    }

    /// An idle-armed ship task does not hold a waiting peer back for a second: a subscriber that
    /// arrives and a second change arms the eager window beside it.
    #[tokio::test]
    async fn a_peer_arriving_over_an_idle_task_gets_the_eager_window() {
        let d = Scratch::new();
        let a = LedgerRoom::open(&d.0, "test", o("cell-a")).unwrap();
        a.append(draft("first")).await.unwrap();
        let mut rx = a.frames().subscribe();
        a.append(draft("second")).await.unwrap();
        let frame = frame_within(&mut rx, Duration::from_millis(300)).await;
        assert!(
            frame.is_some(),
            "the second change waited out the idle window"
        );
    }

    /// The cold version read: it creates nothing.
    #[test]
    fn a_cold_version_read_creates_nothing() {
        let d = Scratch::new();
        let dir = d.0.join("ledger").join("test");
        let none = LedgerRoom::cold_version_json(&dir).unwrap();
        assert_eq!(
            none,
            serde_json::json!({"origins": {}, "durable": {}, "local": null})
        );
        assert!(
            !d.0.join("ledger").exists(),
            "a cold read made the ledger directory"
        );
        // A directory with no origin file is still no ledger.
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(LedgerRoom::cold_version_json(&dir).unwrap(), none);
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
    }

    fn registry_dir(scratch: &Scratch, graph: &str, vocab: &str) -> (String, PathBuf) {
        (
            format!("ledger:{graph}:{vocab}"),
            scratch.0.join(graph).join("ledger").join(vocab),
        )
    }

    /// B1: the registry hands out one room per key; evicting it fences the old `Arc`, and a reopen
    /// (which waits out the old room's last flush and its last holder) is a new room that has every
    /// frame the old one made durable.
    #[tokio::test]
    async fn the_registry_evicts_a_ledger_room_and_reopens_it_behind_the_old_arc() {
        use crate::crdt_engine::rooms::RoomRegistry;
        let d = Scratch::new();
        let (key, dir) = registry_dir(&d, "g", "v");
        let registry = RoomRegistry::default();
        let first = registry
            .get_or_create_ledger(&key, dir.clone())
            .await
            .unwrap();
        let again = registry
            .get_or_create_ledger(&key, dir.clone())
            .await
            .unwrap();
        assert!(
            Arc::ptr_eq(&first, &again),
            "two opens of one key made two rooms"
        );
        assert!(Arc::ptr_eq(&first, &registry.peek_ledger(&key).unwrap()));
        drop(again);
        let local = first.version_json().await["local"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(local.starts_with("cell-"));

        for tag in ["a", "b", "c"] {
            first.append(draft(tag)).await.unwrap();
        }
        let mut rx = first.frames().subscribe();
        assert!(registry.evict_room(&key));
        assert!(first.is_evicted());
        assert_eq!(rx.recv().await.unwrap().kind, FrameKind::Evicted);
        assert!(registry.peek_ledger(&key).is_none());
        assert!(matches!(
            first.append(draft("late")).await,
            Err(ConvergeError::Refused(_))
        ));
        assert!(matches!(
            first.apply(&[1], 1).await,
            Err(ConvergeError::Refused(_))
        ));

        // The old Arc is still held: the streams are flocked, so a reopen cannot succeed yet, and
        // says why, naming the path, when its bounded wait is over.
        let started = tokio::time::Instant::now();
        let refused = registry
            .get_or_create_ledger_within(&key, dir.clone(), Duration::from_millis(200))
            .await
            .err()
            .expect("a reopen behind a live Arc is refused");
        assert!(started.elapsed() >= Duration::from_millis(150), "{refused}");
        assert!(
            refused.contains("locked") && refused.contains(&dir.display().to_string()),
            "{refused}"
        );

        // Let go of it: the reopen then waits only for the eviction's final flush.
        drop(rx);
        drop(first);
        let second = registry
            .get_or_create_ledger(&key, dir.clone())
            .await
            .unwrap();
        assert!(!second.is_evicted());
        assert_eq!(second.version_json().await["local"], local.as_str());
        assert_eq!(second.version_json().await["origins"][&local], 3);
        // What the old room made durable is there for a peer to take.
        assert_eq!(second.version_json().await["durable"][&local], 3);
        second.append(draft("d")).await.unwrap();
        assert_eq!(second.version_json().await["origins"][&local], 4);
    }

    /// M4: the version of a ledger nobody hosts is read without hosting it or changing the disk.
    #[tokio::test]
    async fn the_registry_reads_a_cold_ledger_version_without_a_room() {
        use crate::crdt_engine::rooms::RoomRegistry;
        let d = Scratch::new();
        let (key, dir) = registry_dir(&d, "g", "v");
        let registry = RoomRegistry::default();
        assert_eq!(
            registry.ledger_version(&key, &dir).await.unwrap(),
            serde_json::json!({"origins": {}, "durable": {}, "local": null})
        );
        assert!(!d.0.join("g").exists());

        let local = {
            let room = registry
                .get_or_create_ledger(&key, dir.clone())
                .await
                .unwrap();
            room.append(draft("x")).await.unwrap();
            room.append(draft("y")).await.unwrap();
            room.persist().await.unwrap();
            room.durable().await.unwrap();
            let local = room.version_json().await["local"]
                .as_str()
                .unwrap()
                .to_string();
            assert!(registry.evict_room(&key));
            local
        };
        // Wait for the eviction's flush to let go of the streams, then read cold.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        loop {
            let cold = registry.ledger_version(&key, &dir).await;
            match cold {
                Ok(cold) => {
                    assert_eq!(cold["origins"][&local], 2);
                    assert_eq!(cold["durable"][&local], 2);
                    assert_eq!(cold["local"], local.as_str());
                    break;
                }
                Err(error) if tokio::time::Instant::now() < deadline && error.contains("lock") => {
                    tokio::time::sleep(Duration::from_millis(25)).await
                }
                Err(error) => panic!("cold version read failed: {error}"),
            }
        }
        assert!(
            registry.peek_ledger(&key).is_none(),
            "a cold read hosted a room"
        );
        // And the room opens normally afterwards (the cold read let go of its locks).
        let room = registry
            .get_or_create_ledger(&key, dir.clone())
            .await
            .unwrap();
        assert_eq!(room.version_json().await["origins"][&local], 2);
    }
}
