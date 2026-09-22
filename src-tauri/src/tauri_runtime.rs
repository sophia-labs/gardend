use crate::app_runtime::{App, AppHandle};
use crate::{
    active_documents::ActiveDocumentRegistry,
    cell_graph_boundary::CellGraphBoundary,
    crdt_operation_journal::{prune_crdt_operation_journal, LOCAL_CRDT_JOURNAL_RETENTION_DAYS},
    crdt_queue::{recover_crdt_operations, CrdtOperationQueue},
    hosted_mode::{self, RuntimeMode},
    loopback_server::start_loopback_server,
    operation_completion_ledger::{
        prune_completion_ledger, LOCAL_COMPLETION_LEDGER_RETENTION_DAYS,
    },
    profile_lock::ProfileLock,
    profile_paths,
    restore_guard::RestoreGuardState,
    time_travel_interval_scheduler::start_interval_scheduler,
};
use std::{error::Error, time::Duration};
#[cfg(feature = "desktop")]
use tauri::Manager;

#[cfg(not(feature = "desktop"))]
#[derive(Default)]
struct HeadlessReadiness {
    result: std::sync::Mutex<Option<Result<(), String>>>,
    changed: tokio::sync::Notify,
}

#[cfg(not(feature = "desktop"))]
impl HeadlessReadiness {
    fn complete(&self, result: Result<(), String>) {
        *self
            .result
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(result);
        self.changed.notify_waiters();
    }

    async fn wait(&self) -> Result<(), String> {
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if let Some(result) = self
                .result
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone()
            {
                return result;
            }
            changed.await;
        }
    }
}

#[cfg(feature = "desktop")]
pub(crate) fn setup_native_app(app: &mut App) -> Result<(), Box<dyn Error>> {
    install_dev_logging(app)?;
    // Create the main window in code (not statically in tauri.conf.json) so the
    // persisted title-bar mode can drive the macOS title-bar style without a
    // first-frame flash. Must run before anything looks up the "main" window
    // (e.g. the runtime-mode bootstrap below).
    let title_bar = crate::window_settings::read_window_settings(app.handle())?.title_bar;
    crate::window_settings::build_main_window(app, title_bar)?;
    let mode = setup_core(app.handle())?;
    bootstrap_runtime_mode_globals(app.handle().clone(), mode);
    Ok(())
}

