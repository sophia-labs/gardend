//! Per-graph authority lease for headless CRDT persistence and lifecycle.
//!
//! Every queue enqueue may spawn a drainer, websocket updates arrive outside
//! that queue, and graph deletion is a separate lifecycle surface. A lease is
//! therefore acquired at those roots and held from before mutation through the
//! hot update-v1 write and/or cold projection tail. Inner persistence helpers
//! must not reacquire it; they use their narrower room projection gates.
//!
//! Scope includes queued/WebSocket/lifecycle roots and direct desktop Tauri
//! mutation commands. Public synchronous commands acquire the blocking form of
//! the same lease and delegate to non-reentrant service helpers; queued handlers
//! that already own the async lease call those helpers directly.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
#[cfg(feature = "desktop")]
use tauri::Manager;
use tokio::sync::Notify;

use crate::app_runtime::AppHandle;
use crate::cell_durability_trace::{
    known_crdt_kind, origin_from_location, tracing_enabled, DirtyOrigin, DirtyReason, KnownCrdtKind,
};

#[derive(Default)]
pub(crate) struct GraphPersistenceCoordinator {
    // Strong entries retain lifecycle generations even while a graph is idle,
    // so an old queued flush cannot become admissible after delete/recreate.
    gates: Mutex<HashMap<String, Arc<GraphGate>>>,
}

struct GraphGate {
    lifecycle_writer: AtomicBool,
    lifecycle_readers: AtomicUsize,
    lifecycle_async_notify: Notify,
    lifecycle_blocking_wait: Mutex<()>,
    lifecycle_blocking_notify: Condvar,
    hot_write_held: AtomicBool,
    hot_write_async_notify: Notify,
    hot_write_blocking_wait: Mutex<()>,
    hot_write_blocking_notify: Condvar,
    generation: std::sync::atomic::AtomicU64,
}

pub(crate) struct SharedLease {
    gate: Arc<GraphGate>,
}

struct DirtyLeaseState {
    graph_id: String,
    rdf_read_only: std::cell::Cell<bool>,
    origin: DirtyOrigin,
    crdt_kind: std::cell::Cell<Option<KnownCrdtKind>>,
}

pub(crate) struct HotWriteLease {
    gate: Arc<GraphGate>,
    dirty: DirtyLeaseState,
    _shared: SharedLease,
}

pub(crate) struct ExclusiveLease {
    gate: Arc<GraphGate>,
    dirty: DirtyLeaseState,
}

struct LifecycleWriterIntent {
    gate: Arc<GraphGate>,
    armed: bool,
}

impl LifecycleWriterIntent {
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for LifecycleWriterIntent {
    fn drop(&mut self) {
        if self.armed {
            release_lifecycle_writer(&self.gate);
        }
    }
}

impl DirtyLeaseState {
    // Defaults to "might have written" (false = not read-only), the safe
    // direction: Drop marks the graph's RDF stores dirty unless a caller
    // has explicitly proven otherwise via `declare_rdf_read_only`. Cell
    // (not Atomic) is deliberate — the lease is used from one task at a
    // time, never shared across threads concurrently.
    fn new(graph_id: &str, origin: DirtyOrigin) -> Self {
        Self {
            graph_id: graph_id.to_string(),
            rdf_read_only: std::cell::Cell::new(false),
            origin,
            crdt_kind: std::cell::Cell::new(None),
        }
    }

