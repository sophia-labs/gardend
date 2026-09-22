//! A4 acceptance gates — boot-hook gated catch-up + durable cursor (garden
//! half), §5 "A4 — Sidecar wiring + boot-hook catch-up + cursor durability"
//! of `plans/observatory-analysis-cell-spec-20260715.md` in the `sophia` hub
//! repo, RE-SCOPED (2026-07-16, r1) to a standalone EFS-bundle projector +
//! an immutable-generation/`CURRENT`-pointer file contract.
//!
//! REAL `Store` (`garden_lib::observatory::authority_harness::open_real_store`
//! — the exact function `rdf_service.rs` opens for every RDF request), REAL
//! `run_boot_hook`/`run_catch_up_from_bundle_dir`
//! (`garden_lib::observatory::boot`), REAL A3 `catch_up`, REAL fixture bytes
//! (`../src/observatory/fixtures/*`, the SAME files A2's/A3's own tests
//! already validate against — assembled into a real bundle-directory
//! file-contract directory, not a fabricated shape), and REAL
//! `garden_lib::headless::durability::{flush_forced, hydrate_detailed}` for
//! the reap+reboot proof. No mocks anywhere in this file.
//!
//! Gates (the A4 dispatch's own lettering, plus r1 additions):
//! (a) [`observatory_boot_gate_a_ready_bundle_materializes_both_graphs_with_fresh_projected_through`]
//! (b) [`observatory_boot_gate_b_reap_reboot_rehydrates_and_reconverges_to_no_delta_catch_up`]
//! (c) [`observatory_boot_gate_c_gate_closed_no_current_marker_is_a_strict_noop`] +
//!     [`observatory_boot_gate_c_gate_closed_wrong_graph_id_is_a_strict_noop_even_with_a_real_current_marker`]
//! plus the documented "never hang cell boot" bound:
//! [`observatory_boot_bundle_not_ready_within_bound_skips_cleanly_never_hangs`],
//! plus the RE-SCOPE proof that the bundle dir defaults to a fixed
//! subdirectory of the cell's OWN `GARDEN_DURABLE_DIR` EFS mount:
//! [`observatory_boot_default_bundle_dir_derives_from_the_cells_existing_durable_dir_when_no_override_is_set`],
//! plus r1's hot-cell freshness proof (a SECOND call to `run_boot_hook`
//! against the SAME warm process picks up a newer published generation
//! without a restart):
//! [`observatory_boot_hot_cell_second_call_short_circuits_then_picks_up_a_newer_generation`].

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use garden_lib::headless::durability::{flush_forced, hydrate_detailed, HydrateMode};
use garden_lib::observatory::authority_harness::{graph_has_any_quad, open_real_store};
use garden_lib::observatory::boot::{
    await_bundle_ready, bundle_json_path, raw_snapshot_path, reset_last_applied_token_for_tests,
    run_boot_hook, run_catch_up_from_bundle_dir, BootHookOutcome, CatchUpFromDirError,
    BUNDLE_DIR_ENV_VAR, BUNDLE_WAIT_SECONDS_ENV_VAR, CURRENT_POINTER_FILE,
    DEFAULT_BUNDLE_DIR_SUBPATH,
};
use garden_lib::observatory::graph_identity::{raw_graph_iri, rollups_graph_iri, GRAPH_ID};

use oxigraph::model::Term as OxTerm;
use oxigraph::sparql::{QueryResults, SparqlEvaluator};
use oxigraph::store::Store;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Fixture bytes (REAL — the same files apply.rs's/mapping.rs's own tests use)
// ---------------------------------------------------------------------------

const LIFECYCLE_JSON: &str =
    include_str!("../src/observatory/fixtures/lifecycle_evaluation.input.json");
const BILLING_JSON: &str = include_str!("../src/observatory/fixtures/billing_llm_dau_v1.ok.json");
const RAW_NDJSON: &str = include_str!("../src/observatory/fixtures/valid.ndjson");

const OBS_NS: &str = "http://mnemosyne.dev/observatory#";

/// Fixture's own `projected_through` (`lifecycle_evaluation.input.json`),
/// passed through verbatim by `map_lifecycle`/`catch_up` — the SAME ground
/// truth `tests/observatory_apply.rs`'s own Gate 4 already proved. Asserting
/// against it here (rather than merely "non-empty") proves THIS boot's
/// catch_up actually ran and wrote it, not a stale prior value.
const FIXTURE_PROJECTED_THROUGH: &str = "2026-07-15T09:00:00.018Z";

