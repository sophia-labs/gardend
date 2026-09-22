//! Real-subprocess proof of the `examples/gardend.rs` A4 boot-wiring — the
//! ACTUAL compiled `gardend` headless binary, not a library call standing in
//! for it — proves that (1) a successful observatory catch-up on boot forces
//! a durable (EFS-equivalent) flush before any periodic tick or idle-reap
//! could possibly have produced one, (2) the process then shuts down
//! cleanly on SIGTERM once that work is complete, and (3) RE-SCOPE r1's
//! hot-cell periodic re-check picks up a NEWER projector generation
//! published while the process stays warm, with no restart. No mocks: this
//! is the literal artifact cloud-2 deploys (`cargo build
//! --no-default-features --features headless --example gardend`), driven
//! exactly the way a pod would — real env vars, a real bundle-directory
//! file contract (routed via the `GARDEN_OBSERVATORY_BUNDLE_DIR` override —
//! see `boot.rs`'s module doc for the default EFS-derived path this test
//! does NOT exercise, proven instead at the boot-hook level by
//! `tests/observatory_boot.rs`'s own
//! `observatory_boot_default_bundle_dir_derives_from_the_cells_existing_durable_dir_when_no_override_is_set`
//! — this file's own `durable_dir` must stay pristine until the REAL forced
//! flush lands the first entry in it, which a bundle dir nested under
//! `durable_dir` would defeat) built from the SAME fixture bytes A2/A3/A4's
//! other tests use, a real on-disk profile + durable dir.
//!
//! RE-SCOPE r1: activation is now the presence of a real, published
//! generation at the bundle dir (a `CURRENT` pointer naming
//! `obs-bundle.<token>.json` + `raw-snapshot.<token>.ndjson`) — there is no
//! `GARDEN_OBSERVATORY_PROJECTOR` pod env to set any more (no cell-pod-spec
//! can ever set one — the hard rule that motivated dropping it).
//!
//! Logs are redirected to real temp FILES, never `Stdio::piped()` — a piped
//! child can deadlock the whole test if its OS pipe buffer fills (~64KiB)
//! while this test is busy polling instead of draining it, and `RUST_LOG=info`
//! is verbose enough over a 30s window to risk exactly that. A file has no
//! such backpressure.
//!
//! `[[example]]` targets do not get a `CARGO_BIN_EXE_*` env var (that is
//! `[[bin]]`-only) — [`gardend_binary`] builds it explicitly via a nested
//! `cargo build` (fast/incremental once built once) and resolves the stable
//! `target/<profile>/examples/gardend` path.
//!
//! The `observatory` graph must already exist before this binary boots in
//! production (§A.5: the gateway's `ensure_cell("observatory")` mints it via
//! the existing create-graph flow BEFORE spawning the pod) — F4c self-heal is
//! not a substitute here, because it is armed by
//! `runtime_config::init_self_heal_graphs_from_env` inside
//! `loopback_server::start_loopback_server`, which this fix's own reordering
//! (the observatory boot-hook now runs BEFORE `wait_until_ready`/loopback
//! start) means has not run yet by the time the boot-hook's
//! `existing_graph_dir` call would need it. [`prime_observatory_graph`]
//! mirrors production by writing the same `graph.json` shape
//! `graph_service::create_graph` would directly into the profile dir before
//! the SAME binary boots for real.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use uuid::Uuid;

const LIFECYCLE_JSON: &str =
    include_str!("../src/observatory/fixtures/lifecycle_evaluation.input.json");
const BILLING_JSON: &str = include_str!("../src/observatory/fixtures/billing_llm_dau_v1.ok.json");
const RAW_NDJSON: &str = include_str!("../src/observatory/fixtures/valid.ndjson");

fn temp_dir(label: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "sophia-observatory-gardend-process-{label}-{}",
        Uuid::new_v4()
    ))
}

fn cleanup(dir: &Path) {
    let _ = std::fs::remove_dir_all(dir);
}

fn full_bundle(cursor: &str) -> String {
    format!(
        "{{\"cursor_high_water_mark\":{cursor:?},\"lifecycle\":[{LIFECYCLE_JSON}],\"billing\":[{BILLING_JSON}]}}"
    )
}