    fn mark_if_needed(&self) {
        if !self.rdf_read_only.get() {
            crate::cell_durability::mark_graph_rdf_stores_written_with_context(
                &self.graph_id,
                DirtyReason::GraphLeaseFallback,
                self.origin,
                self.crdt_kind.get(),
            );
        }
    }
}

macro_rules! impl_writing_lease {
    ($lease:ty) => {
        impl $lease {
            /// Declare that everything executed under this lease's scope is
            /// provably read-only with respect to the graph's Oxigraph RDF
            /// stores, so `Drop` should not mark them dirty.
            ///
            /// SAFETY CONTRACT: call this ONLY at a site verified by inspection
            /// to never reach a Store-mutating code path — no SPARQL update, no
            /// RDF/dataset load, no Emporium/materializer write, directly or
            /// transitively, for the entire remaining lifetime of the lease.
            /// The default (never calling this) is the conservative, fail-safe
            /// assumption that Drop honors by marking dirty; a false-DIRTY (an
            /// extra backup) is harmless, but a false-CLEAN from a mistaken call
            /// here would silently drop durable data. Keep the set of call sites
            /// small and each one provably correct — do not thread this through
            /// generic/shared helpers whose future callers you cannot audit.
            #[allow(dead_code)]
            pub(crate) fn declare_rdf_read_only(&self) {
                self.dirty.rdf_read_only.set(true);
            }

            /// Suppress the lease's conservative blanket dirty mark when every
            /// possible RDF write in this scope performs its own precise
            /// durability mark.
            ///
            /// This is distinct from [`Self::declare_rdf_read_only`]: a
            /// guarded path may write, but only through helpers that call
            /// `mark_rdf_store_written` themselves. Call only after the whole
            /// scope succeeded. An error or panic before this declaration
            /// retains the default fail-toward-dirty behavior.
            ///
            /// Audited uses are deliberately narrow:
            ///
            /// - external query: `ensure_graph_store_seeded` is the only
            ///   possible writer and marks the store after completed
            ///   projection work;
            /// - successful restore-point capture: ordinary bundle/history
            ///   writes do not touch RDF, while the rare headless ghost
            ///   self-heal materializer marks its own store write.
            ///
            /// This avoids needless durable-plane flushes without hiding
            /// seed/heal writes.
            #[allow(dead_code)]
            pub(crate) fn declare_rdf_writes_self_tracked(&self) {
                self.dirty.rdf_read_only.set(true);
            }

            /// Attach only a closed, allowlisted operation kind to diagnostics.
            /// An arbitrary caller-supplied string is deliberately discarded.
            #[allow(dead_code)]
            pub(crate) fn set_crdt_kind(&self, kind: &str) {
                if tracing_enabled() {
                    self.dirty.crdt_kind.set(known_crdt_kind(kind));
                }
            }
        }
    };
}

impl_writing_lease!(HotWriteLease);
impl_writing_lease!(ExclusiveLease);

impl GraphPersistenceCoordinator {
    fn gate(&self, graph_id: &str) -> Result<Arc<GraphGate>, String> {
        let mut gates = self
            .gates
            .lock()
            .map_err(|_| "graph persistence coordinator lock poisoned".to_string())?;
        if let Some(gate) = gates.get(graph_id) {
            return Ok(gate.clone());
        }
        let gate = Arc::new(GraphGate {
            lifecycle_writer: AtomicBool::new(false),
            lifecycle_readers: AtomicUsize::new(0),
            lifecycle_async_notify: Notify::new(),
            lifecycle_blocking_wait: Mutex::new(()),
            lifecycle_blocking_notify: Condvar::new(),
            hot_write_held: AtomicBool::new(false),
            hot_write_async_notify: Notify::new(),
            hot_write_blocking_wait: Mutex::new(()),
            hot_write_blocking_notify: Condvar::new(),
            generation: std::sync::atomic::AtomicU64::new(0),
        });
        gates.insert(graph_id.to_string(), gate.clone());
        Ok(gate)
    }

