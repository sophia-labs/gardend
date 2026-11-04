//! y-websocket room hosting for the cell (platform-next Phase 2).
//!
//! One in-process Room per document/workspace Y.Doc: clients sync via the
//! y-protocol (SYNC step1/step2/update + AWARENESS), the room owns the Doc,
//! and applied updates persist to the profile's `update-v1.bin` (the CRDT
//! authority file — same hot-tier discipline as the desktop frontend's
//! `persistFast`). Downstream projections/RDF flush land with the op
//! executor; the authority state is durable here.
//!
//! yrs::sync::Awareness is !Send, so rooms hold the Doc directly and track
//! awareness as merged AwarenessUpdate entries (plain Send data) — the
//! server is a relay + late-joiner answerer, not an awareness participant.

use super::awareness::AwarenessTable;
use super::convergent::{Applied, ConvergeError, Convergent, RoomKind};
use super::ledger_room::LedgerRoom;
use crate::app_error::PersistenceError;
use crate::app_runtime::AppHandle;
use crate::app_runtime::Manager;
use crate::storage_atomic::{
    atomic_temp_path, sync_parent_dir, write_bytes_atomic_durable, Durability,
};
use axum::extract::ws::{Message as WsMessage, WebSocket};
use futures_util::{SinkExt, StreamExt};
use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::sync::{OnceLock, Weak};
use std::time::{Duration, Instant};
use tokio::sync::{broadcast, Mutex, Notify};
use tracing::{instrument::WithSubscriber, Instrument};
use yrs::sync::{AwarenessUpdate, Message, SyncMessage};
use yrs::updates::decoder::Decode;
use yrs::updates::encoder::Encode;
use yrs::{Doc, ReadTxn, Transact, Update};

pub struct Room {
    doc: Mutex<Doc>,
    /// Ephemeral presence (see [`AwarenessTable`]).
    awareness: AwarenessTable,
    /// This room's own `Arc`, so the [`Convergent`] adapter (which only has `&self`) can arm the
    /// group commit and schedule the cold projection, both of which need an `Arc<Room>`.
    me: Weak<Room>,
    /// Raw payloads for in-process observers (tests). Sockets use `frames`.
    tx: broadcast::Sender<Vec<u8>>,
    /// What the websocket loops deliver: the same payloads, tagged with the
    /// connection that produced them and whether they carry graph state.
    frames: broadcast::Sender<RoomFrame>,
    next_connection_id: AtomicU64,
    /// Sockets currently serving this room (see [`ConnectionGuard`]).
    connections: AtomicUsize,
    /// Where to persist the authoritative Y.Doc state (update-v1.bin).
    state_path: PathBuf,
    /// When the sidecar's directory entry was last fsynced (see
    /// [`DirSyncCadence`]).
    dir_sync: Arc<DirSyncCadence>,
    /// Bumped, with the doc mutex held, by every change to the live Y.Doc.
    /// `persisted_state_epoch` is the newest epoch whose bytes are in the
    /// sidecar; the difference is the room's unpersisted tail (group commit).
    state_epoch: AtomicU64,
    persisted_state_epoch: AtomicU64,
    /// A group-commit task is waiting out its window.
    persist_armed: AtomicBool,
    /// Serializes sidecar writes (group commit, API write, flushes) so an
    /// older encode can never be renamed over a newer one.
    persist_gate: Mutex<()>,
    /// Set by the websocket host. Only a room with a scope defers its
    /// persistence; every other caller persists before it returns.
    persist_scope: OnceLock<PersistScope>,
    /// When the oldest unpersisted change happened (telemetry only).
    dirty_since: StdMutex<Option<Instant>>,
    /// Monotonic version of the live Y.Doc state that still needs to reach the
    /// cold document/workspace projections. The Y.Doc remains authoritative;
    /// these epochs only prevent redundant materialization work.
    projection_epoch: AtomicU64,
    persisted_projection_epoch: AtomicU64,
    /// Serialize cold materialization for this room. The headless executor can
    /// have graph- and document-scoped flush operations in flight at once;
    /// each must recheck the dirty epoch after acquiring this gate so the same
    /// snapshot cannot be saved twice as two document revisions.
    projection_flush_lock: Mutex<()>,
    // A projection may run without the graph hot-write lease. Capture only
    // committed room state, never an update whose sidecar persist can fail.
    authority_lock: Mutex<()>,
    /// Graph deletion is a room-lifetime boundary, independent of whether the
    /// cold projection target has been configured yet. Existing websocket
    /// tasks and scheduler waits select on this signal and exit immediately.
    evicted: AtomicBool,
    eviction_notify: Notify,
    /// Cell-owned server debounce. Shrubbery and hosted browsers are sync
    /// clients; only the opt-in legacy frontend authority omits this target.
    projection_flush: OnceLock<Arc<ProjectionFlushTarget>>,
}

/// A websocket's id within a room: awareness ownership and echo suppression key on it.
pub(crate) type ConnectionId = u64;

/// What a [`RoomFrame`] carries.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FrameKind {
    /// A room update (Y.Doc update or ledger bundle): graph content, so delivery is linearized
    /// with deletion.
    State,
    /// Ephemeral presence: no graph content, so delivery takes no lease.
    Awareness,
    /// The room was evicted from the registry (carries no payload). A [`serve_convergent`] loop
    /// that receives it closes its socket; it is how a ledger room, which has no eviction flag of
    /// its own, tells its sockets to go.
    Evicted,
}

/// One message on a room's fan-out channel.
#[derive(Clone)]
pub(crate) struct RoomFrame {
    /// The socket whose frame this is, when that socket already has it. Its
    /// own loop skips it: y-websocket clients do not need their own echo.
    pub(crate) origin: Option<ConnectionId>,
    /// What the payload is.
    pub(crate) kind: FrameKind,
    /// The encoded y-sync message to deliver.
    pub(crate) payload: Vec<u8>,
}

/// Where a room's deferred persistence runs: the graph whose lifecycle lease it
/// takes around a write (so deletion stays ordered against it) and the app that
/// owns the coordinator.
#[derive(Clone)]
struct PersistScope {
    app: AppHandle,
    graph_id: String,
}

/// Longest an acknowledged websocket edit waits before the room state reaches
/// the sidecar. The edit is broadcast at once; within this window the sidecar
/// lags the live room by at most one group commit (Vera's accepted loss on a
/// power cut is about one second, never corruption).
pub(super) const ROOM_PERSIST_WINDOW: Duration = Duration::from_millis(250);

/// The room sidecar is replaced by rename, so after the first write its name
/// already exists: the file's own fsync makes the new content whole, and a
/// power cut leaves the old or the new complete file, never a torn one. The
/// directory fsync would only matter for a brand-new name, yet it cost as much
/// as the file's (stress round 2: 94% of all fsyncs). It now runs for a new
/// name, at most once per `ROOM_DIR_SYNC_INTERVAL` otherwise, and when a
/// connection closes or the process flushes for shutdown, so a rename that was
/// skipped is made durable within about a second. The loss it allows is the
/// accepted window: a power cut may resurrect the previous complete sidecar.
const ROOM_DIR_SYNC_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Default)]
struct DirSyncCadence {
    last: StdMutex<Option<Instant>>,
    /// A rename since the last directory sync has not been made durable.
    pending: AtomicBool,
}

impl DirSyncCadence {
    /// Whether the write about to happen must fsync the directory.
    fn due_at(&self, now: Instant, target_existed: bool, force: bool) -> bool {
        let mut last = self.last.lock().unwrap_or_else(|error| error.into_inner());
        let due = force
            || !target_existed
            || last.map_or(true, |at| {
                now.saturating_duration_since(at) >= ROOM_DIR_SYNC_INTERVAL
            });
        if due {
            *last = Some(now);
            self.pending.store(false, Ordering::SeqCst);
        } else {
            self.pending.store(true, Ordering::SeqCst);
        }
        due
    }

    /// The directory sync was done elsewhere (a close-time settle).
    fn settled_at(&self, now: Instant) {
        *self.last.lock().unwrap_or_else(|error| error.into_inner()) = Some(now);
        self.pending.store(false, Ordering::SeqCst);
    }
}

struct ProjectionFlushTarget {
    graph_id: String,
    document_id: Option<String>,
    debounce: Duration,
    generation: AtomicU64,
    /// The newest lifecycle generation a scheduling call saw; the timer task
    /// flushes against it.
    graph_generation: AtomicU64,
    contexts: StdMutex<FlushContexts>,
    timer: StdMutex<DebounceTimer>,
}

/// One timer task per room serves a whole typing session instead of one
/// spawned task per frame. A frame only moves `last` (and sets `first` when it
/// starts a burst); the task sleeps until `last + debounce`, but never past
/// `first + PROJECTION_DEBOUNCE_MAX_WAIT`, so a person who types without a
/// pause still gets their document projected.
#[derive(Default)]
struct DebounceTimer {
    /// A timer task owns the pending burst.
    armed: bool,
    first: Option<Instant>,
    last: Option<Instant>,
}

/// How long until the pending burst is due: the debounce after the latest
/// update, capped at `max_wait` after the first. Zero means flush now.
fn debounce_remaining(
    first: Instant,
    last: Instant,
    now: Instant,
    debounce: Duration,
    max_wait: Duration,
) -> Duration {
    let due = (last + debounce).min(first + max_wait);
    due.saturating_duration_since(now)
}

#[derive(Default)]
struct FlushContexts {
    updates: Vec<crate::telemetry::TraceContext>,
    dropped: u64,
}

impl FlushContexts {
    fn push(&mut self, context: crate::telemetry::TraceContext) {
        if context.span_context().is_none() {
            return;
        }
        // Match the SDK's link limit; a stalled debounce cannot grow without
        // bound. The span reports overflow explicitly.
        if self.updates.len() < 128 {
            self.updates.push(context);
        } else {
            self.dropped += 1;
        }
    }
}

const WORKSPACE_PROJECTION_DEBOUNCE: Duration = Duration::from_millis(250);
const DOCUMENT_PROJECTION_DEBOUNCE: Duration = Duration::from_millis(600);
/// Continuous typing postpones the debounce for as long as keys keep coming;
/// the projection (cold record, RDF, search) still lands at least this often.
const PROJECTION_DEBOUNCE_MAX_WAIT: Duration = Duration::from_secs(5);
const PROJECTION_RETRY_INITIAL: Duration = Duration::from_millis(100);
const PROJECTION_RETRY_MAX: Duration = Duration::from_secs(5);
const PROJECTION_FLUSH_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(5);

enum ProjectionFlushFailure {
    Terminal(String),
    Retryable(String),
}

fn classify_projection_flush_failure(error: String) -> ProjectionFlushFailure {
    if error.contains("graph not found:")
        || error.contains("stale graph generation")
        || error.contains("stale graph incarnation")
        || error.contains("room was evicted")
        || error.contains("revision conflict")
        || error.contains("precondition")
    {
        ProjectionFlushFailure::Terminal(error)
    } else {
        ProjectionFlushFailure::Retryable(error)
    }
}

#[derive(Default)]
pub struct RoomRegistry {
    // Registry operations never await while the map is locked. A synchronous
    // mutex lets graph deletion evict/cancel rooms before returning from its
    // otherwise-synchronous service path.
    rooms: StdMutex<HashMap<String, RoomEntry>>,
    // A disk-only restore may start only without live graph rooms. Every
    // opener takes this admission mutex before the room map, closing the
    // check-to-open race without evicting existing runtime authority.
    disk_restores: StdMutex<std::collections::HashSet<String>>,
    // One mutex per room key, held across a cold open's disk read and decode
    // so concurrent openers of that key do the work once and openers of other
    // keys never wait on it. Taken before any other registry lock.
    opening: StdMutex<HashMap<String, Arc<StdMutex<()>>>>,
}

pub(crate) struct DiskRestoreRoomGuard<'a> {
    registry: &'a RoomRegistry,
    graph_id: String,
}

impl Drop for DiskRestoreRoomGuard<'_> {
    fn drop(&mut self) {
        if let Ok(mut restores) = self.registry.disk_restores.lock() {
            restores.remove(&self.graph_id);
        }
        // Poison stays fail-closed for future admission/open attempts.
    }
}

/// Whether a registry key names a room of `graph_id`, of either kind: the workspace Y.Doc
/// (`workspace:{g}`), a document Y.Doc (`doc:{g}:{d}`) or a ledger (`ledger:{g}:{vocab}`).
fn room_key_belongs_to_graph(key: &str, graph_id: &str) -> bool {
    key == format!("workspace:{graph_id}")
        || key.starts_with(&format!("doc:{graph_id}:"))
        || key.starts_with(&format!("ledger:{graph_id}:"))
}

/// How long a ledger open waits for stream locks an evicted room still holds.
const LEDGER_LOCKED_WAIT: Duration = Duration::from_secs(2);
/// The pause between those attempts.
const LEDGER_LOCKED_RETRY: Duration = Duration::from_millis(25);

/// Why a ledger room could not be opened.
enum LedgerOpenError {
    /// A stream file is locked by another handle (the path). Retried for a bounded time.
    Locked(String),
    /// Anything else, as a message.
    Other(String),
}

impl From<String> for LedgerOpenError {
    fn from(message: String) -> Self {
        LedgerOpenError::Other(message)
    }
}

/// Why [`RoomRegistry::open_ledger`] refused.
#[derive(Debug)]
pub(crate) enum LedgerAdmissionError {
    /// The graph does not exist or is deleted (HTTP 404).
    NotFound(String),
    /// The lease, the open or the ledger failed (HTTP 500).
    Other(String),
}

#[allow(dead_code)]
/// One hosted room, tagged by what it holds. The registry's operations that are about a key
/// (admission, eviction, graph scans, flushes) cover both kinds; the ones that return a `Room`
/// (`get_or_create`, `peek`, ...) are Y.Doc-only and say so.
#[derive(Clone)]
pub(crate) enum RoomEntry {
    /// A Y.Doc room: a document, the workspace or a board.
    YDoc(Arc<Room>),
    /// A ledger room: one `(graph, vocab)` ledger of streams.
    Ledger(Arc<LedgerRoom>),
}

#[allow(dead_code)]
impl RoomEntry {
    /// What the entry hosts.
    pub(crate) fn kind(&self) -> RoomKind {
        match self {
            RoomEntry::YDoc(_) => RoomKind::YDoc,
            RoomEntry::Ledger(_) => RoomKind::Ledger,
        }
    }

    /// The Y.Doc room, when this is one.
    pub(crate) fn ydoc(&self) -> Option<&Arc<Room>> {
        match self {
            RoomEntry::YDoc(room) => Some(room),
            RoomEntry::Ledger(_) => None,
        }
    }

    /// The ledger room, when this is one.
    pub(crate) fn ledger(&self) -> Option<&Arc<LedgerRoom>> {
        match self {
            RoomEntry::Ledger(room) => Some(room),
            RoomEntry::YDoc(_) => None,
        }
    }

    /// Both kinds carry an eviction flag, set by [`Self::evict`].
    fn is_evicted(&self) -> bool {
        match self {
            RoomEntry::YDoc(room) => room.is_evicted(),
            RoomEntry::Ledger(room) => room.is_evicted(),
        }
    }

    /// Whether `self` and `other` hold the very same room (pointer identity, same kind).
    fn same_room(&self, other: &RoomEntry) -> bool {
        match (self, other) {
            (RoomEntry::YDoc(a), RoomEntry::YDoc(b)) => Arc::ptr_eq(a, b),
            (RoomEntry::Ledger(a), RoomEntry::Ledger(b)) => Arc::ptr_eq(a, b),
            _ => false,
        }
    }

    /// Invalidate the room. A Y.Doc room sets its flag and cancels its projection generation. A
    /// ledger room sets its flag first (so `apply`, `append` and `diff` refuse from then on), tells
    /// every socket to go (a [`FrameKind::Evicted`] frame), then makes what it holds durable on a
    /// spawned task that keeps the room alive until it has finished, so the ledger's stream locks
    /// are released only after the last flush. Idempotent for both kinds; the registry calls it
    /// once per removed entry.
    fn evict(&self) {
        match self {
            RoomEntry::YDoc(room) => room.evict(),
            RoomEntry::Ledger(room) => room.evict(),
        }
    }

    /// Whether a graph-wide flush has work here: a Y.Doc room only when its sidecar lags, a
    /// ledger room always (its barrier is a no-op when nothing was written).
    fn needs_flush(&self) -> bool {
        match self {
            RoomEntry::YDoc(room) => room.state_is_dirty(),
            RoomEntry::Ledger(_) => true,
        }
    }
}

/// Run a future to completion from synchronous code that may itself sit on a runtime worker: the
/// future runs on a fresh OS thread, which is never a runtime thread, so `block_on` is legal there.
fn block_on_fresh_thread<F>(future: F) -> F::Output
where
    F: Future + Send,
    F::Output: Send,
{
    std::thread::scope(|scope| {
        scope
            .spawn(|| crate::app_runtime::async_runtime::block_on(future))
            .join()
            .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
    })
}

#[cfg(test)]
struct TombstoneOpenRaceHook {
    state_path: PathBuf,
    reached: std::sync::mpsc::Sender<()>,
    release: std::sync::mpsc::Receiver<()>,
}

#[cfg(test)]
static TOMBSTONE_OPEN_RACE_HOOK: std::sync::OnceLock<StdMutex<Option<TombstoneOpenRaceHook>>> =
    std::sync::OnceLock::new();

#[cfg(test)]
fn install_tombstone_open_race_hook(
    state_path: PathBuf,
    reached: std::sync::mpsc::Sender<()>,
    release: std::sync::mpsc::Receiver<()>,
) {
    *TOMBSTONE_OPEN_RACE_HOOK
        .get_or_init(|| StdMutex::new(None))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(TombstoneOpenRaceHook {
        state_path,
        reached,
        release,
    });
}

#[cfg(test)]
fn clear_tombstone_open_race_hook() {
    *TOMBSTONE_OPEN_RACE_HOOK
        .get_or_init(|| StdMutex::new(None))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
}

#[cfg(test)]
fn pause_after_tombstone_precheck(state_path: &std::path::Path) {
    let hook = TOMBSTONE_OPEN_RACE_HOOK
        .get_or_init(|| StdMutex::new(None))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let Some(hook) = hook.as_ref().filter(|hook| hook.state_path == state_path) else {
        return;
    };
    let _ = hook.reached.send(());
    let _ = hook.release.recv();
}

#[cfg(not(test))]
fn pause_after_tombstone_precheck(_state_path: &std::path::Path) {}

// A second, independent barrier used only by the identity-checked-eviction
// regression: it fires AFTER a fresh room is inserted but BEFORE its final
// tombstone recheck, so a test can evict that room and install a NEW live room
// at the same key in the window the opener holds no lock.
#[cfg(test)]
struct TombstoneRecheckRaceHook {
    state_path: PathBuf,
    reached: std::sync::mpsc::Sender<()>,
    release: std::sync::mpsc::Receiver<()>,
}

#[cfg(test)]
static TOMBSTONE_RECHECK_RACE_HOOK: std::sync::OnceLock<
    StdMutex<Option<TombstoneRecheckRaceHook>>,
> = std::sync::OnceLock::new();

#[cfg(test)]
fn install_tombstone_recheck_race_hook(
    state_path: PathBuf,
    reached: std::sync::mpsc::Sender<()>,
    release: std::sync::mpsc::Receiver<()>,
) {
    *TOMBSTONE_RECHECK_RACE_HOOK
        .get_or_init(|| StdMutex::new(None))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(TombstoneRecheckRaceHook {
        state_path,
        reached,
        release,
    });
}

#[cfg(test)]
fn clear_tombstone_recheck_race_hook() {
    *TOMBSTONE_RECHECK_RACE_HOOK
        .get_or_init(|| StdMutex::new(None))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
}

#[cfg(test)]
fn pause_before_fresh_recheck(state_path: &std::path::Path) {
    // TAKE the hook (single-fire) and RELEASE the registry-hook mutex BEFORE
    // blocking on `recv`: this barrier's whole purpose is to let the test, on
    // another thread, re-enter `get_or_create` (installing the intervening
    // room) while the opener is parked here — holding the mutex across the
    // block would deadlock that re-entry.
    let hook = {
        let mut guard = TOMBSTONE_RECHECK_RACE_HOOK
            .get_or_init(|| StdMutex::new(None))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let matches = guard
            .as_ref()
            .is_some_and(|hook| hook.state_path == state_path);
        if matches {
            guard.take()
        } else {
            None
        }
    };
    if let Some(hook) = hook {
        let _ = hook.reached.send(());
        let _ = hook.release.recv();
    }
}

#[cfg(not(test))]
fn pause_before_fresh_recheck(_state_path: &std::path::Path) {}

// A third barrier, inside a cold open's disk read and decode (phase 2): it
// parks the opener at the point that used to hold the process-wide registry
// mutex, so a test can prove other rooms are not waiting behind it.
#[cfg(test)]
struct OpenDecodeHook {
    state_path: PathBuf,
    reached: std::sync::mpsc::Sender<()>,
    release: std::sync::mpsc::Receiver<()>,
}

#[cfg(test)]
static OPEN_DECODE_HOOK: std::sync::OnceLock<StdMutex<Option<OpenDecodeHook>>> =
    std::sync::OnceLock::new();

#[cfg(test)]
fn install_open_decode_hook(
    state_path: PathBuf,
    reached: std::sync::mpsc::Sender<()>,
    release: std::sync::mpsc::Receiver<()>,
) {
    *OPEN_DECODE_HOOK
        .get_or_init(|| StdMutex::new(None))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(OpenDecodeHook {
        state_path,
        reached,
        release,
    });
}

#[cfg(test)]
fn pause_during_open_decode(state_path: &std::path::Path) {
    // Single-fire, and the hook mutex is released before blocking.
    let hook = {
        let mut guard = OPEN_DECODE_HOOK
            .get_or_init(|| StdMutex::new(None))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if guard
            .as_ref()
            .is_some_and(|hook| hook.state_path == state_path)
        {
            guard.take()
        } else {
            None
        }
    };
    if let Some(hook) = hook {
        let _ = hook.reached.send(());
        let _ = hook.release.recv();
    }
}

#[cfg(not(test))]
fn pause_during_open_decode(_state_path: &std::path::Path) {}

enum TombstoneOpenPolicy<'a> {
    Reject,
    FreshRecreation(&'a str),
}

