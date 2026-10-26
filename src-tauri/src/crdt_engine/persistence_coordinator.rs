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
use std::time::{Duration, Instant};
#[cfg(feature = "desktop")]
use tauri::Manager;
use tokio::sync::Notify;
use tracing::Instrument;

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

/// Admission of client (websocket) update frames, kept apart from the hot-write
/// gate. A frame only needs the shared lifecycle lease to stay ordered against
/// deletion; the hot-write gate also serialized it behind every queued API write.
/// The few operations that need a quiet room (a restore that applies a diff and
/// then verifies the room equals the target) fence frames explicitly instead:
/// raising a fence waits for frames already in flight and holds new ones back.
#[derive(Default)]
struct FrameGate {
    fences: AtomicUsize,
    in_flight: AtomicUsize,
    changed: Notify,
}

struct GraphGate {
    frames: FrameGate,
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
    hot_holder: Mutex<Option<LeaseHolder>>,
    lifecycle_holder: Mutex<Option<LeaseHolder>>,
}

pub(crate) struct SharedLease {
    gate: Arc<GraphGate>,
    _trace: LeaseTrace,
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
    _trace: LeaseTrace,
    hot_write_released: bool,
}

pub(crate) struct ExclusiveLease {
    gate: Arc<GraphGate>,
    _trace: LeaseTrace,
    dirty: DirtyLeaseState,
}

/// One client frame's place in a graph: the shared lifecycle lease (so deletion
/// and other exclusive roots still wait for it) and a slot in the frame gate
/// (so a frame fence can wait for it). Many frames hold this at once.
pub(crate) struct ClientFrameLease {
    gate: Arc<GraphGate>,
    _shared: SharedLease,
}

/// While held, no client frame is admitted and none is in flight on this graph.
pub(crate) struct ClientFrameFence {
    gate: Arc<GraphGate>,
}

impl Drop for ClientFrameLease {
    fn drop(&mut self) {
        let previous = self.gate.frames.in_flight.fetch_sub(1, Ordering::SeqCst);
        debug_assert!(previous > 0, "client frame count underflow");
        self.gate.frames.changed.notify_waiters();
    }
}

impl Drop for ClientFrameFence {
    fn drop(&mut self) {
        let previous = self.gate.frames.fences.fetch_sub(1, Ordering::SeqCst);
        debug_assert!(previous > 0, "client frame fence count underflow");
        self.gate.frames.changed.notify_waiters();
    }
}

struct LeaseHolder {
    kind: &'static str,
    since: Instant,
}

// The span owns no entered guard: a lease may cross await/thread boundaries.
// Its lifetime includes both acquisition and the complete critical section.
struct LeaseTrace {
    span: tracing::Span,
    waiting: Option<Instant>,
    acquired: Option<Instant>,
}

impl LeaseTrace {
    fn new(graph_id: &str, kind: &'static str, mode: &'static str) -> Self {
        let span = match mode {
            "hot_write" => tracing::info_span!(target: "gardend_otel", "graph.lease.hot_write",
                sophia.graph.id = graph_id, sophia.lease.holder.kind = kind, sophia.lease.origin = kind,
                sophia.lease.wait_ms = tracing::field::Empty, sophia.lease.held_ms = tracing::field::Empty,
                sophia.lease.blocker.kind = tracing::field::Empty, sophia.lease.blocker.held_ms = tracing::field::Empty,
                sophia.lease.outcome = tracing::field::Empty),
            "exclusive" => {
                tracing::info_span!(target: "gardend_otel", "graph.lease.lifecycle_exclusive",
                sophia.graph.id = graph_id, sophia.lease.holder.kind = kind, sophia.lease.origin = kind,
                sophia.lease.wait_ms = tracing::field::Empty, sophia.lease.held_ms = tracing::field::Empty,
                sophia.lease.blocker.kind = tracing::field::Empty, sophia.lease.blocker.held_ms = tracing::field::Empty,
                sophia.lease.outcome = tracing::field::Empty)
            }
            _ => tracing::info_span!(target: "gardend_otel", "graph.lease.lifecycle_shared",
                sophia.graph.id = graph_id, sophia.lease.holder.kind = kind, sophia.lease.origin = kind,
                sophia.lease.wait_ms = tracing::field::Empty, sophia.lease.held_ms = tracing::field::Empty,
                sophia.lease.blocker.kind = tracing::field::Empty, sophia.lease.blocker.held_ms = tracing::field::Empty,
                sophia.lease.outcome = tracing::field::Empty),
        };
        let waiting = (!span.is_disabled()).then(Instant::now);
        Self {
            span,
            waiting,
            acquired: None,
        }
    }