    pub(crate) async fn acquire_lifecycle_shared(
        &self,
        graph_id: &str,
    ) -> Result<SharedLease, String> {
        let gate = self.gate(graph_id)?;
        loop {
            // Register before checking the atomics to avoid a release between
            // the check and awaiting the notification.
            let notified = gate.lifecycle_async_notify.notified();
            if !gate.lifecycle_writer.load(Ordering::SeqCst) {
                gate.lifecycle_readers.fetch_add(1, Ordering::SeqCst);
                if !gate.lifecycle_writer.load(Ordering::SeqCst) {
                    // The registration borrows `gate`; we hold the read now
                    // and will never await it, so release it before moving
                    // the Arc into the lease.
                    drop(notified);
                    return Ok(SharedLease { gate });
                }
                if gate.lifecycle_readers.fetch_sub(1, Ordering::SeqCst) == 1 {
                    notify_lifecycle_waiters(&gate);
                }
            }
            notified.await;
        }
    }

    #[track_caller]
    pub(crate) fn acquire_hot_write<'a>(
        &'a self,
        graph_id: &'a str,
    ) -> impl std::future::Future<Output = Result<HotWriteLease, String>> + 'a {
        let origin = if tracing_enabled() {
            origin_from_location(std::panic::Location::caller())
        } else {
            DirtyOrigin::Other
        };
        self.acquire_hot_write_with_origin(graph_id, origin)
    }

    async fn acquire_hot_write_with_origin(
        &self,
        graph_id: &str,
        origin: DirtyOrigin,
    ) -> Result<HotWriteLease, String> {
        let shared = self.acquire_lifecycle_shared(graph_id).await?;
        let gate = shared.gate.clone();
        acquire_hot_write_gate(&gate).await;
        Ok(HotWriteLease {
            gate,
            dirty: DirtyLeaseState::new(graph_id, origin),
            _shared: shared,
        })
    }

    #[track_caller]
    pub(crate) fn acquire_lifecycle_exclusive<'a>(
        &'a self,
        graph_id: &'a str,
    ) -> impl std::future::Future<Output = Result<ExclusiveLease, String>> + 'a {
        let origin = if tracing_enabled() {
            origin_from_location(std::panic::Location::caller())
        } else {
            DirtyOrigin::Other
        };
        self.acquire_lifecycle_exclusive_with_origin(graph_id, origin)
    }

    async fn acquire_lifecycle_exclusive_with_origin(
        &self,
        graph_id: &str,
        origin: DirtyOrigin,
    ) -> Result<ExclusiveLease, String> {
        let gate = self.gate(graph_id)?;
        loop {
            // Register before checking the atomic to avoid a release between
            // the failed CAS and awaiting the notification.
            let notified = gate.lifecycle_async_notify.notified();
            if gate
                .lifecycle_writer
                .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                break;
            }
            notified.await;
        }
        let mut writer_intent = LifecycleWriterIntent {
            gate: gate.clone(),
            armed: true,
        };
        loop {
            let notified = gate.lifecycle_async_notify.notified();
            if gate.lifecycle_readers.load(Ordering::SeqCst) == 0 {
                break;
            }
            notified.await;
        }
        // Lifecycle writer intent prevents new shared/hot-write entrants, and
        // draining readers proves every earlier hot-write holder has released.
        gate.hot_write_held
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .map_err(|_| "graph hot-write gate remained held after reader drain".to_string())?;
        let lease = ExclusiveLease {
            gate,
            dirty: DirtyLeaseState::new(graph_id, origin),
        };
        writer_intent.disarm();
        Ok(lease)
    }

    /// Synchronous counterpart for Tauri/MCP graph commands. Async loopback
    /// routes use the async forms; this path waits on a condvar rather than
    /// nesting a Tokio runtime or holding a runtime mutex guard.
    ///
    /// The synchronous service roots are reachable in desktop builds. The
    /// headless library pass reaches them only through test targets, so do not
    /// report the API itself as dead in that feature projection.
    #[cfg_attr(feature = "headless", allow(dead_code))]
    pub(crate) fn acquire_lifecycle_shared_blocking(
        &self,
        graph_id: &str,
    ) -> Result<SharedLease, String> {
        let gate = self.gate(graph_id)?;
        let mut wait = gate
            .lifecycle_blocking_wait
            .lock()
            .map_err(|_| "graph lifecycle blocking gate poisoned".to_string())?;
        loop {
            if !gate.lifecycle_writer.load(Ordering::SeqCst) {
                gate.lifecycle_readers.fetch_add(1, Ordering::SeqCst);
                if !gate.lifecycle_writer.load(Ordering::SeqCst) {
                    drop(wait);
                    return Ok(SharedLease { gate });
                }
                if gate.lifecycle_readers.fetch_sub(1, Ordering::SeqCst) == 1 {
                    gate.lifecycle_blocking_notify.notify_all();
                    notify_async_lifecycle_waiters(&gate);
                }
            }
            wait = gate
                .lifecycle_blocking_notify
                .wait(wait)
                .map_err(|_| "graph lifecycle blocking wait poisoned".to_string())?;
        }
    }

    #[cfg_attr(feature = "headless", allow(dead_code))]
    #[track_caller]
    pub(crate) fn acquire_hot_write_blocking(
        &self,
        graph_id: &str,
    ) -> Result<HotWriteLease, String> {
        let origin = if tracing_enabled() {
            origin_from_location(std::panic::Location::caller())
        } else {
            DirtyOrigin::Other
        };
        self.acquire_hot_write_blocking_with_origin(graph_id, origin)
    }

    fn acquire_hot_write_blocking_with_origin(
        &self,
        graph_id: &str,
        origin: DirtyOrigin,
    ) -> Result<HotWriteLease, String> {
        let shared = self.acquire_lifecycle_shared_blocking(graph_id)?;
        let gate = shared.gate.clone();
        acquire_hot_write_gate_blocking(&gate)?;
        Ok(HotWriteLease {
            gate,
            dirty: DirtyLeaseState::new(graph_id, origin),
            _shared: shared,
        })
    }

    #[cfg_attr(feature = "headless", allow(dead_code))]
    #[track_caller]
    pub(crate) fn acquire_lifecycle_exclusive_blocking(
        &self,
        graph_id: &str,
    ) -> Result<ExclusiveLease, String> {
        let origin = if tracing_enabled() {
            origin_from_location(std::panic::Location::caller())
        } else {
            DirtyOrigin::Other
        };
        self.acquire_lifecycle_exclusive_blocking_with_origin(graph_id, origin)
    }

    fn acquire_lifecycle_exclusive_blocking_with_origin(
        &self,
        graph_id: &str,
        origin: DirtyOrigin,
    ) -> Result<ExclusiveLease, String> {
        let gate = self.gate(graph_id)?;
        let mut wait = gate
            .lifecycle_blocking_wait
            .lock()
            .map_err(|_| "graph lifecycle blocking gate poisoned".to_string())?;
        loop {
            if gate
                .lifecycle_writer
                .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                break;
            }
            wait = gate
                .lifecycle_blocking_notify
                .wait(wait)
                .map_err(|_| "graph lifecycle blocking wait poisoned".to_string())?;
        }
        let mut writer_intent = LifecycleWriterIntent {
            gate: gate.clone(),
            armed: true,
        };
        while gate.lifecycle_readers.load(Ordering::SeqCst) != 0 {
            wait = match gate.lifecycle_blocking_notify.wait(wait) {
                Ok(wait) => wait,
                Err(_) => {
                    drop(writer_intent);
                    return Err("graph lifecycle blocking wait poisoned".to_string());
                }
            };
        }
        drop(wait);
        if let Err(error) = acquire_hot_write_gate_blocking(&gate) {
            drop(writer_intent);
            return Err(error);
        }
        let lease = ExclusiveLease {
            gate,
            dirty: DirtyLeaseState::new(graph_id, origin),
        };
        writer_intent.disarm();
        Ok(lease)
    }

    pub(crate) fn generation(&self, graph_id: &str) -> Result<u64, String> {
        Ok(self.gate(graph_id)?.generation.load(Ordering::SeqCst))
    }

    /// Called while the graph's root lease is held by deletion.
    pub(crate) fn advance_generation(&self, graph_id: &str) -> Result<u64, String> {
        Ok(self
            .gate(graph_id)?
            .generation
            .fetch_add(1, Ordering::SeqCst)
            .wrapping_add(1))
    }

    pub(crate) fn require_generation(&self, graph_id: &str, expected: u64) -> Result<(), String> {
        let actual = self.generation(graph_id)?;
        // Generation is process-local cancellation state, while the graph UUID
        // is the durable cross-process incarnation fence. After restart a valid
        // queued flush may carry a value greater than the reset actual=0; allow
        // that. Only a generation advanced in this process beyond the captured
        // value proves the scheduled work was superseded/deleted here.
        if actual <= expected {
            Ok(())
        } else {
            Err(format!(
                "stale graph generation for {graph_id}: expected {expected}, actual {actual}"
            ))
        }
    }
}