/// Runtime-agnostic core setup shared by the desktop app and the headless
/// `gardend` cell: profile lock, managed state, mode resolution, loopback
/// server, CRDT recovery/pruning, interval scheduler. No webview anywhere.
pub(crate) fn setup_core(handle: &AppHandle) -> Result<RuntimeMode, Box<dyn Error>> {
    // Parse, validate, and install the cell owner before recovery, schedulers,
    // or any graph/profile initialization can select durable graph state.
    let cell_graph = std::sync::Arc::new(CellGraphBoundary::from_process_env()?);
    let lock = acquire_profile_lock(handle)?;
    handle.manage(lock);
    register_managed_state(handle, cell_graph);

    let (config, recovered) = hosted_mode::read_and_recover(handle)?;
    if recovered {
        log::info!("hosted-mode pending state recovered on launch; reverted to local");
    }
    let mode = config.mode;
    log::info!("runtime mode resolved to {}", mode.as_str());

    if mode.is_local() {
        // Establish profile metadata synchronously. Local embedding mode also
        // initializes and validates its Omphalos constitution here; a remote
        // embeddings pool deliberately skips that local dependency.
        crate::profile_service::ensure_profile(handle)?;
        // Durable FIFO begins before the API is reachable: recovered journal
        // entries must already occupy the front of the managed queue before a
        // new loopback request can enqueue and spawn a drainer. A recovery
        // failure is therefore startup-fatal rather than an invitation to
        // process newer work ahead of an unknown durable prefix.
        recover_local_crdt_operations(handle.clone())?;
        prune_journal_on_startup(handle.clone());
        prune_completion_ledger_on_startup(handle.clone());
        #[cfg(feature = "frontend-crdt")]
        {
            // Explicit compatibility mode retains the frontend-driven
            // recovered-event/poll lifecycle.
            if let Err(error) = start_local_loopback(handle.clone()) {
                log::error!("Failed to start loopback API: {error}");
            }
            start_interval_scheduler(handle.clone());
        }
        #[cfg(all(not(feature = "frontend-crdt"), not(feature = "desktop")))]
        {
            // WS, metadata, and lifecycle routes bypass the CRDT queue but share
            // graph gates. Keep the entire API unexposed until the recovered
            // durable prefix has actually drained, not merely entered the queue.
            let lifecycle = handle
                .state::<std::sync::Arc<crate::cell_lifecycle::CellLifecycle>>()
                .inner()
                .clone();
            let bootstrap_lease = lifecycle
                .begin_background("headless-bootstrap")
                .expect("newly initialized cell accepts its bootstrap lease");
            let bootstrap_handle = handle.clone();
            let readiness = handle
                .state::<std::sync::Arc<HeadlessReadiness>>()
                .inner()
                .clone();
            crate::app_runtime::async_runtime::spawn(async move {
                let result = drain_recovered_then_start_cell_services(bootstrap_handle).await;
                readiness.complete(result);
                // Start the idle window only after recovered work has drained
                // and the API + scheduler have actually been exposed.
                drop(bootstrap_lease);
            });
        }
        #[cfg(all(not(feature = "frontend-crdt"), feature = "desktop"))]
        {
            // The normal desktop is the same in-process cell authority as
            // gardend. Shrubbery is a Yjs/API client; it never drains the
            // durable queue. Desktop has no idle reaper/readiness waiter, so
            // expose the loopback after recovery on the background runtime and
            // surface any startup error through the native log.
            let bootstrap_handle = handle.clone();
            crate::app_runtime::async_runtime::spawn(async move {
                if let Err(error) = drain_recovered_then_start_cell_services(bootstrap_handle).await
                {
                    log::error!("Failed to start the desktop cell API: {error}");
                }
            });
        }
    } else {
        log::info!(
            "hosted mode active — skipping loopback API, interval scheduler, native runtime drain"
        );
    }
    Ok(mode)
}

fn acquire_profile_lock(app: &AppHandle) -> Result<ProfileLock, String> {
    let profile_dir = profile_paths::profile_dir(app)?;
    ProfileLock::acquire(&profile_dir)
}

#[cfg(feature = "desktop")]
fn install_dev_logging(app: &mut App) -> Result<(), Box<dyn Error>> {
    if cfg!(debug_assertions) {
        app.handle().plugin(
            tauri_plugin_log::Builder::default()
                .level(log::LevelFilter::Info)
                .build(),
        )?;
    }
    Ok(())
}

fn register_managed_state(handle: &AppHandle, cell_graph: std::sync::Arc<CellGraphBoundary>) {
    handle.manage(cell_graph);
    let lifecycle = std::sync::Arc::new(crate::cell_lifecycle::CellLifecycle::new());
    crate::cell_lifecycle::install_process_lifecycle(&lifecycle);
    handle.manage(lifecycle);
    handle.manage(CrdtOperationQueue::default());
    handle.manage(ActiveDocumentRegistry::default());
    handle.manage(std::sync::Arc::new(RestoreGuardState::default()));
    handle.manage(crate::crdt_engine::rooms::RoomRegistry::default());
    #[cfg(not(feature = "desktop"))]
    handle.manage(std::sync::Arc::new(HeadlessReadiness::default()));
    handle.manage(
        crate::crdt_engine::persistence_coordinator::GraphPersistenceCoordinator::default(),
    );
    handle.manage(crate::sparql_admission::SparqlAdmission::from_env());
}

/// Test-only: build an isolated native headless app handle for in-crate tests.
///
/// `with_state=true` also registers the CRDT queue + room registry so
/// `enqueue_crdt_operation` drains through the in-process executor.
#[cfg(all(test, feature = "headless"))]
pub(crate) fn build_mock_app_for_tests(with_state: bool) -> AppHandle {
    let app = App::new();
    let handle = app.handle().clone();
    if with_state {
        register_managed_state(
            &handle,
            std::sync::Arc::new(CellGraphBoundary::for_test(None)),
        );
    }
    handle
}

