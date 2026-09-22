use crate::app_runtime::AppHandle;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
#[cfg(feature = "desktop")]
use tauri::Manager;

/// Cooperative guard against concurrent CRDT mutations during a graph
/// restore. While `is_active` is set, every CRDT enqueue and direct
/// workspace/document save is rejected with `WorkspaceMutationBlocked`.
///
/// The guard is per-process. A single `RestoreGuardState` instance is
/// managed in app state at startup; restore execution acquires it via
/// `try_engage` and releases it via `disengage` on completion or rollback.
#[derive(Default, Debug)]
pub(crate) struct RestoreGuardState {
    is_active: AtomicBool,
    detail: Mutex<Option<RestoreGuardDetail>>,
}

#[derive(Debug, Clone)]
pub(crate) struct RestoreGuardDetail {
    pub graph_id: String,
    pub operation_id: String,
    pub restore_point_id: String,
}

#[derive(Debug)]
pub(crate) struct RestoreGuardLease {
    state: Arc<RestoreGuardState>,
}

impl Drop for RestoreGuardLease {
    fn drop(&mut self) {
        self.state.disengage();
    }
}

impl RestoreGuardState {
    pub(crate) fn try_engage(
        self: &Arc<Self>,
        detail: RestoreGuardDetail,
    ) -> Result<RestoreGuardLease, String> {
        if self
            .is_active
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            let current = self
                .detail
                .lock()
                .map_err(|_| "restore guard lock poisoned".to_string())?;
            let summary = current
                .as_ref()
                .map(|d| format!(" (graph {}, operation {})", d.graph_id, d.operation_id))
                .unwrap_or_default();
            return Err(format!("another restore is already in progress{summary}"));
        }
        let mut slot = self
            .detail
            .lock()
            .map_err(|_| "restore guard lock poisoned".to_string())?;
        *slot = Some(detail);
        drop(slot);
        Ok(RestoreGuardLease {
            state: Arc::clone(self),
        })
    }

    fn disengage(&self) {
        if let Ok(mut slot) = self.detail.lock() {
            *slot = None;
        }
        self.is_active.store(false, Ordering::Release);
    }

    pub(crate) fn is_active(&self) -> bool {
        self.is_active.load(Ordering::Acquire)
    }

    pub(crate) fn current_detail(&self) -> Option<RestoreGuardDetail> {
        self.detail.lock().ok().and_then(|slot| slot.clone())
    }
}

/// Reject any caller's write attempt while a restore is in progress.
/// Callers should propagate the error string verbatim — it carries enough
/// context (which graph, which operation) for the frontend to surface a
/// useful message.
pub(crate) fn require_no_active_restore(app: &AppHandle, _graph_id: &str) -> Result<(), String> {
    let state = app.state::<Arc<RestoreGuardState>>();
    if !state.is_active() {
        return Ok(());
    }
    let suffix = state
        .current_detail()
        .map(|detail| {
            format!(
                " on graph {} restoring {} (operation {})",
                detail.graph_id, detail.restore_point_id, detail.operation_id
            )
        })
        .unwrap_or_default();
    Err(format!(
        "workspace mutation blocked: time-travel restore in progress{suffix}"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn detail() -> RestoreGuardDetail {
        RestoreGuardDetail {
            graph_id: "g1".to_string(),
            operation_id: "op-1".to_string(),
            restore_point_id: "rp-1".to_string(),
        }
    }

    #[test]
    fn engage_blocks_second_engage_until_lease_drops() {
        let state = Arc::new(RestoreGuardState::default());
        let lease = state.try_engage(detail()).expect("first engage succeeds");
        assert!(state.is_active());
        let conflict = state
            .try_engage(detail())
            .expect_err("second engage must fail while a restore is already in progress");
        assert!(conflict.contains("another restore is already in progress"));
        drop(lease);
        assert!(!state.is_active());
        // After drop, a fresh engage works.
        let _lease2 = state.try_engage(detail()).expect("re-engage after drop");
    }

    #[test]
    fn detail_is_cleared_on_disengage() {
        let state = Arc::new(RestoreGuardState::default());
        let lease = state.try_engage(detail()).expect("engage");
        assert_eq!(state.current_detail().unwrap().graph_id, "g1");
        drop(lease);
        assert!(state.current_detail().is_none());
    }

    #[test]
    fn owned_lease_moves_with_blocking_restore_work() {
        let state = Arc::new(RestoreGuardState::default());
        let lease = state.try_engage(detail()).expect("engage");
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            entered_tx.send(()).expect("signal worker");
            release_rx.recv().expect("wait for release");
            drop(lease);
        });

        entered_rx.recv().expect("worker entered");
        assert!(state.is_active());
        release_tx.send(()).expect("release worker");
        worker.join().expect("worker exits");
        assert!(!state.is_active());
    }
}
