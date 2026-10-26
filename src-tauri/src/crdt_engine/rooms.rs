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

use crate::app_error::PersistenceError;
use crate::app_runtime::AppHandle;
use crate::storage_atomic::{atomic_temp_path, sync_parent_dir, write_bytes_atomic_with};
use axum::extract::ws::{Message as WsMessage, WebSocket};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::sync::{OnceLock, Weak};
use std::time::{Duration, Instant};
#[cfg(feature = "desktop")]
use tauri::Manager;
use tokio::sync::{broadcast, Mutex, Notify};
use tracing::{instrument::WithSubscriber, Instrument};
use yrs::block::ClientID;
use yrs::sync::awareness::AwarenessUpdateEntry;
use yrs::sync::{AwarenessUpdate, Message, SyncMessage};
use yrs::updates::decoder::Decode;
use yrs::updates::encoder::Encode;
use yrs::{Doc, ReadTxn, Transact, Update};

pub struct Room {
    doc: Mutex<Doc>,
    awareness: Mutex<HashMap<ClientID, RoomAwarenessEntry>>,
    presence_sessions: Mutex<HashMap<PresenceSessionKey, PresenceSessionLease>>,
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
    #[cfg(not(feature = "frontend-crdt"))]
    projection_flush: OnceLock<Arc<ProjectionFlushTarget>>,
}

type ConnectionId = u64;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FrameKind {
    /// A Y.Doc update: graph content, so delivery is linearized with deletion.
    State,
    /// Ephemeral presence: no graph content, so delivery takes no lease.
    Awareness,
}

#[derive(Clone)]
struct RoomFrame {
    /// The socket whose frame this is, when that socket already has it. Its
    /// own loop skips it: y-websocket clients do not need their own echo.
    origin: Option<ConnectionId>,
    kind: FrameKind,
    payload: Vec<u8>,
}

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