/// A cell not gated open by A4 — cloud-2 runs this SAME `gardend` binary for
/// every graph, including production graphs like `angels` (per the A4
/// dispatch's own load-bearing example).
const NON_OBSERVATORY_GRAPH_ID: &str = "angels";

// ---------------------------------------------------------------------------
// Env-var serialization — every test that touches
// GARDEN_OBSERVATORY_BUNDLE_DIR / _BUNDLE_WAIT_SECONDS / GARDEN_DURABLE_DIR
// must hold this for its whole body: `cargo test` runs `#[test]` fns in this
// binary on a thread pool by default, and these are process-wide env vars.
// Mirrors `cell_durability.rs`'s own `test_serial()` precedent for the SAME
// class of hazard (shared process-global state across parallel test
// threads).
// ---------------------------------------------------------------------------

/// `garden_lib::observatory::boot`'s own (non-`pub`) constant for the cell's
/// existing EFS durable mount — duplicated here as a literal, exactly the way
/// `examples/gardend.rs`/`tests/observatory_gardend_process.rs` already read
/// `GARDEN_DURABLE_DIR` directly rather than through a shared constant.
const DURABLE_DIR_ENV_VAR: &str = "GARDEN_DURABLE_DIR";

fn env_serial() -> &'static Mutex<()> {
    static LOCK: std::sync::OnceLock<Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

struct EnvGuard {
    _lock: std::sync::MutexGuard<'static, ()>,
}

impl EnvGuard {
    /// Acquire the serialization lock and set (or clear) the boot-hook env
    /// vars for the guard's lifetime. `None` clears the var (matches
    /// production's "unset" state); `Some(value)` sets it. `durable_dir` is
    /// `None` at every existing call site (those tests always drive the
    /// `bundle_dir` override directly) — only the RE-SCOPE default-resolution
    /// test below sets it, proving the `GARDEN_DURABLE_DIR`-derived default
    /// path without an override present.
    fn acquire(
        bundle_dir: Option<&Path>,
        bundle_wait_seconds: Option<&str>,
        durable_dir: Option<&Path>,
    ) -> Self {
        let lock = env_serial()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        apply_env(
            BUNDLE_DIR_ENV_VAR,
            bundle_dir
                .map(|p| p.to_string_lossy().into_owned())
                .as_deref(),
        );
        apply_env(BUNDLE_WAIT_SECONDS_ENV_VAR, bundle_wait_seconds);
        apply_env(
            DURABLE_DIR_ENV_VAR,
            durable_dir
                .map(|p| p.to_string_lossy().into_owned())
                .as_deref(),
        );
        Self { _lock: lock }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        // Leave the process exactly as every other test binary would find
        // it: no boot-hook env var set.
        apply_env(BUNDLE_DIR_ENV_VAR, None);
        apply_env(BUNDLE_WAIT_SECONDS_ENV_VAR, None);
        apply_env(DURABLE_DIR_ENV_VAR, None);
    }
}

fn apply_env(var: &str, value: Option<&str>) {
    // SAFETY: serialized by `env_serial()` — no other thread in this process
    // reads/writes these vars concurrently (every caller goes through
    // `EnvGuard::acquire`).
    unsafe {
        match value {
            Some(value) => std::env::set_var(var, value),
            None => std::env::remove_var(var),
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn temp_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "sophia-observatory-boot-{label}-{}",
        Uuid::new_v4()
    ));
    dir
}

fn cleanup(dir: &Path) {
    let _ = std::fs::remove_dir_all(dir);
}

/// `obs-bundle.<token>.json`'s wire shape (A3's `ObsBundleJson` — see
/// `apply.rs`'s module doc): wraps A2's own per-eval JSON verbatim. Mirrors
/// `tests/observatory_apply.rs`'s own `wrap_bundle` helper byte-for-byte.
fn full_bundle(cursor: &str) -> String {
    format!(
        "{{\"cursor_high_water_mark\":{cursor:?},\"lifecycle\":[{LIFECYCLE_JSON}],\"billing\":[{BILLING_JSON}]}}"
    )
}

/// Publish one REAL, immutable generation into `dir` (creating it if
/// needed): `obs-bundle.<token>.json` + `raw-snapshot.<token>.ndjson` from
/// the real fixture bytes, written BEFORE `CURRENT` is (re)pointed at
/// `token` — the projector's own publish ordering (RE-SCOPE r1 — see
/// `boot.rs`'s module doc's "generation protocol"), so a reader can never
/// observe `CURRENT` naming a token whose artifacts are not already fully
/// present.
fn publish_generation(dir: &Path, token: &str, cursor: &str) {
    std::fs::create_dir_all(dir).expect("create bundle dir");
    std::fs::write(bundle_json_path(dir, token), full_bundle(cursor))
        .expect("write obs-bundle.json");
    std::fs::write(raw_snapshot_path(dir, token), RAW_NDJSON).expect("write raw-snapshot.ndjson");
    std::fs::write(dir.join(CURRENT_POINTER_FILE), token).expect("publish CURRENT");
}