impl RoomRegistry {
    pub(crate) fn begin_disk_restore(
        &self,
        graph_id: &str,
    ) -> Result<DiskRestoreRoomGuard<'_>, String> {
        let mut restores = self
            .disk_restores
            .lock()
            .map_err(|_| "restore room admission lock poisoned")?;
        let rooms = self
            .rooms
            .lock()
            .map_err(|_| "room registry lock poisoned")?;
        if restores.contains(graph_id)
            || rooms
                .keys()
                .any(|key| room_key_belongs_to_graph(key, graph_id))
        {
            return Err(format!("graph.restoreArchive: target graph {graph_id} has live room authority or an active disk restore"));
        }
        restores.insert(graph_id.to_string());
        Ok(DiskRestoreRoomGuard {
            registry: self,
            graph_id: graph_id.to_string(),
        })
    }

    /// Non-healing, fallible occupancy read of a Y.Doc room (a key held by a
    /// ledger room is an error). Do not turn a poisoned registry
    /// into absence when deciding whether a first creation is admissible.
    pub(crate) fn existing_room(&self, key: &str) -> Result<Option<Arc<Room>>, String> {
        let rooms = self
            .rooms
            .lock()
            .map_err(|_| "room registry lock poisoned")?;
        match rooms.get(key) {
            Some(RoomEntry::YDoc(room)) => Ok(Some(room.clone())),
            Some(RoomEntry::Ledger(_)) => Err(format!("room key {key} is hosted by a ledger room")),
            None => Ok(None),
        }
    }

    /// Get or create the room for `key`, hydrating the Doc from
    /// `state_path` if the file exists.
    pub async fn get_or_create(&self, key: &str, state_path: PathBuf) -> Result<Arc<Room>, String> {
        self.get_or_create_classified(key, state_path)
            .await
            .map_err(Into::into)
    }

    pub(crate) async fn get_or_create_classified(
        &self,
        key: &str,
        state_path: PathBuf,
    ) -> Result<Arc<Room>, PersistenceError> {
        self.get_or_create_with_tombstone_policy(key, state_path, TombstoneOpenPolicy::Reject)
    }

    /// Open an empty room for a trusted same-ID recreation without hydrating
    /// any stale deleted sidecar. The caller must retain the matching
    /// tombstone until both the fresh document and workspace authorities are
    /// durable.
    pub(crate) async fn recreate_tombstoned_document_room(
        &self,
        key: &str,
        state_path: PathBuf,
        expected_deletion_id: &str,
    ) -> Result<Arc<Room>, PersistenceError> {
        self.get_or_create_with_tombstone_policy(
            key,
            state_path,
            TombstoneOpenPolicy::FreshRecreation(expected_deletion_id),
        )
    }

    fn get_or_create_with_tombstone_policy(
        &self,
        key: &str,
        state_path: PathBuf,
        policy: TombstoneOpenPolicy<'_>,
    ) -> Result<Arc<Room>, PersistenceError> {
        if key.starts_with("ledger:") {
            return Err(PersistenceError::permanent(format!(
                "room key {key} names a ledger room; open it with get_or_create_ledger"
            )));
        }
        crate::document_body_availability::require_state_path_available(&state_path)
            .map_err(PersistenceError::permanent)?;
        let observed_tombstone =
            crate::document_tombstone_store::tombstone_for_ydoc_state_path(&state_path)
                .map_err(PersistenceError::transient)?;
        match (&policy, observed_tombstone.as_ref()) {
            (TombstoneOpenPolicy::Reject, Some(tombstone)) => {
                return Err(PersistenceError::permanent(format!(
                    "document tombstoned: {}",
                    tombstone.document_id
                )));
            }
            (TombstoneOpenPolicy::FreshRecreation(expected), Some(tombstone))
                if tombstone.deletion_id == *expected => {}
            (TombstoneOpenPolicy::FreshRecreation(_), Some(_)) => {
                return Err(PersistenceError::permanent(
                    "document deletion boundary changed before recreation",
                ));
            }
            (TombstoneOpenPolicy::FreshRecreation(_), None) => {
                return Err(PersistenceError::permanent(
                    "document tombstone missing before recreation",
                ));
            }
            (TombstoneOpenPolicy::Reject, None) => {}
        }
        pause_after_tombstone_precheck(&state_path);

        let span = tracing::info_span!(target: "gardend_otel", "room.open",
            sophia.room.hydrated = tracing::field::Empty,
            sophia.room.state_bytes = tracing::field::Empty,
            sophia.room.decode_ms = tracing::field::Empty,
            sophia.room.slot_wait_ms = tracing::field::Empty,
            sophia.room.registry_lock_wait_ms = tracing::field::Empty,
            sophia.room.registry_lock_held_ms = tracing::field::Empty);
        let _opening = span.enter();

        // Openers of the same key queue here; openers of different keys never
        // do. The registry map is locked only for the cheap checks below and
        // for the insert, never across the disk read and the Y.Doc decode: a
        // multi-megabyte cold open used to freeze every room lookup in the
        // process for as long as it took.
        let waiting = crate::telemetry::timer();
        let slot = self.opening_slot(key);
        let open_guard = slot.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        span.record(
            "sophia.room.slot_wait_ms",
            crate::telemetry::elapsed_ms(waiting),
        );
        let mut lock_wait_ms = 0.0;

        // Phase 1: admission and the cache hit, under the locks.
        {
            let restores = self
                .disk_restores
                .lock()
                .map_err(|_| "restore room admission lock poisoned")
                .map_err(PersistenceError::permanent)?;
            if restores
                .iter()
                .any(|graph_id| room_key_belongs_to_graph(key, graph_id))
            {
                return Err(PersistenceError::transient(
                    "graph room is unavailable during disk archive restore",
                ));
            }
            let waiting = crate::telemetry::timer();
            let rooms = self
                .rooms
                .lock()
                .map_err(|_| "room registry lock poisoned".to_string())
                .map_err(PersistenceError::permanent)?;
            lock_wait_ms += crate::telemetry::elapsed_ms(waiting);
            let cached = match rooms.get(key) {
                Some(RoomEntry::YDoc(room)) => Some(room.clone()),
                Some(RoomEntry::Ledger(_)) => {
                    return Err(PersistenceError::permanent(format!(
                        "room key {key} is hosted by a ledger room"
                    )));
                }
                None => None,
            };
            if let Some(room) = cached {
                if !room.is_evicted() && matches!(&policy, TombstoneOpenPolicy::Reject) {
                    drop(rooms);
                    drop(restores);
                    drop(open_guard);
                    // Pre-existing (cache-hit) room: a recheck error returns Err
                    // but the live room stays — it was live before this call.
                    return self.finish_tombstone_checked_open(
                        key,
                        &state_path,
                        room,
                        &policy,
                        false,
                    );
                }
                // An evicted or superseded room is replaced below, atomically
                // with the insert of its successor (no window in which the key
                // is absent), once the new incarnation is decoded.
            }
        }

        // Phase 2: read and decode with no registry lock held.
        pause_during_open_decode(&state_path);
        let decoding = crate::telemetry::timer();
        let doc = Doc::new();
        let mut hydrated = false;
        if matches!(&policy, TombstoneOpenPolicy::Reject) && state_path.exists() {
            let bytes = std::fs::read(&state_path).map_err(|error| {
                PersistenceError::io(error, format!("read ydoc state {}", state_path.display()))
            })?;
            span.record("sophia.room.state_bytes", bytes.len() as u64);
            // Two on-disk shapes both mean "no application content yet": the
            // legacy zero-byte sentinel, and `write_ydoc_update`'s canonical
            // encoded-empty-Y.Doc bytes (written so an empty sidecar is still
            // a valid Yjs update at the blob/source-sync boundary — see
            // `document_sidecar_store::empty_ydoc_update_v1`). A raw byte
            // comparison against a freshly re-encoded reference does NOT
            // detect the second shape (Y.Doc encodes a random per-instance
            // client id even with zero blocks, so two "empty" encodings
            // differ byte-for-byte) — decode first and ask the update itself
            // whether it carries any blocks/deletes. Treating the
            // canonical-empty encoding as hydrated would flip
            // `projection_epoch` to 1 for every freshly-created document's
            // sidecar, making `needs_projection_flush` true on first touch
            // and letting an unrelated graph-wide sweep silently re-derive
            // (and clobber with empty content) a document.json body written
            // through the file-only `save_document` path, which never
            // touches the Y.Doc.
            if !bytes.is_empty() {
                let update = Update::decode_v1(&bytes).map_err(|error| {
                    PersistenceError::permanent(format!(
                        "decode ydoc state {}: {error}",
                        state_path.display()
                    ))
                })?;
                if !update.is_empty() {
                    let mut txn = doc.transact_mut();
                    txn.apply_update(update).map_err(|error| {
                        PersistenceError::permanent(format!(
                            "hydrate ydoc {}: {error}",
                            state_path.display()
                        ))
                    })?;
                    hydrated = true;
                }
            }
        }
        span.record("sophia.room.hydrated", hydrated);
        span.record(
            "sophia.room.decode_ms",
            crate::telemetry::elapsed_ms(decoding),
        );

        let (tx, _) = broadcast::channel(256);
        let (frames, _) = broadcast::channel(256);
        let room = Arc::new_cyclic(|me| Room {
            doc: Mutex::new(doc),
            awareness: AwarenessTable::new(),
            me: me.clone(),
            tx,
            frames,
            next_connection_id: AtomicU64::new(1),
            connections: AtomicUsize::new(0),
            state_path: state_path.clone(),
            dir_sync: Arc::new(DirSyncCadence::default()),
            state_epoch: AtomicU64::new(0),
            persisted_state_epoch: AtomicU64::new(0),
            persist_armed: AtomicBool::new(false),
            persist_gate: Mutex::new(()),
            persist_scope: OnceLock::new(),
            dirty_since: StdMutex::new(None),
            // Conservatively materialize a hydrated room once. Its hot-tier
            // authority may be newer than (or have outlived) the cold files.
            projection_epoch: AtomicU64::new(u64::from(hydrated)),
            persisted_projection_epoch: AtomicU64::new(0),
            projection_flush_lock: Mutex::new(()),
            authority_lock: Mutex::new(()),
            evicted: AtomicBool::new(false),
            eviction_notify: Notify::new(),
            projection_flush: OnceLock::new(),
        });

        // Phase 3: admission again (a disk restore may have begun while this
        // opener was decoding), then swap the entry in.
        {
            let restores = self
                .disk_restores
                .lock()
                .map_err(|_| "restore room admission lock poisoned")
                .map_err(PersistenceError::permanent)?;
            if restores
                .iter()
                .any(|graph_id| room_key_belongs_to_graph(key, graph_id))
            {
                return Err(PersistenceError::transient(
                    "graph room is unavailable during disk archive restore",
                ));
            }
            let waiting = crate::telemetry::timer();
            let mut rooms = self
                .rooms
                .lock()
                .map_err(|_| "room registry lock poisoned".to_string())
                .map_err(PersistenceError::permanent)?;
            lock_wait_ms += crate::telemetry::elapsed_ms(waiting);
            let held = crate::telemetry::timer();
            // A hot-authority write failure invalidates the in-memory Doc.
            // Reopen from the last durable sidecar instead of returning that
            // partially-mutated incarnation to a new caller. Only this
            // function inserts, and it holds the key's opening slot, so what
            // is here now is what phase 1 saw (or less).
            if let Some(removed) = rooms.remove(key) {
                removed.evict();
            }
            rooms.insert(key.to_string(), RoomEntry::YDoc(room.clone()));
            span.record(
                "sophia.room.registry_lock_held_ms",
                crate::telemetry::elapsed_ms(held),
            );
        }
        span.record("sophia.room.registry_lock_wait_ms", lock_wait_ms);
        // Freshly inserted room: a recheck that cannot even be evaluated must
        // evict it (see below) — the caller gets an Err with no Arc, so
        // nothing else would ever evict the just-decoded room, and every
        // retry would then see it as "pre-live" and skip its own cleanup: a
        // permanent one-room leak per transient tombstone-store error. The
        // eviction is IDENTITY-checked so this cleanup can never destroy a
        // newer live room installed here in the no-lock window (test hook).
        // The opening slot is released first: the window exists so that other
        // paths (and the test hook) can open this key again.
        drop(open_guard);
        pause_before_fresh_recheck(&state_path);
        self.finish_tombstone_checked_open(key, &state_path, room, &policy, true)
    }

    /// The per-key mutex that serializes cold opens of one room. The map
    /// itself is held only to fetch or create the entry.
    fn opening_slot(&self, key: &str) -> Arc<StdMutex<()>> {
        let mut slots = self
            .opening
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if slots.len() > 256 {
            // Idle entries (nobody holds or awaits them) are dead weight.
            slots.retain(|_, slot| Arc::strong_count(slot) > 1);
        }
        slots
            .entry(key.to_string())
            .or_insert_with(|| Arc::new(StdMutex::new(())))
            .clone()
    }

    fn finish_tombstone_checked_open(
        &self,
        key: &str,
        state_path: &std::path::Path,
        room: Arc<Room>,
        policy: &TombstoneOpenPolicy<'_>,
        evict_on_error: bool,
    ) -> Result<Arc<Room>, PersistenceError> {
        if let Err(error) =
            crate::document_body_availability::require_state_path_available(state_path)
        {
            self.evict_room_if_identity(key, &room);
            return Err(PersistenceError::permanent(error));
        }
        // The room is already visible in the registry when this recheck runs,
        // so an I/O/parse error out of the tombstone store must not escape
        // with the room still retained when WE inserted it. A pre-existing
        // room keeps today's behavior on the same error: Err returned, live
        // room untouched.
        let current =
            match crate::document_tombstone_store::tombstone_for_ydoc_state_path(state_path) {
                Ok(current) => current,
                Err(error) => {
                    if evict_on_error {
                        // Identity-checked: only evict the room WE inserted, not
                        // a newer live room another path may have installed at
                        // this key while we held no lock.
                        self.evict_room_if_identity(key, &room);
                    }
                    return Err(PersistenceError::transient(error));
                }
            };
        let valid = match (policy, current.as_ref()) {
            (TombstoneOpenPolicy::Reject, None) => true,
            (TombstoneOpenPolicy::FreshRecreation(expected), Some(tombstone)) => {
                tombstone.deletion_id == *expected
            }
            _ => false,
        };
        if valid {
            return Ok(room);
        }
        // Identity-checked, same rationale: a racing path may have replaced our
        // entry with a live room since we dropped the lock — never evict it.
        self.evict_room_if_identity(key, &room);
        match current {
            Some(tombstone) => Err(PersistenceError::permanent(format!(
                "document tombstoned: {}",
                tombstone.document_id
            ))),
            None => Err(PersistenceError::permanent(
                "document tombstone disappeared during recreation",
            )),
        }
    }

    /// Lookup without creating: Some only when `key` is already hosted.
    /// In a headless cell the registry doubles as the TS runtime's
    /// document-channel pool — `peek` is how document.liveProjection asks
    /// "is this doc live?" without warming a room as a side effect.
    #[doc(hidden)]
    pub async fn peek(&self, key: &str) -> Option<Arc<Room>> {
        match self.rooms.lock().ok()?.get(key) {
            Some(RoomEntry::YDoc(room)) if !room.is_evicted() => Some(room.clone()),
            _ => None,
        }
    }

    /// [`Self::peek`] for a ledger room: Some only when `key` is already hosted as one.
    pub(crate) fn peek_ledger(&self, key: &str) -> Option<Arc<LedgerRoom>> {
        match self.rooms.lock().ok()?.get(key) {
            Some(RoomEntry::Ledger(room)) => Some(room.clone()),
            _ => None,
        }
    }

    /// Get or open the ledger room for `key` (`ledger:{graph}:{vocab}`),
    /// whose ledger lives at `ledger_dir` (`{graph_dir}/ledger/{vocab}`). The cell's writer id is
    /// read from `{ledger_dir}/origin`, minted there once on first open.
    ///
    /// The same admission rules as a Y.Doc open apply: a graph with a disk restore in progress
    /// refuses, and cold opens of one key queue on its opening slot. A key held by a Y.Doc room is
    /// an error. The open (a recovery scan of the ledger) runs on the calling thread, as the Y.Doc
    /// open does.
    ///
    /// The ledger's stream files are flocked, so a reopen behind a live `Arc` of an evicted room
    /// (graph duplicate, restore: the old room's last flush and its sockets' last `Arc` are still
    /// finishing) finds them `Locked`. That is retried every [`LEDGER_LOCKED_RETRY`] for up to
    /// [`LEDGER_LOCKED_WAIT`], then fails naming the path. This function does not take the graph's
    /// lifecycle lease; a route that opens on behalf of a request uses [`Self::open_ledger`].
    pub(crate) async fn get_or_create_ledger(
        &self,
        key: &str,
        ledger_dir: PathBuf,
    ) -> Result<Arc<LedgerRoom>, String> {
        self.get_or_create_ledger_within(key, ledger_dir, LEDGER_LOCKED_WAIT)
            .await
    }

    /// [`Self::get_or_create_ledger`] with the bounded wait for a `Locked` ledger spelled out.
    pub(crate) async fn get_or_create_ledger_within(
        &self,
        key: &str,
        ledger_dir: PathBuf,
        wait: Duration,
    ) -> Result<Arc<LedgerRoom>, String> {
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            match self.open_ledger_room(key, &ledger_dir) {
                Ok(room) => return Ok(room),
                Err(LedgerOpenError::Locked(path))
                    if tokio::time::Instant::now() + LEDGER_LOCKED_RETRY <= deadline =>
                {
                    log::debug!("ledger stream {path} is locked; retrying the open of {key}");
                    tokio::time::sleep(LEDGER_LOCKED_RETRY).await;
                }
                Err(LedgerOpenError::Locked(path)) => {
                    return Err(format!(
                        "ledger {} is still locked by another handle after {wait:?} ({path}); \
                         an evicted room has not let go of it",
                        ledger_dir.display()
                    ));
                }
                Err(LedgerOpenError::Other(message)) => return Err(message),
            }
        }
    }

    /// Open the ledger room of `(graph_id, vocab)` for a request: under the graph's shared
    /// lifecycle lease (graph deletion evicts the graph's rooms under the exclusive one, so a room
    /// cannot be installed behind it), and only for a graph that is not deleted. The check is the
    /// one graph deletion itself writes and `linearize_state_bearing_delivery` reads: the graph
    /// record's `status` ([`crate::graph_record_store::read_graph_record_no_heal`] refuses
    /// `GRAPH_STATUS_DELETED` and a missing record, never healing one). `Err(NotFound)` for those.
    pub(crate) async fn open_ledger(
        &self,
        app: &AppHandle,
        graph_id: &str,
        vocab: &str,
    ) -> Result<Arc<LedgerRoom>, LedgerAdmissionError> {
        let coordinator = app
            .try_state::<super::persistence_coordinator::GraphPersistenceCoordinator>()
            .ok_or_else(|| {
                LedgerAdmissionError::Other("graph persistence coordinator is unavailable".into())
            })?;
        let _lease = coordinator
            .acquire_lifecycle_shared(graph_id)
            .await
            .map_err(LedgerAdmissionError::Other)?;
        let (graph_dir, _record) =
            crate::graph_record_store::read_graph_record_no_heal(app, graph_id)
                .map_err(|error| LedgerAdmissionError::NotFound(error.message()))?;
        let room = self
            .get_or_create_ledger(
                &format!("ledger:{graph_id}:{vocab}"),
                graph_dir.join("ledger").join(vocab),
            )
            .await
            .map_err(LedgerAdmissionError::Other)?;
        // The fold host (W7 section 4): one tail per hosted room, started here because it needs the
        // app handle (the hot-write lease, the graph store) that `get_or_create_ledger` does not
        // carry. Idempotent for a room that already has one; it lets go of the room at eviction.
        crate::emporium::ledger_fold::tail::ensure_tail(app, graph_id, vocab, &graph_dir, &room);
        Ok(room)
    }

    /// The version map of the ledger `key` at `ledger_dir` without creating any state: the hosted
    /// room's when one is hosted, else a cold read (see [`LedgerRoom::cold_version_json`]) taken
    /// under the key's opening slot, so it cannot race a room open for the stream locks.
    ///
    /// The cold read opens the streams (an fsync, and a torn tail may be cut), so it runs on a
    /// blocking thread, which takes the opening slot itself. A ledger still locked by an evicted
    /// room's last flush is retried every [`LEDGER_LOCKED_RETRY`] for up to [`LEDGER_LOCKED_WAIT`],
    /// as a room open is, and each retry looks for a hosted room first (one may have been
    /// installed meanwhile; it then answers, and the cold read never contends with it).
    pub(crate) async fn ledger_version(
        &self,
        key: &str,
        ledger_dir: &std::path::Path,
    ) -> Result<serde_json::Value, String> {
        let deadline = tokio::time::Instant::now() + LEDGER_LOCKED_WAIT;
        loop {
            if let Some(room) = self.peek_ledger(key) {
                return Ok(room.version_json().await);
            }
            let slot = self.opening_slot(key);
            let dir = ledger_dir.to_path_buf();
            let cold = tokio::task::spawn_blocking(move || {
                let _guard = slot.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                LedgerRoom::cold_version_json(&dir)
            })
            .await
            .map_err(|error| format!("read ledger version {}: {error}", ledger_dir.display()))?;
            match cold {
                Ok(body) => return Ok(body),
                Err(ConvergeError::Locked(path))
                    if tokio::time::Instant::now() + LEDGER_LOCKED_RETRY <= deadline =>
                {
                    log::debug!(
                        "ledger stream {path} is locked; retrying the version read of {key}"
                    );
                    tokio::time::sleep(LEDGER_LOCKED_RETRY).await;
                }
                Err(ConvergeError::Locked(path)) => {
                    return Err(format!(
                        "read ledger version {}: still locked by another handle after \
                         {LEDGER_LOCKED_WAIT:?} ({path}); an evicted room has not let go of it",
                        ledger_dir.display()
                    ));
                }
                Err(error) => {
                    return Err(format!(
                        "read ledger version {}: {error}",
                        ledger_dir.display()
                    ));
                }
            }
        }
    }

    fn open_ledger_room(
        &self,
        key: &str,
        ledger_dir: &std::path::Path,
    ) -> Result<Arc<LedgerRoom>, LedgerOpenError> {
        if !key.starts_with("ledger:") {
            return Err(
                format!("room key {key} is not a ledger key (ledger:{{graph}}:{{vocab}})").into(),
            );
        }
        let vocab = ledger_dir
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| {
                format!(
                    "ledger directory {} has no vocab name",
                    ledger_dir.display()
                )
            })?
            .to_string();
        if !key.ends_with(&format!(":{vocab}")) {
            return Err(format!(
                "ledger key {key} does not name the vocab of {}",
                ledger_dir.display()
            )
            .into());
        }
        let graph_dir = ledger_dir
            .parent()
            .filter(|parent| parent.file_name().is_some_and(|name| name == "ledger"))
            .and_then(|parent| parent.parent())
            .ok_or_else(|| {
                format!(
                    "ledger directory {} is not {{graph_dir}}/ledger/{{vocab}}",
                    ledger_dir.display()
                )
            })?
            .to_path_buf();

        let slot = self.opening_slot(key);
        let open_guard = slot.lock().unwrap_or_else(|poisoned| poisoned.into_inner());

        // Phase 1: admission and the cache hit, under the locks.
        {
            let restores = self
                .disk_restores
                .lock()
                .map_err(|_| "restore room admission lock poisoned".to_string())?;
            if restores
                .iter()
                .any(|graph_id| room_key_belongs_to_graph(key, graph_id))
            {
                return Err("graph room is unavailable during disk archive restore"
                    .to_string()
                    .into());
            }
            let rooms = self
                .rooms
                .lock()
                .map_err(|_| "room registry lock poisoned".to_string())?;
            match rooms.get(key) {
                Some(RoomEntry::Ledger(room)) => return Ok(room.clone()),
                Some(RoomEntry::YDoc(_)) => {
                    return Err(format!("room key {key} is hosted by a Y.Doc room").into());
                }
                None => {}
            }
        }

        // Phase 2: recover and open with no registry lock held.
        let local =
            garden_ledger::ledger::read_or_mint_origin(ledger_dir).map_err(
                |error| match error {
                    garden_ledger::LedgerError::Stream(garden_ledger::StreamError::Locked {
                        path,
                    }) => LedgerOpenError::Locked(path.display().to_string()),
                    other => LedgerOpenError::Other(format!(
                        "ledger origin in {}: {other}",
                        ledger_dir.display()
                    )),
                },
            )?;
        let room = LedgerRoom::open(&graph_dir, &vocab, local).map_err(|error| match error {
            ConvergeError::Locked(path) => LedgerOpenError::Locked(path),
            other => {
                LedgerOpenError::Other(format!("open ledger {}: {other}", ledger_dir.display()))
            }
        })?;

        // Phase 3: admission again (a disk restore may have begun while this opener was
        // recovering), then insert. Only this function inserts ledger keys and it holds the key's
        // opening slot, so nothing was added since phase 1.
        {
            let restores = self
                .disk_restores
                .lock()
                .map_err(|_| "restore room admission lock poisoned".to_string())?;
            if restores
                .iter()
                .any(|graph_id| room_key_belongs_to_graph(key, graph_id))
            {
                // The room just opened was never installed: let go of its streams at once.
                drop(room);
                return Err("graph room is unavailable during disk archive restore"
                    .to_string()
                    .into());
            }
            let mut rooms = self
                .rooms
                .lock()
                .map_err(|_| "room registry lock poisoned".to_string())?;
            if let Some(removed) = rooms.insert(key.to_string(), RoomEntry::Ledger(room.clone())) {
                removed.evict();
            }
        }
        drop(open_guard);
        Ok(room)
    }

    /// Remove and invalidate one hosted room. Document deletion uses this under
    /// the graph persistence lease before removing cold/sidecar files so an
    /// extant websocket Arc and its pending debounce cannot resurrect them.
    pub(crate) fn evict_room(&self, key: &str) -> bool {
        let room = match self.rooms.lock() {
            Ok(mut rooms) => rooms.remove(key),
            Err(_) => {
                log::error!("room registry lock poisoned while evicting {key}");
                return false;
            }
        };
        if let Some(room) = room {
            room.evict();
            true
        } else {
            false
        }
    }

    /// Remove and invalidate the hosted room for `key` ONLY if the current
    /// registry entry is `expected` (the very same `Arc`). Used by an opener's
    /// error cleanup: the opener drops the registry lock between inserting its
    /// room and the final tombstone recheck, so by the time it fails another
    /// path may have evicted the opener's room and installed a NEW live room
    /// at the same key. Evicting BY KEY would then destroy that healthy,
    /// in-use room. The get + `Arc::ptr_eq` + remove all run under one lock so
    /// the check is atomic. Returns true only when `expected` was the current
    /// entry and was evicted.
    pub(crate) fn evict_room_if_identity(&self, key: &str, expected: &Arc<Room>) -> bool {
        let removed = match self.rooms.lock() {
            Ok(mut rooms) => {
                let is_same = rooms.get(key).is_some_and(
                    |current| matches!(current, RoomEntry::YDoc(room) if Arc::ptr_eq(room, expected)),
                );
                if is_same {
                    rooms.remove(key)
                } else {
                    None
                }
            }
            Err(_) => {
                log::error!("room registry lock poisoned while identity-evicting {key}");
                return false;
            }
        };
        if let Some(room) = removed {
            room.evict();
            true
        } else {
            false
        }
    }

    /// A graph read repairs cold sidecars one at a time. Drop a room it alone
    /// opened, but retain any concurrent reader/writer/socket or newer dirt.
    pub(crate) fn evict_repaired_idle_room(&self, key: &str, expected: &Arc<Room>) {
        if let Ok(mut rooms) = self.rooms.lock() {
            if rooms.get(key).is_some_and(|entry| match entry {
                RoomEntry::YDoc(room) => {
                    Arc::ptr_eq(room, expected)
                        && Arc::strong_count(room) == 2
                        && !room.needs_projection_flush()
                        && !room.state_is_dirty()
                }
                RoomEntry::Ledger(_) => false,
            }) {
                if let Some(entry) = rooms.remove(key) {
                    entry.evict();
                }
            }
        }
    }

    /// Snapshot hosted Y.Doc rooms whose keys start with `prefix` (ledger
    /// rooms are skipped: they are not `Room`s). Sorting makes a
    /// graph-wide flush deterministic and avoids holding the registry lock
    /// while filesystem/RDF persistence runs.
    pub(crate) async fn rooms_with_prefix(&self, prefix: &str) -> Vec<(String, Arc<Room>)> {
        let Ok(rooms) = self.rooms.lock() else {
            return Vec::new();
        };
        let mut matches: Vec<_> = rooms
            .iter()
            .filter(|(key, _)| key.starts_with(prefix))
            .filter_map(|(key, entry)| match entry {
                RoomEntry::YDoc(room) if !room.is_evicted() => Some((key.clone(), room.clone())),
                _ => None,
            })
            .collect();
        matches.sort_by(|(left, _), (right, _)| left.cmp(right));
        matches
    }

    /// The rooms of `graph_id`, of either kind, that have something to flush.
    fn live_rooms_for_graph(&self, graph_id: &str) -> Vec<RoomEntry> {
        let Ok(rooms) = self.rooms.lock() else {
            return Vec::new();
        };
        rooms
            .iter()
            .filter(|(key, entry)| {
                room_key_belongs_to_graph(key, graph_id)
                    && !entry.is_evicted()
                    && entry.needs_flush()
            })
            .map(|(_, entry)| entry.clone())
            .collect()
    }

    /// Write every dirty room of `graph_id` to its sidecar. For callers that
    /// read sidecar bytes directly and hold the graph's lease (source pull).
    pub(crate) async fn flush_graph_rooms(&self, graph_id: &str) -> Result<(), PersistenceError> {
        for entry in self.live_rooms_for_graph(graph_id) {
            match entry {
                RoomEntry::YDoc(room) => {
                    room.flush_state_if_dirty(false).await?;
                }
                // The ordered-durability barrier: the bytes a source pull or a restore point reads
                // are on the file. Shipping to peers rides `durable`, not this.
                RoomEntry::Ledger(room) => room.persist().await?,
            }
        }
        Ok(())
    }

    /// [`Self::flush_graph_rooms`] for synchronous callers that already hold
    /// the graph's lease (restore-point capture).
    pub(crate) fn flush_graph_rooms_blocking(
        &self,
        graph_id: &str,
    ) -> Result<(), PersistenceError> {
        for entry in self.live_rooms_for_graph(graph_id) {
            match entry {
                RoomEntry::YDoc(room) => room.flush_state_blocking()?,
                RoomEntry::Ledger(room) => {
                    block_on_fresh_thread(async move { room.persist().await })?
                }
            }
        }
        Ok(())
    }

    /// Shutdown: every dirty room reaches disk, directory entry included.
    /// Returns the number of rooms written. A failure is logged and does not
    /// stop the remaining rooms.
    pub(crate) async fn flush_dirty_rooms(&self) -> usize {
        let dirty: Vec<RoomEntry> = match self.rooms.lock() {
            Ok(rooms) => rooms
                .values()
                .filter(|entry| !entry.is_evicted() && entry.needs_flush())
                .cloned()
                .collect(),
            Err(_) => Vec::new(),
        };
        let mut written = 0;
        for entry in dirty {
            match entry {
                RoomEntry::YDoc(room) => {
                    match room.flush_state_under_lifecycle(true).await {
                        Ok(true) => written += 1,
                        Ok(false) => {}
                        Err(error) => log::error!("shutdown room flush failed: {error}"),
                    }
                    room.settle_state_dir().await;
                }
                // Shutdown is the durability point for a ledger: the barrier, then the full flush
                // that lets every frame ship.
                RoomEntry::Ledger(room) => {
                    let barrier = room.persist().await;
                    let full = room.durable().await;
                    match (barrier, full) {
                        (Ok(()), Ok(())) => written += 1,
                        (barrier, full) => {
                            if let Err(error) = barrier {
                                log::error!("shutdown ledger barrier failed: {error}");
                            }
                            if let Err(error) = full {
                                log::error!("shutdown ledger flush failed: {error}");
                            }
                        }
                    }
                }
            }
        }
        // Shutdown is a durability point: the ordered writes above (and any the flusher still owes) reach the
        // media with a full flush, synchronously, before the caller goes on to exit.
        match crate::app_runtime::async_runtime::spawn_blocking(crate::storage_atomic::flush_now)
            .await
        {
            Ok(Ok(())) => {}
            Ok(Err(error)) => log::error!("shutdown full flush failed: {error}"),
            Err(error) => log::error!("shutdown full flush join: {error}"),
        }
        written
    }

    /// Remove every hosted room for a graph and synchronously cancel its
    /// debounce/retry generation. Existing websocket tasks may still hold a
    /// room Arc briefly, but an evicted room rejects further client updates and
    /// cannot enqueue another cold projection.
    pub(crate) fn evict_graph(&self, graph_id: &str) -> usize {
        let removed = {
            let Ok(mut rooms) = self.rooms.lock() else {
                log::error!("room registry lock poisoned while evicting graph {graph_id}");
                return 0;
            };
            let keys = rooms
                .keys()
                .filter(|key| room_key_belongs_to_graph(key, graph_id))
                .cloned()
                .collect::<Vec<_>>();
            keys.into_iter()
                .filter_map(|key| rooms.remove(&key))
                .collect::<Vec<_>>()
        };
        for entry in &removed {
            entry.evict();
        }
        removed.len()
    }
}

