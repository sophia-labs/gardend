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

use crate::app_runtime::AppHandle;
use crate::storage_atomic::{atomic_temp_path, write_bytes_atomic};
use axum::extract::ws::{Message as WsMessage, WebSocket};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::sync::{OnceLock, Weak};
use std::time::Duration;
#[cfg(feature = "desktop")]
use tauri::Manager;
use tokio::sync::{broadcast, Mutex, Notify};
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
    tx: broadcast::Sender<Vec<u8>>,
    next_connection_id: AtomicU64,
    /// Where to persist the authoritative Y.Doc state (update-v1.bin).
    state_path: PathBuf,
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
}

#[cfg(not(feature = "frontend-crdt"))]
const WORKSPACE_PROJECTION_DEBOUNCE: Duration = Duration::from_millis(250);
#[cfg(not(feature = "frontend-crdt"))]
const DOCUMENT_PROJECTION_DEBOUNCE: Duration = Duration::from_millis(600);
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

enum TombstoneOpenPolicy<'a> {
    Reject,
    FreshRecreation(&'a str),
}

impl RoomRegistry {
    pub(crate) fn begin_disk_restore(&self, graph_id: &str) -> Result<DiskRestoreRoomGuard<'_>, String> {
        let mut restores = self.disk_restores.lock().map_err(|_| "restore room admission lock poisoned")?;
        let rooms = self.rooms.lock().map_err(|_| "room registry lock poisoned")?;
        if restores.contains(graph_id) || rooms.keys().any(|key| room_key_belongs_to_graph(key, graph_id)) {
            return Err(format!("graph.restoreArchive: target graph {graph_id} has live room authority or an active disk restore"));
        }
        restores.insert(graph_id.to_string());
        Ok(DiskRestoreRoomGuard { registry: self, graph_id: graph_id.to_string() })
    }

    /// Non-healing, fallible occupancy read. Do not turn a poisoned registry
    /// into absence when deciding whether a first creation is admissible.
    pub(crate) fn existing_room(&self, key: &str) -> Result<Option<Arc<Room>>, String> {
        let rooms = self.rooms.lock().map_err(|_| "room registry lock poisoned")?;
        Ok(rooms.get(key).cloned())
    }

    /// Get or create the room for `key`, hydrating the Doc from
    /// `state_path` if the file exists.
    pub async fn get_or_create(&self, key: &str, state_path: PathBuf) -> Result<Arc<Room>, String> {
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
    ) -> Result<Arc<Room>, String> {
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
    ) -> Result<Arc<Room>, String> {
        crate::document_body_availability::require_state_path_available(&state_path)?;
        let observed_tombstone =
            crate::document_tombstone_store::tombstone_for_ydoc_state_path(&state_path)?;
        match (&policy, observed_tombstone.as_ref()) {
            (TombstoneOpenPolicy::Reject, Some(tombstone)) => {
                return Err(format!("document tombstoned: {}", tombstone.document_id));
            }
            (TombstoneOpenPolicy::FreshRecreation(expected), Some(tombstone))
                if tombstone.deletion_id == *expected => {}
            (TombstoneOpenPolicy::FreshRecreation(_), Some(_)) => {
                return Err("document deletion boundary changed before recreation".to_string());
            }
            (TombstoneOpenPolicy::FreshRecreation(_), None) => {
                return Err("document tombstone missing before recreation".to_string());
            }
            (TombstoneOpenPolicy::Reject, None) => {}
        }
        pause_after_tombstone_precheck(&state_path);

        let restores = self.disk_restores.lock().map_err(|_| "restore room admission lock poisoned")?;
        if restores.iter().any(|graph_id| room_key_belongs_to_graph(key, graph_id)) {
            return Err("graph room is unavailable during disk archive restore".to_string());
        }
        let mut rooms = self
            .rooms
            .lock()
            .map_err(|_| "room registry lock poisoned".to_string())?;
        if let Some(room) = rooms.get(key).cloned() {
            if !room.is_evicted() && matches!(&policy, TombstoneOpenPolicy::Reject) {
                drop(rooms);
                drop(restores);
                // Pre-existing (cache-hit) room: a recheck error returns Err
                // but the live room stays — it was live before this call.
                return self.finish_tombstone_checked_open(key, &state_path, room, &policy, false);
            }
            // A hot-authority write failure invalidates the in-memory Doc.
            // Reopen from the last durable sidecar instead of returning that
            // partially-mutated incarnation to a new caller.
            if let Some(removed) = rooms.remove(key) {
                removed.evict();
            }
        }
        let doc = Doc::new();
        let mut hydrated = false;
        if matches!(&policy, TombstoneOpenPolicy::Reject) && state_path.exists() {
            let bytes = std::fs::read(&state_path)
                .map_err(|error| format!("read ydoc state {}: {error}", state_path.display()))?;
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
                    format!("decode ydoc state {}: {error}", state_path.display())
                })?;
                if !update.is_empty() {
                    let mut txn = doc.transact_mut();
                    txn.apply_update(update).map_err(|error| {
                        format!("hydrate ydoc {}: {error}", state_path.display())
                    })?;
                    hydrated = true;
                }
            }
        }
        let (tx, _) = broadcast::channel(256);
        let room = Arc::new(Room {
            doc: Mutex::new(doc),
            awareness: Mutex::new(HashMap::new()),
            presence_sessions: Mutex::new(HashMap::new()),
            tx,
            next_connection_id: AtomicU64::new(1),
            state_path: state_path.clone(),
            // Conservatively materialize a hydrated room once. Its hot-tier
            // authority may be newer than (or have outlived) the cold files.
            projection_epoch: AtomicU64::new(u64::from(hydrated)),
            persisted_projection_epoch: AtomicU64::new(0),
            projection_flush_lock: Mutex::new(()),
            evicted: AtomicBool::new(false),
            eviction_notify: Notify::new(),
            #[cfg(not(feature = "frontend-crdt"))]
            projection_flush: OnceLock::new(),
        });
        rooms.insert(key.to_string(), room.clone());
        drop(rooms);
        drop(restores);
        // Freshly inserted room: a recheck that cannot even be evaluated must
        // evict it (see below) — the caller gets an Err with no Arc, so
        // nothing else would ever evict the just-decoded room, and every
        // retry would then see it as "pre-live" and skip its own cleanup: a
        // permanent one-room leak per transient tombstone-store error. The
        // eviction is IDENTITY-checked so this cleanup can never destroy a
        // newer live room installed here in the no-lock window (test hook).
        pause_before_fresh_recheck(&state_path);
        self.finish_tombstone_checked_open(key, &state_path, room, &policy, true)
    }

    fn finish_tombstone_checked_open(
        &self,
        key: &str,
        state_path: &std::path::Path,
        room: Arc<Room>,
        policy: &TombstoneOpenPolicy<'_>,
        evict_on_error: bool,
    ) -> Result<Arc<Room>, String> {
        if let Err(error) = crate::document_body_availability::require_state_path_available(state_path) {
            self.evict_room_if_identity(key, &room);
            return Err(error);
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
                    return Err(error);
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
            Some(tombstone) => Err(format!("document tombstoned: {}", tombstone.document_id)),
            None => Err("document tombstone disappeared during recreation".to_string()),
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

    fn is_evicted(&self) -> bool {
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
    async fn merge_awareness(
        &self,
        update: &AwarenessUpdate,
        owner: ConnectionId,
    ) -> Option<AwarenessUpdate> {
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
            Some(AwarenessUpdate { clients: accepted })
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
        if self.is_evicted() {
            return Err("room was evicted".to_string());
        }
        let mutation = {
            let doc = self.doc.lock().await;
            let mut txn = doc.transact_mut();
            match mutate(&doc, &mut txn) {
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
        self.persist().await?;
        // An empty transaction still encodes a few framing bytes; only
        // broadcast when content actually changed.
        if update.len() > 3 {
            let message = Message::Sync(SyncMessage::Update(update)).encode_v1();
            let _ = self.tx.send(message);
        }
        Ok(result)
    }

    /// Apply a sync-client update through the same path used by the websocket
    /// host. Returns false for a valid update that did not change room state.
    pub(crate) async fn apply_client_update(&self, update: &[u8]) -> Result<bool, String> {
        if self.is_evicted() {
            return Err("room was evicted".to_string());
        }
        let applied = {
            let doc = self.doc.lock().await;
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
        if changed {
            self.persist().await?;
            let rebroadcast = Message::Sync(SyncMessage::Update(update.to_vec())).encode_v1();
            let _ = self.tx.send(rebroadcast);
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
        let doc = self.doc.lock().await;
        let epoch = self.projection_epoch.load(Ordering::SeqCst);
        (epoch, read(&doc))
    }

    pub(crate) fn needs_projection_flush(&self) -> bool {
        self.projection_epoch.load(Ordering::SeqCst)
            > self.persisted_projection_epoch.load(Ordering::SeqCst)
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
        let bytes = {
            let doc = self.doc.lock().await;
            let txn = doc.transact();
            txn.encode_state_as_update_v1(&yrs::StateVector::default())
        };
        // The commit section runs on a blocking thread: `write_guard()` takes
        // a blocking read of the flush gate, and parking a tokio worker on it
        // starves the runtime once a few persists stack up behind a flush —
        // `/health` included, which flaps the pod NotReady and 503s the whole
        // cell. The guard is `!Send` by design: acquired and released entirely
        // inside the closure, only owned data crosses in.
        let state_path = self.state_path.clone();
        let persisted = crate::app_runtime::async_runtime::spawn_blocking(move || {
            Room::persist_state_bytes_at(&state_path, &bytes)
        })
        .await
        .map_err(|error| format!("room persist join: {error}"))?;
        if let Err(error) = persisted {
            self.evict();
            return Err(error);
        }
        Ok(())
    }

    fn persist_state_bytes_at(state_path: &std::path::Path, bytes: &[u8]) -> Result<(), String> {
        crate::document_body_availability::require_state_path_available(state_path)?;
        // This synchronous hot-authority commit is a durability transaction.
        // The gate prevents an EFS snapshot from walking the profile while the
        // sidecar's parent/temp/final transition is in progress. The guard is
        // re-entrant for callers that already own a broader synchronous commit.
        let _durability_guard = crate::cell_durability::write_guard();
        if let Some(parent) = state_path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| format!("room persist mkdir {}: {error}", parent.display()))?;
        }
        let temp_path = atomic_temp_path(state_path);
        write_bytes_atomic(state_path, &temp_path, bytes, None)
            .map_err(|error| format!("room persist {}: {error}", state_path.display()))
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
        Room::persist_state_bytes_at(&self.state_path, bytes)
    }

    #[cfg(test)]
    pub(crate) fn subscribe_updates_for_test(&self) -> broadcast::Receiver<Vec<u8>> {
        self.tx.subscribe()
    }
}

#[cfg(not(feature = "frontend-crdt"))]
impl ProjectionFlushTarget {
    fn schedule(self: &Arc<Self>, room: Weak<Room>, app: AppHandle, graph_generation: u64) {
        let generation = self
            .generation
            .fetch_add(1, Ordering::SeqCst)
            .wrapping_add(1);
        let debounce = self.debounce;
        let target = Arc::downgrade(self);
        // A socket update can outlive the socket itself while it waits through
        // the projection debounce. Publish that pending work to the process
        // lifecycle before spawning so idle drain cannot overtake it.
        #[cfg(test)]
        let lifecycle_lease: Option<crate::cell_lifecycle::BackgroundLease> = None;
        #[cfg(not(test))]
        let lifecycle_lease = crate::cell_lifecycle::process_lifecycle()
            .and_then(|lifecycle| lifecycle.begin_background("projection-flush").ok());
        crate::app_runtime::async_runtime::spawn(async move {
            let _lifecycle_lease = lifecycle_lease;
            if wait_for_room_delay(&room, debounce).await {
                return;
            }
            // This task intentionally retains AppHandle while an older graph
            // operation owns the persistence lease. It exits once its flush is
            // queued, superseded, or evicted; runtime shutdown aborts spawned
            // tasks. A fixed total deadline would lose dirty state behind a
            // legitimate long-running import.
            run_projection_flush(
                target,
                room,
                app,
                generation,
                graph_generation,
                PROJECTION_FLUSH_ATTEMPT_TIMEOUT,
            )
            .await;
        });
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
    let mut retry_delay = PROJECTION_RETRY_INITIAL;
    let mut attempt = 0_u64;
    'retry: loop {
        let Some(room_ref) = room.upgrade() else {
            return;
        };
        if room_ref.is_evicted() {
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
async fn linearize_state_bearing_delivery<F>(
    app: &AppHandle,
    graph_id: &str,
    room: &Room,
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
    let _lease = tokio::time::timeout(
        WS_STATE_SEND_TIMEOUT,
        coordinator.acquire_lifecycle_shared(graph_id),
    )
    .await
    .map_err(|_| "graph persistence lease acquire timed out".to_string())??;
    crate::graph_record_store::read_graph_record_no_heal(app, graph_id)
        .map_err(crate::app_error::AppError::message)?;
    if room.is_evicted() {
        return Err("room was evicted".to_string());
    }
    tokio::time::timeout(WS_STATE_SEND_TIMEOUT, delivery)
        .await
        .map_err(|_| "websocket state delivery timed out".to_string())?
}

/// Drive one client connection through the room (y-websocket protocol).
pub async fn serve_room(socket: WebSocket, room: Arc<Room>, app: AppHandle, graph_id: String) {
    let connection_id = room.allocate_connection_id();
    let (mut sink, mut stream) = socket.split();
    let mut rx = room.tx.subscribe();

    // Server-initiated SyncStep1 is graph content: deletion must either wait
    // until the socket accepted it or win the lease and evict the room first.
    if let Err(error) = linearize_state_bearing_delivery(&app, &graph_id, &room, async {
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
                        let _graph_lease = if let Some(coordinator) = app.try_state::<
                            super::persistence_coordinator::GraphPersistenceCoordinator,
                        >() {
                            match coordinator.acquire_hot_write(&graph_id).await {
                                Ok(lease) => {
                                    // This lease only linearizes `apply_client_update`
                                    // (merges into the in-memory Y.Doc and, if changed,
                                    // rewrites the Y.Doc state file) against concurrent
                                    // graph deletion. It never reaches Oxigraph directly:
                                    // any RDF projection it needs is materialized later by
                                    // a separately-scheduled `crdt.flush` operation, which
                                    // acquires its own (non-read-only) lease. Most inbound
                                    // sync/update frames are no-ops (`changed == false`,
                                    // e.g. awareness/heartbeat traffic) — declaring this
                                    // read-only stops those from spuriously dirtying the
                                    // graph's RDF stores.
                                    lease.declare_rdf_read_only();
                                    Some(lease)
                                }
                                Err(error) => {
                                    log::error!(
                                        "websocket persistence lease failed for {graph_id}: {error}"
                                    );
                                    continue;
                                }
                            }
                        } else {
                            None
                        };
                        match room.apply_client_update(&update).await {
                            Ok(true) => room.schedule_projection_flush(app.clone()),
                            Ok(false) => {}
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
                        if let Some(accepted) = room.merge_awareness(&update, connection_id).await {
                            let rebroadcast = Message::Awareness(accepted).encode_v1();
                            let _ = room.tx.send(rebroadcast);
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
                let Ok(payload) = broadcasted else {
                    break;
                };
                let result = linearize_state_bearing_delivery(
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

    if let Some(removal) = room.remove_awareness_for_connection(connection_id).await {
        // Other subscribers receive the synthetic removals even when this
        // transport disappeared without a graceful Awareness null.
        let _ = room.tx.send(Message::Awareness(removal).encode_v1());
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn disk_restore_admission_refuses_an_opener_paused_before_registry_insertion() {
        let registry = std::sync::Arc::new(super::RoomRegistry::default());
        let root = std::env::temp_dir().join(format!("garden-restore-room-race-{}", uuid::Uuid::new_v4()));
        let path = root.join("ydocs/workspace/update-v1.bin");
        let (reached_tx, reached_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        super::install_tombstone_open_race_hook(path.clone(), reached_tx, release_rx);
        let opener_registry = registry.clone();
        let opener_path = path.clone();
        let opener = std::thread::spawn(move || crate::app_runtime::async_runtime::block_on(async move {
            opener_registry.get_or_create("workspace:restore-race", opener_path).await
        }));
        reached_rx.recv_timeout(std::time::Duration::from_secs(5)).expect("opener reached pre-admission barrier");
        let guard = registry.begin_disk_restore("restore-race").unwrap();
        release_tx.send(()).unwrap();
        let result = opener.join().expect("opener thread");
        super::clear_tombstone_open_race_hook();
        assert!(result.err().unwrap().contains("during disk archive restore"));
        assert!(registry.existing_room("workspace:restore-race").unwrap().is_none());
        assert!(!path.exists());
        drop(guard);
    }

    #[test]
    fn disk_restore_admission_blocks_new_rooms_and_never_evicts_existing_rooms() {
        crate::app_runtime::async_runtime::block_on(async {
            let registry = super::RoomRegistry::default();
            let root = std::env::temp_dir().join(format!("garden-restore-room-admission-{}", uuid::Uuid::new_v4()));
            let workspace_path = root.join("ydocs/workspace/update-v1.bin");
            let guard = registry.begin_disk_restore("graph-a").unwrap();
            assert!(registry.get_or_create("workspace:graph-a", workspace_path.clone()).await.is_err());
            assert!(registry.get_or_create("doc:graph-a:one", root.join("ydocs/documents/one/update-v1.bin")).await.is_err());
            assert!(registry.begin_disk_restore("graph-a").is_err());
            assert!(registry.existing_room("workspace:graph-a").unwrap().is_none());
            // The bounded reservation is graph-local, not a global room stop.
            let other = registry.get_or_create("workspace:graph-ab", root.join("other/update-v1.bin")).await.unwrap();
            drop(guard);
            let existing = registry.get_or_create("workspace:graph-a", workspace_path.clone()).await.unwrap();
            assert!(registry.begin_disk_restore("graph-a").is_err());
            assert!(std::sync::Arc::ptr_eq(&existing, &registry.existing_room("workspace:graph-a").unwrap().unwrap()));
            assert!(std::sync::Arc::ptr_eq(&other, &registry.existing_room("workspace:graph-ab").unwrap().unwrap()));
            assert!(!workspace_path.exists(), "an untouched preopened room has no disk sidecar");
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
}
