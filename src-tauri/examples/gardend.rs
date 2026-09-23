//! gardend — headless garden cell.
//!
//! Boots the full garden core (loopback REST/MCP server, storage, CRDT
//! queue, scheduler) on Garden's native headless runtime: no webview, no GUI
//! toolkit, and no Tauri dependency. This is the per-graph cell binary for
//! platform-next.
//!
//! Build: cargo build --no-default-features --features headless --example gardend
//!
//! Environment:
//!   GARDEN_PROFILE_DIR        profile directory (required in containers)
//!   GARDEN_DURABLE_DIR        durable NFS/EFS snapshot dir (enables hydrate/flush)
//!   GARDEN_FLUSH_INTERVAL_SECONDS  dirty-driven flush max-RPO ceiling (default 30)
//!   GARDEN_FLUSH_DEBOUNCE_SECONDS  dirty-driven flush debounce window (default 5)
//!   GARDEN_FLUSH_CAUSALITY_TRACE  bounded causal/subphase JSON on stderr (default off)
//!   GARDEN_IDLE_TTL_SECONDS   idle lifetime; 0/unset disables self-termination
//!   GARDEN_QUIESCE_TIMEOUT_SECONDS  signal drain wait (default 20)
//!   GARDEN_LOOPBACK_HOST      bind host (default 127.0.0.1; use 0.0.0.0 in pods)
//!   GARDEN_LOOPBACK_PORT      bind port (default 0 = OS-assigned)
//!   GARDEN_LOOPBACK_TOKEN     fixed bearer token (default: random per run)
//!   SOPHIA_OBSERVATORY_CAPTURE_ENABLED  enable stdout CaptureEvent testimony (default false)
//!   SOPHIA_OBSERVATORY_CONTRACT_BUNDLE_SHA256  exact ratified contract identity (required when enabled)
//!   GARDEN_CELL_GRAPH_ID      public graph name (gateway-provided in Cloud-2)
//!   GARDEN_CELL_ID            opaque physical generation; when present, arms
//!                             the owner-scoped registry/boundary preflight
//!   GARDEN_CELL_OWNER         stable typed owner for an owner-scoped cell
//!   GARDEN_CELL_GRAPH_GENERATION / GARDEN_CELL_REGISTRY_REVISION
//!                             canonical registry fencing coordinates
//!   GARDEN_CELL_MACHINE_ID    stable `cell:{cell_id}` Machine identity for
//!                             owner-scoped cells; legacy cells use
//!                             `cell:{graph_id}`
//!   GARDEN_CELL_MACHINE_RUN_ID  gateway-minted incarnation ULID
//!   GARDEN_DURABLE_EPOCH      U8 cross-process write-lease epoch, minted by
//!                             the gateway at claim time; absent = this cell
//!                             runs legacy-unfenced (see `cell_lease` for the
//!                             full boot-decision table, spec §3.2)
//!   GARDEN_LEASE_URL          gateway lease-authority base URL
//!                             (".../internal/lease")
//!   GARDEN_LEASE_MODE         observe|enforce (default observe — never
//!                             refuses, testifies would-have-fenced outcomes)
//!   GARDEN_LEASE_LAST_SNAP    claim-time boot-repair input (spec §3.6);
//!   GARDEN_LEASE_PENDING_SNAP absent LAST_SNAP means repair is skipped
//!                             entirely (a pre-lease gateway spawned us)
//!   GARDEN_LEASE_RENEW_MS / GARDEN_LEASE_MARGIN_MS /
//!   GARDEN_LEASE_FENCED_MAX_MS / GARDEN_LEASE_PUBLISH_TIMEOUT_MS
//!                             lease timing tunables (defaults 5000/8000/
//!                             300000/5000ms — see `cell_lease`)
//!   GARDEN_OBSERVATORY_BUNDLE_DIR  override for where the boot-hook awaits
//!                             the projector's bundle; defaults to a fixed
//!                             subdir of GARDEN_DURABLE_DIR (see
//!                             `observatory::boot::bundle_dir`). The A4
//!                             boot-hook itself is armed only when
//!                             GARDEN_CELL_GRAPH_ID == "observatory" AND a
//!                             CURRENT marker exists at that bundle dir
//!                             (RE-SCOPE r1: no pod env var arms this — see
//!                             `observatory::boot` for the full gate)
//!   GARDEN_OBSERVATORY_REFRESH_INTERVAL_SECONDS  hot-cell periodic
//!                             re-check cadence for a fresh projector
//!                             generation (default 300s; see
//!                             `observatory::boot::refresh_interval`)
//!   RUST_LOG                  log filter (default info)

/// A boot-repair invariant failure is deterministic and needs an operator to
/// choose authority. It must not share exit(4) with ordinary lease loss,
/// because the gateway is allowed to replace an exit(4) incarnation.
#[cfg(not(feature = "desktop"))]
const BOOT_REPAIR_REQUIRED_EXIT_CODE: i32 = 7;

#[cfg(feature = "desktop")]
fn main() {
    eprintln!(
        "gardend must be built headless: cargo build --no-default-features --features headless --example gardend"
    );
    std::process::exit(1);
}

/// `gardend --version [--json]`, `gardend --check-update`, `gardend update`.
/// Returns `None` when no such command was given (serve as usual). These
/// commands never start the engine and never touch a profile.
#[cfg(not(feature = "desktop"))]
fn run_command(args: &[String]) -> Option<i32> {
    use garden_lib::self_update::{self as update, UpdateOutcome};
    let has = |flag: &str| args.iter().any(|a| a == flag);
    if has("--version") || has("-V") {
        if has("--json") {
            println!("{}", update::version_json());
        } else {
            println!(
                "gardend {} (storage format {})",
                update::CURRENT_VERSION,
                garden_lib::storage_format::STORAGE_FORMAT
            );
        }
        return Some(0);
    }
    let check_only = has("--check-update") || (has("update") && has("--check"));
    if !check_only && args.first().map(String::as_str) != Some("update") {
        if let Some(unknown) = args.first() {
            // gardend has always ignored argv; keep serving, but say so.
            eprintln!(
                "gardend: ignoring argument {unknown:?} (configure via GARDEN_* env; \
                 commands: --version [--json], --check-update, update)"
            );
        }
        return None;
    }
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(error) => {
            eprintln!("gardend: cannot locate the running binary: {error}");
            return Some(1);
        }
    };
    let profile = update::configured_profile_dir();
    let current = update::CURRENT_VERSION;
    let base = update::release_base();
    match update::run_update(&base, &exe, current, check_only, profile.as_deref()) {
        Ok(UpdateOutcome::UpToDate { latest }) => {
            println!("gardend {current} is up to date (latest release: {latest})");
            Some(0)
        }
        Ok(UpdateOutcome::Available {
            latest,
            storage_format_ok,
        }) => {
            println!("{}", update::notice(&latest, current));
            if let Err(why) = storage_format_ok {
                println!("note: {why}");
            }
            Some(0)
        }
        Ok(UpdateOutcome::Installed { latest, path }) => {
            println!(
                "gardend updated {current} -> {latest} at {}",
                path.display()
            );
            Some(0)
        }
        Err(error) => {
            eprintln!("gardend update: {error}");
            Some(1)
        }
    }
}

#[cfg(not(feature = "desktop"))]
fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Some(code) = run_command(&args) {
        std::process::exit(code);
    }
    serve();
}