impl Room {
    fn allocate_connection_id(&self) -> ConnectionId {
        self.next_connection_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Hand one encoded y-protocol message to every subscriber. Sockets read
    /// `frames`; the raw `tx` only has receivers in tests, so production pays
    /// no extra copy for it.
    fn publish(&self, origin: Option<ConnectionId>, kind: FrameKind, payload: Vec<u8>) {
        if self.tx.receiver_count() > 0 {
            let _ = self.tx.send(payload.clone());
        }
        let _ = self.frames.send(RoomFrame {
            origin,
            kind,
            payload,
        });
    }

    /// Install the headless cold-projection scheduler for this hosted room.
    /// Multiple websocket clients race through this safely: the first target
    /// wins and later clients must describe the same room. A hydrated hot-tier
    /// room starts dirty, so merely reopening it also repairs a cold projection
    /// left stale by a prior process exit.
    pub(crate) fn configure_projection_flush(
        &self,
        graph_id: String,
        document_id: Option<String>,
    ) -> Result<(), String> {
        {
            let debounce = if document_id.is_some() {
                DOCUMENT_PROJECTION_DEBOUNCE
            } else {
                WORKSPACE_PROJECTION_DEBOUNCE
            };
            let target = Arc::new(ProjectionFlushTarget {
                graph_id,
                document_id,
                debounce,
                generation: AtomicU64::new(0),
                graph_generation: AtomicU64::new(0),
                contexts: StdMutex::new(FlushContexts::default()),
                timer: StdMutex::new(DebounceTimer::default()),
            });
            if let Err(rejected) = self.projection_flush.set(target) {
                let existing = self
                    .projection_flush
                    .get()
                    .expect("projection flush target is set");
                if existing.graph_id != rejected.graph_id
                    || existing.document_id != rejected.document_id
                {
                    return Err("room projection flush target changed after initialization".into());
                }
            }
            Ok(())
        }
    }

    pub(crate) fn schedule_projection_flush(self: &Arc<Self>, app: AppHandle) {
        if let Some(target) = self.projection_flush.get() {
            if self.needs_projection_flush() && !self.is_evicted() {
                let graph_generation =
                    match app
                        .try_state::<super::persistence_coordinator::GraphPersistenceCoordinator>()
                    {
                        Some(coordinator) => match coordinator.generation(&target.graph_id) {
                            Ok(generation) => generation,
                            Err(error) => {
                                log::error!(
                                    "projection generation lookup failed for {}: {error}",
                                    target.graph_id
                                );
                                return;
                            }
                        },
                        None => 0,
                    };
                target.schedule(Arc::downgrade(self), app, graph_generation);
            }
        }
    }

    fn evict(&self) {
        if self.evicted.swap(true, Ordering::SeqCst) {
            return;
        }
        if let Some(target) = self.projection_flush.get() {
            target.generation.fetch_add(1, Ordering::SeqCst);
        }
        self.eviction_notify.notify_waiters();
    }

    pub(super) fn is_evicted(&self) -> bool {
        self.evicted.load(Ordering::SeqCst)
    }

    async fn wait_until_evicted(&self) {
        if self.is_evicted() {
            return;
        }
        // `notified()` does not join Notify's waiter list until first poll.
        // Pin + enable it before the second atomic check so notify_waiters()
        // cannot fall into the release-before-first-await gap.
        let notified = self.eviction_notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if self.is_evicted() {
            return;
        }
        notified.await;
    }

    /// The entries of `update` whose client id no other live socket holds
    /// (a visitor's awareness; `None` when nothing is left).
    async fn awareness_held_by_this_socket(
        &self,
        update: AwarenessUpdate,
        owner: ConnectionId,
    ) -> Option<AwarenessUpdate> {
        self.awareness.held_by_this_socket(update, owner)
    }

    #[cfg(test)]
    async fn merge_awareness(
        &self,
        update: &AwarenessUpdate,
        owner: ConnectionId,
    ) -> Option<AwarenessUpdate> {
        self.merge_awareness_tagged(update, owner)
            .await
            .map(|(accepted, _)| accepted)
    }

    /// Merge one websocket's awareness update into the room's live state
    /// (see [`AwarenessTable::merge_detailed`]), plus whether every accepted
    /// entry belongs to `owner`'s own live client ids. Only then may the
    /// sender skip its echo: synthetic removals of superseded ids are news to
    /// the sender too.
    async fn merge_awareness_tagged(
        &self,
        update: &AwarenessUpdate,
        owner: ConnectionId,
    ) -> Option<(AwarenessUpdate, bool)> {
        self.awareness.merge_detailed(update, owner)
    }

    async fn full_awareness(&self) -> Option<AwarenessUpdate> {
        self.awareness.full()
    }

    /// Remove every awareness entry still owned by one websocket. Ownership
    /// may already have moved to a newer incarnation; those entries survive.
    async fn remove_awareness_for_connection(
        &self,
        owner: ConnectionId,
    ) -> Option<AwarenessUpdate> {
        self.awareness.remove_connection(owner)
    }

    /// Run a local (server-originated) mutation inside one transaction:
    /// the resulting update broadcasts to connected sync clients and the
    /// authoritative state persists. This is how the in-process executor
    /// writes — collaborators see API writes live.
    #[doc(hidden)]
    pub async fn update_doc<T, F>(&self, mutate: F) -> Result<T, String>
    where
        F: FnOnce(&Doc, &mut yrs::TransactionMut<'_>) -> Result<T, String>,
    {
        self.update_doc_classified(mutate).await.map_err(Into::into)
    }

    pub(crate) async fn update_doc_classified<T, F>(&self, mutate: F) -> Result<T, PersistenceError>
    where
        F: FnOnce(&Doc, &mut yrs::TransactionMut<'_>) -> Result<T, String>,
    {
        let _authority_guard = self.authority_lock.lock().await;
        if self.is_evicted() {
            return Err(PersistenceError::permanent("room was evicted"));
        }
        let mutation = {
            let doc = self.doc.lock().await;
            let mut txn = doc.transact_mut();
            match mutate(&doc, &mut txn)
                .map_err(PersistenceError::permanent)
                .and_then(|result| {
                    // W6: a fenced operation's document writes meet the visitor
                    // content policy before they commit (an Err evicts the room,
                    // so nothing of it persists).
                    match crate::visitor_fence::current_room_policy() {
                        Some(policy) => self
                            .check_visitor_policy(&policy, &txn)
                            .map(|()| result)
                            .map_err(PersistenceError::permanent),
                        None => Ok(result),
                    }
                }) {
                Ok(result) => {
                    let update = txn.encode_update_v1();
                    if update.len() > 3 {
                        self.projection_epoch.fetch_add(1, Ordering::SeqCst);
                        self.state_epoch.fetch_add(1, Ordering::SeqCst);
                    }
                    Ok((result, update))
                }
                Err(error) => Err(error),
            }
        };
        let (result, update) = match mutation {
            Ok(outcome) => outcome,
            Err(error) => {
                // A yrs transaction is committed when dropped even if the
                // mutation closure returns Err. Invalidate this partial Doc so
                // the registry reopens the last durable sidecar; never let the
                // unacknowledged mutation leak into a later cold flush.
                self.evict();
                return Err(error);
            }
        };
        // The hot authority must reach disk before collaborators or the
        // operation caller observe success. On failure `persist` evicts this
        // partially-mutated room so a later connection rehydrates the last
        // durable state instead of flushing it cold.
        self.persist_classified().await?;
        // An empty transaction still encodes a few framing bytes; only
        // broadcast when content actually changed.
        if update.len() > 3 {
            let message = Message::Sync(SyncMessage::Update(update)).encode_v1();
            self.publish(None, FrameKind::State, message);
        }
        Ok(result)
    }

    /// The document this room holds, from its state path
    /// (`ydocs/documents/<id>/update-v1.bin`); `None` for the workspace room.
    fn document_id(&self) -> Option<String> {
        let dir = self.state_path.parent()?;
        let parent = dir.parent()?;
        (parent.file_name()? == "documents" && parent.parent()?.file_name()? == "ydocs")
            .then(|| dir.file_name()?.to_str().map(str::to_string))
            .flatten()
    }

    /// W6: a fenced operation writes only its own document's room, and leaves
    /// it within the visitor content policy.
    fn check_visitor_policy(
        &self,
        policy: &crate::visitor_fence::RoomPolicy,
        txn: &yrs::TransactionMut<'_>,
    ) -> Result<(), String> {
        let Some(document_id) = self.document_id() else {
            return Ok(()); // the workspace room: the operation's own checks hold
        };
        if policy.document_id.as_deref() != Some(document_id.as_str()) {
            return Err(format!(
                "{}a visitor's write reached the document {document_id:?}, which it does not name",
                crate::visitor_fence::POLICY_REFUSAL
            ));
        }
        let size = txn
            .encode_state_as_update_v1(&yrs::StateVector::default())
            .len();
        crate::visitor_fence::check_document_policy(txn, &policy.fence, size, None)
            .map_err(|error| format!("{}{error}", crate::visitor_fence::POLICY_REFUSAL))
    }

    /// W6: what one client update would do to this room, worked out on a copy
    /// (the room is untouched): whether it changes anything, and if so whether
    /// `check` accepts the document it would make (`check` gets the copy, its
    /// encoded size and the comments before the update).
    pub(crate) async fn shadow_apply(
        &self,
        update: &[u8],
        check: impl FnOnce(
            &yrs::Transaction<'_>,
            usize,
            &serde_json::Map<String, serde_json::Value>,
        ) -> Result<(), String>,
    ) -> Result<bool, String> {
        let state = {
            let doc = self.doc.lock().await;
            let txn = doc.transact();
            txn.encode_state_as_update_v1(&yrs::StateVector::default())
        };
        let shadow = Doc::new();
        {
            let mut txn = shadow.transact_mut();
            let base =
                Update::decode_v1(&state).map_err(|error| format!("decode room state: {error}"))?;
            txn.apply_update(base)
                .map_err(|error| format!("copy room state: {error}"))?;
        }
        let before = {
            let txn = shadow.transact();
            crate::visitor_fence::document_comments(&txn)
        };
        let changed = {
            let decoded = Update::decode_v1(update)
                .map_err(|error| format!("decode client update: {error}"))?;
            let mut txn = shadow.transact_mut();
            txn.apply_update(decoded)
                .map_err(|error| format!("apply client update: {error}"))?;
            txn.encode_update_v1().len() > 3
        };
        if !changed {
            return Ok(false);
        }
        let txn = shadow.transact();
        let size = txn
            .encode_state_as_update_v1(&yrs::StateVector::default())
            .len();
        check(&txn, size, &before)?;
        Ok(true)
    }

    /// Apply a sync-client update through the same path used by the websocket
    /// host. Returns false for a valid update that did not change room state.
    #[tracing::instrument(target = "gardend_otel", name = "ydoc.update.apply", skip_all,
        fields(sophia.ydoc.update_bytes = update.len() as u64,
            sophia.ydoc.mutex_wait_ms = tracing::field::Empty,
            sophia.ydoc.changed = tracing::field::Empty,
            otel.status_code = tracing::field::Empty))]
    pub(crate) async fn apply_client_update(&self, update: &[u8]) -> Result<bool, String> {
        let result = self.apply_client_update_inner(update, None, false).await;
        if result.is_err() {
            tracing::Span::current().record("otel.status_code", "ERROR");
        }
        result
    }

    /// Apply a sync-client update and schedule its cold projection (no
    /// connection to exclude from the broadcast).
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) async fn apply_client_update_and_schedule(
        self: &Arc<Self>,
        update: &[u8],
        app: AppHandle,
    ) -> Result<bool, String> {
        self.apply_client_frame(update, app, None).await
    }

    /// [`Self::apply_client_update_and_schedule`] for a frame that arrived on
    /// connection `origin`, which will not be sent its own update back.
    /// Keep the update current through scheduling so a coalesced flush links
    /// to the update it absorbed, even after this connection closes.
    #[tracing::instrument(target = "gardend_otel", name = "ydoc.update.apply", skip_all,
        fields(sophia.ydoc.update_bytes = update.len() as u64,
            sophia.ydoc.mutex_wait_ms = tracing::field::Empty,
            sophia.ydoc.changed = tracing::field::Empty,
            otel.status_code = tracing::field::Empty))]
    async fn apply_client_frame(
        self: &Arc<Self>,
        update: &[u8],
        app: AppHandle,
        origin: Option<ConnectionId>,
    ) -> Result<bool, String> {
        // Only a websocket frame on a room the host bound defers its sidecar
        // write. Source sync and every other caller of the plain path still
        // return only after their change is on disk.
        let defer = origin.is_some() && self.persist_scope.get().is_some();
        let changed = match self.apply_client_update_inner(update, origin, defer).await {
            Ok(changed) => changed,
            Err(error) => {
                tracing::Span::current().record("otel.status_code", "ERROR");
                return Err(error);
            }
        };
        if changed {
            if defer {
                self.arm_state_persist();
            }
            self.schedule_projection_flush(app);
        }
        Ok(changed)
    }

    /// `defer`: broadcast without persisting; the caller arms the group commit
    /// (`arm_state_persist`) that makes the change durable within the window.
    async fn apply_client_update_inner(
        &self,
        update: &[u8],
        origin: Option<ConnectionId>,
        defer: bool,
    ) -> Result<bool, String> {
        let waiting = crate::telemetry::timer();
        let _authority_guard = self.authority_lock.lock().await;
        if self.is_evicted() {
            return Err("room was evicted".to_string());
        }
        let applied = {
            let doc = self.doc.lock().await;
            tracing::Span::current().record(
                "sophia.ydoc.mutex_wait_ms",
                crate::telemetry::elapsed_ms(waiting),
            );
            // `yrs::Update` is not Send, so decode only after the async lock
            // acquisition. Decode still precedes TransactionMut construction:
            // a malformed wire frame cannot dirty or evict a valid room.
            let decoded = Update::decode_v1(update)
                .map_err(|error| format!("decode client ydoc update: {error}"))?;
            let mut txn = doc.transact_mut();
            match txn.apply_update(decoded) {
                Ok(()) => {
                    let applied = txn.encode_update_v1();
                    let changed = applied.len() > 3;
                    if changed {
                        self.projection_epoch.fetch_add(1, Ordering::SeqCst);
                        self.state_epoch.fetch_add(1, Ordering::SeqCst);
                    }
                    Ok(changed)
                }
                Err(error) => Err(format!("apply client ydoc update: {error}")),
            }
        };
        let changed = match applied {
            Ok(changed) => changed,
            Err(error) => {
                // yrs may have integrated a prefix before reporting an update
                // error. Discard this room incarnation just like a failed
                // server mutation so no partial, unacknowledged state leaks.
                self.evict();
                return Err(error);
            }
        };
        tracing::Span::current().record("sophia.ydoc.changed", changed);
        if changed {
            if defer {
                self.note_state_dirty();
            } else {
                self.persist().await?;
            }
            let rebroadcast = Message::Sync(SyncMessage::Update(update.to_vec())).encode_v1();
            self.publish(origin, FrameKind::State, rebroadcast);
        }
        Ok(changed)
    }

    /// Materialize projections from the current doc state.
    #[doc(hidden)]
    pub async fn with_doc<T, F: FnOnce(&Doc) -> T>(&self, read: F) -> T {
        let doc = self.doc.lock().await;
        read(&doc)
    }

    /// Materialize a projection and capture the exact room epoch represented
    /// by it while holding the Y.Doc lock. A later client update advances the
    /// epoch, so marking this snapshot persisted cannot accidentally clear a
    /// newer dirty state.
    pub(crate) async fn with_doc_version<T, F: FnOnce(&Doc) -> T>(&self, read: F) -> (u64, T) {
        // A projection (and a read fence) must never run ahead of the sidecar:
        // bring the group-commit tail to disk first, so after a crash the cold
        // files are at most as new as the authority. The check is repeated
        // with the doc mutex held (state_epoch only moves under it, so it is
        // exact there); a frame that lands between the flush and the lock
        // sends us around again. After a few rounds of continuous typing the
        // read proceeds: the lag is then bounded by the commit window.
        let mut rounds = 0;
        loop {
            if let Err(error) = self.flush_state_if_dirty(false).await {
                log::warn!(
                    "room state flush before projection failed for {}: {error}",
                    self.state_path.display()
                );
            }
            let waiting = crate::telemetry::timer();
            let _authority_guard = self.authority_lock.lock().await;
            let doc = self.doc.lock().await;
            rounds += 1;
            if self.state_is_dirty() && !self.is_evicted() && rounds < 4 {
                continue;
            }
            if let Some(started) = waiting {
                tracing::Span::current().record(
                    "sophia.lock.wait_ms",
                    started.elapsed().as_secs_f64() * 1000.0,
                );
            }
            let held = crate::telemetry::timer();
            let epoch = self.projection_epoch.load(Ordering::SeqCst);
            let value = read(&doc);
            if let Some(started) = held {
                tracing::Span::current().record(
                    "sophia.lock.held_ms",
                    started.elapsed().as_secs_f64() * 1000.0,
                );
            }
            return (epoch, value);
        }
    }

    /// The room's committed epoch, without taking any lock. Every acknowledged
    /// write bumped it before it was acknowledged (`update_doc_classified` and
    /// `apply_client_update_inner` bump under the doc lock, then persist, then
    /// reply), so a reader that starts after an acknowledgement sees it here.
    /// A write still in flight is not yet acknowledged and need not be seen.
    pub(crate) fn committed_projection_epoch(&self) -> u64 {
        self.projection_epoch.load(Ordering::SeqCst)
    }

    /// The sidecar as the projection being materialized sees it. Only
    /// meaningful inside `with_doc_version`, which holds `authority_lock`:
    /// a room write holds that lock across its persist, so the file is then
    /// exactly the state of the Doc being read.
    pub(crate) fn witness_sidecar(&self) -> Option<super::projection_stamp::SidecarWitness> {
        super::projection_stamp::witness_sidecar(&self.state_path)
    }

    pub(crate) fn needs_projection_flush(&self) -> bool {
        self.projection_epoch.load(Ordering::SeqCst)
            > self.persisted_projection_epoch.load(Ordering::SeqCst)
    }

    /// Completion covers the record, RDF and history tail, not merely the
    /// record rename. Epochs are local to this room; reopening starts dirty.
    pub(crate) fn projection_reaches(&self, epoch: u64) -> bool {
        self.persisted_projection_epoch.load(Ordering::SeqCst) >= epoch
    }

    /// Force one cold-projection pass without changing the authoritative
    /// Y.Doc. Source-sync rebuilds use this after deliberately wiping derived
    /// RDF: an equal Y.Doc update is correctly a CRDT no-op, but its faces
    /// still have to be materialized again.
    pub(crate) fn force_projection_rebuild(&self) {
        self.projection_epoch.fetch_add(1, Ordering::SeqCst);
    }

    pub(crate) async fn lock_projection_flush(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.projection_flush_lock.lock().await
    }

    pub(crate) fn mark_projection_persisted(&self, epoch: u64) {
        self.persisted_projection_epoch
            .fetch_max(epoch, Ordering::SeqCst);
    }

    /// Persist the full authoritative state before returning. API writes
    /// (`update_doc`) and every non-websocket caller acknowledge on this.
    async fn persist(&self) -> Result<(), String> {
        self.persist_classified().await.map_err(Into::into)
    }

    async fn persist_classified(&self) -> Result<(), PersistenceError> {
        self.persist_state(false, false).await.map(|_| ())
    }

    /// Whether the live Y.Doc holds changes the sidecar does not.
    fn state_is_dirty(&self) -> bool {
        self.state_epoch.load(Ordering::SeqCst) > self.persisted_state_epoch.load(Ordering::SeqCst)
    }

    #[cfg(test)]
    pub(crate) fn state_is_dirty_for_test(&self) -> bool {
        self.state_is_dirty()
    }

    /// Remember when the oldest unpersisted change happened. Costs nothing
    /// unless telemetry is on.
    fn note_state_dirty(&self) {
        if crate::telemetry::enabled() {
            self.dirty_since
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .get_or_insert_with(Instant::now);
        }
    }

    /// Write the room's whole state to the sidecar (atomic rename, file fsync).
    /// `only_if_dirty`: skip when nothing changed since the last write (group
    /// commit and flushes); an API write persists unconditionally.
    /// `force_dir_sync`: also make the directory entry durable now (close,
    /// shutdown). Returns whether a write happened. On failure the room is
    /// evicted: it is not a faithful picture of the disk, and a reconnecting
    /// client re-pushes whatever the sidecar lacks.
    #[tracing::instrument(target = "gardend_otel", name = "ydoc.state.persist", skip_all,
        fields(sophia.ydoc.state_bytes = tracing::field::Empty,
            sophia.ydoc.batch_updates = tracing::field::Empty,
            sophia.ydoc.dirty_lag_ms = tracing::field::Empty))]
    async fn persist_state(
        &self,
        only_if_dirty: bool,
        force_dir_sync: bool,
    ) -> Result<bool, PersistenceError> {
        let _gate = self.persist_gate.lock().await;
        if only_if_dirty && (!self.state_is_dirty() || self.is_evicted()) {
            return Ok(false);
        }
        let (bytes, epoch) = async {
            let doc = self.doc.lock().await;
            // Read with the doc mutex held: no change can land between the
            // epoch and the bytes it names.
            let epoch = self.state_epoch.load(Ordering::SeqCst);
            let txn = doc.transact();
            (
                txn.encode_state_as_update_v1(&yrs::StateVector::default()),
                epoch,
            )
        }
        .instrument(tracing::info_span!(target: "gardend_otel", "ydoc.state.encode"))
        .await;
        if crate::telemetry::enabled() {
            let span = tracing::Span::current();
            span.record(
                "sophia.ydoc.batch_updates",
                epoch.saturating_sub(self.persisted_state_epoch.load(Ordering::SeqCst)),
            );
            if let Some(since) = self
                .dirty_since
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .take()
            {
                span.record(
                    "sophia.ydoc.dirty_lag_ms",
                    since.elapsed().as_secs_f64() * 1000.0,
                );
            }
        }
        // The commit section runs on a blocking thread: `write_guard()` takes
        // a blocking read of the flush gate, and parking a tokio worker on it
        // starves the runtime once a few persists stack up behind a flush —
        // `/health` included, which flaps the pod NotReady and 503s the whole
        // cell. The guard is `!Send` by design: acquired and released entirely
        // inside the closure, only owned data crosses in.
        let state_path = self.state_path.clone();
        let cadence = self.dir_sync.clone();
        tracing::Span::current().record("sophia.ydoc.state_bytes", bytes.len() as u64);
        let persisted = crate::app_runtime::async_runtime::spawn_blocking(
            crate::telemetry::blocking(move || {
                let existed = state_path.exists();
                let sync_dir = cadence.due_at(Instant::now(), existed, force_dir_sync);
                Room::persist_state_bytes_with(&state_path, &bytes, sync_dir)
            }),
        )
        .await
        .map_err(|error| format!("room persist join: {error}"))
        .map_err(PersistenceError::transient)?;
        if let Err(error) = persisted {
            self.evict();
            return Err(error);
        }
        self.persisted_state_epoch
            .fetch_max(epoch, Ordering::SeqCst);
        Ok(true)
    }

    /// Write the state if the sidecar lags the room (no-op otherwise).
    pub(crate) async fn flush_state_if_dirty(
        &self,
        force_dir_sync: bool,
    ) -> Result<bool, PersistenceError> {
        if !self.state_is_dirty() || self.is_evicted() {
            return Ok(false);
        }
        self.persist_state(true, force_dir_sync).await
    }

    /// [`Self::flush_state_if_dirty`] for a caller that holds no graph lease
    /// (a socket closing, the shutdown flush): the write happens under the
    /// graph's shared lifecycle lease, so deletion cannot purge the directory
    /// and have this write recreate the sidecar behind it.
    pub(crate) async fn flush_state_under_lifecycle(
        &self,
        force_dir_sync: bool,
    ) -> Result<bool, PersistenceError> {
        if !self.state_is_dirty() || self.is_evicted() {
            return Ok(false);
        }
        let _lifecycle = match self.persist_scope.get() {
            Some(scope) => match scope
                .app
                .try_state::<super::persistence_coordinator::GraphPersistenceCoordinator>(
            ) {
                Some(coordinator) => Some(
                    coordinator
                        .acquire_lifecycle_shared(&scope.graph_id)
                        .await
                        .map_err(PersistenceError::permanent)?,
                ),
                None => None,
            },
            None => None,
        };
        self.flush_state_if_dirty(force_dir_sync).await
    }

    /// [`Self::flush_state_if_dirty`] for synchronous callers that already hold
    /// the graph's lease (restore-point capture). Polls the room's async locks
    /// instead of nesting a runtime; the holders it waits for (a group commit,
    /// one frame's apply) are milliseconds.
    pub(crate) fn flush_state_blocking(&self) -> Result<(), PersistenceError> {
        let started = Instant::now();
        loop {
            if !self.state_is_dirty() || self.is_evicted() {
                return Ok(());
            }
            if let Ok(_gate) = self.persist_gate.try_lock() {
                // Re-check under the gate: a group commit may just have run.
                if !self.state_is_dirty() {
                    return Ok(());
                }
                let snapshot = self.doc.try_lock().map(|doc| {
                    let epoch = self.state_epoch.load(Ordering::SeqCst);
                    let bytes = doc
                        .transact()
                        .encode_state_as_update_v1(&yrs::StateVector::default());
                    (bytes, epoch)
                });
                if let Ok((bytes, epoch)) = snapshot {
                    let existed = self.state_path.exists();
                    let sync_dir = self.dir_sync.due_at(Instant::now(), existed, true);
                    if let Err(error) =
                        Room::persist_state_bytes_with(&self.state_path, &bytes, sync_dir)
                    {
                        self.evict();
                        return Err(error);
                    }
                    self.persisted_state_epoch
                        .fetch_max(epoch, Ordering::SeqCst);
                    return Ok(());
                }
            }
            if started.elapsed() > Duration::from_secs(10) {
                return Err(PersistenceError::transient(
                    "timed out waiting for the room to flush its state",
                ));
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// Let the websocket host defer this room's sidecar writes to the group
    /// commit, under `graph_id`'s lifecycle lease.
    pub(super) fn bind_persist_scope(&self, app: &AppHandle, graph_id: &str) {
        let _ = self.persist_scope.set(PersistScope {
            app: app.clone(),
            graph_id: graph_id.to_string(),
        });
    }

    /// Make sure a group commit is coming for the change just broadcast. At
    /// most one task per room is waiting at a time; it clears `persist_armed`
    /// *before* it encodes, so a change that lands after that arms the next
    /// task and one that landed before is in this commit: no change is left
    /// without a pending commit. The task holds a background lifecycle lease
    /// so idle drain and shutdown wait for it.
    fn arm_state_persist(self: &Arc<Self>) {
        if self.persist_armed.swap(true, Ordering::SeqCst) {
            return;
        }
        #[cfg(test)]
        let lifecycle_lease: Option<crate::cell_lifecycle::BackgroundLease> = None;
        #[cfg(not(test))]
        let lifecycle_lease = crate::cell_lifecycle::process_lifecycle()
            .and_then(|lifecycle| lifecycle.begin_background("room-state-persist").ok());
        let room = self.clone();
        crate::app_runtime::async_runtime::spawn(
            async move {
                let _lifecycle_lease = lifecycle_lease;
                tokio::time::sleep(ROOM_PERSIST_WINDOW).await;
                room.persist_armed.store(false, Ordering::SeqCst);
                if let Err(error) = room.flush_state_under_lifecycle(false).await {
                    log::warn!(
                        "room state commit failed for {} (room evicted; clients re-push on reconnect): {error}",
                        room.state_path.display()
                    );
                }
            }
            .with_current_subscriber(),
        );
    }

    #[cfg(test)]
    fn persist_state_bytes_at(
        state_path: &std::path::Path,
        bytes: &[u8],
    ) -> Result<(), PersistenceError> {
        Room::persist_state_bytes_with(state_path, bytes, true)
    }

    #[tracing::instrument(target = "gardend_otel", name = "ydoc.state.write", skip_all,
        fields(sophia.ydoc.state_bytes = bytes.len() as u64, sophia.fsync.dir = sync_dir))]
    fn persist_state_bytes_with(
        state_path: &std::path::Path,
        bytes: &[u8],
        sync_dir: bool,
    ) -> Result<(), PersistenceError> {
        crate::document_body_availability::require_state_path_available(state_path)
            .map_err(PersistenceError::permanent)?;
        // This synchronous hot-authority commit is a durability transaction.
        // The gate prevents an EFS snapshot from walking the profile while the
        // sidecar's parent/temp/final transition is in progress. The guard is
        // re-entrant for callers that already own a broader synchronous commit.
        let _durability_guard = crate::cell_durability::write_guard();
        if let Some(parent) = state_path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| {
                PersistenceError::io(error, format!("room persist mkdir {}", parent.display()))
            })?;
        }
        let temp_path = atomic_temp_path(state_path);
        // Room state is authority, persisted on the group-commit window: ordered (a barrier on macOS), with a
        // full flush owed within the durability window. Shutdown and restore capture flush it fully.
        write_bytes_atomic_durable(
            state_path,
            &temp_path,
            bytes,
            None,
            sync_dir,
            Durability::Ordered,
        )
        .map_err(|error| error.with_context(format!("room persist {}", state_path.display())))
    }

    /// Make the sidecar's directory entry durable if a rename since the last
    /// directory sync skipped it. Called when a connection closes.
    async fn settle_state_dir(&self) {
        if !self.dir_sync.pending.load(Ordering::SeqCst) {
            return;
        }
        let Some(parent) = self.state_path.parent().map(std::path::Path::to_path_buf) else {
            return;
        };
        let cadence = self.dir_sync.clone();
        let settled = crate::app_runtime::async_runtime::spawn_blocking(
            crate::telemetry::blocking(move || {
                let _durability_guard = crate::cell_durability::write_guard();
                let result = sync_parent_dir(&parent);
                if result.is_ok() {
                    cadence.settled_at(Instant::now());
                }
                result
            }),
        )
        .await;
        match settled {
            Ok(Ok(())) => {}
            Ok(Err(error)) => log::warn!(
                "room sidecar directory sync failed for {}: {}",
                self.state_path.display(),
                String::from(error)
            ),
            Err(error) => log::warn!("room sidecar directory sync join: {error}"),
        }
    }

    #[cfg(test)]
    pub(crate) async fn encode_state_for_test(&self) -> Vec<u8> {
        let doc = self.doc.lock().await;
        let encoded = doc
            .transact()
            .encode_state_as_update_v1(&yrs::StateVector::default());
        encoded
    }

    #[cfg(test)]
    pub(crate) fn persist_state_bytes_for_test(&self, bytes: &[u8]) -> Result<(), String> {
        Room::persist_state_bytes_at(&self.state_path, bytes).map_err(Into::into)
    }

    #[cfg(test)]
    pub(crate) fn subscribe_updates_for_test(&self) -> broadcast::Receiver<Vec<u8>> {
        self.tx.subscribe()
    }
}