    fn acquired(&mut self) {
        if self.waiting.is_some() {
            self.span.record(
                "sophia.lease.wait_ms",
                crate::telemetry::elapsed_ms(self.waiting),
            );
            self.span.record("sophia.lease.outcome", "acquired");
            self.acquired = Some(Instant::now());
        }
    }

    fn blocker(&self, gate: &GraphGate, lifecycle: bool) {
        if self.span.is_disabled() {
            return;
        }
        let holder = if lifecycle {
            &gate.lifecycle_holder
        } else {
            &gate.hot_holder
        };
        if let Ok(holder) = holder.lock() {
            if let Some(holder) = holder.as_ref() {
                self.span.record("sophia.lease.blocker.kind", holder.kind);
                self.span.record(
                    "sophia.lease.blocker.held_ms",
                    holder.since.elapsed().as_secs_f64() * 1000.0,
                );
            }
        }
    }

    async fn observe<F: std::future::Future>(
        &self,
        gate: &GraphGate,
        lifecycle: bool,
        future: F,
    ) -> F::Output {
        if self.span.is_disabled() {
            return future.await;
        }
        tokio::pin!(future);
        tokio::select! {
            output = &mut future => output,
            _ = tokio::time::sleep(Duration::from_secs(1).saturating_sub(
                self.waiting.map_or(Duration::ZERO, |start| start.elapsed()),
            )) => {
                self.blocker(gate, lifecycle);
                future.await
            }
        }
    }
}

impl Drop for LeaseTrace {
    fn drop(&mut self) {
        if self.span.is_disabled() {
            return;
        }
        if let Some(acquired) = self.acquired {
            self.span.record(
                "sophia.lease.held_ms",
                acquired.elapsed().as_secs_f64() * 1000.0,
            );
        } else {
            self.span.record(
                "sophia.lease.wait_ms",
                crate::telemetry::elapsed_ms(self.waiting),
            );
            self.span.record("sophia.lease.outcome", "cancelled");
        }
    }
}

/// Remembered only while this acquisition is traced: its own span says so, which (unlike a callsite's cached
/// interest) is exact for the dispatcher the lease was taken under.
fn set_holder(slot: &Mutex<Option<LeaseHolder>>, kind: &'static str, trace: &LeaseTrace) {
    if !trace.span.is_disabled() {
        if let Ok(mut holder) = slot.lock() {
            *holder = Some(LeaseHolder {
                kind,
                since: Instant::now(),
            });
        }
    }
}

fn clear_holder(slot: &Mutex<Option<LeaseHolder>>) {
    if crate::telemetry::enabled() {
        if let Ok(mut holder) = slot.lock() {
            *holder = None;
        }
    }
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
                if !self._trace.span.is_disabled() {
                    let kind = crate::telemetry::safe_crdt_kind(kind);
                    self._trace.span.record("sophia.lease.holder.kind", kind);
                    if let Ok(mut holder) = self.gate.hot_holder.lock() {
                        if let Some(holder) = holder.as_mut() {
                            holder.kind = kind;
                        }
                    }
                }
            }
        }
    };
}

impl_writing_lease!(HotWriteLease);
impl_writing_lease!(ExclusiveLease);