#[cfg(not(feature = "desktop"))]
fn serve() {
    use garden_lib::headless::{
        durability::{
            current_write_epoch, DirtyFlushScheduler, FlushTrigger, HydrateMode, SchedulerAction,
        },
        testimony::{
            install_process_writer, BootFailureStage, BootMode, ErrorCode, FailedFinalFlush,
            FlushMode, SnapshotId, StopReason, SuccessfulFinalFlush,
        },
    };
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    // fd 1 is reserved for canonical CaptureEvent NDJSON. Every ordinary
    // diagnostic, including env_logger output, stays on fd 2.
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .target(env_logger::Target::Stderr)
        .init();

    // Bare-binary installs learn about new releases from one log line, at
    // most once a day, off the boot path. Off in platform cells (they update
    // by image), in CI, and with GARDEN_NO_UPDATE_CHECK=1.
    let _ = std::thread::Builder::new()
        .name("gardend-update-check".into())
        .spawn(|| {
            if let Some(line) = garden_lib::self_update::startup_check() {
                log::info!("{line}");
            }
        });

    let profile_dir = std::env::var("GARDEN_PROFILE_DIR")
        .ok()
        .map(|value| PathBuf::from(value.trim()))
        .filter(|path| !path.as_os_str().is_empty());
    let durable_dir = std::env::var("GARDEN_DURABLE_DIR")
        .ok()
        .map(|value| PathBuf::from(value.trim()))
        .filter(|path| !path.as_os_str().is_empty());
    let testimony = match install_process_writer() {
        Ok(writer) => writer,
        Err(error) => {
            log::error!("cannot establish the CaptureEvent process witness: {error}");
            std::process::exit(5);
        }
    };
    let boot_started = Instant::now();

    // The registry/ACL boundary precedes every storage effect, including
    // hydrate. Accessing an unknown or tombstoned tuple therefore cannot create
    // a profile directory or restore bytes as a side effect.
    if let Err(error) = garden_lib::headless::preflight_cell_registry() {
        log::error!("cell registry preflight failed: {error}");
        testify_startup_failure(
            &testimony,
            BootFailureStage::Setup,
            ErrorCode::CoreSetupFailed,
            boot_started.elapsed(),
            6,
        );
    }

    log::info!(
        "gardend starting (profile dir: {}, durable dir: {})",
        profile_dir
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| "<tauri app-data default>".into()),
        durable_dir
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| "<none>".into()),
    );

    // Build the tokio runtime explicitly with a worker-thread FLOOR and a
    // generous blocking pool, and install it BEFORE anything touches the async
    // runtime (garden_lib::headless::async_runtime::set panics once the runtime is
    // initialized). On a small-core node the default pool would be tiny; a
    // heavy import plus its per-write blocking DB joins can then starve the
    // workers serving /health — the readiness probe fails and the gateway 503s
    // the cell mid-import. A floor of >=6 workers keeps /health schedulable.
    //
    // Built HERE — before hydrate, moved up from this module's historical
    // position after it — because U8-10's boot-time write-lease dance (start
    // the renew task, wait out whatever this mode's protocol requires, run
    // boot-time repair; spec §3.2/§3.6) is async and must complete strictly
    // before `hydrate_detailed` ever runs: a hydrate against an unrepaired
    // `CURRENT` could restore a quarantine-bound zombie snapshot into the
    // live profile.
    let worker_threads = std::env::var("GARDEN_TOKIO_WORKERS")
        .ok()
        .and_then(|value| value.trim().parse::<usize>().ok())
        .filter(|n| *n > 0)
        .unwrap_or_else(|| {
            std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(2)
                .max(6)
        });
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(worker_threads)
        .max_blocking_threads(128)
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            log::error!("build gardend tokio runtime: {error}");
            testify_startup_failure(
                &testimony,
                BootFailureStage::Setup,
                ErrorCode::CoreSetupFailed,
                boot_started.elapsed(),
                2,
            );
        }
    };
    // Leak: the runtime must outlive every spawned task and lives until the
    // process is killed.
    let runtime: &'static tokio::runtime::Runtime = Box::leak(Box::new(runtime));
    garden_lib::headless::async_runtime::set(runtime.handle().clone());
    log::info!("tokio runtime: {worker_threads} workers, max 128 blocking threads");

    // The boot-time write-lease dance (`cell_lease::init` → wait → boot
    // repair, U8-10) then hydrate. The detailed hydrate result preserves the
    // durable snapshot identity and exact copied bytes while the legacy
    // bool-returning API remains available to other callers. Desktop and
    // legacy-unfenced boots fall straight through to hydrate, byte-for-byte
    // as before this dance existed.
    let hydrate_started = Instant::now();
    let hydrate = garden_lib::headless::async_runtime::block_on(boot_lease_dance_and_hydrate(
        &testimony,
        boot_started,
        profile_dir.as_deref(),
        durable_dir.as_deref(),
    ));
    let hydrate_duration = hydrate_started.elapsed();
    if hydrate.mode == HydrateMode::Restored {
        log::info!("durable hydrate complete");
    }
    // TEST ONLY: widen the post-hydrate/pre-setup boundary so the real
    // process suite can deterministically deliver a renew 409 in this
    // otherwise narrow window. Production pod specs never set this.
    if let Some(delay_ms) = std::env::var("GARDEN_LEASE_TEST_POST_HYDRATE_DELAY_MS")
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|value| *value > 0)
    {
        log::warn!("TEST ONLY: delaying {delay_ms}ms after hydrate before the enforce lease guard");
        std::thread::sleep(std::time::Duration::from_millis(delay_ms));
    }
    enforce_lease_boot_guard(&testimony, boot_started, "after hydrate, before core setup");

    // DURABILITY AUDIT FINDING (2026-07-18): capture the dirty-scheduler's
    // clean baseline HERE — before `garden_lib::headless::setup` below starts
    // the loopback server and CRDT recovery, i.e. before ANYTHING can accept
    // a write. `current_write_epoch` is a plain global counter (meaningful
    // from process start regardless of whether durable dirs are configured
    // yet), so reading it this early costs nothing and is always valid.
    //
    // Capturing it later (originally: at `DirtyFlushScheduler::new(...)`,
    // deep inside the `durable` block below) left a real window — the server
    // is already accepting requests and recovery may already be replaying
    // unflushed operations from before a restart — during which a write
    // could land, bump the epoch, and then get silently baked into the
    // scheduler's "nothing pending" baseline: it would never be scheduled for
    // a flush unless some LATER write happened to bump the epoch again
    // (unbounded RPO for that write if nothing else ever touched the graph
    // again). Capturing early also means CRDT recovery's own writes are
    // correctly tracked as pending from the start, which is desirable: they
    // are exactly the kind of "survived a restart, get it durable again
    // promptly" work the scheduler exists to bound.
    let boot_write_epoch = garden_lib::headless::durability::current_write_epoch();

    let setup_started = Instant::now();

    let app = garden_lib::headless::new_app();

    let mode = match garden_lib::headless::setup(app.handle()) {
        Ok(mode) => mode,
        Err(error) => {
            log::error!("gardend core setup failed: {error}");
            testify_startup_failure(
                &testimony,
                BootFailureStage::Setup,
                ErrorCode::CoreSetupFailed,
                boot_started.elapsed(),
                2,
            );
        }
    };
    if !mode.is_local() {
        log::error!("gardend requires local runtime mode; hosted mode is not meaningful in a cell");
        testify_startup_failure(
            &testimony,
            BootFailureStage::Setup,
            ErrorCode::InvalidRuntimeMode,
            boot_started.elapsed(),
            2,
        );
    }
    // Observatory boot-hook (A4, garden half — see `src/observatory/boot.rs`
    // for the gate/read/apply detail this call wraps). r1: run to completion
    // (bounded — see `boot.rs`'s own `apply_budget`/`bundle_wait_budget`)
    // INLINE, strictly BEFORE `wait_until_ready` below ever exposes the
    // loopback API to real traffic — never a detached background task. §5
    // A4's own acceptance gate ("gardend boot materializes both graphs; POST
    // /api/sparql/query returns fresh obs:projectedThrough") can only hold if
    // nothing can reach the API before catch_up has run (or bounded-skipped);
    // the previous shape spawned this detached, so the wake-up freshness
    // query could race the still-running catch_up and read stale/absent
    // projections.
    //
    // Gated by `projector_gate_open` BEFORE touching anything else in this
    // block, so for every cell that is not the observatory cell (every
    // production graph this SAME binary also boots, e.g. `angels`) the ONLY
    // thing that runs is that one cheap graph_id check (short-circuits
    // before any I/O): no lease, no early durable-dir registration, no
    // `run_boot_hook_for_cell` call (`existing_graph_dir`/`open_graph_store`
    // are never reached), no boot delay — every other cell's boot sequence
    // below (readiness, the periodic-flush/idle loop, durable-dir
    // registration) is byte-for-byte unchanged from before this fix. For the
    // observatory graph_id itself, `projector_gate_open` additionally reads
    // one small `CURRENT` marker file (RE-SCOPE r1 — no pod env var arms
    // this gate; see `observatory::boot`'s module doc) — still cheap, never
    // the bounded multi-second bundle wait.
    let lifecycle = garden_lib::headless::lifecycle::tracker(app.handle());
    let observatory_cell_graph_id = std::env::var("GARDEN_CELL_GRAPH_ID").unwrap_or_default();
    // Cloned (Tauri's AppHandle is cheaply Clone) so the hot-cell periodic
    // re-check task spawned further below — inside the later async block,
    // well after `app` itself goes out of scope — has its own handle.
    // `observatory_gate_open_at_boot` mirrors the SAME check that guards
    // this whole branch, computed once and carried forward rather than
    // re-derived with different inputs later.
    let observatory_app_handle = app.handle().clone();
    let observatory_gate_open_at_boot =
        garden_lib::observatory::boot::projector_gate_open(&observatory_cell_graph_id);
    if observatory_gate_open_at_boot {
        // Register durability FIRST (before catch_up ever touches the
        // store): a reap/SIGTERM racing this call sees a fully-wired durable
        // plane from the start, not one only registered later at the
        // periodic-flush setup further below. `set_durable_dirs` is a
        // first-call-wins `OnceLock` (see `cell_durability.rs`), so the
        // unconditional call further below (unchanged, still runs for every
        // durable-configured cell) is a harmless no-op when this branch
        // already registered the same paths.
        let observatory_durable_dirs = match (profile_dir.as_ref(), durable_dir.as_ref()) {
            (Some(profile), Some(durable)) => {
                garden_lib::headless::durability::set_durable_dirs(
                    profile.clone(),
                    durable.clone(),
                );
                Some((profile.clone(), durable.clone()))
            }
            _ => None,
        };

        // Hold a lifecycle background-maintenance lease across catch-up and
        // its forced publish: a signal/idle drain that raced this call would
        // see non-quiescent state and correctly wait. Structurally this
        // cannot race today (the shutdown wait-loop below has not even
        // started yet — signal handling itself is not installed until
        // `wait_for_shutdown_signal` runs, further down), but the lease makes
        // that invariant self-enforcing rather than merely true by the
        // current placement of this code.
        let observatory_lease = lifecycle.begin_maintenance("observatory-catch-up").ok();

        use garden_lib::observatory::boot::{run_boot_hook_for_cell, BootHookOutcome};
        match garden_lib::headless::async_runtime::block_on(run_boot_hook_for_cell(
            app.handle(),
            &observatory_cell_graph_id,
        )) {
            BootHookOutcome::GateClosed => {
                // Unreachable in practice (the `if` above already proved the
                // gate open moments ago) — kept so this match stays
                // exhaustive without a wildcard arm silently swallowing a
                // future variant.
            }
            BootHookOutcome::BundleNotReady { waited } => {
                log::warn!(
                    "observatory boot-hook: no ready bundle within {}s — this boot skips catch_up cleanly",
                    waited.as_secs()
                );
            }
            BootHookOutcome::AlreadyCurrent { token } => {
                // Unreachable at a fresh process boot (the hot-cell
                // short-circuit's "last applied" state starts empty every
                // incarnation) — kept so this match stays exhaustive; the
                // periodic re-check task below is where this arm actually
                // fires in production.
                log::info!("observatory boot-hook: generation {token} already applied");
            }
            BootHookOutcome::GenerationChangedDuringRead { attempted_token } => {
                // A second projector run published a newer generation in the
                // narrow window between this boot's readiness wait and its
                // read — benign; the periodic re-check task below picks up
                // the fresh generation on its next tick.
                log::info!(
                    "observatory boot-hook: generation {attempted_token} was superseded mid-read \
                     during boot — not applied; the periodic re-check will pick up the fresh one"
                );
            }
            BootHookOutcome::InProgress { token } => {
                // Unreachable at a fresh process boot (single-flight state
                // starts empty every incarnation, same as `AlreadyCurrent`
                // above) — kept so this match stays exhaustive; the periodic
                // re-check task below joins whatever this leaves in flight.
                log::warn!(
                    "observatory boot-hook: apply of generation {token} still in flight past its \
                     own budget — this boot proceeds; the periodic re-check will join it"
                );
            }
            BootHookOutcome::Applied(report) => {
                log::info!(
                    "observatory boot-hook: catch_up applied on boot — raw +{}/-{}, rollups +{}/-{}",
                    report.raw.added,
                    report.raw.removed,
                    report.rollups.added,
                    report.rollups.removed,
                );
                // Force an EFS flush immediately after a successful apply —
                // publication must not wait for the next periodic 30s tick;
                // a reap moments after boot must not lose a catch_up that
                // already committed to the in-memory store. Reuses the SAME
                // `run_flush_blocking`/`testify_flush_result` helpers the
                // periodic/final flush call below use — no new flush/
                // testimony code invented here. `FlushMode::Periodic` (not
                // `Final`) is the honest choice: this is neither of the two
                // flush moments that enum was defined for (the 30s tick, the
                // terminal shutdown flush), and adding a third variant would
                // mean editing `capture_event.rs`, outside this fix's scope.
                if let Some((profile, durable)) = &observatory_durable_dirs {
                    let flush_started = Instant::now();
                    let flush_result = garden_lib::headless::async_runtime::block_on(run_flush_blocking(
                        profile,
                        durable,
                        FlushTrigger::ObservatoryApply,
                    ));
                    testify_flush_result(
                        &testimony,
                        FlushMode::Periodic,
                        flush_started.elapsed(),
                        &flush_result,
                    );
                    match flush_result {
                        Ok(_) => log::info!(
                            "observatory boot-hook: forced durable flush after catch_up succeeded"
                        ),
                        Err(error) => log::error!(
                            "observatory boot-hook: forced durable flush after catch_up failed: {error}"
                        ),
                    }
                }
            }
            BootHookOutcome::Failed(error) => {
                log::error!("observatory boot-hook failed (cell boot unaffected): {error}");
            }
        }
        drop(observatory_lease);
    }

    // Warm-open this cell's own graph store BEFORE the durable flusher can
    // take its first exclusive lifecycle-gate window: the store cache's hit
    // path is deliberately gate-free, so this one open keeps the whole
    // SPARQL/Emporium lane serving through boot flushes. Without it, the
    // first post-hydration query blocks inside `open_graph_store` for the
    // entire boot-flush plain-file walk (observed 20+ minutes on a
    // 41k-file, 2.2 GB profile) while holding its admission permit and
    // graph persistence lease, starving every later caller. Non-fatal: a
    // cell without a resolvable graph dir simply pays the gate on first use.
    if !observatory_cell_graph_id.is_empty() {
        match garden_lib::headless::warm_open_cell_graph_store(
            app.handle(),
            &observatory_cell_graph_id,
        ) {
            Ok(()) => log::info!(
                "cell graph store warm-opened for {observatory_cell_graph_id}; \
                 RDF reads stay gate-free through boot flushes"
            ),
            Err(error) => log::warn!(
                "cell graph store warm-open failed for {observatory_cell_graph_id} \
                 (first query pays the lifecycle gate instead): {error}"
            ),
        }
    }

    enforce_lease_boot_guard(
        &testimony,
        boot_started,
        "after core setup, before API readiness",
    );
    if let Err(error) =
        garden_lib::headless::async_runtime::block_on(garden_lib::headless::wait_until_ready(app.handle()))
    {
        enforce_lease_boot_guard(&testimony, boot_started, "while waiting for API readiness");
        log::error!("gardend API setup failed: {error}");
        testify_startup_failure(
            &testimony,
            BootFailureStage::Setup,
            ErrorCode::CoreSetupFailed,
            boot_started.elapsed(),
            2,
        );
    }
    enforce_lease_boot_guard(
        &testimony,
        boot_started,
        "after API readiness, before boot-ready testimony",
    );
    let setup_duration = setup_started.elapsed();
    let durable_configured = profile_dir.is_some() && durable_dir.is_some();
    let (boot_mode, snapshot_id, hydrated_bytes) = if durable_configured {
        let mode = match hydrate.mode {
            HydrateMode::Fresh => BootMode::Fresh,
            HydrateMode::Restored => BootMode::Restored,
            HydrateMode::WarmProfile => BootMode::WarmProfile,
        };
        (
            mode,
            hydrate
                .snapshot_id
                .and_then(|value| SnapshotId::new(value).ok()),
            hydrate.hydrated_bytes,
        )
    } else {
        (BootMode::NoDurablePlane, None, None)
    };
    testimony.emit_boot_ready(
        boot_mode,
        boot_started.elapsed(),
        hydrate_duration,
        setup_duration,
        snapshot_id,
        hydrated_bytes,
    );

    // No GUI event loop in headless mode — park the main thread while the
    // tokio runtime (loopback server, scheduler, queue) does the work.
    let lifecycle_testimony = testimony.clone();
    let shutdown_result: Result<(), String> = garden_lib::headless::async_runtime::block_on(async move {
        let idle_ttl = configured_idle_ttl();
        let quiesce_timeout = configured_quiesce_timeout();
        match idle_ttl {
            Some(ttl) => log::info!("cell-local idle shutdown enabled: {}s", ttl.as_secs()),
            None => log::info!("cell-local idle shutdown disabled"),
        }

        // Durable plane is active only when both dirs are configured.
        let durable = match (profile_dir, durable_dir) {
            (Some(profile), Some(durable)) => Some((profile, durable)),
            _ => None,
        };

        let Some((profile_dir, durable_dir)) = durable else {
            let reason = wait_for_shutdown(&lifecycle, idle_ttl).await;
            let reason =
                drain_admitted_work(&lifecycle, &lifecycle_testimony, reason, quiesce_timeout)
                    .await;
            lifecycle_testimony
                .emit_terminal_succeeded(reason.testimony(), SuccessfulFinalFlush::NotConfigured);
            log::info!("gardend shutting down ({})", reason.label());
            return Ok(());
        };

        // Register the dirs so a finished import can flush on its own thread
        // (cell_durability::ImportGuard) without threading paths through the
        // executor.
        garden_lib::headless::durability::set_durable_dirs(
            profile_dir.clone(),
            durable_dir.clone(),
        );

        // Dirty-driven, not a fixed tick: a flush is attempted shortly after
        // real activity (debounced, coalescing bursts), targeting a max-RPO
        // ceiling so worst-case crash loss stays close to what the old fixed
        // interval already bounded. An idle cell (the global write epoch
        // never advances — see `current_write_epoch`) never reaches
        // `SchedulerAction::FlushNow`, so it pays no periodic
        // RDF-store-enumeration or tree-walk cost at all while quiescent.
        // `GARDEN_FLUSH_INTERVAL_SECONDS` keeps its name/default (30) for
        // deploy/back-compat continuity, but its meaning is now "max RPO
        // target", not "unconditional tick".
        //
        // NOT A HARD REAL-TIME GUARANTEE (corrected 2026-07-18, re-refute
        // #3, finding 2(a) — an earlier version of this comment overstated
        // it as one): `flush_serial` (serializing every flush in the
        // process) has no timeout, and the forced write-gate wait
        // (`FORCED_FLUSH_GATE_WAIT`, 20s) only bounds ACQUIRING the gate,
        // not how long a prior, still-running flush or the checkpoint/copy
        // work itself takes. Under sustained contention or an unusually
        // slow durable-mount round trip, a pending write can wait longer
        // than `max_rpo_secs` before actually landing durably. This is a
        // pre-existing characteristic of the flush design (the old fixed
        // ticker had the identical `flush_serial`/gate-wait exposure) that
        // this scheduler does not worsen and cannot, by itself, turn into a
        // hard bound without a larger redesign (bounded/preemptible I/O).
        // Treat `max_rpo_secs` as the target latency under normal operating
        // conditions, not a provable ceiling.
        let debounce_secs = std::env::var("GARDEN_FLUSH_DEBOUNCE_SECONDS")
            .ok()
            .and_then(|value| value.trim().parse::<u64>().ok())
            .filter(|secs| *secs > 0)
            .unwrap_or(5);
        let max_rpo_secs = std::env::var("GARDEN_FLUSH_INTERVAL_SECONDS")
            .ok()
            .and_then(|value| value.trim().parse::<u64>().ok())
            .filter(|secs| *secs > 0)
            .unwrap_or(30);
        log::info!(
            "durable flush enabled: dirty-driven (debounce {debounce_secs}s, max RPO {max_rpo_secs}s)"
        );

        let (periodic_stop, mut periodic_stopped) = tokio::sync::oneshot::channel::<()>();
        let periodic = {
            let profile_dir = profile_dir.clone();
            let durable_dir = durable_dir.clone();
            let lifecycle = std::sync::Arc::clone(&lifecycle);
            let testimony = lifecycle_testimony.clone();
            garden_lib::headless::async_runtime::spawn(async move {
                // Cheap atomic-load cadence to check whether anything is
                // pending; unrelated to the debounce/max-RPO *values* above,
                // which are logical deadlines the scheduler measures against
                // wall-clock `Instant`s, not tick counts. Small relative to
                // the smallest realistic debounce so quantization error stays
                // negligible.
                const POLL_INTERVAL: Duration = Duration::from_millis(500);

                // `boot_write_epoch`, NOT a fresh `current_write_epoch()` read
                // here: this baseline was captured before the server started
                // accepting requests (see its declaration site) precisely so
                // this constructor cannot swallow a write that landed in the
                // startup/recovery window as already-clean.
                let mut scheduler = DirtyFlushScheduler::new(
                    Duration::from_secs(debounce_secs),
                    Duration::from_secs(max_rpo_secs),
                    boot_write_epoch,
                );
                let mut ticker = tokio::time::interval(POLL_INTERVAL);
                ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                // First tick fires immediately; skip it so we don't act before
                // the tree we just hydrated has settled.
                ticker.tick().await;
                loop {
                    tokio::select! {
                        _ = &mut periodic_stopped => return,
                        _ = ticker.tick() => {}
                    }
                    // `FlushAtRpoCeiling` MUST force the attempt: an ordinary
                    // (non-forced) flush unconditionally defers while an
                    // import is active, which would otherwise let the max-RPO
                    // bound this scheduler exists to guarantee be silently
                    // broken by a concurrent acknowledged write sitting
                    // unpublished for an entire ~90s import. See
                    // `SchedulerAction::FlushAtRpoCeiling`'s doc comment for
                    // why forcing here is safe.
                    let force = match scheduler.poll(Instant::now(), current_write_epoch()) {
                        SchedulerAction::Wait => continue,
                        SchedulerAction::FlushNow => false,
                        SchedulerAction::FlushAtRpoCeiling => true,
                    };
                    // Housekeeping blocks an in-progress drain, but does not
                    // refresh the idle clock and thereby keep the cell alive.
                    let _maintenance = match lifecycle.begin_maintenance("durable-flush") {
                        Ok(lease) => lease,
                        Err(_) => return,
                    };
                    // Capture the epoch immediately before the attempt itself
                    // (not the one `poll` saw, which may be stale by now) —
                    // mirrors `flush`'s own "acknowledge only the epoch
                    // captured before the checkpoint" discipline.
                    let epoch_before = current_write_epoch();
                    let started = Instant::now();
                    let result =
                        run_scheduled_flush_blocking(&profile_dir, &durable_dir, force).await;
                    testify_flush_result(
                        &testimony,
                        FlushMode::Periodic,
                        started.elapsed(),
                        &result,
                    );
                    match result {
                        Ok((_outcome, attempt)) => {
                            scheduler.record_flush_attempt(epoch_before, attempt);
                        }
                        Err(error) => {
                            log::error!("periodic durable flush failed: {error}");
                            // Deliberately do not advance the scheduler's
                            // watermark: an error means nothing was
                            // confirmed, so `epoch_before` stays pending and
                            // the very next poll tick retries (its deadlines
                            // have already passed, since this attempt was
                            // itself triggered by one).
                        }
                    }
                }
            })
        };

        // Hot-cell freshness (A4 review SUSPECT — see `observatory::boot`'s
        // module doc, "Hot-cell freshness"): the projector publishes hourly,
        // but this boot-hook otherwise only runs once, at boot. If the cell
        // stays warm across a publish (kept alive by other traffic), a
        // boot-only hook would never see the fresh generation until the next
        // cold start.
        //
        // WRONG (review r2): this used to gate the SPAWN of the re-check
        // ticker itself on `observatory_gate_open_at_boot` — but that flag
        // additionally requires a `CURRENT` marker to ALREADY exist at boot
        // (`projector_gate_open`'s EFS-discoverable activation check). An
        // observatory cell that boots BEFORE the projector's very first
        // publish (or any cell warm before the projector is first turned on
        // for this environment) never got a ticker spawned at all — it would
        // then sit warm forever, never noticing even the projector's FIRST
        // publication, until its next cold start. Gate the spawn on the
        // graph_id alone instead (the cheap, zero-I/O half of the SAME
        // check `projector_gate_open` does) — every OTHER cell (e.g.
        // `angels`) still spawns nothing here, but an observatory cell
        // always gets the ticker, and each tick's own
        // `run_boot_hook_for_cell` call re-evaluates the FULL gate
        // (including the `CURRENT` marker) fresh every time, correctly
        // returning `GateClosed` (a cheap no-op, see the `BootHookOutcome`
        // match arm below) for every tick before the projector's first
        // publication ever lands, and picking it up on whichever tick comes
        // after.
        let (observatory_refresh_stop, mut observatory_refresh_stopped) =
            tokio::sync::oneshot::channel::<()>();
        let observatory_cell_is_target_graph =
            garden_lib::observatory::boot::is_observatory_cell(&observatory_cell_graph_id);
        let observatory_refresh = observatory_cell_is_target_graph.then(|| {
            let profile_dir = profile_dir.clone();
            let durable_dir = durable_dir.clone();
            let lifecycle = std::sync::Arc::clone(&lifecycle);
            let testimony = lifecycle_testimony.clone();
            let app_handle = observatory_app_handle.clone();
            let graph_id = observatory_cell_graph_id.clone();
            garden_lib::headless::async_runtime::spawn(async move {
                let mut ticker =
                    tokio::time::interval(garden_lib::observatory::boot::refresh_interval());
                ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                // First tick fires immediately; skip it — the boot-time hook
                // above already applied whatever generation was ready
                // moments ago.
                ticker.tick().await;
                loop {
                    tokio::select! {
                        _ = &mut observatory_refresh_stopped => return,
                        _ = ticker.tick() => {}
                    }
                    let _maintenance = match lifecycle.begin_maintenance("observatory-refresh") {
                        Ok(lease) => lease,
                        Err(_) => return,
                    };
                    use garden_lib::observatory::boot::{run_boot_hook_for_cell, BootHookOutcome};
                    match run_boot_hook_for_cell(&app_handle, &graph_id).await {
                        BootHookOutcome::Applied(report) => {
                            log::info!(
                                "observatory periodic re-check: catch_up applied a fresh \
                                 generation — raw +{}/-{}, rollups +{}/-{}",
                                report.raw.added,
                                report.raw.removed,
                                report.rollups.added,
                                report.rollups.removed,
                            );
                            // Same rationale as the boot-time forced flush:
                            // publication must not wait for the next
                            // periodic flush tick.
                            let flush_started = Instant::now();
                            let flush_result =
                                run_flush_blocking(
                                    &profile_dir,
                                    &durable_dir,
                                    FlushTrigger::ObservatoryApply,
                                )
                                .await;
                            testify_flush_result(
                                &testimony,
                                FlushMode::Periodic,
                                flush_started.elapsed(),
                                &flush_result,
                            );
                            if let Err(error) = flush_result {
                                log::error!(
                                    "observatory periodic re-check: forced durable flush failed: {error}"
                                );
                            }
                        }
                        BootHookOutcome::GenerationChangedDuringRead { attempted_token } => {
                            log::info!(
                                "observatory periodic re-check: generation {attempted_token} was \
                                 superseded mid-read — retrying next tick"
                            );
                        }
                        BootHookOutcome::InProgress { token } => {
                            // Single-flight (review r2): a PREVIOUS tick's
                            // apply is still running past its own budget —
                            // this tick did NOT spawn a second, concurrent
                            // one. No action needed here; the next tick (or
                            // this same in-flight task simply finishing on
                            // its own) resolves it.
                            log::info!(
                                "observatory periodic re-check: apply of generation {token} still \
                                 in flight from an earlier tick — not spawning a second one"
                            );
                        }
                        BootHookOutcome::Failed(error) => {
                            log::error!("observatory periodic re-check: catch_up failed: {error}");
                        }
                        // AlreadyCurrent (the common case — nothing new since
                        // the last apply) and BundleNotReady (projector
                        // hasn't published since this cell came up) need no
                        // action. GateClosed is REACHABLE here now (review
                        // r2 SUSPECT fix — this ticker spawns for every
                        // observatory cell regardless of whether the
                        // projector had already published at boot; a tick
                        // before its first-ever publication finds no
                        // CURRENT marker yet and closes the gate cleanly,
                        // same as any other tick before the next
                        // publication).
                        BootHookOutcome::AlreadyCurrent { .. }
                        | BootHookOutcome::BundleNotReady { .. }
                        | BootHookOutcome::GateClosed => {}
                    }
                }
            })
        });

        let reason = wait_for_shutdown(&lifecycle, idle_ttl).await;

        // U8 write lease (spec §3.5): positive terminal evidence (409
        // lease_lost/lease_forfeit, or the GARDEN_LEASE_FENCED_MAX_MS cap
        // with no successor evidence at all) means the final flush below
        // would itself be the zombie write this whole protocol exists to
        // forbid. Skip draining, the periodic-task teardown, and the flush
        // entirely, and fall into the SAME exit(4) plumbing every other
        // shutdown failure already uses (the `Err` arm at the bottom of this
        // block) — no new exit code. `begin_draining` still runs here
        // (unlike the merely-FENCED-but-recoverable state handled further
        // below, which must keep serving reads per spec) because this path
        // is, unconditionally, about to exit: it only stops brand-new HTTP
        // admission in the last moments before the process dies.
        if let ShutdownReason::LeaseTerminal(terminal_reason) = &reason {
            lifecycle.begin_draining();
            log::error!(
                "gardend exiting on positive write-lease-terminal evidence \
                 ({terminal_reason:?}) — skipping the final durable flush entirely"
            );
            lifecycle_testimony.emit_terminal_failed(
                StopReason::LeaseTerminal,
                FailedFinalFlush::SkippedFenced,
                lease_terminal_error_code(terminal_reason),
            );
            return Err(format!("write lease terminated: {terminal_reason:?}"));
        }

        let reason =
            drain_admitted_work(&lifecycle, &lifecycle_testimony, reason, quiesce_timeout).await;
        log::info!(
            "gardend {} shutdown; running final durable flush",
            reason.label()
        );

        // Stop at the next cancellation boundary. If a periodic flush is
        // already running, let it finish and testify before the final flush;
        // aborting its async caller would orphan the blocking operation.
        let _ = periodic_stop.send(());
        if let Err(error) = periodic.await {
            log::error!("periodic durable flush task failed: {error}");
        }
        // Same shutdown discipline for the observatory hot-cell re-check
        // ticker — a no-op when it was never spawned (non-observatory cell,
        // or the projector gate was closed at boot).
        let _ = observatory_refresh_stop.send(());
        if let Some(observatory_refresh) = observatory_refresh {
            if let Err(error) = observatory_refresh.await {
                log::error!("observatory periodic re-check task failed: {error}");
            }
        }
        // Single-flight shutdown drain (review r2 WRONG "...track and join
        // the worker before retry OR SHUTDOWN"): if a catch_up apply is
        // still in flight (a previous tick's own budget elapsed but the
        // task itself kept running), wait for it here — bounded by the SAME
        // apply budget every apply respects — so the imminent final flush
        // below has a chance to capture it instead of losing it to an
        // orphaned background task. A cheap no-op for every cell where
        // nothing is in flight (the overwhelming common case), so this is
        // unconditional — no graph_id gate needed.
        garden_lib::observatory::boot::drain_in_flight_apply().await;
        let lease_handle = garden_lib::headless::lease::handle();
        // The shutdown selector and this continuation are concurrent with
        // the renew/publish workers. Recheck terminal evidence after every
        // drain/join and before even considering a final flush: a signal or
        // idle branch may have won the select just before the terminal
        // notification. Terminal evidence always means exit(4), no release.
        if let Some(terminal_reason) = lease_handle.and_then(|lease| lease.terminal_reason()) {
            log::error!(
                "positive write-lease-terminal evidence arose during shutdown \
                 ({terminal_reason:?}) — skipping the final durable flush and release"
            );
            lifecycle_testimony.emit_terminal_failed(
                StopReason::LeaseTerminal,
                FailedFinalFlush::SkippedFenced,
                lease_terminal_error_code(&terminal_reason),
            );
            return Err(format!(
                "write lease terminated during shutdown: {terminal_reason:?}"
            ));
        }

        // A live-but-FENCED lease (margin exhausted on what so far looks
        // like a transient blip — no successor evidence yet, so this is the
        // *recoverable* state, not the `LeaseTerminal` branch handled above)
        // must not run the final flush: under a fenced writer that flush IS
        // the zombie write (spec §3.5). This is the only place
        // `examples/gardend.rs` consults `WriteLease::is_fenced` directly —
        // `cell_durability`'s Gate A (U8-9) independently refuses the publish
        // and `run_flush_blocking` preserves `FlushAttemptOutcome::Fenced`
        // for testimony. This early guard avoids doing expensive work that
        // is already known to be unsafe.
        //
        // Mode-gated exactly like Gates A/B/C themselves: `is_fenced` is
        // true renewal state regardless of mode (see `wait_for_lease_
        // terminal`'s doc comment), so `Observe` must short-circuit to
        // "safe" here too, or observe mode would start silently skipping
        // final flushes on lease trouble — a real behavior change observe
        // mode must never make.
        let flush_is_safe = lease_handle
            .map(|lease| {
                lease.mode() == garden_lib::headless::lease::LeaseMode::Observe
                    || (lease.terminal_reason().is_none() && !lease.is_fenced())
            })
            .unwrap_or(true);

        // Final flush is FORCED when safe to run at all: capture state even
        // if an import is mid-run (the import is replayable; losing the
        // just-captured state is worse).
        let mut final_flush_completed = flush_is_safe;
        let final_result = if flush_is_safe {
            let final_started = Instant::now();
            let result =
                run_flush_blocking(&profile_dir, &durable_dir, FlushTrigger::FinalShutdown).await;
            if matches!(
                result,
                Ok((
                    _,
                    garden_lib::headless::durability::FlushAttemptOutcome::Fenced
                ))
            ) {
                final_flush_completed = false;
            }
            testify_flush_result(
                &lifecycle_testimony,
                FlushMode::Final,
                final_started.elapsed(),
                &result,
            );
            // Gate B/C can change lease state while the blocking flush is in
            // flight. The pre-flush `flush_is_safe` sample is therefore not
            // a shutdown verdict: re-read the latch before testifying or
            // releasing. A Gate-C 409 is terminal and must take the same
            // exit(4), no-release path as terminal evidence selected before
            // shutdown began.
            if let Some(lease) = lease_handle {
                if let Some(terminal_reason) = lease.terminal_reason() {
                    log::error!(
                        "positive write-lease-terminal evidence arose during the final durable \
                         flush ({terminal_reason:?}) — refusing release and exit success"
                    );
                    lifecycle_testimony.emit_terminal_failed(
                        StopReason::LeaseTerminal,
                        FailedFinalFlush::SkippedFenced,
                        lease_terminal_error_code(&terminal_reason),
                    );
                    return Err(format!(
                        "write lease terminated during final flush: {terminal_reason:?}"
                    ));
                }
                if lease.mode() == garden_lib::headless::lease::LeaseMode::Enforce
                    && lease.is_fenced()
                {
                    final_flush_completed = false;
                    log::warn!(
                        "write lease became FENCED during the final durable flush — \
                         testifying the final flush as skipped/fenced, never completed"
                    );
                }
            }
            result
        } else {
            log::warn!(
                "skipping the final durable flush: the write lease is FENCED \
                 (would be a zombie write)"
            );
            Ok((
                garden_lib::headless::durability::FlushOutcome::default(),
                garden_lib::headless::durability::FlushAttemptOutcome::Fenced,
            ))
        };
        match final_result {
            Ok(_) => {
                // Clean-exit release (spec §1.2/§3.5): safe to attempt
                // regardless of `flush_is_safe` — the gateway's CAS handles
                // a stale holder's release gracefully either way (a 409
                // just means a successor already took over;
                // `WriteLease::release` already treats that as expected and
                // does not retry).
                if let Some(lease) = lease_handle {
                    if let Err(error) = lease.release(reason.label()).await {
                        log::warn!(
                            "write lease release failed (non-fatal, the row \
                             lapses on its own TTL): {error}"
                        );
                    }
                }
                let final_flush = if final_flush_completed {
                    SuccessfulFinalFlush::Completed
                } else {
                    SuccessfulFinalFlush::SkippedFenced
                };
                lifecycle_testimony.emit_terminal_succeeded(reason.testimony(), final_flush);
                log::info!(
                    "gardend shutting down ({})",
                    if final_flush_completed {
                        "final flush complete"
                    } else {
                        "final flush skipped: lease fenced"
                    }
                );
                Ok(())
            }
            Err(error) => {
                lifecycle_testimony.emit_terminal_failed(
                    StopReason::RuntimeFailure,
                    FailedFinalFlush::Failed,
                    ErrorCode::FinalFlushFailed,
                );
                Err(format!("final durable flush failed: {error}"))
            }
        }
    });

    if !testimony.shutdown(Duration::from_millis(250)) {
        log::warn!("CaptureEvent stdout remained stalled past the 250ms shutdown budget");
    }

    if let Err(error) = shutdown_result {
        log::error!("gardend shutdown failed: {error}");
        std::process::exit(4);
    }
}

