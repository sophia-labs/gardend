//! Durable-plane hydrate/flush for headless cells (platform-next M3 "EFS pattern").
//!
//! A cell runs its profile on a fast local `emptyDir` (`GARDEN_PROFILE_DIR`) and
//! periodically persists a consistent snapshot to a slow-but-durable NFS/EFS
//! mount (`GARDEN_DURABLE_DIR`). On boot it hydrates the profile from the latest
//! durable snapshot. None of this is active unless `GARDEN_DURABLE_DIR` is set —
//! desktop builds and durable-less cells behave exactly as before.
//!
//! Snapshot layout under the durable dir:
//!   {durable}/CURRENT          text file naming the live snapshot (e.g. "snap-000042")
//!   {durable}/snap-000042/     a full profile tree (mirrors GARDEN_PROFILE_DIR)
//!
//! Consistency model:
//!   * Oxigraph RocksDB stores are backed up via `Store::backup` (a RocksDB
//!     checkpoint) — never plain-copied while live. The `Store::backup`
//!     checkpoint now runs INSIDE the flush gate's write guard, together with
//!     the plain-file walk below, as one uninterrupted gated span. This closes
//!     an import TOCTOU: a completion-ledger append (which takes the gate's read
//!     side) can no longer land between the RDF checkpoint and the file copy, so
//!     the walk can never publish a ledger without its corresponding
//!     checkpointed RDF. (The engine still guarantees a consistent checkpoint
//!     under concurrent writes; the gate additionally orders it against the walk.)
//!   * Everything else (Y.Doc state, snapshots, semantic index JSON, originals,
//!     ledgers, worklog, Turso DBs) is plain-copied while the same write guard is
//!     held. File-write paths take read guards, so holding the write guard
//!     quiesces them for the duration of the checkpoint-plus-copy span. NOTE: the
//!     checkpoint's duration is now part of this gated window; measure
//!     `Store::backup` timing on a live cell before wider rollout.
//!
//! Crash window: a flush publishes by writing `CURRENT.tmp` then `fs::rename` to
//! `CURRENT`. A crash before the rename leaves a complete-but-unreferenced
//! `snap-*` dir that hydrate ignores (it reads CURRENT). A crash after the
//! rename but before prune leaves an extra old snapshot, harmless. The newest
//! durable state a restarted pod can see is the last *published* snapshot, so up
//! to one flush-interval of work can be lost on an uncontrolled crash; a clean
//! SIGTERM runs a final flush first.
//!
//! Concurrency: all flushes in the process serialize on `FLUSH_SERIAL` inside
//! `flush()` itself. This must NOT be relaxed to caller-side async locking:
//! aborting a tokio task that is awaiting a `spawn_blocking` flush releases the
//! caller's async guard while the blocking thread keeps running, and a
//! concurrently started flush would then race it onto the same snapshot name
//! (observed on a cell as a published mixed-epoch snapshot missing `graphs/`).
//! Snapshots are also built under a unique `.building-*` name and claimed with
//! an atomic rename, sequence numbers are never reused, and a completeness
//! check refuses to publish a snapshot missing top-level profile entries.

// `flush`/`hydrate` and their helpers are only wired up by the headless
// `gardend` bin. `flush_gate` is exercised by write paths in both flavors, so
// the desktop build still pulls the module in but never calls the snapshot
// entry points — silence the resulting dead-code noise there.
#![cfg_attr(feature = "desktop", allow(dead_code))]

use crate::cell_durability_trace::{
    elapsed_ms, next_store_slot_id, origin_from_location, tracing_enabled, DeferReason,
    DirtyEvidence, DirtyEvidenceSnapshot, DirtyOrigin, DirtyReason, FailedPhase, FlushTrace,
    FlushTrigger, KnownCrdtKind, StoreDecision, StoreKind, StoreObservation, TimingPhase,
    TraceOutcome,
};
use std::{cell::RefCell, collections::BTreeMap, marker::PhantomData, rc::Rc};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        Arc, Mutex, OnceLock, RwLock, Weak,
    },
    time::{Duration, Instant},
};

const CURRENT_FILE: &str = "CURRENT";
const CURRENT_TMP_FILE: &str = "CURRENT.tmp";
const SNAP_PREFIX: &str = "snap-";
const BUILDING_PREFIX: &str = ".building-";
const PERIODIC_FLUSH_GATE_WAIT: Duration = Duration::from_secs(1);
// Cell pods currently receive a 60-second termination grace period. Leave the
// second half of it for the EFS walk while giving in-flight writers a
// materially longer chance to drain than a periodic tick does.
const FORCED_FLUSH_GATE_WAIT: Duration = Duration::from_secs(20);
const FLUSH_GATE_RETRY_INTERVAL: Duration = Duration::from_millis(25);

/// Serializes every flush in the process, including blocking threads orphaned
/// by an aborted async caller. See the module docs for why this lives here and
/// not at the call site.
fn flush_serial() -> &'static Mutex<()> {
    static SERIAL: OnceLock<Mutex<()>> = OnceLock::new();
    SERIAL.get_or_init(|| Mutex::new(()))
}

/// Count of heavy operations (graph/vault imports) running in this process.
///
/// While > 0, the *periodic* flusher defers. A long import (e.g. a 605K-quad
/// graph) runs ~90s; a 30s flush tick landing mid-import would hold the flush
/// gate's write guard across a ~25s EFS copy, blocking every per-write read
/// guard and starving the tokio runtime that also serves `/health` — the pod
/// flaps `NotReady` and the gateway 503s the client. The *forced* flush
/// (SIGTERM shutdown, and the one fired when the last import finishes) ignores
/// this, so durable state is always captured at shutdown and right after an
/// import completes.
static IMPORT_ACTIVE: AtomicUsize = AtomicUsize::new(0);

/// Monotonic completion epoch for Garden-owned persistent writes.
///
/// The epoch advances only after a write boundary has finished. A flush records
/// the epoch it saw before starting a RocksDB checkpoint and acknowledges at
/// most that value after publishing. A write that overlaps the checkpoint thus
/// always leaves a newer dirty epoch for the next flush, whether or not RocksDB
/// happened to include it in the checkpoint.
static STORE_WRITE_EPOCH: AtomicU64 = AtomicU64::new(1);

/// Fail-safe for poisoned dirty-state bookkeeping. Once set, every open store
/// is treated as dirty for the rest of the process. Extra checkpoints are safe;
/// calling a changed store clean is not.
static STORE_DIRTY_UNKNOWN: AtomicBool = AtomicBool::new(false);

/// Highest write epoch known to be covered by an actual durable-plane flush
/// completion (the `awaitDurable` receipt watermark).
///
/// Advanced ONLY in the flush funnel
/// (`flush_inner_with_trigger_and_gate_wait`), in the same stroke as a flush
/// attempt that finished as [`FlushAttemptOutcome::Published`] (the snapshot
/// survived Gate C `publish_commit`) or [`FlushAttemptOutcome::ConfirmedClean`]
/// (the full dirty computation ran to completion and confirmed everything at
/// or before the attempt's starting epoch was already in the published
/// snapshot). `Deferred`, `Fenced`, and errored attempts advance nothing —
/// exactly [`DirtyFlushScheduler::record_flush_attempt`]'s discipline, made
/// process-global so the write path can consult it.
///
/// This is a commit-backed receipt, not a bare counter: the value stored is
/// always an epoch captured *before* the dirty computation of an attempt that
/// actually completed, so `durably_resolved_epoch() >= e` genuinely implies
/// every Garden write whose completion mark landed at or before epoch `e` is
/// contained in a published (or confirmed-already-published) durable
/// snapshot. A write overlapping the flush bumps the epoch past the captured
/// value and correctly stays uncovered.
///
/// Process-global is sound because the durable plane is process-global: a
/// cell owns exactly one graph and one `(profile_dir, durable_dir)` pair
/// (see [`DURABLE_DIRS`] — a `OnceLock`). Like the write epoch itself, this
/// resets on restart (a restarted process re-earns coverage with its first
/// completed flush) — it can under-claim after a restart, never over-claim.
static DURABLY_RESOLVED_EPOCH: AtomicU64 = AtomicU64::new(0);

/// Highest write epoch captured in a snapshot that became VISIBLE on the
/// durable plane — the build dir was renamed to its final `snap-*` name and
/// `CURRENT` was rewritten — regardless of what Gate C decided afterwards.
///
/// Advanced in the flush funnel in the same stroke as `publish_current`,
/// with the exact epoch discipline of [`DURABLY_RESOLVED_EPOCH`] (the value
/// is the epoch captured before the attempt's dirty computation). On the
/// ordinary Published path the resolved watermark immediately catches up and
/// this one carries no extra information. It matters on exactly one path:
/// Gate C refuses (or fails) `publish_commit` AFTER the rename — the tree
/// has advanced locally, the attempt reports `Fenced`, and the standing
/// snapshot becomes the successor's repair carrier (boot repair `S == P` →
/// `AcceptPending`, the production analogue of the formal model's
/// still-publishable pending intent that `RepairPublication` can honour).
/// The epochs it captures are therefore NOT known-discarded even when the
/// fence goes terminal: a durability inquiry for them must keep answering
/// "pending" (fate with the authority/successor), never "revoked" — see
/// [`DURABLY_REVOKED_EPOCH`].
static PLANE_VISIBLE_EPOCH: AtomicU64 = AtomicU64::new(0);

/// The same-stroke revocation watermark (Lane 2 repair of the 2026-08-21
/// incident): highest write epoch known DISCARDED by the write-lease fence.
///
/// On 2026-08-21 the cell acknowledged writes, then lease safety worked
/// perfectly — the fenced cell correctly skipped its final flush — and
/// thereby discarded 16.5 hours of acknowledged writes with no path to tell
/// anyone. The integrity mechanism (fencing) worked; the honesty mechanism
/// (the fate of issued acks) did not exist. This watermark is that missing
/// honesty mechanism's production shape: it is advanced ONLY by
/// [`record_fence_discard_revocation`], which enforce-mode terminal fencing
/// (`cell_lease::WriteLease::set_terminal`) calls in the same stroke as the
/// latch that makes the discard irrevocable — never from anything a caller
/// requested (no self-testimony). Once terminal, Gate A refuses every future
/// flush, the loopback router refuses every request, and Gardend skips the
/// final flush entirely, so every locally-completed write epoch above the
/// resolved watermark (and not standing in a plane-visible snapshot, see
/// [`PLANE_VISIBLE_EPOCH`]) is stranded at that instant.
///
/// Consumed by the write path's durability verdict: an inquiry for a write
/// acked "pending" in the discarded range answers `revoked` instead of
/// pending-forever or silence ("the caller is never told" was the defect).
/// Like the other watermarks it is monotone (`fetch_max`) and process-local:
/// it resets on restart, which under-claims (a successor answers "pending"
/// for its own writes), never over-claims.
static DURABLY_REVOKED_EPOCH: AtomicU64 = AtomicU64::new(0);

struct StoreBackupEpoch {
    identity: Weak<oxigraph::store::Store>,
    dirty: u64,
    backed_up: u64,
    slot_id: u64,
    incarnation: u64,
    evidence: DirtyEvidence,
}

/// Proof that an empty local profile was copied from one exact immutable
/// durable snapshot before the cell could serve writes.
///
/// This is deliberately profile-scoped rather than store-scoped: Oxigraph
/// stores are opened only after hydrate, so no live `Arc<Store>` identity
/// exists yet. `store_epoch_to_backup` consumes the proof only when the live
/// store path maps to the same relative directory under `snapshot_dir` and no
/// Garden write epoch advanced after the copy began. Every ambiguity falls
/// back to a real checkpoint.
#[derive(Debug, Clone)]
struct HydratedProfileSeed {
    snapshot_dir: PathBuf,
    clean_epoch: u64,
}

#[derive(Default)]
struct DirtyBookkeeping {
    stores: BTreeMap<PathBuf, StoreBackupEpoch>,
    hydrated_profiles: BTreeMap<PathBuf, HydratedProfileSeed>,
    global_evidence: DirtyEvidence,
}

fn store_backup_epochs() -> &'static Mutex<DirtyBookkeeping> {
    static EPOCHS: OnceLock<Mutex<DirtyBookkeeping>> = OnceLock::new();
    EPOCHS.get_or_init(|| Mutex::new(DirtyBookkeeping::default()))
}

fn next_store_write_epoch() -> u64 {
    match STORE_WRITE_EPOCH.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |epoch| {
        epoch.checked_add(1)
    }) {
        Ok(previous) => previous + 1,
        Err(saturated) => {
            STORE_DIRTY_UNKNOWN.store(true, Ordering::Release);
            saturated
        }
    }
}

/// Test-only: allocate a fresh global write epoch without touching any
/// per-store dirty bookkeeping (a pure atomic bump), so cross-module tests
/// (`cell_lease`'s same-stroke revocation tests) can pin "a write completed
/// before the stroke" without constructing a store or racing the dirty
/// bookkeeping lock.
#[cfg(test)]
pub(crate) fn advance_write_epoch_for_tests() -> u64 {
    next_store_write_epoch()
}

fn same_store(
    identity: &Weak<oxigraph::store::Store>,
    store: &Arc<oxigraph::store::Store>,
) -> bool {
    Weak::ptr_eq(identity, &Arc::downgrade(store))
}

/// Clear any stale proof for `profile_dir` and capture the write epoch before
/// hydrate copies a snapshot. The bookkeeping lock is the same lock every
/// Garden persistence-completion hook takes before advancing the epoch, so a
/// write racing the copy is observable at finish time.
fn begin_profile_hydration(profile_dir: &Path) -> Option<u64> {
    if STORE_DIRTY_UNKNOWN.load(Ordering::Acquire) {
        return None;
    }
    let Ok(mut bookkeeping) = store_backup_epochs().lock() else {
        STORE_DIRTY_UNKNOWN.store(true, Ordering::Release);
        return None;
    };
    bookkeeping.hydrated_profiles.remove(profile_dir);
    Some(current_write_epoch())
}

/// Commit a clean-hydration proof only when no Garden write completed while
/// the snapshot was being copied. A later write advances the same epoch and
/// invalidates the proof when the store is first observed.
fn finish_profile_hydration(
    profile_dir: &Path,
    snapshot_dir: &Path,
    epoch_before_copy: Option<u64>,
) {
    let Some(epoch_before_copy) = epoch_before_copy else {
        return;
    };
    if STORE_DIRTY_UNKNOWN.load(Ordering::Acquire) {
        return;
    }
    let Ok(mut bookkeeping) = store_backup_epochs().lock() else {
        STORE_DIRTY_UNKNOWN.store(true, Ordering::Release);
        return;
    };
    let epoch_after_copy = current_write_epoch();
    if epoch_after_copy != epoch_before_copy {
        log::warn!(
            "durable hydrate: write epoch advanced from {epoch_before_copy} to \
             {epoch_after_copy} during snapshot copy; first store flush remains conservative"
        );
        return;
    }
    bookkeeping.hydrated_profiles.insert(
        profile_dir.to_path_buf(),
        HydratedProfileSeed {
            snapshot_dir: snapshot_dir.to_path_buf(),
            clean_epoch: epoch_after_copy,
        },
    );
}

/// DURABILITY AUDIT FINDING (2026-07-18, re-refute): every marking function
/// below used to call `next_store_write_epoch()` (publishing the new global
/// epoch — the value `current_write_epoch()`/the scheduler observes) BEFORE
/// acquiring `store_backup_epochs`'s lock to record the corresponding
/// per-store `dirty` write. Those are two SEPARATE publications, and nothing
/// forced them to be observed together: a flush's own dirty check
/// (`store_epoch_to_backup`, which also takes this lock) could acquire the
/// lock in the gap — after the epoch had already moved, but before the
/// per-store write landed — see a store as still clean, skip it, and (via
/// the scheduler's `record_flush_attempt`) advance the watermark PAST the
/// epoch that write will ever reach. The write's own per-store `dirty` field
/// would still update moments later, but nothing would ever call `flush`
/// again to notice — a permanent false-CLEAN for that store's write, not
/// just a stale read.
///
/// Fix: acquire the lock FIRST, and perform the epoch allocation and the
/// per-store write inside the SAME critical section as `store_epoch_to_backup`
/// reads. Any flush's dirty check now either runs entirely before this
/// section starts (sees the old epoch AND the old dirty state, consistently)
/// or entirely after it ends (sees the new epoch AND the new dirty state,
/// consistently) — there is no window where one is visible without the
/// other, because both are guarded by the identical lock.
///
/// Mark every store already known to the durable flusher dirty after a broad
/// Garden persistence transaction. Stores first opened later are dirty by
/// construction when the flusher first observes their identity.
fn mark_registered_stores_written(origin: DirtyOrigin, crdt_kind: Option<KnownCrdtKind>) {
    let trace = tracing_enabled();
    let Ok(mut bookkeeping) = store_backup_epochs().lock() else {
        // Poisoned: there is no per-store write left to make atomic with the
        // epoch bump, so there is nothing to hold the lock across. Flag
        // first, bump second: any reader that acquire-loads the bumped
        // epoch and then acquire-loads the flag is guaranteed (release/
        // acquire pair on the two atomics, program order within this
        // thread) to observe it as true.
        STORE_DIRTY_UNKNOWN.store(true, Ordering::Release);
        let _ = next_store_write_epoch();
        return;
    };
    let epoch = next_store_write_epoch();
    for state in bookkeeping.stores.values_mut() {
        state.dirty = epoch;
    }
    if trace {
        bookkeeping
            .global_evidence
            .record(DirtyReason::WriteGuardCompleted, origin, crdt_kind);
        for state in bookkeeping.stores.values_mut() {
            state
                .evidence
                .record(DirtyReason::WriteGuardCompleted, origin, crdt_kind);
        }
    }
}

/// Narrow completion hook for a mutation performed directly against one real
/// Oxigraph store (SPARQL update / RDF loader). If the store has not participated
/// in a durable flush yet, first observation will conservatively mark it dirty.
///
/// See [`mark_registered_stores_written`]'s doc comment for why the lock is
/// acquired before the epoch is allocated.
#[track_caller]
pub(crate) fn mark_rdf_store_written(store: &oxigraph::store::Store) {
    let trace = tracing_enabled();
    let origin = if trace {
        origin_from_location(std::panic::Location::caller())
    } else {
        DirtyOrigin::Other
    };
    let Ok(mut bookkeeping) = store_backup_epochs().lock() else {
        STORE_DIRTY_UNKNOWN.store(true, Ordering::Release);
        let _ = next_store_write_epoch();
        return;
    };
    let epoch = next_store_write_epoch();
    #[cfg(test)]
    pause_inside_mark_dirty_for_test();
    for state in bookkeeping.stores.values_mut() {
        if std::ptr::eq(Weak::as_ptr(&state.identity), store) {
            state.dirty = epoch;
        }
    }
    if trace {
        bookkeeping
            .global_evidence
            .record(DirtyReason::RdfDirectHook, origin, None);
        for state in bookkeeping.stores.values_mut() {
            if std::ptr::eq(Weak::as_ptr(&state.identity), store) {
                state
                    .evidence
                    .record(DirtyReason::RdfDirectHook, origin, None);
            }
        }
    }
}

pub(crate) fn mark_graph_rdf_stores_written_with_context(
    graph_id: &str,
    reason: DirtyReason,
    origin: DirtyOrigin,
    crdt_kind: Option<KnownCrdtKind>,
) {
    let trace = tracing_enabled();
    let Ok(mut bookkeeping) = store_backup_epochs().lock() else {
        STORE_DIRTY_UNKNOWN.store(true, Ordering::Release);
        let _ = next_store_write_epoch();
        return;
    };
    let epoch = next_store_write_epoch();
    let matches_graph = |path: &Path| {
        let is_graph_store = path
            .file_name()
            .is_some_and(|name| name == "store.oxigraph")
            && path
                .parent()
                .and_then(Path::file_name)
                .is_some_and(|name| name == graph_id);
        let is_profile_store = path
            .file_name()
            .is_some_and(|name| name == "metadata.oxigraph")
            || path.file_name().is_some_and(|name| name == "omphalos");
        is_graph_store || is_profile_store
    };
    for (path, state) in bookkeeping.stores.iter_mut() {
        if matches_graph(path) {
            state.dirty = epoch;
        }
    }
    if trace {
        bookkeeping
            .global_evidence
            .record(reason, origin, crdt_kind);
        for (path, state) in bookkeeping.stores.iter_mut() {
            if matches_graph(path) {
                state.evidence.record(reason, origin, crdt_kind);
            }
        }
    }
}

/// Profile + durable dirs, registered once by `gardend` when the durable plane
/// is active, so an [`ImportGuard`] can flush on drop without threading the
/// paths through the executor.
static DURABLE_DIRS: OnceLock<Option<(PathBuf, PathBuf)>> = OnceLock::new();

/// Register the durable dirs (called once at startup when durable is active).
pub fn set_durable_dirs(profile_dir: PathBuf, durable_dir: PathBuf) {
    let _ = DURABLE_DIRS.set(Some((profile_dir, durable_dir)));
}

/// Publish a strict write before its caller performs an external effect.
/// Desktop profiles are already the durable ground. A headless cell without
/// registered durable dirs must fail closed, including during startup.
pub(crate) fn flush_registered_for_strict_write() -> Result<(), String> {
    let Some(Some((profile_dir, durable_dir))) = DURABLE_DIRS.get() else {
        return if durable_plane_semantics() {
            Err("durable plane is not configured".into())
        } else {
            Ok(())
        };
    };
    match flush_forced_detailed(profile_dir, durable_dir)? {
        (_, FlushAttemptOutcome::Published | FlushAttemptOutcome::ConfirmedClean) => Ok(()),
        (_, FlushAttemptOutcome::Deferred) => Err("durable flush deferred".into()),
        (_, FlushAttemptOutcome::Fenced) => Err("durable flush fenced".into()),
    }
}

/// RAII marker: a heavy import is running in this process.
///
/// Created at the top of a heavy operation (held for its whole duration on the
/// import's blocking thread). On drop it decrements the counter and, if it was
/// the last active import, fires a **forced** flush — capturing the
/// freshly-imported state immediately rather than waiting up to a full flush
/// interval. The drop runs on the import's own blocking thread, so the
/// synchronous flush there is safe and keeps the pod responsive.
#[must_use = "hold the guard for the duration of the import"]
pub struct ImportGuard {
    _private: (),
    // Dropped after this type's custom Drop completes, so the cell remains
    // non-quiescent through the post-import forced flush as well as the import.
    _lifecycle: Option<crate::cell_lifecycle::BackgroundLease>,
}

/// Mark a heavy import as in progress; see [`ImportGuard`].
///
/// Under `cfg(test)` the global counter is left untouched: many lib tests
/// exercise the import ops, and a perturbed `IMPORT_ACTIVE` would make
/// concurrent durability flush tests defer. The defer logic itself is unit-
/// tested by driving `IMPORT_ACTIVE` directly; the integration is validated on
/// the cluster.
pub fn import_guard() -> ImportGuard {
    #[cfg(not(test))]
    IMPORT_ACTIVE.fetch_add(1, Ordering::SeqCst);
    #[cfg(test)]
    let lifecycle = None;
    #[cfg(not(test))]
    let lifecycle = crate::cell_lifecycle::process_lifecycle()
        .and_then(|lifecycle| lifecycle.begin_background("heavy-import").ok());
    ImportGuard {
        _private: (),
        _lifecycle: lifecycle,
    }
}

impl Drop for ImportGuard {
    fn drop(&mut self) {
        #[cfg(not(test))]
        {
            let prev = IMPORT_ACTIVE.fetch_sub(1, Ordering::SeqCst);
            if prev == 1 {
                if let Some(Some((profile_dir, durable_dir))) = DURABLE_DIRS.get() {
                    match flush_forced_with_trigger(
                        profile_dir,
                        durable_dir,
                        FlushTrigger::PostImport,
                    ) {
                        Ok(outcome) if outcome.published => log::info!(
                            "durable flush after import published snap-{:06}: {} copied, {} linked, {} bytes",
                            outcome.sequence,
                            outcome.files_copied,
                            outcome.files_linked,
                            outcome.bytes_copied
                        ),
                        Ok(_) => {}
                        Err(error) => log::error!("durable flush after import failed: {error}"),
                    }
                }
            }
        }
    }
}

/// Process-wide gate coordinating non-oxigraph file writes with the flusher.
///
/// Write paths acquire a *read* guard for the duration of their write; the
/// flusher acquires the *write* guard while it plain-copies non-oxigraph files,
/// so no half-written file lands in a snapshot. Oxigraph backups do not use this
/// gate (the RocksDB checkpoint is engine-consistent on its own).
pub fn flush_gate() -> &'static RwLock<()> {
    static GATE: OnceLock<RwLock<()>> = OnceLock::new();
    GATE.get_or_init(|| RwLock::new(()))
}

struct ThreadWriteGuardState {
    depth: usize,
    root: Option<std::sync::RwLockReadGuard<'static, ()>>,
    origin: Option<DirtyOrigin>,
}

thread_local! {
    /// `std::sync::RwLock` may hold a new reader behind an already-waiting
    /// writer. A persistence root that called another guarded helper would
    /// therefore deadlock unless nested acquisition is handled explicitly.
    /// Keep the one real read guard in thread-local storage and hand nested
    /// callers lightweight depth tokens.
    static THREAD_WRITE_GUARD: RefCell<ThreadWriteGuardState> = const {
        RefCell::new(ThreadWriteGuardState { depth: 0, root: None, origin: None })
    };
}

/// Re-entrant token for one synchronous persistence transaction.
///
/// The token is intentionally `!Send`: the underlying standard-library read
/// guard is thread-affine. Async roots must acquire it only around their
/// synchronous filesystem commit section, never across an `.await`.
pub(crate) struct CellDurabilityWriteGuard {
    active: bool,
    _not_send: PhantomData<Rc<()>>,
}

impl Drop for CellDurabilityWriteGuard {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        THREAD_WRITE_GUARD.with(|state| {
            let mut state = state.borrow_mut();
            debug_assert!(state.depth > 0, "durability write-guard depth underflow");
            if state.depth == 0 {
                return;
            }
            state.depth -= 1;
            if state.depth == 0 {
                // Release the one real RwLock reader only after the last token,
                // even if nested tokens were dropped out of lexical order.
                state.root.take();
                // The persistence transaction has fully completed. Mark after
                // releasing the gate: if a waiting flush slips in first, it can
                // acknowledge only the older epoch and this write remains dirty.
                let origin = state.origin.take().unwrap_or(DirtyOrigin::Other);
                mark_registered_stores_written(origin, None);
            }
        });
        self.active = false;
    }
}

fn acquire_write_guard(origin: DirtyOrigin) -> Option<CellDurabilityWriteGuard> {
    THREAD_WRITE_GUARD.with(|state| {
        let mut state = state.borrow_mut();
        if state.depth == 0 {
            let root = match flush_gate().read() {
                Ok(root) => root,
                Err(_) => {
                    // Production writes deliberately proceed if the gate is
                    // poisoned. Never let that fail-open behavior become a
                    // false-clean store decision.
                    STORE_DIRTY_UNKNOWN.store(true, Ordering::Release);
                    // DURABILITY AUDIT FINDING (2026-07-18): setting the flag
                    // alone is not enough — `DirtyFlushScheduler` decides
                    // whether to call `flush` at all by watching
                    // `current_write_epoch()`, and this branch previously
                    // never advanced it. A poisoning that landed with no
                    // other write ever happening afterward would set the
                    // fail-safe flag but the scheduler would never poll a
                    // flush that could consult it — the store-dirty-unknown
                    // fallback would sit unconsulted forever. Bump the epoch
                    // too, so the very next scheduler poll sees pending
                    // activity and (per `store_epoch_to_backup`, which checks
                    // `STORE_DIRTY_UNKNOWN` first) is guaranteed to treat
                    // every store as dirty.
                    let _ = next_store_write_epoch();
                    return None;
                }
            };
            state.root = Some(root);
            state.origin = Some(origin);
        }
        state.depth = state.depth.saturating_add(1);
        Some(CellDurabilityWriteGuard {
            active: true,
            _not_send: PhantomData,
        })
    })
}

/// Acquire a read guard on the flush gate for the duration of a file write,
/// keeping it consistent with the flusher's write guard. Poison is treated as a
/// no-op (a poisoned gate means a flusher panicked mid-copy; writes proceed
/// rather than wedge the cell). The returned guard must be held until the write
/// completes — bind it to a `_guard` local at the top of the write.
///
/// Under `cfg(test)` this is a no-op: the gate is a process-global, and the
/// many unrelated lib tests that exercise write paths would otherwise hold read
/// guards that race the durability tests' flush assertions. The flusher's own
/// gate usage (and the explicit gate-contention test) go through
/// [`flush_gate`] directly and are unaffected; the production binary is built
/// without `cfg(test)`, so it always takes the real guard.
#[cfg(not(test))]
#[track_caller]
pub(crate) fn write_guard() -> Option<CellDurabilityWriteGuard> {
    let origin = if tracing_enabled() {
        origin_from_location(std::panic::Location::caller())
    } else {
        DirtyOrigin::Other
    };
    acquire_write_guard(origin)
}

#[cfg(test)]
pub(crate) fn write_guard() -> Option<std::sync::RwLockReadGuard<'static, ()>> {
    None
}

#[cfg(test)]
fn write_guard_for_test() -> Option<CellDurabilityWriteGuard> {
    acquire_write_guard(DirtyOrigin::Other)
}

/// Result of a flush attempt.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct FlushOutcome {
    /// `true` if a new snapshot was published; `false` if nothing changed.
    pub published: bool,
    /// Snapshot sequence number published (0 when `published` is false).
    pub sequence: u64,
    pub files_copied: usize,
    pub files_linked: usize,
    pub stores_backed_up: usize,
    pub bytes_copied: u64,
}

/// Why a flush attempt did or did not resolve the pending dirty state, for
/// callers that need that distinction (the dirty-driven scheduler — see
/// [`DirtyFlushScheduler`] and [`flush_scheduled`]). `flush`/`flush_forced`
/// discard this; they only ever cared about `FlushOutcome`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlushAttemptOutcome {
    /// A new snapshot was published.
    Published,
    /// The full dirty computation ran to completion (Oxigraph stores AND the
    /// "everything else" tree comparison) and confirmed nothing had changed.
    /// Safe to treat the epoch observed before this attempt as resolved.
    ConfirmedClean,
    /// The attempt returned before completing that computation — a heavy
    /// import was active, or a write path held the flush gate past the wait
    /// budget. Nothing was confirmed either way: the pending epoch must
    /// remain pending and be retried soon, never treated as resolved.
    Deferred,
    /// U8 cross-process write lease (spec §3.4): the flush refused at Gate A
    /// (no margin left on the lease), Gate B (`publish_intent` refused or
    /// timed out immediately before the snapshot rename), or Gate C
    /// (`publish_commit` failed after `CURRENT` was already rewritten — the
    /// tree still advanced locally, but this incarnation fences itself and
    /// the successor's boot-time repair reconciles the authority record via
    /// the `S == P` accept case, spec §3.6).
    ///
    /// Distinguished from [`FlushAttemptOutcome::Deferred`] deliberately:
    /// `Deferred` means "nothing was checked, retry soon"; `Fenced` means
    /// "the lease said no — lost work, never corruption" (spec §1.5). Only
    /// present when a lease is active (`cell_lease::handle()` is `Some`) and
    /// `GARDEN_LEASE_MODE=enforce`; in observe mode every gate evaluates and
    /// testifies would-have-fenced outcomes but never returns this variant.
    /// `// U1: delete with the snapshot flush path`.
    Fenced,
}

/// Process-local RAII witness for the real serialized flush interval.
///
/// The renew task reads this marker while the blocking flush thread is doing
/// work. Keeping the cleanup in `Drop` is intentional: every early return and
/// unwinding path clears `flush_in_progress_ms`, so a failed attempt cannot
/// leave the holder looking permanently wedged to the authority.
struct LeaseFlushMarker {
    lease: Option<&'static crate::cell_lease::WriteLease>,
    ok: bool,
}