/// Assemble a REAL, ready single-generation bundle directory at a fresh temp
/// path (the `GARDEN_OBSERVATORY_BUNDLE_DIR`-override shape every gate test
/// below uses). `label` doubles as the generation token (already unique per
/// test via `temp_dir`'s own UUID suffix, but a distinct token per call
/// keeps intent obvious at each call site).
fn ready_bundle_dir(label: &str, cursor: &str) -> PathBuf {
    let dir = temp_dir(&format!("bundle-{label}"));
    publish_generation(&dir, "gen-1", cursor);
    dir
}

/// A bundle directory that exists but deliberately never gets a `CURRENT`
/// pointer — the projector-still-running (or never-activated) case.
fn not_ready_bundle_dir(label: &str) -> PathBuf {
    let dir = temp_dir(&format!("bundle-not-ready-{label}"));
    std::fs::create_dir_all(&dir).expect("create bundle dir");
    dir
}

fn tokio_rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime")
}

fn graph_has_any_quad_expect(store: &Store, graph_iri: &str) -> bool {
    graph_has_any_quad(store, graph_iri).expect("graph_has_any_quad")
}

fn single_literal(store: &Store, graph_iri: &str, type_iri: &str, predicate_iri: &str) -> String {
    let query = format!(
        "SELECT ?v WHERE {{ GRAPH <{graph_iri}> {{ ?s a <{type_iri}> ; <{predicate_iri}> ?v }} }}"
    );
    let results = SparqlEvaluator::new()
        .parse_query(&query)
        .expect("parse literal query")
        .on_store(store)
        .execute()
        .expect("execute literal query");
    let QueryResults::Solutions(solutions) = results else {
        panic!("expected SELECT solutions")
    };
    let rows: Vec<_> = solutions.collect();
    assert_eq!(
        rows.len(),
        1,
        "expected exactly one {predicate_iri} row, got {}",
        rows.len()
    );
    let row = rows.into_iter().next().unwrap().expect("row ok");
    match row.get("v").expect("?v bound") {
        OxTerm::Literal(lit) => lit.value().to_string(),
        other => panic!("{predicate_iri} is not a literal: {other:?}"),
    }
}

/// Every `(?s,?p,?o)` in `graph_iri`, as a canonical line set — the
/// byte-identical comparator used to prove the hydrate+reconverge round
/// trip lost nothing and gained nothing.
fn graph_triples(store: &Store, graph_iri: &str) -> std::collections::BTreeSet<String> {
    let query = format!("SELECT ?s ?p ?o WHERE {{ GRAPH <{graph_iri}> {{ ?s ?p ?o }} }}");
    let results = SparqlEvaluator::new()
        .parse_query(&query)
        .expect("parse graph dump")
        .on_store(store)
        .execute()
        .expect("execute graph dump");
    let QueryResults::Solutions(solutions) = results else {
        panic!("expected SELECT solutions")
    };
    solutions
        .map(|row| {
            let row = row.expect("graph dump row");
            let s = row.get("s").expect("s bound").to_string();
            let p = row.get("p").expect("p bound").to_string();
            let o = row.get("o").expect("o bound").to_string();
            format!("{s} {p} {o} .")
        })
        .collect()
}

/// Open a real on-disk store at the SAME relative path a real cell boot
/// would use: `<profile_dir>/graphs/<GRAPH_ID>/`. Load-bearing for the
/// reap+reboot gate (b): `cell_durability::flush_forced` only backs up
/// stores whose path is nested under the `profile_dir` it is given, so the
/// store MUST live at this exact convention
/// (`graph_paths::existing_graph_dir` / `profile_paths::graphs_dir` —
/// `profile_dir.join("graphs").join(graph_id)`) for the real durability
/// primitives to find and checkpoint it.
fn open_store_under_profile(profile_dir: &Path) -> (PathBuf, std::sync::Arc<Store>) {
    let graph_dir = profile_dir.join("graphs").join(GRAPH_ID);
    // Mirrors production's `graph_paths::ensure_graph_content_dirs` side
    // effect (a real cell boot creates the graph dir before ever opening its
    // store) — `Store::open` itself only `mkdir`s its own leaf
    // `store.oxigraph`, not missing ancestors.
    std::fs::create_dir_all(&graph_dir).expect("create graph dir under profile dir");
    let store = open_real_store(&graph_dir).expect("open real store under profile dir");
    (graph_dir, store)
}