#[cfg(not(feature = "desktop"))]
#[derive(Clone, Debug, PartialEq, Eq)]
enum ShutdownReason {
    Signal,
    Idle,
    /// U8 write lease (spec §3.5): positive terminal evidence (409
    /// `lease_lost`/`lease_forfeit`, or the `GARDEN_LEASE_FENCED_MAX_MS` cap
    /// with no successor evidence at all). Carries the reason for testimony;
    /// this incarnation always exits nonzero and never runs the final
    /// flush. Not `Copy` (unlike the other two reasons) because
    /// `TerminalReason` is not — every call site below borrows via
    /// `label()`/`testimony()` rather than relying on implicit copies.
    LeaseTerminal(garden_lib::headless::lease::TerminalReason),
}

#[cfg(not(feature = "desktop"))]
impl ShutdownReason {
    fn label(&self) -> &'static str {
        match self {
            Self::Signal => "signal",
            Self::Idle => "idle",
            Self::LeaseTerminal(_) => "lease-terminal",
        }
    }

    fn testimony(&self) -> garden_lib::headless::testimony::StopReason {
        match self {
            Self::Signal => garden_lib::headless::testimony::StopReason::Signal,
            Self::Idle => garden_lib::headless::testimony::StopReason::Idle,
            Self::LeaseTerminal(_) => garden_lib::headless::testimony::StopReason::LeaseTerminal,
        }
    }
}