impl HotWriteLease {
    /// The room has committed its authority; only its ordered projection tail
    /// remains. Keep the lifecycle reader continuously held against deletion
    /// and mark RDF dirty when that tail finishes, while allowing other hot
    /// writers through now. The tail must never rewrite the room sidecar.
    pub(crate) fn release_for_projection(&mut self) {
        if !self.hot_write_released {
            self.hot_write_released = true;
            release_hot_write(&self.gate);
            // End the hold measurement here, not when the projection finishes.
            self._trace = LeaseTrace {
                span: tracing::Span::none(),
                waiting: None,
                acquired: None,
            };
        }
    }
}

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
            frames: FrameGate::default(),
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
            hot_holder: Mutex::new(None),
            lifecycle_holder: Mutex::new(None),
        });
        gates.insert(graph_id.to_string(), gate.clone());
        Ok(gate)
    }

    pub(crate) async fn acquire_lifecycle_shared(
        &self,
        graph_id: &str,
    ) -> Result<SharedLease, String> {
        let gate = self.gate(graph_id)?;
        self.acquire_lifecycle_shared_gate(graph_id, gate).await
    }

    async fn acquire_lifecycle_shared_gate(
        &self,
        graph_id: &str,
        gate: Arc<GraphGate>,
    ) -> Result<SharedLease, String> {
        self.acquire_lifecycle_shared_gate_for(graph_id, gate, "shared")
            .await
    }

    async fn acquire_lifecycle_shared_gate_for(
        &self,
        graph_id: &str,
        gate: Arc<GraphGate>,
        kind: &'static str,
    ) -> Result<SharedLease, String> {
        let mut trace = LeaseTrace::new(graph_id, kind, "shared");
        trace
            .observe(&gate, true, async {
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
                            return Ok::<(), String>(());
                        }
                        if gate.lifecycle_readers.fetch_sub(1, Ordering::SeqCst) == 1 {
                            notify_lifecycle_waiters(&gate);
                        }
                    }
                    notified.await;
                }
            })
            .await?;
        trace.acquired();
        Ok(SharedLease {
            gate,
            _trace: trace,
        })
    }

    /// Admit one client update frame. Unlike [`Self::acquire_hot_write`] this
    /// does not queue behind other writers: it waits only for a lifecycle
    /// exclusive root (deletion and kin) and for an explicit frame fence.
    pub(crate) async fn acquire_client_frame(
        &self,
        graph_id: &str,
    ) -> Result<ClientFrameLease, String> {
        let gate = self.gate(graph_id)?;
        let shared = self
            .acquire_lifecycle_shared_gate_for(graph_id, gate.clone(), "ws.frame")
            .await?;
        loop {
            // Created before the checks so a release in between still wakes it.
            let changed = gate.frames.changed.notified();
            if gate.frames.fences.load(Ordering::SeqCst) == 0 {
                gate.frames.in_flight.fetch_add(1, Ordering::SeqCst);
                // Either this sees a fence raised meanwhile, or the fence's
                // drain sees this frame.
                if gate.frames.fences.load(Ordering::SeqCst) == 0 {
                    // `changed` borrows `gate`; end that borrow before moving it.
                    drop(changed);
                    return Ok(ClientFrameLease {
                        gate,
                        _shared: shared,
                    });
                }
                gate.frames.in_flight.fetch_sub(1, Ordering::SeqCst);
                gate.frames.changed.notify_waiters();
            }
            changed.await;
        }
    }

    /// Hold client frames off a graph: wait for the ones in flight, then admit
    /// none until the returned fence drops. The caller should already hold the
    /// operation's own lease; this never waits for a hot-write holder.
    pub(crate) async fn fence_client_frames(
        &self,
        graph_id: &str,
    ) -> Result<ClientFrameFence, String> {
        let gate = self.gate(graph_id)?;
        gate.frames.fences.fetch_add(1, Ordering::SeqCst);
        // Owned before the drain wait, so a cancelled wait still lifts it.
        let fence = ClientFrameFence { gate: gate.clone() };
        loop {
            let changed = gate.frames.changed.notified();
            if gate.frames.in_flight.load(Ordering::SeqCst) == 0 {
                return Ok(fence);
            }
            changed.await;
        }
    }

    #[track_caller]
    pub(crate) fn acquire_hot_write<'a>(
        &'a self,
        graph_id: &'a str,
    ) -> impl std::future::Future<Output = Result<HotWriteLease, String>> + 'a {
        let origin = if tracing_enabled() || crate::telemetry::enabled() {
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
        let mut trace = LeaseTrace::new(graph_id, origin.label(), "hot_write");
        let gate = self.gate(graph_id)?;
        let shared = trace
            .observe(
                &gate,
                true,
                self.acquire_lifecycle_shared_gate(graph_id, gate.clone())
                    .instrument(trace.span.clone()),
            )
            .await?;
        trace
            .observe(&gate, false, acquire_hot_write_gate(&gate))
            .await;
        trace.acquired();
        set_holder(&gate.hot_holder, origin.label(), &trace);
        Ok(HotWriteLease {
            gate,
            dirty: DirtyLeaseState::new(graph_id, origin),
            _shared: shared,
            _trace: trace,
            hot_write_released: false,
        })
    }

    #[track_caller]
    pub(crate) fn acquire_lifecycle_exclusive<'a>(
        &'a self,
        graph_id: &'a str,
    ) -> impl std::future::Future<Output = Result<ExclusiveLease, String>> + 'a {
        let origin = if tracing_enabled() || crate::telemetry::enabled() {
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
        let mut trace = LeaseTrace::new(graph_id, origin.label(), "exclusive");
        trace
            .observe(&gate, true, async {
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
            })
            .await;
        let mut writer_intent = LifecycleWriterIntent {
            gate: gate.clone(),
            armed: true,
        };
        set_holder(&gate.lifecycle_holder, origin.label(), &trace);
        trace
            .observe(&gate, false, async {
                loop {
                    let notified = gate.lifecycle_async_notify.notified();
                    if gate.lifecycle_readers.load(Ordering::SeqCst) == 0 {
                        break;
                    }
                    notified.await;
                }
            })
            .await;
        // Lifecycle writer intent prevents new shared/hot-write entrants, and
        // draining readers proves every earlier hot-write holder has released.
        gate.hot_write_held
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .map_err(|_| "graph hot-write gate remained held after reader drain".to_string())?;
        trace.acquired();
        set_holder(&gate.hot_holder, origin.label(), &trace);
        let lease = ExclusiveLease {
            gate,
            dirty: DirtyLeaseState::new(graph_id, origin),
            _trace: trace,
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
        let mut trace = LeaseTrace::new(graph_id, "shared", "shared");
        let mut wait = gate
            .lifecycle_blocking_wait
            .lock()
            .map_err(|_| "graph lifecycle blocking gate poisoned".to_string())?;
        loop {
            if !gate.lifecycle_writer.load(Ordering::SeqCst) {
                gate.lifecycle_readers.fetch_add(1, Ordering::SeqCst);
                if !gate.lifecycle_writer.load(Ordering::SeqCst) {
                    drop(wait);
                    trace.acquired();
                    return Ok(SharedLease {
                        gate,
                        _trace: trace,
                    });
                }
                if gate.lifecycle_readers.fetch_sub(1, Ordering::SeqCst) == 1 {
                    gate.lifecycle_blocking_notify.notify_all();
                    notify_async_lifecycle_waiters(&gate);
                }
            }
            wait = wait_lifecycle_gate(&gate, &trace, wait, true)?;
        }
    }

    #[cfg_attr(feature = "headless", allow(dead_code))]
    #[track_caller]
    pub(crate) fn acquire_hot_write_blocking(
        &self,
        graph_id: &str,
    ) -> Result<HotWriteLease, String> {
        let origin = if tracing_enabled() || crate::telemetry::enabled() {
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
        let mut trace = LeaseTrace::new(graph_id, origin.label(), "hot_write");
        let shared = trace
            .span
            .in_scope(|| self.acquire_lifecycle_shared_blocking(graph_id))?;
        let gate = shared.gate.clone();
        acquire_hot_write_gate_blocking(&gate, &trace)?;
        trace.acquired();
        set_holder(&gate.hot_holder, origin.label(), &trace);
        Ok(HotWriteLease {
            gate,
            dirty: DirtyLeaseState::new(graph_id, origin),
            _shared: shared,
            _trace: trace,
            hot_write_released: false,
        })
    }

    #[cfg_attr(feature = "headless", allow(dead_code))]
    #[track_caller]
    pub(crate) fn acquire_lifecycle_exclusive_blocking(
        &self,
        graph_id: &str,
    ) -> Result<ExclusiveLease, String> {
        let origin = if tracing_enabled() || crate::telemetry::enabled() {
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
        let mut trace = LeaseTrace::new(graph_id, origin.label(), "exclusive");
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
            wait = wait_lifecycle_gate(&gate, &trace, wait, true)?;
        }
        let mut writer_intent = LifecycleWriterIntent {
            gate: gate.clone(),
            armed: true,
        };
        set_holder(&gate.lifecycle_holder, origin.label(), &trace);
        while gate.lifecycle_readers.load(Ordering::SeqCst) != 0 {
            wait = match wait_lifecycle_gate(&gate, &trace, wait, false) {
                Ok(wait) => wait,
                Err(_) => {
                    drop(writer_intent);
                    return Err("graph lifecycle blocking wait poisoned".to_string());
                }
            };
        }
        drop(wait);
        if let Err(error) = acquire_hot_write_gate_blocking(&gate, &trace) {
            drop(writer_intent);
            return Err(error);
        }
        trace.acquired();
        set_holder(&gate.hot_holder, origin.label(), &trace);
        let lease = ExclusiveLease {
            gate,
            dirty: DirtyLeaseState::new(graph_id, origin),
            _trace: trace,
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

fn acquire_hot_write_gate_blocking(gate: &GraphGate, trace: &LeaseTrace) -> Result<(), String> {
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
        wait = if trace.span.is_disabled() {
            gate.hot_write_blocking_notify
                .wait(wait)
                .map_err(|_| "graph hot-write blocking wait poisoned".to_string())?
        } else {
            gate.hot_write_blocking_notify
                .wait_timeout(wait, Duration::from_secs(1))
                .map_err(|_| "graph hot-write blocking wait poisoned".to_string())?
                .0
        };
        if trace
            .waiting
            .is_some_and(|start| start.elapsed() >= Duration::from_secs(1))
        {
            trace.blocker(gate, false);
        }
    }
}

fn wait_lifecycle_gate<'a>(
    gate: &'a GraphGate,
    trace: &LeaseTrace,
    wait: std::sync::MutexGuard<'a, ()>,
    lifecycle: bool,
) -> Result<std::sync::MutexGuard<'a, ()>, String> {
    if trace.span.is_disabled() {
        return gate
            .lifecycle_blocking_notify
            .wait(wait)
            .map_err(|_| "graph lifecycle blocking wait poisoned".to_string());
    }
    let wait = gate
        .lifecycle_blocking_notify
        .wait_timeout(wait, Duration::from_secs(1))
        .map_err(|_| "graph lifecycle blocking wait poisoned".to_string())?
        .0;
    if trace
        .waiting
        .is_some_and(|start| start.elapsed() >= Duration::from_secs(1))
    {
        trace.blocker(gate, lifecycle);
    }
    Ok(wait)
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
    clear_holder(&gate.hot_holder);
    let _wait = gate.hot_write_blocking_wait.lock().ok();
    gate.hot_write_held.store(false, Ordering::SeqCst);
    gate.hot_write_blocking_notify.notify_one();
    // notify_one stores a permit when an async waiter has created but not yet
    // polled `notified()`, closing the failed-CAS lost-wakeup window.
    gate.hot_write_async_notify.notify_one();
}

fn release_lifecycle_writer(gate: &GraphGate) {
    clear_holder(&gate.lifecycle_holder);
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
    let origin = if tracing_enabled() || crate::telemetry::enabled() {
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
    let origin = if tracing_enabled() || crate::telemetry::enabled() {
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
        if !self.hot_write_released {
            release_hot_write(&self.gate);
        }
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

    const FRAME_WAIT: std::time::Duration = std::time::Duration::from_millis(50);

    #[test]
    fn client_frames_do_not_queue_behind_a_hot_write_holder() {
        crate::app_runtime::async_runtime::block_on(async {
            let coordinator = GraphPersistenceCoordinator::default();
            // An agent's queued write owns the hot-write gate for as long as it likes.
            let _writer = coordinator
                .acquire_hot_write("graph-frames")
                .await
                .expect("writer");
            let first = tokio::time::timeout(
                std::time::Duration::from_secs(1),
                coordinator.acquire_client_frame("graph-frames"),
            )
            .await
            .expect("a frame is admitted beside a hot-write holder")
            .expect("frame lease");
            // Frames share: a second one is not excluded by the first.
            let second = tokio::time::timeout(
                std::time::Duration::from_secs(1),
                coordinator.acquire_client_frame("graph-frames"),
            )
            .await
            .expect("frames are concurrent")
            .expect("second frame lease");
            drop((first, second));
        });
    }

    #[test]
    fn lifecycle_exclusive_roots_still_exclude_client_frames() {
        crate::app_runtime::async_runtime::block_on(async {
            let coordinator = Arc::new(GraphPersistenceCoordinator::default());
            // A frame in flight: deletion (an exclusive root) waits for it.
            let frame = coordinator
                .acquire_client_frame("graph-delete")
                .await
                .expect("frame lease");
            let exclusive_coordinator = coordinator.clone();
            let exclusive = crate::app_runtime::async_runtime::spawn(async move {
                exclusive_coordinator
                    .acquire_lifecycle_exclusive("graph-delete")
                    .await
                    .map(|_| ())
            });
            tokio::task::yield_now().await;
            tokio::time::sleep(FRAME_WAIT).await;
            assert!(
                !exclusive.is_finished(),
                "an exclusive root entered while a frame was in flight"
            );
            drop(frame);
            tokio::time::timeout(std::time::Duration::from_secs(1), exclusive)
                .await
                .expect("exclusive root enters once the frame is done")
                .expect("task")
                .expect("exclusive lease");

            // And while an exclusive root is held, no new frame gets in.
            let held = coordinator
                .acquire_lifecycle_exclusive("graph-delete")
                .await
                .expect("exclusive lease");
            assert!(
                tokio::time::timeout(FRAME_WAIT, coordinator.acquire_client_frame("graph-delete"))
                    .await
                    .is_err(),
                "a frame entered during an exclusive root"
            );
            drop(held);
            tokio::time::timeout(
                std::time::Duration::from_secs(1),
                coordinator.acquire_client_frame("graph-delete"),
            )
            .await
            .expect("frames resume")
            .expect("frame lease");
        });
    }

    #[test]
    fn a_frame_fence_drains_frames_in_flight_and_holds_new_ones() {
        crate::app_runtime::async_runtime::block_on(async {
            let coordinator = Arc::new(GraphPersistenceCoordinator::default());
            let in_flight = coordinator
                .acquire_client_frame("graph-fence")
                .await
                .expect("frame lease");

            let fencing = coordinator.clone();
            let fence = crate::app_runtime::async_runtime::spawn(async move {
                fencing.fence_client_frames("graph-fence").await
            });
            tokio::task::yield_now().await;
            tokio::time::sleep(FRAME_WAIT).await;
            assert!(
                !fence.is_finished(),
                "the fence was raised while a frame was still in flight"
            );
            // The fence is already up for newcomers while it drains.
            assert!(
                tokio::time::timeout(FRAME_WAIT, coordinator.acquire_client_frame("graph-fence"))
                    .await
                    .is_err(),
                "a new frame was admitted behind a pending fence"
            );
            // A different graph is unaffected.
            tokio::time::timeout(
                std::time::Duration::from_secs(1),
                coordinator.acquire_client_frame("graph-other"),
            )
            .await
            .expect("another graph's frames are not fenced")
            .expect("frame lease");

            drop(in_flight);
            let fence = tokio::time::timeout(std::time::Duration::from_secs(1), fence)
                .await
                .expect("the fence completes once the frame is done")
                .expect("task")
                .expect("fence");
            assert!(
                tokio::time::timeout(FRAME_WAIT, coordinator.acquire_client_frame("graph-fence"))
                    .await
                    .is_err(),
                "a frame was admitted while the fence was held"
            );

            // The held-back frame resumes the moment the fence lifts.
            let waiting = coordinator.clone();
            let resumed = crate::app_runtime::async_runtime::spawn(async move {
                waiting
                    .acquire_client_frame("graph-fence")
                    .await
                    .map(|_| ())
            });
            tokio::task::yield_now().await;
            drop(fence);
            tokio::time::timeout(std::time::Duration::from_secs(1), resumed)
                .await
                .expect("the frame resumes after the fence")
                .expect("task")
                .expect("frame lease");
        });
    }
}