/// Publish one REAL, immutable generation into `dir` (creating it if
/// needed) — the SAME `obs-bundle.<token>.json` + `raw-snapshot.<token>.ndjson`
/// + `CURRENT` file contract `garden_lib::observatory::boot` reads,
/// mirroring `tests/observatory_boot.rs`'s own `publish_generation` helper
/// byte-for-byte (duplicated here as literal filenames, not through a
/// shared `pub` helper, exactly the way this file already duplicates
/// `GARDEN_DURABLE_DIR`'s literal rather than importing a private constant —
/// this is an external, real-subprocess test crate, not the same
/// compilation unit as `boot.rs`'s own tests).
fn publish_generation(dir: &Path, token: &str, cursor: &str) {
    std::fs::create_dir_all(dir).expect("create bundle dir");
    std::fs::write(
        dir.join(format!("obs-bundle.{token}.json")),
        full_bundle(cursor),
    )
    .expect("write obs-bundle.json");
    std::fs::write(dir.join(format!("raw-snapshot.{token}.ndjson")), RAW_NDJSON)
        .expect("write raw-snapshot.ndjson");
    std::fs::write(dir.join("CURRENT"), token).expect("publish CURRENT");
}

/// Write `<profile_dir>/graphs/observatory/graph.json` — the SAME
/// `GraphRecord` shape (`src/graph_record_store.rs`) `graph_service::create_graph`
/// produces for a fresh local graph, hand-assembled here only because that
/// service function is `pub(crate)` (unreachable from this external test
/// crate) — see the module doc for why production never needs this (the
/// gateway creates it before the pod boots) and why F4c self-heal cannot
/// substitute for it in THIS test.
fn prime_observatory_graph(profile_dir: &Path) {
    let graph_dir = profile_dir.join("graphs").join("observatory");
    std::fs::create_dir_all(&graph_dir).expect("create observatory graph dir");
    let now = "2026-07-16T00:00:00.000Z";
    let record = serde_json::json!({
        "graphId": "observatory",
        "title": "observatory",
        "status": "active",
        "origin": "local",
        "providerId": "local-profile",
        "localPath": graph_dir.to_string_lossy(),
        "createdAt": now,
        "updatedAt": now,
        "capabilities": [],
    });
    std::fs::write(
        graph_dir.join("graph.json"),
        serde_json::to_vec_pretty(&record).expect("serialize graph.json"),
    )
    .expect("write graph.json");
}

/// Locate (building if needed) the REAL compiled `gardend` headless example
/// binary — the literal artifact cloud-2 deploys.
fn gardend_binary() -> PathBuf {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let status = Command::new(env!("CARGO"))
        .args([
            "build",
            "--no-default-features",
            "--features",
            "headless",
            "--example",
            "gardend",
        ])
        .current_dir(&manifest_dir)
        .status()
        .expect("spawn `cargo build --example gardend`");
    assert!(
        status.success(),
        "cargo build --example gardend must succeed"
    );
    let path = manifest_dir.join("target/debug/examples/gardend");
    assert!(
        path.is_file(),
        "expected a built binary at {}",
        path.display()
    );
    path
}

/// Start from the invoking shell for ordinary developer settings, but remove
/// every canary lease, capture, identity, and U8 test-hook variable this
/// harness owns. Individual tests add their intentional values afterward.
fn gardend_command(binary: &Path) -> Command {
    let mut command = Command::new(binary);
    for name in [
        "SOPHIA_OBSERVATORY_CAPTURE_ENABLED",
        "SOPHIA_OBSERVATORY_CONTRACT_BUNDLE_SHA256",
        "GARDEN_CAPTURE_QUEUE_CAPACITY",
        "GARDEN_CAPTURE_HEARTBEAT_SECONDS",
        "GARDEN_CELL_GRAPH_ID",
        "GARDEN_CELL_MACHINE_ID",
        "GARDEN_CELL_MACHINE_RUN_ID",
        "GARDEN_DURABLE_DIR",
        "GARDEN_DURABLE_EPOCH",
        "GARDEN_LEASE_URL",
        "GARDEN_LEASE_MODE",
        "GARDEN_LEASE_RENEW_MS",
        "GARDEN_LEASE_MARGIN_MS",
        "GARDEN_LEASE_FENCED_MAX_MS",
        "GARDEN_LEASE_PUBLISH_TIMEOUT_MS",
        "GARDEN_LEASE_LAST_SNAP",
        "GARDEN_LEASE_PENDING_SNAP",
        "GARDEN_LEASE_TEST_BYPASS_GATE_A",
        "GARDEN_LEASE_TEST_POST_HYDRATE_DELAY_MS",
        "GARDEN_LEASE_TEST_PRE_LOOPBACK_DELAY_MS",
        "GARDEN_LEASE_TEST_POST_LOOPBACK_PRE_READY_DELAY_MS",
        "GARDEN_LOOPBACK_TOKEN",
    ] {
        command.env_remove(name);
    }
    command
}