#[cfg(not(feature = "desktop"))]
fn testify_startup_failure(
    testimony: &garden_lib::headless::testimony::CaptureWriter,
    stage: garden_lib::headless::testimony::BootFailureStage,
    error_code: garden_lib::headless::testimony::ErrorCode,
    duration: std::time::Duration,
    exit_code: i32,
) -> ! {
    testimony.emit_boot_failed(stage, error_code, duration);
    testimony.emit_terminal_failed(
        garden_lib::headless::testimony::StopReason::StartupFailure,
        garden_lib::headless::testimony::FailedFinalFlush::NotConfigured,
        error_code,
    );
    if !testimony.shutdown(std::time::Duration::from_millis(250)) {
        log::warn!("CaptureEvent stdout remained stalled past the 250ms startup-failure budget");
    }
    std::process::exit(exit_code);
}

/// Enforce-mode startup must never outlive terminal/fenced lease evidence.
/// Renew runs concurrently with hydrate and core setup, so the checks inside
/// the initial repair dance are not a lasting boot verdict. Recheck at each
/// boundary before building/exposing the API and before claiming boot-ready.
#[cfg(not(feature = "desktop"))]
fn enforce_lease_boot_guard(
    testimony: &garden_lib::headless::testimony::CaptureWriter,
    boot_started: std::time::Instant,
    checkpoint: &str,
) {
    use garden_lib::headless::lease::{handle, LeaseMode};
    use garden_lib::headless::testimony::{BootFailureStage, ErrorCode};

    let Some(lease) = handle() else {
        return;
    };
    if lease.mode() == LeaseMode::Observe {
        return;
    }
    if let Some(terminal) = lease.terminal_reason() {
        log::error!(
            "write lease became terminal {checkpoint} ({terminal:?}) — refusing to expose \
             readiness or emit boot-ready"
        );
        testify_startup_failure(
            testimony,
            BootFailureStage::LeaseBoot,
            ErrorCode::LeaseUnavailableAtBoot,
            boot_started.elapsed(),
            4,
        );
    }
    if lease.is_fenced() || !lease.valid_with_margin() {
        log::error!(
            "write lease was fenced or invalid {checkpoint} — refusing to expose readiness or \
             emit boot-ready"
        );
        testify_startup_failure(
            testimony,
            BootFailureStage::LeaseBoot,
            ErrorCode::LeaseUnavailableAtBoot,
            boot_started.elapsed(),
            4,
        );
    }
}

