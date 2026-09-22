//! Cell-local activity and quiescence authority.
//!
//! A replicated gateway cannot infer graph-cell idleness from the requests one
//! replica happened to proxy. The cell can: every admitted request, upgraded
//! WebSocket, and registered background operation crosses this process. This
//! tracker makes admission and the `Running -> Draining` transition one atomic
//! decision under a single short-held lock.

use std::{
    collections::BTreeSet,
    sync::{Arc, Mutex, MutexGuard, OnceLock, Weak},
    time::{Duration, Instant},
};
use tokio::sync::Notify;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CellPhase {
    Running,
    Draining,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CellActivitySnapshot {
    pub phase: CellPhase,
    pub idle_for: Duration,
    pub in_flight_requests: usize,
    pub open_websockets: usize,
    pub background_jobs: usize,
    pub background_leases: usize,
}

impl CellActivitySnapshot {
    pub fn is_quiescent(&self) -> bool {
        self.in_flight_requests == 0
            && self.open_websockets == 0
            && self.background_jobs == 0
            && self.background_leases == 0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdmissionClosed;

impl std::fmt::Display for AdmissionClosed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("cell is draining")
    }
}

impl std::error::Error for AdmissionClosed {}

struct LifecycleState {
    phase: CellPhase,
    last_activity: Instant,
    in_flight_requests: usize,
    open_websockets: usize,
    background_jobs: BTreeSet<String>,
    background_leases: usize,
}

pub struct CellLifecycle {
    state: Mutex<LifecycleState>,
    changed: Notify,
}

impl Default for CellLifecycle {
    fn default() -> Self {
        Self::new()
    }
}

impl CellLifecycle {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(LifecycleState {
                phase: CellPhase::Running,
                last_activity: Instant::now(),
                in_flight_requests: 0,
                open_websockets: 0,
                background_jobs: BTreeSet::new(),
                background_leases: 0,
            }),
            changed: Notify::new(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, LifecycleState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub fn snapshot(&self) -> CellActivitySnapshot {
        self.snapshot_at(Instant::now())
    }

    fn snapshot_at(&self, now: Instant) -> CellActivitySnapshot {
        let state = self.lock();
        CellActivitySnapshot {
            phase: state.phase,
            idle_for: now.saturating_duration_since(state.last_activity),
            in_flight_requests: state.in_flight_requests,
            open_websockets: state.open_websockets,
            background_jobs: state.background_jobs.len(),
            background_leases: state.background_leases,
        }
    }

    pub fn is_draining(&self) -> bool {
        self.lock().phase == CellPhase::Draining
    }

    /// Admit one non-probe HTTP request. The check and increment share the same
    /// lock as `try_begin_idle_drain`, closing the accept-vs-drain race.
    pub fn admit_request(self: &Arc<Self>) -> Result<RequestLease, AdmissionClosed> {
        let mut state = self.lock();
        if state.phase == CellPhase::Draining {
            return Err(AdmissionClosed);
        }
        state.in_flight_requests += 1;
        state.last_activity = Instant::now();
        drop(state);
        self.changed.notify_waiters();
        Ok(RequestLease {
            lifecycle: Arc::clone(self),
        })
    }

    /// Acquire the socket lease before returning a WebSocket upgrade response.
    /// The request lease is still held at that point, so there is no uncounted
    /// interval between HTTP admission and the upgraded connection.
    pub fn open_websocket(self: &Arc<Self>) -> Result<WebSocketLease, AdmissionClosed> {
        let mut state = self.lock();
        if state.phase == CellPhase::Draining {
            return Err(AdmissionClosed);
        }
        state.open_websockets += 1;
        state.last_activity = Instant::now();
        drop(state);
        self.changed.notify_waiters();
        Ok(WebSocketLease {
            lifecycle: Arc::clone(self),
        })
    }

    /// Hold an anonymous internal-work lease (imports, projection work, or a
    /// periodic durability flush). New work is refused after draining begins.
    pub fn begin_background(
        self: &Arc<Self>,
        _name: &'static str,
    ) -> Result<BackgroundLease, AdmissionClosed> {
        let mut state = self.lock();
        // A signal can fence admission while an already-admitted request or
        // socket is still registering the durable work it owns. Let that
        // child work become visible to quiescence; idle-driven draining cannot
        // reach this branch because it requires both counters to be zero.
        if state.phase == CellPhase::Draining
            && state.in_flight_requests == 0
            && state.open_websockets == 0
        {
            return Err(AdmissionClosed);
        }
        state.background_leases += 1;
        state.last_activity = Instant::now();
        drop(state);
        self.changed.notify_waiters();
        Ok(BackgroundLease {
            lifecycle: Arc::clone(self),
            refresh_idle_on_drop: true,
        })
    }

    /// Hold internal housekeeping across the drain boundary without treating
    /// a periodic tick as user activity. Otherwise a 30-second durability
    /// flush would refresh a 15-minute idle TTL forever. Once the lease drops,
    /// an already-expired idle watcher can transition immediately.
    pub fn begin_maintenance(
        self: &Arc<Self>,
        _name: &'static str,
    ) -> Result<BackgroundLease, AdmissionClosed> {
        let mut state = self.lock();
        if state.phase == CellPhase::Draining {
            return Err(AdmissionClosed);
        }
        state.background_leases += 1;
        drop(state);
        self.changed.notify_waiters();
        Ok(BackgroundLease {
            lifecycle: Arc::clone(self),
            refresh_idle_on_drop: false,
        })
    }

    /// Register a durable local job by id. A set makes repeated observation of
    /// the same queued/running record idempotent instead of over-counting.
    pub fn job_started(&self, job_id: &str) -> Result<(), AdmissionClosed> {
        let mut state = self.lock();
        if state.phase == CellPhase::Draining
            && state.in_flight_requests == 0
            && state.open_websockets == 0
        {
            return Err(AdmissionClosed);
        }
        if state.background_jobs.insert(job_id.to_string()) {
            state.last_activity = Instant::now();
        }
        drop(state);
        self.changed.notify_waiters();
        Ok(())
    }

    pub fn job_finished(&self, job_id: &str) {
        let mut state = self.lock();
        if state.background_jobs.remove(job_id) {
            state.last_activity = Instant::now();
        }
        drop(state);
        self.changed.notify_waiters();
    }

    /// Atomically enter `Draining` iff the TTL has elapsed and every counted
    /// activity class is empty.
    pub fn try_begin_idle_drain(&self, now: Instant, idle_ttl: Duration) -> bool {
        let mut state = self.lock();
        if state.phase == CellPhase::Draining
            || state.in_flight_requests != 0
            || state.open_websockets != 0
            || !state.background_jobs.is_empty()
            || state.background_leases != 0
            || now.saturating_duration_since(state.last_activity) < idle_ttl
        {
            return false;
        }
        state.phase = CellPhase::Draining;
        drop(state);
        self.changed.notify_waiters();
        true
    }

    /// Signal-driven shutdown does not wait for the idle TTL, but it uses the
    /// same admission fence before waiting for already-admitted work.
    pub fn begin_draining(&self) -> bool {
        let mut state = self.lock();
        if state.phase == CellPhase::Draining {
            return false;
        }
        state.phase = CellPhase::Draining;
        drop(state);
        self.changed.notify_waiters();
        true
    }

    fn next_idle_wait(&self, now: Instant, idle_ttl: Duration) -> Option<Duration> {
        let state = self.lock();
        if state.phase == CellPhase::Draining {
            return Some(Duration::ZERO);
        }
        if state.in_flight_requests != 0
            || state.open_websockets != 0
            || !state.background_jobs.is_empty()
            || state.background_leases != 0
        {
            return None;
        }
        Some(idle_ttl.saturating_sub(now.saturating_duration_since(state.last_activity)))
    }

    pub async fn wait_until_idle(self: &Arc<Self>, idle_ttl: Duration) {
        loop {
            // Register before reading state so a change between the snapshot and
            // the await cannot become a lost wakeup. `notified()` alone is lazy;
            // enable the pinned waiter before taking the state snapshot.
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            let now = Instant::now();
            match self.next_idle_wait(now, idle_ttl) {
                Some(wait) if wait.is_zero() => {
                    if self.try_begin_idle_drain(now, idle_ttl) || self.is_draining() {
                        return;
                    }
                }
                Some(wait) => {
                    tokio::select! {
                        _ = &mut changed => {}
                        _ = tokio::time::sleep(wait) => {}
                    }
                }
                None => changed.await,
            }
        }
    }

    /// Wait for admitted work to finish after a signal-driven admission fence.
    /// Returns false on timeout so the caller can proceed to a forced final
    /// flush within Kubernetes' termination grace period.
    pub async fn wait_for_quiescence(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if self.snapshot().is_quiescent() {
                return true;
            }
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            tokio::select! {
                _ = &mut changed => {}
                _ = tokio::time::sleep_until(deadline.into()) => return self.snapshot().is_quiescent(),
            }
        }
    }

    fn request_finished(&self) {
        let mut state = self.lock();
        state.in_flight_requests = state.in_flight_requests.saturating_sub(1);
        state.last_activity = Instant::now();
        drop(state);
        self.changed.notify_waiters();
    }

    fn websocket_finished(&self) {
        let mut state = self.lock();
        state.open_websockets = state.open_websockets.saturating_sub(1);
        state.last_activity = Instant::now();
        drop(state);
        self.changed.notify_waiters();
    }

    fn background_finished(&self, refresh_idle: bool) {
        let mut state = self.lock();
        state.background_leases = state.background_leases.saturating_sub(1);
        if refresh_idle {
            state.last_activity = Instant::now();
        }
        drop(state);
        self.changed.notify_waiters();
    }
}

#[must_use = "the request remains active until this lease is dropped"]
pub struct RequestLease {
    lifecycle: Arc<CellLifecycle>,
}

impl Drop for RequestLease {
    fn drop(&mut self) {
        self.lifecycle.request_finished();
    }
}

#[must_use = "the WebSocket remains active until this lease is dropped"]
pub struct WebSocketLease {
    lifecycle: Arc<CellLifecycle>,
}

impl Drop for WebSocketLease {
    fn drop(&mut self) {
        self.lifecycle.websocket_finished();
    }
}

#[must_use = "background work remains active until this lease is dropped"]
pub struct BackgroundLease {
    lifecycle: Arc<CellLifecycle>,
    refresh_idle_on_drop: bool,
}

impl Drop for BackgroundLease {
    fn drop(&mut self) {
        self.lifecycle
            .background_finished(self.refresh_idle_on_drop);
    }
}

fn process_slot() -> &'static Mutex<Weak<CellLifecycle>> {
    static PROCESS: OnceLock<Mutex<Weak<CellLifecycle>>> = OnceLock::new();
    PROCESS.get_or_init(|| Mutex::new(Weak::new()))
}