// ---------------------------------------------------------------------------
// Gate (a) — gate ON + graph_id observatory + a ready generation: boot
// materializes both graphs, obs:projectedThrough is fresh.
// ---------------------------------------------------------------------------

#[test]
fn observatory_boot_gate_a_ready_bundle_materializes_both_graphs_with_fresh_projected_through() {
    // Compute every path BEFORE acquiring the env guard — `EnvGuard` holds
    // a non-reentrant `Mutex`, so it must be acquired exactly once per test.
    let profile_dir = temp_dir("gate-a-profile");
    let (_graph_dir, store) = open_store_under_profile(&profile_dir);
    let bundle_dir = ready_bundle_dir("gate-a", "cursor-gate-a");
    // Route the hook at this real bundle dir via the test/dev override
    // instead of relying on the GARDEN_DURABLE_DIR-derived default.
    let _env = EnvGuard::acquire(Some(&bundle_dir), None, None);

    let raw = raw_graph_iri();
    let rollups = rollups_graph_iri();
    assert!(
        !graph_has_any_quad_expect(&store, &raw),
        "raw graph starts empty"
    );
    assert!(
        !graph_has_any_quad_expect(&store, &rollups),
        "rollups graph starts empty"
    );

    let outcome = tokio_rt().block_on(run_boot_hook(GRAPH_ID, &store));

    let BootHookOutcome::Applied(report) = outcome else {
        panic!("expected Applied, got {outcome:?}");
    };
    assert!(
        report.raw.added > 0,
        "raw lane must materialize CaptureEvents"
    );
    assert!(
        report.rollups.added > 0,
        "rollups lane must materialize rollup subjects"
    );
    assert_eq!(report.raw.removed, 0, "fresh boot has no raw removes");
    assert_eq!(
        report.rollups.removed, 0,
        "fresh boot has no rollups removes"
    );

    assert!(
        graph_has_any_quad_expect(&store, &raw),
        "raw graph materialized"
    );
    assert!(
        graph_has_any_quad_expect(&store, &rollups),
        "rollups graph materialized"
    );

    let projected_through = single_literal(
        &store,
        &rollups,
        &format!("{OBS_NS}ProjectionRun"),
        &format!("{OBS_NS}projectedThrough"),
    );
    assert_eq!(
        projected_through, FIXTURE_PROJECTED_THROUGH,
        "obs:projectedThrough must be THIS boot's fresh value, not stale/absent"
    );

    cleanup(&profile_dir);
    cleanup(&bundle_dir);
}

// ---------------------------------------------------------------------------
// Gate (b) — reap+reboot: drop the in-memory store, re-open from the
// persisted (durable) path via REAL flush_forced/hydrate_detailed, re-run
// the hook against the SAME projector bundle. Must converge to a no-delta
// catch-up — the cursor/cache is durable, not lost on reap.
// ---------------------------------------------------------------------------