async fn acquire_hot_write_gate(gate: &GraphGate) {
    loop {
        // Register before checking the atomic to avoid a release between the
        // failed CAS and awaiting the notification.
        let notified = gate.hot_write_async_notify.notified();
        if gate
            .hot_write_held
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            return;
        }
        notified.await;
    }
}

fn acquire_hot_write_gate_blocking(gate: &GraphGate) -> Result<(), String> {
    let mut wait = gate
        .hot_write_blocking_wait
        .lock()
        .map_err(|_| "graph hot-write blocking gate poisoned".to_string())?;
    loop {
        if gate
            .hot_write_held
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            return Ok(());
        }
        wait = gate
            .hot_write_blocking_notify
            .wait(wait)
            .map_err(|_| "graph hot-write blocking wait poisoned".to_string())?;
    }
}

fn notify_async_lifecycle_waiters(gate: &GraphGate) {
    // Wake already-registered readers together; notify_one also stores a
    // permit for a future created just before, but not yet polled at, release.
    gate.lifecycle_async_notify.notify_waiters();
    gate.lifecycle_async_notify.notify_one();
}

fn notify_lifecycle_waiters(gate: &GraphGate) {
    // Pair with the blocking wait mutex so a blocking waiter cannot miss the
    // transition between its state check and condvar sleep.
    let _wait = gate.lifecycle_blocking_wait.lock().ok();
    gate.lifecycle_blocking_notify.notify_all();
    notify_async_lifecycle_waiters(gate);
}