pub(crate) fn install_process_lifecycle(lifecycle: &Arc<CellLifecycle>) {
    *process_slot()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Arc::downgrade(lifecycle);
}

pub(crate) fn process_lifecycle() -> Option<Arc<CellLifecycle>> {
    process_slot()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .upgrade()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_admission_and_idle_drain_are_atomic() {
        let lifecycle = Arc::new(CellLifecycle::new());
        let request = lifecycle.admit_request().expect("running accepts request");
        assert!(!lifecycle.try_begin_idle_drain(
            Instant::now() + Duration::from_secs(60),
            Duration::from_secs(30)
        ));
        drop(request);
        assert!(lifecycle.try_begin_idle_drain(
            Instant::now() + Duration::from_secs(31),
            Duration::from_secs(30)
        ));
        assert_eq!(lifecycle.snapshot().phase, CellPhase::Draining);
        assert!(lifecycle.admit_request().is_err());
    }

    #[test]
    fn websocket_holds_activity_through_an_otherwise_idle_window() {
        let lifecycle = Arc::new(CellLifecycle::new());
        let socket = lifecycle.open_websocket().expect("socket opens");
        assert!(!lifecycle.try_begin_idle_drain(
            Instant::now() + Duration::from_secs(600),
            Duration::from_secs(30)
        ));
        drop(socket);
        assert_eq!(lifecycle.snapshot().open_websockets, 0);
        assert!(lifecycle.try_begin_idle_drain(
            Instant::now() + Duration::from_secs(31),
            Duration::from_secs(30)
        ));
    }

    #[test]
    fn job_ids_are_idempotent_and_block_drain_until_finished() {
        let lifecycle = CellLifecycle::new();
        lifecycle.job_started("job-1").unwrap();
        lifecycle.job_started("job-1").unwrap();
        assert_eq!(lifecycle.snapshot().background_jobs, 1);
        assert!(!lifecycle.try_begin_idle_drain(
            Instant::now() + Duration::from_secs(600),
            Duration::from_secs(30)
        ));
        lifecycle.job_finished("job-1");
        assert_eq!(lifecycle.snapshot().background_jobs, 0);
    }

    #[tokio::test]
    async fn signal_drain_waits_for_existing_work_and_refuses_new_work() {
        let lifecycle = Arc::new(CellLifecycle::new());
        let background = lifecycle.begin_background("test").unwrap();
        assert!(lifecycle.begin_draining());
        assert!(lifecycle.admit_request().is_err());
        assert!(
            !lifecycle
                .wait_for_quiescence(Duration::from_millis(5))
                .await
        );
        drop(background);
        assert!(
            lifecycle
                .wait_for_quiescence(Duration::from_millis(50))
                .await
        );
    }

    #[test]
    fn signal_drain_allows_socket_owned_work_to_register_before_close() {
        let lifecycle = Arc::new(CellLifecycle::new());
        let socket = lifecycle.open_websocket().unwrap();
        assert!(lifecycle.begin_draining());

        let projection = lifecycle
            .begin_background("projection-flush")
            .expect("an admitted socket may publish its child work");
        lifecycle
            .job_started("socket-job")
            .expect("an admitted socket may publish its child job");

        drop(socket);
        assert!(lifecycle.begin_background("late-work").is_err());
        assert!(lifecycle.job_started("late-job").is_err());
        drop(projection);
        lifecycle.job_finished("socket-job");
        assert!(lifecycle.snapshot().is_quiescent());
    }

    #[test]
    fn maintenance_blocks_a_drain_but_does_not_extend_the_idle_window() {
        let lifecycle = Arc::new(CellLifecycle::new());
        let maintenance = lifecycle.begin_maintenance("test-maintenance").unwrap();
        assert!(!lifecycle.try_begin_idle_drain(
            Instant::now() + Duration::from_secs(60),
            Duration::from_secs(30)
        ));
        drop(maintenance);
        assert!(lifecycle.try_begin_idle_drain(
            Instant::now() + Duration::from_secs(60),
            Duration::from_secs(30)
        ));
    }
}