/// Kills (and reaps) the child on drop regardless of test outcome — no
/// leaked `gardend` processes across a failing assertion or early return.
struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn wait_with_timeout(child: &mut Child, timeout: Duration) -> Option<std::process::ExitStatus> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(Some(status)) = child.try_wait() {
            return Some(status);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// A published flush leaves at least one entry directly under `durable_dir`
/// (a `CURRENT` pointer file + a `snap-*` directory — `cell_durability.rs`'s
/// own, already-tested internal shape; this test only needs "something
/// durable landed here", not that internal contract).
fn durable_dir_has_a_published_snapshot(durable_dir: &Path) -> bool {
    std::fs::read_dir(durable_dir)
        .map(|entries| entries.flatten().count() > 0)
        .unwrap_or(false)
}

/// `cell_durability`'s own `CURRENT` pointer, directly under `durable_dir`
/// (a DIFFERENT file from the observatory bundle dir's own `CURRENT` — they
/// live in different directories entirely, `durable_dir` vs.
/// `durable_dir/<bundle-subdir>`; see `boot.rs`'s module doc, "Fixed
/// subdirectory... a sibling of (never inside) cell_durability's own
/// CURRENT/snap-NNNNNN snapshot namespace"). Its content (e.g.
/// `"snap-000002"`) is the published-snapshot identity: it advances only
/// when a flush actually PUBLISHES (content changed since the previous
/// snapshot) — `cell_durability::flush`'s own documented "skips publishing
/// when nothing changed" behavior — so watching it advance is a real,
/// non-mocked signal that a SECOND forced flush actually happened, not just
/// that the periodic ticker woke up.
fn durable_current_snapshot_id(durable_dir: &Path) -> Option<String> {
    std::fs::read_to_string(durable_dir.join("CURRENT"))
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn log_files(dir: &Path, label: &str) -> (File, File) {
    let stdout = File::create(dir.join(format!("{label}.stdout.log"))).expect("create stdout log");
    let stderr = File::create(dir.join(format!("{label}.stderr.log"))).expect("create stderr log");
    (stdout, stderr)
}

fn read_log(dir: &Path, label: &str, stream: &str) -> String {
    std::fs::read_to_string(dir.join(format!("{label}.{stream}.log"))).unwrap_or_default()
}

#[cfg(unix)]
#[test]
fn observatory_gardend_process_forces_a_durable_flush_after_catch_up_before_any_periodic_tick_could_fire(
) {
    let bundle_dir = temp_dir("forced-flush-bundle");
    publish_generation(&bundle_dir, "gen-1", "cursor-process-test");
    let profile_dir = temp_dir("forced-flush-profile");
    let durable_dir = temp_dir("forced-flush-durable");
    let log_dir = temp_dir("forced-flush-logs");
    std::fs::create_dir_all(&profile_dir).expect("create profile dir");
    std::fs::create_dir_all(&durable_dir).expect("create durable dir");
    std::fs::create_dir_all(&log_dir).expect("create log dir");
    prime_observatory_graph(&profile_dir);

    let binary = gardend_binary();
    let (stdout, stderr) = log_files(&log_dir, "run");

    let mut child = ChildGuard(
        gardend_command(&binary)
            .env("GARDEN_PROFILE_DIR", &profile_dir)
            .env("GARDEN_DURABLE_DIR", &durable_dir)
            .env("GARDEN_CELL_GRAPH_ID", "observatory")
            .env("GARDEN_OBSERVATORY_BUNDLE_DIR", &bundle_dir)
            .env("GARDEN_LOOPBACK_HOST", "127.0.0.1")
            .env("GARDEN_LOOPBACK_PORT", "0")
            // Impossibly far off relative to this test's <=30s observation
            // window — a snapshot appearing in `durable_dir` that soon can
            // ONLY be the forced post-catch_up flush this fix adds, not the
            // periodic ticker, an idle-triggered final flush, or the
            // hot-cell observatory re-check ticker (also given an
            // impossibly long interval).
            .env("GARDEN_FLUSH_INTERVAL_SECONDS", "3600")
            .env("GARDEN_OBSERVATORY_REFRESH_INTERVAL_SECONDS", "3600")
            .env("GARDEN_IDLE_TTL_SECONDS", "3600")
            .env("RUST_LOG", "info")
            .stdout(stdout)
            .stderr(stderr)
            .spawn()
            .expect("spawn gardend"),
    );

    let deadline = Instant::now() + Duration::from_secs(30);
    let mut published = false;
    while Instant::now() < deadline {
        if durable_dir_has_a_published_snapshot(&durable_dir) {
            published = true;
            break;
        }
        // The process must still be alive and progressing — an early exit
        // (a boot failure) would otherwise silently starve this poll loop
        // until the deadline instead of failing fast with a useful log.
        if let Ok(Some(status)) = child.0.try_wait() {
            panic!(
                "gardend exited early ({status:?}) before publishing a durable snapshot\n\
                 --- stdout ---\n{}\n--- stderr ---\n{}",
                read_log(&log_dir, "run", "stdout"),
                read_log(&log_dir, "run", "stderr"),
            );
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    if !published {
        let _ = child.0.kill();
        let _ = child.0.wait();
        panic!(
            "expected a durable snapshot in {} within 30s of boot — the forced post-catch_up \
             flush (examples/gardend.rs) never fired; periodic (3600s) and idle-reap (3600s) are \
             both impossibly far off, so nothing else could have produced one this soon\n\
             --- stdout ---\n{}\n--- stderr ---\n{}",
            durable_dir.display(),
            read_log(&log_dir, "run", "stdout"),
            read_log(&log_dir, "run", "stderr"),
        );
    }

    // Wait for gardend to reach ITS OWN signal-handler installation point
    // before signaling it. Signal handling
    // (`tokio::signal::unix::signal(SignalKind::terminate())`) is only
    // installed once `wait_for_shutdown` is entered — the LAST step of
    // gardend's synchronous boot sequence, well after the observatory
    // catch-up/forced-flush this fix runs (which happens BEFORE
    // `wait_until_ready` even exposes the loopback API). A SIGTERM sent
    // before that point hits the OS default disposition (immediate,
    // non-graceful termination — `unix_wait_status` for SIGTERM, not a clean
    // `exit(0)`) — a real, pre-existing property of gardend's boot sequence
    // this fix's reordering does not change (any signal during ANY earlier
    // boot phase — hydrate, setup, `wait_until_ready` itself — has the SAME
    // property). `"durable flush enabled"` is the last `log::info!` line
    // `examples/gardend.rs` emits before calling `wait_for_shutdown`.
    let signal_ready_deadline = Instant::now() + Duration::from_secs(15);
    let mut signal_ready = false;
    while Instant::now() < signal_ready_deadline {
        if read_log(&log_dir, "run", "stderr").contains("durable flush enabled") {
            signal_ready = true;
            break;
        }
        if let Ok(Some(status)) = child.0.try_wait() {
            panic!(
                "gardend exited early ({status:?}) before reaching its own shutdown wait-loop\n\
                 --- stdout ---\n{}\n--- stderr ---\n{}",
                read_log(&log_dir, "run", "stdout"),
                read_log(&log_dir, "run", "stderr"),
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        signal_ready,
        "gardend never logged \"durable flush enabled\" within 15s — cannot safely SIGTERM-test \
         a graceful shutdown\n--- stdout ---\n{}\n--- stderr ---\n{}",
        read_log(&log_dir, "run", "stdout"),
        read_log(&log_dir, "run", "stderr"),
    );

    // NOW fenced by lifecycle shutdown: the observatory work is already
    // fully complete AND gardend has reached its own signal-aware wait-loop,
    // so a SIGTERM here must produce a clean, graceful exit.
    let _ = Command::new("kill")
        .args(["-TERM", &child.0.id().to_string()])
        .status();
    let exit = wait_with_timeout(&mut child.0, Duration::from_secs(25)).unwrap_or_else(|| {
        panic!(
            "gardend did not exit within 25s of SIGTERM\n--- stdout ---\n{}\n--- stderr ---\n{}",
            read_log(&log_dir, "run", "stdout"),
            read_log(&log_dir, "run", "stderr"),
        )
    });
    assert!(
        exit.success(),
        "gardend must exit 0 on a clean SIGTERM shutdown, got {exit:?}\n--- stdout ---\n{}\n--- stderr ---\n{}",
        read_log(&log_dir, "run", "stdout"),
        read_log(&log_dir, "run", "stderr"),
    );

    cleanup(&bundle_dir);
    cleanup(&profile_dir);
    cleanup(&durable_dir);
    cleanup(&log_dir);
}

/// IMAGE-TWO DEBT (panel finding, image-one review): the full escalation test
/// belongs beside A1-T2, which builds exactly the env this needs
/// (`GARDEN_SELF_HEAL_GRAPHS=1`, absent `graph.json`, real gardend, loopback
/// WS client). Required assertions when implemented: (a) the open succeeds and
/// the healed graph's seed writes land while the exclusive lease is held;
/// (b) with a deletion interleaved into the escalation window (advance the
/// graph's generation between the shared drop and the exclusive acquire), the
/// open ABORTS instead of resurrecting the graph — the generation-fence
/// primitive itself is already pinned in-crate by
/// `generation_allows_restart_reset_but_rejects_process_local_advance`.
#[test]
#[ignore = "image-two: composes with A1-T2's env; see doc comment for the required assertions"]
fn heal_escalation_missing_graph_open_runs_heal_under_exclusive() {
    todo!("exercise a missing graph room open with GARDEN_SELF_HEAL_GRAPHS=1");
}

/// The load-bearing negative control: a NON-observatory graph_id (`angels`,
/// the A4 dispatch's own example) boots the SAME binary with the SAME
/// durable dirs configured and must reach ready + idle-reap on its own short
/// TTL WITHOUT ever invoking the observatory boot-hook machinery — proving
/// the graph_id gate really does guarantee zero behavior change for every
/// other cell, in the actual compiled binary, not just by code inspection.
/// No `GARDEN_OBSERVATORY_BUNDLE_DIR` is set either (the production default
/// for a non-observatory cell) — RE-SCOPE r1 has no separate activation env
/// var to leave unset any more.
#[cfg(unix)]
#[test]
fn observatory_gardend_process_non_observatory_graph_boots_and_idle_reaps_normally_with_the_gate_closed(
) {
    let profile_dir = temp_dir("angels-profile");
    let durable_dir = temp_dir("angels-durable");
    let log_dir = temp_dir("angels-logs");
    std::fs::create_dir_all(&profile_dir).expect("create profile dir");
    std::fs::create_dir_all(&durable_dir).expect("create durable dir");
    std::fs::create_dir_all(&log_dir).expect("create log dir");

    let binary = gardend_binary();
    let (stdout, stderr) = log_files(&log_dir, "run");

    let mut child = ChildGuard(
        gardend_command(&binary)
            .env("GARDEN_PROFILE_DIR", &profile_dir)
            .env("GARDEN_DURABLE_DIR", &durable_dir)
            .env("GARDEN_CELL_GRAPH_ID", "angels")
            .env("GARDEN_LOOPBACK_HOST", "127.0.0.1")
            .env("GARDEN_LOOPBACK_PORT", "0")
            .env("GARDEN_FLUSH_INTERVAL_SECONDS", "3600")
            // Short idle TTL — a non-observatory cell must reach ready and
            // then idle-reap entirely on its own, unaffected by anything
            // this fix added.
            .env("GARDEN_IDLE_TTL_SECONDS", "2")
            .env("RUST_LOG", "info")
            .stdout(stdout)
            .stderr(stderr)
            .spawn()
            .expect("spawn gardend"),
    );

    let exit = wait_with_timeout(&mut child.0, Duration::from_secs(30)).unwrap_or_else(|| {
        panic!(
            "a non-observatory cell must idle-reap within 30s on a 2s TTL\n\
             --- stdout ---\n{}\n--- stderr ---\n{}",
            read_log(&log_dir, "run", "stdout"),
            read_log(&log_dir, "run", "stderr"),
        )
    });
    let stdout_text = read_log(&log_dir, "run", "stdout");
    let stderr_text = read_log(&log_dir, "run", "stderr");
    assert!(
        exit.success(),
        "angels must idle-reap cleanly (exit 0), got {exit:?}\n--- stdout ---\n{stdout_text}\n--- stderr ---\n{stderr_text}"
    );
    assert!(
        !stdout_text.contains("observatory") && !stderr_text.contains("observatory boot-hook"),
        "a non-observatory cell must never log anything from the observatory boot-hook\n\
         --- stdout ---\n{stdout_text}\n--- stderr ---\n{stderr_text}"
    );

    cleanup(&profile_dir);
    cleanup(&durable_dir);
    cleanup(&log_dir);
}

/// RE-SCOPE r1's "hot-cell freshness" fix (A4 review SUSPECT), proven
/// end-to-end against the REAL compiled binary: with a short periodic
/// re-check interval and a long idle TTL (the cell stays warm the whole
/// test), publishing a SECOND, genuinely newer generation to the bundle dir
/// WHILE the process is running — no restart — must be picked up and
/// durably flushed, without ever touching `GARDEN_OBSERVATORY_PROJECTOR`
/// (RE-SCOPE r1 has no such env var any more).
#[cfg(unix)]
#[test]
fn observatory_gardend_process_hot_cell_periodic_recheck_applies_a_newer_generation_without_restart(
) {
    let bundle_dir = temp_dir("hot-cell-bundle");
    publish_generation(&bundle_dir, "gen-1", "cursor-hot-cell-process-1");
    let profile_dir = temp_dir("hot-cell-profile");
    let durable_dir = temp_dir("hot-cell-durable");
    let log_dir = temp_dir("hot-cell-logs");
    std::fs::create_dir_all(&profile_dir).expect("create profile dir");
    std::fs::create_dir_all(&durable_dir).expect("create durable dir");
    std::fs::create_dir_all(&log_dir).expect("create log dir");
    prime_observatory_graph(&profile_dir);

    let binary = gardend_binary();
    let (stdout, stderr) = log_files(&log_dir, "run");

    let mut child = ChildGuard(
        gardend_command(&binary)
            .env("GARDEN_PROFILE_DIR", &profile_dir)
            .env("GARDEN_DURABLE_DIR", &durable_dir)
            .env("GARDEN_CELL_GRAPH_ID", "observatory")
            .env("GARDEN_OBSERVATORY_BUNDLE_DIR", &bundle_dir)
            .env("GARDEN_LOOPBACK_HOST", "127.0.0.1")
            .env("GARDEN_LOOPBACK_PORT", "0")
            .env("GARDEN_FLUSH_INTERVAL_SECONDS", "3600")
            // Short — the whole point of this test.
            .env("GARDEN_OBSERVATORY_REFRESH_INTERVAL_SECONDS", "2")
            // Keep the cell warm across the entire test: the periodic
            // re-check must fire without any boot/reboot.
            .env("GARDEN_IDLE_TTL_SECONDS", "3600")
            .env("RUST_LOG", "info")
            .stdout(stdout)
            .stderr(stderr)
            .spawn()
            .expect("spawn gardend"),
    );

    // Wait for the FIRST (boot-time) forced flush to publish.
    let first_deadline = Instant::now() + Duration::from_secs(30);
    let mut first_snapshot = None;
    while Instant::now() < first_deadline {
        if let Some(id) = durable_current_snapshot_id(&durable_dir) {
            first_snapshot = Some(id);
            break;
        }
        if let Ok(Some(status)) = child.0.try_wait() {
            panic!(
                "gardend exited early ({status:?}) before publishing the first durable snapshot\n\
                 --- stdout ---\n{}\n--- stderr ---\n{}",
                read_log(&log_dir, "run", "stdout"),
                read_log(&log_dir, "run", "stderr"),
            );
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let first_snapshot = first_snapshot.unwrap_or_else(|| {
        panic!(
            "expected a first durable snapshot within 30s of boot\n\
             --- stdout ---\n{}\n--- stderr ---\n{}",
            read_log(&log_dir, "run", "stdout"),
            read_log(&log_dir, "run", "stderr"),
        )
    });

    // Now publish a SECOND, genuinely newer generation WHILE the process
    // keeps running — no restart, no re-spawn.
    publish_generation(&bundle_dir, "gen-2", "cursor-hot-cell-process-2");

    // The periodic re-check ticks every 2s; give it generous headroom
    // (well beyond a couple of ticks) before concluding it never fired.
    let second_deadline = Instant::now() + Duration::from_secs(30);
    let mut advanced = false;
    while Instant::now() < second_deadline {
        if let Some(id) = durable_current_snapshot_id(&durable_dir) {
            if id != first_snapshot {
                advanced = true;
                break;
            }
        }
        if let Ok(Some(status)) = child.0.try_wait() {
            panic!(
                "gardend exited early ({status:?}) before the hot-cell re-check could publish a \
                 second snapshot\n--- stdout ---\n{}\n--- stderr ---\n{}",
                read_log(&log_dir, "run", "stdout"),
                read_log(&log_dir, "run", "stderr"),
            );
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    if !advanced {
        let _ = child.0.kill();
        let _ = child.0.wait();
        panic!(
            "expected the durable CURRENT snapshot pointer to advance past {first_snapshot:?} \
             within 30s of publishing a newer generation (2s re-check interval) — the hot-cell \
             periodic re-check never applied+flushed it\n--- stdout ---\n{}\n--- stderr ---\n{}",
            read_log(&log_dir, "run", "stdout"),
            read_log(&log_dir, "run", "stderr"),
        );
    }
    let stderr_text = read_log(&log_dir, "run", "stderr");
    assert!(
        stderr_text.contains("catch_up applied a fresh generation")
            || stderr_text.contains("observatory periodic re-check"),
        "expected an explicit periodic re-check log line, got:\n{stderr_text}"
    );

    let _ = child.0.kill();
    let _ = child.0.wait();

    cleanup(&bundle_dir);
    cleanup(&profile_dir);
    cleanup(&durable_dir);
    cleanup(&log_dir);
}

/// review r2 SUSPECT "the hot-cell periodic re-check covers already-running
/// cells": the OLD shape gated the re-check ticker's own SPAWN on whether a
/// `CURRENT` marker already existed at boot — an observatory cell that came
/// up BEFORE the projector's first-ever publication (or was warm before the
/// projector was turned on for this environment) never got a ticker spawned
/// at all, and would sit warm forever never noticing even the projector's
/// FIRST publication until its next cold start. This test boots with an
/// EMPTY bundle dir (no `CURRENT` at all — the gate is closed at boot,
/// unlike the sibling hot-cell test above, which already has `gen-1`
/// published before the process ever starts), confirms the cell reaches
/// ready without ever calling the observatory machinery, THEN publishes the
/// FIRST generation while the process stays warm — no restart — and proves
/// the periodic re-check still picks it up and durably flushes it. Real
/// compiled binary, real fixture bytes, no mocks.
#[cfg(unix)]
#[test]
fn observatory_gardend_process_warm_cell_boots_with_no_current_then_picks_up_the_first_publication()
{
    let bundle_dir = temp_dir("warm-cell-bundle");
    // Deliberately empty — no `publish_generation` call here. The dir must
    // still EXIST (a real projector CronJob's least-privilege subPath mount
    // means the bundle dir is always present once wired, just possibly
    // empty before the first hourly run) so this test proves the gate-closed
    // path, not a missing-directory edge case.
    std::fs::create_dir_all(&bundle_dir).expect("create empty bundle dir");
    let profile_dir = temp_dir("warm-cell-profile");
    let durable_dir = temp_dir("warm-cell-durable");
    let log_dir = temp_dir("warm-cell-logs");
    std::fs::create_dir_all(&profile_dir).expect("create profile dir");
    std::fs::create_dir_all(&durable_dir).expect("create durable dir");
    std::fs::create_dir_all(&log_dir).expect("create log dir");
    prime_observatory_graph(&profile_dir);

    let binary = gardend_binary();
    let (stdout, stderr) = log_files(&log_dir, "run");

    let mut child = ChildGuard(
        gardend_command(&binary)
            .env("GARDEN_PROFILE_DIR", &profile_dir)
            .env("GARDEN_DURABLE_DIR", &durable_dir)
            .env("GARDEN_CELL_GRAPH_ID", "observatory")
            .env("GARDEN_OBSERVATORY_BUNDLE_DIR", &bundle_dir)
            .env("GARDEN_LOOPBACK_HOST", "127.0.0.1")
            .env("GARDEN_LOOPBACK_PORT", "0")
            .env("GARDEN_FLUSH_INTERVAL_SECONDS", "3600")
            // Under the dirty-driven flush (merged in a8d2199),
            // GARDEN_FLUSH_INTERVAL_SECONDS is only the max-RPO CEILING — the
            // DEBOUNCE (default 5s) is what actually schedules a flush after
            // real boot activity, and gardend's boot performs at least one
            // epoch-bumping write with no client attached at all:
            // `LocalJobRegistry::new` → `local_job_db::with_job_connection`
            // holds `cell_durability::write_guard()` across the first-use
            // `jobs.turso` open/CREATE TABLE, and the guard's Drop calls
            // `mark_registered_stores_written()`. On a release-fast box that
            // puts an ordinary CELL-WIDE snapshot publish (~boot+5s) inside
            // this test's 8s "nothing published yet" window (debug builds
            // boot slowly enough to hide it — the failure was release-only).
            // Pin the debounce out of reach so the ONLY thing that can
            // publish during this test is the observatory machinery's own
            // FORCED flush (`run_flush_blocking(.., true)` bypasses the
            // scheduler entirely) — which is exactly what phase 2 detects.
            .env("GARDEN_FLUSH_DEBOUNCE_SECONDS", "3600")
            // Short — the whole point of this test: the ticker must exist
            // and tick even though the gate was closed at boot.
            .env("GARDEN_OBSERVATORY_REFRESH_INTERVAL_SECONDS", "2")
            .env("GARDEN_IDLE_TTL_SECONDS", "3600")
            .env("RUST_LOG", "info")
            .stdout(stdout)
            .stderr(stderr)
            .spawn()
            .expect("spawn gardend"),
    );

    // Give the process a real chance to boot AND run a few 2s re-check ticks
    // against the still-empty bundle dir. It must stay alive (no crash/exit)
    // and must NOT publish anything durable yet — there is nothing to
    // publish (the gate closes cleanly every tick: `GateClosed`, not a
    // hang, not an error).
    let quiet_deadline = Instant::now() + Duration::from_secs(8);
    while Instant::now() < quiet_deadline {
        if let Ok(Some(status)) = child.0.try_wait() {
            panic!(
                "gardend exited early ({status:?}) while the bundle dir was still empty — the \
                 gate-closed path must be a strict, crash-free no-op\n\
                 --- stdout ---\n{}\n--- stderr ---\n{}",
                read_log(&log_dir, "run", "stdout"),
                read_log(&log_dir, "run", "stderr"),
            );
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    assert!(
        !durable_dir_has_a_published_snapshot(&durable_dir),
        "no durable snapshot should exist yet — the bundle dir never had a CURRENT marker, and \
         the ordinary dirty-driven scheduler is debounce-pinned out of this test's life (see the \
         GARDEN_FLUSH_DEBOUNCE_SECONDS env comment), so only an observatory forced flush could \
         have published"
    );
    // The DIRECT gate-closed invariant (not the durable-plane proxy above):
    // with no CURRENT marker ever published, neither the boot-time hook nor
    // any 2s re-check tick may have applied — or attempted and failed — a
    // catch_up. Matches every apply log site (`catch_up applied on boot`,
    // `catch_up applied generation`, `catch_up applied a fresh generation`)
    // and both failure sites (`observatory boot-hook failed`, `catch_up
    // failed`) in examples/gardend.rs / observatory::boot.
    let gate_closed_stderr = read_log(&log_dir, "run", "stderr");
    assert!(
        !gate_closed_stderr.contains("catch_up applied"),
        "the observatory machinery must not APPLY anything while the bundle dir has no CURRENT \
         marker\n--- stderr ---\n{gate_closed_stderr}"
    );
    assert!(
        !gate_closed_stderr.contains("catch_up failed")
            && !gate_closed_stderr.contains("observatory boot-hook failed"),
        "the gate-closed path must be a strict no-op, not a swallowed error\n--- stderr ---\n{gate_closed_stderr}"
    );

    // NOW publish the FIRST generation — the projector's very first hourly
    // run, landing while this cell has been warm the whole time.
    publish_generation(&bundle_dir, "gen-1", "cursor-warm-cell-first-publish");

    let deadline = Instant::now() + Duration::from_secs(30);
    let mut published = false;
    while Instant::now() < deadline {
        if durable_dir_has_a_published_snapshot(&durable_dir) {
            published = true;
            break;
        }
        if let Ok(Some(status)) = child.0.try_wait() {
            panic!(
                "gardend exited early ({status:?}) before publishing a durable snapshot for the \
                 first-ever generation\n--- stdout ---\n{}\n--- stderr ---\n{}",
                read_log(&log_dir, "run", "stdout"),
                read_log(&log_dir, "run", "stderr"),
            );
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    if !published {
        let _ = child.0.kill();
        let _ = child.0.wait();
        panic!(
            "expected the WARM cell's periodic re-check (2s interval, running the whole time \
             despite the gate being closed at boot) to notice and apply the projector's FIRST-ever \
             publication within 30s — this is exactly the SUSPECT regression review r2 fixes: the \
             ticker must be spawned whenever graph_id == observatory, not only when CURRENT already \
             existed at boot\n--- stdout ---\n{}\n--- stderr ---\n{}",
            read_log(&log_dir, "run", "stdout"),
            read_log(&log_dir, "run", "stderr"),
        );
    }
    let stderr_text = read_log(&log_dir, "run", "stderr");
    assert!(
        stderr_text.contains("catch_up applied a fresh generation")
            || stderr_text.contains("observatory periodic re-check"),
        "expected an explicit periodic re-check log line, got:\n{stderr_text}"
    );

    let _ = child.0.kill();
    let _ = child.0.wait();

    cleanup(&bundle_dir);
    cleanup(&profile_dir);
    cleanup(&durable_dir);
    cleanup(&log_dir);
}