fn release_hot_write(gate: &GraphGate) {
    let _wait = gate.hot_write_blocking_wait.lock().ok();
    gate.hot_write_held.store(false, Ordering::SeqCst);
    gate.hot_write_blocking_notify.notify_one();
    // notify_one stores a permit when an async waiter has created but not yet
    // polled `notified()`, closing the failed-CAS lost-wakeup window.
    gate.hot_write_async_notify.notify_one();
}

fn release_lifecycle_writer(gate: &GraphGate) {
    let _wait = gate.lifecycle_blocking_wait.lock().ok();
    gate.lifecycle_writer.store(false, Ordering::SeqCst);
    gate.lifecycle_blocking_notify.notify_all();
    notify_async_lifecycle_waiters(gate);
}

/// Acquire process-local hot-write authority when the runtime manages it.
///
/// A few narrow unit harnesses construct service helpers without installing
/// Tauri state, so absence remains supported. Every real desktop/headless setup
/// manages the coordinator before exposing mutation entrypoints.
#[allow(dead_code)]
pub(crate) fn acquire_lifecycle_shared_blocking_if_managed(
    app: &AppHandle,
    graph_id: &str,
) -> Result<Option<SharedLease>, String> {
    app.try_state::<GraphPersistenceCoordinator>()
        .map(|coordinator| coordinator.acquire_lifecycle_shared_blocking(graph_id))
        .transpose()
}