/// The Y.Doc side of the [`Convergent`] seam: an adapter over the room's existing methods, so a
/// generic loop (and any in-process caller) can drive a Y.Doc room exactly as it drives a ledger.
///
/// * `version` is the encoded state vector, `diff` is `encode_state_as_update_v1(since)` as one
///   payload (a Y.Doc update is not splittable, so `max_bytes` does not apply).
/// * `apply` is [`Room::apply_client_frame`] on a room the websocket host bound (see
///   `bind_persist_scope`): the `ydoc.update.apply` span, the broadcast, the epoch bumps, the evict
///   on a failed integrate, the sidecar write deferred to the group commit and the cold projection
///   scheduled, as the websocket loop did. On any other room it is [`Room::apply_client_update_inner`]
///   and the update is on disk before `apply` returns.
/// * `persist` is the sidecar write if the room is dirty; `durable` is a no-op (the sidecar write is
///   its own durability point and shutdown ends with a full flush); `shippable` is empty (the
///   update was broadcast when it was applied).
impl Convergent for Room {
    fn kind(&self) -> RoomKind {
        RoomKind::YDoc
    }

    async fn version(&self) -> Vec<u8> {
        let doc = self.doc.lock().await;
        let state_vector = doc.transact().state_vector();
        state_vector.encode_v1()
    }

    async fn diff(&self, since: &[u8], _max_bytes: usize) -> Result<Vec<Vec<u8>>, ConvergeError> {
        let state_vector = yrs::StateVector::decode_v1(since)
            .map_err(|error| ConvergeError::Decode(format!("state vector: {error}")))?;
        let doc = self.doc.lock().await;
        let update = doc.transact().encode_state_as_update_v1(&state_vector);
        Ok(vec![update])
    }

    async fn apply(&self, payload: &[u8], from: ConnectionId) -> Result<Applied, ConvergeError> {
        let map_error = |error: String| {
            if error.starts_with("decode client ydoc update") {
                ConvergeError::Decode(error)
            } else {
                ConvergeError::Refused(error)
            }
        };
        // A room the websocket host bound goes through `apply_client_frame`, as `serve_room_with`
        // did: the `ydoc.update.apply` span (update_bytes, mutex_wait_ms, changed, status), the
        // deferred sidecar write with its armed group commit, and the cold-projection schedule.
        let changed = match (self.me.upgrade(), self.persist_scope.get()) {
            (Some(room), Some(scope)) => room
                .apply_client_frame(payload, scope.app.clone(), Some(from))
                .await
                .map_err(map_error)?,
            // No host bound: the update is on disk before `apply` returns, and nothing is scheduled.
            _ => self
                .apply_client_update_inner(payload, Some(from), false)
                .await
                .map_err(map_error)?,
        };
        Ok(Applied {
            changed,
            reply: None,
        })
    }

    async fn persist(&self) -> Result<(), PersistenceError> {
        self.flush_state_if_dirty(false).await.map(|_| ())
    }

    async fn durable(&self) -> Result<(), PersistenceError> {
        Ok(())
    }

    async fn shippable(&self) -> Vec<Vec<u8>> {
        Vec::new()
    }

    fn is_evicted(&self) -> bool {
        Room::is_evicted(self)
    }

    fn awareness(&self) -> &AwarenessTable {
        &self.awareness
    }

    fn frames(&self) -> &broadcast::Sender<RoomFrame> {
        &self.frames
    }
}

impl ProjectionFlushTarget {
    /// Note one update. Returns true when the caller must start the timer
    /// task: only the first update of a burst does, later ones just move
    /// `last`.
    fn register_update(&self, now: Instant) -> bool {
        let mut timer = self.timer.lock().unwrap_or_else(|error| error.into_inner());
        timer.first.get_or_insert(now);
        timer.last = Some(now);
        !std::mem::replace(&mut timer.armed, true)
    }

    fn schedule(self: &Arc<Self>, room: Weak<Room>, app: AppHandle, graph_generation: u64) {
        if crate::telemetry::enabled() {
            let mut contexts = self
                .contexts
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            contexts.push(crate::telemetry::TraceContext::capture());
        }
        self.generation.fetch_add(1, Ordering::SeqCst);
        self.graph_generation
            .store(graph_generation, Ordering::SeqCst);
        if !self.register_update(Instant::now()) {
            return;
        }
        let target = Arc::downgrade(self);
        // A socket update can outlive the socket itself while it waits through
        // the projection debounce. Publish that pending work to the process
        // lifecycle before spawning so idle drain cannot overtake it.
        #[cfg(test)]
        let lifecycle_lease: Option<crate::cell_lifecycle::BackgroundLease> = None;
        #[cfg(not(test))]
        let lifecycle_lease = crate::cell_lifecycle::process_lifecycle()
            .and_then(|lifecycle| lifecycle.begin_background("projection-flush").ok());
        crate::app_runtime::async_runtime::spawn(
            async move {
                let _lifecycle_lease = lifecycle_lease;
                run_debounced_projection(target, room, app, PROJECTION_FLUSH_ATTEMPT_TIMEOUT).await;
            }
            .with_current_subscriber(),
        );
    }
}

/// The room's single timer task: wait for the burst to go quiet (or hit the
/// max wait), flush against the generation as it stands, and go again if
/// updates arrived while the flush ran. It exits, disarmed, when nothing is
/// pending, and silently when the room or its target is gone or evicted.
async fn run_debounced_projection(
    target: Weak<ProjectionFlushTarget>,
    room: Weak<Room>,
    app: AppHandle,
    attempt_timeout: Duration,
) {
    loop {
        let Some(target_ref) = target.upgrade() else {
            return;
        };
        let wait = {
            let mut timer = target_ref
                .timer
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            let (Some(first), Some(last)) = (timer.first, timer.last) else {
                timer.armed = false;
                return;
            };
            debounce_remaining(
                first,
                last,
                Instant::now(),
                target_ref.debounce,
                PROJECTION_DEBOUNCE_MAX_WAIT,
            )
        };
        drop(target_ref);
        if !wait.is_zero() {
            if wait_for_room_delay(&room, wait).await {
                return;
            }
            continue;
        }
        let Some(target_ref) = target.upgrade() else {
            return;
        };
        let (generation, graph_generation) = {
            let mut timer = target_ref
                .timer
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            timer.first = None;
            timer.last = None;
            (
                target_ref.generation.load(Ordering::SeqCst),
                target_ref.graph_generation.load(Ordering::SeqCst),
            )
        };
        drop(target_ref);
        // This task intentionally retains AppHandle while an older graph
        // operation owns the persistence lease. It exits once its flush is
        // queued, superseded, or evicted; runtime shutdown aborts spawned
        // tasks. A fixed total deadline would lose dirty state behind a
        // legitimate long-running import.
        run_projection_flush(
            target.clone(),
            room.clone(),
            app.clone(),
            generation,
            graph_generation,
            attempt_timeout,
        )
        .await;
    }
}

enum ProjectionAttempt<T> {
    Completed(T),
    Evicted,
    TimedOut,
}

async fn wait_for_room_delay(room: &Weak<Room>, delay: Duration) -> bool {
    let Some(room) = room.upgrade() else {
        return true;
    };
    tokio::select! {
        _ = tokio::time::sleep(delay) => false,
        _ = room.wait_until_evicted() => true,
    }
}

async fn wait_for_projection_attempt<F, T>(
    room: &Weak<Room>,
    future: Pin<&mut F>,
    timeout: Duration,
) -> ProjectionAttempt<T>
where
    F: Future<Output = T>,
{
    let Some(room) = room.upgrade() else {
        return ProjectionAttempt::Evicted;
    };
    tokio::select! {
        output = future => ProjectionAttempt::Completed(output),
        _ = room.wait_until_evicted() => ProjectionAttempt::Evicted,
        _ = tokio::time::sleep(timeout) => ProjectionAttempt::TimedOut,
    }
}

async fn run_projection_flush(
    target: Weak<ProjectionFlushTarget>,
    room: Weak<Room>,
    app: AppHandle,
    generation: u64,
    graph_generation: u64,
    attempt_timeout: Duration,
) {
    let Some(target_ref) = target.upgrade() else {
        return;
    };
    let span = if crate::telemetry::enabled() {
        let mut contexts = target_ref
            .contexts
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if target_ref.generation.load(Ordering::SeqCst) != generation {
            return;
        }
        let cohort = std::mem::take(&mut *contexts);
        crate::telemetry::linked_flush_span(
            &target_ref.graph_id,
            target_ref.document_id.as_deref(),
            generation,
            target_ref.debounce,
            cohort.updates,
            cohort.dropped,
        )
    } else {
        tracing::Span::none()
    };
    drop(target_ref);
    run_projection_flush_inner(
        target,
        room,
        app,
        generation,
        graph_generation,
        attempt_timeout,
    )
    .instrument(span)
    .await;
}

async fn run_projection_flush_inner(
    target: Weak<ProjectionFlushTarget>,
    room: Weak<Room>,
    app: AppHandle,
    generation: u64,
    graph_generation: u64,
    attempt_timeout: Duration,
) {
    let mut retry_delay = PROJECTION_RETRY_INITIAL;
    let mut attempt = 0_u64;
    'retry: loop {
        let Some(room_ref) = room.upgrade() else {
            return;
        };
        if room_ref.is_evicted() || !room_ref.needs_projection_flush() {
            return;
        }
        drop(room_ref);
        let Some(target_ref) = target.upgrade() else {
            // The room/app was torn down while the debounce was pending.
            return;
        };
        if target_ref.generation.load(Ordering::SeqCst) != generation {
            // A newer update owns the debounce. This check runs before each
            // bounded enqueue/wait chunk.
            return;
        }
        let graph_id = target_ref.graph_id.clone();
        let document_id = target_ref.document_id.clone();
        drop(target_ref);

        if let Some(document_id) = document_id.as_deref() {
            let Some(room_ref) = room.upgrade() else {
                return;
            };
            // Document projections must not re-enter the mutation FIFO: a
            // slow RDF reconcile would otherwise stall subsequent hot writes.
            let outcome = super::flush_ops::flush_document_background(
                app.clone(),
                graph_id.clone(),
                document_id.to_string(),
                room_ref,
                graph_generation,
            )
            .await;
            attempt += 1;
            tracing::Span::current().record("sophia.projection.attempts", attempt);
            match outcome {
                Ok(()) => return,
                Err(error) => {
                    let error = super::flush_ops::classify_flush_error(error);
                    if matches!(&error, super::executor::ApplyOperationError::Terminal(_))
                        || attempt > u64::from(super::executor::max_retryable_attempts())
                    {
                        log::warn!("automatic document projection stopped for {graph_id}/{document_id}: {error}");
                        return;
                    }
                    log::warn!("automatic document projection failed for {graph_id}/{document_id} (attempt {attempt}): {error}");
                    if wait_for_room_delay(&room, retry_delay).await {
                        return;
                    }
                    retry_delay = retry_delay.saturating_mul(2).min(PROJECTION_RETRY_MAX);
                    continue;
                }
            }
        }

        let queued = Arc::new(AtomicBool::new(false));
        let flush = async {
            match document_id.as_deref() {
                Some(document_id) => {
                    crate::crdt_projection_flush::flush_document_projection_generation(
                        app.clone(),
                        &graph_id,
                        document_id,
                        graph_generation,
                        queued.clone(),
                    )
                    .await
                }
                None => {
                    crate::crdt_projection_flush::flush_graph_projection_generation(
                        app.clone(),
                        &graph_id,
                        graph_generation,
                        queued.clone(),
                    )
                    .await
                }
            }
        };
        tokio::pin!(flush);
        let outcome = loop {
            match wait_for_projection_attempt(&room, flush.as_mut(), attempt_timeout).await {
                ProjectionAttempt::Completed(outcome) => break outcome,
                ProjectionAttempt::Evicted => return,
                ProjectionAttempt::TimedOut if queued.load(Ordering::Acquire) => {
                    // Queue insertion is durable and cancellation is no longer
                    // safe, but dropping outcome observation would also lose
                    // retryable failures. Keep polling this exact future in
                    // bounded chunks until its executor result is known.
                    log::debug!(
                        "automatic projection flush is queued and still running for {graph_id}/{}",
                        document_id.as_deref().unwrap_or("workspace"),
                    );
                }
                ProjectionAttempt::TimedOut => {
                    // The future timed out while awaiting its enqueue graph
                    // lease. It was dropped before journal/queue insertion, so
                    // retrying cannot duplicate an operation. Lease contention
                    // consumes neither the retry budget nor a terminal
                    // wall-clock deadline.
                    log::debug!(
                        "automatic projection flush is still waiting to enqueue for {graph_id}/{}",
                        document_id.as_deref().unwrap_or("workspace"),
                    );
                    if wait_for_room_delay(&room, PROJECTION_RETRY_INITIAL).await {
                        return;
                    }
                    continue 'retry;
                }
            }
        };
        attempt += 1;
        tracing::Span::current().record("sophia.projection.attempts", attempt);
        match outcome {
            Ok(()) => return,
            Err(error) => {
                let error = match classify_projection_flush_failure(error) {
                    ProjectionFlushFailure::Terminal(error) => {
                        log::warn!(
                            "automatic projection flush stopped for {graph_id}/{}: {error}",
                            document_id.as_deref().unwrap_or("workspace"),
                        );
                        return;
                    }
                    ProjectionFlushFailure::Retryable(error) => error,
                };
                if queued.load(Ordering::Acquire) {
                    // Once queue insertion is durable, the executor's
                    // per-graph scheduled retry owns this exact journal ID
                    // indefinitely. Never manufacture a fresh flush op after
                    // a caller timeout/channel error.
                    log::warn!(
                        "automatic projection flush remains durably queued for {graph_id}/{} after observer error: {error}",
                        document_id.as_deref().unwrap_or("workspace"),
                    );
                    return;
                }
                log::warn!(
                    "automatic projection flush failed for {graph_id}/{} (attempt {attempt}; retrying in {}ms): {error}",
                    document_id.as_deref().unwrap_or("workspace"),
                    retry_delay.as_millis(),
                );
                if wait_for_room_delay(&room, retry_delay).await {
                    return;
                }
                retry_delay = retry_delay.saturating_mul(2).min(PROJECTION_RETRY_MAX);
            }
        }
    }
}

#[cfg(all(test, not(feature = "desktop")))]
pub(super) async fn run_projection_flush_for_test(
    room: Arc<Room>,
    app: AppHandle,
    graph_generation: u64,
    attempt_timeout: Duration,
) {
    let target = room
        .projection_flush
        .get()
        .expect("test room projection target is configured")
        .clone();
    let generation = target
        .generation
        .fetch_add(1, Ordering::SeqCst)
        .wrapping_add(1);
    run_projection_flush(
        Arc::downgrade(&target),
        Arc::downgrade(&room),
        app,
        generation,
        graph_generation,
        attempt_timeout,
    )
    .await;
}

const WS_STATE_SEND_TIMEOUT: Duration = Duration::from_secs(5);

/// A room that can say whether graph deletion has evicted it. The delivery helpers below check
/// this under the graph's shared lifecycle lease. Both kinds of room carry the flag (a ledger
/// room's is [`LedgerRoom::evict`]'s); the generic loop passes [`RoomFlag`], which asks the room.
pub(crate) trait Evictable {
    /// Whether the room has been evicted.
    fn is_evicted(&self) -> bool;
}

impl Evictable for Room {
    fn is_evicted(&self) -> bool {
        self.evicted.load(Ordering::SeqCst)
    }
}

impl<T: Evictable + ?Sized> Evictable for Arc<T> {
    fn is_evicted(&self) -> bool {
        (**self).is_evicted()
    }
}

impl<T: Evictable + ?Sized> Evictable for &T {
    fn is_evicted(&self) -> bool {
        (**self).is_evicted()
    }
}

/// The eviction flag of whatever room a generic loop serves.
struct RoomFlag<C: Convergent>(Arc<C>);

impl<C: Convergent> Evictable for RoomFlag<C> {
    fn is_evicted(&self) -> bool {
        Convergent::is_evicted(&*self.0)
    }
}

/// Hold graph lifecycle authority from before a state-bearing frame is built
/// until the socket sink has accepted it. The delivery future is created by
/// the caller but is first polled here, after the lease and active-graph/room
/// checks. Timing it out cancels the sink send in place; there is no queued
/// frame that could leak after deletion advances the graph lifecycle.
#[tracing::instrument(target = "gardend_otel", name = "ydoc.broadcast.deliver", skip_all,
    fields(sophia.graph.id = graph_id, sophia.lease.wait_ms = tracing::field::Empty,
        sophia.delivery.outcome = tracing::field::Empty,
        otel.status_code = tracing::field::Empty))]
async fn linearize_state_bearing_delivery<F>(
    app: &AppHandle,
    graph_id: &str,
    room: &(impl Evictable + ?Sized),
    delivery: F,
) -> Result<(), String>
where
    F: std::future::Future<Output = Result<(), String>>,
{
    let result = linearize_state_bearing_delivery_inner(app, graph_id, room, true, delivery).await;
    if result.is_err() {
        tracing::Span::current().record("otel.status_code", "ERROR");
    }
    result
}

/// Delivery of a broadcast update to one socket. The shared lifecycle lease
/// still orders it against graph deletion, but the `graph.json` read is gone:
/// deletion evicts the graph's rooms while it holds the exclusive lease, so
/// `room.is_evicted()` under the shared lease already says "deleted", and the
/// read-and-parse of the record per frame per socket told it nothing more.
#[tracing::instrument(target = "gardend_otel", name = "ydoc.broadcast.deliver", skip_all,
    fields(sophia.graph.id = graph_id, sophia.lease.wait_ms = tracing::field::Empty,
        sophia.delivery.outcome = tracing::field::Empty,
        otel.status_code = tracing::field::Empty))]
async fn broadcast_state_delivery<F>(
    app: &AppHandle,
    graph_id: &str,
    room: &(impl Evictable + ?Sized),
    delivery: F,
) -> Result<(), String>
where
    F: std::future::Future<Output = Result<(), String>>,
{
    let result = linearize_state_bearing_delivery_inner(app, graph_id, room, false, delivery).await;
    if result.is_err() {
        tracing::Span::current().record("otel.status_code", "ERROR");
    }
    result
}

async fn linearize_state_bearing_delivery_inner<F>(
    app: &AppHandle,
    graph_id: &str,
    room: &(impl Evictable + ?Sized),
    check_graph_record: bool,
    delivery: F,
) -> Result<(), String>
where
    F: std::future::Future<Output = Result<(), String>>,
{
    let coordinator = app
        .try_state::<super::persistence_coordinator::GraphPersistenceCoordinator>()
        .ok_or_else(|| "graph persistence coordinator is unavailable".to_string())?;
    // Bounded, not un-timed: an un-timed acquire here head-of-line-blocks
    // every doc-open/SyncStep2 behind whatever else currently holds the
    // graph lease (observed live: a persist() stalled behind the durable
    // flush's gate for ~9s — see
    // plans/canary-doc-open-ws-diagnosis-20260715.md THIRD LAYER). Failing
    // fast here lets the client's own reconnect retry instead of parking
    // the connection un-timed. Nothing has been sent to the socket yet at
    // this point, so a timeout here cannot drop or double-send a frame.
    let waiting = crate::telemetry::timer();
    let acquired = tokio::time::timeout(
        WS_STATE_SEND_TIMEOUT,
        coordinator.acquire_lifecycle_shared(graph_id),
    )
    .await;
    let wait_ms = crate::telemetry::elapsed_ms(waiting);
    tracing::Span::current().record("sophia.lease.wait_ms", wait_ms);
    let _lease = match acquired {
        Ok(Ok(lease)) => lease,
        Ok(Err(error)) => {
            tracing::Span::current().record("sophia.delivery.outcome", "lease_error");
            return Err(error);
        }
        Err(_) => {
            tracing::Span::current().record("sophia.delivery.outcome", "lease_timeout");
            return Err("graph persistence lease acquire timed out".to_string());
        }
    };
    if check_graph_record {
        if let Err(error) = crate::graph_record_store::read_graph_record_no_heal(app, graph_id) {
            tracing::Span::current().record("sophia.delivery.outcome", "graph_gone");
            return Err(error.message());
        }
    }
    if room.is_evicted() {
        tracing::Span::current().record("sophia.delivery.outcome", "room_evicted");
        return Err("room was evicted".to_string());
    }
    let result = tokio::time::timeout(WS_STATE_SEND_TIMEOUT, delivery).await;
    let outcome = match &result {
        Ok(Ok(())) => "sent",
        Ok(Err(_)) => "peer_gone",
        Err(_) => "delivery_timeout",
    };
    tracing::Span::current().record("sophia.delivery.outcome", outcome);
    result.map_err(|_| "websocket state delivery timed out".to_string())?
}