#[cfg(not(feature = "desktop"))]
fn configured_idle_ttl() -> Option<std::time::Duration> {
    std::env::var("GARDEN_IDLE_TTL_SECONDS")
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|seconds| *seconds > 0)
        .map(std::time::Duration::from_secs)
}

#[cfg(not(feature = "desktop"))]
fn configured_quiesce_timeout() -> std::time::Duration {
    std::env::var("GARDEN_QUIESCE_TIMEOUT_SECONDS")
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|seconds| *seconds > 0)
        .map(std::time::Duration::from_secs)
        .unwrap_or_else(|| std::time::Duration::from_secs(20))
}

#[cfg(not(feature = "desktop"))]
async fn wait_for_shutdown(
    lifecycle: &std::sync::Arc<garden_lib::headless::lifecycle::CellLifecycle>,
    idle_ttl: Option<std::time::Duration>,
) -> ShutdownReason {
    let Some(idle_ttl) = idle_ttl else {
        tokio::select! {
            _ = wait_for_shutdown_signal() => return ShutdownReason::Signal,
            reason = wait_for_lease_terminal() => return ShutdownReason::LeaseTerminal(reason),
        }
    };
    tokio::select! {
        _ = wait_for_shutdown_signal() => ShutdownReason::Signal,
        _ = lifecycle.wait_until_idle(idle_ttl) => ShutdownReason::Idle,
        reason = wait_for_lease_terminal() => ShutdownReason::LeaseTerminal(reason),
    }
}