#[test]
fn observatory_boot_gate_b_reap_reboot_rehydrates_and_reconverges_to_no_delta_catch_up() {
    // Compute every path BEFORE acquiring the env guard (single acquire —
    // see the note in gate (a)).
    let durable_dir = temp_dir("gate-b-durable");
    let bundle_dir = ready_bundle_dir("gate-b", "cursor-gate-b");
    let profile_dir_1 = temp_dir("gate-b-profile-1");
    let _env = EnvGuard::acquire(Some(&bundle_dir), None, None);

    // --- Boot 1: first incarnation, applies the fixture bundle for real. ---
    let (graph_dir_1, store_1) = open_store_under_profile(&profile_dir_1);

    let first = tokio_rt().block_on(run_boot_hook(GRAPH_ID, &store_1));
    let BootHookOutcome::Applied(first_report) = first else {
        panic!("expected first boot to Apply, got {first:?}");
    };
    assert!(first_report.raw.added > 0 && first_report.rollups.added > 0);

    let raw = raw_graph_iri();
    let rollups = rollups_graph_iri();
    let raw_before = graph_triples(&store_1, &raw);
    let rollups_before = graph_triples(&store_1, &rollups);
    assert!(!raw_before.is_empty() && !rollups_before.is_empty());

    // --- Reap: flush the live store's REAL RocksDB checkpoint to the
    // durable dir (the ONLY durable writable substrate the cell owns,
    // §A.3), then drop this incarnation's in-memory handle entirely — the
    // pod is gone; only `durable_dir` (EFS) survives.
    let flush_outcome =
        flush_forced(&profile_dir_1, &durable_dir).expect("flush_forced backs up the store");
    assert!(
        flush_outcome.published,
        "flush must publish a durable snapshot"
    );
    assert!(
        flush_outcome.stores_backed_up >= 1,
        "the observatory store must be checkpointed"
    );
    drop(store_1);
    drop(graph_dir_1);

    // --- Boot 2: a genuinely NEW profile dir (fresh local disk — a new
    // pod), hydrated from the SAME durable dir, exactly gardend's own boot
    // sequence ("hydrates store from EFS" before touching the store). This
    // is a NEW process incarnation in production, where the hot-cell
    // short-circuit's in-process "last applied" registry starts empty by
    // construction (a fresh process never carries it over) — this test
    // simulates that boundary explicitly via
    // `reset_last_applied_token_for_tests` (both "incarnations" necessarily
    // share one `cargo test` OS process; see that function's own doc), so
    // this boot exercises the REAL content-level idempotency `catch_up`'s
    // survey→diff→apply provides, not merely the in-process short-circuit
    // skipping the read entirely.
    reset_last_applied_token_for_tests();
    let profile_dir_2 = temp_dir("gate-b-profile-2");
    let hydrate_outcome = hydrate_detailed(&profile_dir_2, &durable_dir)
        .expect("hydrate_detailed restores the snapshot");
    assert_eq!(
        hydrate_outcome.mode,
        HydrateMode::Restored,
        "a fresh profile dir with a durable snapshot present must restore, not start fresh"
    );

    let (_graph_dir_2, store_2) = open_store_under_profile(&profile_dir_2);
    // Sanity: hydration alone (before ANY second catch_up) already carried
    // the previously-materialized content across — proves durability is
    // real, not merely "the second catch_up happens to reconverge anyway".
    assert_eq!(
        graph_triples(&store_2, &raw),
        raw_before,
        "raw graph survives reap+reboot byte-identical"
    );
    assert_eq!(
        graph_triples(&store_2, &rollups),
        rollups_before,
        "rollups graph (incl. the cursor/freshness triples) survives reap+reboot byte-identical"
    );

    // --- Re-run the boot-hook against the SAME projector bundle (same
    // token, same content). The idempotent full-window recompute means this
    // converges to a TRUE no-op, not a re-apply.
    let second = tokio_rt().block_on(run_boot_hook(GRAPH_ID, &store_2));
    let BootHookOutcome::Applied(second_report) = second else {
        panic!("expected second (post-reboot) boot to Apply (possibly no-delta), got {second:?}");
    };
    assert!(
        second_report.raw.is_empty(),
        "post-reboot catch_up on the SAME window must be a raw no-op, got {:?}",
        second_report.raw
    );
    assert!(
        second_report.rollups.is_empty(),
        "post-reboot catch_up on the SAME bundle must be a rollups no-op, got {:?}",
        second_report.rollups
    );
    assert_eq!(
        graph_triples(&store_2, &raw),
        raw_before,
        "no-delta catch-up must not perturb raw content"
    );
    assert_eq!(
        graph_triples(&store_2, &rollups),
        rollups_before,
        "no-delta catch-up must not perturb rollups content"
    );

    cleanup(&profile_dir_1);
    cleanup(&profile_dir_2);
    cleanup(&durable_dir);
    cleanup(&bundle_dir);
}

// ---------------------------------------------------------------------------
// Gate (c) — the gate closed: strict no-op. No store writes, no file reads,
// no boot delay. Two independent closures of the SAME gate: no CURRENT
// marker at all, and a real CURRENT marker but the wrong graph_id.
// ---------------------------------------------------------------------------