impl LeaseFlushMarker {
    fn start() -> Self {
        let lease = crate::cell_lease::handle();
        if let Some(lease) = lease {
            lease.mark_flush_started();
        }
        Self { lease, ok: false }
    }

    fn mark_ok(&mut self) {
        self.ok = true;
    }
}

impl Drop for LeaseFlushMarker {
    fn drop(&mut self) {
        if let Some(lease) = self.lease {
            lease.mark_flush_finished(self.ok);
        }
    }
}

/// Why profile hydration did or did not copy a durable snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HydrateMode {
    /// No durable CURRENT pointer exists; this is the cell's first durable boot.
    Fresh,
    /// A durable snapshot was copied into an empty local profile.
    Restored,
    /// The local profile already contains state (for example, same-pod restart).
    WarmProfile,
}

/// Detailed hydrate evidence used by lifecycle testimony. The legacy
/// [`hydrate`] wrapper remains available for callers that only need a boolean.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HydrateOutcome {
    pub mode: HydrateMode,
    pub snapshot_id: Option<String>,
    pub hydrated_bytes: Option<u64>,
}

impl HydrateOutcome {
    pub fn restored(&self) -> bool {
        self.mode == HydrateMode::Restored
    }
}

/// Restore the latest durable snapshot into `profile_dir` if the profile is
/// empty/absent. No-op (returns `false`) when there is no CURRENT pointer or the
/// profile already has content (a container restart inside the same pod keeps
/// the newer local state). Returns `true` when a snapshot was hydrated.
pub fn hydrate(profile_dir: &Path, durable_dir: &Path) -> Result<bool, String> {
    hydrate_detailed(profile_dir, durable_dir).map(|outcome| outcome.restored())
}

/// Detailed form of [`hydrate`], retaining the selected snapshot identifier
/// and exact copied-byte count for Observatory boot testimony.
pub fn hydrate_detailed(profile_dir: &Path, durable_dir: &Path) -> Result<HydrateOutcome, String> {
    // First-call proof only. Warm/fresh/failed hydrate outcomes deliberately
    // leave no seed, so the first observed store remains dirty by default.
    let hydrate_epoch_before_copy = begin_profile_hydration(profile_dir);
    if profile_has_content(profile_dir) {
        log::info!(
            "durable hydrate skipped: profile dir {} is non-empty (same-pod restart)",
            display(profile_dir)
        );
        return Ok(HydrateOutcome {
            mode: HydrateMode::WarmProfile,
            snapshot_id: None,
            hydrated_bytes: None,
        });
    }

    let current_path = durable_dir.join(CURRENT_FILE);
    let snap_name = match fs::read_to_string(&current_path) {
        Ok(raw) => raw.trim().to_string(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            log::info!(
                "durable hydrate: no {} under {} — fresh profile (first boot)",
                CURRENT_FILE,
                display(durable_dir)
            );
            return Ok(HydrateOutcome {
                mode: HydrateMode::Fresh,
                snapshot_id: None,
                hydrated_bytes: None,
            });
        }
        Err(error) => return Err(format!("read durable {}: {error}", display(&current_path))),
    };

    if !is_valid_snap_name(&snap_name) {
        return Err(format!(
            "durable {} names an invalid snapshot {snap_name:?}",
            CURRENT_FILE
        ));
    }
    let mut snap_name = snap_name;
    let mut snap_dir = durable_dir.join(&snap_name);
    if !snap_dir.is_dir() {
        // Self-heal a dangling CURRENT (e.g. a prune raced a briefly
        // overlapping writer): every snap-* dir is complete by construction
        // (claimed via atomic rename), so the newest one present is safe.
        let fallback_seq = max_existing_snap_seq(durable_dir)?;
        if fallback_seq == 0 {
            return Err(format!(
                "durable {} points at missing snapshot {} and no other snapshots exist",
                CURRENT_FILE,
                display(&snap_dir)
            ));
        }
        let fallback = format!("{SNAP_PREFIX}{fallback_seq:06}");
        log::warn!(
            "durable {} points at missing snapshot {snap_name}; falling back to newest complete snapshot {fallback}",
            CURRENT_FILE
        );
        snap_name = fallback;
        snap_dir = durable_dir.join(&snap_name);
    }

    let started = Instant::now();
    fs::create_dir_all(profile_dir)
        .map_err(|error| format!("create profile dir {}: {error}", display(profile_dir)))?;
    let bytes = copy_tree(&snap_dir, profile_dir)?;
    finish_profile_hydration(profile_dir, &snap_dir, hydrate_epoch_before_copy);
    log::info!(
        "durable hydrate: restored {snap_name} ({} bytes) into {} in {:?}",
        bytes,
        display(profile_dir),
        started.elapsed()
    );
    Ok(HydrateOutcome {
        mode: HydrateMode::Restored,
        snapshot_id: Some(snap_name),
        hydrated_bytes: Some(bytes),
    })
}

/// Flush a consistent snapshot of `profile_dir` to `durable_dir` and publish it.
/// Skips publishing (returns a non-published outcome) when nothing changed since
/// the previous snapshot, or while a heavy import is running (see
/// [`IMPORT_ACTIVE`]).
pub fn flush(profile_dir: &Path, durable_dir: &Path) -> Result<FlushOutcome, String> {
    flush_inner(
        profile_dir,
        durable_dir,
        false,
        FlushTrigger::ManualPeriodic,
    )
    .map(|(outcome, _)| outcome)
}

/// Like [`flush`] but runs even while an import is active. Used by the SIGTERM
/// final flush and the post-import flush, where capturing state matters more
/// than deferring to avoid mid-import runtime contention.
pub fn flush_forced(profile_dir: &Path, durable_dir: &Path) -> Result<FlushOutcome, String> {
    flush_forced_with_trigger(profile_dir, durable_dir, FlushTrigger::ManualForced)
}

/// Forced flush with a fixed-cardinality causal label. The label affects only
/// env-gated stderr diagnostics; `force` semantics remain identical to
/// [`flush_forced`].
pub fn flush_forced_with_trigger(
    profile_dir: &Path,
    durable_dir: &Path,
    trigger: FlushTrigger,
) -> Result<FlushOutcome, String> {
    flush_inner(profile_dir, durable_dir, true, trigger).map(|(outcome, _)| outcome)
}

/// Forced flush with the lease/defer disposition preserved for lifecycle
/// testimony. Most library callers only need [`FlushOutcome`] and should keep
/// using [`flush_forced`]; the headless process must distinguish `Fenced`
/// from an honest clean no-op so it never emits a false successful flush.
pub fn flush_forced_detailed(
    profile_dir: &Path,
    durable_dir: &Path,
) -> Result<(FlushOutcome, FlushAttemptOutcome), String> {
    flush_forced_detailed_with_trigger(profile_dir, durable_dir, FlushTrigger::ManualForced)
}

/// Trigger-labelled forced flush with the lease/defer disposition preserved.
pub fn flush_forced_detailed_with_trigger(
    profile_dir: &Path,
    durable_dir: &Path,
    trigger: FlushTrigger,
) -> Result<(FlushOutcome, FlushAttemptOutcome), String> {
    flush_inner(profile_dir, durable_dir, true, trigger)
}

/// Like [`flush`], but for the dirty-driven periodic scheduler: also reports
/// [`FlushAttemptOutcome`] so the caller can tell a *confirmed*-clean result
/// (safe to advance [`DirtyFlushScheduler`]'s watermark past) apart from a
/// *deferred* one (an import was active, or a write path held the flush gate
/// past the wait budget — nothing was actually checked, so the pending epoch
/// must remain pending and retried soon).
///
/// `force` should be `true` exactly when [`DirtyFlushScheduler::poll`]
/// returned [`SchedulerAction::FlushAtRpoCeiling`] — see that variant's doc
/// comment for why the max-RPO bound would otherwise be silently broken by a
/// concurrent heavy import.
pub fn flush_scheduled(
    profile_dir: &Path,
    durable_dir: &Path,
    force: bool,
) -> Result<(FlushOutcome, FlushAttemptOutcome), String> {
    let trigger = if force {
        FlushTrigger::RpoCeiling
    } else {
        FlushTrigger::PeriodicDebounce
    };
    flush_scheduled_with_trigger(profile_dir, durable_dir, force, trigger)
}

/// Scheduled flush with a fixed-cardinality causal label. The explicit
/// `force` argument remains the sole behavioral authority; `trigger` is never
/// consulted by the persistence algorithm.
pub fn flush_scheduled_with_trigger(
    profile_dir: &Path,
    durable_dir: &Path,
    force: bool,
    trigger: FlushTrigger,
) -> Result<(FlushOutcome, FlushAttemptOutcome), String> {
    flush_inner(profile_dir, durable_dir, force, trigger)
}

fn flush_inner(
    profile_dir: &Path,
    durable_dir: &Path,
    force: bool,
    trigger: FlushTrigger,
) -> Result<(FlushOutcome, FlushAttemptOutcome), String> {
    let gate_wait = if force {
        FORCED_FLUSH_GATE_WAIT
    } else {
        PERIODIC_FLUSH_GATE_WAIT
    };
    flush_inner_with_trigger_and_gate_wait(profile_dir, durable_dir, force, trigger, gate_wait)
}

#[cfg(test)]
fn flush_inner_with_gate_wait(
    profile_dir: &Path,
    durable_dir: &Path,
    force: bool,
    gate_wait: Duration,
) -> Result<(FlushOutcome, FlushAttemptOutcome), String> {
    let trigger = if force {
        FlushTrigger::ManualForced
    } else {
        FlushTrigger::ManualPeriodic
    };
    flush_inner_with_trigger_and_gate_wait(profile_dir, durable_dir, force, trigger, gate_wait)
}

fn flush_inner_with_trigger_and_gate_wait(
    profile_dir: &Path,
    durable_dir: &Path,
    force: bool,
    trigger: FlushTrigger,
    gate_wait: Duration,
) -> Result<(FlushOutcome, FlushAttemptOutcome), String> {
    let trace_enabled = tracing_enabled();
    let epoch_at_start = if trace_enabled {
        current_write_epoch()
    } else {
        0
    };
    let trace = FlushTrace::new(trigger, force, epoch_at_start);
    let serial_wait = trace.timer(TimingPhase::SerialWait);
    let _serial = match flush_serial().lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    drop(serial_wait);

    // Captured while owning FLUSH_SERIAL, before the dirty computation: any
    // write still in flight bumps the epoch past this value and stays
    // uncovered by the receipt watermark below — the same "acknowledge only
    // the epoch captured before the checkpoint" discipline `flush` applies
    // per store and `DirtyFlushScheduler` applies to its own watermark.
    let receipt_epoch_before = current_write_epoch();
    // S2 fence (sealed examination 2026-08-24): in observe mode a refused
    // Gate B/C logs and proceeds, so Published alone cannot carry the
    // durability claim — any lease doubt testified during the attempt
    // withholds the watermark advance (under-claim, never over-claim).
    let degradation_events_before = crate::cell_lease::observe_degradation_events();

    let global_evidence = trace.enabled().then(snapshot_global_dirty_evidence);
    if let Some(evidence) = &global_evidence {
        trace.set_global_evidence(evidence.clone());
    }
    let result = flush_inner_body(
        profile_dir,
        durable_dir,
        force,
        gate_wait,
        receipt_epoch_before,
        &trace,
    );
    if let (
        Ok((_, FlushAttemptOutcome::Published | FlushAttemptOutcome::ConfirmedClean)),
        Some(evidence),
    ) = (&result, &global_evidence)
    {
        acknowledge_global_dirty_evidence(evidence);
    }
    // The awaitDurable receipt watermark advances in the same stroke as the
    // completion that pays it — never on Deferred/Fenced/error (see
    // `durably_resolved_epoch_after_attempt`). `fetch_max` keeps it monotone
    // under the (test-only) case of interleaved flushes of different dirs.
    let lease_doubted =
        crate::cell_lease::observe_degradation_events() != degradation_events_before;
    if let Some(resolved) =
        durably_resolved_epoch_after_attempt(receipt_epoch_before, &result, lease_doubted)
    {
        DURABLY_RESOLVED_EPOCH.fetch_max(resolved, Ordering::SeqCst);
    }

    let trace_outcome = match &result {
        Ok((_, FlushAttemptOutcome::Published)) => TraceOutcome::Published,
        Ok((_, FlushAttemptOutcome::ConfirmedClean)) => TraceOutcome::ConfirmedClean,
        Ok((_, FlushAttemptOutcome::Deferred)) => TraceOutcome::Deferred,
        // The existing diagnostic vocabulary predates U8. A lease fence is a
        // refused attempt, not a clean/deferred one, so retain the closed
        // vocabulary and attribute it to the phase where the fence fired.
        Ok((_, FlushAttemptOutcome::Fenced)) => TraceOutcome::Failed,
        Err(_) => TraceOutcome::Failed,
    };
    // Match the original serialization boundary: diagnostic serialization and
    // stderr logging must not keep the next flush waiting on FLUSH_SERIAL.
    drop(_serial);
    if trace.enabled() {
        trace.finish(
            trace_outcome,
            current_write_epoch(),
            STORE_DIRTY_UNKNOWN.load(Ordering::Acquire),
        );
    }
    result
}

/// DURABILITY AUDIT FINDING (2026-07-18, re-refute #3 — TOCTOU on the
/// re-refute #2 fix): the previous fix computed a `gate_first` bool from a
/// ONE-SHOT `IMPORT_ACTIVE` sample taken before this function even acquired
/// `flush_serial`, then branched the checkpoint/gate ordering on it. That
/// sample can go stale: an import can start in the gap between the sample
/// and the RDF checkpoint actually running (whether because `flush_serial`
/// was contended, or simply because the sample and the checkpoint are not
/// the same instant) — landing exactly on the coarser, torn-import-vulnerable
/// ordering the fix was supposed to replace for exactly the scenario it was
/// built to close, now triggered by a freshly-STARTING import instead of an
/// already-active one. No amount of re-sampling closes this: any read of
/// `IMPORT_ACTIVE` is stale the instant after it's taken.
///
/// Fix: stop deriving the checkpoint/gate ordering from `IMPORT_ACTIVE` at
/// all. The plain-file flush gate is now acquired BEFORE the Oxigraph store
/// checkpoint step UNCONDITIONALLY, for every flush attempt that reaches
/// this point (periodic or forced, import active or not), and held through
/// both the checkpoint and the plain-file walk — never released and
/// reacquired between them. See the inline comment at the gate acquisition
/// below for the full non-torn-composition argument (unchanged from the
/// prior pass's `gate_first`, now simply the only ordering that exists).
///
/// Cost, disclosed rather than assumed away: today, EVERY flush (periodic
/// or forced) already holds this exact gate for the ENTIRE plain-file walk
/// — the dominant cost (the platform review measured 10-17s per cycle,
/// hard-linking ~479-593 files against EFS). That is pre-existing and
/// unchanged by this fix. What this fix ADDS to that already-existing
/// exclusive window is the Oxigraph checkpoint's own duration
/// (`Store::backup`, per open store) — previously run un-gated, before the
/// wait. That checkpoint's target is `build_dir`, itself under
/// `durable_dir` (the EFS mount) — so its cost is NOT guaranteed to be
/// negligible/local-disk-fast in the real deployment; it is a genuinely
/// open question whether it is small or material relative to the walk,
/// and I could not measure it from this environment (no live EFS access).
/// This is a real, live-metrics-dependent tradeoff, not a solved one — see
/// the handoff for the explicit ask to measure `MeteredIOBytes`/checkpoint
/// duration on canary before treating this as cost-free. I considered and
/// rejected, for this pass, a "consistent gated cut" (freeze a fast,
/// local-disk-only file-metadata snapshot under the gate, then do the slow
/// EFS hard-link walk from that frozen list AFTER releasing the gate) —
/// architecturally the better answer (gate held only across the two FAST
/// steps, not the slow walk) but it requires proving, for every "everything
/// else" writer (Y.Doc state, workspace snapshots, semantic index JSON,
/// originals, ledgers, worklog, Turso DBs — a wide, not-fully-audited
/// surface), that files are replaced via atomic rename rather than mutated
/// in place; get that wrong for even one writer and a frozen-list copy done
/// after releasing the gate could read torn, half-written content. I judged
/// that unverified-invariant risk higher than the disclosed, bounded cost
/// of the simpler fix below, given the time available — flagged explicitly
/// as the preferred follow-up, not a rejected idea.
fn flush_inner_body(
    profile_dir: &Path,
    durable_dir: &Path,
    force: bool,
    gate_wait: Duration,
    receipt_epoch_before: u64,
    trace: &FlushTrace,
) -> Result<(FlushOutcome, FlushAttemptOutcome), String> {
    // Start the stuck-flush clock only after this attempt owns the process
    // serializer. Time queued behind another local flush is not authority
    // work and must not cause this holder to forfeit. Serialization itself is
    // owned by `flush_inner_with_trigger_and_gate_wait`, which also records
    // the causal trace's serial-wait duration.
    let mut lease_flush_marker = LeaseFlushMarker::start();

    // Periodic flushes SKIP (pure optimization, not a correctness gate — see
    // this function's doc comment) while a heavy import looks active: an
    // import's own write_guard()-gated per-document saves will very likely
    // keep the flush gate contended for the whole ~90s anyway, so trying
    // costs a wasted RDF checkpoint under the gate for nothing. Correctness
    // does NOT depend on this check's freshness: even if IMPORT_ACTIVE flips
    // to >0 the instant after this read, the unconditional gate-first
    // ordering below still closes the torn-composition race regardless. The
    // forced path (shutdown / post-import / RPO-ceiling) always proceeds.
    if !force && IMPORT_ACTIVE.load(Ordering::Acquire) > 0 {
        trace.set_defer_reason(DeferReason::ImportActive);
        log::debug!("durable flush deferred: import in progress");
        return Ok((FlushOutcome::default(), FlushAttemptOutcome::Deferred));
    }

    trace.set_phase(FailedPhase::Prepare);

    // Gate A (U8 spec §3.4) — the cross-process write lease's zero-I/O
    // check, evaluated for every flush attempt regardless of force. A no-op
    // whenever no lease is active: `cell_lease::handle()` is `None` on
    // desktop and on legacy-unfenced boots (no `GARDEN_DURABLE_EPOCH`), which
    // preserves today's behavior byte for byte on both paths. `false` here
    // covers "never yet renewed" (still inside the boot-time effective
    // wait), "margin exhausted", and a latched publish fence. A successful
    // renew clears a recoverable publish fence; until then Gate A must not
    // allow another intent to overwrite the unresolved pending snapshot.
    if let Some(lease) = crate::cell_lease::handle() {
        // TEST-ONLY escape hatch (U8 spec section 6.2, Z1 bypass-injection) —
        // never set by production pod specs; exists solely so the integration
        // suite can prove Gate B/C are an independent fencing layer, not
        // merely downstream of Gate A's own check.
        let bypass_gate_a = std::env::var("GARDEN_LEASE_TEST_BYPASS_GATE_A").is_ok();
        if bypass_gate_a {
            log::warn!(
                "TEST ONLY: bypassing lease Gate A for U8 bypass-injection proof; Gates B/C remain active"
            );
        } else if lease.terminal_reason().is_some()
            || lease.is_fenced()
            || !lease.valid_with_margin()
        {
            match lease.mode() {
                crate::cell_lease::LeaseMode::Enforce => {
                    log::warn!(
                        "durable flush FENCED at Gate A: lease for {} epoch {} is terminal, \
                         latched fenced, or has no margin remaining",
                        lease.graph_id(),
                        lease.epoch()
                    );
                    return Ok((FlushOutcome::default(), FlushAttemptOutcome::Fenced));
                }
                crate::cell_lease::LeaseMode::Observe => {
                    lease.testify_observe_degraded(
                        crate::cell_lease::ObserveLeaseDegradation::GateAMargin,
                    );
                    log::warn!(
                        "durable flush WOULD HAVE been fenced at Gate A (observe mode): lease for \
                         {} epoch {} is terminal, latched fenced, or has no margin remaining — \
                         proceeding unfenced, full protocol still exercised at Gates B/C below",
                        lease.graph_id(),
                        lease.epoch()
                    );
                }
            }
        }
    }

    let started = Instant::now();
    fs::create_dir_all(durable_dir)
        .map_err(|error| format!("create durable dir {}: {error}", display(durable_dir)))?;

    let previous = read_current(durable_dir)?;
    let previous_dir = previous.as_ref().map(|name| durable_dir.join(name));
    // Never reuse a sequence number, even one held by a complete-but-unreferenced
    // dir from an earlier crash — claiming an existing name would mean deleting a
    // tree someone else may still be writing.
    let current_seq = previous.as_ref().and_then(|name| parse_seq(name));
    trace.set_previous_sequence(current_seq);
    let next_seq = current_seq
        .unwrap_or(0)
        .max(max_existing_snap_seq(durable_dir)?)
        + 1;
    let next_name = format!("{SNAP_PREFIX}{next_seq:06}");
    let next_dir = durable_dir.join(&next_name);

    // Build under a unique temp name, then claim the final name atomically.
    // The epoch stamp (0 when no lease is active — desktop, or a
    // legacy-unfenced boot) kills the verified cross-container PID collision
    // (`std::process::id()` alone: two gateway-managed containers can share
    // PID 1 in their own namespaces on the same EFS mount) for free, and lets
    // an operator eyeball which incarnation built a stale `.building-*` dir
    // without touching the `snap-NNNNNN` namespace at all (spec §3.4).
    static BUILD_COUNTER: AtomicU64 = AtomicU64::new(0);
    let build_epoch = crate::cell_lease::handle()
        .map(|lease| lease.epoch())
        .unwrap_or(0);
    let build_name = format!(
        "{BUILDING_PREFIX}{next_seq:06}-e{build_epoch:010}-{}-{}",
        std::process::id(),
        BUILD_COUNTER.fetch_add(1, Ordering::Relaxed)
    );
    let build_dir = durable_dir.join(&build_name);

    // P5: the under-the-gate capture stages on the profile's OWN filesystem.
    // This is load-bearing, not incidental — `fs::hard_link` cannot cross a
    // filesystem boundary, and linking is what makes the capture cost ~6 µs
    // per inode instead of an EFS round trip. `durable_dir` is the EFS mount;
    // the profile's parent is local disk.
    let staging_root = profile_dir
        .parent()
        .ok_or_else(|| format!("profile dir {} has no parent", display(profile_dir)))?;
    let staging_dir = staging_root.join(format!(".{build_name}.staging"));
    // A previous crash can only ever strand a uniquely-named staging tree,
    // never a half-published generation; clear our own name defensively.
    let _ = fs::remove_dir_all(&staging_dir);

    /// Removes the local staging tree on EVERY exit from the flush body —
    /// success, early return, error, or panic. `flush_inner_body` has a dozen
    /// return points; a guard is the only way to be sure none of them leaks a
    /// full copy of the profile onto local disk once per flush.
    struct StagingGuard(PathBuf);
    impl Drop for StagingGuard {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    let _staging_guard = StagingGuard(staging_dir.clone());

    let mut outcome = FlushOutcome {
        sequence: next_seq,
        ..FlushOutcome::default()
    };
    // The capture walk's link/copy counts describe local staging work, not the
    // durable-plane I/O the trace reports. Keep them separate so `files_linked`
    // and `files_copied` continue to mean "EFS operations".
    let mut capture_outcome = FlushOutcome::default();
    let mut backed_store_epochs: Vec<(PathBuf, Arc<oxigraph::store::Store>, StoreBackupPlan)> =
        Vec::new();

    let result = (|| -> Result<FlushAttemptOutcome, String> {
        // Freeze the set of physical Oxigraph path incarnations before taking
        // the registry snapshot, and keep it frozen until the plain-file walk
        // finishes. Store::backup remains concurrent with writes to an
        // already-open store; this barrier covers only open/evict/replace.
        //
        // Lock ordering is intentionally lifecycle -> flush gate. A normal
        // persistence root can hold the flush gate while waiting to open a
        // store; the bounded try_write below therefore skips after ~1s rather
        // than waiting forever while holding this exclusive lifecycle guard.
        trace.set_phase(FailedPhase::StoreLifecycleGate);
        let lifecycle_wait = trace.timer(TimingPhase::StoreLifecycleWait);
        let _rdf_store_lifecycle_guard = crate::rdf_store_service::rdf_store_lifecycle_gate()
            .write()
            .map_err(|_| "Oxigraph store lifecycle gate poisoned".to_string())?;
        drop(lifecycle_wait);

        // Acquire the plain-file flush gate NOW, unconditionally, before the
        // RDF checkpoint below, and carry it into step 2 without ever
        // releasing it in between — see this function's doc comment for the
        // torn-import-composition race this closes and why deriving this
        // ordering from any `IMPORT_ACTIVE` sample (past or present) is not
        // an option: the completion-ledger append (and every per-document
        // save) of ANY concurrent operation — an import or otherwise — takes
        // this identical gate, so once we hold it, such a write is provably
        // either (a) already fully landed before our wait started, in which
        // case every step that happens-before it in that operation's own
        // program order (e.g. an import's archive-wide RDF load, which
        // precedes its ledger append) is ALSO already landed, and the
        // checkpoint we are about to take will correctly capture it, or (b)
        // still blocked behind our held gate and therefore cannot appear in
        // the file walk we are about to run either. `Store::backup` remains
        // a live, engine-consistent checkpoint regardless of when it runs
        // relative to this gate, so holding the gate across it only costs
        // time (the checkpoint's own duration, added to the walk's — see
        // this function's doc comment for the disclosed, unmeasured cost of
        // that), never correctness.
        trace.set_phase(FailedPhase::FlushGateWait);
        let flush_gate_wait = trace.timer(TimingPhase::FlushGateWait);
        let early_gate_guard = match acquire_flush_gate(force, gate_wait)? {
            Some(guard) => guard,
            None => {
                drop(flush_gate_wait);
                trace.set_defer_reason(DeferReason::FlushGateContended);
                log::info!(
                    "durable flush skipped: write paths busy (gate contended for {gate_wait:?}); \
                     will retry next tick"
                );
                return Ok(FlushAttemptOutcome::Deferred);
            }
        };
        drop(flush_gate_wait);
        let flush_gate_held = trace.timer(TimingPhase::FlushGateHeld);

        // 1. Oxigraph stores — RocksDB checkpoint, now under the SAME held
        //    gate as step 2 (see above). Dirty open stores use a checkpoint;
        //    clean open stores are omitted from the build until we know
        //    another profile change actually requires publishing. Closed
        //    stores on disk still go through the plain walker.
        // A cache entry can outlive a directory that was detached before a
        // same-ID publication reached its eviction boundary. Never resurrect
        // that detached incarnation into the canonical path in a snapshot.
        trace.set_phase(FailedPhase::StoreBackup);
        let open_stores = open_store_paths()
            .into_iter()
            .filter(|(store_path, _)| store_path.is_dir())
            .collect::<Vec<_>>();
        #[cfg(test)]
        pause_after_rdf_store_enumeration_for_test();
        let mut clean_store_rels = Vec::new();
        for (store_path, store) in &open_stores {
            // A real cell only ever has its own graph's stores open, all under
            // the profile dir. A store outside it can only appear when an
            // unrelated process shares the global store registry (the test
            // harness, where many modules open stores) — skip it rather than
            // fail the whole flush.
            let Some(rel) = relative_to(profile_dir, store_path) else {
                continue;
            };
            let previous_store_dir = previous_dir.as_ref().map(|dir| dir.join(&rel));
            let plan = store_epoch_to_backup(
                profile_dir,
                store_path,
                store,
                previous_store_dir.as_deref(),
            );
            trace.record_store(plan.observation());
            if !plan.needs_backup {
                clean_store_rels.push(rel);
                continue;
            }
            // P5: checkpoint into LOCAL staging. `Store::backup` on the same
            // filesystem lets RocksDB hard-link its SSTs instead of streaming
            // O(store bytes) across EFS inside the hold. The checkpoint still
            // happens inside the same uninterrupted gate hold as the file
            // capture below, so R1 is untouched.
            let target = staging_dir.join(&rel);
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)
                    .map_err(|error| format!("create {}: {error}", display(parent)))?;
            }
            let store_backup = trace.timer(TimingPhase::StoreBackup);
            let backup_started = trace.enabled().then(Instant::now);
            store.backup(&target).map_err(|error| {
                format!("backup oxigraph store {}: {error}", display(store_path))
            })?;
            drop(store_backup);
            if let Some(backup_started) = backup_started {
                trace.complete_store_backup(
                    plan.slot_id,
                    plan.incarnation,
                    elapsed_ms(backup_started),
                );
            }
            outcome.stores_backed_up += 1;
            trace.observe_io(
                outcome.files_copied,
                outcome.files_linked,
                outcome.stores_backed_up,
                outcome.bytes_copied,
            );
            copy_registered_store_sidecars(store_path, &target, &mut outcome)?;
            trace.observe_io(
                outcome.files_copied,
                outcome.files_linked,
                outcome.stores_backed_up,
                outcome.bytes_copied,
            );
            backed_store_epochs.push((store_path.clone(), Arc::clone(store), plan));
        }
        let open_store_paths: Vec<PathBuf> = open_stores.iter().map(|(p, _)| p.clone()).collect();

        // 2. Everything else — under the SAME flush-gate write guard
        //    acquired before step 1 above. `early_gate_guard` is never
        //    released and reacquired between steps 1 and 2 — doing so would
        //    reopen exactly the torn-composition window this ordering exists
        //    to close.
        //
        // NEVER block indefinitely acquiring the gate in the first place: a
        // queued writer makes std RwLock hold back NEW readers, so any write
        // path that acquires a second read guard while holding one deadlocks
        // the whole cell (observed live: the chat-archive import froze at
        // its first flush tick — every smaller import simply finished
        // inside the 30s window). That is why acquisition above uses a
        // bounded `try_write` poll (periodic ticks retry briefly and skip;
        // a forced shutdown/post-import/RPO-ceiling flush retries to its
        // substantially longer deadline and returns an explicit error if
        // the write paths still have not drained — it must never disguise
        // that failure as an unchanged snapshot). Once acquired, though, the
        // guard is simply held across both steps; there is no second
        // acquisition here to bound.
        {
            let _guard = early_gate_guard;
            trace.set_phase(FailedPhase::Walk);
            let walk = trace.timer(TimingPhase::Walk);
            fs::create_dir_all(&staging_dir)
                .map_err(|error| format!("create {}: {error}", display(&staging_dir)))?;
            {
                // P5: capture into LOCAL staging, not onto EFS. This is the
                // whole point of the change — the gate now covers only
                // local-filesystem work (hard links at ~6 µs/inode), so the
                // hold is tens of milliseconds instead of the seconds of EFS
                // namespace operations it used to be. Nothing about the
                // consistency argument changes: capture and the RDF
                // checkpoints above still happen inside ONE uninterrupted
                // hold, so R1 (cross-subsystem happens-before) and R3
                // (document.json vs sidecar cannot skew) are preserved
                // exactly as before.
                let mut walker = FlushWalker {
                    profile_dir,
                    next_dir: &staging_dir,
                    previous_dir: None,
                    open_store_paths: &open_store_paths,
                    outcome: &mut capture_outcome,
                    link_from_source: true,
                };
                walker.copy_dir(profile_dir)?;
                // The walker skips whole registered roots. Authored store
                // sidecars are ordinary profile state and must still
                // participate in the dirty decision even when the RocksDB
                // contents are clean.
                for rel in &clean_store_rels {
                    walker.copy_store_sidecars(&profile_dir.join(rel))?;
                }
            }
            drop(walk);
        }
        drop(flush_gate_held);

        // ---- Everything below runs with NO gate held. ----
        //
        // Ship the frozen staging tree to EFS using today's
        // link-against-the-previous-generation logic. This is still O(inodes)
        // of EFS namespace work for a dirty flush, and that cost is disclosed
        // rather than hidden: it simply no longer blocks a document open, a
        // write, or a health check, because the gate was released above. v2's
        // log removes this tail entirely; P5 only de-fangs it.
        trace.set_phase(FailedPhase::Walk);
        {
            fs::create_dir_all(&build_dir)
                .map_err(|error| format!("create {}: {error}", display(&build_dir)))?;
            let mut walker = FlushWalker {
                profile_dir: &staging_dir,
                next_dir: &build_dir,
                previous_dir: previous_dir.as_deref(),
                // Staging already contains the checkpointed store directories
                // as ordinary files; nothing may be skipped on the way out.
                open_store_paths: &[],
                outcome: &mut outcome,
                link_from_source: false,
            };
            walker.copy_dir(&staging_dir)?;
        }
        trace.observe_io(
            outcome.files_copied,
            outcome.files_linked,
            outcome.stores_backed_up,
            outcome.bytes_copied,
        );

        // 3. Dirty-skip. Clean open stores are deliberately absent from the
        //    candidate tree, so compare the rest of the tree while ignoring
        //    their RocksDB files (but not authored sidecars).
        trace.set_phase(FailedPhase::Decision);
        let decision = trace.timer(TimingPhase::Decision);
        if let Some(prev_dir) = &previous_dir {
            if outcome.files_copied == 0
                && outcome.stores_backed_up == 0
                && trees_equal_ignoring_clean_stores(&build_dir, prev_dir, &clean_store_rels)
            {
                log::debug!(
                    "durable flush no-op: nothing changed since {} ({:?})",
                    previous.as_deref().unwrap_or("?"),
                    started.elapsed()
                );
                return Ok(FlushAttemptOutcome::ConfirmedClean);
            }

            // Another file changed, so a snapshot really will publish. Reuse
            // each clean store from the immutable previous snapshot without
            // touching live RocksDB or manufacturing a fresh checkpoint.
            for rel in &clean_store_rels {
                link_tree_missing(&prev_dir.join(rel), &build_dir.join(rel), &mut outcome)?;
            }
            trace.observe_io(
                outcome.files_copied,
                outcome.files_linked,
                outcome.stores_backed_up,
                outcome.bytes_copied,
            );
        }

        // 4. Refuse to publish a snapshot missing top-level profile entries —
        //    cheap insurance against any walk/skip regression silently losing
        //    a whole subtree (graphs/, metadata.oxigraph, ...).
        verify_snapshot_complete(profile_dir, &build_dir)?;
        trace.observe_io(
            outcome.files_copied,
            outcome.files_linked,
            outcome.stores_backed_up,
            outcome.bytes_copied,
        );
        drop(decision);
        Ok(FlushAttemptOutcome::Published)
    })();

    let decision = match result {
        Ok(decision) => decision,
        Err(error) => {
            // Preserve partial work testimony (for example, a long hard-link
            // walk that failed near its end) before discarding the candidate.
            trace.observe_io(
                outcome.files_copied,
                outcome.files_linked,
                outcome.stores_backed_up,
                outcome.bytes_copied,
            );
            let _ = fs::remove_dir_all(&build_dir);
            return Err(error);
        }
    };
    if !matches!(decision, FlushAttemptOutcome::Published) {
        trace.set_phase(FailedPhase::Cleanup);
        if let Err(error) = fs::remove_dir_all(&build_dir) {
            // The build dir may not exist at all on the contended-skip path
            // (no open stores backed up before the walk was skipped).
            if error.kind() != std::io::ErrorKind::NotFound {
                return Err(format!(
                    "remove unchanged snapshot {}: {error}",
                    display(&build_dir)
                ));
            }
        }
        if matches!(decision, FlushAttemptOutcome::ConfirmedClean) {
            lease_flush_marker.mark_ok();
        }
        return Ok((
            FlushOutcome {
                sequence: 0,
                ..FlushOutcome::default()
            },
            decision,
        ));
    }

    // 5. Publish under the cross-process lease fence.
    trace.set_phase(FailedPhase::Publish);
    //    Gate B (U8 spec §3.4) — the real fence: a conditional publish-intent
    //    write to the lease authority, immediately before the rename that
    //    would make this build visible to `hydrate`/a racing reader. Gate A
    //    above is a cached, zero-I/O check; the window between it and here is
    //    unbounded by construction (the EFS walk above has no timeout, and
    //    84s stalls have been observed in production — spec §1.6), so a
    //    cached check alone cannot be the fence: this CAS is.
    //    `// U1: delete with the snapshot flush path`.
    if let Some(lease) = crate::cell_lease::handle() {
        if let Err(error) = lease.publish(next_seq, crate::cell_lease::PublishPhase::Intent) {
            match lease.mode() {
                crate::cell_lease::LeaseMode::Enforce => {
                    log::warn!(
                        "durable flush FENCED at Gate B: publish_intent(seq={next_seq}) refused \
                         for lease {} epoch {} ({error:?}) — discarding the build dir, CURRENT \
                         untouched (lost work, never corruption)",
                        lease.graph_id(),
                        lease.epoch()
                    );
                    lease.mark_fenced_by_publish_error(error);
                    let _ = fs::remove_dir_all(&build_dir);
                    return Ok((
                        FlushOutcome {
                            sequence: 0,
                            ..FlushOutcome::default()
                        },
                        FlushAttemptOutcome::Fenced,
                    ));
                }
                crate::cell_lease::LeaseMode::Observe => {
                    lease.testify_observe_degraded(
                        crate::cell_lease::ObserveLeaseDegradation::GateBPublishIntent,
                    );
                    log::warn!(
                        "durable flush WOULD HAVE been fenced at Gate B (observe mode): \
                         publish_intent(seq={next_seq}) refused for lease {} epoch {} ({error:?}) \
                         — proceeding unfenced",
                        lease.graph_id(),
                        lease.epoch()
                    );
                }
            }
        }
    }

    // Claim the snapshot name (atomic; the name is fresh by construction),
    // publish CURRENT, then prune old snapshots and stale build dirs. Keep the
    // existing publish timer scoped to local durable-plane publication; the
    // Gate B authority round trip above is not part of that established metric.
    let publish = trace.timer(TimingPhase::Publish);
    fs::rename(&build_dir, &next_dir).map_err(|error| {
        format!(
            "claim snapshot {} -> {}: {error}",
            display(&build_dir),
            display(&next_dir)
        )
    })?;
    publish_current(durable_dir, &next_name)?;
    outcome.published = true;
    trace.set_snapshot_sequence(next_seq);
    drop(publish);

    // The build is now VISIBLE on the durable plane (renamed and named by
    // CURRENT), whatever Gate C decides below. If the commit is refused the
    // standing snapshot becomes the successor's repair carrier (boot repair
    // `S == P` → `AcceptPending`), so the epochs it captures must never be
    // reported "revoked" by a later fence stroke — record their plane
    // visibility BEFORE Gate C can latch that stroke. Same epoch discipline
    // as the resolved watermark: only what was captured before the dirty
    // computation is claimed. On the ordinary Published path the resolved
    // watermark catches up to this same value in the outer funnel.
    PLANE_VISIBLE_EPOCH.fetch_max(receipt_epoch_before, Ordering::SeqCst);

    // Gate C (U8 spec §3.4) — `publish_commit` after `CURRENT` is rewritten.
    // Unlike Gate B, a failure here cannot be undone: the tree has already
    // advanced locally. It must nevertheless NOT acknowledge captured dirty
    // epochs or report Published. Any HTTP 409 here is positive authority
    // inconsistency and becomes terminal immediately (platform-next's current
    // `lease_contended` body does not include an epoch); an unavailable
    // authority remains a recoverable fence. The successor's boot repair
    // reconciles the local pointer. `// U1: delete with the snapshot flush
    // path`.
    if let Some(lease) = crate::cell_lease::handle() {
        if let Err(error) = lease.publish(next_seq, crate::cell_lease::PublishPhase::Commit) {
            match lease.mode() {
                crate::cell_lease::LeaseMode::Enforce => {
                    log::error!(
                        "durable flush Gate C publish_commit(seq={next_seq}) failed for lease {} \
                         epoch {} ({error:?}) — CURRENT already advanced to {next_name}; fencing \
                         this incarnation, the successor's boot repair reconciles via S==P",
                        lease.graph_id(),
                        lease.epoch()
                    );
                    lease.mark_terminal_by_commit_refusal(error);
                    outcome.published = false;
                    return Ok((outcome, FlushAttemptOutcome::Fenced));
                }
                crate::cell_lease::LeaseMode::Observe => {
                    lease.testify_observe_degraded(
                        crate::cell_lease::ObserveLeaseDegradation::GateCPublishCommit,
                    );
                    log::warn!(
                        "durable flush Gate C WOULD HAVE fenced (observe mode): \
                         publish_commit(seq={next_seq}) failed for lease {} epoch {} ({error:?})",
                        lease.graph_id(),
                        lease.epoch()
                    );
                }
            }
        }
    }

    // Acknowledge only the epochs captured before their checkpoints. Any write
    // that completed during backup has a greater epoch and remains dirty.
    for (store_path, store, plan) in &backed_store_epochs {
        if let Some(backed_up_epoch_after) = acknowledge_store_backup(store_path, store, plan) {
            trace.acknowledge_store(plan.slot_id, plan.incarnation, backed_up_epoch_after);
        }
    }

    trace.set_phase(FailedPhase::Prune);
    let prune = trace.timer(TimingPhase::Prune);
    let authority_snapshot = crate::cell_lease::handle()
        .and_then(|lease| lease.authority_last_snap())
        .filter(|seq| *seq > 0)
        .map(|seq| format!("{SNAP_PREFIX}{seq:06}"));
    prune_snapshots(
        durable_dir,
        &next_name,
        previous.as_deref(),
        authority_snapshot.as_deref(),
    )?;
    drop(prune);

    log::info!(
        "durable flush published {next_name}: {} files copied, {} hard-linked, {} stores backed up, {} bytes, in {:?}",
        outcome.files_copied,
        outcome.files_linked,
        outcome.stores_backed_up,
        outcome.bytes_copied,
        started.elapsed()
    );
    lease_flush_marker.mark_ok();
    Ok((outcome, FlushAttemptOutcome::Published))
}