/// Resolves the instant this incarnation's write lease reports a
/// [`TerminalReason`](garden_lib::headless::lease::TerminalReason) — positive
/// evidence (spec §3.5) that this process must exit(4) without a final
/// flush. Races alongside signal/idle in [`wait_for_shutdown`]'s
/// `tokio::select!`; when no lease exists at all (desktop, or a
/// legacy-unfenced boot — `cell_lease::handle()` is `None` forever in both
/// cases) this future never resolves, so it simply never wins that race —
/// the Tauri desktop app and legacy boots are untouched.
///
/// Mode policy is enforced inside `WriteLease`: observe mode keeps
/// `is_fenced()==false`, emits content-free `dep.state` testimony, and never
/// latches `terminal_reason`; enforce mode latches terminal and fences in one
/// transition. Consequently this future cannot resolve in observe mode. The
/// defensive mode check below preserves availability even if that internal
/// invariant regresses.
#[cfg(not(feature = "desktop"))]
async fn wait_for_lease_terminal() -> garden_lib::headless::lease::TerminalReason {
    use garden_lib::headless::lease::LeaseMode;
    let Some(lease) = garden_lib::headless::lease::handle() else {
        std::future::pending::<()>().await;
        unreachable!("std::future::pending() never resolves");
    };
    loop {
        // Register before reading state (mirrors `CellLifecycle::
        // wait_until_idle`'s own pattern): `Notify::notify_waiters` only
        // wakes listeners already enabled at the moment it fires, so
        // enabling first closes the gap between checking `terminal_reason`
        // and awaiting the next notification.
        let notified = lease.state_changed().notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if let Some(reason) = lease.terminal_reason() {
            if lease.mode() == LeaseMode::Observe {
                log::warn!(
                    "observe mode: write lease reports terminal evidence ({reason:?}) — \
                     NOT exiting (observe mode never refuses; testimony only)"
                );
                // `terminal` is a set-once `OnceLock` (see `cell_lease.rs`),
                // so this can only ever fire once per process — pending
                // forever here means this race arm simply never resolves
                // for the rest of the process's life, exactly like the
                // no-lease case above (never awoken again: nothing ever
                // calls `notify_waiters` on this lease again after
                // `terminal` is set).
                std::future::pending::<()>().await;
            }
            return reason;
        }
        notified.await;
    }
}

/// Maps a [`TerminalReason`](garden_lib::headless::lease::TerminalReason) to
/// the closed testimony vocabulary (`capture_event::ErrorCode`) so an
/// operator reading CaptureEvent NDJSON can tell the three terminal causes
/// apart (spec §3.5's failure-matrix rows 3, 8, and the 300s cap) without
/// parsing free-text log lines.
#[cfg(not(feature = "desktop"))]
fn lease_terminal_error_code(
    reason: &garden_lib::headless::lease::TerminalReason,
) -> garden_lib::headless::testimony::ErrorCode {
    use garden_lib::headless::lease::TerminalReason;
    use garden_lib::headless::testimony::ErrorCode;
    match reason {
        TerminalReason::LeaseLost { .. } => ErrorCode::LeaseLost,
        TerminalReason::LeaseForfeit => ErrorCode::LeaseForfeit,
        TerminalReason::FencedTimeout => ErrorCode::LeaseFencedTimeout,
    }
}

/// `GARDEN_LEASE_LAST_SNAP`/`GARDEN_LEASE_PENDING_SNAP` (spec §3.2's
/// boot-repair carrier — the claim's `ALL_OLD` values, stamped into env by
/// the gateway at spawn). Parsed here, once, rather than inside
/// `cell_durability::boot_repair` itself, so that function stays testable
/// against plain integer literals (see its own doc comment).
#[cfg(not(feature = "desktop"))]
fn lease_repair_env_from_process() -> (Option<u64>, Option<u64>) {
    let parse = |name: &str| {
        std::env::var(name)
            .ok()
            .and_then(|value| value.trim().parse::<u64>().ok())
    };
    (
        parse("GARDEN_LEASE_LAST_SNAP"),
        parse("GARDEN_LEASE_PENDING_SNAP"),
    )
}