#[track_caller]
pub(crate) fn acquire_hot_write_blocking_if_managed(
    app: &AppHandle,
    graph_id: &str,
) -> Result<Option<HotWriteLease>, String> {
    let origin = if tracing_enabled() {
        origin_from_location(std::panic::Location::caller())
    } else {
        DirtyOrigin::Other
    };
    app.try_state::<GraphPersistenceCoordinator>()
        .map(|coordinator| coordinator.acquire_hot_write_blocking_with_origin(graph_id, origin))
        .transpose()
}

/// Acquire process-local exclusive lifecycle authority when managed.
#[track_caller]
pub(crate) fn acquire_lifecycle_exclusive_blocking_if_managed(
    app: &AppHandle,
    graph_id: &str,
) -> Result<Option<ExclusiveLease>, String> {
    let origin = if tracing_enabled() {
        origin_from_location(std::panic::Location::caller())
    } else {
        DirtyOrigin::Other
    };
    app.try_state::<GraphPersistenceCoordinator>()
        .map(|coordinator| {
            coordinator.acquire_lifecycle_exclusive_blocking_with_origin(graph_id, origin)
        })
        .transpose()
}

impl Drop for SharedLease {
    fn drop(&mut self) {
        let _wait = self.gate.lifecycle_blocking_wait.lock().ok();
        let previous = self.gate.lifecycle_readers.fetch_sub(1, Ordering::SeqCst);
        debug_assert!(previous > 0, "shared lease reader count underflow");
        if previous == 1 {
            self.gate.lifecycle_blocking_notify.notify_all();
            notify_async_lifecycle_waiters(&self.gate);
        }
    }
}

impl Drop for HotWriteLease {
    fn drop(&mut self) {
        // This lease encloses Garden's graph mutation roots, including the
        // low-level materializers that intentionally accept only `&Store`.
        // Mark on completion so a concurrent checkpoint can acknowledge only
        // work that had already finished before it began. Skipped only when
        // this exact lease was declared read-only or its possible writes were
        // proven self-tracked (see `declare_rdf_read_only` /
        // `declare_rdf_writes_self_tracked`) — every other root remains
        // unconditional, matching the fail-toward-dirty safety direction: a
        // false-DIRTY (an extra backup) is fine, a false-CLEAN silently loses
        // data.
        self.dirty.mark_if_needed();
        release_hot_write(&self.gate);
        // `_shared` drops after this body, preserving hot-write-before-shared
        // release order.
    }
}