fn acquire_flush_gate(
    force: bool,
    wait: Duration,
) -> Result<Option<std::sync::RwLockWriteGuard<'static, ()>>, String> {
    let started = Instant::now();
    loop {
        match flush_gate().try_write() {
            Ok(guard) => return Ok(Some(guard)),
            Err(std::sync::TryLockError::Poisoned(_)) => {
                return Err("durable flush gate poisoned".to_string());
            }
            Err(std::sync::TryLockError::WouldBlock) => {
                let elapsed = started.elapsed();
                if elapsed >= wait {
                    if force {
                        return Err(format!(
                            "forced durable flush timed out after {wait:?} waiting for active write paths"
                        ));
                    }
                    return Ok(None);
                }
                std::thread::sleep(FLUSH_GATE_RETRY_INTERVAL.min(wait - elapsed));
            }
        }
    }
}

/// Enumerate open oxigraph stores (profile metadata + per-graph) as
/// `(store_path, store)`. Indirected so tests can exercise the file machinery
/// without a live registry.
fn open_store_paths() -> Vec<(PathBuf, std::sync::Arc<oxigraph::store::Store>)> {
    let mut stores = crate::profile_rdf_store_service::open_profile_metadata_stores();
    stores.extend(crate::rdf_store_service::open_graph_stores());
    stores.extend(crate::omphalos::open_omphalos_stores());
    stores
}

#[derive(Clone)]
struct StoreBackupPlan {
    needs_backup: bool,
    dirty_epoch: u64,
    backed_up_epoch_before: u64,
    slot_id: u64,
    incarnation: u64,
    store_kind: StoreKind,
    evidence: DirtyEvidenceSnapshot,
}

impl StoreBackupPlan {
    fn observation(&self) -> StoreObservation {
        StoreObservation {
            slot_id: self.slot_id,
            incarnation: self.incarnation,
            store_kind: self.store_kind,
            decision: if self.needs_backup {
                StoreDecision::Backup
            } else {
                StoreDecision::ReuseClean
            },
            dirty_epoch: self.dirty_epoch,
            backed_up_epoch_before: self.backed_up_epoch_before,
            backed_up_epoch_after: self.backed_up_epoch_before,
            backup_ms: 0,
            evidence: self.evidence.clone(),
        }
    }
}

/// Return the exact dirty epoch and bounded evidence this flush observed for a
/// store. `needs_backup` is false only when the same live identity has already
/// been backed up through its latest completed Garden write. Missing prior data
/// and replacement identities remain dirty exactly as before; the extra fields
/// are diagnostic only.
fn store_epoch_to_backup(
    profile_dir: &Path,
    store_path: &Path,
    store: &Arc<oxigraph::store::Store>,
    previous_store_dir: Option<&Path>,
) -> StoreBackupPlan {
    let store_kind = StoreKind::classify(profile_dir, store_path);
    let trace = tracing_enabled();
    if STORE_DIRTY_UNKNOWN.load(Ordering::Acquire) {
        let dirty_epoch = next_store_write_epoch();
        let mut evidence = DirtyEvidence::default();
        if trace {
            evidence.record(
                DirtyReason::BookkeepingUnknown,
                DirtyOrigin::DurabilityInternal,
                None,
            );
        }
        return StoreBackupPlan {
            needs_backup: true,
            dirty_epoch,
            backed_up_epoch_before: 0,
            slot_id: if trace { next_store_slot_id() } else { 0 },
            incarnation: 0,
            store_kind,
            evidence: if trace {
                evidence.pending_snapshot()
            } else {
                DirtyEvidenceSnapshot::default()
            },
        };
    }
    let Ok(mut bookkeeping) = store_backup_epochs().lock() else {
        STORE_DIRTY_UNKNOWN.store(true, Ordering::Release);
        let dirty_epoch = next_store_write_epoch();
        let mut evidence = DirtyEvidence::default();
        if trace {
            evidence.record(
                DirtyReason::BookkeepingUnknown,
                DirtyOrigin::DurabilityInternal,
                None,
            );
        }
        return StoreBackupPlan {
            needs_backup: true,
            dirty_epoch,
            backed_up_epoch_before: 0,
            slot_id: if trace { next_store_slot_id() } else { 0 },
            incarnation: 0,
            store_kind,
            evidence: if trace {
                evidence.pending_snapshot()
            } else {
                DirtyEvidenceSnapshot::default()
            },
        };
    };
    // A cold process has no live store identity to carry across hydrate. The
    // profile seed proves that this exact relative store directory was copied
    // from the immutable snapshot that is still `previous_store_dir`, and the
    // global epoch equality proves no Garden persistence completion landed
    // between that copy and this first observation. This is the only path that
    // may initialize a fresh identity as already backed up.
    let hydrated_clean_epoch = if STORE_DIRTY_UNKNOWN.load(Ordering::Acquire) {
        None
    } else {
        bookkeeping
            .hydrated_profiles
            .get(profile_dir)
            .and_then(|seed| {
                let relative = relative_to(profile_dir, store_path)?;
                let expected_previous = seed.snapshot_dir.join(relative);
                (previous_store_dir == Some(expected_previous.as_path())
                    && expected_previous.is_dir()
                    && current_write_epoch() == seed.clean_epoch)
                    .then_some(seed.clean_epoch)
            })
    };
    let stores = &mut bookkeeping.stores;
    let mut first_observed = false;
    let mut first_observed_hydrated_clean = false;
    let state = match stores.entry(store_path.to_path_buf()) {
        std::collections::btree_map::Entry::Occupied(entry) => entry.into_mut(),
        std::collections::btree_map::Entry::Vacant(entry) => {
            first_observed = true;
            first_observed_hydrated_clean = hydrated_clean_epoch.is_some();
            let dirty = hydrated_clean_epoch.unwrap_or_else(next_store_write_epoch);
            entry.insert(StoreBackupEpoch {
                identity: Arc::downgrade(store),
                dirty,
                backed_up: hydrated_clean_epoch.unwrap_or(0),
                slot_id: if trace { next_store_slot_id() } else { 0 },
                incarnation: 1,
                evidence: DirtyEvidence::default(),
            })
        }
    };
    if trace && first_observed && !first_observed_hydrated_clean {
        state.evidence.record(
            DirtyReason::StoreFirstObserved,
            DirtyOrigin::DurabilityInternal,
            None,
        );
    }
    if !same_store(&state.identity, store) {
        let dirty = next_store_write_epoch();
        let slot_id = state.slot_id;
        let incarnation = state.incarnation.saturating_add(1);
        *state = StoreBackupEpoch {
            identity: Arc::downgrade(store),
            dirty,
            backed_up: 0,
            slot_id,
            incarnation,
            evidence: DirtyEvidence::default(),
        };
        if trace {
            state.evidence.record(
                DirtyReason::StoreIdentityReplaced,
                DirtyOrigin::DurabilityInternal,
                None,
            );
        }
    }
    if !previous_store_dir.is_some_and(Path::is_dir) {
        state.dirty = next_store_write_epoch();
        if trace {
            state.evidence.record(
                DirtyReason::PreviousSnapshotMissing,
                DirtyOrigin::DurabilityInternal,
                None,
            );
        }
    }
    StoreBackupPlan {
        needs_backup: state.dirty > state.backed_up,
        dirty_epoch: state.dirty,
        backed_up_epoch_before: state.backed_up,
        slot_id: state.slot_id,
        incarnation: state.incarnation,
        store_kind,
        evidence: if trace {
            state.evidence.pending_snapshot()
        } else {
            DirtyEvidenceSnapshot::default()
        },
    }
}

fn acknowledge_store_backup(
    store_path: &Path,
    store: &Arc<oxigraph::store::Store>,
    plan: &StoreBackupPlan,
) -> Option<u64> {
    let trace = tracing_enabled();
    if STORE_DIRTY_UNKNOWN.load(Ordering::Acquire) {
        return None;
    }
    let Ok(mut bookkeeping) = store_backup_epochs().lock() else {
        STORE_DIRTY_UNKNOWN.store(true, Ordering::Release);
        return None;
    };
    let Some(state) = bookkeeping.stores.get_mut(store_path) else {
        STORE_DIRTY_UNKNOWN.store(true, Ordering::Release);
        return None;
    };
    if same_store(&state.identity, store) {
        state.backed_up = state.backed_up.max(plan.dirty_epoch);
        if trace && state.slot_id == plan.slot_id && state.incarnation == plan.incarnation {
            state.evidence.acknowledge(&plan.evidence);
        }
        Some(state.backed_up)
    } else {
        None
    }
}

fn snapshot_global_dirty_evidence() -> DirtyEvidenceSnapshot {
    if !tracing_enabled() {
        return DirtyEvidenceSnapshot::default();
    }
    match store_backup_epochs().lock() {
        Ok(bookkeeping) => bookkeeping.global_evidence.pending_snapshot(),
        Err(_) => {
            let mut evidence = DirtyEvidence::default();
            evidence.record(
                DirtyReason::BookkeepingUnknown,
                DirtyOrigin::DurabilityInternal,
                None,
            );
            evidence.pending_snapshot()
        }
    }
}

fn acknowledge_global_dirty_evidence(snapshot: &DirtyEvidenceSnapshot) {
    if !tracing_enabled() || STORE_DIRTY_UNKNOWN.load(Ordering::Acquire) {
        return;
    }
    if let Ok(mut bookkeeping) = store_backup_epochs().lock() {
        bookkeeping.global_evidence.acknowledge(snapshot);
    }
}

/// Read the current global write-completion epoch without doing any I/O.
///
/// This is the SAME monotonic counter [`next_store_write_epoch`] hands out to
/// every real persistence completion in the process: the narrow RDF hooks
/// ([`mark_rdf_store_written`]), the coarse per-graph fallback
/// ([`mark_graph_rdf_stores_written_with_context`], driven by the writer
/// leases' Drop — `HotWriteLease`/`ExclusiveLease`; a `SharedLease` never marks),
/// and the general non-RDF write gate (`CellDurabilityWriteGuard::drop` via
/// `write_guard()`, which every synchronous persistence transaction —
/// including `Room::persist_state_bytes`, the Y.Doc state-file writer — holds
/// for its commit). A caller cannot tell *what* changed from this value alone,
/// only *whether anything did* since a previously-observed reading.
///
/// [`DirtyFlushScheduler`] uses exactly this coarseness on purpose: it exists
/// only to decide WHEN a flush attempt is worth making, never WHAT a flush
/// should back up. `flush`/`flush_forced` remain the sole authority on that,
/// via their own per-store epoch bookkeeping and full tree comparison. An
/// over-eager wake here (the epoch moved because of an operation that turns
/// out to touch nothing tracked) costs one wasted no-op flush attempt; it can
/// never cause a real dirty store to go unflushed, because nothing about this
/// reading suppresses `flush`'s own dirty computation.
pub fn current_write_epoch() -> u64 {
    STORE_WRITE_EPOCH.load(Ordering::Acquire)
}

/// Read the durable-plane receipt watermark — see [`DURABLY_RESOLVED_EPOCH`].
///
/// `durably_resolved_epoch() >= e` means every Garden write whose completion
/// mark landed at or before epoch `e` is covered by an actual durable-plane
/// flush completion. The write path uses this to decide whether a
/// `durabilityChecked` claim can honestly be paid; it must NEVER be derived
/// from anything the caller requested.
pub fn durably_resolved_epoch() -> u64 {
    DURABLY_RESOLVED_EPOCH.load(Ordering::Acquire)
}

/// Read the fence-discard revocation watermark — see [`DURABLY_REVOKED_EPOCH`].
///
/// Two layers, both paid by the same discard event (the enforce-mode terminal
/// latch), never by anything a caller requested:
///
/// * the value latched by [`record_fence_discard_revocation`] in the same
///   stroke as the latch — covering every write epoch allocated before it;
/// * while the process-global lease stands terminal in enforce mode, the
///   current write epoch — covering writes that were admitted before the
///   latch but completed after it. The latch is permanent
///   (`OnceLock`-backed) and the write epoch monotone, so this extension is
///   itself monotone and truthful: a terminal incarnation can never publish
///   anything again (Gate A refuses every attempt), so any epoch it
///   completes is equally stranded.
pub fn durably_revoked_epoch() -> u64 {
    let latched = DURABLY_REVOKED_EPOCH.load(Ordering::SeqCst);
    let standing = crate::cell_lease::handle()
        .filter(|lease| {
            lease.mode() == crate::cell_lease::LeaseMode::Enforce
                && lease.terminal_reason().is_some()
        })
        .map(|_| current_write_epoch())
        .unwrap_or(0);
    latched.max(standing)
}

/// The same-stroke revocation (Lane 2 repair, 2026-08-21): called by
/// `cell_lease::WriteLease::set_terminal` in enforce mode, in the same
/// stroke as the latch that makes this incarnation's fence permanent —
/// which is exactly the moment its acked-but-unflushed writes are
/// discarded (Gate A will refuse every future flush, the router every
/// request, and Gardend the final flush). Idempotent and monotone; the
/// receipt is paid from the discard event itself, never from caller input.
pub(crate) fn record_fence_discard_revocation(graph_id: &str, lease_epoch: u64) {
    let stranded_through = current_write_epoch();
    let previous = DURABLY_REVOKED_EPOCH.fetch_max(stranded_through, Ordering::SeqCst);
    if previous < stranded_through {
        let covered = durably_resolved_epoch().max(PLANE_VISIBLE_EPOCH.load(Ordering::SeqCst));
        log::error!(
            "write lease for {graph_id} epoch {lease_epoch} went TERMINAL: acked-but-unflushed \
             write epochs ({covered},{stranded_through}] are DISCARDED — revocation recorded in \
             the same stroke; durability inquiries for the range now answer \"revoked\" instead \
             of pending-forever (2026-08-21 repair)"
        );
    }
}

/// The watermark bundle the write path's durability verdict consumes —
/// see `document_mcp_write_payloads::write_durability_verdict` for the
/// precedence (resolved covers, then plane-visible shields, then revoked
/// answers, else pending).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DurabilityWatermarks {
    /// [`DURABLY_RESOLVED_EPOCH`] — commit-backed coverage.
    pub resolved: u64,
    /// [`PLANE_VISIBLE_EPOCH`] — standing published-but-uncommitted
    /// snapshot coverage (the successor's repair carrier).
    pub plane_visible: u64,
    /// [`durably_revoked_epoch`] — fence-discarded range.
    pub revoked: u64,
}

/// Read the three watermarks for one verdict.
///
/// Read order matters and is deliberate: `revoked` FIRST, then the two
/// covering watermarks, all `SeqCst`. The revocation stroke's writer orders
/// plane-visible (set at publish) before revoked (set at the terminal
/// latch), so a reader that observes a revocation is guaranteed to also
/// observe every cover that preceded it — the failure mode this forbids is
/// answering "revoked" for an epoch whose plane-visible cover was written
/// but not yet seen. The reverse staleness (seeing a cover but a stale
/// `revoked`) only ever under-claims to "pending", which is the safe side.
pub fn durability_watermarks() -> DurabilityWatermarks {
    let revoked = durably_revoked_epoch();
    let plane_visible = PLANE_VISIBLE_EPOCH.load(Ordering::SeqCst);
    let resolved = DURABLY_RESOLVED_EPOCH.load(Ordering::SeqCst);
    DurabilityWatermarks {
        resolved,
        plane_visible,
        revoked,
    }
}

/// Whether this process's durability ground is a durable *plane* (snapshots
/// published to a durable dir behind the Gate A/B/C lease protocol) rather
/// than the local profile dir itself.
///
/// Headless builds (gardend cells) always answer `true`: even a cell booted
/// without durable dirs configured, or serving in the window before
/// [`set_durable_dirs`] runs, must be held to plane semantics — its pod-local
/// disk is ephemeral, so a "durable" claim there that is not backed by a
/// published snapshot would be false (fail-closed: such a cell simply never
/// pays the claim). Desktop builds answer `true` only if something registered
/// durable dirs; otherwise the local profile dir on the user's disk is the
/// durable ground and there is no later publication step to await.
pub fn durable_plane_semantics() -> bool {
    cfg!(feature = "headless") || DURABLE_DIRS.get().is_some_and(Option::is_some)
}

/// Pure decision: what epoch (if any) does a finished flush attempt resolve
/// as durably covered? `epoch_before` MUST be the [`current_write_epoch`]
/// value captured after the attempt owned `FLUSH_SERIAL` and before its dirty
/// computation began. Mirrors [`DirtyFlushScheduler::record_flush_attempt`]:
/// only `Published` and `ConfirmedClean` resolve anything; a deferred,
/// fenced, or errored attempt confirmed nothing and must leave the watermark
/// untouched (advancing it there would be exactly the 2026-08-21 false-ack
/// shape at the plane level).
fn durably_resolved_epoch_after_attempt(
    epoch_before: u64,
    result: &Result<(FlushOutcome, FlushAttemptOutcome), String>,
    lease_doubted: bool,
) -> Option<u64> {
    if lease_doubted {
        // Observe-mode lease doubt during the attempt: the snapshot may have
        // been published unfenced past a lost lease (the 08-21 shape). The
        // bytes may well be fine, but the claim cannot be paid — withhold.
        return None;
    }
    match result {
        Ok((_, FlushAttemptOutcome::Published | FlushAttemptOutcome::ConfirmedClean)) => {
            Some(epoch_before)
        }
        Ok((_, FlushAttemptOutcome::Deferred | FlushAttemptOutcome::Fenced)) | Err(_) => None,
    }
}

/// What [`DirtyFlushScheduler::poll`] wants the caller to do this tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SchedulerAction {
    /// Nothing pending since the last flush attempt (or nothing pending yet
    /// has waited long enough) — sleep until the next poll tick.
    Wait,
    /// The debounce window quieted down with no further writes — attempt an
    /// ordinary (non-forced) flush via [`flush_scheduled`] with `force: false`.
    /// An import-active or gate-contended [`FlushAttemptOutcome::Deferred`]
    /// here is fine: the RPO ceiling has not been reached yet, so there is no
    /// bound being violated by deferring once more.
    FlushNow,
    /// The max-RPO ceiling was reached: real pending dirtiness has now waited
    /// as long as this scheduler is allowed to let it wait, REGARDLESS of
    /// whether a heavy import is active or the write-path gate is contended.
    ///
    /// The caller MUST call [`flush_scheduled`] with `force: true` here, not
    /// `false` — an ordinary attempt returns `Deferred` unconditionally while
    /// `IMPORT_ACTIVE > 0` (see `flush_inner`), which would let a concurrent
    /// acknowledged write during a long import (imports run up to ~90s) sit
    /// unpublished for the entire import, silently breaking the max-RPO bound
    /// this scheduler exists to guarantee. Forcing here is safe: it is the
    /// exact same code path already used for the SIGTERM final flush and the
    /// post-import flush, both of which already run "even while an import is
    /// active" on the stated rationale that "the import is replayable; losing
    /// the just-captured state is worse" (see `flush_forced`'s doc comment) —
    /// capturing a snapshot mid-import produces, at worst, a partial import
    /// state on restart (the import resumes/redoes), never a corrupt one:
    /// `verify_snapshot_complete` only requires top-level profile entries to
    /// exist, which an in-progress import never removes.
    FlushAtRpoCeiling,
}

/// Dirty-driven flush scheduling.
///
/// Replaces "flush unconditionally every fixed interval" with "flush shortly
/// after real activity, debounced, bounded by an RPO ceiling":
///
/// NOTE on "ceiling": the `max_rpo` bound is a *target* under normal
/// conditions, NOT a provable mathematical ceiling. When the ceiling is
/// reached the caller forces the attempt, but that forced flush can still be
/// defeated by sustained flush-gate contention (its acquisition budget times
/// out — 1s periodic / 20s forced). In that case the pending write stays
/// dirty and is retried on the next tick; the RPO target may be exceeded under
/// pathological contention, but this NEVER produces a false-CLEAN (nothing is
/// marked resolved and no watermark advances).
///
/// * An **idle** cell (the global write epoch never advances) never reaches
///   [`SchedulerAction::FlushNow`] — no flush attempt is ever made, so the
///   periodic task pays no RDF-store-enumeration or tree-walk cost at all
///   while quiescent (today's fixed ticker pays that cost every tick
///   regardless of dirtiness; only *publishing* was skippable).
/// * A **bursty** cell (several writes close together) coalesces into one
///   flush shortly after the burst quiets down (the debounce window), rather
///   than however many fixed ticks the burst happened to straddle.
/// * A **continuously busy** cell (writes keep landing faster than the
///   debounce can quiesce) still flushes at least every `max_rpo`, bounding
///   worst-case crash loss exactly as the old fixed interval did — INCLUDING
///   while a heavy import is active (see [`SchedulerAction::FlushAtRpoCeiling`]):
///   without that escalation, a concurrent acknowledged write could sit
///   unpublished for the whole ~90s import, breaking the RPO bound.
///
/// This type is a pure function of its inputs — it never reads the system
/// clock or the global epoch itself ([`poll`](Self::poll) takes both as
/// parameters) — so tests can drive every case (idle, debounced, max-RPO
/// forced, mid-flush write) with synthetic instants and epoch values, with no
/// real or `tokio::time`-simulated sleeping required.
///
/// SAFETY ARGUMENT (why this can never cause a false-CLEAN skip): the
/// scheduler's only power is deciding *when* to call `flush`; it never
/// substitutes its own judgment for `flush`'s dirty computation, and it never
/// discards an epoch bump. Every branch of [`poll`](Self::poll) either (a)
/// correctly recognizes nothing is pending (`current_epoch == last flushed
/// epoch`, meaning no `mark_*` call has fired since), or (b) treats *any*
/// pending epoch as needing an eventual flush and can only delay that flush
/// up to `max_rpo` past when it was first observed — and once `max_rpo` is
/// reached, the caller is required to force the attempt
/// ([`SchedulerAction::FlushAtRpoCeiling`]), which bypasses the import-active
/// deferral. Forcing removes the import-active deferral as a cause of unbounded
/// delay, but does not make the ceiling a hard bound: a forced attempt can
/// still fail under sustained gate contention (see the ceiling NOTE above), in
/// which case the write stays dirty and is retried. A flush skipped under
/// contention or import-active — before OR at the RPO ceiling — is retried by
/// the caller on the next tick exactly as before; the scheduler does not change
/// that retry behavior, only the cadence at which ticks are worth acting on. In
/// no case is a pending write marked resolved without actually being published.
pub struct DirtyFlushScheduler {
    debounce: Duration,
    max_rpo: Duration,
    /// Epoch value as of the start of the last flush *attempt* (whether it
    /// published or genuinely no-opped). Anything strictly greater than this
    /// is unflushed.
    last_flushed_epoch: u64,
    /// Epoch value observed at the previous [`poll`](Self::poll) call, used
    /// only to detect "did anything change since the last time we looked" so
    /// the debounce deadline can be pushed out on continued activity.
    last_polled_epoch: u64,
    /// When the currently-pending epoch range was first observed unflushed.
    first_pending_at: Option<Instant>,
    /// Most recent poll at which the epoch had advanced since the previous
    /// poll — the debounce deadline is measured from here.
    last_dirty_at: Option<Instant>,
}