/// The non-lease hydrate path (desktop, and legacy-unfenced boots): byte-
/// for-byte the same logic this module ran before U8-10 existed, now shared
/// by [`boot_lease_dance_and_hydrate`]'s early-return arms as well as its
/// post-repair tail.
#[cfg(not(feature = "desktop"))]
fn run_hydrate_or_exit(
    testimony: &garden_lib::headless::testimony::CaptureWriter,
    boot_started: std::time::Instant,
    profile_dir: Option<&std::path::Path>,
    durable_dir: Option<&std::path::Path>,
) -> garden_lib::headless::durability::HydrateOutcome {
    use garden_lib::headless::testimony::{BootFailureStage, ErrorCode};
    match (profile_dir, durable_dir) {
        (Some(profile), Some(durable)) => {
            match garden_lib::headless::durability::hydrate_detailed(profile, durable) {
                Ok(outcome) => outcome,
                Err(error) => {
                    log::error!("durable hydrate failed: {error}");
                    testify_startup_failure(
                        testimony,
                        BootFailureStage::Hydrate,
                        ErrorCode::DurableHydrateFailed,
                        boot_started.elapsed(),
                        3,
                    );
                }
            }
        }
        _ => garden_lib::headless::durability::HydrateOutcome {
            mode: garden_lib::headless::durability::HydrateMode::Fresh,
            snapshot_id: None,
            hydrated_bytes: None,
        },
    }
}

/// U8 boot-time write-lease dance (spec §3.2), run strictly before
/// `hydrate_detailed`: start the renew task (`cell_lease::init`), wait out
/// whatever this mode's boot contract requires, run the (possibly dry-run)
/// boot-time repair (`cell_durability::boot_repair`, spec §3.6), then
/// hydrate. Desktop and legacy-unfenced boots (`cell_lease::InitOutcome`'s
/// first two variants) fall straight through to [`run_hydrate_or_exit`],
/// unchanged from before this dance existed.
///
/// Enforce mode waits (unbounded, but internally capped by `cell_lease`'s
/// own `GARDEN_LEASE_FENCED_MAX_MS` renew-retry ceiling) for the first
/// successful renew, then the effective deadline, before ever touching
/// disk — "authority unreachable ⇒ keep retrying, never serve writes"
/// (spec §3.2): the loopback server is not even built yet at this point in
/// `main`, so readiness genuinely fails and Kubernetes recycles the pod.
///
/// Observe mode adds **zero** boot latency ("observe mode changes no
/// behavior anywhere — testimony only"): it never awaits `first_renew` at
/// all, going straight to a dry-run boot repair (still meaningful — the
/// claim-time `LAST_SNAP`/`PENDING_SNAP` env values it reads do not depend
/// on any renew ever succeeding) and then hydrate.
#[cfg(not(feature = "desktop"))]
async fn boot_lease_dance_and_hydrate(
    testimony: &garden_lib::headless::testimony::CaptureWriter,
    boot_started: std::time::Instant,
    profile_dir: Option<&std::path::Path>,
    durable_dir: Option<&std::path::Path>,
) -> garden_lib::headless::durability::HydrateOutcome {
    use garden_lib::headless::lease::{self, InitOutcome, LeaseMode, PublishPhase};
    use garden_lib::headless::testimony::{BootFailureStage, ErrorCode};

    let init_outcome = match lease::init().await {
        Ok(outcome) => outcome,
        Err(reason) => {
            log::error!("write lease refused to boot: {reason}");
            testify_startup_failure(
                testimony,
                BootFailureStage::LeaseBoot,
                ErrorCode::LeaseBootRefused,
                boot_started.elapsed(),
                4,
            );
        }
    };

    let (mode, first_renew) = match init_outcome {
        InitOutcome::Desktop | InitOutcome::LegacyUnfenced => {
            return run_hydrate_or_exit(testimony, boot_started, profile_dir, durable_dir);
        }
        InitOutcome::Lease { mode, first_renew } => (mode, first_renew),
    };

    let dry_run = match mode {
        LeaseMode::Observe => {
            // See this fn's doc comment: observe mode never awaits
            // `first_renew`. `cell_lease`'s sender side already tolerates a
            // dropped receiver (`let _ = sender.send(...)`), so dropping it
            // here is sound — the renew task keeps running in the
            // background regardless.
            drop(first_renew);
            true
        }
        LeaseMode::Enforce => match first_renew.await {
            Ok(Ok(first)) => {
                log::info!(
                    "write lease established (effective_in_ms={}, ttl_remaining_ms={}); \
                     waiting out the effective deadline before repair/hydrate",
                    first.effective_in_ms,
                    first.ttl_remaining_ms
                );
                if first.effective_in_ms > 0 {
                    tokio::time::sleep(std::time::Duration::from_millis(first.effective_in_ms))
                        .await;
                }
                let lease = lease::handle().expect("lease initialized above");
                if let Some(terminal) = lease.terminal_reason() {
                    log::error!(
                        "write lease became terminal during the effective-delay wait \
                         ({terminal:?}) — refusing to repair/hydrate"
                    );
                    testify_startup_failure(
                        testimony,
                        BootFailureStage::LeaseBoot,
                        ErrorCode::LeaseUnavailableAtBoot,
                        boot_started.elapsed(),
                        4,
                    );
                }
                if lease.is_fenced() || !lease.valid_with_margin() {
                    log::error!(
                        "write lease was not valid after the effective-delay wait — \
                         refusing to repair/hydrate"
                    );
                    testify_startup_failure(
                        testimony,
                        BootFailureStage::LeaseBoot,
                        ErrorCode::LeaseUnavailableAtBoot,
                        boot_started.elapsed(),
                        4,
                    );
                }
                false
            }
            Ok(Err(terminal)) => {
                log::error!(
                    "write lease terminated before ever renewing successfully ({terminal:?}) \
                     — refusing to boot"
                );
                testify_startup_failure(
                    testimony,
                    BootFailureStage::LeaseBoot,
                    ErrorCode::LeaseUnavailableAtBoot,
                    boot_started.elapsed(),
                    4,
                );
            }
            Err(_recv_error) => {
                // The renew task always resolves `first_renew` (on success
                // or on a terminal event) before returning — see
                // `cell_lease::init`'s doc comment. A closed channel without
                // an answer should not happen in practice; handled as the
                // conservative refuse-to-boot case rather than a panic.
                log::error!(
                    "write lease renew task ended without ever answering first_renew — \
                     refusing to boot"
                );
                testify_startup_failure(
                    testimony,
                    BootFailureStage::LeaseBoot,
                    ErrorCode::LeaseUnavailableAtBoot,
                    boot_started.elapsed(),
                    4,
                );
            }
        },
    };

    let Some(durable) = durable_dir else {
        // Unreachable: `cell_lease::decide_boot` only returns `Lease` when
        // GARDEN_DURABLE_DIR is set. Kept so this function can never
        // silently mis-hydrate if that invariant is ever violated elsewhere.
        return run_hydrate_or_exit(testimony, boot_started, profile_dir, durable_dir);
    };

    let (last_snap, pending_snap) = lease_repair_env_from_process();
    match garden_lib::headless::durability::boot_repair(durable, last_snap, pending_snap, dry_run) {
        Ok(outcome) => {
            log::info!(
                "boot-time lease repair: {outcome:?}{}",
                if dry_run {
                    " (observe mode, dry run)"
                } else {
                    ""
                }
            );
            if !dry_run {
                if let Some(lease) = lease::handle() {
                    use garden_lib::headless::durability::BootRepairOutcome;
                    match outcome {
                        BootRepairOutcome::AcceptPending { seq } => {
                            if let Err(error) = lease.publish(seq, PublishPhase::Commit) {
                                lease.mark_terminal_by_commit_refusal(error);
                                log::error!(
                                    "boot repair follow-up publish(commit, {seq}) failed: \
                                     {error:?} — refusing to hydrate/serve"
                                );
                                testify_startup_failure(
                                    testimony,
                                    BootFailureStage::LeaseBoot,
                                    ErrorCode::LeaseBootRefused,
                                    boot_started.elapsed(),
                                    4,
                                );
                            }
                        }
                        BootRepairOutcome::AbortedPending { seq } => {
                            if let Err(error) = lease.publish(seq, PublishPhase::Abort) {
                                lease.mark_fenced_by_publish_error(error);
                                log::error!(
                                    "boot repair follow-up publish(abort, {seq}) failed: \
                                     {error:?} — refusing to hydrate/serve"
                                );
                                testify_startup_failure(
                                    testimony,
                                    BootFailureStage::LeaseBoot,
                                    ErrorCode::LeaseBootRefused,
                                    boot_started.elapsed(),
                                    4,
                                );
                            }
                        }
                        _ => {}
                    }
                    if let Some(terminal) = lease.terminal_reason() {
                        log::error!(
                            "write lease became terminal during boot repair ({terminal:?}) — \
                             refusing to hydrate/serve"
                        );
                        testify_startup_failure(
                            testimony,
                            BootFailureStage::LeaseBoot,
                            ErrorCode::LeaseUnavailableAtBoot,
                            boot_started.elapsed(),
                            4,
                        );
                    }
                    if lease.is_fenced() || !lease.valid_with_margin() {
                        log::error!(
                            "write lease was no longer valid after boot repair — \
                             refusing to hydrate/serve"
                        );
                        testify_startup_failure(
                            testimony,
                            BootFailureStage::LeaseBoot,
                            ErrorCode::LeaseUnavailableAtBoot,
                            boot_started.elapsed(),
                            4,
                        );
                    }
                }
            }
        }
        Err(error) => {
            log::error!("boot-time lease repair refused to boot: {error}");
            let exit_code = if error.requires_snapshot_authority_repair() {
                BOOT_REPAIR_REQUIRED_EXIT_CODE
            } else {
                4
            };
            testify_startup_failure(
                testimony,
                BootFailureStage::LeaseBoot,
                ErrorCode::LeaseBootRefused,
                boot_started.elapsed(),
                exit_code,
            );
        }
    }

    run_hydrate_or_exit(testimony, boot_started, profile_dir, durable_dir)
}