#[cfg(all(test, feature = "headless"))]
pub(crate) fn build_mock_cell_app_for_tests(
    owner_graph_id: &str,
    owner_principal: &str,
    graph_generation: u64,
) -> AppHandle {
    let app = App::new();
    let handle = app.handle().clone();
    register_managed_state(
        &handle,
        std::sync::Arc::new(CellGraphBoundary::for_test_binding(
            owner_graph_id,
            owner_principal,
            graph_generation,
        )),
    );
    handle
}

/// Test-only: the ONE process-wide mutex for headless tests that set
/// `GARDEN_PROFILE_DIR` (a process-global env var) and write into a shared graph
/// dir. A per-module mutex is insufficient — different test modules would hold
/// different mutexes and still race the env var. Every such test must lock THIS
/// mutex so their profiles cannot stomp each other.
#[cfg(all(test, feature = "headless"))]
pub(crate) fn profile_env_serial() -> &'static std::sync::Mutex<()> {
    static T: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    T.get_or_init(|| std::sync::Mutex::new(()))
}

fn start_local_loopback(app_handle: AppHandle) -> Result<(), String> {
    let manifest = start_loopback_server(app_handle)?;
    log::info!(
        "Local API ready at {}; MCP at {}",
        manifest.api_url,
        manifest.mcp_url
    );
    Ok(())
}

fn recover_local_crdt_operations(app_handle: AppHandle) -> Result<usize, String> {
    let count = recover_crdt_operations(app_handle)?;
    if count > 0 {
        log::info!("Recovered {count} durable CRDT operation(s) for local drain");
    }
    Ok(count)
}

#[cfg(not(feature = "frontend-crdt"))]
async fn drain_recovered_then_start_cell_services(app_handle: AppHandle) -> Result<(), String> {
    crate::crdt_engine::executor::drain_queue(app_handle.clone()).await;
    #[cfg(not(feature = "desktop"))]
    if let Some(delay_ms) = std::env::var("GARDEN_LEASE_TEST_PRE_LOOPBACK_DELAY_MS")
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|value| *value > 0)
    {
        log::warn!("TEST ONLY: delaying {delay_ms}ms before loopback exposure");
        tokio::time::sleep(Duration::from_millis(delay_ms)).await;
    }
    #[cfg(not(feature = "desktop"))]
    refuse_loopback_exposure_under_invalid_lease()?;
    start_local_loopback(app_handle.clone())?;
    #[cfg(not(feature = "desktop"))]
    if let Some(delay_ms) = std::env::var("GARDEN_LEASE_TEST_POST_LOOPBACK_PRE_READY_DELAY_MS")
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|value| *value > 0)
    {
        log::warn!(
            "TEST ONLY: delaying {delay_ms}ms after loopback bind before readiness completion"
        );
        tokio::time::sleep(Duration::from_millis(delay_ms)).await;
    }
    start_interval_scheduler(app_handle);
    Ok(())
}

/// The renew task runs concurrently with recovered-queue drain. Its initial
/// boot verdict can therefore become stale before this task binds the
/// loopback socket. Make the lease check part of the service-exposure gate,
/// not merely a check in the outer Gardend orchestration.
#[cfg(all(not(feature = "frontend-crdt"), not(feature = "desktop")))]
fn refuse_loopback_exposure_under_invalid_lease() -> Result<(), String> {
    use crate::cell_lease::LeaseMode;

    let Some(lease) = crate::cell_lease::handle() else {
        return Ok(());
    };
    if lease.mode() == LeaseMode::Observe {
        return Ok(());
    }
    if lease.terminal_reason().is_some() || lease.is_fenced() || !lease.valid_with_margin() {
        log::error!(
            "refusing cell services before loopback exposure: write lease is terminal, fenced, \
             or invalid"
        );
        return Err(
            "write lease became terminal, fenced, or invalid before loopback exposure".into(),
        );
    }
    Ok(())
}

#[cfg(not(feature = "desktop"))]
pub(crate) async fn wait_for_headless_readiness(handle: &AppHandle) -> Result<(), String> {
    handle
        .state::<std::sync::Arc<HeadlessReadiness>>()
        .inner()
        .wait()
        .await
}