/// Shared body for both gate-closed cases: assert `GateClosed`, assert it
/// returned near-instantly (proves it never entered the bounded
/// `await_bundle_ready` poll — regression would take up to the DEFAULT 30s
/// budget, since this test deliberately leaves
/// `GARDEN_OBSERVATORY_BUNDLE_WAIT_SECONDS` unset), and assert neither
/// reserved graph gained a single quad (proves no store write, and — since
/// the only way a write could happen is via a successful bundle-dir read —
/// transitively proves no artifact file was read either).
fn assert_strict_noop(own_graph_id: &str, store: &std::sync::Arc<Store>) {
    let raw = raw_graph_iri();
    let rollups = rollups_graph_iri();
    assert!(
        !graph_has_any_quad_expect(store, &raw),
        "raw graph starts empty"
    );
    assert!(
        !graph_has_any_quad_expect(store, &rollups),
        "rollups graph starts empty"
    );

    let started = Instant::now();
    let outcome = tokio_rt().block_on(run_boot_hook(own_graph_id, store));
    let elapsed = started.elapsed();

    assert!(
        matches!(outcome, BootHookOutcome::GateClosed),
        "expected GateClosed for graph_id {own_graph_id:?}, got {outcome:?}"
    );
    assert!(
        elapsed < Duration::from_millis(500),
        "a closed gate must return near-instantly (no boot delay); took {elapsed:?} — \
         a regression that lets the gate open would instead wait up to the default 30s bundle-ready budget"
    );

    assert!(
        !graph_has_any_quad_expect(store, &raw),
        "GateClosed must write NOTHING to the raw graph"
    );
    assert!(
        !graph_has_any_quad_expect(store, &rollups),
        "GateClosed must write NOTHING to the rollups graph"
    );
}

#[test]
fn observatory_boot_gate_c_gate_closed_no_current_marker_is_a_strict_noop() {
    // A real, existing (but empty — no CURRENT written) bundle dir at this
    // path — if the gate were ever bypassed, `await_bundle_ready` would poll
    // it (finding no CURRENT, a silent `None` from `read_current_token`, no
    // panic) for the full default 30s bound, which `assert_strict_noop`'s
    // timing assertion would catch.
    let bundle_dir = not_ready_bundle_dir("gate-c-no-current");
    let _env = EnvGuard::acquire(Some(&bundle_dir), None, None);
    let profile_dir = temp_dir("gate-c-no-current-profile");
    let (_graph_dir, store) = open_store_under_profile(&profile_dir);

    assert_strict_noop(GRAPH_ID, &store);

    cleanup(&profile_dir);
    cleanup(&bundle_dir);
}

#[test]
fn observatory_boot_gate_c_gate_closed_wrong_graph_id_is_a_strict_noop_even_with_a_real_current_marker(
) {
    // The load-bearing case named by the A4 dispatch itself: the SAME
    // `gardend` binary also boots production graphs like `angels`. Even
    // with a REAL, ready CURRENT marker present (as it would be for the
    // observatory cell's own bundle dir, or a misconfigured shared mount), a
    // non-observatory graph_id must still close the gate.
    let bundle_dir = ready_bundle_dir("gate-c-wrong-graph", "cursor-gate-c-wrong-graph");
    let _env = EnvGuard::acquire(Some(&bundle_dir), None, None);
    let profile_dir = temp_dir("gate-c-wrong-graph-profile");
    let (_graph_dir, store) = open_store_under_profile(&profile_dir);

    assert_strict_noop(NON_OBSERVATORY_GRAPH_ID, &store);

    cleanup(&profile_dir);
    cleanup(&bundle_dir);
}

// ---------------------------------------------------------------------------
// Extra — the documented "never hang cell boot" bound, proven directly
// (not merely implied by gate (c)'s timing check on the CLOSED-gate path):
// a bundle dir that exists (so the gate itself is well-formed to poll) but
// whose CURRENT points at a token whose artifacts never land must still
// return within the configured bound, never hang.
// ---------------------------------------------------------------------------

#[test]
fn observatory_boot_bundle_not_ready_within_bound_skips_cleanly_never_hangs() {
    let bundle_dir = not_ready_bundle_dir("never-hangs");
    // CURRENT names a token, but neither artifact file for it ever lands —
    // `await_bundle_ready` must keep polling (never treat a bare CURRENT as
    // sufficient) until the SAME configured bound elapses.
    std::fs::write(bundle_dir.join(CURRENT_POINTER_FILE), "gen-never-lands")
        .expect("publish a CURRENT pointing at a generation that never lands");
    // A short override (1s) keeps this test fast while proving the SAME
    // bounded-wait code path production runs at 30s.
    let _env = EnvGuard::acquire(Some(&bundle_dir), Some("1"), None);
    let profile_dir = temp_dir("bundle-not-ready-profile");
    let (_graph_dir, store) = open_store_under_profile(&profile_dir);

    let started = Instant::now();
    let outcome = tokio_rt().block_on(run_boot_hook(GRAPH_ID, &store));
    let elapsed = started.elapsed();

    let BootHookOutcome::BundleNotReady { waited } = outcome else {
        panic!("expected BundleNotReady, got {outcome:?}");
    };
    assert_eq!(
        waited,
        Duration::from_secs(1),
        "must respect the configured bound, not the 30s default"
    );
    assert!(
        elapsed < Duration::from_secs(3),
        "must return promptly after the bound elapses, never hang; took {elapsed:?}"
    );

    let raw = raw_graph_iri();
    let rollups = rollups_graph_iri();
    assert!(
        !graph_has_any_quad_expect(&store, &raw),
        "a never-ready bundle must not partially apply raw"
    );
    assert!(
        !graph_has_any_quad_expect(&store, &rollups),
        "a never-ready bundle must not partially apply rollups"
    );

    cleanup(&profile_dir);
    cleanup(&bundle_dir);
}