impl DirtyFlushScheduler {
    /// Construct a scheduler starting from `current_epoch` (read via
    /// [`current_write_epoch`] by the caller) as the initial "nothing pending
    /// yet" baseline.
    pub fn new(debounce: Duration, max_rpo: Duration, current_epoch: u64) -> Self {
        Self {
            debounce,
            max_rpo,
            last_flushed_epoch: current_epoch,
            last_polled_epoch: current_epoch,
            first_pending_at: None,
            last_dirty_at: None,
        }
    }

    /// Call on every poll tick with the current time and the latest observed
    /// global write epoch ([`current_write_epoch`]).
    pub fn poll(&mut self, now: Instant, current_epoch: u64) -> SchedulerAction {
        // DURABILITY AUDIT FINDING (2026-07-18, re-refute): `STORE_DIRTY_UNKNOWN`
        // is a sticky, never-reset fail-safe (lock poisoning, or the
        // astronomically unlikely u64 write-epoch exhaustion). In the
        // exhaustion case specifically, `STORE_WRITE_EPOCH` can no longer
        // advance at all once saturated — `current_epoch` could never again
        // exceed `last_flushed_epoch` by epoch comparison alone, meaning this
        // scheduler could get permanently stuck believing nothing is pending
        // while the flusher's own fail-safe considers every store dirty.
        // Consult the flag directly, unconditionally, so that can't happen:
        // once it is set, every poll demands an immediate, forced flush
        // (skipping debounce entirely — this is a degraded, "something might
        // already be wrong" state where safety dominates cadence).
        if STORE_DIRTY_UNKNOWN.load(Ordering::Acquire) {
            return SchedulerAction::FlushAtRpoCeiling;
        }
        if current_epoch != self.last_polled_epoch {
            self.last_dirty_at = Some(now);
            self.last_polled_epoch = current_epoch;
        }
        if current_epoch <= self.last_flushed_epoch {
            // Nothing pending since the last flush attempt started.
            return SchedulerAction::Wait;
        }
        let first_pending_at = *self.first_pending_at.get_or_insert(now);
        let debounce_deadline = self.last_dirty_at.unwrap_or(first_pending_at) + self.debounce;
        let rpo_deadline = first_pending_at + self.max_rpo;
        // The RPO ceiling takes priority when both have elapsed: it is the
        // stronger action (forces through import-active/gate-contention,
        // where an ordinary FlushNow would not), and it is always safe to
        // force when an ordinary flush would also have been attempted.
        if now >= rpo_deadline {
            SchedulerAction::FlushAtRpoCeiling
        } else if now >= debounce_deadline {
            SchedulerAction::FlushNow
        } else {
            SchedulerAction::Wait
        }
    }

    /// Record that a flush attempt just ran, having captured `epoch_before`
    /// via [`current_write_epoch`] *before* the flush began, and report which
    /// [`FlushAttemptOutcome`] it reached (use [`flush_scheduled`], not
    /// `flush`/`flush_forced`, to get one).
    ///
    /// Only [`FlushAttemptOutcome::Published`] and
    /// [`FlushAttemptOutcome::ConfirmedClean`] advance the watermark — both
    /// mean the full dirty computation ran to completion, so nothing at or
    /// before `epoch_before` remains unaccounted for (any write that lands
    /// *during* the flush itself bumps the epoch past `epoch_before`, so it
    /// correctly remains pending — this mirrors `flush`'s own store-epoch
    /// acknowledgement discipline: "acknowledge only the epochs captured
    /// before their checkpoints").
    ///
    /// [`FlushAttemptOutcome::Deferred`] deliberately does NOT advance
    /// anything: the attempt returned before completing the dirty
    /// computation (import active, or the write-path gate was contended past
    /// its wait budget), so treating `epoch_before` as resolved would be a
    /// false-CLEAN — the pending state is left exactly as it was, and the
    /// next poll retries (immediately, if the debounce/max-RPO deadline has
    /// already passed).
    ///
    /// [`FlushAttemptOutcome::Fenced`] is treated the same as `Deferred`, for
    /// the same reason: the lease refused before (or instead of) confirming
    /// anything, so `epoch_before` must stay pending. Recovery is not this
    /// scheduler's job — either the lease un-fences on a later renew (the
    /// next poll simply retries and succeeds), or the process is terminal
    /// and about to `exit(4)`, in which case no scheduler state survives it
    /// anyway.
    pub fn record_flush_attempt(&mut self, epoch_before: u64, outcome: FlushAttemptOutcome) {
        match outcome {
            FlushAttemptOutcome::Published | FlushAttemptOutcome::ConfirmedClean => {
                self.last_flushed_epoch = self.last_flushed_epoch.max(epoch_before);
                self.first_pending_at = None;
            }
            FlushAttemptOutcome::Deferred | FlushAttemptOutcome::Fenced => {}
        }
    }
}

/// Copy authored files colocated with a registered RocksDB root.
///
/// Omphalos intentionally keeps `constitution.ttl` beside its database. The
/// checkpoint captures only RocksDB files, while the plain walker skips the
/// whole live store root. Preserve that authored source explicitly so hydrate
/// does not silently replace it with a compiled default.
fn copy_registered_store_sidecars(
    store_path: &Path,
    target: &Path,
    outcome: &mut FlushOutcome,
) -> Result<(), String> {
    for filename in ["constitution.ttl"] {
        let source = store_path.join(filename);
        if !source.is_file() {
            continue;
        }
        let destination = target.join(filename);
        let bytes = fs::copy(&source, &destination).map_err(|error| {
            format!(
                "copy Oxigraph sidecar {} -> {}: {error}",
                display(&source),
                display(&destination)
            )
        })?;
        outcome.files_copied += 1;
        outcome.bytes_copied += bytes;
    }
    Ok(())
}

#[cfg(test)]
struct RdfStoreEnumerationPause {
    reached: std::sync::mpsc::Sender<()>,
    release: std::sync::mpsc::Receiver<()>,
}

#[cfg(test)]
static RDF_STORE_ENUMERATION_PAUSE: OnceLock<Mutex<Option<RdfStoreEnumerationPause>>> =
    OnceLock::new();

#[cfg(test)]
fn install_rdf_store_enumeration_pause_for_test(
    reached: std::sync::mpsc::Sender<()>,
    release: std::sync::mpsc::Receiver<()>,
) {
    *RDF_STORE_ENUMERATION_PAUSE
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) =
        Some(RdfStoreEnumerationPause { reached, release });
}

#[cfg(test)]
fn pause_after_rdf_store_enumeration_for_test() {
    let pause = RDF_STORE_ENUMERATION_PAUSE
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .take();
    if let Some(pause) = pause {
        let _ = pause.reached.send(());
        let _ = pause.release.recv();
    }
}

/// Test-only rendezvous for the 2026-07-18 re-refute's finding #1
/// reproduction: pauses `mark_rdf_store_written` AFTER it has bumped the
/// global write epoch but BEFORE it writes the corresponding per-store
/// `dirty` field — while STILL HOLDING `store_backup_epochs`'s lock. A
/// concurrent flush's `store_epoch_to_backup` needs that same lock, so with
/// the fix (lock acquired before the epoch bump, held across both) it must
/// block here rather than observing the epoch already moved with the
/// dirty-map not yet caught up.
#[cfg(test)]
struct MarkDirtyPause {
    reached: std::sync::mpsc::Sender<()>,
    release: std::sync::mpsc::Receiver<()>,
}

#[cfg(test)]
static MARK_DIRTY_PAUSE: OnceLock<Mutex<Option<MarkDirtyPause>>> = OnceLock::new();

#[cfg(test)]
fn install_mark_dirty_pause_for_test(
    reached: std::sync::mpsc::Sender<()>,
    release: std::sync::mpsc::Receiver<()>,
) {
    *MARK_DIRTY_PAUSE
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) =
        Some(MarkDirtyPause { reached, release });
}

#[cfg(test)]
fn pause_inside_mark_dirty_for_test() {
    let pause = MARK_DIRTY_PAUSE
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .take();
    if let Some(pause) = pause {
        let _ = pause.reached.send(());
        let _ = pause.release.recv();
    }
}

/// Files a snapshot must never hard-link, only byte-copy.
///
/// A hard link aliases the source inode. That is exactly what makes it a
/// permanent freeze for an atomic-rename writer (rename replaces the directory
/// entry, leaving our link pointing at the frozen original) and exactly what
/// makes it *corruption* for an in-place writer: subsequent `pwrite`s mutate
/// the very bytes the published snapshot claims to have captured. The gate
/// audit's R2 refutation is this list; anything unproven belongs here, because
/// the failure mode of copying an atomic writer is a few wasted bytes and the
/// failure mode of linking an in-place writer is a silently corrupt snapshot.
///
/// This also closes the live growing-snapshot defect: an unrotated `.jsonl`
/// hard-linked into a snapshot keeps growing after publication.
fn never_hardlink(name: &str) -> bool {
    name.ends_with(".jsonl")
        || name.ends_with(".turso")
        || name.ends_with(".turso-wal")
        || name.ends_with(".turso-tshm")
        || name.contains(".turso-")
        || name.ends_with(".ttl")
        || name.ends_with(".db")
        || name.ends_with(".db-wal")
        || name.ends_with(".db-shm")
        || name.ends_with(".sqlite")
        || name.ends_with(".sqlite-wal")
        || name.ends_with(".sqlite-shm")
}

struct FlushWalker<'a> {
    profile_dir: &'a Path,
    next_dir: &'a Path,
    previous_dir: Option<&'a Path>,
    open_store_paths: &'a [PathBuf],
    outcome: &'a mut FlushOutcome,
    /// Capture mode (P5). When true this walk is the under-the-gate freeze of
    /// the live profile into a *local* staging tree, so unchanged-vs-previous
    /// is irrelevant: every atomic-rename writer is hard-linked straight from
    /// the source (~6 µs/inode, local) and every in-place writer is byte-copied.
    /// When false this is the ordinary ship walk, which runs outside the gate
    /// and keeps today's link-against-previous-generation behaviour.
    link_from_source: bool,
}

impl FlushWalker<'_> {
    fn copy_dir(&mut self, dir: &Path) -> Result<(), String> {
        let entries = match fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(format!("read dir {}: {error}", display(dir))),
        };
        for entry in entries {
            let entry =
                entry.map_err(|error| format!("read dir entry {}: {error}", display(dir)))?;
            let path = entry.path();
            let file_type = entry
                .file_type()
                .map_err(|error| format!("file type {}: {error}", display(&path)))?;

            if self.should_skip(&path, file_type.is_dir()) {
                continue;
            }
            if file_type.is_dir() {
                self.copy_dir(&path)?;
            } else if file_type.is_file() {
                self.copy_file(&path)?;
            }
            // Symlinks and other special files are not part of the profile model.
        }
        Ok(())
    }

    fn should_skip(&self, path: &Path, is_dir: bool) -> bool {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        // Every registered live Oxigraph root is handled by the backup pass;
        // skip its RocksDB files entirely. Omphalos is rooted at `omphalos/`
        // rather than a `*.oxigraph` directory, so exact-path detection must
        // precede the conventional-name fallback.
        if is_dir && self.open_store_paths.iter().any(|open| open == path) {
            return true;
        }
        // A conventional but currently closed store is immutable in this
        // process while the lifecycle gate is held, so plain-copying is safe.
        if is_dir && name.ends_with(".oxigraph") {
            return false;
        }
        if is_dir {
            // Re-downloadable model cache; profile lock dir is not snapshot state.
            return name == "models";
        }
        // Skip lock files and atomic-write temp turds.
        is_ephemeral_profile_file(&name)
    }

    fn copy_file(&mut self, path: &Path) -> Result<(), String> {
        let rel = relative_to(self.profile_dir, path)
            .ok_or_else(|| format!("path {} escapes profile dir", display(path)))?;
        let target = self.next_dir.join(&rel);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)
                .map_err(|error| format!("create {}: {error}", display(parent)))?;
        }

        let meta =
            fs::metadata(path).map_err(|error| format!("stat {}: {error}", display(path)))?;

        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();

        if self.link_from_source {
            // P5 capture: freeze into local staging. Atomic-rename writers are
            // linked (the link keeps pointing at the frozen original once the
            // writer renames over the name); in-place writers fall through to
            // the byte copy below, which is R2's requirement.
            if !never_hardlink(&name) && fs::hard_link(path, &target).is_ok() {
                self.outcome.files_linked += 1;
                return Ok(());
            }
        } else if let Some(prev_dir) = self.previous_dir {
            // Incremental: hard-link unchanged files from the previous
            // snapshot. `never_hardlink` deliberately does NOT apply here.
            // Its hazard is aliasing an inode a writer still mutates in
            // place; a published snapshot is immutable, so linking one
            // generation's frozen copy into the next is safe and is what
            // makes an unchanged flush cheap. The predicate governs the
            // capture branch above, where the source IS the live file.
            let prev = prev_dir.join(&rel);
            if same_size_mtime(&meta, &prev) && fs::hard_link(&prev, &target).is_ok() {
                self.outcome.files_linked += 1;
                return Ok(());
            }
        }

        let bytes = fs::copy(path, &target)
            .map_err(|error| format!("copy {} -> {}: {error}", display(path), display(&target)))?;

        // `fs::copy` preserves the source mtime on macOS but not on Linux.
        // The next flush identifies unchanged files by size + mtime so it can
        // hard-link them from this snapshot; without normalizing the copied
        // file's mtime, Linux cells recopied every file and published a new
        // snapshot even when the profile tree was unchanged.
        //
        // Timestamp preservation is an optimization aid, not a durability
        // requirement. If the backing filesystem refuses it, keep the copied
        // bytes and let a later flush conservatively copy the file again.
        if let Ok(modified) = meta.modified() {
            match fs::OpenOptions::new().write(true).open(&target) {
                Ok(target_file) => {
                    if let Err(error) =
                        target_file.set_times(fs::FileTimes::new().set_modified(modified))
                    {
                        log::debug!(
                            "durable flush: could not preserve mtime for {}: {error}",
                            display(&target)
                        );
                    }
                }
                Err(error) => log::debug!(
                    "durable flush: could not reopen copied file {} to preserve mtime: {error}",
                    display(&target)
                ),
            }
        }
        self.outcome.files_copied += 1;
        self.outcome.bytes_copied += bytes;
        Ok(())
    }

    fn copy_store_sidecars(&mut self, store_path: &Path) -> Result<(), String> {
        for filename in ["constitution.ttl"] {
            let source = store_path.join(filename);
            if source.is_file() {
                self.copy_file(&source)?;
            }
        }
        Ok(())
    }
}

/// Reuse an immutable subtree from the previous snapshot. Existing targets are
/// sidecars already copied by the walker; leave those newer candidates intact.
fn link_tree_missing(
    source: &Path,
    target: &Path,
    outcome: &mut FlushOutcome,
) -> Result<(), String> {
    let entries = fs::read_dir(source)
        .map_err(|error| format!("read clean store {}: {error}", display(source)))?;
    fs::create_dir_all(target)
        .map_err(|error| format!("create clean store target {}: {error}", display(target)))?;
    for entry in entries {
        let entry = entry.map_err(|error| format!("read clean store entry: {error}"))?;
        let source_path = entry.path();
        let target_path = target.join(entry.file_name());
        let file_type = entry
            .file_type()
            .map_err(|error| format!("file type {}: {error}", display(&source_path)))?;
        if file_type.is_dir() {
            link_tree_missing(&source_path, &target_path, outcome)?;
        } else if file_type.is_file() && !target_path.exists() {
            if fs::hard_link(&source_path, &target_path).is_ok() {
                outcome.files_linked += 1;
            } else {
                let bytes = fs::copy(&source_path, &target_path).map_err(|error| {
                    format!(
                        "copy clean store {} -> {}: {error}",
                        display(&source_path),
                        display(&target_path)
                    )
                })?;
                outcome.files_copied += 1;
                outcome.bytes_copied += bytes;
            }
        }
    }
    Ok(())
}

fn publish_current(durable_dir: &Path, snap_name: &str) -> Result<(), String> {
    let tmp = durable_dir.join(CURRENT_TMP_FILE);
    let current = durable_dir.join(CURRENT_FILE);
    {
        let mut file = fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&tmp)
            .map_err(|error| format!("open {}: {error}", display(&tmp)))?;
        file.write_all(snap_name.as_bytes())
            .and_then(|_| file.sync_all())
            .map_err(|error| format!("write {}: {error}", display(&tmp)))?;
    }
    fs::rename(&tmp, &current)
        .map_err(|error| format!("rename {} -> {}: {error}", display(&tmp), display(&current)))
}

/// Highest sequence among existing `snap-*` dirs (0 if none) so a new flush
/// never claims a name that exists, referenced or not.
fn max_existing_snap_seq(durable_dir: &Path) -> Result<u64, String> {
    let entries = match fs::read_dir(durable_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => {
            return Err(format!(
                "read durable dir {}: {error}",
                display(durable_dir)
            ))
        }
    };
    let mut max = 0u64;
    for entry in entries {
        let entry = entry.map_err(|error| format!("read durable entry: {error}"))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if let Some(seq) = parse_seq(&name) {
            max = max.max(seq);
        }
    }
    Ok(max)
}

/// Every top-level profile entry the walker should have captured must exist in
/// the snapshot. `models/`, lock files, and temp turds are intentionally
/// excluded; empty directories are not snapshot (the walker creates dirs only
/// as file parents).
fn verify_snapshot_complete(profile_dir: &Path, snap_dir: &Path) -> Result<(), String> {
    let entries = fs::read_dir(profile_dir)
        .map_err(|error| format!("read profile dir {}: {error}", display(profile_dir)))?;
    let mut missing = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| format!("read profile entry: {error}"))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        let skipped = if is_dir {
            name == "models" || (dir_is_effectively_empty(&entry.path()))
        } else {
            is_ephemeral_profile_file(&name)
        };
        if skipped {
            continue;
        }
        if !snap_dir.join(&name).exists() {
            missing.push(name);
        }
    }
    if missing.is_empty() {
        Ok(())
    } else {
        missing.sort();
        Err(format!(
            "snapshot is missing top-level profile entries {missing:?} — refusing to publish"
        ))
    }
}

/// True if the directory contains no regular files anywhere (the walker would
/// have produced nothing for it).
fn dir_is_effectively_empty(dir: &Path) -> bool {
    let Ok(entries) = fs::read_dir(dir) else {
        return true;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        match entry.file_type() {
            Ok(t) if t.is_dir() => {
                if name != "models" && !dir_is_effectively_empty(&entry.path()) {
                    return false;
                }
            }
            Ok(t) if t.is_file() && !is_ephemeral_profile_file(&name) => return false,
            _ => {}
        }
    }
    true
}

fn is_ephemeral_profile_file(name: &str) -> bool {
    name == ".lock" || name.ends_with(".tmp") || name.contains(".tmp-")
}

fn read_current(durable_dir: &Path) -> Result<Option<String>, String> {
    match fs::read_to_string(durable_dir.join(CURRENT_FILE)) {
        Ok(raw) => {
            let name = raw.trim().to_string();
            if is_valid_snap_name(&name) {
                Ok(Some(name))
            } else {
                Ok(None)
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("read {}: {error}", CURRENT_FILE)),
    }
}

fn prune_snapshots(
    durable_dir: &Path,
    keep_current: &str,
    keep_previous: Option<&str>,
    keep_authority: Option<&str>,
) -> Result<(), String> {
    let entries = fs::read_dir(durable_dir)
        .map_err(|error| format!("read durable dir {}: {error}", display(durable_dir)))?;
    for entry in entries {
        let entry = entry.map_err(|error| format!("read durable entry: {error}"))?;
        if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        // Stale .building-* dirs (crashed mid-flush) are safe to remove here:
        // flushes are serialized process-wide and the pod name is the
        // cross-pod single-writer lease, so nothing else is writing them.
        let stale_build = name.starts_with(BUILDING_PREFIX);
        // Never remove a snapshot newer than the one just published — if a
        // briefly-overlapping process (kubelet container resurrection) claimed
        // a higher sequence, pruning it could dangle that process's CURRENT.
        let newer_than_current = parse_seq(&name) > parse_seq(keep_current);
        if !stale_build
            && (!is_valid_snap_name(&name)
                || name == keep_current
                || Some(name.as_str()) == keep_previous
                || Some(name.as_str()) == keep_authority
                || newer_than_current)
        {
            continue;
        }
        if let Err(error) = fs::remove_dir_all(entry.path()) {
            log::warn!(
                "durable prune: failed to remove old snapshot {}: {error}",
                display(&entry.path())
            );
        }
    }
    Ok(())
}

fn profile_has_content(profile_dir: &Path) -> bool {
    let Ok(entries) = fs::read_dir(profile_dir) else {
        return false;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        // A bare .lock from a previous boot does not count as real content.
        if name == ".lock" {
            continue;
        }
        return true;
    }
    false
}

/// Bounded fan-out for the hydration snapshot copy (durable EFS → local disk).
///
/// The serial per-file walk was measured at 198 s for a 1.17 GB store: EFS
/// per-file open/close round-trips dominate (effective ~5.9 MB/s against a
/// local-disk floor orders of magnitude higher), so this is latency hiding,
/// not bandwidth chasing — with N files in flight, wall clock approaches
/// (file_count / N) × per-file latency. 16 captures most of that win for the
/// store profile (a few hundred to low thousands of RocksDB SST files,
/// mostly 1–100 MB) while staying a deliberately fixed constant: not config,
/// no knob to mis-tune per environment.
const HYDRATE_COPY_PARALLELISM: usize = 16;

/// Copy `source`'s tree into `target`: same file set, same bytes, same
/// relative layout as the serial walk this replaces. Two phases so every
/// ordering that matters is trivially preserved: (1) a serial walk
/// enumerates the files and creates the full destination directory skeleton
/// — every directory exists before any child copy starts; (2) the collected
/// file copies run with bounded concurrency. `thread::scope` joins every
/// worker before this function returns, so everything sequenced after
/// `copy_tree` (hydration bookkeeping, store open) still happens strictly
/// after the last byte landed.
///
/// Fail-fast: the first error in enumeration order among those observed
/// aborts the copy and is returned; workers stop claiming new files once any
/// error is recorded. An `Err` is never accompanied by a success claim. (As
/// with the serial walk, an errored copy may leave a partial tree under
/// `target`; hydrate already treats any `Err` as fatal and never publishes a
/// clean-hydration proof for it.)
fn copy_tree(source: &Path, target: &Path) -> Result<u64, String> {
    fs::create_dir_all(target)
        .map_err(|error| format!("create {}: {error}", display(target)))?;
    let mut files: Vec<(PathBuf, PathBuf)> = Vec::new();
    collect_copy_tree_files(source, target, &mut files)?;
    copy_files_bounded_parallel(&files)
}

/// Phase 1 of [`copy_tree`]: recurse `source`, mirror the directory skeleton
/// under `target` (empty directories included), and collect every
/// (source file, destination file) pair. Entry handling is identical to the
/// old serial walk: directories recurse, regular files copy, anything else
/// (symlinks, special files) is skipped.
fn collect_copy_tree_files(
    source: &Path,
    target: &Path,
    files: &mut Vec<(PathBuf, PathBuf)>,
) -> Result<(), String> {
    let entries =
        fs::read_dir(source).map_err(|error| format!("read dir {}: {error}", display(source)))?;
    for entry in entries {
        let entry = entry.map_err(|error| format!("read dir entry: {error}"))?;
        let path = entry.path();
        let file_type = entry
            .file_type()
            .map_err(|error| format!("file type {}: {error}", display(&path)))?;
        let dest = target.join(entry.file_name());
        if file_type.is_dir() {
            fs::create_dir_all(&dest)
                .map_err(|error| format!("create {}: {error}", display(&dest)))?;
            collect_copy_tree_files(&path, &dest, files)?;
        } else if file_type.is_file() {
            files.push((path, dest));
        }
    }
    Ok(())
}

/// Phase 2 of [`copy_tree`]: run the collected copies on a scoped worker
/// pool [`HYDRATE_COPY_PARALLELISM`] wide, fail-fast, returning total bytes.
///
/// Scoped threads rather than the async runtime, deliberately: hydrate is a
/// synchronous once-per-boot path that must not assume a Tokio runtime
/// exists yet, and `thread::scope` both borrows the job list directly and
/// guarantees every worker is joined before returning — the completion
/// barrier the caller's sequencing relies on.
fn copy_files_bounded_parallel(files: &[(PathBuf, PathBuf)]) -> Result<u64, String> {
    let next = AtomicUsize::new(0);
    let total = AtomicU64::new(0);
    let stop = AtomicBool::new(false);
    // First error in enumeration order: parallel workers can observe failures
    // out of order, so keep the lowest-index one for a deterministic report.
    let first_error: Mutex<Option<(usize, String)>> = Mutex::new(None);
    let workers = HYDRATE_COPY_PARALLELISM.min(files.len());
    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| loop {
                if stop.load(Ordering::Acquire) {
                    break;
                }
                let index = next.fetch_add(1, Ordering::Relaxed);
                let Some((path, dest)) = files.get(index) else {
                    break;
                };
                match fs::copy(path, dest) {
                    Ok(bytes) => {
                        total.fetch_add(bytes, Ordering::Relaxed);
                    }
                    Err(error) => {
                        stop.store(true, Ordering::Release);
                        let message =
                            format!("copy {} -> {}: {error}", display(path), display(dest));
                        let mut slot = first_error.lock().unwrap_or_else(|p| p.into_inner());
                        if slot.as_ref().is_none_or(|(held, _)| index < *held) {
                            *slot = Some((index, message));
                        }
                        break;
                    }
                }
            });
        }
    });
    let first_error = first_error
        .into_inner()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some((_, message)) = first_error {
        return Err(message);
    }
    Ok(total.into_inner())
}

fn trees_equal_ignoring_clean_stores(a: &Path, b: &Path, clean_stores: &[PathBuf]) -> bool {
    let mut left = Vec::new();
    let mut right = Vec::new();
    if collect_files(a, a, &mut left).is_err() || collect_files(b, b, &mut right).is_err() {
        return false;
    }
    left.retain(|(path, _)| !is_clean_store_database_file(path, clean_stores));
    right.retain(|(path, _)| !is_clean_store_database_file(path, clean_stores));
    left.sort();
    right.sort();
    left == right
}

fn is_clean_store_database_file(path: &Path, clean_stores: &[PathBuf]) -> bool {
    clean_stores.iter().any(|store| {
        path.strip_prefix(store)
            .is_ok_and(|suffix| suffix != Path::new("constitution.ttl"))
    })
}

fn collect_files(root: &Path, dir: &Path, out: &mut Vec<(PathBuf, u64)>) -> Result<(), String> {
    let entries =
        fs::read_dir(dir).map_err(|error| format!("read dir {}: {error}", display(dir)))?;
    for entry in entries {
        let entry = entry.map_err(|error| format!("read dir entry: {error}"))?;
        let path = entry.path();
        let file_type = entry
            .file_type()
            .map_err(|error| format!("file type: {error}"))?;
        if file_type.is_dir() {
            collect_files(root, &path, out)?;
        } else if file_type.is_file() {
            let rel = relative_to(root, &path).unwrap_or_else(|| path.clone());
            let size = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            out.push((rel, size));
        }
    }
    Ok(())
}

/// rsync-style quick-check deciding hard-link vs copy: equal length + exactly
/// equal mtime means "unchanged".
///
/// Known blind spot, deliberate: file mtimes are stamped from the kernel's
/// coarse clock (one kernel tick of granularity), so a same-size rewrite that
/// lands within the same tick as the version captured by the previous
/// snapshot is indistinguishable here and gets hard-linked instead of copied.
/// At production cadence the window is unreachable — consecutive flushes are
/// separated by multi-second walks and 30s scheduler ticks, so the previous
/// snapshot's mtime is always ticks in the past. Release-profile TESTS are
/// fast enough to fit seed → flush → rewrite inside one tick, however: test
/// mutations between flushes must change file SIZE, never only bytes (see
/// `second_flush_hard_links_unchanged_files` / `prune_keeps_current_and_previous`).
fn same_size_mtime(meta: &fs::Metadata, previous: &Path) -> bool {
    let Ok(prev_meta) = fs::metadata(previous) else {
        return false;
    };
    if meta.len() != prev_meta.len() {
        return false;
    }
    match (meta.modified(), prev_meta.modified()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

fn relative_to(base: &Path, path: &Path) -> Option<PathBuf> {
    path.strip_prefix(base).ok().map(Path::to_path_buf)
}

fn parse_seq(name: &str) -> Option<u64> {
    name.strip_prefix(SNAP_PREFIX)?.parse().ok()
}

fn is_valid_snap_name(name: &str) -> bool {
    parse_seq(name).is_some()
}

fn display(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

// ---------------------------------------------------------------------------
// Boot-time repair (U8 spec §3.6 — the zombie rule, bridge form)
// ---------------------------------------------------------------------------
//
// Replaced at U1 by the frame-recovery rule (scan to the last `EpochOpen`,
// truncate lower-epoch frames). `// U1: delete with the snapshot flush path`.

/// One row of the spec §3.6 table, decided from `S` (read fresh from
/// `durable_dir`'s `CURRENT` inside [`boot_repair`]) against the claim-time
/// bridge values the gateway stamped into env at spawn — `L`/`P`
/// (`GARDEN_LEASE_LAST_SNAP`/`GARDEN_LEASE_PENDING_SNAP`), parsed and passed
/// in by the caller, never read from the environment here, so this stays
/// testable against a bare tempdir with plain integer literals.
///
/// This enum reports the *decision*; [`boot_repair`] performs the matching
/// filesystem action unless `dry_run` is `true` — observe mode calls it with
/// `dry_run = true` to compute and log the same decision without ever
/// touching disk (spec §3.2: "observe mode testifies what it would have
/// done"). Enforce-mode boot orchestration (`examples/gardend.rs`, U8-10)
/// calls this strictly after "wait out the effective deadline" and strictly
/// before `hydrate`/`hydrate_detailed` — a hydrate run against an unrepaired
/// `CURRENT` could restore a quarantine-bound zombie snapshot into the live
/// profile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BootRepairOutcome {
    /// `L` absent from env: a pre-lease gateway spawned this cell. Repair is
    /// suppressed entirely — never guessed (spec §3.6 row 1; the ops judge's
    /// first-rollout misfire fix, proven in the failing direction by Z5).
    SkippedNoLastSnap,
    /// `S == L`, `P` absent: nothing to repair.
    Clean,
    /// `P` present, `S == P`: a crash landed between the rename and the
    /// commit CAS. The tree is already correct; the caller must send
    /// `publish(commit, P)` once the lease is live, to reconcile the
    /// authority's record (worst case is the existing between-flush RPO
    /// window, never divergent history — spec §3.6).
    AcceptPending { seq: u64 },
    /// `P` present, `S < P`: a crash landed before the rename. The matching
    /// `.building-{seq:06}-e*` orphan has been pruned (unless `dry_run`); the
    /// caller must send `publish(abort, P)`.
    AbortedPending { seq: u64 },
    /// `S > L` and (`P` absent or `S > P`): an unrecorded publish escaped —
    /// `snap-{L+1..=S}` were moved into `.orphan-{ulid}/` and `CURRENT`
    /// rewritten to `snap-L` (unless `dry_run`). Truncate, never apply.
    Quarantined {
        restored_to: u64,
        orphaned: Vec<u64>,
        orphan_dir_name: String,
    },
    /// `S < L`: a zombie's late `publish_current` rewound `CURRENT` below
    /// the authority's high-water mark; restored to `snap-L` (unless
    /// `dry_run`).
    Restored { restored_to: u64 },
}

/// A boot-repair failure classified by whether replaying a fresh cell can
/// possibly change the outcome. Ordinary filesystem failures remain
/// retryable; a missing authority high-water snapshot requires an explicit
/// operator choice and is surfaced by Gardend with its dedicated exit code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootRepairError {
    message: String,
    requires_snapshot_authority_repair: bool,
}

impl BootRepairError {
    fn retryable(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            requires_snapshot_authority_repair: false,
        }
    }

    fn authority_snapshot_missing(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            requires_snapshot_authority_repair: true,
        }
    }

    pub fn requires_snapshot_authority_repair(&self) -> bool {
        self.requires_snapshot_authority_repair
    }
}