#[cfg(not(feature = "desktop"))]
async fn drain_admitted_work(
    lifecycle: &std::sync::Arc<garden_lib::headless::lifecycle::CellLifecycle>,
    testimony: &garden_lib::headless::testimony::CaptureWriter,
    reason: ShutdownReason,
    timeout: std::time::Duration,
) -> ShutdownReason {
    // Idle shutdown already performed the atomic transition in
    // `wait_until_idle`; signal shutdown performs it here. If a signal and the
    // idle deadline race and idle won the fence, retain the transition's true
    // cause rather than attributing it to the selected signal branch.
    let transitioned = lifecycle.begin_draining();
    let reason = if reason == ShutdownReason::Signal && !transitioned {
        ShutdownReason::Idle
    } else {
        reason
    };
    let draining = lifecycle.snapshot();
    testimony.emit_draining(
        reason.testimony(),
        activity_counts(&draining),
        (reason == ShutdownReason::Idle).then_some(draining.idle_for),
    );

    let quiesce_started = std::time::Instant::now();
    let quiesced = lifecycle.wait_for_quiescence(timeout).await;
    let final_activity = lifecycle.snapshot();
    testimony.emit_quiesced(
        reason.testimony(),
        !quiesced,
        activity_counts(&final_activity),
        quiesce_started.elapsed(),
    );
    if !quiesced {
        log::warn!(
            "gardend {} drain timed out after {}s with activity {:?}; proceeding to forced final flush",
            reason.label(),
            timeout.as_secs(),
            lifecycle.snapshot(),
        );
    }
    reason
}

#[cfg(not(feature = "desktop"))]
fn activity_counts(
    snapshot: &garden_lib::headless::lifecycle::CellActivitySnapshot,
) -> garden_lib::headless::testimony::ActivityCounts {
    garden_lib::headless::testimony::ActivityCounts {
        in_flight_requests: u64::try_from(snapshot.in_flight_requests).unwrap_or(u64::MAX),
        open_websockets: u64::try_from(snapshot.open_websockets).unwrap_or(u64::MAX),
        background_jobs: u64::try_from(snapshot.background_jobs).unwrap_or(u64::MAX),
        background_leases: u64::try_from(snapshot.background_leases).unwrap_or(u64::MAX),
    }
}

/// Wait for either SIGTERM (Kubernetes pod delete) or ctrl_c.
#[cfg(all(not(feature = "desktop"), unix))]
async fn wait_for_shutdown_signal() {
    use tokio::signal::unix::{signal, SignalKind};
    let mut sigterm = match signal(SignalKind::terminate()) {
        Ok(stream) => stream,
        Err(error) => {
            log::error!("install SIGTERM handler failed: {error}; falling back to ctrl_c only");
            let _ = tokio::signal::ctrl_c().await;
            return;
        }
    };
    tokio::select! {
        _ = sigterm.recv() => log::info!("SIGTERM received"),
        result = tokio::signal::ctrl_c() => {
            if let Err(error) = result {
                log::error!("ctrl_c listener failed: {error}");
            }
        }
    }
}

#[cfg(all(not(feature = "desktop"), not(unix)))]
async fn wait_for_shutdown_signal() {
    if let Err(error) = tokio::signal::ctrl_c().await {
        log::error!("ctrl_c listener failed: {error}");
    }
}

/// Run a caller-labelled forced flush on a blocking thread (it does synchronous
/// filesystem work), preserving the detailed disposition so callers and
/// CaptureEvent testimony can distinguish a lease fence from an honest clean
/// no-op.
#[cfg(not(feature = "desktop"))]
async fn run_flush_blocking(
    profile_dir: &std::path::Path,
    durable_dir: &std::path::Path,
    trigger: garden_lib::headless::durability::FlushTrigger,
) -> Result<
    (
        garden_lib::headless::durability::FlushOutcome,
        garden_lib::headless::durability::FlushAttemptOutcome,
    ),
    String,
> {
    let profile_dir = profile_dir.to_path_buf();
    let durable_dir = durable_dir.to_path_buf();
    let result = garden_lib::headless::async_runtime::spawn_blocking(move || {
        garden_lib::headless::durability::flush_forced_detailed_with_trigger(
            &profile_dir,
            &durable_dir,
            trigger,
        )
    })
    .await
    .map_err(|error| format!("durable flush task panicked: {error}"))?;
    match &result {
        Ok((outcome, _)) if outcome.published => log::info!(
            "durable flush published snap-{:06}: {} copied, {} linked, {} stores, {} bytes",
            outcome.sequence,
            outcome.files_copied,
            outcome.files_linked,
            outcome.stores_backed_up,
            outcome.bytes_copied
        ),
        Ok((_, garden_lib::headless::durability::FlushAttemptOutcome::Fenced)) => {
            log::warn!("durable flush refused by write-lease fence")
        }
        Ok(_) => log::debug!("durable flush no-op (nothing changed)"),
        Err(_) => {}
    }
    result
}

/// Like [`run_flush_blocking`], but for the dirty-driven periodic scheduler:
/// runs `flush_scheduled` (forced only when `force` is true — see
/// `SchedulerAction::FlushAtRpoCeiling`) and also returns
/// [`garden_lib::headless::durability::FlushAttemptOutcome`] so the caller can
/// correctly distinguish a confirmed-clean/published attempt (safe to advance
/// the scheduler's watermark past) from a deferred one (must not).
#[cfg(not(feature = "desktop"))]
async fn run_scheduled_flush_blocking(
    profile_dir: &std::path::Path,
    durable_dir: &std::path::Path,
    force: bool,
) -> Result<
    (
        garden_lib::headless::durability::FlushOutcome,
        garden_lib::headless::durability::FlushAttemptOutcome,
    ),
    String,
> {
    let profile_dir = profile_dir.to_path_buf();
    let durable_dir = durable_dir.to_path_buf();
    let result = garden_lib::headless::async_runtime::spawn_blocking(move || {
        let trigger = if force {
            garden_lib::headless::durability::FlushTrigger::RpoCeiling
        } else {
            garden_lib::headless::durability::FlushTrigger::PeriodicDebounce
        };
        garden_lib::headless::durability::flush_scheduled_with_trigger(
            &profile_dir,
            &durable_dir,
            force,
            trigger,
        )
    })
    .await
    .map_err(|error| format!("durable flush task panicked: {error}"))?;
    if let Ok((outcome, _)) = &result {
        if outcome.published {
            log::info!(
                "durable flush published snap-{:06}: {} copied, {} linked, {} stores, {} bytes",
                outcome.sequence,
                outcome.files_copied,
                outcome.files_linked,
                outcome.stores_backed_up,
                outcome.bytes_copied
            );
        } else {
            log::debug!("durable flush no-op (nothing changed)");
        }
    }
    result
}

#[cfg(not(feature = "desktop"))]
fn testify_flush_result(
    testimony: &garden_lib::headless::testimony::CaptureWriter,
    mode: garden_lib::headless::testimony::FlushMode,
    duration: std::time::Duration,
    result: &Result<
        (
            garden_lib::headless::durability::FlushOutcome,
            garden_lib::headless::durability::FlushAttemptOutcome,
        ),
        String,
    >,
) {
    use garden_lib::headless::testimony::{ErrorCode, FlushMeasurements, SnapshotId};
    match result {
        Ok((_, garden_lib::headless::durability::FlushAttemptOutcome::Fenced)) => {
            testimony.emit_flush_failed(mode, ErrorCode::LeaseFenced, duration)
        }
        Ok((outcome, _)) => testimony.emit_flush_succeeded(
            mode,
            outcome.published,
            duration,
            FlushMeasurements {
                snapshot_id: outcome
                    .published
                    .then(|| SnapshotId::from_sequence(outcome.sequence)),
                bytes: outcome.bytes_copied,
                files_copied: u64::try_from(outcome.files_copied).unwrap_or(u64::MAX),
                files_linked: u64::try_from(outcome.files_linked).unwrap_or(u64::MAX),
                stores_backed_up: u64::try_from(outcome.stores_backed_up).unwrap_or(u64::MAX),
            },
        ),
        Err(error) => testimony.emit_flush_failed(
            mode,
            if error.contains("forced durable flush timed out") {
                ErrorCode::FlushGateTimeout
            } else {
                ErrorCode::DurableFlushFailed
            },
            duration,
        ),
    }
}