// ---------------------------------------------------------------------------
// RE-SCOPE — default bundle-dir resolution: with NO
// GARDEN_OBSERVATORY_BUNDLE_DIR override set, the bundle dir must default to
// a fixed subdirectory of GARDEN_DURABLE_DIR — the cell's OWN existing EFS
// mount — so a standalone projector CronJob can drop the bundle there
// without any cell-pod-spec change. Real fixture bytes, real Store, no
// override env set; only GARDEN_DURABLE_DIR, exactly a real observatory
// cell's env (no separate activation env var either — RE-SCOPE r1: the
// generation itself, published at the default-derived path, IS what opens
// the gate).
// ---------------------------------------------------------------------------

#[test]
fn observatory_boot_default_bundle_dir_derives_from_the_cells_existing_durable_dir_when_no_override_is_set(
) {
    // Compute every path BEFORE acquiring the env guard (single acquire —
    // see the note in gate (a)).
    let durable_dir = temp_dir("default-bundle-durable");
    std::fs::create_dir_all(&durable_dir).expect("create durable dir");
    // Publish the fixture generation exactly where the documented default
    // (no override) resolves to: <GARDEN_DURABLE_DIR>/<DEFAULT_BUNDLE_DIR_SUBPATH>.
    let expected_bundle_dir = durable_dir.join(DEFAULT_BUNDLE_DIR_SUBPATH);
    publish_generation(&expected_bundle_dir, "gen-default", "cursor-default-dir");
    let profile_dir = temp_dir("default-bundle-profile");
    let (_graph_dir, store) = open_store_under_profile(&profile_dir);
    // Deliberately NO bundle_dir override — only GARDEN_DURABLE_DIR.
    let _env = EnvGuard::acquire(None, None, Some(&durable_dir));

    let raw = raw_graph_iri();
    let rollups = rollups_graph_iri();
    assert!(
        !graph_has_any_quad_expect(&store, &raw),
        "raw graph starts empty"
    );
    assert!(
        !graph_has_any_quad_expect(&store, &rollups),
        "rollups graph starts empty"
    );

    let outcome = tokio_rt().block_on(run_boot_hook(GRAPH_ID, &store));
    let BootHookOutcome::Applied(report) = outcome else {
        panic!("expected Applied via the default EFS-derived bundle dir, got {outcome:?}");
    };
    assert!(
        report.raw.added > 0,
        "raw lane must materialize from the default-derived dir"
    );
    assert!(
        report.rollups.added > 0,
        "rollups lane must materialize from the default-derived dir"
    );
    assert!(
        graph_has_any_quad_expect(&store, &raw),
        "raw graph materialized via the default-derived dir"
    );
    assert!(
        graph_has_any_quad_expect(&store, &rollups),
        "rollups graph materialized via the default-derived dir"
    );

    cleanup(&profile_dir);
    cleanup(&durable_dir);
}

// ---------------------------------------------------------------------------
// r1 — hot-cell freshness: a SECOND call to `run_boot_hook` against the SAME
// warm process (no restart) short-circuits when nothing changed, then picks
// up a genuinely newer published generation. This is exactly the mechanism
// `examples/gardend.rs`'s periodic re-check ticker relies on.
// ---------------------------------------------------------------------------