#[tracing::instrument(target = "gardend_otel", name = "room.initial_sync", skip_all,
    fields(sophia.graph.id = graph_id, sophia.lease.wait_ms = tracing::field::Empty,
        sophia.delivery.outcome = tracing::field::Empty,
        otel.status_code = tracing::field::Empty))]
pub(super) async fn initial_sync_delivery<F>(
    app: &AppHandle,
    graph_id: &str,
    room: &(impl Evictable + ?Sized),
    delivery: F,
) -> Result<(), String>
where
    F: std::future::Future<Output = Result<(), String>>,
{
    let result = linearize_state_bearing_delivery_inner(app, graph_id, room, true, delivery).await;
    if result.is_err() {
        tracing::Span::current().record("otel.status_code", "ERROR");
    }
    result
}

/// What the other end of a room connection may send (W6). Every caller the
/// rooms admitted before public mode is `ReadWrite`, exactly as before.
#[derive(Debug, Clone)]
pub(crate) enum RoomAccess {
    /// A credential holding the room's write scope.
    ReadWrite,
    /// A public-mode visitor: reads the room; a frame that would change it
    /// closes the connection (4403). `account`: a signed-in account's id, so
    /// its presence is stamped; an anonymous visitor publishes none.
    ReadOnly { account: Option<String> },
    /// A signed-in account with a fence: its frames land only while the
    /// document is its own and inside the fence, and pass the content policy.
    Fenced {
        fence: crate::visitor_fence::WriteFence,
        document_id: String,
    },
    /// A hosted cell's Viewer (a gateway lease below Editor): reads the room
    /// as an Editor sees it; a frame that would change it closes the
    /// connection (4403), as for `ReadOnly`; its presence is stamped with the
    /// lease `principal`.
    Viewer { principal: String },
}

/// The close code a refused visitor frame gets (application range).
pub(crate) const VISITOR_REFUSED_CLOSE: u16 = 4403;

/// Counts one socket in a room. When the last one goes away the room schedules
/// its projection repair: a room hydrated from its sidecar is marked dirty in
/// case a prior exit left the cold files stale, and that repair now runs when
/// nobody is watching instead of when the first client opens the document.
/// A no-op when the room is clean or has no projection target.
struct ConnectionGuard {
    room: Arc<Room>,
    app: AppHandle,
}

impl ConnectionGuard {
    fn new(room: &Arc<Room>, app: &AppHandle) -> Self {
        room.connections.fetch_add(1, Ordering::SeqCst);
        Self {
            room: room.clone(),
            app: app.clone(),
        }
    }
}

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        if self.room.connections.fetch_sub(1, Ordering::SeqCst) == 1 {
            self.room.schedule_projection_flush(self.app.clone());
        }
    }
}

/// Drive one client connection through the room (y-websocket protocol).
pub async fn serve_room(socket: WebSocket, room: Arc<Room>, app: AppHandle, graph_id: String) {
    serve_room_with(socket, room, app, graph_id, RoomAccess::ReadWrite).await
}

/// [`serve_room`] for a connection with the given [`RoomAccess`].
///
/// No route calls this any more: they serve through [`serve_convergent`]. It stays because the
/// room, fence and otel tests drive the Y.Doc loop through it directly, and so keeps a second,
/// independent implementation of the same behaviour for them to compare against.
#[tracing::instrument(target = "gardend_otel", name = "room.connection", skip_all,
    fields(sophia.graph.id = %graph_id))]
pub(crate) async fn serve_room_with(
    socket: WebSocket,
    room: Arc<Room>,
    app: AppHandle,
    graph_id: String,
    access: RoomAccess,
) {
    let connection_id = room.allocate_connection_id();
    let _connection = ConnectionGuard::new(&room, &app);
    // From here the room's websocket frames are broadcast first and made
    // durable by the group commit (within ROOM_PERSIST_WINDOW).
    room.bind_persist_scope(&app, &graph_id);
    // W6: a visitor's frames that change nothing still cost a copy of the room;
    // a connection gets a few, then it is closed.
    let mut visitor_noop_frames: u32 = 0;
    const VISITOR_NOOP_FRAMES: u32 = 16;
    let (mut sink, mut stream) = socket.split();
    let mut rx = room.frames.subscribe();

    // Server-initiated SyncStep1 is graph content: deletion must either wait
    // until the socket accepted it or win the lease and evict the room first.
    if let Err(error) = initial_sync_delivery(&app, &graph_id, &room, async {
        let doc = room.doc.lock().await;
        let sv = doc.transact().state_vector();
        let step1 = Message::Sync(SyncMessage::SyncStep1(sv)).encode_v1();
        drop(doc);
        sink.send(WsMessage::binary(step1))
            .await
            .map_err(|error| format!("send websocket SyncStep1: {error}"))
    })
    .await
    {
        log::warn!("closing websocket initial sync for {graph_id}: {error}");
        return;
    }
    if let Some(update) = room.full_awareness().await {
        let msg = Message::Awareness(update).encode_v1();
        let result =
            tokio::time::timeout(WS_STATE_SEND_TIMEOUT, sink.send(WsMessage::binary(msg))).await;
        if !matches!(result, Ok(Ok(()))) {
            return;
        }
    }

    loop {
        tokio::select! {
            _ = room.wait_until_evicted() => break,
            frame = stream.next() => {
                let Some(Ok(frame)) = frame else {
                    break;
                };
                let data = match frame {
                    WsMessage::Binary(data) => data,
                    // Complete the RFC 6455 close handshake. Dropping the sink
                    // without echoing the peer's close makes real browsers
                    // report an abnormal 1006 even though the room exited.
                    WsMessage::Close(_) => {
                        // Tungstenite queues the symmetric reply while reading
                        // the peer close. Drive that queued frame to the wire;
                        // trying to enqueue a second close is rejected because
                        // the protocol state is already ClosedByPeer.
                        if let Err(error) = sink.flush().await {
                            log::warn!("flush websocket close reply for {graph_id}: {error}");
                        }
                        break;
                    }
                    _ => continue,
                };
                let Ok(message) = Message::decode_v1(&data) else {
                    continue;
                };
                match message {
                    Message::Sync(SyncMessage::SyncStep1(sv)) => {
                        let result = linearize_state_bearing_delivery(
                            &app,
                            &graph_id,
                            &room,
                            async {
                                let doc = room.doc.lock().await;
                                let update = doc.transact().encode_state_as_update_v1(&sv);
                                drop(doc);
                                let reply = Message::Sync(SyncMessage::SyncStep2(update)).encode_v1();
                                sink.send(WsMessage::binary(reply))
                                    .await
                                    .map_err(|error| format!("send websocket SyncStep2: {error}"))
                            },
                        )
                        .await;
                        if let Err(error) = result {
                            log::warn!("closing websocket sync reply for {graph_id}: {error}");
                            break;
                        }
                    }
                    Message::Sync(SyncMessage::SyncStep2(update))
                    | Message::Sync(SyncMessage::Update(update)) => {
                        // W6: what a visitor may send.
                        // An empty update (a fresh client's SyncStep2) costs nothing.
                        let empty = !matches!(access, RoomAccess::ReadWrite)
                            && Update::decode_v1(&update).is_ok_and(|decoded| decoded.is_empty());
                        if empty {
                            continue;
                        }
                        let refused = match &access {
                            RoomAccess::ReadWrite => None,
                            RoomAccess::ReadOnly { .. } | RoomAccess::Viewer { .. } => match room.shadow_apply(&update, |_, _, _| Ok(())).await {
                                Ok(false) => {
                                    visitor_noop_frames += 1;
                                    if visitor_noop_frames <= VISITOR_NOOP_FRAMES {
                                        continue;
                                    }
                                    Some("too many frames that change nothing".to_string())
                                }
                                Ok(true) if matches!(access, RoomAccess::Viewer { .. }) => {
                                    Some("read-only: viewers read this room".to_string())
                                }
                                Ok(true) => Some("read-only: visitors read this room".to_string()),
                                Err(error) => Some(error),
                            },
                            RoomAccess::Fenced { fence, document_id } => {
                                match crate::visitor_fence::admit_room_update(
                                    &app, &graph_id, fence, document_id, &room, &update,
                                )
                                .await
                                {
                                    Ok(true) => None,
                                    Ok(false) => {
                                        visitor_noop_frames += 1;
                                        if visitor_noop_frames <= VISITOR_NOOP_FRAMES {
                                            continue;
                                        }
                                        Some("too many frames that change nothing".to_string())
                                    }
                                    Err(error) => Some(error),
                                }
                            }
                        };
                        if let Some(reason) = refused {
                            log::info!("visitor room frame refused on {graph_id}: {reason}");
                            let reason: String = reason.chars().take(110).collect();
                            let _ = sink
                                .send(WsMessage::Close(Some(axum::extract::ws::CloseFrame {
                                    code: VISITOR_REFUSED_CLOSE,
                                    reason: reason.into(),
                                })))
                                .await;
                            break;
                        }
                        // A frame needs the graph's shared lifecycle lease, so deletion
                        // and the other exclusive roots still order against it, and
                        // nothing more: it never queues behind an API write holding the
                        // hot-write gate (an agent mid-`insert_blocks` used to stall
                        // Vera's typing). Operations that need a quiet room (a forward
                        // restore applies a diff, then verifies the room equals the
                        // target) raise an explicit frame fence, which waits for frames
                        // in flight and holds new ones back. Dirty marking is not this
                        // lease's job: a client frame never touches Oxigraph, and the
                        // sidecar write marks itself through `write_guard`.
                        let _frame_lease = if let Some(coordinator) = app.try_state::<
                            super::persistence_coordinator::GraphPersistenceCoordinator,
                        >() {
                            match coordinator.acquire_client_frame(&graph_id).await {
                                Ok(lease) => Some(lease),
                                Err(error) => {
                                    log::error!(
                                        "websocket frame lease failed for {graph_id}: {error}"
                                    );
                                    continue;
                                }
                            }
                        } else {
                            None
                        };
                        match room.apply_client_frame(&update, app.clone(), Some(connection_id)).await {
                            Ok(_) => {}
                            Err(error) => {
                                // Deletion marks extant room Arcs evicted before it
                                // releases the graph lease. Close instead of silently
                                // accepting and dropping every later client write.
                                log::warn!("closing websocket room for {graph_id}: {error}");
                                break;
                            }
                        }
                    }
                    Message::Awareness(update) => {
                        // W6: a visitor's presence is the account's own (or
                        // none, for an anonymous visitor).
                        let update = match &access {
                            RoomAccess::ReadWrite => update,
                            RoomAccess::ReadOnly { account } => {
                                match crate::visitor_fence::stamp_awareness_update(&app, account.as_deref(), update) {
                                    Some(update) => update,
                                    None => continue,
                                }
                            }
                            RoomAccess::Fenced { fence, .. } => {
                                match crate::visitor_fence::stamp_awareness_update(&app, Some(&fence.account), update) {
                                    Some(update) => update,
                                    None => continue,
                                }
                            }
                            // A Viewer's presence is its lease principal.
                            RoomAccess::Viewer { principal } => {
                                match crate::visitor_fence::stamp_viewer_awareness_update(principal, update) {
                                    Some(update) => update,
                                    None => continue,
                                }
                            }
                        };
                        // A visitor never takes over a client id another live
                        // socket holds (sealed exam 2026-10-15: a higher clock
                        // displaced a collaborator's presence).
                        let update = match &access {
                            RoomAccess::ReadWrite => update,
                            _ => match room.awareness_held_by_this_socket(update, connection_id).await {
                                Some(update) => update,
                                None => continue,
                            },
                        };
                        if let Some((accepted, own_only)) =
                            room.merge_awareness_tagged(&update, connection_id).await
                        {
                            let rebroadcast = Message::Awareness(accepted).encode_v1();
                            room.publish(
                                own_only.then_some(connection_id),
                                FrameKind::Awareness,
                                rebroadcast,
                            );
                        }
                    }
                    Message::AwarenessQuery => {
                        if let Some(update) = room.full_awareness().await {
                            let result = tokio::time::timeout(
                                WS_STATE_SEND_TIMEOUT,
                                sink.send(WsMessage::binary(
                                    Message::Awareness(update).encode_v1(),
                                )),
                            )
                            .await;
                            if !matches!(result, Ok(Ok(()))) {
                                break;
                            }
                        }
                    }
                    Message::Auth(_) | Message::Custom(..) => {}
                }
            }
            broadcasted = rx.recv() => {
                let Ok(frame) = broadcasted else {
                    break;
                };
                // A client that produced the frame already has it.
                if frame.origin == Some(connection_id) {
                    continue;
                }
                let RoomFrame { kind, payload, .. } = frame;
                match kind {
                    // Presence is ephemeral and carries no graph content:
                    // nothing for deletion to order against, so no lease.
                    FrameKind::Awareness => {
                        let result = tokio::time::timeout(
                            WS_STATE_SEND_TIMEOUT,
                            sink.send(WsMessage::binary(payload)),
                        )
                        .await;
                        if !matches!(result, Ok(Ok(()))) {
                            log::warn!("closing websocket awareness broadcast for {graph_id}");
                            break;
                        }
                    }
                    FrameKind::State => {
                        let result = broadcast_state_delivery(
                            &app,
                            &graph_id,
                            &room,
                            async {
                                sink.send(WsMessage::binary(payload))
                                    .await
                                    .map_err(|error| format!("send websocket room update: {error}"))
                            },
                        )
                        .await;
                        if let Err(error) = result {
                            log::warn!("closing websocket broadcast for {graph_id}: {error}");
                            break;
                        }
                    }
                    // Only ledger rooms announce eviction this way; a Y.Doc room's loop
                    // selects on its own flag. Harmless to honour.
                    FrameKind::Evicted => break,
                }
            }
        }
    }

    // The socket is gone: bring the room's unpersisted tail to disk now, with
    // the directory entry, rather than waiting out the window.
    if let Err(error) = room.flush_state_under_lifecycle(true).await {
        log::warn!("room state flush at close failed for {graph_id}: {error}");
    }
    room.settle_state_dir().await;
    if let Some(removal) = room.remove_awareness_for_connection(connection_id).await {
        // Other subscribers receive the synthetic removals even when this
        // transport disappeared without a graceful Awareness null.
        room.publish(
            None,
            FrameKind::Awareness,
            Message::Awareness(removal).encode_v1(),
        );
    }
}

// ---------------------------------------------------------------------------------------------
// The generic room loop (W6 lane R)
// ---------------------------------------------------------------------------------------------

/// Connection ids for [`serve_convergent`] sockets. A Y.Doc room's own counter (used by
/// [`serve_room_with`]) starts at 1; these start high so a room served by both loops can never hand
/// out one id twice.
static NEXT_CONVERGENT_CONNECTION: AtomicU64 = AtomicU64::new(1 << 40);

/// The size each diff payload is cut to: half the websocket's 16 MiB cap.
const CONVERGENT_DIFF_CHUNK_BYTES: usize = 8 * 1024 * 1024;

/// The close code a connection gets when a frame it sent contradicts one we hold (a fork).
pub(crate) const LEDGER_FORK_CLOSE: u16 = 4409;

/// The close code for a payload that names a wire version this cell does not speak
/// (`ledger-vv/N` on SyncStep1, `ledger-bundle/N` on an update). The reason reads
/// `ledger-vv version N unsupported`. (W6.md first drafted 4409 for this; that was already the fork code.)
pub(crate) const LEDGER_VERSION_CLOSE: u16 = 4410;

/// The close code for an update that does not decode; the reason is the decode message.
pub(crate) const LEDGER_DECODE_CLOSE: u16 = 4400;

/// A close frame with `code` and a `reason` cut to the 110 characters a close frame may carry.
fn close_with(code: u16, reason: &str) -> WsMessage {
    WsMessage::Close(Some(axum::extract::ws::CloseFrame {
        code,
        reason: reason.chars().take(110).collect::<String>().into(),
    }))
}

/// y-sync message tag for a sync message.
const MSG_SYNC: u8 = 0;
/// y-sync sync sub-tags.
const SYNC_STEP1: u8 = 0;
const SYNC_STEP2: u8 = 1;
const SYNC_UPDATE: u8 = 2;

/// Append `value` as a lib0 variable-length unsigned integer.
fn write_var_u64(out: &mut Vec<u8>, mut value: u64) {
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

/// Read a lib0 variable-length unsigned integer at `*pos`, advancing it; `None` on a short or
/// over-long encoding.
pub(super) fn read_var_u64(buf: &[u8], pos: &mut usize) -> Option<u64> {
    let mut value: u64 = 0;
    let mut shift = 0u32;
    loop {
        let byte = *buf.get(*pos)?;
        *pos += 1;
        value |= u64::from(byte & 0x7f).checked_shl(shift)?;
        if byte & 0x80 == 0 {
            return Some(value);
        }
        shift += 7;
        if shift > 63 {
            return None;
        }
    }
}

/// One y-sync sync message carrying an opaque payload: `[0][sub][varuint len][payload]`. For a
/// Y.Doc this is byte-for-byte what yrs encodes for `SyncStep1` / `SyncStep2` / `Update`; for a
/// ledger the payload is a `ledger-vv/1` or `ledger-bundle/1` (W6.md section 3).
fn sync_message(sub: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 12);
    out.push(MSG_SYNC);
    out.push(sub);
    write_var_u64(&mut out, payload.len() as u64);
    out.extend_from_slice(payload);
    out
}

/// A decoded inbound websocket message.
enum Inbound {
    /// SyncStep1: the peer's version (state vector or version vector), wire-encoded.
    Step1(Vec<u8>),
    /// SyncStep2: what the peer says we lack.
    Step2(Vec<u8>),
    /// Update: a new change from the peer.
    Update(Vec<u8>),
    /// Presence.
    Awareness(AwarenessUpdate),
    /// A request for the room's full presence.
    AwarenessQuery,
    /// Auth, custom or unknown sub-tags: nothing to do.
    Ignored,
}

/// Decode one binary websocket message. Sync messages are parsed here because their payload is
/// opaque (a ledger version vector is not a yrs `StateVector`); presence and the rest go through
/// yrs. `None` for a message that does not decode, which the loop skips as it always has.
fn decode_inbound(data: &[u8]) -> Option<Inbound> {
    if data.first() == Some(&MSG_SYNC) {
        let mut pos = 1usize;
        let sub = read_var_u64(data, &mut pos)?;
        let len = usize::try_from(read_var_u64(data, &mut pos)?).ok()?;
        let end = pos.checked_add(len)?;
        let payload = data.get(pos..end)?.to_vec();
        return Some(match sub {
            0 => Inbound::Step1(payload),
            1 => Inbound::Step2(payload),
            2 => Inbound::Update(payload),
            _ => Inbound::Ignored,
        });
    }
    Some(match Message::decode_v1(data).ok()? {
        Message::Awareness(update) => Inbound::Awareness(update),
        Message::AwarenessQuery => Inbound::AwarenessQuery,
        _ => Inbound::Ignored,
    })
}

/// `room` as the Y.Doc `Room` it is, when it is one: the generic loop's few Y.Doc-only needs (the
/// persist scope, the connection count, the visitor dry-run, the fence, the eviction flag) go
/// through this.
fn as_ydoc<C: Convergent>(room: &Arc<C>) -> Option<Arc<Room>> {
    let any: Arc<dyn std::any::Any + Send + Sync> = room.clone();
    any.downcast::<Room>().ok()
}

/// Hand one encoded message to every subscriber of the room. A Y.Doc room goes through
/// `Room::publish` (which also feeds the in-process observers tests use).
fn publish_to<C: Convergent>(
    room: &C,
    ydoc: Option<&Arc<Room>>,
    origin: Option<ConnectionId>,
    kind: FrameKind,
    payload: Vec<u8>,
) {
    match ydoc {
        Some(ydoc) => ydoc.publish(origin, kind, payload),
        None => {
            let _ = room.frames().send(RoomFrame {
                origin,
                kind,
                payload,
            });
        }
    }
}

/// A barrier, a full flush, then fan out whatever became shippable as Update messages to every
/// subscriber (no origin: the frames may have come from any peer, and a peer that already holds
/// them applies zero new). This is the ledger's ship step; for a Y.Doc room all three are no-ops
/// or empty. Errors are logged: a failed full flush leaves the room owing it, so the next call
/// retries and nothing ships that is not durable.
pub(crate) async fn ship_pending<C: Convergent>(room: &C) {
    if let Err(error) = room.persist().await {
        log::warn!("room barrier before shipping failed: {error}");
    }
    if let Err(error) = room.durable().await {
        log::warn!("room full flush before shipping failed: {error}");
        return;
    }
    for payload in room.shippable().await {
        let _ = room.frames().send(RoomFrame {
            origin: None,
            kind: FrameKind::State,
            payload: sync_message(SYNC_UPDATE, &payload),
        });
    }
}

/// Whether a peer's sync payload carries nothing: an empty yrs update, or a ledger bundle with no
/// entries. An empty frame costs nothing and is never refused.
fn payload_is_empty(kind: RoomKind, payload: &[u8]) -> bool {
    match kind {
        RoomKind::YDoc => Update::decode_v1(payload).is_ok_and(|decoded| decoded.is_empty()),
        RoomKind::Ledger => {
            super::ledger_room::decode_bundle(payload).is_ok_and(|entries| entries.is_empty())
        }
    }
}

/// Whether applying `payload` would add a frame this ledger lacks: some entry holds a seq past
/// what we have for its origin. (The tip frame every diff repeats does not count.)
async fn ledger_payload_would_change<C: Convergent>(
    room: &C,
    payload: &[u8],
) -> Result<bool, String> {
    let ours =
        super::ledger_room::decode_vv(&room.version().await).map_err(|error| error.to_string())?;
    let entries = super::ledger_room::decode_bundle(payload).map_err(|error| error.to_string())?;
    for (origin, raw) in entries {
        let held = ours.get(&origin);
        let mut offset = 0usize;
        while offset < raw.len() {
            let inspected = garden_ledger::frame::inspect(&raw[offset..])
                .map_err(|error| format!("ledger frame: {error:?}"))?;
            if inspected.header.seq > held {
                return Ok(true);
            }
            offset += inspected.frame_len;
        }
    }
    Ok(false)
}

/// Drive one client connection through a room of any kind (y-websocket protocol).
///
/// This is [`serve_room_with`] with the Y.Doc calls behind [`Convergent`]: the leases (initial sync,
/// every state-bearing delivery, every client frame), the visitor gate with its no-op budget and
/// refusal close, presence, and fan-out are the same code paths; what a state vector, a diff, an
/// update and a flush *are* comes from the room.
///
/// * SyncStep1 from the room is `room.version()`. A peer's SyncStep1 is answered with
///   `room.diff(..)`. For a Y.Doc that one payload is encoded and sent inside a single
///   state-bearing delivery (under the lifecycle lease and the delivery timeout). For a ledger the
///   diff is a list of bounded chunks, one SyncStep2 message each, every chunk delivered under its
///   own lease so a cold sync of a large ledger is bounded per chunk, not per ledger. A ledger
///   SyncStep1 that names an unknown `ledger-vv` version closes with [`LEDGER_VERSION_CLOSE`].
/// * A peer's SyncStep2 / Update goes to `room.apply(..)` under the client-frame lease. For a Y.Doc
///   room that is `apply_client_frame`. For a ledger room a change also arms the room's own
///   persist and ship tasks (see `ledger_room`), which fan the shippable bundles out to every
///   subscriber whether or not this connection sends another frame. A [`ConvergeError::Fork`]
///   closes the socket with [`LEDGER_FORK_CLOSE`], an unsupported bundle version with
///   [`LEDGER_VERSION_CLOSE`], an undecodable ledger update with [`LEDGER_DECODE_CLOSE`], and a
///   refusal (an evicted room) ends the loop.
/// * At close the room is made durable (a Y.Doc room: the sidecar under the lifecycle lease; a
///   ledger room: barrier, full flush, ship) and the socket's presence is removed.
///
/// Both kinds carry an eviction flag (`Convergent::is_evicted`), checked under the shared lease
/// before every delivery; a ledger room also announces its eviction with [`FrameKind::Evicted`],
/// which ends this loop. The doc and workspace routes and the ledger route all serve through here;
/// [`serve_room_with`] stays for the tests that drive it directly.
#[tracing::instrument(target = "gardend_otel", name = "room.connection", skip_all,
    fields(sophia.graph.id = %graph_id))]