impl Drop for ExclusiveLease {
    fn drop(&mut self) {
        self.dirty.mark_if_needed();
        release_hot_write(&self.gate);
        release_lifecycle_writer(&self.gate);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crdt_engine::rooms::RoomRegistry;
    use uuid::Uuid;
    use yrs::updates::decoder::Decode;
    use yrs::{Map, ReadTxn, StateVector, Transact, Update, WriteTxn};

    #[test]
    fn async_waiter_cannot_enter_before_root_lease_releases() {
        crate::app_runtime::async_runtime::block_on(async {
            let coordinator = Arc::new(GraphPersistenceCoordinator::default());
            let first = coordinator
                .acquire_hot_write("graph-a")
                .await
                .expect("first lease");
            let (started_tx, started_rx) = tokio::sync::oneshot::channel();
            let entered = Arc::new(Notify::new());
            let entered_task = entered.clone();
            let coordinator_task = coordinator.clone();
            let waiter = crate::app_runtime::async_runtime::spawn(async move {
                let _ = started_tx.send(());
                let _lease = coordinator_task
                    .acquire_hot_write("graph-a")
                    .await
                    .expect("second lease");
                entered_task.notify_one();
            });
            started_rx.await.expect("waiter started");
            tokio::task::yield_now().await;
            assert!(
                tokio::time::timeout(std::time::Duration::from_millis(25), entered.notified())
                    .await
                    .is_err(),
                "second root entered while first lease was held"
            );
            drop(first);
            tokio::time::timeout(std::time::Duration::from_secs(1), entered.notified())
                .await
                .expect("second root entered after release");
            waiter.await.expect("waiter task");
        });
    }

    #[test]
    fn blocking_release_before_first_async_poll_preserves_wakeup() {
        crate::app_runtime::async_runtime::block_on(async {
            let coordinator = GraphPersistenceCoordinator::default();
            let first = coordinator
                .acquire_hot_write_blocking("graph-mixed")
                .expect("blocking lease");
            let gate = coordinator.gate("graph-mixed").expect("graph gate");

            // Reproduce the exact dangerous window deterministically: the
            // async side has failed CAS but has not polled `notified()` when the
            // blocking owner releases.
            let notified = gate.hot_write_async_notify.notified();
            assert!(gate
                .hot_write_held
                .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                .is_err());
            drop(first);
            notified.await;
            assert!(gate
                .hot_write_held
                .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok());
            release_hot_write(&gate);
        });
    }

    #[test]
    fn open_succeeds_during_held_hot_write() {
        crate::app_runtime::async_runtime::block_on(async {
            let coordinator = GraphPersistenceCoordinator::default();
            let _hot_write = coordinator
                .acquire_hot_write("graph-open")
                .await
                .expect("hot-write lease");

            tokio::time::timeout(std::time::Duration::from_millis(100), async {
                let shared = coordinator
                    .acquire_lifecycle_shared("graph-open")
                    .await
                    .expect("shared lease");
                drop(shared);
            })
            .await
            .expect("shared open blocked behind hot write");
        });
    }

    #[test]
    fn exclusive_excludes_shared_and_hot_write() {
        crate::app_runtime::async_runtime::block_on(async {
            let coordinator = Arc::new(GraphPersistenceCoordinator::default());
            let exclusive = coordinator
                .acquire_lifecycle_exclusive("graph-exclusive")
                .await
                .expect("exclusive lease");

            let (shared_started_tx, shared_started_rx) = tokio::sync::oneshot::channel();
            let (shared_entered_tx, mut shared_entered_rx) = tokio::sync::oneshot::channel();
            let shared_coordinator = coordinator.clone();
            let shared_waiter = crate::app_runtime::async_runtime::spawn(async move {
                let _ = shared_started_tx.send(());
                let _lease = shared_coordinator
                    .acquire_lifecycle_shared("graph-exclusive")
                    .await
                    .expect("shared lease");
                let _ = shared_entered_tx.send(());
            });

            let (hot_started_tx, hot_started_rx) = tokio::sync::oneshot::channel();
            let (hot_entered_tx, mut hot_entered_rx) = tokio::sync::oneshot::channel();
            let hot_coordinator = coordinator.clone();
            let hot_waiter = crate::app_runtime::async_runtime::spawn(async move {
                let _ = hot_started_tx.send(());
                let _lease = hot_coordinator
                    .acquire_hot_write("graph-exclusive")
                    .await
                    .expect("hot-write lease");
                let _ = hot_entered_tx.send(());
            });

            shared_started_rx.await.expect("shared waiter started");
            hot_started_rx.await.expect("hot-write waiter started");
            tokio::task::yield_now().await;
            assert!(
                tokio::time::timeout(std::time::Duration::from_millis(25), &mut shared_entered_rx)
                    .await
                    .is_err(),
                "shared lease entered while exclusive was held"
            );
            assert!(
                tokio::time::timeout(std::time::Duration::from_millis(25), &mut hot_entered_rx)
                    .await
                    .is_err(),
                "hot-write lease entered while exclusive was held"
            );

            drop(exclusive);
            tokio::time::timeout(std::time::Duration::from_secs(1), shared_entered_rx)
                .await
                .expect("shared waiter remained blocked after exclusive release")
                .expect("shared waiter dropped");
            tokio::time::timeout(std::time::Duration::from_secs(1), hot_entered_rx)
                .await
                .expect("hot-write waiter remained blocked after exclusive release")
                .expect("hot-write waiter dropped");
            shared_waiter.await.expect("shared waiter task");
            hot_waiter.await.expect("hot-write waiter task");
        });
    }

    #[test]
    fn generation_allows_restart_reset_but_rejects_process_local_advance() {
        let coordinator = GraphPersistenceCoordinator::default();
        assert!(
            coordinator.require_generation("graph-restart", 7).is_ok(),
            "a durable flush captured before restart may exceed reset actual=0"
        );
        coordinator
            .advance_generation("graph-restart")
            .expect("advance process-local generation");
        let error = coordinator
            .require_generation("graph-restart", 0)
            .expect_err("a process-local advance supersedes generation zero");
        assert!(error.contains("stale graph generation"), "{error}");
        assert!(coordinator.require_generation("graph-restart", 1).is_ok());
        assert!(coordinator.require_generation("graph-restart", 7).is_ok());
    }

    #[test]
    fn delayed_old_hot_write_cannot_land_after_new_websocket_update() {
        crate::app_runtime::async_runtime::block_on(async {
            let dir = std::env::temp_dir().join(format!("garden-hot-order-{}", Uuid::new_v4()));
            let state_path = dir.join("update-v1.bin");
            let room = RoomRegistry::default()
                .get_or_create("doc:graph-hot:document", state_path.clone())
                .await
                .expect("room");
            let old_remote = yrs::Doc::new();
            {
                let mut txn = old_remote.transact_mut();
                txn.get_or_insert_map("metadata")
                    .insert(&mut txn, "old", true);
            }
            let old_update = old_remote
                .transact()
                .encode_state_as_update_v1(&StateVector::default());
            room.apply_client_update(&old_update)
                .await
                .expect("old websocket update");
            let delayed_old_bytes = room.encode_state_for_test().await;

            let coordinator = Arc::new(GraphPersistenceCoordinator::default());
            let old_lease = coordinator
                .acquire_hot_write("graph-hot")
                .await
                .expect("old root lease");
            let (started_tx, started_rx) = tokio::sync::oneshot::channel();
            let coordinator_new = coordinator.clone();
            let room_new = room.clone();
            let newer = crate::app_runtime::async_runtime::spawn(async move {
                let _ = started_tx.send(());
                let _new_lease = coordinator_new
                    .acquire_hot_write("graph-hot")
                    .await
                    .expect("new root lease");
                let remote = yrs::Doc::new();
                {
                    let mut txn = remote.transact_mut();
                    txn.get_or_insert_map("metadata")
                        .insert(&mut txn, "new", true);
                }
                let update = remote
                    .transact()
                    .encode_state_as_update_v1(&StateVector::default());
                room_new
                    .apply_client_update(&update)
                    .await
                    .expect("new websocket update");
            });
            started_rx.await.expect("new websocket root started");

            // Model the original dangerous interleaving: E was snapshotted,
            // then its file rename was delayed. The hot-write lease keeps E+1
            // from mutating/snapshotting until this old write is complete.
            room.persist_state_bytes_for_test(&delayed_old_bytes)
                .expect("persist delayed old hot state");
            drop(old_lease);
            newer.await.expect("new websocket task");

            let bytes = std::fs::read(&state_path).expect("final hot state");
            let final_doc = yrs::Doc::new();
            {
                let mut txn = final_doc.transact_mut();
                txn.apply_update(Update::decode_v1(&bytes).expect("decode final hot state"))
                    .expect("apply final hot state");
            }
            let txn = final_doc.transact();
            let metadata = txn.get_map("metadata").expect("metadata map");
            assert!(
                metadata.contains_key(&txn, "new"),
                "new hot update was lost"
            );
            let _ = std::fs::remove_dir_all(dir);
        });
    }
}
