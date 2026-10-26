use crate::app_runtime::AppHandle;
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
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
    /// Bumped on every successful engage. A restore-point capture that does not hold the graph lease for
    /// its whole walk reads it before and after: a changed epoch (or an active restore) means a restore ran
    /// between two of its documents, so the half-and-half result must be refused.
    epoch: AtomicU64,
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
        self.epoch.fetch_add(1, Ordering::AcqRel);
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

    /// Number of restores engaged so far in this process.
    pub(crate) fn epoch(&self) -> u64 {
        self.epoch.load(Ordering::Acquire)
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
    // The restore's own task: its saves are the restore, not a mutation racing it. Only the operation
    // that holds the guard is admitted, and a task it spawns does not inherit the scope.
    if let (Some(owner), Some(detail)) = (current_restore_owner(), state.current_detail()) {
        if owner == detail.operation_id {
            return Ok(());
        }
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

tokio::task_local! {
    /// The operation id of the restore this task is running, if any (the visitor fence's pattern,
    /// visitor_fence.rs:941-944).
    static RESTORE_OWNER: String;
}

/// The operation id of the restore the current task is the owner of, if any.
fn current_restore_owner() -> Option<String> {
    RESTORE_OWNER.try_with(Clone::clone).ok()
}

/// Run `future` as the owner of the active restore `operation_id`: inside it, `require_no_active_restore`
/// admits writes, because they are the restore's own. Every other task is still refused.
pub(crate) async fn as_restore_owner<F: std::future::Future>(
    operation_id: String,
    future: F,
) -> F::Output {
    RESTORE_OWNER.scope(operation_id, future).await
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
    fn epoch_counts_engagements_even_after_they_finish() {
        let state = Arc::new(RestoreGuardState::default());
        assert_eq!(state.epoch(), 0);
        drop(state.try_engage(detail()).expect("engage"));
        assert!(!state.is_active());
        assert_eq!(state.epoch(), 1, "a finished restore must still be visible");
        let _held = state.try_engage(detail()).expect("engage again");
        assert!(state.try_engage(detail()).is_err());
        assert_eq!(state.epoch(), 2, "a refused engage must not bump the epoch");
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

    /// A mock app in a throwaway profile, with the restore guard engaged by `operation_id` when given.
    fn with_guard_app(operation_id: Option<&str>, test: impl FnOnce(&AppHandle)) {
        let _serial = crate::tauri_runtime::profile_env_serial()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let previous = std::env::var_os("GARDEN_PROFILE_DIR");
        let profile =
            std::env::temp_dir().join(format!("garden-restore-guard-{}", uuid::Uuid::new_v4()));
        std::env::set_var("GARDEN_PROFILE_DIR", &profile);
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let app = crate::tauri_runtime::build_mock_app_for_tests(true);
            let state = app.state::<Arc<RestoreGuardState>>();
            let _lease = operation_id.map(|operation_id| {
                state
                    .inner()
                    .try_engage(RestoreGuardDetail {
                        graph_id: "g1".to_string(),
                        operation_id: operation_id.to_string(),
                        restore_point_id: "rp-1".to_string(),
                    })
                    .expect("engage the guard")
            });
            test(&app);
        }));
        match previous {
            Some(value) => std::env::set_var("GARDEN_PROFILE_DIR", value),
            None => std::env::remove_var("GARDEN_PROFILE_DIR"),
        }
        let _ = std::fs::remove_dir_all(&profile);
        if let Err(panic) = outcome {
            std::panic::resume_unwind(panic);
        }
    }

    #[test]
    fn owner_scope_admits_its_own_restore_only() {
        with_guard_app(Some("op-1"), |app| {
            let inside = crate::app_runtime::async_runtime::block_on(as_restore_owner(
                "op-1".to_string(),
                async { require_no_active_restore(app, "g1") },
            ));
            assert!(
                inside.is_ok(),
                "the restore's own task is admitted: {inside:?}"
            );

            // The scope ends with the future: the next caller is refused again.
            assert!(require_no_active_restore(app, "g1").is_err());

            // A task spawned from inside the scope is not the restore: task-locals do not propagate.
            let spawned = crate::app_runtime::async_runtime::block_on(as_restore_owner(
                "op-1".to_string(),
                async {
                    let app = app.clone();
                    crate::app_runtime::async_runtime::spawn(async move {
                        require_no_active_restore(&app, "g1")
                    })
                    .await
                    .expect("join the spawned task")
                },
            ));
            assert!(
                spawned.is_err(),
                "a spawned task does not inherit the owner scope"
            );
        });
    }

    #[test]
    fn outside_the_scope_is_refused() {
        with_guard_app(Some("op-1"), |app| {
            let error = require_no_active_restore(app, "g1")
                .expect_err("an unscoped caller is refused while a restore runs");
            assert!(error.contains("workspace mutation blocked"), "{error}");
            assert!(error.contains("operation op-1"), "{error}");
        });
    }

    #[test]
    fn no_active_restore_refuses_nobody() {
        with_guard_app(None, |app| {
            assert!(require_no_active_restore(app, "g1").is_ok());
        });
    }

    #[test]
    fn another_operation_id_is_refused() {
        with_guard_app(Some("op-1"), |app| {
            let other = crate::app_runtime::async_runtime::block_on(as_restore_owner(
                "op-2".to_string(),
                async { require_no_active_restore(app, "g1") },
            ));
            assert!(
                other.is_err(),
                "the owner of a different operation is not the owner of this restore"
            );
        });
    }
}