#[derive(Clone)]
struct RoomAwarenessEntry {
    entry: AwarenessUpdateEntry,
    /// The websocket currently responsible for a live state. Tombstones keep
    /// only the protocol clock, so they are never replayed as a collaborator.
    owner: Option<ConnectionId>,
    session: Option<PresenceSession>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PresenceSession {
    key: PresenceSessionKey,
    connection_epoch: u64,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct PresenceSessionKey {
    human_id: String,
    device_id: String,
    client_id: String,
}

/// High-water state for one logical browser tab while any socket that has
/// participated in that tab's lifecycle is still connected. Keeping each
/// owner's own epoch prevents an older socket from claiming a newer epoch
/// during the small gap between replacement removal and transport teardown.
struct PresenceSessionLease {
    high_water_epoch: u64,
    owner_epochs: HashMap<ConnectionId, u64>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PresenceEnvelope {
    presence: Option<PresenceWireIdentity>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PresenceWireIdentity {
    human_id: String,
    device_id: String,
    client_id: String,
    connection_epoch: u64,
}

fn presence_session(entry: &AwarenessUpdateEntry) -> Option<PresenceSession> {
    let envelope: PresenceEnvelope = serde_json::from_str(entry.json.as_ref()).ok()?;
    let identity = envelope.presence?;
    if identity.human_id.trim().is_empty()
        || identity.device_id.trim().is_empty()
        || identity.client_id.trim().is_empty()
    {
        return None;
    }
    Some(PresenceSession {
        key: PresenceSessionKey {
            human_id: identity.human_id,
            device_id: identity.device_id,
            client_id: identity.client_id,
        },
        connection_epoch: identity.connection_epoch,
    })
}

fn is_awareness_removal(entry: &AwarenessUpdateEntry) -> bool {
    entry.json.trim() == "null"
}

#[cfg(not(feature = "frontend-crdt"))]
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
#[cfg(not(feature = "frontend-crdt"))]
#[derive(Default)]
struct DebounceTimer {
    /// A timer task owns the pending burst.
    armed: bool,
    first: Option<Instant>,
    last: Option<Instant>,
}

/// How long until the pending burst is due: the debounce after the latest
/// update, capped at `max_wait` after the first. Zero means flush now.
#[cfg(not(feature = "frontend-crdt"))]
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

#[cfg(not(feature = "frontend-crdt"))]
#[derive(Default)]
struct FlushContexts {
    updates: Vec<crate::telemetry::TraceContext>,
    dropped: u64,
}

#[cfg(not(feature = "frontend-crdt"))]
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

#[cfg(not(feature = "frontend-crdt"))]
const WORKSPACE_PROJECTION_DEBOUNCE: Duration = Duration::from_millis(250);
#[cfg(not(feature = "frontend-crdt"))]
const DOCUMENT_PROJECTION_DEBOUNCE: Duration = Duration::from_millis(600);
/// Continuous typing postpones the debounce for as long as keys keep coming;
/// the projection (cold record, RDF, search) still lands at least this often.
#[cfg(not(feature = "frontend-crdt"))]
const PROJECTION_DEBOUNCE_MAX_WAIT: Duration = Duration::from_secs(5);
#[cfg(not(feature = "frontend-crdt"))]
const PROJECTION_RETRY_INITIAL: Duration = Duration::from_millis(100);
#[cfg(not(feature = "frontend-crdt"))]
const PROJECTION_RETRY_MAX: Duration = Duration::from_secs(5);
#[cfg(not(feature = "frontend-crdt"))]
const PROJECTION_FLUSH_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(5);

#[cfg(not(feature = "frontend-crdt"))]
enum ProjectionFlushFailure {
    Terminal(String),
    Retryable(String),
}

#[cfg(not(feature = "frontend-crdt"))]
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
    rooms: StdMutex<HashMap<String, Arc<Room>>>,
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

fn room_key_belongs_to_graph(key: &str, graph_id: &str) -> bool {
    key == format!("workspace:{graph_id}") || key.starts_with(&format!("doc:{graph_id}:"))
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

    /// Non-healing, fallible occupancy read. Do not turn a poisoned registry
    /// into absence when deciding whether a first creation is admissible.
    pub(crate) fn existing_room(&self, key: &str) -> Result<Option<Arc<Room>>, String> {
        let rooms = self
            .rooms
            .lock()
            .map_err(|_| "room registry lock poisoned")?;
        Ok(rooms.get(key).cloned())
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
        crate::document_body_availability::require_state_path_available(&state_path)
            .map_err(PersistenceError::permanent)?;
        let observed_tombstone =
            crate::document_tombstone_store::tombstone_for_ydoc_state_path(&state_path)?;
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
                .map_err(|_| "restore room admission lock poisoned")?;
            if restores
                .iter()
                .any(|graph_id| room_key_belongs_to_graph(key, graph_id))
            {
                return Err("graph room is unavailable during disk archive restore".into());
            }
            let waiting = crate::telemetry::timer();
            let rooms = self
                .rooms
                .lock()
                .map_err(|_| "room registry lock poisoned".to_string())?;
            lock_wait_ms += crate::telemetry::elapsed_ms(waiting);
            if let Some(room) = rooms.get(key).cloned() {
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
        let room = Arc::new(Room {
            doc: Mutex::new(doc),
            awareness: Mutex::new(HashMap::new()),
            presence_sessions: Mutex::new(HashMap::new()),
            tx,
            frames,
            next_connection_id: AtomicU64::new(1),
            connections: AtomicUsize::new(0),
            state_path: state_path.clone(),
            dir_sync: Arc::new(DirSyncCadence::default()),
            // Conservatively materialize a hydrated room once. Its hot-tier
            // authority may be newer than (or have outlived) the cold files.
            projection_epoch: AtomicU64::new(u64::from(hydrated)),
            persisted_projection_epoch: AtomicU64::new(0),
            projection_flush_lock: Mutex::new(()),
            authority_lock: Mutex::new(()),
            evicted: AtomicBool::new(false),
            eviction_notify: Notify::new(),
            #[cfg(not(feature = "frontend-crdt"))]
            projection_flush: OnceLock::new(),
        });

        // Phase 3: admission again (a disk restore may have begun while this
        // opener was decoding), then swap the entry in.
        {
            let restores = self
                .disk_restores
                .lock()
                .map_err(|_| "restore room admission lock poisoned")?;
            if restores
                .iter()
                .any(|graph_id| room_key_belongs_to_graph(key, graph_id))
            {
                return Err("graph room is unavailable during disk archive restore".into());
            }
            let waiting = crate::telemetry::timer();
            let mut rooms = self
                .rooms
                .lock()
                .map_err(|_| "room registry lock poisoned".to_string())?;
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
            rooms.insert(key.to_string(), room.clone());
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
                    return Err(error.into());
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
        self.rooms
            .lock()
            .ok()?
            .get(key)
            .filter(|room| !room.is_evicted())
            .cloned()
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
                let is_same = rooms
                    .get(key)
                    .is_some_and(|current| Arc::ptr_eq(current, expected));
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
            if rooms.get(key).is_some_and(|room| {
                Arc::ptr_eq(room, expected)
                    && Arc::strong_count(room) == 2
                    && !room.needs_projection_flush()
            }) {
                if let Some(room) = rooms.remove(key) {
                    room.evict();
                }
            }
        }
    }

    /// Snapshot hosted rooms whose keys start with `prefix`. Sorting makes a
    /// graph-wide flush deterministic and avoids holding the registry lock
    /// while filesystem/RDF persistence runs.
    pub(crate) async fn rooms_with_prefix(&self, prefix: &str) -> Vec<(String, Arc<Room>)> {
        let Ok(rooms) = self.rooms.lock() else {
            return Vec::new();
        };
        let mut matches: Vec<_> = rooms
            .iter()
            .filter(|(key, room)| key.starts_with(prefix) && !room.is_evicted())
            .map(|(key, room)| (key.clone(), room.clone()))
            .collect();
        matches.sort_by(|(left, _), (right, _)| left.cmp(right));
        matches
    }

    /// Remove every hosted room for a graph and synchronously cancel its
    /// debounce/retry generation. Existing websocket tasks may still hold a
    /// room Arc briefly, but an evicted room rejects further client updates and
    /// cannot enqueue another cold projection.
    pub(crate) fn evict_graph(&self, graph_id: &str) -> usize {
        let workspace_key = format!("workspace:{graph_id}");
        let document_prefix = format!("doc:{graph_id}:");
        let removed = {
            let Ok(mut rooms) = self.rooms.lock() else {
                log::error!("room registry lock poisoned while evicting graph {graph_id}");
                return 0;
            };
            let keys = rooms
                .keys()
                .filter(|key| **key == workspace_key || key.starts_with(&document_prefix))
                .cloned()
                .collect::<Vec<_>>();
            keys.into_iter()
                .filter_map(|key| rooms.remove(&key))
                .collect::<Vec<_>>()
        };
        for room in &removed {
            room.evict();
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
        #[cfg(feature = "frontend-crdt")]
        {
            let _ = (graph_id, document_id);
            Ok(())
        }
        #[cfg(not(feature = "frontend-crdt"))]
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
        #[cfg(not(feature = "frontend-crdt"))]
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
        #[cfg(feature = "frontend-crdt")]
        let _ = app;
    }

    fn evict(&self) {
        if self.evicted.swap(true, Ordering::SeqCst) {
            return;
        }
        #[cfg(not(feature = "frontend-crdt"))]
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

    /// Merge one websocket's awareness update into the room's live state.
    ///
    /// Awareness is ephemeral connection state, not durable document state:
    /// every live entry has an owning socket. JSON `null` becomes an invisible
    /// clock tombstone, never something late joiners replay; retaining that
    /// clock is what prevents delayed pre-close frames from resurrecting a
    /// removed collaborator. Shrubbery additionally
    /// supplies a stable human/device/tab key and a monotonically increasing
    /// connection epoch. A newer incarnation supersedes that exact tab only;
    /// two tabs belonging to the same human remain independently present.
    ///
    /// The returned update is the canonical subset safe to broadcast. It may
    /// include synthetic clock+1 removals for client IDs superseded by a newer
    /// incarnation. Stale or non-owning frames are omitted.
    /// The entries of `update` whose client id no other live socket holds
    /// (a visitor's awareness; `None` when nothing is left).
    async fn awareness_held_by_this_socket(
        &self,
        mut update: AwarenessUpdate,
        owner: ConnectionId,
    ) -> Option<AwarenessUpdate> {
        let state = self.awareness.lock().await;
        update.clients.retain(|client, _| {
            !state
                .get(client)
                .is_some_and(|existing| existing.owner.is_some_and(|holder| holder != owner))
        });
        (!update.clients.is_empty()).then_some(update)
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

    /// [`Self::merge_awareness`], plus whether every accepted entry belongs to
    /// `owner`'s own live client ids. Only then may the sender skip its echo:
    /// synthetic removals of superseded ids are news to the sender too.
    async fn merge_awareness_tagged(
        &self,
        update: &AwarenessUpdate,
        owner: ConnectionId,
    ) -> Option<(AwarenessUpdate, bool)> {
        let mut state = self.awareness.lock().await;
        let mut sessions = self.presence_sessions.lock().await;
        let mut accepted = HashMap::new();

        for (client, entry) in &update.clients {
            if is_awareness_removal(entry) {
                let existing = state.get(client).cloned();
                if let Some(current) = existing.as_ref() {
                    if current.entry.clock > entry.clock {
                        continue;
                    }
                    if current
                        .owner
                        .is_some_and(|current_owner| current_owner != owner)
                    {
                        // A delayed close from an old socket must never delete
                        // live state already transferred to its replacement.
                        continue;
                    }
                    if current.owner.is_none() && current.entry.clock >= entry.clock {
                        continue;
                    }
                }

                if let Some(session) = existing.and_then(|current| current.session) {
                    let owner_still_live = state.iter().any(|(other_client, live)| {
                        other_client != client
                            && live.owner == Some(owner)
                            && live
                                .session
                                .as_ref()
                                .is_some_and(|candidate| candidate.key == session.key)
                    });
                    if !owner_still_live {
                        let drop_lease = if let Some(lease) = sessions.get_mut(&session.key) {
                            lease.owner_epochs.remove(&owner);
                            lease.owner_epochs.is_empty()
                        } else {
                            false
                        };
                        if drop_lease {
                            sessions.remove(&session.key);
                        }
                    }
                }
                state.insert(
                    *client,
                    RoomAwarenessEntry {
                        entry: entry.clone(),
                        owner: None,
                        session: None,
                    },
                );
                accepted.insert(*client, entry.clone());
                continue;
            }

            // Awareness clocks are the protocol-level ordering for one Yjs
            // client ID. Validate this before mutating logical session state.
            if state
                .get(client)
                .is_some_and(|existing| existing.entry.clock >= entry.clock)
            {
                continue;
            }

            let incoming_session = presence_session(entry);
            if let Some(incoming) = incoming_session.as_ref() {
                if let Some(lease) = sessions.get(&incoming.key) {
                    if incoming.connection_epoch < lease.high_water_epoch {
                        continue;
                    }
                    if incoming.connection_epoch == lease.high_water_epoch {
                        let owner_is_current = lease
                            .owner_epochs
                            .get(&owner)
                            .is_some_and(|epoch| *epoch == incoming.connection_epoch);
                        let live_for_key = state.iter().find_map(|(other_client, existing)| {
                            existing.session.as_ref().and_then(|session| {
                                (session.key == incoming.key)
                                    .then_some((*other_client, existing.owner))
                            })
                        });
                        let live_is_compatible = match live_for_key {
                            Some((live_client, live_owner)) => {
                                live_client == *client && live_owner == Some(owner)
                            }
                            None => owner_is_current,
                        };
                        if !owner_is_current || !live_is_compatible {
                            // Equal epochs are heartbeats/republication by the
                            // already-known socket, never ownership transfer.
                            continue;
                        }
                    }
                }

                let superseded: Vec<_> = state
                    .iter()
                    .filter_map(|(other_client, existing)| {
                        existing.session.as_ref().and_then(|session| {
                            (session.key == incoming.key
                                && session.connection_epoch < incoming.connection_epoch)
                                .then_some((*other_client, existing.entry.clock))
                        })
                    })
                    .collect();
                for (other_client, clock) in superseded {
                    state.insert(
                        other_client,
                        RoomAwarenessEntry {
                            entry: AwarenessUpdateEntry {
                                clock: clock.saturating_add(1),
                                json: "null".into(),
                            },
                            owner: None,
                            session: None,
                        },
                    );
                    if other_client != *client {
                        accepted.insert(
                            other_client,
                            AwarenessUpdateEntry {
                                clock: clock.saturating_add(1),
                                json: "null".into(),
                            },
                        );
                    }
                }

                let lease =
                    sessions
                        .entry(incoming.key.clone())
                        .or_insert_with(|| PresenceSessionLease {
                            high_water_epoch: incoming.connection_epoch,
                            owner_epochs: HashMap::new(),
                        });
                lease.high_water_epoch = lease.high_water_epoch.max(incoming.connection_epoch);
                lease.owner_epochs.insert(owner, incoming.connection_epoch);
            }

            state.insert(
                *client,
                RoomAwarenessEntry {
                    entry: entry.clone(),
                    owner: Some(owner),
                    session: incoming_session,
                },
            );
            accepted.insert(*client, entry.clone());
        }

        if accepted.is_empty() {
            None
        } else {
            let own_only = accepted.keys().all(|client| {
                state
                    .get(client)
                    .is_some_and(|live| live.owner == Some(owner))
            });
            Some((AwarenessUpdate { clients: accepted }, own_only))
        }
    }

    async fn full_awareness(&self) -> Option<AwarenessUpdate> {
        let state = self.awareness.lock().await;
        let clients = state
            .iter()
            .filter_map(|(client, live)| {
                (!is_awareness_removal(&live.entry)).then_some((*client, live.entry.clone()))
            })
            .collect::<HashMap<_, _>>();
        (!clients.is_empty()).then_some(AwarenessUpdate { clients })
    }

    /// Remove every awareness entry still owned by one websocket. Ownership
    /// may already have moved to a newer incarnation; those entries survive.
    async fn remove_awareness_for_connection(
        &self,
        owner: ConnectionId,
    ) -> Option<AwarenessUpdate> {
        let mut state = self.awareness.lock().await;
        let mut sessions = self.presence_sessions.lock().await;
        let owned: Vec<_> = state
            .iter()
            .filter_map(|(client, live)| {
                (live.owner == Some(owner)).then_some((*client, live.entry.clock))
            })
            .collect();
        let mut clients = HashMap::with_capacity(owned.len());
        for (client, clock) in owned {
            let tombstone = AwarenessUpdateEntry {
                clock: clock.saturating_add(1),
                json: "null".into(),
            };
            state.insert(
                client,
                RoomAwarenessEntry {
                    entry: tombstone.clone(),
                    owner: None,
                    session: None,
                },
            );
            clients.insert(client, tombstone);
        }
        sessions.retain(|_, lease| {
            lease.owner_epochs.remove(&owner);
            !lease.owner_epochs.is_empty()
        });
        if clients.is_empty() {
            None
        } else {
            Some(AwarenessUpdate { clients })
        }
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
        let result = self.apply_client_update_inner(update, None).await;
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
        let changed = match self.apply_client_update_inner(update, origin).await {
            Ok(changed) => changed,
            Err(error) => {
                tracing::Span::current().record("otel.status_code", "ERROR");
                return Err(error);
            }
        };
        if changed {
            self.schedule_projection_flush(app);
        }
        Ok(changed)
    }

    async fn apply_client_update_inner(
        &self,
        update: &[u8],
        origin: Option<ConnectionId>,
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
            self.persist().await?;
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
        let _authority_guard = self.authority_lock.lock().await;
        let doc = self.doc.lock().await;
        let epoch = self.projection_epoch.load(Ordering::SeqCst);
        (epoch, read(&doc))
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

    /// Persist the full authoritative state. Called after applied updates;
    /// cheap for M1-scale docs (full-state write, atomic rename).
    async fn persist(&self) -> Result<(), String> {
        self.persist_classified().await.map_err(Into::into)
    }

    #[tracing::instrument(target = "gardend_otel", name = "ydoc.state.persist", skip_all,
        fields(sophia.ydoc.state_bytes = tracing::field::Empty))]
    async fn persist_classified(&self) -> Result<(), PersistenceError> {
        let bytes = async {
            let doc = self.doc.lock().await;
            let txn = doc.transact();
            txn.encode_state_as_update_v1(&yrs::StateVector::default())
        }
        .instrument(tracing::info_span!(target: "gardend_otel", "ydoc.state.encode"))
        .await;
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
                let sync_dir = cadence.due_at(Instant::now(), existed, false);
                Room::persist_state_bytes_with(&state_path, &bytes, sync_dir)
            }),
        )
        .await
        .map_err(|error| format!("room persist join: {error}"))?;
        if let Err(error) = persisted {
            self.evict();
            return Err(error);
        }
        Ok(())
    }

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
        write_bytes_atomic_with(state_path, &temp_path, bytes, None, sync_dir)
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

#[cfg(not(feature = "frontend-crdt"))]
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
#[cfg(not(feature = "frontend-crdt"))]
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

#[cfg(not(feature = "frontend-crdt"))]
enum ProjectionAttempt<T> {
    Completed(T),
    Evicted,
    TimedOut,
}

#[cfg(not(feature = "frontend-crdt"))]
async fn wait_for_room_delay(room: &Weak<Room>, delay: Duration) -> bool {
    let Some(room) = room.upgrade() else {
        return true;
    };
    tokio::select! {
        _ = tokio::time::sleep(delay) => false,
        _ = room.wait_until_evicted() => true,
    }
}

#[cfg(not(feature = "frontend-crdt"))]
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

#[cfg(not(feature = "frontend-crdt"))]
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

#[cfg(not(feature = "frontend-crdt"))]
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
    room: &Room,
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
    room: &Room,
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
    room: &Room,
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
    room: &Room,
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
                            RoomAccess::ReadOnly { .. } => match room.shadow_apply(&update, |_, _, _| Ok(())).await {
                                Ok(false) => {
                                    visitor_noop_frames += 1;
                                    if visitor_noop_frames <= VISITOR_NOOP_FRAMES {
                                        continue;
                                    }
                                    Some("too many frames that change nothing".to_string())
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
                }
            }
        }
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
            let state = room.awareness.lock().await;
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
            let state = room.awareness.lock().await;
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

    #[cfg(not(feature = "frontend-crdt"))]
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

    #[cfg(not(feature = "frontend-crdt"))]
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

    #[cfg(not(feature = "frontend-crdt"))]
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

    #[cfg(not(feature = "frontend-crdt"))]
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

    #[cfg(not(feature = "frontend-crdt"))]
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