impl std::fmt::Display for BootRepairError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for BootRepairError {}

impl From<String> for BootRepairError {
    fn from(message: String) -> Self {
        Self::retryable(message)
    }
}

/// Boot-time repair per spec §3.6. See [`BootRepairOutcome`] for the decision
/// table and the `dry_run` (observe-mode) contract.
///
/// Pure decision, conditionally impure application: which branch is taken
/// depends only on `S` (read fresh from `durable_dir/CURRENT`), `last_snap`,
/// and `pending_snap` — never wall-clock time or process-global state — but
/// the non-`Clean`/`AcceptPending`/`SkippedNoLastSnap` branches perform real
/// filesystem mutations when `dry_run` is `false`. Call at most once per
/// boot.
pub fn boot_repair(
    durable_dir: &Path,
    last_snap: Option<u64>,
    pending_snap: Option<u64>,
    dry_run: bool,
) -> Result<BootRepairOutcome, BootRepairError> {
    let Some(l) = last_snap else {
        log::info!(
            "boot repair: LAST_SNAP absent from env — a pre-lease gateway spawned this cell; \
             suppressing repair entirely (spec §3.6 row 1)"
        );
        return Ok(BootRepairOutcome::SkippedNoLastSnap);
    };

    let s = read_current(durable_dir)?
        .and_then(|name| parse_seq(&name))
        .unwrap_or(0);

    let dry_suffix = if dry_run {
        " (dry run — observe mode, no filesystem change)"
    } else {
        ""
    };

    if let Some(p) = pending_snap {
        if s == p {
            log::info!(
                "boot repair: S={s} == PENDING_SNAP={p} — crash between rename and commit; \
                 accepting, will publish(commit, {p}) after boot"
            );
            return Ok(BootRepairOutcome::AcceptPending { seq: p });
        }
        if s < p {
            log::warn!(
                "boot repair: S={s} < PENDING_SNAP={p} — crash before the rename landed; \
                 aborting the pending intent{dry_suffix}"
            );
            if !dry_run {
                prune_building_dir_for_seq(durable_dir, p)?;
            }
            return Ok(BootRepairOutcome::AbortedPending { seq: p });
        }
        // s > p falls through to the quarantine check below. `p` is only
        // ever set after `l`'s own commit (a pending intent always follows
        // the previously-committed last_snap), so p > l always — meaning
        // s > p here implies s > l too.
    }

    if s == l {
        return Ok(BootRepairOutcome::Clean);
    }

    if s > l {
        let snap_l_name = format!("{SNAP_PREFIX}{l:06}");
        if !durable_dir.join(&snap_l_name).is_dir() {
            return Err(BootRepairError::authority_snapshot_missing(format!(
                "boot repair: an unrecorded publish escaped (S={s} > LAST_SNAP={l}) but the \
                 authority's own high-water mark {snap_l_name} does not exist on disk — refusing \
                 to boot (spec §3.6: \"truncate, never apply\"; see the forfeit runbook)"
            )));
        }
        let orphaned: Vec<u64> = ((l + 1)..=s).collect();
        // `.orphan-*` never starts with SNAP_PREFIX or BUILDING_PREFIX, so
        // `prune_snapshots` (which only ever touches `snap-*`/`.building-*`,
        // see its own doc comment) naturally preserves this quarantine dir
        // for the operator across every future flush cycle.
        let orphan_dir_name = format!(".orphan-{}", new_orphan_ulid());
        log::warn!(
            "boot repair: an unrecorded publish escaped (S={s} > LAST_SNAP={l}) — quarantining \
             {orphaned:?} into {orphan_dir_name} and restoring CURRENT to {snap_l_name}{dry_suffix}"
        );
        if !dry_run {
            let orphan_dir = durable_dir.join(&orphan_dir_name);
            fs::create_dir_all(&orphan_dir)
                .map_err(|error| format!("create {}: {error}", display(&orphan_dir)))?;
            for seq in &orphaned {
                let name = format!("{SNAP_PREFIX}{seq:06}");
                let source = durable_dir.join(&name);
                if source.is_dir() {
                    fs::rename(&source, orphan_dir.join(&name)).map_err(|error| {
                        format!(
                            "quarantine {} -> {}: {error}",
                            display(&source),
                            display(&orphan_dir.join(&name))
                        )
                    })?;
                }
            }
            publish_current(durable_dir, &snap_l_name)?;
        }
        return Ok(BootRepairOutcome::Quarantined {
            restored_to: l,
            orphaned,
            orphan_dir_name,
        });
    }

    // s < l here (and, per the fallthrough above, `pending_snap` was `None` —
    // otherwise `s < p <= ... ` would already have returned `AbortedPending`,
    // since p > l > s would make s < p true).
    let snap_l_name = format!("{SNAP_PREFIX}{l:06}");
    if !durable_dir.join(&snap_l_name).is_dir() {
        return Err(BootRepairError::authority_snapshot_missing(format!(
            "boot repair: CURRENT was rewound below the authority's high-water mark (S={s} < \
             LAST_SNAP={l}) but {snap_l_name} does not exist on disk — refusing to boot"
        )));
    }
    log::warn!(
        "boot repair: CURRENT was rewound below LAST_SNAP (S={s} < L={l}) — restoring to \
         {snap_l_name}{dry_suffix}"
    );
    if !dry_run {
        publish_current(durable_dir, &snap_l_name)?;
    }
    Ok(BootRepairOutcome::Restored { restored_to: l })
}

/// A ULID for the `.orphan-{ulid}/` quarantine directory name (spec §3.6).
/// The `ulid` crate is an optional dependency pulled in only by the
/// `headless` feature (`Cargo.toml`); the desktop feature set never touches
/// this function in practice (durable mode never runs on desktop), but the
/// non-headless arm must still exist so the crate type-checks under the
/// default (desktop) feature set.
#[cfg(feature = "headless")]
fn new_orphan_ulid() -> String {
    ulid::Ulid::new().to_string()
}

#[cfg(not(feature = "headless"))]
fn new_orphan_ulid() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