fn prune_journal_on_startup(app_handle: AppHandle) {
    match prune_crdt_operation_journal(&app_handle, LOCAL_CRDT_JOURNAL_RETENTION_DAYS) {
        Ok(0) => {}
        Ok(count) => log::info!("Pruned {count} expired CRDT journal event(s)"),
        Err(error) => log::warn!("CRDT journal prune failed (non-fatal): {error}"),
    }
}

fn prune_completion_ledger_on_startup(app_handle: AppHandle) {
    match prune_completion_ledger(&app_handle, LOCAL_COMPLETION_LEDGER_RETENTION_DAYS) {
        Ok(0) => {}
        Ok(count) => log::info!("Pruned {count} expired completion ledger entry/entries"),
        Err(error) => log::warn!("Completion ledger prune failed (non-fatal): {error}"),
    }
}

/// Inject the runtime-mode globals into the webview as soon as the main
/// window exists. Tokens are NEVER injected this way — the frontend
/// pulls them via the `get_hosted_credentials` Tauri command after boot.
#[cfg(feature = "desktop")]
fn bootstrap_runtime_mode_globals(app_handle: AppHandle, mode: RuntimeMode) {
    crate::app_runtime::async_runtime::spawn(async move {
        let mode_str = mode.as_str();
        let native_local = mode.is_local();
        for attempt in 1..=5 {
            tokio::time::sleep(Duration::from_millis(750 * attempt)).await;
            let Some(window) = app_handle.get_webview_window("main") else {
                continue;
            };
            let script = format!(
                r#"(() => {{
                  globalThis.__MN_RUNTIME_MODE__ = "{mode_str}";
                  globalThis.__MN_NATIVE_LOCAL__ = {native_local};
                }})();"#
            );
            if let Err(error) = window.eval(&script) {
                log::debug!("runtime-mode bootstrap eval attempt {attempt} failed: {error}");
                continue;
            }
            log::debug!("runtime-mode bootstrap injected on attempt {attempt} ({mode_str})");
            return;
        }
    });
}

#[cfg(test)]
mod startup_order_tests {
    #[test]
    fn boundary_is_installed_before_recovery_and_headless_bootstrap() {
        let source = include_str!("tauri_runtime.rs");
        let setup_start = source.find("pub(crate) fn setup_core").expect("setup_core");
        let setup_end = source[setup_start..]
            .find("fn acquire_profile_lock")
            .map(|offset| setup_start + offset)
            .expect("end of setup_core section");
        let setup = &source[setup_start..setup_end];
        let boundary = setup
            .find("CellGraphBoundary::from_process_env()")
            .expect("cell owner parse");
        let install = setup
            .find("register_managed_state(handle, cell_graph)")
            .expect("cell owner managed-state install");
        let recovery = setup
            .find("recover_local_crdt_operations(handle.clone())?")
            .expect("startup recovery call");
        let bootstrap = setup
            .find("drain_recovered_then_start_cell_services(bootstrap_handle).await")
            .expect("cell bootstrap call");
        assert!(
            boundary < install && install < recovery && recovery < bootstrap,
            "cell owner must be installed before recovery, which precedes headless bootstrap"
        );
    }

    #[cfg(not(feature = "frontend-crdt"))]
    #[test]
    fn cell_recovered_drain_completes_before_api_exposure() {
        let source = include_str!("tauri_runtime.rs");
        let helper_start = source
            .find("async fn drain_recovered_then_start_cell_services")
            .expect("cell bootstrap helper");
        let helper_end = source[helper_start..]
            .find("fn prune_journal_on_startup")
            .map(|offset| helper_start + offset)
            .expect("end of headless helper");
        let helper = &source[helper_start..helper_end];
        let drain = helper
            .find("executor::drain_queue(app_handle.clone()).await")
            .expect("awaited recovered drain");
        let exposure = helper
            .find("start_local_loopback(app_handle.clone())")
            .expect("API exposure");
        assert!(
            drain < exposure,
            "cell must finish the recovered prefix before API exposure"
        );
    }
}