#[test]
fn observatory_boot_hot_cell_second_call_short_circuits_then_picks_up_a_newer_generation() {
    let bundle_dir = temp_dir("hot-cell");
    let profile_dir = temp_dir("hot-cell-profile");
    let _env = EnvGuard::acquire(Some(&bundle_dir), None, None);
    let (_graph_dir, store) = open_store_under_profile(&profile_dir);

    publish_generation(&bundle_dir, "gen-1", "cursor-hot-cell-1");
    let first = tokio_rt().block_on(run_boot_hook(GRAPH_ID, &store));
    let BootHookOutcome::Applied(first_report) = first else {
        panic!("expected the first call to Apply, got {first:?}");
    };
    assert!(first_report.raw.added > 0 && first_report.rollups.added > 0);

    // Same warm process, same store, nothing republished: a periodic
    // re-check tick must short-circuit without touching the store.
    let raw = raw_graph_iri();
    let rollups = rollups_graph_iri();
    let raw_after_first = graph_triples(&store, &raw);
    let rollups_after_first = graph_triples(&store, &rollups);

    let second = tokio_rt().block_on(run_boot_hook(GRAPH_ID, &store));
    match second {
        BootHookOutcome::AlreadyCurrent { token } => assert_eq!(token, "gen-1"),
        other => panic!("expected AlreadyCurrent for an unchanged generation, got {other:?}"),
    }
    assert_eq!(
        graph_triples(&store, &raw),
        raw_after_first,
        "AlreadyCurrent must not perturb the raw graph"
    );
    assert_eq!(
        graph_triples(&store, &rollups),
        rollups_after_first,
        "AlreadyCurrent must not perturb the rollups graph"
    );

    // Now a second projector run publishes a genuinely newer generation
    // (distinct token, distinct cursor) WHILE this process stays warm — no
    // restart, no re-open of the store. The next call must pick it up.
    publish_generation(&bundle_dir, "gen-2", "cursor-hot-cell-2");
    let third = tokio_rt().block_on(run_boot_hook(GRAPH_ID, &store));
    let BootHookOutcome::Applied(third_report) = third else {
        panic!("expected the third call to Apply the newer generation, got {third:?}");
    };
    // Same fixture content re-mapped under a fresh cursor value: the
    // idempotent diff still converges (0 net new rollup subjects beyond the
    // freshness triple's own cursor value changing), but the freshness
    // triple itself must have moved — proving this call actually re-read
    // and re-applied, not merely returned a stale cached outcome.
    let cursor_after_third = single_literal(
        &store,
        &rollups,
        &format!("{OBS_NS}ProjectionRun"),
        &format!("{OBS_NS}cursorHighWaterMark"),
    );
    assert_eq!(
        cursor_after_third, "cursor-hot-cell-2",
        "the hot-cell re-check must apply the NEWER generation's cursor, not the stale one"
    );
    let _ = third_report;

    cleanup(&profile_dir);
    cleanup(&bundle_dir);
}

// ---------------------------------------------------------------------------
// Core-primitive smoke: `await_bundle_ready`/`run_catch_up_from_bundle_dir`
// directly, independent of the env gate — the two building blocks
// `run_boot_hook` composes, each individually real and testable (module
// doc's own stated design), plus the generation-token protocol's two
// documented error shapes.
// ---------------------------------------------------------------------------

#[test]
fn observatory_boot_await_bundle_ready_returns_the_token_once_the_generation_exists() {
    let bundle_dir = ready_bundle_dir("primitive-smoke", "cursor-primitive-smoke");
    let token = tokio_rt().block_on(await_bundle_ready(&bundle_dir, Duration::from_secs(5)));
    assert_eq!(
        token.as_deref(),
        Some("gen-1"),
        "the published generation's token must be returned"
    );
    cleanup(&bundle_dir);
}

#[test]
fn observatory_boot_run_catch_up_from_bundle_dir_reads_the_real_file_contract_and_applies() {
    let bundle_dir = ready_bundle_dir("catch-up-primitive", "cursor-catch-up-primitive");
    let profile_dir = temp_dir("catch-up-primitive-profile");
    let (_graph_dir, store) = open_store_under_profile(&profile_dir);

    let report = run_catch_up_from_bundle_dir(&store, &bundle_dir)
        .expect("catch_up applies from a real bundle dir");
    assert!(report.raw.added > 0);
    assert!(report.rollups.added > 0);

    cleanup(&bundle_dir);
    cleanup(&profile_dir);
}

#[test]
fn observatory_boot_run_catch_up_from_bundle_dir_reports_not_ready_with_no_current_marker() {
    let bundle_dir = not_ready_bundle_dir("primitive-not-ready");
    let profile_dir = temp_dir("primitive-not-ready-profile");
    let (_graph_dir, store) = open_store_under_profile(&profile_dir);

    let result = run_catch_up_from_bundle_dir(&store, &bundle_dir);
    assert!(
        matches!(result, Err(CatchUpFromDirError::NotReady)),
        "expected NotReady with no CURRENT marker, got {result:?}"
    );

    cleanup(&bundle_dir);
    cleanup(&profile_dir);
}