/// Remove the `.building-{seq:06}-e*` orphan left behind by a flush that
/// crashed before claiming its rename (spec §3.6's `AbortedPending` row).
/// Flushes are serialized process-wide (`flush_serial`) and the durable dir
/// is single-writer per epoch (the lease), so at most one such dir can exist
/// for a given `seq`.
fn prune_building_dir_for_seq(durable_dir: &Path, seq: u64) -> Result<(), String> {
    let prefix = format!("{BUILDING_PREFIX}{seq:06}-");
    let entries = match fs::read_dir(durable_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(format!(
                "read durable dir {}: {error}",
                display(durable_dir)
            ))
        }
    };
    for entry in entries {
        let entry = entry.map_err(|error| format!("read durable entry: {error}"))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with(&prefix) {
            fs::remove_dir_all(entry.path()).map_err(|error| {
                format!("prune orphan build dir {}: {error}", display(&entry.path()))
            })?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxigraph::model::{GraphNameRef, NamedNodeRef, QuadRef};
    use oxigraph::store::Store;
    use std::time::{SystemTime, UNIX_EPOCH};

    /// Flush tests share process-global state (the flush gate + FLUSH_SERIAL);
    /// serialize them so a test holding the gate cannot make sibling flushes
    /// skip-under-contention and fail their assertions.
    ///
    /// Battery gotcha: this lock serializes ONLY this file's tests. The
    /// process-global `STORE_WRITE_EPOCH` is also advanced — unconditionally,
    /// before any per-graph filtering — by fixtures in other files that share
    /// the same `cargo test --lib` binary: every `WriteGateGuard::drop`
    /// (emporium/write_gate.rs), every non-read-only `GraphPersistenceLease`
    /// drop (crdt_engine/persistence_coordinator.rs,
    /// `DirtyLeaseState::mark_if_needed`), and every `mark_rdf_store_written`
    /// caller (rdf_query_service, source_sync, rdf_record_materializer,
    /// rdf_seed_service, geist_memory_rdf, emporium/{violation_ledger,
    /// reconcile}). A filtered `cargo test cell_durability::` never schedules
    /// them; the full battery at 32 threads does. Any assertion here that
    /// depends on the epoch NOT moving across a window must witness the epoch
    /// itself (see `hydrated_clean_first_flush_attempt`) rather than trust
    /// this lock. Diagnosis: `garden-battery-r1-flake-diagnosis-20260825`.
    fn test_serial() -> &'static Mutex<()> {
        static T: OnceLock<Mutex<()>> = OnceLock::new();
        T.get_or_init(|| Mutex::new(()))
    }

    fn temp_dir(name: &str) -> PathBuf {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        std::env::temp_dir().join(format!("garden-cell-durability-{name}-{suffix}"))
    }

    fn write(path: &Path, bytes: &[u8]) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        let mut file = fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(path)
            .unwrap();
        file.write_all(bytes).unwrap();
    }

    #[test]
    fn store_trace_slot_survives_identity_replacement() {
        let _serial = test_serial().lock().unwrap_or_else(|p| p.into_inner());
        let profile = temp_dir("store-trace-slot");
        let previous_profile = temp_dir("store-trace-slot-previous");
        let store_path = profile.join("graphs/graph-a/store.oxigraph");
        let previous_store = previous_profile.join("graphs/graph-a/store.oxigraph");
        fs::create_dir_all(&previous_store).expect("create prior store fixture");

        let first = Arc::new(Store::new().expect("first in-memory store"));
        let first_plan =
            store_epoch_to_backup(&profile, &store_path, &first, Some(&previous_store));
        assert!(first_plan.needs_backup);
        let slot_id = first_plan.slot_id;
        assert_eq!(first_plan.incarnation, 1);
        acknowledge_store_backup(&store_path, &first, &first_plan)
            .expect("acknowledge first store incarnation");

        let clean_plan =
            store_epoch_to_backup(&profile, &store_path, &first, Some(&previous_store));
        assert!(!clean_plan.needs_backup);
        assert_eq!(clean_plan.slot_id, slot_id);
        assert_eq!(clean_plan.incarnation, 1);

        let replacement = Arc::new(Store::new().expect("replacement in-memory store"));
        let replacement_plan =
            store_epoch_to_backup(&profile, &store_path, &replacement, Some(&previous_store));
        assert!(replacement_plan.needs_backup);
        assert_eq!(replacement_plan.slot_id, slot_id);
        assert_eq!(replacement_plan.incarnation, 2);

        if let Ok(mut bookkeeping) = store_backup_epochs().lock() {
            bookkeeping.stores.remove(&store_path);
        }
        let _ = fs::remove_dir_all(profile);
        let _ = fs::remove_dir_all(previous_profile);
    }

    /// Build a profile-ish tree (no live oxigraph stores) under `dir`.
    fn seed_profile(dir: &Path) {
        write(&dir.join("metadata.turso"), b"turso-db");
        write(&dir.join("metadata.turso-wal"), b"wal");
        write(
            &dir.join("graphs/g1/documents/doc-1/document.json"),
            b"{\"doc\":1}",
        );
        write(
            &dir.join("graphs/g1/ydocs/documents/doc-1/update-v1.bin"),
            b"\x00\x01\x02",
        );
        write(
            &dir.join("graphs/g1/indexes/semantic/blocks.json"),
            b"{\"blocks\":[]}",
        );
        write(&dir.join("worklog/operation-completion.jsonl"), b"{}\n");
        write(&dir.join("jobs/jobs.turso"), b"jobs-db");
        // Things that must be skipped:
        write(&dir.join(".lock"), b"");
        write(&dir.join("models/semantic/model.onnx"), b"huge-blob");
        write(&dir.join(".document.json.tmp-123-456"), b"partial");
    }

    #[test]
    fn roundtrip_flush_then_hydrate_is_identical() {
        let _serial = test_serial().lock().unwrap_or_else(|p| p.into_inner());
        let root = temp_dir("roundtrip");
        let profile = root.join("profile");
        let durable = root.join("durable");
        seed_profile(&profile);

        let outcome = flush(&profile, &durable).expect("flush");
        assert!(outcome.published);
        assert!(outcome.files_copied >= 6, "copied {}", outcome.files_copied);

        // Wipe the profile and hydrate from durable.
        fs::remove_dir_all(&profile).unwrap();
        let hydrated = hydrate_detailed(&profile, &durable).expect("hydrate");
        assert_eq!(hydrated.mode, HydrateMode::Restored);
        assert_eq!(hydrated.snapshot_id.as_deref(), Some("snap-000001"));
        assert!(hydrated.hydrated_bytes.is_some_and(|bytes| bytes > 0));

        // Persisted data round-trips...
        assert_eq!(
            fs::read(profile.join("graphs/g1/documents/doc-1/document.json")).unwrap(),
            b"{\"doc\":1}"
        );
        assert_eq!(
            fs::read(profile.join("jobs/jobs.turso")).unwrap(),
            b"jobs-db"
        );
        // ...and skipped paths did NOT round-trip.
        assert!(!profile.join("models/semantic/model.onnx").exists());
        assert!(!profile.join(".document.json.tmp-123-456").exists());

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn create_once_admission_roundtrips_through_actual_flush_and_hydration() {
        let _serial = test_serial().lock().unwrap_or_else(|p| p.into_inner());
        let root = temp_dir("create-once-admission-roundtrip");
        let profile = root.join("profile");
        let graph_dir = profile.join("graphs/starter");
        let durable = root.join("durable");
        let restored = root.join("restored-profile");
        fs::create_dir_all(&graph_dir).unwrap();
        crate::crdt_engine::create_once::retain_admission_for_test(&graph_dir, "page").unwrap();
        let path = crate::crdt_engine::create_once::admission_path(&graph_dir, "page").unwrap();
        let expected = fs::read(&path).unwrap();
        let outcome = flush(&profile, &durable).expect("actual file walker flush");
        assert!(outcome.published);
        let hydration = hydrate_detailed(&restored, &durable).expect("actual snapshot hydration");
        assert_eq!(hydration.mode, HydrateMode::Restored);
        let restored_graph = restored.join("graphs/starter");
        let restored_path = crate::crdt_engine::create_once::admission_path(&restored_graph, "page").unwrap();
        assert_eq!(fs::read(restored_path).unwrap(), expected);
        assert!(crate::crdt_engine::create_once::retain_admission_for_test(&restored_graph, "page").is_err());
        let _ = fs::remove_dir_all(root);
    }

    /// Walk `root` collecting sorted (relative dir paths, relative file path
    /// → full contents) — the comparison basis for copy_tree identity.
    fn collect_tree_for_compare(root: &Path) -> (Vec<PathBuf>, Vec<(PathBuf, Vec<u8>)>) {
        fn recurse(
            root: &Path,
            dir: &Path,
            dirs: &mut Vec<PathBuf>,
            files: &mut Vec<(PathBuf, Vec<u8>)>,
        ) {
            for entry in fs::read_dir(dir).unwrap() {
                let entry = entry.unwrap();
                let path = entry.path();
                let rel = path.strip_prefix(root).unwrap().to_path_buf();
                if path.is_dir() {
                    dirs.push(rel);
                    recurse(root, &path, dirs, files);
                } else {
                    files.push((rel, fs::read(&path).unwrap()));
                }
            }
        }
        let mut dirs = Vec::new();
        let mut files = Vec::new();
        recurse(root, root, &mut dirs, &mut files);
        dirs.sort();
        files.sort();
        (dirs, files)
    }

    /// The bounded-parallel hydration copy must be indistinguishable from the
    /// serial walk it replaced: same directories (empty ones included), same
    /// file set, same bytes, and a returned byte count equal to the sum of
    /// the file sizes.
    #[test]
    fn copy_tree_many_file_tree_is_identical() {
        let root = temp_dir("copy-tree-identical");
        let source = root.join("source");
        let target = root.join("target");
        // 200 files spread across nested dirs, contents unique per path so a
        // routing mistake (wrong destination) cannot cancel out.
        for index in 0..200u32 {
            let path = source
                .join(format!("d{}", index % 7))
                .join(format!("e{}", index % 3))
                .join(format!("file-{index:03}.bin"));
            let mut bytes = format!("payload-{index}-").into_bytes();
            bytes.extend(std::iter::repeat(index as u8).take((index as usize % 96) + 1));
            write(&path, &bytes);
        }
        // Empty directories are layout too and must survive the copy.
        fs::create_dir_all(source.join("d-empty/nested-empty")).unwrap();

        let bytes = copy_tree(&source, &target).expect("copy_tree");

        let (source_dirs, source_files) = collect_tree_for_compare(&source);
        let (target_dirs, target_files) = collect_tree_for_compare(&target);
        assert_eq!(source_files.len(), 200);
        assert_eq!(source_dirs, target_dirs);
        assert_eq!(source_files, target_files);
        let expected: u64 = source_files
            .iter()
            .map(|(_, bytes)| bytes.len() as u64)
            .sum();
        assert_eq!(bytes, expected);

        let _ = fs::remove_dir_all(&root);
    }

    /// Fail-fast: an unreadable file mid-set must surface as an `Err` naming
    /// the failing file — never a partial tree reported as success.
    ///
    /// Precondition: mode bits must bind the test runner. A runner holding
    /// CAP_DAC_OVERRIDE (root) reads a mode-000 file fine, so the test probes
    /// that precondition first and reports a visible skip when it does not
    /// hold, rather than minting a pass it did not earn.
    #[cfg(unix)]
    #[test]
    fn copy_tree_unreadable_file_fails_not_partial_success() {
        use std::os::unix::fs::PermissionsExt;

        let root = temp_dir("copy-tree-failfast");
        let source = root.join("source");
        let target = root.join("target");
        for index in 0..60u32 {
            write(
                &source
                    .join(format!("d{}", index % 5))
                    .join(format!("file-{index:02}")),
                format!("bytes-{index}").as_bytes(),
            );
        }
        // Poison one file in the middle of the set.
        let poisoned = source.join("d2").join("file-22");
        fs::set_permissions(&poisoned, fs::Permissions::from_mode(0o000)).unwrap();

        // Battery gotcha: the cloud garden-battery lane (sophia-labs/cloud
        // garden-battery/Dockerfile.garden-battery + native-builder-check.sh)
        // ran its obligations container as root through r4 — the Dockerfile
        // set no USER and the check hook passed no --user — so root's
        // DAC bypass kept the poisoned file readable and this test panicked
        // ("unreadable source file must abort the copy: 470"; r4 on
        // integrate/parallel-hydrate-on-main-20260825, 1452 passed / 1
        // failed). The lane now runs unprivileged; this guard keeps the
        // test honest on any other root runner by probing the precondition
        // directly instead of guessing at uids or capabilities.
        if fs::read(&poisoned).is_ok() {
            // Written straight to fd 2 on purpose: libtest captures
            // `eprintln!` from passing tests, which would make this skip
            // invisible in a battery log.
            use std::io::Write;
            let _ = writeln!(
                std::io::stderr(),
                "SKIP copy_tree_unreadable_file_fails_not_partial_success: \
                 mode 0o000 does not deny reads to this runner (root / \
                 CAP_DAC_OVERRIDE); the fail-fast assertion is untestable here"
            );
            fs::set_permissions(&poisoned, fs::Permissions::from_mode(0o644)).unwrap();
            let _ = fs::remove_dir_all(&root);
            return;
        }

        let result = copy_tree(&source, &target);
        let error = result.expect_err("unreadable source file must abort the copy");
        assert!(
            error.contains("file-22"),
            "error should name the failing file: {error}"
        );
        // The failing file itself never lands: fs::copy opens the source
        // before creating the destination.
        assert!(!target.join("d2/file-22").exists());

        fs::set_permissions(&poisoned, fs::Permissions::from_mode(0o644)).unwrap();
        let _ = fs::remove_dir_all(&root);
    }

    /// The awaitDurable receipt watermark resolves an epoch ONLY for a flush
    /// attempt that actually completed — `Published` (survived Gate C) or
    /// `ConfirmedClean` (full dirty computation confirmed prior coverage).
    /// `Deferred`, `Fenced`, and errored attempts resolve nothing: advancing
    /// the watermark there would recreate the 2026-08-21 false-ack shape at
    /// the durable-plane level (a durability claim with no publication
    /// behind it).
    #[test]
    fn durable_watermark_resolves_only_completed_attempts() {
        let published = Ok((
            FlushOutcome {
                published: true,
                sequence: 3,
                ..Default::default()
            },
            FlushAttemptOutcome::Published,
        ));
        assert_eq!(durably_resolved_epoch_after_attempt(9, &published, false), Some(9));
        // S2: a Published attempt with observe-mode lease doubt pays nothing.
        assert_eq!(durably_resolved_epoch_after_attempt(9, &published, true), None);

        let clean = Ok((FlushOutcome::default(), FlushAttemptOutcome::ConfirmedClean));
        assert_eq!(durably_resolved_epoch_after_attempt(9, &clean, false), Some(9));

        let deferred = Ok((FlushOutcome::default(), FlushAttemptOutcome::Deferred));
        assert_eq!(durably_resolved_epoch_after_attempt(9, &deferred, false), None);

        let fenced = Ok((FlushOutcome::default(), FlushAttemptOutcome::Fenced));
        assert_eq!(durably_resolved_epoch_after_attempt(9, &fenced, false), None);

        let errored: Result<(FlushOutcome, FlushAttemptOutcome), String> = Err("boom".to_string());
        assert_eq!(durably_resolved_epoch_after_attempt(9, &errored, false), None);
    }

    /// End-to-end positive: a real published flush advances the awaitDurable
    /// receipt watermark at or past a write epoch marked before it, so the
    /// MCP write path's `durabilityChecked` claim becomes payable exactly
    /// then. (The negative arms — Deferred/Fenced/error advance nothing —
    /// are pinned on the pure `durably_resolved_epoch_after_attempt` seam
    /// above rather than re-asserted here, because the watermark and the
    /// write epoch are process-globals shared with sibling tests whose own
    /// flushes may legitimately advance them mid-assertion.)
    #[test]
    fn published_flush_advances_durable_receipt_watermark() {
        let _serial = test_serial().lock().unwrap_or_else(|p| p.into_inner());
        let root = temp_dir("receipt-watermark");
        let profile = root.join("profile");
        let durable = root.join("durable");
        seed_profile(&profile);

        // A completed Garden write's epoch mark (what the durability write
        // guard's drop publishes after a persistence transaction finishes).
        let write_epoch = next_store_write_epoch();

        let outcome = flush(&profile, &durable).expect("flush");
        assert!(outcome.published);
        assert!(
            durably_resolved_epoch() >= write_epoch,
            "a published flush must resolve the receipt watermark at or past \
             a write epoch marked before it (resolved {}, write {write_epoch})",
            durably_resolved_epoch(),
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn fresh_profile_with_only_worklog_lockfiles_can_publish() {
        let _serial = test_serial().lock().unwrap_or_else(|p| p.into_inner());
        let root = temp_dir("fresh-lock-only-worklog");
        let profile = root.join("profile");
        let durable = root.join("durable");
        write(&profile.join("profile.json"), b"{}");
        fs::create_dir_all(profile.join("graphs")).unwrap();
        write(
            &profile.join("worklog/.jsonl-locks/crdt-operations.jsonl/.lock"),
            b"",
        );
        write(
            &profile.join("worklog/.jsonl-locks/operation-completion.jsonl/.lock"),
            b"",
        );

        let outcome = flush(&profile, &durable).expect("fresh profile flush");
        assert!(outcome.published);
        let current = fs::read_to_string(durable.join(CURRENT_FILE)).unwrap();
        assert!(durable.join(&current).join("profile.json").is_file());
        assert!(
            !durable.join(&current).join("worklog").exists(),
            "lock-only worklog is intentionally not durable state"
        );
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn hydrate_skips_when_profile_non_empty() {
        let _serial = test_serial().lock().unwrap_or_else(|p| p.into_inner());
        let root = temp_dir("skip-nonempty");
        let profile = root.join("profile");
        let durable = root.join("durable");
        seed_profile(&profile);
        flush(&profile, &durable).expect("flush");

        // Mutate local state, then hydrate must NOT clobber it.
        write(
            &profile.join("graphs/g1/documents/doc-1/document.json"),
            b"LOCAL",
        );
        let hydrated = hydrate(&profile, &durable).expect("hydrate");
        assert!(!hydrated);
        assert_eq!(
            fs::read(profile.join("graphs/g1/documents/doc-1/document.json")).unwrap(),
            b"LOCAL"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn hydrate_no_current_is_fresh_profile() {
        let _serial = test_serial().lock().unwrap_or_else(|p| p.into_inner());
        let root = temp_dir("fresh");
        let profile = root.join("profile");
        let durable = root.join("durable");
        fs::create_dir_all(&durable).unwrap();
        let hydrated = hydrate(&profile, &durable).expect("hydrate");
        assert!(!hydrated);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn partial_snapshot_without_current_is_ignored() {
        let _serial = test_serial().lock().unwrap_or_else(|p| p.into_inner());
        let root = temp_dir("partial");
        let profile = root.join("profile");
        let durable = root.join("durable");
        seed_profile(&profile);
        flush(&profile, &durable).expect("flush"); // publishes snap-000001

        // Simulate a crash mid-flush: a complete snap dir exists but CURRENT
        // still points at the old one.
        let ghost = durable.join("snap-000999");
        write(
            &ghost.join("graphs/g1/documents/doc-1/document.json"),
            b"GHOST",
        );
        assert_eq!(
            fs::read_to_string(durable.join(CURRENT_FILE))
                .unwrap()
                .trim(),
            "snap-000001"
        );

        fs::remove_dir_all(&profile).unwrap();
        hydrate(&profile, &durable).expect("hydrate");
        // The ghost snapshot was never published, so it is not restored.
        assert_eq!(
            fs::read(profile.join("graphs/g1/documents/doc-1/document.json")).unwrap(),
            b"{\"doc\":1}"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn second_flush_hard_links_unchanged_files() {
        let _serial = test_serial().lock().unwrap_or_else(|p| p.into_inner());
        let root = temp_dir("hardlink");
        let profile = root.join("profile");
        let durable = root.join("durable");
        seed_profile(&profile);
        flush(&profile, &durable).expect("first flush");

        // Change exactly one file; the rest must be hard-linked. The rewrite
        // must change the file SIZE, not just its bytes: the walker's change
        // detector is size + exact mtime (`same_size_mtime`), and mtimes come
        // from the kernel's coarse clock — a release-profile run fits this
        // whole seed → flush → rewrite sequence inside one clock tick, so an
        // equal-size rewrite is indistinguishable from the previous snapshot's
        // copy and gets silently hard-linked (observed on the release suite
        // as `!second.published`).
        write(
            &profile.join("graphs/g1/documents/doc-1/document.json"),
            b"{\"doc\":2,\"rev\":2}",
        );
        let second = flush(&profile, &durable).expect("second flush");
        assert!(second.published);
        assert!(second.files_linked >= 4, "linked {}", second.files_linked);
        assert!(second.files_copied >= 1, "copied {}", second.files_copied);

        // The unchanged file in the new snapshot shares an inode with the old one.
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let prev = durable.join("snap-000001/jobs/jobs.turso");
            let next = durable.join("snap-000002/jobs/jobs.turso");
            assert_eq!(
                fs::metadata(&prev).unwrap().ino(),
                fs::metadata(&next).unwrap().ino(),
                "unchanged file should be hard-linked across snapshots"
            );
        }
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn prune_keeps_current_and_previous() {
        let _serial = test_serial().lock().unwrap_or_else(|p| p.into_inner());
        let root = temp_dir("prune");
        let profile = root.join("profile");
        let durable = root.join("durable");
        seed_profile(&profile);

        // Three publishing flushes (mutate between each so they publish).
        // Every mutation changes the file SIZE: an equal-size rewrite can
        // collide with the previous snapshot's copy under the size + mtime
        // quick-check when consecutive flushes complete within one
        // coarse-clock tick (see `same_size_mtime`), and a silently skipped
        // publish here would leave pruning untested. Assert each publish so
        // a regression fails at its cause, not at the prune assertions.
        assert!(flush(&profile, &durable).expect("flush 1").published);
        write(
            &profile.join("worklog/operation-completion.jsonl"),
            b"{\"a\":1}\n",
        );
        assert!(flush(&profile, &durable).expect("flush 2").published);
        write(
            &profile.join("worklog/operation-completion.jsonl"),
            b"{\"a\":2,\"b\":2}\n",
        );
        assert!(flush(&profile, &durable).expect("flush 3").published);

        assert!(!durable.join("snap-000001").exists(), "oldest pruned");
        assert!(durable.join("snap-000002").exists(), "previous kept");
        assert!(durable.join("snap-000003").exists(), "current kept");
        assert_eq!(
            fs::read_to_string(durable.join(CURRENT_FILE))
                .unwrap()
                .trim(),
            "snap-000003"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn dirty_skip_when_nothing_changed() {
        let _serial = test_serial().lock().unwrap_or_else(|p| p.into_inner());
        let root = temp_dir("dirty-skip");
        let profile = root.join("profile");
        let durable = root.join("durable");
        seed_profile(&profile);
        flush(&profile, &durable).expect("first flush");

        // No mutation → second flush must not publish.
        let second = flush(&profile, &durable).expect("second flush");
        assert!(!second.published, "unchanged tree must skip publishing");
        assert!(!durable.join("snap-000002").exists());
        assert_eq!(
            fs::read_to_string(durable.join(CURRENT_FILE))
                .unwrap()
                .trim(),
            "snap-000001"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn clean_open_oxigraph_store_second_flush_is_noop() {
        let _serial = test_serial().lock().unwrap_or_else(|p| p.into_inner());
        let root = temp_dir("clean-open-oxigraph");
        let profile = root.join("profile");
        let durable = root.join("durable");
        let graph_id = root
            .file_name()
            .expect("temp dir name")
            .to_string_lossy()
            .into_owned();
        let graph_dir = profile.join("graphs").join(&graph_id);
        fs::create_dir_all(&graph_dir).expect("create graph dir");
        let store =
            crate::rdf_store_service::open_graph_store(&graph_dir).expect("open real graph store");

        let first = flush(&profile, &durable).expect("first real-store flush");
        assert!(first.published);
        assert_eq!(first.stores_backed_up, 1);

        let second = flush(&profile, &durable).expect("second real-store flush");
        assert!(!second.published, "clean open store must not republish");
        assert_eq!(second.stores_backed_up, 0);
        assert!(!durable.join("snap-000002").exists());
        assert!(fs::read_dir(&durable)
            .expect("read durable dir")
            .flatten()
            .all(|entry| !entry
                .file_name()
                .to_string_lossy()
                .starts_with(BUILDING_PREFIX)));

        crate::rdf_store_service::evict_graph_store(&graph_dir).expect("evict graph store");
        drop(store);
        let _ = fs::remove_dir_all(&root);
    }

    /// One self-witnessed attempt of the hydrated-clean proof: publish a
    /// baseline, evict, hydrate into a fresh profile, open the restored store,
    /// flush once — with the global write epoch read immediately before
    /// `hydrate_detailed` and immediately after `flush`.
    struct HydratedCleanAttempt {
        first: FlushOutcome,
        hydrated_quads: usize,
        epoch_before_hydrate: u64,
        epoch_after_flush: u64,
    }

    impl HydratedCleanAttempt {
        /// True when SOMETHING advanced `STORE_WRITE_EPOCH` inside the window.
        /// The attempt's own path makes exactly zero bumps (see the helper's
        /// doc comment), so any movement is foreign — or the fast path having
        /// been switched off by a foreign bump, which then adds the
        /// first-observed bump of `store_epoch_to_backup` on top.
        fn contaminated(&self) -> bool {
            self.epoch_after_flush != self.epoch_before_hydrate
        }
    }

    /// Run one hydrate→open→flush window in a fresh temp root and report what
    /// the global write epoch did around it.
    ///
    /// WHY this is witnessed rather than assumed: the hydrated-clean fast
    /// path is gated on the process-global `STORE_WRITE_EPOCH` being
    /// unchanged between `begin_profile_hydration` (inside `hydrate_detailed`)
    /// and the first `store_epoch_to_backup` (inside `flush`). Production's
    /// premise — one process owns one store, so only that store's own writes
    /// advance the epoch — is FALSE inside the `cargo test --lib` binary:
    /// ~70 tests in seven other files advance the same counter from their
    /// fixtures (see the battery gotcha on `test_serial`), and libtest
    /// schedules them alongside this one. A foreign bump landing inside the
    /// window silently and correctly (conservatively) turns the fast path
    /// off, and the proof assertion fails without any defect in the code
    /// under test. Diagnosis: canary graph doc
    /// `garden-battery-r1-flake-diagnosis-20260825` (cloud garden-battery
    /// lane r1 on main b85c41a: 1447 passed / 1 failed, this test).
    ///
    /// Bump accounting for the window (garden b85c41a): the test's own path
    /// advances the epoch ZERO times between the two reads —
    /// * `hydrate_detailed` only reads it (`begin_profile_hydration`,
    ///   `finish_profile_hydration`);
    /// * `rdf_store_service::open_graph_store` never touches it;
    /// * `flush` on the clean path only reads it (`receipt_epoch_before`,
    ///   the seed equality check in `store_epoch_to_backup`) and takes none
    ///   of the bumping branches (dirty-unknown, poisoned bookkeeping lock,
    ///   first-observed-without-seed, identity replaced, previous snapshot
    ///   missing).
    /// The baseline publish before the window DOES bump (first observation
    /// with no seed, plus no previous snapshot) — which is why the first read
    /// is taken after it, not at the top of the attempt.
    fn hydrated_clean_first_flush_attempt(attempt: usize) -> HydratedCleanAttempt {
        let root = temp_dir(&format!("hydrated-clean-store-a{attempt}"));
        let profile = root.join("profile");
        let restored_profile = root.join("restored-profile");
        let durable = root.join("durable");
        let graph_id = root
            .file_name()
            .expect("temp dir name")
            .to_string_lossy()
            .into_owned();
        let graph_dir = profile.join("graphs").join(&graph_id);
        fs::create_dir_all(&graph_dir).expect("create graph dir");
        let store =
            crate::rdf_store_service::open_graph_store(&graph_dir).expect("open real graph store");

        let baseline = flush(&profile, &durable).expect("publish baseline store");
        assert!(baseline.published);
        assert_eq!(baseline.stores_backed_up, 1);
        crate::rdf_store_service::evict_graph_store(&graph_dir).expect("evict baseline store");
        drop(store);

        // ---- window opens: nothing below on our own path advances the epoch.
        let epoch_before_hydrate = current_write_epoch();
        let hydrated =
            hydrate_detailed(&restored_profile, &durable).expect("hydrate baseline snapshot");
        assert_eq!(hydrated.mode, HydrateMode::Restored);
        assert_eq!(hydrated.snapshot_id.as_deref(), Some("snap-000001"));
        let restored_graph_dir = restored_profile.join("graphs").join(&graph_id);
        let restored_store = crate::rdf_store_service::open_graph_store(&restored_graph_dir)
            .expect("open hydrated store");
        let first = flush(&restored_profile, &durable).expect("first cold-process flush");
        let epoch_after_flush = current_write_epoch();
        // ---- window closes.

        let hydrated_quads = restored_store.len().expect("hydrated store quad count");
        crate::rdf_store_service::evict_graph_store(&restored_graph_dir)
            .expect("evict hydrated store");
        drop(restored_store);
        let _ = fs::remove_dir_all(&root);
        HydratedCleanAttempt {
            first,
            hydrated_quads,
            epoch_before_hydrate,
            epoch_after_flush,
        }
    }

    #[test]
    fn hydrated_clean_oxigraph_store_first_flush_reuses_snapshot() {
        let _serial = test_serial().lock().unwrap_or_else(|p| p.into_inner());
        const MAX_ATTEMPTS: usize = 5;
        let mut contaminated = Vec::new();
        for attempt in 1..=MAX_ATTEMPTS {
            let outcome = hydrated_clean_first_flush_attempt(attempt);
            if outcome.contaminated() && outcome.first.published {
                // A foreign write completion landed inside the window and
                // (correctly, conservatively) switched the fast path off.
                // Nothing about the code under test is refuted; open a fresh
                // window. See `hydrated_clean_first_flush_attempt`.
                contaminated.push((
                    attempt,
                    outcome.epoch_before_hydrate,
                    outcome.epoch_after_flush,
                ));
                continue;
            }
            // Either the window was unwitnessed by any other writer — in
            // which case the proof below is decisive — or the fast path
            // fired anyway: `published == false` on a first observation can
            // only come from the hydrated-clean seed (every other first
            // observation bumps and publishes), so the assertion is sound in
            // both cases. It is NOT weakened to the safety property.
            assert!(
                !outcome.first.published,
                "a store copied from the still-current snapshot with no intervening write must be \
                 clean (attempt {attempt}: epoch before hydrate {}, after flush {}; the epoch did \
                 not move, so no foreign write can explain this)",
                outcome.epoch_before_hydrate, outcome.epoch_after_flush
            );
            assert_eq!(
                outcome.first.stores_backed_up, 0,
                "cold first observation must reuse the hydrated checkpoint"
            );
            assert_eq!(outcome.hydrated_quads, 0);
            return;
        }
        panic!(
            "hydrated-clean proof: all {MAX_ATTEMPTS} attempts were contaminated by foreign \
             write-epoch bumps inside the hydrate→flush window \
             (attempt, epoch before hydrate, epoch after flush): {contaminated:?}; \
             STORE_DIRTY_UNKNOWN={}. This is a finding about the test suite's scheduling (or \
             poisoned dirty bookkeeping for the rest of the process), not about hydrate/flush — \
             see garden-battery-r1-flake-diagnosis-20260825.",
            STORE_DIRTY_UNKNOWN.load(Ordering::Acquire)
        );
    }

    /// Battery gotcha (see `test_serial`): unlike its sibling above, this test
    /// needs no contamination guard. Its own SPARQL update advances the epoch
    /// on purpose, and a foreign bump landing inside the window can only push
    /// the store further toward "dirty" — the direction every assertion here
    /// already expects. It cannot flake in the other direction.
    #[test]
    fn write_between_hydrate_and_first_flush_forces_real_store_backup() {
        let _serial = test_serial().lock().unwrap_or_else(|p| p.into_inner());
        let root = temp_dir("hydrated-store-written-before-flush");
        let profile = root.join("profile");
        let restored_profile = root.join("restored-profile");
        let durable = root.join("durable");
        let graph_id = root
            .file_name()
            .expect("temp dir name")
            .to_string_lossy()
            .into_owned();
        let graph_dir = profile.join("graphs").join(&graph_id);
        fs::create_dir_all(&graph_dir).expect("create graph dir");
        let store =
            crate::rdf_store_service::open_graph_store(&graph_dir).expect("open real graph store");
        let baseline = flush(&profile, &durable).expect("publish baseline store");
        assert!(baseline.published);
        crate::rdf_store_service::evict_graph_store(&graph_dir).expect("evict baseline store");
        drop(store);

        hydrate_detailed(&restored_profile, &durable).expect("hydrate baseline snapshot");
        let restored_graph_dir = restored_profile.join("graphs").join(&graph_id);
        let restored_store = crate::rdf_store_service::open_graph_store(&restored_graph_dir)
            .expect("open hydrated store");
        crate::rdf_query_service::execute_sparql_update(
            &restored_store,
            "INSERT DATA { <http://example.com/s> <http://example.com/p> <http://example.com/o> . }",
        )
        .expect("write after hydrate and before first flush");

        let first = flush_forced(&restored_profile, &durable).expect("flush post-hydrate write");
        assert!(first.published, "the post-hydrate write must publish");
        assert_eq!(first.sequence, 2);
        assert_eq!(
            first.stores_backed_up, 1,
            "an advanced write epoch must invalidate the clean hydrate seed"
        );
        let backed_up = Store::open_read_only(
            durable
                .join("snap-000002")
                .join("graphs")
                .join(&graph_id)
                .join("store.oxigraph"),
        )
        .expect("open backed-up post-hydrate store");
        assert_eq!(backed_up.len().expect("backed-up quad count"), 1);
        drop(backed_up);

        crate::rdf_store_service::evict_graph_store(&restored_graph_dir)
            .expect("evict hydrated store");
        drop(restored_store);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn oxigraph_quad_write_after_flush_is_published() {
        let _serial = test_serial().lock().unwrap_or_else(|p| p.into_inner());
        let root = temp_dir("changed-open-oxigraph");
        let profile = root.join("profile");
        let durable = root.join("durable");
        let graph_id = root
            .file_name()
            .expect("temp dir name")
            .to_string_lossy()
            .into_owned();
        let graph_dir = profile.join("graphs").join(&graph_id);
        fs::create_dir_all(&graph_dir).expect("create graph dir");
        let store =
            crate::rdf_store_service::open_graph_store(&graph_dir).expect("open real graph store");

        let first = flush(&profile, &durable).expect("first real-store flush");
        assert!(first.published);

        crate::rdf_query_service::execute_sparql_update(
            &store,
            "INSERT DATA { <http://example.com/s> <http://example.com/p> <http://example.com/o> . }",
        )
        .expect("write real quad through Garden RDF boundary");

        let second = flush_forced(&profile, &durable).expect("flush changed real store");
        assert!(second.published, "a completed quad write must publish");
        assert_eq!(second.stores_backed_up, 1);
        assert_eq!(second.sequence, 2);

        let current = fs::read_to_string(durable.join(CURRENT_FILE)).expect("read CURRENT");
        let backed_up_store = durable
            .join(current.trim())
            .join("graphs")
            .join(&graph_id)
            .join("store.oxigraph");
        let restored = Store::open_read_only(&backed_up_store).expect("open real store backup");
        assert_eq!(restored.len().expect("backup quad count"), 1);
        drop(restored);

        crate::rdf_store_service::evict_graph_store(&graph_dir).expect("evict graph store");
        drop(store);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn read_only_lease_acquire_and_drop_does_not_dirty_a_clean_store() {
        // FIX A regression (A1): a `HotWriteLease` acquire+drop with
        // no real Oxigraph mutation under it — declared read-only exactly as
        // the WS update-apply branch in crdt_engine::rooms does for a no-op
        // sync frame — must NOT dirty the store. The next flush must be a
        // genuine no-op (published: false), reviving the dirty-skip that the
        // parent fix (abf49b7) intended.
        let _serial = test_serial().lock().unwrap_or_else(|p| p.into_inner());
        let root = temp_dir("read-only-lease-clean");
        let profile = root.join("profile");
        let durable = root.join("durable");
        let graph_id = root
            .file_name()
            .expect("temp dir name")
            .to_string_lossy()
            .into_owned();
        let graph_dir = profile.join("graphs").join(&graph_id);
        fs::create_dir_all(&graph_dir).expect("create graph dir");
        let store =
            crate::rdf_store_service::open_graph_store(&graph_dir).expect("open real graph store");

        let first = flush(&profile, &durable).expect("first real-store flush");
        assert!(first.published);
        assert_eq!(first.stores_backed_up, 1);

        let coordinator =
            crate::crdt_engine::persistence_coordinator::GraphPersistenceCoordinator::default();
        let lease = coordinator
            .acquire_hot_write_blocking(&graph_id)
            .expect("acquire graph lease");
        lease.declare_rdf_read_only();
        drop(lease);

        let second = flush(&profile, &durable).expect("second real-store flush");
        assert!(
            !second.published,
            "a read-only lease drop with no real write must not dirty a clean store"
        );
        assert_eq!(second.stores_backed_up, 0);
        assert!(!durable.join("snap-000002").exists());

        crate::rdf_store_service::evict_graph_store(&graph_dir).expect("evict graph store");
        drop(store);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn direct_store_write_under_a_default_lease_still_dirties_the_store() {
        // FIX A regression (A2, the safety direction): a real Oxigraph
        // mutation performed directly against `&Store` — the shape of a
        // low-level materializer that "intentionally accepts only &Store"
        // (Emporium reconcile/memory_applier/etc. — out of scope to touch
        // here) and so never calls the narrow `mark_rdf_store_written` hook
        // itself — must still be caught. The enclosing lease is NOT declared
        // read-only (the default, conservative case), so its Drop must still
        // mark the graph's stores dirty via the coarse per-graph fallback,
        // and the next flush must publish.
        let _serial = test_serial().lock().unwrap_or_else(|p| p.into_inner());
        let root = temp_dir("coarse-lease-dirty");
        let profile = root.join("profile");
        let durable = root.join("durable");
        let graph_id = root
            .file_name()
            .expect("temp dir name")
            .to_string_lossy()
            .into_owned();
        let graph_dir = profile.join("graphs").join(&graph_id);
        fs::create_dir_all(&graph_dir).expect("create graph dir");
        let store =
            crate::rdf_store_service::open_graph_store(&graph_dir).expect("open real graph store");

        let first = flush(&profile, &durable).expect("first real-store flush");
        assert!(first.published);

        let coordinator =
            crate::crdt_engine::persistence_coordinator::GraphPersistenceCoordinator::default();
        let lease = coordinator
            .acquire_hot_write_blocking(&graph_id)
            .expect("acquire graph lease");
        store
            .insert(QuadRef::new(
                NamedNodeRef::new("http://example.com/s").unwrap(),
                NamedNodeRef::new("http://example.com/p").unwrap(),
                NamedNodeRef::new("http://example.com/o").unwrap(),
                GraphNameRef::DefaultGraph,
            ))
            .expect("write real quad directly against the store, bypassing the narrow hook");
        drop(lease);

        let second = flush_forced(&profile, &durable).expect("flush changed real store");
        assert!(
            second.published,
            "a real store mutation under a default (non-read-only) lease must still publish"
        );
        assert_eq!(second.stores_backed_up, 1);

        crate::rdf_store_service::evict_graph_store(&graph_dir).expect("evict graph store");
        drop(store);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn concurrent_flushes_serialize_and_publish_complete_snapshots() {
        let _serial = test_serial().lock().unwrap_or_else(|p| p.into_inner());
        // Regression: an aborted async caller leaves its blocking flush running;
        // a concurrently started flush must serialize behind it instead of
        // racing onto the same snapshot name (observed as a published snapshot
        // missing graphs/ entirely).
        let root = temp_dir("concurrent");
        let profile = root.join("profile");
        let durable = root.join("durable");
        seed_profile(&profile);

        let handles: Vec<_> = (0..4)
            .map(|i| {
                let profile = profile.clone();
                let durable = durable.clone();
                std::thread::spawn(move || {
                    // Each thread mutates a file first so every flush is dirty.
                    write(
                        &profile.join("worklog/operation-completion.jsonl"),
                        format!("{{\"thread\":{i}}}\n").as_bytes(),
                    );
                    flush(&profile, &durable).unwrap()
                })
            })
            .collect();
        for handle in handles {
            handle.join().unwrap();
        }

        let current = read_current(&durable).unwrap().expect("CURRENT published");
        let snap = durable.join(&current);
        for required in ["graphs", "worklog", "jobs", "metadata.turso"] {
            assert!(snap.join(required).exists(), "snapshot missing {required}");
        }
        // No half-built dirs survive.
        for entry in fs::read_dir(&durable).unwrap().flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            assert!(
                !name.starts_with(BUILDING_PREFIX),
                "stale build dir left behind: {name}"
            );
        }
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn flush_never_reuses_existing_snapshot_names() {
        let _serial = test_serial().lock().unwrap_or_else(|p| p.into_inner());
        let root = temp_dir("no-reuse");
        let profile = root.join("profile");
        let durable = root.join("durable");
        seed_profile(&profile);
        // A complete-but-unreferenced snapshot from a crashed flush sits at a
        // higher sequence than CURRENT will say.
        write(&durable.join("snap-000007/orphan.txt"), b"crashed flush");

        let outcome = flush(&profile, &durable).unwrap();
        assert!(outcome.published);
        // Claims a fresh name past the orphan instead of wiping and reusing it
        // (the orphan is then legitimately removed by prune, as an old snap).
        assert_eq!(
            outcome.sequence, 8,
            "must claim a fresh name past snap-000007"
        );
        assert_eq!(
            read_current(&durable).unwrap().as_deref(),
            Some("snap-000008")
        );
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn hydrate_falls_back_when_current_dangles() {
        let _serial = test_serial().lock().unwrap_or_else(|p| p.into_inner());
        let root = temp_dir("dangling");
        let profile = root.join("profile");
        let durable = root.join("durable");
        seed_profile(&profile);
        flush(&profile, &durable).unwrap();
        // Dangle CURRENT at a snapshot that no longer exists.
        write(&durable.join(CURRENT_FILE), b"snap-000999");

        let restored = root.join("restored");
        assert!(hydrate(&restored, &durable).unwrap());
        assert!(restored
            .join("graphs/g1/documents/doc-1/document.json")
            .exists());
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn flush_refuses_snapshot_missing_toplevel_entries() {
        let _serial = test_serial().lock().unwrap_or_else(|p| p.into_inner());
        let root = temp_dir("verify");
        let profile = root.join("profile");
        let snap = root.join("snap");
        seed_profile(&profile);
        // Simulate a walk regression: snapshot has everything except graphs/.
        fs::create_dir_all(&snap).unwrap();
        for keep in ["metadata.turso", "metadata.turso-wal", "jobs/jobs.turso"] {
            let src = profile.join(keep);
            let dst = snap.join(keep);
            fs::create_dir_all(dst.parent().unwrap()).unwrap();
            fs::copy(&src, &dst).unwrap();
        }
        write(&snap.join("worklog/operation-completion.jsonl"), b"{}\n");
        let error = verify_snapshot_complete(&profile, &snap).unwrap_err();
        assert!(
            error.contains("graphs"),
            "error should name the missing entry: {error}"
        );
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn flush_skips_instead_of_deadlocking_when_gate_held() {
        let _serial = test_serial().lock().unwrap_or_else(|p| p.into_inner());
        // Regression: a queued writer blocks new readers, so a write path that
        // nests read guards deadlocked the cell at the first flush tick during
        // a long import. The flusher must try_write + skip, never queue.
        let root = temp_dir("contended");
        let profile = root.join("profile");
        let durable = root.join("durable");
        seed_profile(&profile);

        let _held = flush_gate().read().unwrap();
        let started = std::time::Instant::now();
        let outcome = flush(&profile, &durable).unwrap();
        assert!(!outcome.published, "must skip while the gate is held");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "must give up quickly, not block"
        );
        drop(_held);

        // And a quiet gate flushes normally.
        let outcome = flush(&profile, &durable).unwrap();
        assert!(outcome.published);
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn nested_write_tokens_do_not_deadlock_behind_a_waiting_flusher() {
        let _serial = test_serial().lock().unwrap_or_else(|p| p.into_inner());
        let outer = write_guard_for_test().expect("outer durability token");
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (acquired_tx, acquired_rx) = std::sync::mpsc::channel();
        let writer = std::thread::spawn(move || {
            started_tx.send(()).expect("announce waiting writer");
            let _write = flush_gate().write().expect("flush writer lock");
            acquired_tx.send(()).expect("announce acquired writer");
        });
        started_rx.recv().expect("writer started");
        std::thread::sleep(Duration::from_millis(25));

        // A raw second read may be held behind the queued writer. The
        // re-entrant token must reuse the first thread-local reader instead.
        let started = Instant::now();
        let inner = write_guard_for_test().expect("nested durability token");
        assert!(
            started.elapsed() < Duration::from_millis(100),
            "nested persistence helper waited behind the flusher"
        );

        // Dropping tokens out of lexical order must retain the real reader
        // until the final nested token exits.
        drop(outer);
        assert!(
            acquired_rx.recv_timeout(Duration::from_millis(50)).is_err(),
            "flusher acquired while a nested durability token remained"
        );
        drop(inner);
        acquired_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("flusher acquired after final token");
        writer.join().expect("writer thread");
    }

    #[test]
    fn periodic_flush_defers_during_import_forced_proceeds() {
        let _serial = test_serial().lock().unwrap_or_else(|p| p.into_inner());
        // A periodic flush landing mid-import would hold the gate's write guard
        // across a long EFS copy and starve the runtime; it must defer. The
        // forced path (shutdown / post-import) must still proceed.
        let root = temp_dir("import-defer");
        let profile = root.join("profile");
        let durable = root.join("durable");
        seed_profile(&profile);

        // Drive IMPORT_ACTIVE directly (import_guard() is a no-op under cfg(test)
        // to keep this process-global from leaking across parallel test modules).
        // Decrement BEFORE the asserts so a failure cannot leak the flag.
        IMPORT_ACTIVE.fetch_add(1, Ordering::SeqCst);
        let periodic = flush(&profile, &durable);
        let forced = flush_forced(&profile, &durable);
        // Decrement BEFORE unwrap/assert so a panic can never leak the flag.
        IMPORT_ACTIVE.fetch_sub(1, Ordering::SeqCst);
        assert!(
            !periodic.unwrap().published,
            "periodic flush must defer while an import is active"
        );
        assert!(
            forced.unwrap().published,
            "forced flush must proceed even during an import"
        );

        // Periodic flushing resumes once the import is done.
        write(&profile.join("worklog/x.jsonl"), b"changed\n");
        let resumed = flush(&profile, &durable).unwrap();
        assert!(resumed.published, "periodic flush resumes after import");
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn forced_flush_waits_past_periodic_deadline_for_writers_to_drain() {
        let _serial = test_serial().lock().unwrap_or_else(|p| p.into_inner());
        let root = temp_dir("forced-contention-drains");
        let profile = root.join("profile");
        let durable = root.join("durable");
        seed_profile(&profile);

        let (held_tx, held_rx) = std::sync::mpsc::channel();
        let holder = std::thread::spawn(move || {
            let _held = flush_gate().read().expect("hold active write token");
            held_tx.send(()).expect("announce held gate");
            std::thread::sleep(PERIODIC_FLUSH_GATE_WAIT + Duration::from_millis(250));
        });
        held_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("writer acquired gate");

        let started = Instant::now();
        let outcome = flush_forced(&profile, &durable).expect("forced flush after writer drain");
        assert!(outcome.published);
        assert!(
            started.elapsed() >= PERIODIC_FLUSH_GATE_WAIT,
            "forced flush gave up at the periodic contention deadline"
        );
        holder.join().expect("gate holder");
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn forced_flush_gate_timeout_is_an_error_not_a_noop() {
        let _serial = test_serial().lock().unwrap_or_else(|p| p.into_inner());
        let root = temp_dir("forced-contention-timeout");
        let profile = root.join("profile");
        let durable = root.join("durable");
        seed_profile(&profile);

        let held = flush_gate().read().expect("hold active write token");
        let error = flush_inner_with_gate_wait(&profile, &durable, true, Duration::from_millis(75))
            .expect_err("forced contention must not look like an unchanged snapshot");
        assert!(
            error.contains("forced durable flush timed out"),
            "unexpected forced contention error: {error}"
        );
        assert!(
            !durable.join(CURRENT_FILE).exists(),
            "timed-out forced flush published a snapshot"
        );
        drop(held);

        let recovered = flush_forced(&profile, &durable).expect("forced retry after drain");
        assert!(recovered.published);
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn first_store_open_waits_until_flush_finishes_its_enumerated_file_walk() {
        let _serial = test_serial().lock().unwrap_or_else(|p| p.into_inner());
        let root = temp_dir("first-store-open-barrier");
        let profile = root.join("profile");
        let durable = root.join("durable");
        seed_profile(&profile);
        let graph_dir = profile.join("graphs/first-open");
        write(
            &graph_dir.join("graph.json"),
            br#"{"graphId":"first-open"}"#,
        );

        let (reached_tx, reached_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        install_rdf_store_enumeration_pause_for_test(reached_tx, release_rx);
        let flush_profile = profile.clone();
        let flush_durable = durable.clone();
        let flusher = std::thread::spawn(move || flush(&flush_profile, &flush_durable));
        reached_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("flush reached post-enumeration boundary");

        let (opened_tx, opened_rx) = std::sync::mpsc::channel();
        let open_graph_dir = graph_dir.clone();
        let opener = std::thread::spawn(move || {
            let result = crate::rdf_store_service::open_graph_store(&open_graph_dir);
            let _ = opened_tx.send(result.is_ok());
            result
        });
        assert!(
            opened_rx.recv_timeout(Duration::from_millis(75)).is_err(),
            "a first store open crossed the flusher's enumerated lifecycle boundary"
        );
        assert!(
            !graph_dir.join("store.oxigraph").exists(),
            "blocked first open created a RocksDB directory during the plain walk"
        );

        release_tx.send(()).expect("release durability flush");
        let outcome = flusher
            .join()
            .expect("flush thread")
            .expect("durability flush");
        assert!(outcome.published);
        assert!(
            opened_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("store open completed after flush"),
            "store open failed after lifecycle boundary"
        );
        let store = opener
            .join()
            .expect("store opener thread")
            .expect("open first store");

        let current = read_current(&durable)
            .expect("read CURRENT")
            .expect("published snapshot");
        assert!(
            !durable
                .join(current)
                .join("graphs/first-open/store.oxigraph")
                .exists(),
            "the post-enumeration store was plain-copied into the earlier snapshot"
        );

        crate::rdf_store_service::evict_graph_store(&graph_dir).expect("evict first store");
        drop(store);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn cached_store_lookup_does_not_wait_for_durable_file_walk() {
        let _serial = test_serial().lock().unwrap_or_else(|p| p.into_inner());
        let root = temp_dir("cached-store-fast-path");
        let profile = root.join("profile");
        let durable = root.join("durable");
        seed_profile(&profile);
        let graph_dir = profile.join("graphs/cache-hit");
        write(&graph_dir.join("graph.json"), br#"{"graphId":"cache-hit"}"#);
        let opened = crate::rdf_store_service::open_graph_store(&graph_dir)
            .expect("prime graph store cache");

        let (reached_tx, reached_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        install_rdf_store_enumeration_pause_for_test(reached_tx, release_rx);
        let flush_profile = profile.clone();
        let flush_durable = durable.clone();
        let flusher = std::thread::spawn(move || flush(&flush_profile, &flush_durable));
        reached_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("flush reached post-enumeration boundary");

        let started = Instant::now();
        let cache_hit = crate::rdf_store_service::open_graph_store(&graph_dir)
            .expect("open already-cached store during flush");
        assert!(std::sync::Arc::ptr_eq(&opened, &cache_hit));
        assert!(
            started.elapsed() < Duration::from_millis(100),
            "cache hit stalled behind the durability lifecycle barrier"
        );

        release_tx.send(()).expect("release durability flush");
        let outcome = flusher
            .join()
            .expect("flush thread")
            .expect("durability flush");
        assert!(outcome.published);

        drop(cache_hit);
        crate::rdf_store_service::evict_graph_store(&graph_dir).expect("evict cached store");
        drop(opened);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn registered_non_suffix_omphalos_store_is_never_plain_copied() {
        let root = temp_dir("omphalos-store-skip");
        let profile = root.join("profile");
        let snapshot = root.join("snapshot");
        let omphalos = profile.join("omphalos");
        let open_store_paths = vec![omphalos.clone()];
        let mut outcome = FlushOutcome::default();
        let walker = FlushWalker {
            profile_dir: &profile,
            next_dir: &snapshot,
            previous_dir: None,
            open_store_paths: &open_store_paths,
            outcome: &mut outcome,
            link_from_source: false,
        };

        assert!(
            walker.should_skip(&omphalos, true),
            "the live Omphalos RocksDB root lacks an .oxigraph suffix but must be checkpoint-only"
        );
        drop(walker);

        write(&omphalos.join("constitution.ttl"), b"authored constitution");
        fs::create_dir_all(&snapshot).expect("snapshot store root");
        copy_registered_store_sidecars(&omphalos, &snapshot, &mut outcome)
            .expect("copy authored Omphalos sidecar");
        assert_eq!(
            fs::read(snapshot.join("constitution.ttl")).expect("read copied constitution"),
            b"authored constitution"
        );
        assert_eq!(outcome.files_copied, 1);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn same_id_publication_waits_and_cannot_mix_old_store_with_new_manifest() {
        use crate::graph_duplicate_storage::{ExistingTargetPolicy, GraphPublicationReservation};

        let _serial = test_serial().lock().unwrap_or_else(|p| p.into_inner());
        let root = temp_dir("same-id-publication-barrier");
        let profile = root.join("profile");
        let durable = root.join("durable");
        seed_profile(&profile);
        let target = profile.join("graphs/reused");
        write(
            &target.join("graph.json"),
            br#"{"graphId":"reused","status":"active"}"#,
        );
        let old_store = crate::rdf_store_service::open_graph_store(&target).expect("old store");
        old_store
            .insert(QuadRef::new(
                NamedNodeRef::new("http://example.com/old").unwrap(),
                NamedNodeRef::new("http://example.com/p").unwrap(),
                NamedNodeRef::new("http://example.com/o").unwrap(),
                GraphNameRef::DefaultGraph,
            ))
            .expect("insert old quad");
        let detached_old = profile.join("detached-old-incarnation");
        fs::rename(&target, &detached_old).expect("detach old graph incarnation");
        let old_store_weak = std::sync::Arc::downgrade(&old_store);
        drop(old_store);
        assert!(
            old_store_weak.upgrade().is_some(),
            "the final-path cache must retain the detached old incarnation before publication"
        );

        let publication = GraphPublicationReservation::acquire(
            &profile,
            target.clone(),
            "reused",
            ExistingTargetPolicy::Reject,
        )
        .expect("reserve replacement graph");
        let replacement_store = crate::rdf_store_service::open_graph_store(publication.stage_dir())
            .expect("replacement stage store");
        for suffix in ["new-one", "new-two"] {
            let subject = format!("http://example.com/{suffix}");
            replacement_store
                .insert(QuadRef::new(
                    NamedNodeRef::new(&subject).unwrap(),
                    NamedNodeRef::new("http://example.com/p").unwrap(),
                    NamedNodeRef::new("http://example.com/o").unwrap(),
                    GraphNameRef::DefaultGraph,
                ))
                .expect("insert replacement quad");
        }
        drop(replacement_store);
        crate::rdf_store_service::evict_graph_store(publication.stage_dir())
            .expect("evict replacement staging handle");
        write(
            &publication.stage_dir().join("graph.json"),
            br#"{"graphId":"reused","status":"active","incarnationId":"replacement"}"#,
        );

        let (reached_tx, reached_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        install_rdf_store_enumeration_pause_for_test(reached_tx, release_rx);
        let flush_profile = profile.clone();
        let flush_durable = durable.clone();
        let flusher = std::thread::spawn(move || flush(&flush_profile, &flush_durable));
        reached_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("flush reached post-enumeration boundary");

        let (published_tx, published_rx) = std::sync::mpsc::channel();
        let publisher = std::thread::spawn(move || {
            let result = publication.publish();
            let _ = published_tx.send(result.clone());
            result
        });
        assert!(
            published_rx
                .recv_timeout(Duration::from_millis(75))
                .is_err(),
            "same-ID publication crossed the flusher's store lifecycle boundary"
        );
        assert!(
            !target.exists(),
            "replacement became visible while the old store epoch was enumerated"
        );

        release_tx.send(()).expect("release durability flush");
        let outcome = flusher
            .join()
            .expect("flush thread")
            .expect("durability flush");
        assert!(outcome.published);
        published_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("publication completed after flush")
            .expect("publish replacement");
        publisher
            .join()
            .expect("publisher thread")
            .expect("publisher result");

        let current = read_current(&durable)
            .expect("read CURRENT")
            .expect("published snapshot");
        let snap_target = durable.join(current).join("graphs/reused");
        assert!(
            !snap_target.join("graph.json").exists(),
            "snapshot mixed the replacement manifest into the old store epoch"
        );
        assert!(
            !snap_target.join("store.oxigraph").exists(),
            "snapshot resurrected the detached old store at its canonical path"
        );

        let reopened = crate::rdf_store_service::open_graph_store(&target)
            .expect("open published replacement store");
        assert_eq!(reopened.len().expect("replacement store len"), 2);
        assert!(
            old_store_weak.upgrade().is_none(),
            "publication left the detached final-path store incarnation alive"
        );
        crate::rdf_store_service::evict_graph_store(&target).expect("evict replacement");
        drop(reopened);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn oxigraph_backup_roundtrips_while_store_open() {
        let _serial = test_serial().lock().unwrap_or_else(|p| p.into_inner());
        let root = temp_dir("oxigraph-backup");
        let store_dir = root.join("store.oxigraph");
        let backup_dir = root.join("backup.oxigraph");
        fs::create_dir_all(&root).unwrap();

        let store = Store::open(&store_dir).expect("open store");
        let subject = NamedNodeRef::new("http://example.com/s").unwrap();
        let predicate = NamedNodeRef::new("http://example.com/p").unwrap();
        let object = NamedNodeRef::new("http://example.com/o").unwrap();
        store
            .insert(QuadRef::new(
                subject,
                predicate,
                object,
                GraphNameRef::DefaultGraph,
            ))
            .expect("insert quad");

        // Backup while the store is still open (the live-checkpoint case).
        store.backup(&backup_dir).expect("backup");
        assert_eq!(store.len().unwrap(), 1);

        // Reopen the backup independently and confirm the quad survived.
        drop(store);
        let restored = Store::open(&backup_dir).expect("open backup");
        assert_eq!(restored.len().unwrap(), 1);
        let _ = fs::remove_dir_all(&root);
    }

    // -- DirtyFlushScheduler: pure decision logic -----------------------
    //
    // `poll`/`record_flush_attempt` never read the system clock or the global
    // epoch themselves — every case below drives synthetic `Instant`s and
    // epoch values directly, so there is no real or `tokio::time`-simulated
    // sleeping anywhere in this group.

    #[test]
    fn scheduler_idle_never_flushes() {
        let t0 = Instant::now();
        let mut scheduler = DirtyFlushScheduler::new(
            Duration::from_secs(5),
            Duration::from_secs(30),
            /* baseline epoch */ 100,
        );
        // Epoch never advances past the baseline: every poll, however far out
        // in wall-clock time, must stay a no-op `Wait` — an idle cell must
        // never even attempt a flush, let alone publish one.
        for seconds in [0, 1, 5, 30, 300, 3600] {
            assert_eq!(
                scheduler.poll(t0 + Duration::from_secs(seconds), 100),
                SchedulerAction::Wait,
                "idle at t+{seconds}s must not flush"
            );
        }
    }

    #[test]
    fn scheduler_debounces_a_single_write() {
        let t0 = Instant::now();
        let mut scheduler =
            DirtyFlushScheduler::new(Duration::from_secs(5), Duration::from_secs(30), 1);
        // A single write bumps the epoch to 2 at t=0.
        assert_eq!(scheduler.poll(t0, 2), SchedulerAction::Wait);
        // Still quiet before the 5s debounce window elapses.
        assert_eq!(
            scheduler.poll(t0 + Duration::from_secs(4), 2),
            SchedulerAction::Wait
        );
        // At/after the debounce deadline, with no further writes, flush.
        assert_eq!(
            scheduler.poll(t0 + Duration::from_secs(5), 2),
            SchedulerAction::FlushNow
        );
    }

    #[test]
    fn scheduler_max_rpo_forces_flush_under_continuous_writes() {
        let t0 = Instant::now();
        let debounce = Duration::from_secs(5);
        let max_rpo = Duration::from_secs(10);
        let mut scheduler = DirtyFlushScheduler::new(debounce, max_rpo, 1);
        // A write every 3s keeps re-arming the 5s debounce window (it never
        // naturally quiesces), but the 10s max-RPO ceiling is measured from
        // the FIRST unflushed write and must still fire — proving the
        // ceiling is honored even under continuous re-arming.
        assert_eq!(scheduler.poll(t0, 2), SchedulerAction::Wait); // first_pending_at = t0
        assert_eq!(
            scheduler.poll(t0 + Duration::from_secs(3), 3),
            SchedulerAction::Wait
        );
        assert_eq!(
            scheduler.poll(t0 + Duration::from_secs(6), 4),
            SchedulerAction::Wait
        );
        assert_eq!(
            scheduler.poll(t0 + Duration::from_secs(9), 5),
            SchedulerAction::Wait
        );
        // t=11: debounce deadline (9+5=14) has NOT passed, but the max-RPO
        // ceiling (t0+10=10) has — must force a flush despite ongoing writes.
        // `FlushAtRpoCeiling`, not `FlushNow`: the caller MUST escalate to a
        // forced attempt here, or a concurrent heavy import could defer this
        // forever (finding #2 of the 2026-07-18 durability re-refute).
        assert_eq!(
            scheduler.poll(t0 + Duration::from_secs(11), 5),
            SchedulerAction::FlushAtRpoCeiling
        );
    }

    #[test]
    fn scheduler_published_or_confirmed_clean_resets_pending_state() {
        let t0 = Instant::now();
        for outcome in [
            FlushAttemptOutcome::Published,
            FlushAttemptOutcome::ConfirmedClean,
        ] {
            let mut scheduler =
                DirtyFlushScheduler::new(Duration::from_secs(5), Duration::from_secs(30), 1);
            assert_eq!(scheduler.poll(t0, 2), SchedulerAction::Wait);
            assert_eq!(
                scheduler.poll(t0 + Duration::from_secs(5), 2),
                SchedulerAction::FlushNow
            );
            // The attempt captured epoch=2 before it ran, and reached `outcome`.
            scheduler.record_flush_attempt(2, outcome);
            // Immediately after, with no further writes, nothing is pending.
            assert_eq!(
                scheduler.poll(t0 + Duration::from_secs(5), 2),
                SchedulerAction::Wait,
                "{outcome:?} must resolve the pending epoch"
            );
            // Even much later, still nothing pending (idle again).
            assert_eq!(
                scheduler.poll(t0 + Duration::from_secs(600), 2),
                SchedulerAction::Wait
            );
        }
    }

    #[test]
    fn scheduler_deferred_attempt_does_not_resolve_pending_state() {
        // FALSE-CLEAN GUARD: a `Deferred` outcome means the flush returned
        // before completing its dirty computation (import active, or the
        // write-path gate was contended past its wait budget). Treating that
        // as "resolved" would be exactly the false-CLEAN this scheduler must
        // never produce — a still-pending write would be silently forgotten.
        let t0 = Instant::now();
        let mut scheduler =
            DirtyFlushScheduler::new(Duration::from_secs(5), Duration::from_secs(30), 1);
        assert_eq!(scheduler.poll(t0, 2), SchedulerAction::Wait);
        assert_eq!(
            scheduler.poll(t0 + Duration::from_secs(5), 2),
            SchedulerAction::FlushNow
        );
        scheduler.record_flush_attempt(2, FlushAttemptOutcome::Deferred);
        // The pending epoch (2) must still be considered unresolved: the very
        // next poll must want to flush again immediately (its deadlines had
        // already elapsed when the deferred attempt was triggered).
        assert_eq!(
            scheduler.poll(t0 + Duration::from_secs(5), 2),
            SchedulerAction::FlushNow,
            "a deferred attempt must not be mistaken for a resolved one"
        );
    }

    #[test]
    fn scheduler_write_landing_during_flush_remains_pending_for_next_round() {
        let t0 = Instant::now();
        let mut scheduler =
            DirtyFlushScheduler::new(Duration::from_secs(5), Duration::from_secs(30), 1);
        assert_eq!(scheduler.poll(t0, 2), SchedulerAction::Wait);
        assert_eq!(
            scheduler.poll(t0 + Duration::from_secs(5), 2),
            SchedulerAction::FlushNow
        );
        // The attempt captured epoch=2 before starting, but a write landed
        // WHILE it ran (mirrors flush()'s own "acknowledge only the epoch
        // captured before the checkpoint" discipline) — the real epoch is
        // now 3 by the time the attempt completes.
        scheduler.record_flush_attempt(2, FlushAttemptOutcome::Published);
        assert_eq!(
            scheduler.poll(t0 + Duration::from_secs(5), 3),
            SchedulerAction::Wait,
            "the mid-flush write starts a fresh debounce window"
        );
        assert_eq!(
            scheduler.poll(t0 + Duration::from_secs(10), 3),
            SchedulerAction::FlushNow,
            "the mid-flush write must still be flushed once its own debounce elapses"
        );
    }

    // -- DirtyFlushScheduler: real flush()/store integration -------------

    #[test]
    fn scheduler_never_calls_flush_while_truly_idle() {
        let _serial = test_serial().lock().unwrap_or_else(|p| p.into_inner());
        let root = temp_dir("scheduler-idle");
        let profile = root.join("profile");
        let durable = root.join("durable");
        let graph_id = root
            .file_name()
            .expect("temp dir name")
            .to_string_lossy()
            .into_owned();
        let graph_dir = profile.join("graphs").join(&graph_id);
        fs::create_dir_all(&graph_dir).expect("create graph dir");
        let store =
            crate::rdf_store_service::open_graph_store(&graph_dir).expect("open real graph store");

        let baseline = flush(&profile, &durable).expect("baseline flush");
        assert!(baseline.published);

        let t0 = Instant::now();
        let mut scheduler = DirtyFlushScheduler::new(
            Duration::from_secs(5),
            Duration::from_secs(30),
            current_write_epoch(),
        );
        let mut flush_attempts = 0usize;
        // Poll far past both the debounce window and the max-RPO ceiling,
        // with zero real activity in *this profile* in between. A genuinely
        // idle cell must never even attempt a flush.
        //
        // Note on the assertion below: `current_write_epoch` reads a single
        // process-wide counter, and `cargo test` runs this whole crate's
        // tests in one process, so an unrelated concurrently-running test
        // touching an entirely different store can in principle bump it
        // during this loop. That cannot make THIS test's profile dirty
        // (per-store dirtiness in `flush` is tracked by path/identity, not by
        // this counter), so `flush_attempts == 0` is expected in isolation
        // but not load-bearing for safety; the load-bearing assertion is the
        // one after the loop, which holds even if a spurious wake occurs.
        for seconds in [0, 1, 5, 10, 30, 60, 300] {
            match scheduler.poll(t0 + Duration::from_secs(seconds), current_write_epoch()) {
                SchedulerAction::Wait => {}
                // Both wake variants get the same treatment here: whether a
                // spurious wake would (if real) be an ordinary or an
                // RPO-forced attempt does not matter for THIS profile — it
                // must confirm clean and not publish either way.
                SchedulerAction::FlushNow | SchedulerAction::FlushAtRpoCeiling => {
                    flush_attempts += 1;
                    let epoch_before = current_write_epoch();
                    let outcome = flush(&profile, &durable).expect("flush attempt");
                    assert!(
                        !outcome.published,
                        "an idle profile must never publish, even on a spurious wake"
                    );
                    scheduler
                        .record_flush_attempt(epoch_before, FlushAttemptOutcome::ConfirmedClean);
                }
            }
        }
        if flush_attempts > 0 {
            log::warn!(
                "scheduler_never_calls_flush_while_truly_idle: {flush_attempts} spurious wake(s) \
                 from unrelated concurrent test activity on the shared process-wide write epoch \
                 (harmless — each confirmed clean and did not publish)"
            );
        }
        assert!(
            !durable.join("snap-000002").exists(),
            "no second snapshot should ever have been created"
        );

        crate::rdf_store_service::evict_graph_store(&graph_dir).expect("evict graph store");
        drop(store);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn scheduler_real_mutation_triggers_debounced_flush_then_publishes() {
        let _serial = test_serial().lock().unwrap_or_else(|p| p.into_inner());
        let root = temp_dir("scheduler-debounced-real");
        let profile = root.join("profile");
        let durable = root.join("durable");
        let graph_id = root
            .file_name()
            .expect("temp dir name")
            .to_string_lossy()
            .into_owned();
        let graph_dir = profile.join("graphs").join(&graph_id);
        fs::create_dir_all(&graph_dir).expect("create graph dir");
        let store =
            crate::rdf_store_service::open_graph_store(&graph_dir).expect("open real graph store");

        let baseline = flush(&profile, &durable).expect("baseline flush");
        assert!(baseline.published);

        let debounce = Duration::from_secs(5);
        let max_rpo = Duration::from_secs(30);
        let t0 = Instant::now();
        let mut scheduler = DirtyFlushScheduler::new(debounce, max_rpo, current_write_epoch());

        // A real mutation through Garden's RDF boundary — bumps the same
        // global epoch the scheduler polls.
        crate::rdf_query_service::execute_sparql_update(
            &store,
            "INSERT DATA { <http://example.com/s> <http://example.com/p> <http://example.com/o> . }",
        )
        .expect("real quad write");
        let epoch_after_write = current_write_epoch();

        assert_eq!(
            scheduler.poll(t0, epoch_after_write),
            SchedulerAction::Wait,
            "must not flush the instant the write lands"
        );
        assert_eq!(
            scheduler.poll(t0 + Duration::from_secs(4), epoch_after_write),
            SchedulerAction::Wait,
            "still inside the debounce window"
        );
        assert_eq!(
            scheduler.poll(t0 + Duration::from_secs(5), epoch_after_write),
            SchedulerAction::FlushNow,
            "debounce window elapsed with no further writes"
        );

        let epoch_before = current_write_epoch();
        let outcome = flush(&profile, &durable).expect("debounced flush");
        scheduler.record_flush_attempt(epoch_before, FlushAttemptOutcome::Published);
        assert!(outcome.published, "the real mutation must publish");
        assert_eq!(outcome.stores_backed_up, 1);

        crate::rdf_store_service::evict_graph_store(&graph_dir).expect("evict graph store");
        drop(store);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn scheduler_max_rpo_ceiling_forces_flush_under_continuous_real_mutation() {
        let _serial = test_serial().lock().unwrap_or_else(|p| p.into_inner());
        let root = temp_dir("scheduler-max-rpo-real");
        let profile = root.join("profile");
        let durable = root.join("durable");
        let graph_id = root
            .file_name()
            .expect("temp dir name")
            .to_string_lossy()
            .into_owned();
        let graph_dir = profile.join("graphs").join(&graph_id);
        fs::create_dir_all(&graph_dir).expect("create graph dir");
        let store =
            crate::rdf_store_service::open_graph_store(&graph_dir).expect("open real graph store");

        let baseline = flush(&profile, &durable).expect("baseline flush");
        assert!(baseline.published);

        let debounce = Duration::from_secs(5);
        let max_rpo = Duration::from_secs(10);
        let t0 = Instant::now();
        let mut scheduler = DirtyFlushScheduler::new(debounce, max_rpo, current_write_epoch());

        // Real writes every 3s — closer together than the 5s debounce, so it
        // never naturally quiesces, exactly like the residual pattern
        // observed on a live canary cell (something re-dirties well inside
        // any fixed debounce window). Only the 10s max-RPO ceiling can force
        // a flush here.
        for (index, seconds) in [0u64, 3, 6, 9].into_iter().enumerate() {
            crate::rdf_query_service::execute_sparql_update(
                &store,
                &format!(
                    "INSERT DATA {{ <http://example.com/s> <http://example.com/p{index}> \"{index}\" . }}"
                ),
            )
            .expect("real quad write");
            let action = scheduler.poll(t0 + Duration::from_secs(seconds), current_write_epoch());
            assert_eq!(
                action,
                SchedulerAction::Wait,
                "continuous writes at {seconds}s must keep re-arming the debounce, not the RPO ceiling yet"
            );
        }
        // No further write after t=9; poll at t=11 — debounce deadline
        // (9+5=14) has not passed, but the max-RPO ceiling (first pending at
        // t0, +10s=10) has.
        let epoch_before_flush = current_write_epoch();
        assert_eq!(
            scheduler.poll(t0 + Duration::from_secs(11), epoch_before_flush),
            SchedulerAction::FlushAtRpoCeiling,
            "the max-RPO ceiling must force a flush despite continuous activity"
        );
        // Real wiring uses `flush_scheduled(.., force: true)` for exactly this
        // action — not a plain `flush()` — so drive it the same way here.
        let (outcome, attempt) =
            flush_scheduled(&profile, &durable, true).expect("rpo-forced flush");
        scheduler.record_flush_attempt(epoch_before_flush, attempt);
        assert_eq!(attempt, FlushAttemptOutcome::Published);
        assert!(outcome.published);
        assert_eq!(outcome.stores_backed_up, 1);

        crate::rdf_store_service::evict_graph_store(&graph_dir).expect("evict graph store");
        drop(store);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn kill_and_restore_after_debounced_not_yet_flushed_write_bounds_loss_to_rpo() {
        // THE fail-safe property: a crash before the max-RPO ceiling elapses
        // loses at most the write(s) since the last real flush (expected,
        // bounded, unchanged from before this scheduler existed) — and a
        // process that lives to the ceiling is GUARANTEED to have captured
        // them by then. Neither half of that bound is optional.
        let _serial = test_serial().lock().unwrap_or_else(|p| p.into_inner());
        let root = temp_dir("scheduler-kill-restore");
        let profile = root.join("profile");
        let durable = root.join("durable");
        let graph_id = root
            .file_name()
            .expect("temp dir name")
            .to_string_lossy()
            .into_owned();
        let graph_dir = profile.join("graphs").join(&graph_id);
        fs::create_dir_all(&graph_dir).expect("create graph dir");
        let store =
            crate::rdf_store_service::open_graph_store(&graph_dir).expect("open real graph store");

        let baseline = flush(&profile, &durable).expect("baseline flush");
        assert!(baseline.published);
        assert_eq!(store.len().expect("baseline quad count"), 0);

        let debounce = Duration::from_secs(5);
        let max_rpo = Duration::from_secs(30);
        let t0 = Instant::now();
        let mut scheduler = DirtyFlushScheduler::new(debounce, max_rpo, current_write_epoch());

        crate::rdf_query_service::execute_sparql_update(
            &store,
            "INSERT DATA { <http://example.com/s> <http://example.com/p> <http://example.com/o> . }",
        )
        .expect("real quad write");

        // "Crash" well before either deadline: the scheduler correctly says
        // Wait, and — because we never call flush() here — the write is
        // simply never made durable. No corruption, no false claim.
        //
        // Poll at `t0` itself (not `t0 + a delay`): `first_pending_at` is set
        // to whichever instant `poll` FIRST observes the pending write, so
        // this establishes it as exactly `t0`, matching the `t0 + max_rpo`
        // deadline used below (a real poller running every ~500ms would add
        // at most that much slack to the true bound — negligible next to a
        // 30s ceiling, so not modeled here).
        assert_eq!(
            scheduler.poll(t0, current_write_epoch()),
            SchedulerAction::Wait
        );

        // "Restart": hydrate into a fresh profile dir from the still-baseline
        // durable snapshot.
        let restored_profile = root.join("restored-profile");
        crate::rdf_store_service::evict_graph_store(&graph_dir).expect("evict graph store");
        drop(store);
        let hydrated = hydrate_detailed(&restored_profile, &durable).expect("hydrate after crash");
        assert_eq!(hydrated.mode, HydrateMode::Restored);
        let restored_graph_dir = restored_profile.join("graphs").join(&graph_id);
        let restored_store = crate::rdf_store_service::open_graph_store(&restored_graph_dir)
            .expect("open restored graph store");
        assert_eq!(
            restored_store.len().expect("restored quad count"),
            0,
            "a write inside the RPO window that never reached its deadline must not be durable"
        );
        crate::rdf_store_service::evict_graph_store(&restored_graph_dir)
            .expect("evict restored graph store");
        drop(restored_store);

        // Now demonstrate the OTHER half of the bound: had the process lived
        // to the max-RPO ceiling instead of crashing, the write WOULD have
        // been captured. Reopen the original store and carry the same
        // scenario forward to the ceiling.
        let store =
            crate::rdf_store_service::open_graph_store(&graph_dir).expect("reopen graph store");
        let epoch_before_flush = current_write_epoch();
        assert_eq!(
            scheduler.poll(t0 + Duration::from_secs(30), epoch_before_flush),
            SchedulerAction::FlushAtRpoCeiling,
            "the max-RPO ceiling must have elapsed by t+30s"
        );
        // Real wiring escalates to `flush_scheduled(.., force: true)` here —
        // matters most under a concurrent import (see the dedicated test
        // below); harmless to always use it at the RPO ceiling.
        let (forced, attempt) =
            flush_scheduled(&profile, &durable, true).expect("rpo-forced flush");
        scheduler.record_flush_attempt(epoch_before_flush, attempt);
        assert!(forced.published);

        let restored_profile_2 = root.join("restored-profile-2");
        crate::rdf_store_service::evict_graph_store(&graph_dir).expect("evict graph store");
        drop(store);
        hydrate_detailed(&restored_profile_2, &durable).expect("hydrate after rpo-forced flush");
        let restored_graph_dir_2 = restored_profile_2.join("graphs").join(&graph_id);
        let restored_store_2 = crate::rdf_store_service::open_graph_store(&restored_graph_dir_2)
            .expect("open restored graph store 2");
        assert_eq!(
            restored_store_2.len().expect("restored quad count 2"),
            1,
            "a write that survived to the RPO ceiling must be durable"
        );
        crate::rdf_store_service::evict_graph_store(&restored_graph_dir_2)
            .expect("evict restored graph store 2");
        drop(restored_store_2);

        let _ = fs::remove_dir_all(&root);
    }

    // -- 2026-07-18 durability re-refute: 3 confirmed false-CLEAN paths --
    //
    // A paranoid adversarial re-refute (independent model) found that landing
    // the dirty-driven scheduler UNMASKED three real data-loss paths that the
    // old unconditional-every-30s ticker had been (accidentally) papering
    // over. The tests below exercise each fix directly against the real
    // production code paths (no mocks): an Emporium write-gate acquisition
    // that previously left its store's mutation invisible to `flush`'s dirty
    // tracking, a heavy import silently breaking the max-RPO bound, and a
    // scheduler baseline captured too late to see a pre-boot write.

    /// Finding #1 (the big one — acknowledged data loss): multiple
    /// direct-Oxigraph writers (the Emporium memory applier, the
    /// applied-plan journal, the memory event log, simple-projection,
    /// chamber, the violation ledger, Geist's memory-care/archive
    /// materializer, the graph seed service) bump NEITHER
    /// `mark_rdf_store_written` NOR any writer lease (`HotWriteLease`/
    /// `ExclusiveLease`), so a real,
    /// acknowledged write through them was invisible to the durable flush.
    ///
    /// Rather than patch each site (more places to miss on the next new
    /// writer), `emporium::write_gate::acquire_write_gate` — the ONE gate
    /// every real Emporium mutation already funnels through by its own
    /// documented invariant ("taken by the MUTATING spine entry points
    /// only") — is now itself the marking choke point
    /// (`write_gate::WriteGateGuard::drop`). This test drives that exact
    /// primitive with a real direct-on-store write matching the shape of
    /// `emporium::memory_applier::run_memory_update` (SparqlEvaluator
    /// straight against `&Store`, no narrow hook called), with ZERO other
    /// lease/gate activity — the scenario the audit found silently lost data.
    #[test]
    fn write_gate_guard_drop_marks_graph_dirty_after_a_real_store_mutation() {
        let _serial = test_serial().lock().unwrap_or_else(|p| p.into_inner());
        let root = temp_dir("write-gate-marks");
        let profile = root.join("profile");
        let durable = root.join("durable");
        let graph_id = root
            .file_name()
            .expect("temp dir name")
            .to_string_lossy()
            .into_owned();
        let graph_dir = profile.join("graphs").join(&graph_id);
        fs::create_dir_all(&graph_dir).expect("create graph dir");
        let store =
            crate::rdf_store_service::open_graph_store(&graph_dir).expect("open real graph store");

        let baseline = flush(&profile, &durable).expect("baseline flush");
        assert!(baseline.published);

        crate::app_runtime::async_runtime::block_on(async {
            let gate = crate::emporium::write_gate::acquire_write_gate(&graph_id).await;
            // Exactly the shape of `run_memory_update`/`append_violations`/etc:
            // raw SparqlEvaluator execution against `&Store`, no narrow
            // `mark_rdf_store_written` call anywhere near it — this is the
            // precise pattern the audit found unmarked.
            oxigraph::sparql::SparqlEvaluator::new()
                .parse_update(
                    "INSERT DATA { <http://example.com/s> <http://example.com/p> <http://example.com/o> . }",
                )
                .expect("parse test update")
                .on_store(&store)
                .execute()
                .expect("execute real quad write, bypassing every narrow hook");
            drop(gate);
        });

        let second = flush_forced(&profile, &durable).expect("flush after write-gate mutation");
        assert!(
            second.published,
            "a real store mutation under the Emporium write-gate, with no other lease activity, \
             must still publish — this is exactly the data-loss scenario the audit found"
        );
        assert_eq!(second.stores_backed_up, 1);

        crate::rdf_store_service::evict_graph_store(&graph_dir).expect("evict graph store");
        drop(store);
        let _ = fs::remove_dir_all(&root);
    }

    /// Finding #2: during a heavy import, an ordinary (non-forced)
    /// `flush_scheduled` call unconditionally defers (`IMPORT_ACTIVE > 0`),
    /// so a concurrent, unrelated acknowledged write could previously sit
    /// unpublished for the WHOLE import (imports run up to ~90s) — silently
    /// breaking the max-RPO bound. `SchedulerAction::FlushAtRpoCeiling` must
    /// escalate to `force: true`, which bypasses the import-active defer
    /// (exactly like the existing SIGTERM/post-import forced flush already
    /// does), so the bound holds regardless.
    #[test]
    fn import_active_defers_ordinary_flush_but_rpo_ceiling_forces_it_through() {
        let _serial = test_serial().lock().unwrap_or_else(|p| p.into_inner());
        let root = temp_dir("import-active-rpo-force");
        let profile = root.join("profile");
        let durable = root.join("durable");
        let graph_id = root
            .file_name()
            .expect("temp dir name")
            .to_string_lossy()
            .into_owned();
        let graph_dir = profile.join("graphs").join(&graph_id);
        fs::create_dir_all(&graph_dir).expect("create graph dir");
        let store =
            crate::rdf_store_service::open_graph_store(&graph_dir).expect("open real graph store");

        let baseline = flush(&profile, &durable).expect("baseline flush");
        assert!(baseline.published);
        assert_eq!(store.len().expect("baseline quad count"), 0);

        // A real, unrelated write lands (e.g. a concurrent user edit on a
        // different document) while a heavy import is in progress elsewhere
        // in the same cell.
        crate::rdf_query_service::execute_sparql_update(
            &store,
            "INSERT DATA { <http://example.com/s> <http://example.com/p> <http://example.com/o> . }",
        )
        .expect("real quad write");

        // Drive IMPORT_ACTIVE directly, as the existing
        // `periodic_flush_defers_during_import_forced_proceeds` test does.
        // Decrement before any assert/panic so the flag can never leak into
        // other tests sharing this process.
        IMPORT_ACTIVE.fetch_add(1, Ordering::SeqCst);
        let ordinary = flush_scheduled(&profile, &durable, false);
        // Simulate the scheduler reaching the RPO ceiling while the import is
        // STILL active: this must still succeed (force bypasses the defer).
        let forced = flush_scheduled(&profile, &durable, true);
        IMPORT_ACTIVE.fetch_sub(1, Ordering::SeqCst);

        let (ordinary_outcome, ordinary_attempt) =
            ordinary.expect("ordinary flush_scheduled call succeeds (may defer)");
        assert!(
            !ordinary_outcome.published,
            "an ordinary flush must still defer while an import is active"
        );
        assert_eq!(ordinary_attempt, FlushAttemptOutcome::Deferred);

        let (forced_outcome, forced_attempt) =
            forced.expect("rpo-ceiling-forced flush_scheduled call");
        assert!(
            forced_outcome.published,
            "a real pending write must be captured at the RPO ceiling EVEN while an import is \
             active — the whole point of `SchedulerAction::FlushAtRpoCeiling` forcing"
        );
        assert_eq!(forced_attempt, FlushAttemptOutcome::Published);
        assert_eq!(forced_outcome.stores_backed_up, 1);

        // Prove it durably: kill-and-restore now shows the write, not just an
        // unfinished/torn import artifact.
        let restored_profile = root.join("restored-profile");
        crate::rdf_store_service::evict_graph_store(&graph_dir).expect("evict graph store");
        drop(store);
        hydrate_detailed(&restored_profile, &durable).expect("hydrate after rpo-forced flush");
        let restored_graph_dir = restored_profile.join("graphs").join(&graph_id);
        let restored_store = crate::rdf_store_service::open_graph_store(&restored_graph_dir)
            .expect("open restored graph store");
        assert_eq!(
            restored_store.len().expect("restored quad count"),
            1,
            "a write forced through at the RPO ceiling during an active import must be durable"
        );
        crate::rdf_store_service::evict_graph_store(&restored_graph_dir)
            .expect("evict restored graph store");
        drop(restored_store);

        let _ = fs::remove_dir_all(&root);
    }

    /// Finding #3 (startup race): the scheduler's "nothing pending yet"
    /// baseline must be captured BEFORE anything can accept a write —
    /// `examples/gardend.rs` now captures `boot_write_epoch` right after
    /// hydrate, before `garden_lib::headless::setup` starts the loopback
    /// server. This test proves the underlying mechanism the fix relies on:
    /// constructing the scheduler with a baseline captured before a write
    /// correctly sees that write as pending, while constructing it with a
    /// baseline captured after (the ORIGINAL bug — deep inside the `durable`
    /// async block, well after the server was already accepting requests)
    /// silently swallows it as already-clean, exactly the regression this
    /// guards against. (`examples/gardend.rs`'s own boot-ordering is a
    /// binary, not a `cargo test --lib` target — this test exercises the
    /// scheduler mechanism the fix depends on, by code inspection wired
    /// identically in `main`.)
    #[test]
    fn scheduler_baseline_captured_before_a_write_sees_it_pending_captured_after_does_not() {
        let epoch_before_boot_write = current_write_epoch();

        // A real write lands (recovery replaying an unflushed operation from
        // before a restart, or an ordinary request arriving the instant the
        // server opens up) before the scheduler is ever constructed.
        let root = temp_dir("scheduler-startup-race");
        let profile = root.join("profile");
        let graph_id = root
            .file_name()
            .expect("temp dir name")
            .to_string_lossy()
            .into_owned();
        let graph_dir = profile.join("graphs").join(&graph_id);
        fs::create_dir_all(&graph_dir).expect("create graph dir");
        let store =
            crate::rdf_store_service::open_graph_store(&graph_dir).expect("open real graph store");
        crate::rdf_query_service::execute_sparql_update(
            &store,
            "INSERT DATA { <http://example.com/s> <http://example.com/p> <http://example.com/o> . }",
        )
        .expect("real quad write before the scheduler exists");
        let epoch_after_boot_write = current_write_epoch();

        // CORRECT (the fix): baseline captured BEFORE the write.
        let t0 = Instant::now();
        let mut correct_scheduler = DirtyFlushScheduler::new(
            Duration::from_secs(5),
            Duration::from_secs(30),
            epoch_before_boot_write,
        );
        assert_eq!(
            correct_scheduler.poll(t0, epoch_after_boot_write),
            SchedulerAction::Wait,
            "not yet at the debounce deadline, but pending is registered"
        );
        assert_eq!(
            correct_scheduler.poll(t0 + Duration::from_secs(5), epoch_after_boot_write),
            SchedulerAction::FlushNow,
            "a baseline captured before the write correctly sees it as pending and flushes"
        );

        // THE BUG (what the original ordering did): baseline captured AFTER
        // the write, using whatever the epoch happens to be by construction
        // time — indistinguishable, from the scheduler's point of view, from
        // "nothing has happened yet."
        let mut buggy_scheduler = DirtyFlushScheduler::new(
            Duration::from_secs(5),
            Duration::from_secs(30),
            epoch_after_boot_write,
        );
        for seconds in [0, 5, 30, 300] {
            assert_eq!(
                buggy_scheduler.poll(t0 + Duration::from_secs(seconds), epoch_after_boot_write),
                SchedulerAction::Wait,
                "a baseline captured after the write silently swallows it — this is the exact \
                 regression the early-capture fix in gardend.rs prevents"
            );
        }

        crate::rdf_store_service::evict_graph_store(&graph_dir).expect("evict graph store");
        drop(store);
        let _ = fs::remove_dir_all(&root);
    }

    // -- 2026-07-18 durability re-refute #2: 2 confirmed reproductions --
    //
    // A second, paranoid adversarial re-refute confirmed COVERAGE,
    // DROP-TIMING, STARTUP-BASELINE, and REGRESSION all held from the first
    // response, but found the choke-point fix itself had introduced a new
    // false-CLEAN race (finding #1 below), plus an incompleteness in the
    // import-RPO escalation (finding #2). Both are reproduced directly
    // against the real production code below, no mocks.

    /// Finding #1 (the important one): reproduces the EXACT interleaving the
    /// re-refute described. Before this fix, `mark_rdf_store_written` (and
    /// its siblings) published the new global write-epoch via
    /// `next_store_write_epoch()` BEFORE acquiring `store_backup_epochs`'s
    /// lock to record the per-store `dirty` write — two separate
    /// publications with nothing forcing them to be observed together. A
    /// concurrent flush's `store_epoch_to_backup` (which also takes that
    /// lock) could acquire it in the gap: after the epoch had already moved,
    /// but before the per-store write landed, see the store as still clean,
    /// skip it, and — via `record_flush_attempt` — permanently advance the
    /// scheduler's watermark past an epoch that write's own store would
    /// never be checked against again.
    ///
    /// With the fix, the epoch allocation and the per-store write happen
    /// inside the SAME critical section a flush's own dirty check also locks
    /// — so this test proves directly that a concurrent flush attempt
    /// cannot even COMPLETE while a writer is paused between "epoch bumped"
    /// and "dirty map written" (it blocks on the identical mutex), and that
    /// the write survives once the writer's mark finishes.
    #[test]
    fn epoch_bump_and_dirty_write_are_atomic_a_concurrent_flush_cannot_observe_the_gap() {
        let _serial = test_serial().lock().unwrap_or_else(|p| p.into_inner());
        let root = temp_dir("epoch-dirty-atomicity");
        let profile = root.join("profile");
        let durable = root.join("durable");
        let graph_id = root
            .file_name()
            .expect("temp dir name")
            .to_string_lossy()
            .into_owned();
        let graph_dir = profile.join("graphs").join(&graph_id);
        fs::create_dir_all(&graph_dir).expect("create graph dir");
        let store =
            crate::rdf_store_service::open_graph_store(&graph_dir).expect("open real graph store");

        let baseline = flush(&profile, &durable).expect("baseline flush");
        assert!(baseline.published);

        let (reached_tx, reached_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        install_mark_dirty_pause_for_test(reached_tx, release_rx);

        let store_for_writer = Arc::clone(&store);
        let writer = std::thread::spawn(move || {
            crate::rdf_query_service::execute_sparql_update(
                &store_for_writer,
                "INSERT DATA { <http://example.com/s> <http://example.com/p> <http://example.com/o> . }",
            )
            .expect("real quad write, paused mid-mark by the test hook")
        });

        // Do not proceed until the writer is confirmed paused INSIDE the
        // mark call — the epoch is already bumped, the dirty-map lock is
        // still held, and the per-store write has not happened yet.
        reached_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("writer reached the mark-dirty pause point");

        let (finished_tx, finished_rx) = std::sync::mpsc::channel();
        let flush_profile = profile.clone();
        let flush_durable = durable.clone();
        let flusher = std::thread::spawn(move || {
            let result = flush(&flush_profile, &flush_durable);
            let _ = finished_tx.send(());
            result
        });

        assert!(
            finished_rx
                .recv_timeout(Duration::from_millis(200))
                .is_err(),
            "a concurrent flush completed while the writer's epoch bump had not yet been paired \
             with its dirty-map write — it must block on the same lock instead"
        );

        release_tx.send(()).expect("release the paused writer");
        writer.join().expect("writer thread");
        let second = flusher
            .join()
            .expect("flusher thread")
            .expect("flush succeeds once the writer's mark completes");

        assert!(
            second.published,
            "the real write must publish once the writer's atomic mark completes"
        );
        assert_eq!(second.stores_backed_up, 1);

        let current = fs::read_to_string(durable.join(CURRENT_FILE)).expect("read CURRENT");
        let backed_up_store = durable
            .join(current.trim())
            .join("graphs")
            .join(&graph_id)
            .join("store.oxigraph");
        let restored = Store::open_read_only(&backed_up_store).expect("open real store backup");
        assert_eq!(
            restored.len().expect("backup quad count"),
            1,
            "the write paused mid-mark must not be lost from the published snapshot"
        );
        drop(restored);

        crate::rdf_store_service::evict_graph_store(&graph_dir).expect("evict graph store");
        drop(store);
        let _ = fs::remove_dir_all(&root);
    }

    /// Finding #2(b): a forced flush reached via
    /// `SchedulerAction::FlushAtRpoCeiling` while `IMPORT_ACTIVE > 0` must
    /// not publish a snapshot where the plain-file walk captures a NEWER
    /// `write_guard()`-gated write (standing in here for the import's own
    /// operation-completion-ledger append, which uses this exact mechanism
    /// and, by construction, always happens strictly after every earlier
    /// step — including the archive-wide RDF load — already succeeded) than
    /// what the Oxigraph checkpoint reflects. That specific "torn" outcome
    /// would leave a completed-looking operation with stale/missing RDF
    /// PERMANENTLY: the operation-completion ledger's Tier-B replay guard
    /// trusts a present entry and never re-touches the RDF on retry.
    ///
    /// Reproduced directly: a real `write_guard()`-gated write is already
    /// held open (mid-write) when the forced-during-import flush starts.
    /// Proves (i) the flush cannot even begin enumerating Oxigraph stores
    /// until that write releases (the `gate_first` ordering), so its RDF
    /// checkpoint and the plain-file walk can only ever see BOTH the RDF
    /// write and the "ledger" file together, never one without the other,
    /// and (ii) the result durably survives a kill-and-restore with both
    /// present, never torn.
    #[test]
    fn forced_flush_during_import_never_composes_a_torn_ledger_and_stale_rdf_combination() {
        struct ImportActiveTestGuard;
        impl Drop for ImportActiveTestGuard {
            fn drop(&mut self) {
                // RAII, not a manual pre-assert decrement: this test has
                // many intermediate synchronization assertions between the
                // increment and the natural decrement point, any of which
                // could panic. IMPORT_ACTIVE is a process-global shared with
                // every other test in this binary, so it must never leak
                // incremented, panic or not.
                IMPORT_ACTIVE.fetch_sub(1, Ordering::SeqCst);
            }
        }

        let _serial = test_serial().lock().unwrap_or_else(|p| p.into_inner());
        let root = temp_dir("import-torn-race");
        let profile = root.join("profile");
        let durable = root.join("durable");
        let graph_id = root
            .file_name()
            .expect("temp dir name")
            .to_string_lossy()
            .into_owned();
        let graph_dir = profile.join("graphs").join(&graph_id);
        fs::create_dir_all(&graph_dir).expect("create graph dir");
        let store =
            crate::rdf_store_service::open_graph_store(&graph_dir).expect("open real graph store");

        let baseline = flush(&profile, &durable).expect("baseline flush");
        assert!(baseline.published);
        assert_eq!(store.len().expect("baseline quad count"), 0);

        IMPORT_ACTIVE.fetch_add(1, Ordering::SeqCst);
        let _import_active_guard = ImportActiveTestGuard;

        // The "ledger append" write_guard() is already held open, mid-write,
        // before the forced flush even starts trying to acquire anything.
        let (held_tx, held_rx) = std::sync::mpsc::channel();
        let (proceed_tx, proceed_rx) = std::sync::mpsc::channel();
        let ledger_path = graph_dir.join("import-ledger-marker.json");
        let store_for_writer = Arc::clone(&store);
        let ledger_path_for_writer = ledger_path.clone();
        let writer = std::thread::spawn(move || {
            let _guard = write_guard_for_test();
            held_tx.send(()).expect("signal guard held");
            proceed_rx.recv().expect("await release signal");
            // Mirrors the import's own program order: the archive-wide RDF
            // load happens-before the completion-ledger append, both under
            // this same write_guard()-gated critical section in spirit.
            crate::rdf_query_service::execute_sparql_update(
                &store_for_writer,
                "INSERT DATA { <http://example.com/s> <http://example.com/p> <http://example.com/o> . }",
            )
            .expect("real RDF write standing in for the archive-wide load");
            write(&ledger_path_for_writer, b"{\"done\":true}");
            // `_guard` drops here, releasing write_guard() — mirrors the
            // ledger append's own guard release.
        });
        held_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("writer holds the write-guard");

        let (reached_tx, reached_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        install_rdf_store_enumeration_pause_for_test(reached_tx, release_rx);
        let flush_profile = profile.clone();
        let flush_durable = durable.clone();
        let flusher =
            std::thread::spawn(move || flush_scheduled(&flush_profile, &flush_durable, true));

        // While the writer still holds the guard, the forced (gate-first)
        // flush must not even reach RDF-store enumeration yet — it is
        // blocked acquiring the SAME gate the writer holds.
        assert!(
            reached_rx.recv_timeout(Duration::from_millis(200)).is_err(),
            "a forced flush during an active import began its RDF checkpoint before the \
             write-guard()-gated write ahead of it had released — the exact ordering that \
             allows a torn ledger+RDF snapshot"
        );
        assert!(
            !ledger_path.is_file(),
            "the ledger-standin file must not exist yet at this point in the test"
        );

        // Let the writer proceed: RDF write, then the "ledger" file, then
        // guard release.
        proceed_tx.send(()).expect("let the writer proceed");
        writer.join().expect("writer thread");

        // Now the flush's gate acquisition succeeds and it reaches
        // enumeration.
        reached_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("flush reached RDF-store enumeration after the writer released");
        release_tx
            .send(())
            .expect("release the flush's enumeration pause");

        let flush_result = flusher.join();
        drop(_import_active_guard); // decrement before unwrapping either result
        let (outcome, attempt) = flush_result
            .expect("flusher thread")
            .expect("forced flush during import succeeds");

        assert_eq!(attempt, FlushAttemptOutcome::Published);
        assert!(outcome.published);

        let current = fs::read_to_string(durable.join(CURRENT_FILE)).expect("read CURRENT");
        let snap_dir = durable.join(current.trim());
        let ledger_in_snapshot = snap_dir
            .join("graphs")
            .join(&graph_id)
            .join("import-ledger-marker.json")
            .is_file();
        let restored = Store::open_read_only(
            &snap_dir
                .join("graphs")
                .join(&graph_id)
                .join("store.oxigraph"),
        )
        .expect("open real store backup");
        let rdf_in_snapshot = restored.len().expect("backup quad count") > 0;
        drop(restored);

        assert!(
            ledger_in_snapshot && rdf_in_snapshot,
            "both the RDF write and the ledger-standin file completed before the flush's \
             gate-first acquisition succeeded, so both must be captured together \
             (ledger_in_snapshot={ledger_in_snapshot}, rdf_in_snapshot={rdf_in_snapshot})"
        );

        // Durability: kill-and-restore shows the SAME consistent pair, never
        // a torn combination.
        let restored_profile = root.join("restored-profile");
        crate::rdf_store_service::evict_graph_store(&graph_dir).expect("evict graph store");
        drop(store);
        hydrate_detailed(&restored_profile, &durable)
            .expect("hydrate after forced-during-import flush");
        let restored_graph_dir = restored_profile.join("graphs").join(&graph_id);
        assert!(
            restored_graph_dir
                .join("import-ledger-marker.json")
                .is_file(),
            "restored profile is missing the ledger-standin file"
        );
        let restored_store_2 = crate::rdf_store_service::open_graph_store(&restored_graph_dir)
            .expect("open restored graph store");
        assert_eq!(
            restored_store_2.len().expect("restored quad count"),
            1,
            "restored profile's RDF must match the ledger-standin's presence — never torn"
        );
        crate::rdf_store_service::evict_graph_store(&restored_graph_dir)
            .expect("evict restored graph store");
        drop(restored_store_2);

        let _ = fs::remove_dir_all(&root);
    }

    /// 2026-07-18 re-refute #3 (the TOCTOU on the re-refute #2 fix): the
    /// PREVIOUS version of the torn-import fix computed `gate_first = force
    /// && IMPORT_ACTIVE.load(...) > 0` as a ONE-SHOT sample, before
    /// `flush_serial` was even acquired. A concurrent operation (an import,
    /// or any other write) that STARTS after that sample but before the RDF
    /// checkpoint would leave `gate_first` wrongly `false` — landing on the
    /// exact torn-composition-vulnerable ordering the fix existed to close,
    /// now triggered by a freshly-starting write instead of an
    /// already-active one. The actual fix removes the sample entirely: the
    /// flush gate is now acquired before the RDF checkpoint
    /// UNCONDITIONALLY, for every attempt, so there is no decision left to
    /// race against.
    ///
    /// This test proves exactly that: it reproduces the identical
    /// interleaving as `forced_flush_during_import_never_composes_a_torn_ledger_and_stale_rdf_combination`
    /// (a real `write_guard()`-gated write racing a real forced flush) but
    /// deliberately leaves `IMPORT_ACTIVE` at 0 for the ENTIRE test — the
    /// exact condition under which the old one-shot sample would have
    /// computed `gate_first = false` and been vulnerable. The flush must
    /// still compose only a consistent (RDF-and-ledger-together-or-neither)
    /// snapshot, and that snapshot must survive a kill-and-restore.
    #[test]
    fn forced_flush_closes_the_import_active_toctou_even_when_the_flag_was_never_set() {
        let _serial = test_serial().lock().unwrap_or_else(|p| p.into_inner());
        assert_eq!(
            IMPORT_ACTIVE.load(Ordering::SeqCst),
            0,
            "precondition: no other test in this process left IMPORT_ACTIVE set"
        );
        let root = temp_dir("import-toctou-flag-never-set");
        let profile = root.join("profile");
        let durable = root.join("durable");
        let graph_id = root
            .file_name()
            .expect("temp dir name")
            .to_string_lossy()
            .into_owned();
        let graph_dir = profile.join("graphs").join(&graph_id);
        fs::create_dir_all(&graph_dir).expect("create graph dir");
        let store =
            crate::rdf_store_service::open_graph_store(&graph_dir).expect("open real graph store");

        let baseline = flush(&profile, &durable).expect("baseline flush");
        assert!(baseline.published);
        assert_eq!(store.len().expect("baseline quad count"), 0);

        // A real write_guard()-gated write is already held open, mid-write
        // — mirroring a concurrent operation whose completion-ledger append
        // has not yet landed — but `IMPORT_ACTIVE` is NEVER touched anywhere
        // in this test. Under the old one-shot-sample design, a forced flush
        // starting right now would have sampled `IMPORT_ACTIVE == 0` and
        // committed to the vulnerable ordering. The fix must not care.
        let (held_tx, held_rx) = std::sync::mpsc::channel();
        let (proceed_tx, proceed_rx) = std::sync::mpsc::channel();
        let ledger_path = graph_dir.join("concurrent-write-ledger-marker.json");
        let store_for_writer = Arc::clone(&store);
        let ledger_path_for_writer = ledger_path.clone();
        let writer = std::thread::spawn(move || {
            let _guard = write_guard_for_test();
            held_tx.send(()).expect("signal guard held");
            proceed_rx.recv().expect("await release signal");
            crate::rdf_query_service::execute_sparql_update(
                &store_for_writer,
                "INSERT DATA { <http://example.com/s> <http://example.com/p> <http://example.com/o> . }",
            )
            .expect("real RDF write");
            write(&ledger_path_for_writer, b"{\"done\":true}");
        });
        held_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("writer holds the write-guard");

        let (reached_tx, reached_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        install_rdf_store_enumeration_pause_for_test(reached_tx, release_rx);
        let flush_profile = profile.clone();
        let flush_durable = durable.clone();
        // force=true with IMPORT_ACTIVE == 0 throughout: exactly the
        // scenario the old one-shot sample would have gotten wrong.
        let flusher =
            std::thread::spawn(move || flush_scheduled(&flush_profile, &flush_durable, true));

        assert!(
            reached_rx.recv_timeout(Duration::from_millis(200)).is_err(),
            "a forced flush with IMPORT_ACTIVE == 0 the whole time still began its RDF checkpoint \
             before a concurrent write-guard()-gated write ahead of it had released — the fix must \
             not depend on IMPORT_ACTIVE's value at all"
        );
        assert!(
            !ledger_path.is_file(),
            "the ledger-standin file must not exist yet at this point in the test"
        );

        proceed_tx.send(()).expect("let the writer proceed");
        writer.join().expect("writer thread");

        reached_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("flush reached RDF-store enumeration after the writer released");
        release_tx
            .send(())
            .expect("release the flush's enumeration pause");

        let (outcome, attempt) = flusher
            .join()
            .expect("flusher thread")
            .expect("forced flush succeeds");

        assert_eq!(attempt, FlushAttemptOutcome::Published);
        assert!(outcome.published);

        let current = fs::read_to_string(durable.join(CURRENT_FILE)).expect("read CURRENT");
        let snap_dir = durable.join(current.trim());
        let ledger_in_snapshot = snap_dir
            .join("graphs")
            .join(&graph_id)
            .join("concurrent-write-ledger-marker.json")
            .is_file();
        let restored = Store::open_read_only(
            &snap_dir
                .join("graphs")
                .join(&graph_id)
                .join("store.oxigraph"),
        )
        .expect("open real store backup");
        let rdf_in_snapshot = restored.len().expect("backup quad count") > 0;
        drop(restored);

        assert!(
            ledger_in_snapshot && rdf_in_snapshot,
            "both the RDF write and the ledger-standin file completed before the flush's gate \
             acquisition succeeded, so both must be captured together — with IMPORT_ACTIVE never \
             set, this proves the fix does not depend on that flag \
             (ledger_in_snapshot={ledger_in_snapshot}, rdf_in_snapshot={rdf_in_snapshot})"
        );

        let restored_profile = root.join("restored-profile");
        crate::rdf_store_service::evict_graph_store(&graph_dir).expect("evict graph store");
        drop(store);
        hydrate_detailed(&restored_profile, &durable).expect("hydrate after forced flush");
        let restored_graph_dir = restored_profile.join("graphs").join(&graph_id);
        assert!(
            restored_graph_dir
                .join("concurrent-write-ledger-marker.json")
                .is_file(),
            "restored profile is missing the ledger-standin file"
        );
        let restored_store_2 = crate::rdf_store_service::open_graph_store(&restored_graph_dir)
            .expect("open restored graph store");
        assert_eq!(
            restored_store_2.len().expect("restored quad count"),
            1,
            "restored profile's RDF must match the ledger-standin's presence — never torn"
        );
        crate::rdf_store_service::evict_graph_store(&restored_graph_dir)
            .expect("evict restored graph store");
        drop(restored_store_2);

        assert_eq!(
            IMPORT_ACTIVE.load(Ordering::SeqCst),
            0,
            "postcondition: this test never touched IMPORT_ACTIVE"
        );
        let _ = fs::remove_dir_all(&root);
    }

    // -----------------------------------------------------------------
    // U8 spec §3.6 — boot-time repair (`boot_repair`)
    // -----------------------------------------------------------------

    fn set_current(durable: &Path, seq: u64) {
        write(
            &durable.join(CURRENT_FILE),
            format!("snap-{seq:06}").as_bytes(),
        );
    }

    fn make_snap_dir(durable: &Path, seq: u64) {
        fs::create_dir_all(durable.join(format!("snap-{seq:06}"))).unwrap();
    }

    fn read_current_raw(durable: &Path) -> Option<String> {
        fs::read_to_string(durable.join(CURRENT_FILE))
            .ok()
            .map(|s| s.trim().to_string())
    }

    #[test]
    fn boot_repair_skips_entirely_when_last_snap_is_absent_from_env() {
        let durable = temp_dir("repair-skip");
        make_snap_dir(&durable, 3);
        set_current(&durable, 3);

        let outcome = boot_repair(&durable, None, None, false).expect("boot_repair");
        assert_eq!(outcome, BootRepairOutcome::SkippedNoLastSnap);
        // Never guessed: CURRENT and the snapshot tree are untouched.
        assert_eq!(read_current_raw(&durable), Some("snap-000003".to_string()));
        assert!(durable.join("snap-000003").is_dir());

        let _ = fs::remove_dir_all(&durable);
    }

    #[test]
    fn boot_repair_is_clean_when_current_matches_last_snap() {
        let durable = temp_dir("repair-clean");
        make_snap_dir(&durable, 2);
        set_current(&durable, 2);

        let outcome = boot_repair(&durable, Some(2), None, false).expect("boot_repair");
        assert_eq!(outcome, BootRepairOutcome::Clean);
        assert_eq!(read_current_raw(&durable), Some("snap-000002".to_string()));

        let _ = fs::remove_dir_all(&durable);
    }

    #[test]
    fn boot_repair_is_clean_on_a_first_boot_with_no_current_at_all() {
        // The first-claim seeding case (spec §1.2's `:seed`, proven at U8-2's
        // G9): a freshly claimed graph with no prior CURRENT at all seeds
        // `last_snap = 0`, and `S` (no CURRENT file) also reads as 0 here —
        // never a quarantine misfire on first rollout (Z5).
        let durable = temp_dir("repair-first-boot");
        fs::create_dir_all(&durable).unwrap();

        let outcome = boot_repair(&durable, Some(0), None, false).expect("boot_repair");
        assert_eq!(outcome, BootRepairOutcome::Clean);
        assert_eq!(read_current_raw(&durable), None, "no CURRENT was written");

        let _ = fs::remove_dir_all(&durable);
    }

    #[test]
    fn boot_repair_accepts_pending_when_current_equals_pending_and_touches_nothing() {
        let durable = temp_dir("repair-accept");
        make_snap_dir(&durable, 2);
        make_snap_dir(&durable, 3);
        set_current(&durable, 3);

        let outcome = boot_repair(&durable, Some(2), Some(3), false).expect("boot_repair");
        assert_eq!(outcome, BootRepairOutcome::AcceptPending { seq: 3 });
        // Table: "proceed" — no filesystem mutation at all, the caller only
        // sends publish(commit, 3) after boot.
        assert_eq!(read_current_raw(&durable), Some("snap-000003".to_string()));
        assert!(durable.join("snap-000002").is_dir());
        assert!(durable.join("snap-000003").is_dir());

        let _ = fs::remove_dir_all(&durable);
    }

    #[test]
    fn boot_repair_aborts_pending_and_prunes_the_matching_building_orphan_when_current_is_behind() {
        let durable = temp_dir("repair-abort");
        make_snap_dir(&durable, 2);
        set_current(&durable, 2);
        // The interrupted flush's build dir for the escaped intent (seq 3),
        // stamped with an epoch and pid per this task's naming change.
        fs::create_dir_all(durable.join(".building-000003-e0000000007-4242-0")).unwrap();
        // A dir for a DIFFERENT seq must survive the prune.
        fs::create_dir_all(durable.join(".building-000009-e0000000007-4242-1")).unwrap();

        let outcome = boot_repair(&durable, Some(2), Some(3), false).expect("boot_repair");
        assert_eq!(outcome, BootRepairOutcome::AbortedPending { seq: 3 });
        assert_eq!(read_current_raw(&durable), Some("snap-000002".to_string()));
        assert!(!durable.join(".building-000003-e0000000007-4242-0").exists());
        assert!(
            durable.join(".building-000009-e0000000007-4242-1").exists(),
            "pruning must only remove the orphan matching the escaped seq"
        );

        let _ = fs::remove_dir_all(&durable);
    }

    #[test]
    fn boot_repair_quarantines_an_escaped_publish_and_restores_current() {
        let durable = temp_dir("repair-quarantine");
        make_snap_dir(&durable, 2);
        make_snap_dir(&durable, 3);
        make_snap_dir(&durable, 4);
        make_snap_dir(&durable, 5);
        set_current(&durable, 5);

        let outcome = boot_repair(&durable, Some(2), None, false).expect("boot_repair");
        let orphan_dir_name = match &outcome {
            BootRepairOutcome::Quarantined {
                restored_to,
                orphaned,
                orphan_dir_name,
            } => {
                assert_eq!(*restored_to, 2);
                assert_eq!(orphaned, &vec![3, 4, 5]);
                orphan_dir_name.clone()
            }
            other => panic!("expected Quarantined, got {other:?}"),
        };
        assert!(orphan_dir_name.starts_with(".orphan-"));
        assert_eq!(read_current_raw(&durable), Some("snap-000002".to_string()));
        assert!(durable.join("snap-000002").is_dir(), "L survives in place");
        for seq in [3, 4, 5] {
            let name = format!("snap-{seq:06}");
            assert!(
                !durable.join(&name).exists(),
                "escaped snapshot {name} must be gone from the top level"
            );
            assert!(
                durable.join(&orphan_dir_name).join(&name).is_dir(),
                "escaped snapshot {name} must be quarantined, not deleted"
            );
        }

        let _ = fs::remove_dir_all(&durable);
    }

    #[test]
    fn boot_repair_quarantine_refuses_boot_loudly_when_last_snap_is_missing_on_disk() {
        let durable = temp_dir("repair-quarantine-refuse");
        make_snap_dir(&durable, 5);
        set_current(&durable, 5);
        // snap-000002 (the claimed last_snap) was never actually written to
        // this disk — an anomaly serious enough to refuse rather than guess.

        let error = boot_repair(&durable, Some(2), None, false)
            .expect_err("must refuse to boot rather than apply a repair with no L on disk");
        assert!(error.to_string().contains("refusing to boot"));
        assert!(error.requires_snapshot_authority_repair());
        // Nothing was touched: refusal happens before any mutation.
        assert_eq!(read_current_raw(&durable), Some("snap-000005".to_string()));
        assert!(durable.join("snap-000005").is_dir());

        let _ = fs::remove_dir_all(&durable);
    }

    #[test]
    fn boot_repair_restores_current_when_a_zombie_rewound_it_below_last_snap() {
        let durable = temp_dir("repair-restore");
        make_snap_dir(&durable, 2);
        make_snap_dir(&durable, 5);
        set_current(&durable, 2); // rewound by a late zombie publish_current

        let outcome = boot_repair(&durable, Some(5), None, false).expect("boot_repair");
        assert_eq!(outcome, BootRepairOutcome::Restored { restored_to: 5 });
        assert_eq!(read_current_raw(&durable), Some("snap-000005".to_string()));

        let _ = fs::remove_dir_all(&durable);
    }

    #[test]
    fn boot_repair_restore_refuses_boot_when_last_snap_is_missing_on_disk() {
        let durable = temp_dir("repair-restore-refuse");
        make_snap_dir(&durable, 1);
        set_current(&durable, 1);

        let error = boot_repair(&durable, Some(5), None, false)
            .expect_err("must refuse rather than restore to a snapshot that does not exist");
        assert!(error.to_string().contains("refusing to boot"));
        assert!(error.requires_snapshot_authority_repair());
        assert_eq!(read_current_raw(&durable), Some("snap-000001".to_string()));

        let _ = fs::remove_dir_all(&durable);
    }

    #[test]
    fn boot_repair_io_failure_remains_retryable() {
        let durable = temp_dir("repair-io-retryable");
        fs::create_dir_all(&durable).unwrap();
        let not_a_directory = durable.join("not-a-directory");
        fs::write(&not_a_directory, b"x").unwrap();

        let error = boot_repair(&not_a_directory, Some(5), None, false)
            .expect_err("reading CURRENT through a regular file must fail");
        assert!(!error.requires_snapshot_authority_repair());

        let _ = fs::remove_dir_all(&durable);
    }

    #[test]
    fn boot_repair_dry_run_computes_the_decision_but_never_touches_the_filesystem() {
        let durable = temp_dir("repair-dry-run");
        make_snap_dir(&durable, 2);
        make_snap_dir(&durable, 3);
        set_current(&durable, 3);

        // Same inputs as the quarantine test above, but dry_run = true
        // (observe mode): compute and testify the same decision, mutate
        // nothing — spec §3.2, "observe mode testifies what it would have
        // done".
        let outcome = boot_repair(&durable, Some(2), None, true).expect("boot_repair dry run");
        match &outcome {
            BootRepairOutcome::Quarantined {
                restored_to,
                orphaned,
                ..
            } => {
                assert_eq!(*restored_to, 2);
                assert_eq!(orphaned, &vec![3]);
            }
            other => panic!("expected Quarantined, got {other:?}"),
        }
        // Nothing on disk moved: CURRENT is unchanged and snap-000003 is
        // still at the top level, not quarantined.
        assert_eq!(read_current_raw(&durable), Some("snap-000003".to_string()));
        assert!(durable.join("snap-000003").is_dir());
        assert!(
            fs::read_dir(&durable)
                .unwrap()
                .filter_map(|entry| entry.ok())
                .all(|entry| !entry.file_name().to_string_lossy().starts_with(".orphan-")),
            "dry_run must not create an orphan dir"
        );

        let _ = fs::remove_dir_all(&durable);
    }

    #[test]
    fn prune_snapshots_preserves_orphan_quarantine_dirs() {
        // Asserted in `boot_repair`'s own doc comment: `.orphan-*` never
        // matches SNAP_PREFIX or BUILDING_PREFIX, so `prune_snapshots`
        // (which only ever removes those two families) must never touch it.
        let durable = temp_dir("repair-prune-preserves-orphan");
        make_snap_dir(&durable, 2);
        make_snap_dir(&durable, 3);
        make_snap_dir(&durable, 4);
        make_snap_dir(&durable, 5);
        fs::create_dir_all(durable.join(".orphan-01ARZ3NDEKTSV4RRFFQ69G5FAV")).unwrap();

        prune_snapshots(
            &durable,
            "snap-000005",
            Some("snap-000004"),
            Some("snap-000002"),
        )
        .expect("prune");

        assert!(
            durable.join(".orphan-01ARZ3NDEKTSV4RRFFQ69G5FAV").is_dir(),
            "prune_snapshots must never remove an .orphan-* quarantine dir"
        );
        assert!(
            durable.join("snap-000002").is_dir(),
            "prune_snapshots must retain the lease authority high-water mark"
        );
        assert!(
            !durable.join("snap-000003").exists(),
            "unprotected historical snapshots should still be pruned"
        );

        let _ = fs::remove_dir_all(&durable);
    }
}