pub(crate) async fn serve_convergent<C: Convergent>(
    socket: WebSocket,
    room: Arc<C>,
    app: AppHandle,
    graph_id: String,
    access: RoomAccess,
) {
    let connection_id = NEXT_CONVERGENT_CONNECTION.fetch_add(1, Ordering::Relaxed);
    let kind = room.kind();
    let ydoc = as_ydoc(&room);
    let evictable = RoomFlag(room.clone());
    let _connection = ydoc.as_ref().map(|ydoc| ConnectionGuard::new(ydoc, &app));
    if let Some(ydoc) = ydoc.as_ref() {
        // From here the room's websocket frames are broadcast first and made
        // durable by the group commit (within ROOM_PERSIST_WINDOW).
        ydoc.bind_persist_scope(&app, &graph_id);
    }
    // W6: a visitor's frames that change nothing still cost a look at the room;
    // a connection gets a few, then it is closed.
    let mut visitor_noop_frames: u32 = 0;
    const VISITOR_NOOP_FRAMES: u32 = 16;
    let (mut sink, mut stream) = socket.split();
    let mut rx = room.frames().subscribe();

    // Server-initiated SyncStep1 is graph content: deletion must either wait
    // until the socket accepted it or win the lease and evict the room first.
    if let Err(error) = initial_sync_delivery(&app, &graph_id, &evictable, async {
        let step1 = sync_message(SYNC_STEP1, &room.version().await);
        sink.send(WsMessage::binary(step1))
            .await
            .map_err(|error| format!("send websocket SyncStep1: {error}"))
    })
    .await
    {
        log::warn!("closing websocket initial sync for {graph_id}: {error}");
        return;
    }
    if let Some(msg) = room.awareness().encode_all() {
        let result =
            tokio::time::timeout(WS_STATE_SEND_TIMEOUT, sink.send(WsMessage::binary(msg))).await;
        if !matches!(result, Ok(Ok(()))) {
            return;
        }
    }

    loop {
        tokio::select! {
            _ = async {
                match ydoc.as_ref() {
                    Some(ydoc) => ydoc.wait_until_evicted().await,
                    None => std::future::pending::<()>().await,
                }
            } => break,
            frame = stream.next() => {
                let Some(Ok(frame)) = frame else {
                    break;
                };
                let data = match frame {
                    WsMessage::Binary(data) => data,
                    // Complete the RFC 6455 close handshake. Dropping the sink
                    // without echoing the peer's close makes real browsers
                    // report an abnormal 1006 even though the room exited.
                    WsMessage::Close(_) => {
                        // Tungstenite queues the symmetric reply while reading
                        // the peer close. Drive that queued frame to the wire;
                        // trying to enqueue a second close is rejected because
                        // the protocol state is already ClosedByPeer.
                        if let Err(error) = sink.flush().await {
                            log::warn!("flush websocket close reply for {graph_id}: {error}");
                        }
                        break;
                    }
                    _ => continue,
                };
                let Some(message) = decode_inbound(&data) else {
                    continue;
                };
                match message {
                    Inbound::Step1(since) if kind == RoomKind::YDoc => {
                        // A Y.Doc's diff is one payload and, as `serve_room_with` did, is encoded
                        // under the graph's lifecycle lease and inside the delivery timeout, so
                        // deletion cannot interleave and a slow encode cannot park the connection.
                        let result = linearize_state_bearing_delivery(
                            &app,
                            &graph_id,
                            &evictable,
                            async {
                                let parts = match room.diff(&since, CONVERGENT_DIFF_CHUNK_BYTES).await {
                                    Ok(parts) => parts,
                                    // An undecodable version is skipped, as an undecodable message is.
                                    Err(ConvergeError::Decode(error)) => {
                                        log::warn!("ignoring undecodable SyncStep1 on {graph_id}: {error}");
                                        return Ok(());
                                    }
                                    Err(error) => return Err(error.to_string()),
                                };
                                for part in parts {
                                    sink.send(WsMessage::binary(sync_message(SYNC_STEP2, &part)))
                                        .await
                                        .map_err(|error| format!("send websocket SyncStep2: {error}"))?;
                                }
                                Ok(())
                            },
                        )
                        .await;
                        if let Err(error) = result {
                            log::warn!("closing websocket sync reply for {graph_id}: {error}");
                            break;
                        }
                    }
                    Inbound::Step1(since) => {
                        // A ledger's diff is a list of bounded chunks (a cold sync of a large ledger
                        // is many): each is delivered under its own lease, so the lease is held per
                        // chunk, not per ledger.
                        let parts = match room.diff(&since, CONVERGENT_DIFF_CHUNK_BYTES).await {
                            Ok(parts) => parts,
                            // A version this cell does not speak: say so and go, rather than leave
                            // the peer waiting for a reply that is never coming.
                            Err(error @ ConvergeError::UnsupportedVersion { .. }) => {
                                log::warn!("closing websocket sync reply for {graph_id}: {error}");
                                let _ = sink
                                    .send(close_with(LEDGER_VERSION_CLOSE, &error.to_string()))
                                    .await;
                                break;
                            }
                            // An undecodable version is skipped, as an undecodable message is.
                            Err(ConvergeError::Decode(error)) => {
                                log::warn!("ignoring undecodable SyncStep1 on {graph_id}: {error}");
                                continue;
                            }
                            Err(error) => {
                                log::warn!("closing websocket sync reply for {graph_id}: {error}");
                                break;
                            }
                        };
                        let mut failed = false;
                        let parts = if parts.is_empty() {
                            room.empty_diff().into_iter().collect()
                        } else {
                            parts
                        };
                        for part in parts {
                            let reply = sync_message(SYNC_STEP2, &part);
                            let result = linearize_state_bearing_delivery(
                                &app,
                                &graph_id,
                                &evictable,
                                async {
                                    sink.send(WsMessage::binary(reply))
                                        .await
                                        .map_err(|error| format!("send websocket SyncStep2: {error}"))
                                },
                            )
                            .await;
                            if let Err(error) = result {
                                log::warn!("closing websocket sync reply for {graph_id}: {error}");
                                failed = true;
                                break;
                            }
                        }
                        if failed {
                            break;
                        }
                    }
                    Inbound::Step2(update) | Inbound::Update(update) => {
                        // W6: what a visitor may send.
                        // An empty update (a fresh client's SyncStep2) costs nothing.
                        let empty = !matches!(access, RoomAccess::ReadWrite)
                            && payload_is_empty(kind, &update);
                        if empty {
                            continue;
                        }
                        let refused = match &access {
                            RoomAccess::ReadWrite => None,
                            RoomAccess::ReadOnly { .. } | RoomAccess::Viewer { .. } => {
                                let would_change = match ydoc.as_ref() {
                                    Some(ydoc) => ydoc.shadow_apply(&update, |_, _, _| Ok(())).await,
                                    None => ledger_payload_would_change(&*room, &update).await,
                                };
                                match would_change {
                                    Ok(false) => {
                                        visitor_noop_frames += 1;
                                        if visitor_noop_frames <= VISITOR_NOOP_FRAMES {
                                            continue;
                                        }
                                        Some("too many frames that change nothing".to_string())
                                    }
                                    Ok(true) if matches!(access, RoomAccess::Viewer { .. }) => {
                                        Some("read-only: viewers read this room".to_string())
                                    }
                                    Ok(true) => Some("read-only: visitors read this room".to_string()),
                                    Err(error) => Some(error),
                                }
                            }
                            RoomAccess::Fenced { fence, document_id } => match ydoc.as_ref() {
                                Some(ydoc) => {
                                    match crate::visitor_fence::admit_room_update(
                                        &app, &graph_id, fence, document_id, ydoc, &update,
                                    )
                                    .await
                                    {
                                        Ok(true) => None,
                                        Ok(false) => {
                                            visitor_noop_frames += 1;
                                            if visitor_noop_frames <= VISITOR_NOOP_FRAMES {
                                                continue;
                                            }
                                            Some("too many frames that change nothing".to_string())
                                        }
                                        Err(error) => Some(error),
                                    }
                                }
                                None => Some("a write fence applies to documents only".to_string()),
                            },
                        };
                        if let Some(reason) = refused {
                            log::info!("visitor room frame refused on {graph_id}: {reason}");
                            let reason: String = reason.chars().take(110).collect();
                            let _ = sink
                                .send(WsMessage::Close(Some(axum::extract::ws::CloseFrame {
                                    code: VISITOR_REFUSED_CLOSE,
                                    reason: reason.into(),
                                })))
                                .await;
                            break;
                        }
                        // A frame needs the graph's shared lifecycle lease, so deletion
                        // and the other exclusive roots still order against it, and
                        // nothing more: it never queues behind an API write holding the
                        // hot-write gate (an agent mid-`insert_blocks` used to stall
                        // Vera's typing). Operations that need a quiet room (a forward
                        // restore applies a diff, then verifies the room equals the
                        // target) raise an explicit frame fence, which waits for frames
                        // in flight and holds new ones back.
                        let _frame_lease = if let Some(coordinator) = app.try_state::<
                            super::persistence_coordinator::GraphPersistenceCoordinator,
                        >() {
                            match coordinator.acquire_client_frame(&graph_id).await {
                                Ok(lease) => Some(lease),
                                Err(error) => {
                                    log::error!(
                                        "websocket frame lease failed for {graph_id}: {error}"
                                    );
                                    continue;
                                }
                            }
                        } else {
                            None
                        };
                        match room.apply(&update, connection_id).await {
                            Ok(applied) => {
                                // A ledger room ships what it applied on its own timer (the ship
                                // task `LedgerRoom::apply` arms), so a connection that goes quiet,
                                // or a server-side append, still reaches every peer.
                                if let Some(reply) = applied.reply {
                                    let result = tokio::time::timeout(
                                        WS_STATE_SEND_TIMEOUT,
                                        sink.send(WsMessage::binary(sync_message(SYNC_UPDATE, &reply))),
                                    )
                                    .await;
                                    if !matches!(result, Ok(Ok(()))) {
                                        break;
                                    }
                                }
                            }
                            Err(ConvergeError::Fork { origin, seq }) => {
                                // The ledger is untouched. Say why, and go.
                                log::error!(
                                    "fork on {graph_id}: origin {origin} wrote two different events at seq {seq}; closing the connection"
                                );
                                let _ = sink
                                    .send(close_with(
                                        LEDGER_FORK_CLOSE,
                                        &format!("fork: {origin} seq {seq}"),
                                    ))
                                    .await;
                                break;
                            }
                            Err(error @ ConvergeError::UnsupportedVersion { .. }) => {
                                log::warn!("closing websocket room for {graph_id}: {error}");
                                let _ = sink
                                    .send(close_with(LEDGER_VERSION_CLOSE, &error.to_string()))
                                    .await;
                                break;
                            }
                            // (A Y.Doc room keeps its old behaviour: the generic arm below closes
                            // without a reason.)
                            Err(ConvergeError::Decode(reason)) if kind == RoomKind::Ledger => {
                                log::warn!("closing websocket room for {graph_id}: undecodable update: {reason}");
                                let _ = sink.send(close_with(LEDGER_DECODE_CLOSE, &reason)).await;
                                break;
                            }
                            Err(error) => {
                                // Deletion marks extant room Arcs evicted before it
                                // releases the graph lease. Close instead of silently
                                // accepting and dropping every later client write.
                                log::warn!("closing websocket room for {graph_id}: {error}");
                                break;
                            }
                        }
                    }
                    Inbound::Awareness(update) => {
                        // W6: a visitor's presence is the account's own (or
                        // none, for an anonymous visitor).
                        let update = match &access {
                            RoomAccess::ReadWrite => update,
                            RoomAccess::ReadOnly { account } => {
                                match crate::visitor_fence::stamp_awareness_update(&app, account.as_deref(), update) {
                                    Some(update) => update,
                                    None => continue,
                                }
                            }
                            RoomAccess::Fenced { fence, .. } => {
                                match crate::visitor_fence::stamp_awareness_update(&app, Some(&fence.account), update) {
                                    Some(update) => update,
                                    None => continue,
                                }
                            }
                            // A Viewer's presence is its lease principal.
                            RoomAccess::Viewer { principal } => {
                                match crate::visitor_fence::stamp_viewer_awareness_update(principal, update) {
                                    Some(update) => update,
                                    None => continue,
                                }
                            }
                        };
                        // A visitor never takes over a client id another live
                        // socket holds (sealed exam 2026-10-15: a higher clock
                        // displaced a collaborator's presence).
                        let update = match &access {
                            RoomAccess::ReadWrite => update,
                            _ => match room.awareness().held_by_this_socket(update, connection_id) {
                                Some(update) => update,
                                None => continue,
                            },
                        };
                        if let Some((accepted, own_only)) =
                            room.awareness().merge_detailed(&update, connection_id)
                        {
                            let rebroadcast = Message::Awareness(accepted).encode_v1();
                            publish_to(
                                &*room,
                                ydoc.as_ref(),
                                own_only.then_some(connection_id),
                                FrameKind::Awareness,
                                rebroadcast,
                            );
                        }
                    }
                    Inbound::AwarenessQuery => {
                        if let Some(msg) = room.awareness().encode_all() {
                            let result = tokio::time::timeout(
                                WS_STATE_SEND_TIMEOUT,
                                sink.send(WsMessage::binary(msg)),
                            )
                            .await;
                            if !matches!(result, Ok(Ok(()))) {
                                break;
                            }
                        }
                    }
                    Inbound::Ignored => {}
                }
            }
            broadcasted = rx.recv() => {
                let Ok(frame) = broadcasted else {
                    break;
                };
                // A client that produced the frame already has it.
                if frame.origin == Some(connection_id) {
                    continue;
                }
                let RoomFrame { kind: frame_kind, payload, .. } = frame;
                match frame_kind {
                    // Presence is ephemeral and carries no graph content:
                    // nothing for deletion to order against, so no lease.
                    FrameKind::Awareness => {
                        let result = tokio::time::timeout(
                            WS_STATE_SEND_TIMEOUT,
                            sink.send(WsMessage::binary(payload)),
                        )
                        .await;
                        if !matches!(result, Ok(Ok(()))) {
                            log::warn!("closing websocket awareness broadcast for {graph_id}");
                            break;
                        }
                    }
                    FrameKind::State => {
                        let result = broadcast_state_delivery(
                            &app,
                            &graph_id,
                            &evictable,
                            async {
                                sink.send(WsMessage::binary(payload))
                                    .await
                                    .map_err(|error| format!("send websocket room update: {error}"))
                            },
                        )
                        .await;
                        if let Err(error) = result {
                            log::warn!("closing websocket broadcast for {graph_id}: {error}");
                            break;
                        }
                    }
                    FrameKind::Evicted => break,
                }
            }
        }
    }

    // The socket is gone: bring the room's unpersisted tail to disk now,
    // rather than waiting out the window.
    match ydoc.as_ref() {
        Some(ydoc) => {
            // With the directory entry, under the graph's lifecycle lease.
            if let Err(error) = ydoc.flush_state_under_lifecycle(true).await {
                log::warn!("room state flush at close failed for {graph_id}: {error}");
            }
            ydoc.settle_state_dir().await;
        }
        None => ship_pending(&*room).await,
    }
    if let Some(removal) = room.awareness().remove_connection(connection_id) {
        // Other subscribers receive the synthetic removals even when this
        // transport disappeared without a graceful Awareness null.
        publish_to(
            &*room,
            ydoc.as_ref(),
            None,
            FrameKind::Awareness,
            Message::Awareness(removal).encode_v1(),
        );
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn disk_restore_admission_refuses_an_opener_paused_before_registry_insertion() {
        let registry = std::sync::Arc::new(super::RoomRegistry::default());
        let root =
            std::env::temp_dir().join(format!("garden-restore-room-race-{}", uuid::Uuid::new_v4()));
        let path = root.join("ydocs/workspace/update-v1.bin");
        let (reached_tx, reached_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        super::install_tombstone_open_race_hook(path.clone(), reached_tx, release_rx);
        let opener_registry = registry.clone();
        let opener_path = path.clone();
        let opener = std::thread::spawn(move || {
            crate::app_runtime::async_runtime::block_on(async move {
                opener_registry
                    .get_or_create("workspace:restore-race", opener_path)
                    .await
            })
        });
        reached_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("opener reached pre-admission barrier");
        let guard = registry.begin_disk_restore("restore-race").unwrap();
        release_tx.send(()).unwrap();
        let result = opener.join().expect("opener thread");
        super::clear_tombstone_open_race_hook();
        assert!(result
            .err()
            .unwrap()
            .contains("during disk archive restore"));
        assert!(registry
            .existing_room("workspace:restore-race")
            .unwrap()
            .is_none());
        assert!(!path.exists());
        drop(guard);
    }

    #[test]
    fn disk_restore_admission_blocks_new_rooms_and_never_evicts_existing_rooms() {
        crate::app_runtime::async_runtime::block_on(async {
            let registry = super::RoomRegistry::default();
            let root = std::env::temp_dir().join(format!(
                "garden-restore-room-admission-{}",
                uuid::Uuid::new_v4()
            ));
            let workspace_path = root.join("ydocs/workspace/update-v1.bin");
            let guard = registry.begin_disk_restore("graph-a").unwrap();
            assert!(registry
                .get_or_create("workspace:graph-a", workspace_path.clone())
                .await
                .is_err());
            assert!(registry
                .get_or_create(
                    "doc:graph-a:one",
                    root.join("ydocs/documents/one/update-v1.bin")
                )
                .await
                .is_err());
            assert!(registry.begin_disk_restore("graph-a").is_err());
            assert!(registry
                .existing_room("workspace:graph-a")
                .unwrap()
                .is_none());
            // The bounded reservation is graph-local, not a global room stop.
            let other = registry
                .get_or_create("workspace:graph-ab", root.join("other/update-v1.bin"))
                .await
                .unwrap();
            drop(guard);
            let existing = registry
                .get_or_create("workspace:graph-a", workspace_path.clone())
                .await
                .unwrap();
            assert!(registry.begin_disk_restore("graph-a").is_err());
            assert!(std::sync::Arc::ptr_eq(
                &existing,
                &registry
                    .existing_room("workspace:graph-a")
                    .unwrap()
                    .unwrap()
            ));
            assert!(std::sync::Arc::ptr_eq(
                &other,
                &registry
                    .existing_room("workspace:graph-ab")
                    .unwrap()
                    .unwrap()
            ));
            assert!(
                !workspace_path.exists(),
                "an untouched preopened room has no disk sidecar"
            );
        });
    }

    use super::*;
    use uuid::Uuid;
    use yrs::block::ClientID;
    use yrs::sync::awareness::AwarenessUpdateEntry;
    use yrs::{Map, StateVector, WriteTxn};

    fn awareness_update(entries: Vec<(u64, u32, String)>) -> AwarenessUpdate {
        AwarenessUpdate {
            clients: entries
                .into_iter()
                .map(|(client, clock, json)| {
                    (
                        ClientID::new(client),
                        AwarenessUpdateEntry {
                            clock,
                            json: json.into(),
                        },
                    )
                })
                .collect(),
        }
    }

    fn presence_json(
        human_id: &str,
        device_id: &str,
        client_id: &str,
        connection_epoch: u64,
    ) -> String {
        serde_json::json!({
            "user": { "name": human_id, "color": "#2563eb" },
            "presence": {
                "schemaVersion": 1,
                "humanId": human_id,
                "deviceId": device_id,
                "clientId": client_id,
                "connectionEpoch": connection_epoch,
                "room": { "graphId": "graph", "kind": "document", "documentId": "doc" }
            }
        })
        .to_string()
    }

    async fn presence_test_room(label: &str) -> Arc<Room> {
        let dir = std::env::temp_dir().join(format!("garden-room-{label}-{}", Uuid::new_v4()));
        RoomRegistry::default()
            .get_or_create(label, dir.join("update-v1.bin"))
            .await
            .expect("create presence test room")
    }

    #[test]
    fn awareness_null_is_removed_and_never_seeded_for_late_joiners() {
        crate::app_runtime::async_runtime::block_on(async {
            let room = presence_test_room("presence-null").await;
            let client = ClientID::new(41);
            let added = room
                .merge_awareness(
                    &awareness_update(vec![(
                        client.get(),
                        1,
                        presence_json("vera", "device-a", "tab-a", 1),
                    )]),
                    1,
                )
                .await
                .expect("active state accepted");
            assert!(added.clients.contains_key(&client));
            assert!(room.full_awareness().await.is_some());

            let removed = room
                .merge_awareness(&awareness_update(vec![(client.get(), 2, "null".into())]), 1)
                .await
                .expect("owned removal accepted");
            assert_eq!(removed.clients[&client].json.as_ref(), "null");
            assert!(room.full_awareness().await.is_none());

            // An otherwise-unknown tombstone can clean up a peer race. The
            // room retains its clock metadata but never replays it as presence.
            assert!(room
                .merge_awareness(&awareness_update(vec![(99, 4, "null".into())]), 2)
                .await
                .is_some());
            assert!(room.full_awareness().await.is_none());

            // A delayed packet from before the removal cannot resurrect the
            // collaborator even though the live JSON has already disappeared.
            assert!(room
                .merge_awareness(
                    &awareness_update(vec![(
                        client.get(),
                        1,
                        presence_json("vera", "device-a", "tab-a", 1),
                    )]),
                    1,
                )
                .await
                .is_none());
            assert!(room.full_awareness().await.is_none());
        });
    }

    #[test]
    fn disconnect_removes_only_entries_owned_by_that_socket() {
        crate::app_runtime::async_runtime::block_on(async {
            let room = presence_test_room("presence-owner").await;
            let first = ClientID::new(11);
            let second = ClientID::new(22);
            room.merge_awareness(
                &awareness_update(vec![(
                    first.get(),
                    7,
                    presence_json("vera", "device-a", "tab-a", 1),
                )]),
                100,
            )
            .await;
            room.merge_awareness(
                &awareness_update(vec![(
                    second.get(),
                    3,
                    presence_json("ada", "device-b", "tab-b", 1),
                )]),
                200,
            )
            .await;

            let removal = room
                .remove_awareness_for_connection(100)
                .await
                .expect("first connection owns one client");
            assert_eq!(removal.clients.len(), 1);
            assert_eq!(removal.clients[&first].clock, 8);
            assert_eq!(removal.clients[&first].json.as_ref(), "null");
            let live = room.full_awareness().await.expect("second user remains");
            assert_eq!(live.clients.len(), 1);
            assert!(live.clients.contains_key(&second));
            assert!(room.remove_awareness_for_connection(100).await.is_none());

            // Hard-close cleanup also leaves a protocol clock tombstone, so
            // an echoed pre-close state from another socket stays dead.
            assert!(room
                .merge_awareness(
                    &awareness_update(vec![(
                        first.get(),
                        7,
                        presence_json("vera", "device-a", "tab-a", 1),
                    )]),
                    300,
                )
                .await
                .is_none());
            let live = room
                .full_awareness()
                .await
                .expect("second user still remains");
            assert_eq!(live.clients.len(), 1);
            assert!(live.clients.contains_key(&second));
        });
    }

    #[test]
    fn same_human_in_distinct_tabs_remains_legitimately_present() {
        crate::app_runtime::async_runtime::block_on(async {
            let room = presence_test_room("presence-tabs").await;
            room.merge_awareness(
                &awareness_update(vec![(
                    101,
                    1,
                    presence_json("vera", "device-a", "tab-a", 1),
                )]),
                1,
            )
            .await;
            room.merge_awareness(
                &awareness_update(vec![(
                    202,
                    1,
                    presence_json("vera", "device-a", "tab-b", 1),
                )]),
                2,
            )
            .await;

            let live = room.full_awareness().await.expect("both tabs remain live");
            assert_eq!(live.clients.len(), 2);
            assert!(live.clients.contains_key(&ClientID::new(101)));
            assert!(live.clients.contains_key(&ClientID::new(202)));
        });
    }

    #[test]
    fn newer_epoch_supersedes_only_the_same_logical_tab() {
        crate::app_runtime::async_runtime::block_on(async {
            let room = presence_test_room("presence-epoch").await;
            let old_client = ClientID::new(301);
            let new_client = ClientID::new(302);
            room.merge_awareness(
                &awareness_update(vec![(
                    old_client.get(),
                    5,
                    presence_json("vera", "device-a", "tab-a", 8),
                )]),
                10,
            )
            .await;

            let replacement = room
                .merge_awareness(
                    &awareness_update(vec![(
                        new_client.get(),
                        1,
                        presence_json("vera", "device-a", "tab-a", 9),
                    )]),
                    20,
                )
                .await
                .expect("new epoch accepted");
            assert_eq!(replacement.clients.len(), 2);
            assert_eq!(replacement.clients[&old_client].clock, 6);
            assert_eq!(replacement.clients[&old_client].json.as_ref(), "null");
            assert_eq!(replacement.clients[&new_client].clock, 1);

            assert!(room
                .merge_awareness(
                    &awareness_update(vec![(
                        old_client.get(),
                        99,
                        presence_json("vera", "device-a", "tab-a", 8),
                    )]),
                    10,
                )
                .await
                .is_none());
            assert!(room.remove_awareness_for_connection(10).await.is_none());
            let live = room.full_awareness().await.expect("replacement remains");
            assert_eq!(live.clients.len(), 1);
            assert!(live.clients.contains_key(&new_client));
        });
    }

    #[test]
    fn delayed_old_null_cannot_delete_same_client_id_replacement() {
        crate::app_runtime::async_runtime::block_on(async {
            let room = presence_test_room("presence-delayed-null").await;
            let client = ClientID::new(501);
            room.merge_awareness(
                &awareness_update(vec![(
                    client.get(),
                    5,
                    presence_json("vera", "device-a", "tab-a", 1),
                )]),
                10,
            )
            .await;
            room.merge_awareness(
                &awareness_update(vec![(
                    client.get(),
                    6,
                    presence_json("vera", "device-a", "tab-a", 2),
                )]),
                20,
            )
            .await
            .expect("new socket takes ownership at a higher epoch");

            assert!(room
                .merge_awareness(
                    &awareness_update(vec![(client.get(), 7, "null".into())]),
                    10,
                )
                .await
                .is_none());
            assert!(room.remove_awareness_for_connection(10).await.is_none());
            let state = room.awareness.entries_for_test();
            assert_eq!(
                state.get(&client).expect("replacement remains").owner,
                Some(20)
            );
        });
    }

    #[test]
    fn equal_epoch_from_another_socket_cannot_transfer_ownership() {
        crate::app_runtime::async_runtime::block_on(async {
            let room = presence_test_room("presence-equal-epoch").await;
            let client = ClientID::new(601);
            room.merge_awareness(
                &awareness_update(vec![(
                    client.get(),
                    1,
                    presence_json("vera", "device-a", "tab-a", 7),
                )]),
                10,
            )
            .await;

            assert!(room
                .merge_awareness(
                    &awareness_update(vec![(
                        client.get(),
                        2,
                        presence_json("vera", "device-a", "tab-a", 7),
                    )]),
                    20,
                )
                .await
                .is_none());
            assert!(room.remove_awareness_for_connection(20).await.is_none());
            let state = room.awareness.entries_for_test();
            let live = state.get(&client).expect("original socket remains owner");
            assert_eq!(live.owner, Some(10));
            assert_eq!(live.entry.clock, 1);
        });
    }

    #[test]
    fn superseded_epoch_cannot_reanimate_during_replacement_exit_gap() {
        crate::app_runtime::async_runtime::block_on(async {
            let room = presence_test_room("presence-epoch-gap").await;
            let old_client = ClientID::new(701);
            let new_client = ClientID::new(702);
            room.merge_awareness(
                &awareness_update(vec![(
                    old_client.get(),
                    2,
                    presence_json("vera", "device-a", "tab-a", 8),
                )]),
                10,
            )
            .await;
            room.merge_awareness(
                &awareness_update(vec![(
                    new_client.get(),
                    1,
                    presence_json("vera", "device-a", "tab-a", 9),
                )]),
                20,
            )
            .await;

            room.remove_awareness_for_connection(20)
                .await
                .expect("replacement removed");
            assert!(room.full_awareness().await.is_none());
            assert!(room
                .merge_awareness(
                    &awareness_update(vec![(
                        old_client.get(),
                        99,
                        presence_json("vera", "device-a", "tab-a", 8),
                    )]),
                    10,
                )
                .await
                .is_none());

            assert!(room.remove_awareness_for_connection(10).await.is_none());
            assert!(room
                .merge_awareness(
                    &awareness_update(vec![(
                        703,
                        1,
                        presence_json("vera", "device-a", "tab-a", 1),
                    )]),
                    30,
                )
                .await
                .is_some());
        });
    }

    #[test]
    fn legacy_awareness_without_presence_metadata_is_still_connection_scoped() {
        crate::app_runtime::async_runtime::block_on(async {
            let room = presence_test_room("presence-legacy").await;
            let client = ClientID::new(801);
            room.merge_awareness(
                &awareness_update(vec![(
                    client.get(),
                    1,
                    r#"{"user":{"name":"legacy"}}"#.into(),
                )]),
                80,
            )
            .await
            .expect("legacy awareness accepted");
            let removal = room
                .remove_awareness_for_connection(80)
                .await
                .expect("legacy state cleaned up on disconnect");
            assert_eq!(removal.clients[&client].json.as_ref(), "null");
            assert!(room.full_awareness().await.is_none());
        });
    }

    #[test]
    fn tombstoned_sidecar_is_rejected_and_recreation_opens_fresh_authority() {
        crate::app_runtime::async_runtime::block_on(async {
            let graph_dir = std::env::temp_dir()
                .join(format!("garden-room-document-tombstone-{}", Uuid::new_v4()));
            let graph_id = "tombstone-room-graph";
            let document_id = "tombstone-room-document";
            let key = format!("doc:{graph_id}:{document_id}");
            let state_path = crate::ydoc_paths::document_ydoc_state_path(&graph_dir, document_id);
            let registry = RoomRegistry::default();
            let stale = registry
                .get_or_create(&key, state_path.clone())
                .await
                .expect("create stale room");
            stale
                .update_doc(|_doc, txn| {
                    txn.get_or_insert_map("metadata")
                        .insert(txn, "deleted-sentinel", true);
                    Ok(())
                })
                .await
                .expect("persist stale sidecar");
            registry.evict_room(&key);
            let tombstone =
                crate::document_tombstone_store::write_document_tombstone_for_operation(
                    &graph_dir,
                    document_id,
                    Some("delete-tombstone-room"),
                )
                .expect("write document tombstone");

            let error = match registry.get_or_create(&key, state_path.clone()).await {
                Ok(_) => panic!("ordinary hydration must reject tombstone"),
                Err(error) => error,
            };
            assert!(error.contains("document tombstoned"), "{error}");
            assert!(registry.peek(&key).await.is_none());

            let fresh = registry
                .recreate_tombstoned_document_room(&key, state_path, &tombstone.deletion_id)
                .await
                .expect("open trusted fresh recreation room");
            fresh
                .with_doc(|doc| {
                    let txn = doc.transact();
                    assert!(txn.get_map("metadata").is_none());
                })
                .await;
            assert!(crate::document_tombstone_store::document_is_tombstoned(
                &graph_dir,
                document_id
            )
            .expect("tombstone remains during recreation"));
            let newer = crate::document_tombstone_store::write_document_tombstone_for_operation(
                &graph_dir,
                document_id,
                Some("newer-delete-during-recreation"),
            )
            .expect("establish newer delete fence");
            assert_ne!(newer.deletion_id, tombstone.deletion_id);
            registry.evict_room(&key);
            let clear_error = crate::document_tombstone_store::clear_document_tombstone_if_matches(
                &graph_dir,
                document_id,
                &tombstone.deletion_id,
            )
            .expect_err("stale recreation cannot clear newer delete");
            assert!(
                clear_error.contains("deletion boundary changed"),
                "{clear_error}"
            );
            assert_eq!(
                crate::document_tombstone_store::read_document_tombstone(&graph_dir, document_id,)
                    .expect("read newer delete")
                    .expect("newer delete remains")
                    .deletion_id,
                newer.deletion_id
            );
            assert!(registry.peek(&key).await.is_none());
            let _ = std::fs::remove_dir_all(graph_dir);
        });
    }

    #[test]
    fn tombstone_inserted_between_room_precheck_and_insert_evicts_new_room() {
        let graph_dir =
            std::env::temp_dir().join(format!("garden-room-tombstone-race-{}", Uuid::new_v4()));
        let graph_id = "tombstone-race-graph";
        let document_id = "tombstone-race-document";
        let key = format!("doc:{graph_id}:{document_id}");
        let state_path = crate::ydoc_paths::document_ydoc_state_path(&graph_dir, document_id);
        let registry = Arc::new(RoomRegistry::default());
        let (reached_tx, reached_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        install_tombstone_open_race_hook(state_path.clone(), reached_tx, release_rx);

        let opener_registry = registry.clone();
        let opener_key = key.clone();
        let opener_path = state_path.clone();
        let opener = std::thread::spawn(move || {
            crate::app_runtime::async_runtime::block_on(async {
                opener_registry
                    .get_or_create(&opener_key, opener_path)
                    .await
            })
        });
        reached_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("room open reached post-precheck barrier");
        crate::document_tombstone_store::write_document_tombstone_for_operation(
            &graph_dir,
            document_id,
            Some("delete-during-room-open"),
        )
        .expect("write racing tombstone");
        assert!(
            !registry.evict_room(&key),
            "delete-side eviction should observe no inserted room yet"
        );
        release_tx.send(()).expect("release room opener");
        let error = match opener.join().expect("room opener thread") {
            Ok(_) => panic!("post-insert check must reject racing tombstone"),
            Err(error) => error,
        };
        assert!(error.contains("document tombstoned"), "{error}");
        crate::app_runtime::async_runtime::block_on(async {
            assert!(registry.peek(&key).await.is_none());
        });
        clear_tombstone_open_race_hook();
        let _ = std::fs::remove_dir_all(graph_dir);
    }

    /// Regression for the recheck-error room leak: `get_or_create` inserts
    /// the room BEFORE its final tombstone recheck, and an I/O/parse error
    /// out of the tombstone store used to escape via `?` with the freshly
    /// decoded room still retained — the caller got Err (no Arc), nothing
    /// would ever evict it, and every retry saw it as "pre-live". Uses the
    /// same post-precheck race hook as the tombstone-race test, but corrupts
    /// the tombstone store during the pause so the RECHECK errors instead of
    /// observing a valid tombstone: the open must fail AND leave no room
    /// behind.
    #[test]
    fn tombstone_recheck_error_does_not_retain_freshly_inserted_room() {
        let graph_dir =
            std::env::temp_dir().join(format!("garden-room-recheck-error-{}", Uuid::new_v4()));
        let graph_id = "recheck-error-graph";
        let document_id = "recheck-error-document";
        let key = format!("doc:{graph_id}:{document_id}");
        let state_path = crate::ydoc_paths::document_ydoc_state_path(&graph_dir, document_id);
        let registry = Arc::new(RoomRegistry::default());
        let (reached_tx, reached_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        install_tombstone_open_race_hook(state_path.clone(), reached_tx, release_rx);

        let opener_registry = registry.clone();
        let opener_key = key.clone();
        let opener_path = state_path.clone();
        let opener = std::thread::spawn(move || {
            crate::app_runtime::async_runtime::block_on(async {
                opener_registry
                    .get_or_create(&opener_key, opener_path)
                    .await
            })
        });
        reached_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("room open reached post-precheck barrier");
        // Corrupt the tombstone store DURING the pause: the precheck already
        // passed clean, so the failure is confined to the post-insert
        // recheck — unreadable garbage where a tombstone record would be.
        let tombstone_path =
            crate::document_tombstone_store::document_tombstone_path(&graph_dir, document_id)
                .expect("tombstone path");
        std::fs::create_dir_all(tombstone_path.parent().expect("tombstones dir"))
            .expect("create tombstones dir");
        std::fs::write(&tombstone_path, b"{{{ not tombstone json")
            .expect("write corrupt tombstone");
        release_tx.send(()).expect("release room opener");

        let error = match opener.join().expect("room opener thread") {
            Ok(_) => panic!("an unevaluable tombstone recheck must fail the open"),
            Err(error) => error,
        };
        assert!(
            !error.contains("document tombstoned:"),
            "this is the recheck ERROR path, not a valid-tombstone rejection: {error}"
        );
        crate::app_runtime::async_runtime::block_on(async {
            assert!(
                registry.peek(&key).await.is_none(),
                "a failed open must never leave its freshly inserted room retained"
            );
        });
        clear_tombstone_open_race_hook();
        let _ = std::fs::remove_dir_all(graph_dir);
    }

    /// Direct guard on the identity-checked eviction primitive: after a room
    /// R1 is replaced at its key by a distinct live room R2, evicting BY R1's
    /// identity must be a no-op (R2 survives), while evicting by R2's identity
    /// removes the current entry. This is the invariant the opener error
    /// cleanup relies on to never destroy an intervening live room.
    #[test]
    fn evict_room_if_identity_spares_a_replacement_room() {
        crate::app_runtime::async_runtime::block_on(async {
            let dir =
                std::env::temp_dir().join(format!("garden-room-identity-evict-{}", Uuid::new_v4()));
            let key = "doc:identity-graph:identity-document";
            let registry = RoomRegistry::default();

            let r1 = registry
                .get_or_create(key, dir.join("state-1.bin"))
                .await
                .expect("open R1");
            // Replace R1 with a fresh, distinct live room R2 at the same key.
            assert!(registry.evict_room(key), "R1 must be the current entry");
            let r2 = registry
                .get_or_create(key, dir.join("state-2.bin"))
                .await
                .expect("open R2");
            assert!(!Arc::ptr_eq(&r1, &r2), "R2 must be a distinct room");

            // Identity-checked eviction against the STALE R1 must not touch R2.
            assert!(
                !registry.evict_room_if_identity(key, &r1),
                "evicting by R1 identity must be a no-op — R1 is no longer the entry"
            );
            let live = registry.peek(key).await.expect("R2 must survive");
            assert!(Arc::ptr_eq(&live, &r2), "the surviving room must be R2");
            assert!(!r2.is_evicted(), "R2 must not be evicted");

            // Identity-checked eviction against the CURRENT R2 removes it.
            assert!(
                registry.evict_room_if_identity(key, &r2),
                "evicting by R2 identity removes the current entry"
            );
            assert!(registry.peek(key).await.is_none(), "R2 must be gone");
            assert!(r2.is_evicted(), "R2 must now be evicted");

            let _ = std::fs::remove_dir_all(dir);
        });
    }

    /// End-to-end interleaving for the evict-by-key hazard: opener A inserts
    /// R1, then (in the no-lock window before its final tombstone recheck)
    /// another path evicts R1 and installs a NEW live room R2 at the same key;
    /// A's recheck then ERRORS. A's error cleanup must remove ONLY the room it
    /// inserted — R2, a healthy room a collaborator is using, must survive.
    /// Before the fix A evicted BY KEY and destroyed R2.
    #[test]
    fn recheck_error_cleanup_spares_an_intervening_live_room() {
        let graph_dir = std::env::temp_dir().join(format!(
            "garden-room-recheck-intervening-{}",
            Uuid::new_v4()
        ));
        let graph_id = "recheck-intervening-graph";
        let document_id = "recheck-intervening-document";
        let key = format!("doc:{graph_id}:{document_id}");
        let state_path = crate::ydoc_paths::document_ydoc_state_path(&graph_dir, document_id);
        let registry = Arc::new(RoomRegistry::default());
        let (reached_tx, reached_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        // A dedicated barrier that fires AFTER A inserts its room but BEFORE
        // its recheck — the exact no-lock window the race exploits.
        install_tombstone_recheck_race_hook(state_path.clone(), reached_tx, release_rx);

        let opener_registry = registry.clone();
        let opener_key = key.clone();
        let opener_path = state_path.clone();
        let opener = std::thread::spawn(move || {
            crate::app_runtime::async_runtime::block_on(async {
                opener_registry
                    .get_or_create(&opener_key, opener_path)
                    .await
            })
        });
        reached_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("room open reached post-insert / pre-recheck barrier");

        // A has inserted R1. Evict it and install a NEW distinct live room R2
        // at the same key. R2 opens with an ALTERNATE state path so the
        // recheck barrier (keyed on A's path) does not pause it.
        assert!(registry.evict_room(&key), "A's R1 must be present to evict");
        let alt_document_id = "recheck-intervening-live";
        let alt_state_path =
            crate::ydoc_paths::document_ydoc_state_path(&graph_dir, alt_document_id);
        let r2 = crate::app_runtime::async_runtime::block_on(async {
            registry.get_or_create(&key, alt_state_path).await
        })
        .expect("install intervening live room R2");

        // Make A's recheck ERROR: corrupt the tombstone store for A's document
        // during the pause (its precheck already passed clean).
        let tombstone_path =
            crate::document_tombstone_store::document_tombstone_path(&graph_dir, document_id)
                .expect("tombstone path");
        std::fs::create_dir_all(tombstone_path.parent().expect("tombstones dir"))
            .expect("create tombstones dir");
        std::fs::write(&tombstone_path, b"{{{ not tombstone json").expect("corrupt tombstone");

        release_tx.send(()).expect("release room opener");
        let error = match opener.join().expect("room opener thread") {
            Ok(_) => panic!("an unevaluable tombstone recheck must fail A's open"),
            Err(error) => error,
        };
        assert!(
            !error.contains("document tombstoned:"),
            "this is the recheck ERROR path, not a valid-tombstone rejection: {error}"
        );

        // The intervening live room R2 must still be the entry at the key —
        // A's identity-checked cleanup must not have evicted it.
        crate::app_runtime::async_runtime::block_on(async {
            let live = registry
                .peek(&key)
                .await
                .expect("intervening live room R2 must survive A's error cleanup");
            assert!(
                Arc::ptr_eq(&live, &r2),
                "the surviving room must be R2, not evicted by A's by-key cleanup"
            );
        });
        assert!(!r2.is_evicted(), "R2 must not have been evicted by A");

        clear_tombstone_recheck_race_hook();
        let _ = std::fs::remove_dir_all(graph_dir);
    }

    #[test]
    fn projection_epoch_does_not_clear_a_newer_room_update() {
        crate::app_runtime::async_runtime::block_on(async {
            let dir = std::env::temp_dir().join(format!("garden-room-epoch-{}", Uuid::new_v4()));
            let state_path = dir.join("update-v1.bin");
            let registry = RoomRegistry::default();
            let room = registry
                .get_or_create("doc:graph:document", state_path)
                .await
                .expect("create room");
            assert!(!room.needs_projection_flush());

            room.update_doc(|_doc, txn| {
                txn.get_or_insert_map("metadata").insert(txn, "step", 1);
                Ok(())
            })
            .await
            .expect("first update");
            assert!(room.needs_projection_flush());
            let (first_epoch, ()) = room.with_doc_version(|_| ()).await;

            room.update_doc(|_doc, txn| {
                txn.get_or_insert_map("metadata").insert(txn, "step", 2);
                Ok(())
            })
            .await
            .expect("second update");
            room.mark_projection_persisted(first_epoch);
            assert!(
                room.needs_projection_flush(),
                "persisting an older snapshot must not clear the second update"
            );

            let (latest_epoch, ()) = room.with_doc_version(|_| ()).await;
            room.mark_projection_persisted(latest_epoch);
            assert!(!room.needs_projection_flush());
            let _ = std::fs::remove_dir_all(dir);
        });
    }

    #[test]
    fn duplicate_client_update_does_not_redirty_room() {
        crate::app_runtime::async_runtime::block_on(async {
            let dir = std::env::temp_dir().join(format!("garden-room-sync-{}", Uuid::new_v4()));
            let registry = RoomRegistry::default();
            let room = registry
                .get_or_create("doc:graph:remote", dir.join("update-v1.bin"))
                .await
                .expect("create room");
            let remote = Doc::new();
            {
                let mut txn = remote.transact_mut();
                txn.get_or_insert_map("metadata")
                    .insert(&mut txn, "source", "browser");
            }
            let update = remote
                .transact()
                .encode_state_as_update_v1(&StateVector::default());
            assert!(room
                .apply_client_update(&update)
                .await
                .expect("first client update"));
            let (epoch, ()) = room.with_doc_version(|_| ()).await;
            room.mark_projection_persisted(epoch);
            assert!(!room.needs_projection_flush());

            assert!(!room
                .apply_client_update(&update)
                .await
                .expect("duplicate client update"));
            assert!(
                !room.needs_projection_flush(),
                "a sync replay with no state delta must remain clean"
            );
            let _ = std::fs::remove_dir_all(dir);
        });
    }

    #[test]
    fn hot_state_write_failure_is_rejected_before_broadcast_and_room_is_reopened() {
        crate::app_runtime::async_runtime::block_on(async {
            let dir = std::env::temp_dir().join(format!("garden-room-io-{}", Uuid::new_v4()));
            std::fs::create_dir_all(&dir).expect("create test root");
            let impossible_parent = dir.join("not-a-directory");
            std::fs::write(&impossible_parent, b"file").expect("create impossible parent");
            let state_path = impossible_parent.join("update-v1.bin");
            let registry = RoomRegistry::default();
            let key = "doc:graph:io-failure";
            let room = registry
                .get_or_create(key, state_path.clone())
                .await
                .expect("create room");
            let mut broadcasts = room.subscribe_updates_for_test();
            let remote = Doc::new();
            {
                let mut txn = remote.transact_mut();
                txn.get_or_insert_map("metadata")
                    .insert(&mut txn, "must-not-ack", true);
            }
            let update = remote
                .transact()
                .encode_state_as_update_v1(&StateVector::default());

            let error = room
                .apply_client_update(&update)
                .await
                .expect_err("failed authority write must reject client update");
            assert!(error.contains("room persist mkdir"), "{error}");
            assert!(room.is_evicted());
            assert!(registry.peek(key).await.is_none());
            assert!(matches!(
                broadcasts.try_recv(),
                Err(tokio::sync::broadcast::error::TryRecvError::Empty)
            ));

            std::fs::remove_file(&impossible_parent).expect("remove impossible parent");
            std::fs::create_dir_all(&impossible_parent).expect("repair parent");
            let reopened = registry
                .get_or_create(key, state_path.clone())
                .await
                .expect("reopen from durable state");
            assert!(!Arc::ptr_eq(&room, &reopened));
            assert!(reopened
                .apply_client_update(&update)
                .await
                .expect("persist after repairing path"));
            assert!(state_path.is_file());
            let _ = std::fs::remove_dir_all(dir);
        });
    }

    #[test]
    fn mutate_then_error_evicts_partial_doc_and_reopens_durable_state() {
        crate::app_runtime::async_runtime::block_on(async {
            let dir = std::env::temp_dir().join(format!("garden-room-mutate-{}", Uuid::new_v4()));
            let state_path = dir.join("update-v1.bin");
            let registry = RoomRegistry::default();
            let key = "doc:graph:mutate-error";
            let room = registry
                .get_or_create(key, state_path.clone())
                .await
                .expect("create room");
            room.update_doc(|_doc, txn| {
                txn.get_or_insert_map("metadata")
                    .insert(txn, "durable", true);
                Ok(())
            })
            .await
            .expect("seed durable state");
            let mut broadcasts = room.subscribe_updates_for_test();
            while broadcasts.try_recv().is_ok() {}

            let error = room
                .update_doc(|_doc, txn| {
                    txn.get_or_insert_map("metadata")
                        .insert(txn, "sentinel", true);
                    Err::<(), _>("reject after mutation".to_string())
                })
                .await
                .expect_err("mutate-then-error must be rejected");
            assert_eq!(error, "reject after mutation");
            assert!(room.is_evicted());
            assert!(registry.peek(key).await.is_none());
            assert!(matches!(
                broadcasts.try_recv(),
                Err(tokio::sync::broadcast::error::TryRecvError::Empty)
            ));

            let reopened = registry
                .get_or_create(key, state_path)
                .await
                .expect("reopen durable room");
            assert!(!Arc::ptr_eq(&room, &reopened));
            reopened
                .with_doc(|doc| {
                    let txn = doc.transact();
                    let metadata = txn.get_map("metadata").expect("metadata map");
                    assert!(metadata.contains_key(&txn, "durable"));
                    assert!(!metadata.contains_key(&txn, "sentinel"));
                })
                .await;
            let _ = std::fs::remove_dir_all(dir);
        });
    }

    #[test]
    fn eviction_notification_survives_release_before_first_await() {
        crate::app_runtime::async_runtime::block_on(async {
            let dir = std::env::temp_dir().join(format!("garden-room-notify-{}", Uuid::new_v4()));
            let room = RoomRegistry::default()
                .get_or_create("doc:notify:document", dir.join("update-v1.bin"))
                .await
                .expect("create room");

            // Reproduce the Notify edge exactly: register the waiter, evict,
            // and only then perform its first await.
            let notified = room.eviction_notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            room.evict();
            tokio::time::timeout(std::time::Duration::from_secs(1), notified)
                .await
                .expect("registered eviction waiter woke");
            room.wait_until_evicted().await;
            let _ = std::fs::remove_dir_all(dir);
        });
    }

    #[test]
    fn eviction_cancels_backoff_and_in_flight_waits() {
        crate::app_runtime::async_runtime::block_on(async {
            let dir = std::env::temp_dir().join(format!("garden-room-cancel-{}", Uuid::new_v4()));

            let backoff_registry = RoomRegistry::default();
            let backoff_room = backoff_registry
                .get_or_create(
                    "doc:backoff-graph:document",
                    dir.join("backoff-update-v1.bin"),
                )
                .await
                .expect("create backoff room");
            let backoff_weak = Arc::downgrade(&backoff_room);
            let backoff = crate::app_runtime::async_runtime::spawn(async move {
                wait_for_room_delay(&backoff_weak, std::time::Duration::from_secs(60)).await
            });
            tokio::task::yield_now().await;
            assert_eq!(backoff_registry.evict_graph("backoff-graph"), 1);
            assert!(
                tokio::time::timeout(std::time::Duration::from_secs(1), backoff)
                    .await
                    .expect("backoff waiter cancelled")
                    .expect("backoff task")
            );

            let attempt_registry = RoomRegistry::default();
            let attempt_room = attempt_registry
                .get_or_create(
                    "doc:attempt-graph:document",
                    dir.join("attempt-update-v1.bin"),
                )
                .await
                .expect("create attempt room");
            let attempt_weak = Arc::downgrade(&attempt_room);
            let attempt = crate::app_runtime::async_runtime::spawn(async move {
                let pending = std::future::pending::<()>();
                tokio::pin!(pending);
                wait_for_projection_attempt(
                    &attempt_weak,
                    pending.as_mut(),
                    std::time::Duration::from_secs(60),
                )
                .await
            });
            tokio::task::yield_now().await;
            assert_eq!(attempt_registry.evict_graph("attempt-graph"), 1);
            assert!(matches!(
                tokio::time::timeout(std::time::Duration::from_secs(1), attempt)
                    .await
                    .expect("in-flight waiter cancelled")
                    .expect("attempt task"),
                ProjectionAttempt::Evicted
            ));

            let _ = std::fs::remove_dir_all(dir);
        });
    }

    #[cfg(all(feature = "headless", not(feature = "desktop")))]
    #[test]
    fn deletion_waits_until_state_bearing_delivery_is_linearized() {
        use crate::graph_service::{
            create_graph_service, soft_delete_graph_service_async, CreateGraphInput,
        };

        let _serial = crate::tauri_runtime::profile_env_serial()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let profile =
            std::env::temp_dir().join(format!("garden-ws-delete-linearization-{}", Uuid::new_v4()));
        std::env::set_var("GARDEN_PROFILE_DIR", &profile);

        let result = std::panic::catch_unwind(|| {
            crate::app_runtime::async_runtime::block_on(async {
                let app = crate::tauri_runtime::build_mock_app_for_tests(true);
                let graph_id = "ws-delete-linearization";
                create_graph_service(
                    &app,
                    CreateGraphInput {
                        title: "WS deletion fence".to_string(),
                        graph_id: Some(graph_id.to_string()),
                        description: None,
                        operation_id: None,
                    },
                )
                .expect("create graph");
                let graph_dir =
                    crate::graph_paths::existing_graph_dir(&app, graph_id).expect("graph dir");
                let registry = app.state::<RoomRegistry>();
                let room = registry
                    .get_or_create(
                        &format!("workspace:{graph_id}"),
                        crate::ydoc_paths::workspace_ydoc_state_path(&graph_dir),
                    )
                    .await
                    .expect("workspace room");

                let (delivery_started_tx, delivery_started_rx) = tokio::sync::oneshot::channel();
                let (allow_delivery_tx, allow_delivery_rx) = tokio::sync::oneshot::channel();
                let delivered = Arc::new(AtomicBool::new(false));
                let app_for_sync = app.clone();
                let room_for_sync = room.clone();
                let delivered_for_sync = delivered.clone();
                let sync = crate::app_runtime::async_runtime::spawn(async move {
                    linearize_state_bearing_delivery(
                        &app_for_sync,
                        graph_id,
                        &room_for_sync,
                        async move {
                            let _ = delivery_started_tx.send(());
                            allow_delivery_rx
                                .await
                                .map_err(|_| "test delivery release channel dropped".to_string())?;
                            delivered_for_sync.store(true, Ordering::SeqCst);
                            Ok(())
                        },
                    )
                    .await
                });
                tokio::time::timeout(std::time::Duration::from_secs(1), delivery_started_rx)
                    .await
                    .expect("state-bearing delivery acquired graph lease")
                    .expect("delivery owns graph lease");

                let (delete_done_tx, mut delete_done_rx) = tokio::sync::mpsc::channel(1);
                let app_for_delete = app.clone();
                let delete = crate::app_runtime::async_runtime::spawn(async move {
                    let result = soft_delete_graph_service_async(
                        &app_for_delete,
                        graph_id.to_string(),
                        false,
                    )
                    .await;
                    let _ = delete_done_tx.send(result).await;
                });
                assert!(
                    tokio::time::timeout(
                        std::time::Duration::from_millis(25),
                        delete_done_rx.recv(),
                    )
                    .await
                    .is_err(),
                    "deletion completed while a state-bearing frame still owned authority"
                );
                assert!(!delivered.load(Ordering::SeqCst));

                allow_delivery_tx.send(()).expect("release delivery");
                sync.await
                    .expect("sync task")
                    .expect("state-bearing delivery");
                let delete_result =
                    tokio::time::timeout(std::time::Duration::from_secs(5), delete_done_rx.recv())
                        .await
                        .expect("deletion released after delivery")
                        .expect("delete result message");
                delete_result.expect("delete graph");
                delete.await.expect("delete task");

                assert!(delivered.load(Ordering::SeqCst));
                assert!(registry
                    .peek(&format!("workspace:{graph_id}"))
                    .await
                    .is_none());
                assert!(
                    crate::graph_record_store::read_graph_record_no_heal(&app, graph_id).is_err(),
                    "deleted graph remained attachable"
                );
            });
        });

        std::env::remove_var("GARDEN_PROFILE_DIR");
        let _ = std::fs::remove_dir_all(&profile);
        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
    }

    #[test]
    fn linearize_state_bearing_delivery_acquire_fails_fast_behind_a_held_lease() {
        // FIX B regression: an un-timed lease acquire here head-of-line-blocks
        // every doc-open/SyncStep2 behind whatever else currently holds the
        // graph lease (e.g. a persist() stalled behind the durable flush's
        // gate for ~9s — see plans/canary-doc-open-ws-diagnosis-20260715.md
        // THIRD LAYER). Bounding it with WS_STATE_SEND_TIMEOUT must fail fast
        // instead of parking forever, and must never send/double-send a
        // frame in doing so. Paused-clock current-thread harness (mirrors
        // crdt_engine::executor's incarnation_recovery_tests) so the test
        // doesn't burn 5 real seconds.
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("current-thread Tokio runtime");
        runtime.block_on(async {
            tokio::time::pause();
            let app = crate::tauri_runtime::build_mock_app_for_tests(true);
            let graph_id = "graph-linearize-acquire-timeout";
            let room = presence_test_room("linearize-acquire-timeout").await;

            // Hold exclusive lifecycle authority from a separate root for the
            // whole test. Shared delivery must remain bounded when deletion or
            // another lifecycle operation owns the graph.
            let coordinator = app
                .state::<crate::crdt_engine::persistence_coordinator::GraphPersistenceCoordinator>(
                );
            let _holder = coordinator
                .acquire_lifecycle_exclusive(graph_id)
                .await
                .expect("holder acquires the graph lease first");

            let delivered = Arc::new(AtomicBool::new(false));
            let delivered_for_call = delivered.clone();
            let app_for_call = app.clone();
            let room_for_call = room.clone();
            // Plain `tokio::spawn` (not `crate::app_runtime::async_runtime::spawn`) binds
            // to this ambient paused-clock runtime — see the identical note
            // on `spawn_scheduled_retry` in crdt_engine::executor.
            let call = tokio::spawn(async move {
                linearize_state_bearing_delivery(
                    &app_for_call,
                    graph_id,
                    &room_for_call,
                    async move {
                        delivered_for_call.store(true, Ordering::SeqCst);
                        Ok(())
                    },
                )
                .await
            });
            // Let the spawned task run up to (and park inside) its acquire
            // before advancing time past the bound.
            tokio::task::yield_now().await;
            assert!(
                !call.is_finished(),
                "acquire resolved before the lease was held"
            );
            tokio::time::advance(WS_STATE_SEND_TIMEOUT + Duration::from_millis(1)).await;

            let outcome = call.await.expect("spawned linearize task");
            let error = outcome.expect_err(
                "acquire must fail fast rather than park un-timed behind a lease held elsewhere",
            );
            assert!(
                error.contains("lease acquire timed out"),
                "unexpected error: {error}"
            );
            assert!(
                !delivered.load(Ordering::SeqCst),
                "delivery must never run once the acquire itself timed out — \
                 no partial/double frame is possible"
            );

            drop(_holder);
        });
    }

    #[cfg(all(feature = "headless", not(feature = "desktop")))]
    #[test]
    fn real_state_bearing_delivery_does_not_dirty_the_graphs_oxigraph_store() {
        // FIX A end-to-end regression, one level below the primitive.
        // `read_only_lease_acquire_and_drop_does_not_dirty_a_clean_store`
        // (cell_durability) proves `declare_rdf_read_only` itself is safe;
        // it does NOT prove that the real `linearize_state_bearing_delivery`
        // call site still invokes it. A future edit that dropped the
        // declaration from this function (while leaving every other test in
        // this file untouched — none of them assert on durable dirty-state)
        // would go undetected without this. Drives the actual production
        // function against a real graph/store/flush, no mocks.
        use crate::graph_service::{create_graph_service, CreateGraphInput};

        let _serial = crate::tauri_runtime::profile_env_serial()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let profile =
            std::env::temp_dir().join(format!("garden-ws-linearize-dirty-{}", Uuid::new_v4()));
        std::env::set_var("GARDEN_PROFILE_DIR", &profile);
        let durable = std::env::temp_dir().join(format!(
            "garden-ws-linearize-dirty-durable-{}",
            Uuid::new_v4()
        ));

        let result = std::panic::catch_unwind(|| {
            crate::app_runtime::async_runtime::block_on(async {
                let app = crate::tauri_runtime::build_mock_app_for_tests(true);
                let graph_id = "ws-linearize-dirty";
                create_graph_service(
                    &app,
                    CreateGraphInput {
                        title: "WS linearize dirty-skip".to_string(),
                        graph_id: Some(graph_id.to_string()),
                        description: None,
                        operation_id: None,
                    },
                )
                .expect("create graph");
                let graph_dir =
                    crate::graph_paths::existing_graph_dir(&app, graph_id).expect("graph dir");

                // Register the graph's real Oxigraph store with the durable
                // flusher and get it to a clean baseline exactly like the
                // cell_durability FIX-A tests do.
                let store = crate::rdf_store_service::open_graph_store(&graph_dir)
                    .expect("open real graph store");
                let first = crate::cell_durability::flush(&profile, &durable)
                    .expect("first real-store flush");
                assert!(
                    first.published,
                    "first flush of a freshly-observed store must publish"
                );

                let registry = app.state::<RoomRegistry>();
                let room = registry
                    .get_or_create(
                        &format!("workspace:{graph_id}"),
                        crate::ydoc_paths::workspace_ydoc_state_path(&graph_dir),
                    )
                    .await
                    .expect("workspace room");

                // The real call site: SyncStep1 delivery, exactly as
                // `serve_room` drives it on doc-open. Pure Y.Doc read + WS
                // send in production; here a no-op stand-in for the send.
                linearize_state_bearing_delivery(&app, graph_id, &room, async { Ok(()) })
                    .await
                    .expect("state-bearing delivery");

                let second = crate::cell_durability::flush(&profile, &durable)
                    .expect("second real-store flush");
                assert!(
                    !second.published,
                    "a real linearize_state_bearing_delivery call must not dirty the \
                     graph's Oxigraph store — it only reads the Y.Doc and writes to \
                     the websocket"
                );
                assert_eq!(second.stores_backed_up, 0);

                crate::rdf_store_service::evict_graph_store(&graph_dir).expect("evict graph store");
                drop(store);
            })
        });

        std::env::remove_var("GARDEN_PROFILE_DIR");
        let _ = std::fs::remove_dir_all(&profile);
        let _ = std::fs::remove_dir_all(&durable);
        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
    }

    #[test]
    fn awareness_echo_is_skipped_only_for_the_senders_own_ids() {
        crate::app_runtime::async_runtime::block_on(async {
            let room = presence_test_room("awareness-own-only").await;
            let (_, own_only) = room
                .merge_awareness_tagged(
                    &awareness_update(vec![(41, 1, presence_json("vera", "device-a", "tab-a", 1))]),
                    1,
                )
                .await
                .expect("first incarnation accepted");
            assert!(own_only, "a socket's own live id needs no echo");

            // A newer incarnation of the same tab, on another socket, supersedes
            // client 41: the synthetic removal rides along and the sender must
            // still hear it, so the frame is not tagged as the sender's own.
            let (accepted, own_only) = room
                .merge_awareness_tagged(
                    &awareness_update(vec![(42, 1, presence_json("vera", "device-a", "tab-a", 2))]),
                    2,
                )
                .await
                .expect("newer incarnation accepted");
            assert!(accepted.clients.contains_key(&ClientID::new(41)));
            assert!(
                !own_only,
                "removal of a superseded id is news to the sender"
            );
        });
    }

    #[test]
    fn client_frame_is_published_tagged_with_its_origin() {
        crate::app_runtime::async_runtime::block_on(async {
            let room = presence_test_room("frame-origin").await;
            let app = crate::tauri_runtime::build_mock_app_for_tests(true);
            let mut frames = room.frames.subscribe();
            let remote = Doc::new();
            {
                let mut txn = remote.transact_mut();
                txn.get_or_insert_map("metadata")
                    .insert(&mut txn, "typed", true);
            }
            let update = remote
                .transact()
                .encode_state_as_update_v1(&StateVector::default());
            assert!(room
                .apply_client_frame(&update, app.clone(), Some(7))
                .await
                .expect("client frame"));
            let frame = frames.try_recv().expect("the frame is broadcast");
            assert_eq!(frame.origin, Some(7), "the author's loop will skip it");
            assert_eq!(frame.kind, FrameKind::State);

            room.update_doc(|_doc, txn| {
                txn.get_or_insert_map("metadata").insert(txn, "api", true);
                Ok(())
            })
            .await
            .expect("server write");
            let frame = frames.try_recv().expect("the server write is broadcast");
            assert_eq!(frame.origin, None, "an API write is nobody's echo");
        });
    }

    #[test]
    fn broadcast_delivery_does_not_read_the_graph_record() {
        crate::app_runtime::async_runtime::block_on(async {
            let app = crate::tauri_runtime::build_mock_app_for_tests(true);
            let room = presence_test_room("broadcast-no-record").await;
            // No graph.json exists for this id. The first-delivery path still
            // refuses a vanished graph; the per-frame path no longer asks.
            let graph_id = "graph-without-a-record";
            assert!(
                linearize_state_bearing_delivery(&app, graph_id, &room, async { Ok(()) })
                    .await
                    .is_err()
            );
            broadcast_state_delivery(&app, graph_id, &room, async { Ok(()) })
                .await
                .expect("broadcast delivery rests on the lease and the eviction flag");
            room.evict();
            let error = broadcast_state_delivery(&app, graph_id, &room, async { Ok(()) })
                .await
                .expect_err("an evicted room delivers nothing");
            assert!(error.contains("evicted"), "{error}");
        });
    }

    fn typed_update(key: &str) -> Vec<u8> {
        let remote = Doc::new();
        {
            let mut txn = remote.transact_mut();
            txn.get_or_insert_map("metadata")
                .insert(&mut txn, key, true);
        }
        let update = remote
            .transact()
            .encode_state_as_update_v1(&StateVector::default());
        update
    }

    /// Whether the sidecar on disk holds `metadata.<key>`.
    fn sidecar_has(room: &Room, key: &str) -> bool {
        let Ok(bytes) = std::fs::read(&room.state_path) else {
            return false;
        };
        let doc = Doc::new();
        let update = Update::decode_v1(&bytes).expect("sidecar decodes");
        doc.transact_mut().apply_update(update).expect("apply");
        let txn = doc.transact();
        txn.get_map("metadata")
            .is_some_and(|map| map.get(&txn, key).is_some())
    }

    async fn deferred_room(label: &str) -> (Arc<Room>, AppHandle) {
        let app = crate::tauri_runtime::build_mock_app_for_tests(true);
        let room = presence_test_room(label).await;
        room.bind_persist_scope(&app, "deferred-graph");
        (room, app)
    }

    #[test]
    fn a_dirty_room_is_persisted_within_the_window() {
        crate::app_runtime::async_runtime::block_on(async {
            let (room, app) = deferred_room("group-commit").await;
            for key in ["a", "b", "c"] {
                assert!(room
                    .apply_client_frame(&typed_update(key), app.clone(), Some(1))
                    .await
                    .expect("client frame"));
            }
            // Broadcast already happened; the sidecar lags until the window.
            assert!(room.state_is_dirty());
            assert!(!sidecar_has(&room, "a"), "persistence is deferred");
            tokio::time::sleep(ROOM_PERSIST_WINDOW * 3).await;
            assert!(!room.state_is_dirty(), "the group commit ran");
            for key in ["a", "b", "c"] {
                assert!(sidecar_has(&room, key), "{key} reached the sidecar");
            }
            assert!(
                !room.persist_armed.load(Ordering::SeqCst),
                "the commit task has finished"
            );
        });
    }

    #[test]
    fn a_change_after_a_commit_arms_the_next_commit() {
        crate::app_runtime::async_runtime::block_on(async {
            let (room, app) = deferred_room("group-commit-twice").await;
            room.apply_client_frame(&typed_update("first"), app.clone(), Some(1))
                .await
                .expect("first");
            tokio::time::sleep(ROOM_PERSIST_WINDOW * 3).await;
            assert!(sidecar_has(&room, "first"));
            room.apply_client_frame(&typed_update("second"), app.clone(), Some(1))
                .await
                .expect("second");
            tokio::time::sleep(ROOM_PERSIST_WINDOW * 3).await;
            assert!(sidecar_has(&room, "second"), "no change is left unarmed");
        });
    }

    #[test]
    fn closing_flushes_a_dirty_room_at_once_and_shutdown_flushes_every_room() {
        crate::app_runtime::async_runtime::block_on(async {
            let (room, app) = deferred_room("close-flush").await;
            room.apply_client_frame(&typed_update("tail"), app.clone(), Some(1))
                .await
                .expect("frame");
            assert!(!sidecar_has(&room, "tail"));
            assert!(room
                .flush_state_under_lifecycle(true)
                .await
                .expect("close flush"));
            assert!(sidecar_has(&room, "tail"), "flushed without waiting");
            assert!(!room.state_is_dirty());
            assert!(
                !room
                    .flush_state_under_lifecycle(true)
                    .await
                    .expect("clean flush"),
                "a clean room writes nothing"
            );

            // Registry-wide shutdown flush over two dirty rooms.
            let registry = RoomRegistry::default();
            let root =
                std::env::temp_dir().join(format!("garden-room-shutdown-{}", Uuid::new_v4()));
            let mut rooms = Vec::new();
            for name in ["one", "two"] {
                let room = registry
                    .get_or_create(
                        &format!("doc:deferred-graph:{name}"),
                        root.join(name).join("update-v1.bin"),
                    )
                    .await
                    .expect("room");
                room.bind_persist_scope(&app, "deferred-graph");
                room.apply_client_frame(&typed_update(name), app.clone(), Some(1))
                    .await
                    .expect("frame");
                rooms.push((name, room));
            }
            assert_eq!(registry.flush_dirty_rooms().await, 2);
            for (name, room) in &rooms {
                assert!(sidecar_has(room, name), "{name} was flushed at shutdown");
            }
            assert_eq!(registry.flush_dirty_rooms().await, 0);
        });
    }

    #[test]
    fn a_blocking_flush_writes_the_tail_for_a_restore_point() {
        crate::app_runtime::async_runtime::block_on(async {
            let (room, app) = deferred_room("blocking-flush").await;
            room.apply_client_frame(&typed_update("tail"), app.clone(), Some(1))
                .await
                .expect("frame");
            let flusher = room.clone();
            crate::app_runtime::async_runtime::spawn_blocking(move || {
                flusher.flush_state_blocking()
            })
            .await
            .expect("join")
            .expect("blocking flush");
            assert!(sidecar_has(&room, "tail"));
            assert!(!room.state_is_dirty());
        });
    }

    #[test]
    fn an_evicted_room_is_never_written_by_the_group_commit() {
        crate::app_runtime::async_runtime::block_on(async {
            let (room, app) = deferred_room("evicted-no-write").await;
            room.apply_client_frame(&typed_update("lost"), app.clone(), Some(1))
                .await
                .expect("frame");
            room.evict();
            tokio::time::sleep(ROOM_PERSIST_WINDOW * 3).await;
            assert!(
                !room.state_path.exists(),
                "a deleted graph's room must not recreate its sidecar"
            );
        });
    }

    #[test]
    fn a_failed_group_commit_evicts_the_room() {
        crate::app_runtime::async_runtime::block_on(async {
            let (room, app) = deferred_room("failed-commit").await;
            // A directory where the sidecar file belongs makes the rename fail.
            std::fs::create_dir_all(&room.state_path).expect("block the sidecar path");
            room.apply_client_frame(&typed_update("x"), app.clone(), Some(1))
                .await
                .expect("the frame itself is accepted and broadcast");
            assert!(!room.is_evicted());
            tokio::time::sleep(ROOM_PERSIST_WINDOW * 3).await;
            assert!(
                room.is_evicted(),
                "the room no longer matches the disk; clients re-push on reconnect"
            );
        });
    }

    #[test]
    fn api_writes_still_persist_before_they_return_on_a_bound_room() {
        crate::app_runtime::async_runtime::block_on(async {
            let (room, _app) = deferred_room("api-inline").await;
            room.update_doc(|_doc, txn| {
                txn.get_or_insert_map("metadata").insert(txn, "api", true);
                Ok(())
            })
            .await
            .expect("api write");
            assert!(sidecar_has(&room, "api"), "acknowledged means on disk");
            assert!(!room.state_is_dirty());
        });
    }

    #[test]
    fn a_slow_open_of_one_room_does_not_block_another_rooms_lookup() {
        let registry = Arc::new(RoomRegistry::default());
        let root = std::env::temp_dir().join(format!("garden-room-slow-open-{}", Uuid::new_v4()));
        let slow_key = "doc:slow-graph:slow-document";
        let slow_path = root.join("slow/update-v1.bin");
        let (reached_tx, reached_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        install_open_decode_hook(slow_path.clone(), reached_tx, release_rx);

        let opener_registry = registry.clone();
        let opener_path = slow_path.clone();
        let opener = std::thread::spawn(move || {
            crate::app_runtime::async_runtime::block_on(async {
                opener_registry.get_or_create(slow_key, opener_path).await
            })
        });
        reached_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("the slow open is parked inside its read and decode");

        // Everything below used to wait on the opener's registry mutex. It
        // runs on its own thread so a regression fails instead of hanging.
        let other_registry = registry.clone();
        let other_root = root.clone();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let outcome = crate::app_runtime::async_runtime::block_on(async {
                assert!(other_registry.peek("doc:other-graph:other").await.is_none());
                assert!(other_registry
                    .existing_room("doc:other-graph:other")
                    .unwrap()
                    .is_none());
                let other = other_registry
                    .get_or_create(
                        "doc:other-graph:other",
                        other_root.join("other/update-v1.bin"),
                    )
                    .await
                    .expect("another graph's room opens meanwhile");
                assert_eq!(other_registry.evict_graph("unrelated-graph"), 0);
                assert!(
                    other_registry
                        .rooms_with_prefix("doc:other-graph:")
                        .await
                        .len()
                        == 1
                );
                other
            });
            let _ = done_tx.send(outcome);
        });
        let other = done_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("another room's lookup and open are not blocked by the slow open");

        release_tx.send(()).expect("release the slow opener");
        let slow = opener
            .join()
            .expect("opener thread")
            .expect("the slow open completes");
        assert!(!Arc::ptr_eq(&slow, &other));
        crate::app_runtime::async_runtime::block_on(async {
            assert!(Arc::ptr_eq(
                &registry.peek(slow_key).await.expect("slow room registered"),
                &slow
            ));
        });
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn concurrent_openers_of_one_key_share_one_room() {
        let registry = Arc::new(RoomRegistry::default());
        let root = std::env::temp_dir().join(format!("garden-room-one-key-{}", Uuid::new_v4()));
        let path = root.join("update-v1.bin");
        let handles: Vec<_> = (0..4)
            .map(|_| {
                let registry = registry.clone();
                let path = path.clone();
                std::thread::spawn(move || {
                    crate::app_runtime::async_runtime::block_on(async {
                        registry.get_or_create("doc:one-key:document", path).await
                    })
                    .expect("open")
                })
            })
            .collect();
        let rooms: Vec<_> = handles
            .into_iter()
            .map(|handle| handle.join().expect("opener"))
            .collect();
        for room in &rooms[1..] {
            assert!(Arc::ptr_eq(&rooms[0], room), "one key, one room");
        }
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn room_sidecar_directory_sync_is_bounded_to_one_per_interval() {
        let cadence = DirSyncCadence::default();
        let t0 = Instant::now();
        assert!(
            cadence.due_at(t0, false, false),
            "a new name is always synced"
        );
        assert!(!cadence.due_at(t0 + Duration::from_millis(100), true, false));
        assert!(cadence.pending.load(Ordering::SeqCst));
        assert!(!cadence.due_at(t0 + Duration::from_millis(900), true, false));
        assert!(cadence.due_at(t0 + ROOM_DIR_SYNC_INTERVAL, true, false));
        assert!(!cadence.pending.load(Ordering::SeqCst));
        assert!(
            cadence.due_at(
                t0 + ROOM_DIR_SYNC_INTERVAL + Duration::from_millis(1),
                true,
                true
            ),
            "a forced sync (close, shutdown) is always due"
        );
    }

    #[test]
    fn skipped_directory_sync_is_settled_and_the_state_is_whole() {
        crate::app_runtime::async_runtime::block_on(async {
            let room = presence_test_room("dir-sync-settle").await;
            room.update_doc(|_doc, txn| {
                txn.get_or_insert_map("metadata").insert(txn, "first", true);
                Ok(())
            })
            .await
            .expect("first write creates the sidecar and syncs its new name");
            assert!(!room.dir_sync.pending.load(Ordering::SeqCst));

            // Pretend the directory was synced just now: the next rename skips it.
            room.dir_sync
                .settled_at(Instant::now() + Duration::from_secs(3600));
            room.update_doc(|_doc, txn| {
                txn.get_or_insert_map("metadata")
                    .insert(txn, "second", true);
                Ok(())
            })
            .await
            .expect("second write replaces the sidecar by rename");
            assert!(
                room.dir_sync.pending.load(Ordering::SeqCst),
                "the rename skipped its directory sync"
            );

            room.settle_state_dir().await;
            assert!(!room.dir_sync.pending.load(Ordering::SeqCst));

            // The skipped sync never costs integrity: the sidecar is the whole
            // state, and no temp file is left behind.
            let reopened = RoomRegistry::default()
                .get_or_create("doc:dir-sync:settle", room.state_path.clone())
                .await
                .expect("reopen from the sidecar");
            reopened
                .with_doc(|doc| {
                    let txn = doc.transact();
                    let metadata = txn.get_map("metadata").expect("metadata map");
                    assert!(metadata.contains_key(&txn, "first"));
                    assert!(metadata.contains_key(&txn, "second"));
                })
                .await;
            let leftovers = std::fs::read_dir(room.state_path.parent().unwrap())
                .unwrap()
                .filter_map(Result::ok)
                .filter(|entry| entry.file_name().to_string_lossy().contains(".tmp-"))
                .count();
            assert_eq!(leftovers, 0);
        });
    }

    #[test]
    fn continuous_typing_is_flushed_by_the_max_wait() {
        let debounce = Duration::from_millis(600);
        let max_wait = PROJECTION_DEBOUNCE_MAX_WAIT;
        let first = Instant::now();
        let ms = Duration::from_millis;

        // A pause: due the debounce after the latest update.
        assert_eq!(
            debounce_remaining(first, first + ms(100), first + ms(100), debounce, max_wait),
            debounce
        );
        // Keys keep coming, so the debounce keeps moving out, but never past
        // the max wait counted from the first update of the burst.
        let near_cap = first + max_wait - ms(50);
        assert_eq!(
            debounce_remaining(first, near_cap, near_cap, debounce, max_wait),
            ms(50)
        );
        assert_eq!(
            debounce_remaining(
                first,
                first + max_wait + ms(900),
                first + max_wait,
                debounce,
                max_wait
            ),
            Duration::ZERO,
            "continuous typing is flushed once the max wait has elapsed"
        );
        // Overdue never goes negative.
        assert_eq!(
            debounce_remaining(first, first, first + max_wait * 3, debounce, max_wait),
            Duration::ZERO
        );
    }

    #[test]
    fn one_timer_task_serves_a_whole_burst_of_frames() {
        crate::app_runtime::async_runtime::block_on(async {
            let room = presence_test_room("one-timer-per-burst").await;
            room.configure_projection_flush("graph".to_string(), Some("doc".to_string()))
                .expect("configure projection target");
            let target = room.projection_flush.get().expect("target").clone();

            let start = Instant::now();
            assert!(
                target.register_update(start),
                "the first frame of a burst starts the timer task"
            );
            for frame in 1..200 {
                assert!(
                    !target.register_update(start + Duration::from_millis(frame)),
                    "frame {frame} must not spawn another task"
                );
            }
            {
                let timer = target.timer.lock().unwrap();
                assert_eq!(timer.first, Some(start));
                assert_eq!(timer.last, Some(start + Duration::from_millis(199)));
            }

            // The task flushed and found nothing newer: it disarms, and the
            // next burst starts a fresh one.
            {
                let mut timer = target.timer.lock().unwrap();
                timer.first = None;
                timer.last = None;
                timer.armed = false;
            }
            assert!(target.register_update(start + Duration::from_secs(1)));
        });
    }

    #[test]
    fn the_last_connection_to_close_schedules_the_projection_repair() {
        crate::app_runtime::async_runtime::block_on(async {
            let app = crate::tauri_runtime::build_mock_app_for_tests(true);
            let dir = std::env::temp_dir().join(format!("garden-room-repair-{}", Uuid::new_v4()));
            let state_path = dir.join("update-v1.bin");
            let first = RoomRegistry::default()
                .get_or_create("doc:repair-graph:repair-doc", state_path.clone())
                .await
                .expect("create room");
            first
                .update_doc(|_doc, txn| {
                    txn.get_or_insert_map("metadata").insert(txn, "seed", true);
                    Ok(())
                })
                .await
                .expect("persist a sidecar to hydrate from");

            // Reopened from the sidecar the room is dirty: the cold files may be stale.
            let room = RoomRegistry::default()
                .get_or_create("doc:repair-graph:repair-doc", state_path)
                .await
                .expect("hydrate room");
            room.configure_projection_flush(
                "repair-graph".to_string(),
                Some("repair-doc".to_string()),
            )
            .expect("configure projection target");
            assert!(room.needs_projection_flush());
            let target = room.projection_flush.get().expect("target").clone();

            let first_socket = ConnectionGuard::new(&room, &app);
            let second_socket = ConnectionGuard::new(&room, &app);
            assert!(
                target.timer.lock().unwrap().first.is_none(),
                "opening a room schedules no projection"
            );
            drop(first_socket);
            assert!(
                target.timer.lock().unwrap().first.is_none(),
                "a socket closing while another is open schedules nothing"
            );
            drop(second_socket);
            assert!(
                target.timer.lock().unwrap().first.is_some(),
                "the last socket closing schedules the repair"
            );
            room.evict();
            let _ = std::fs::remove_dir_all(dir);
        });
    }
}
