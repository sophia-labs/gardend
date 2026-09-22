//! Boot-hook: gated catch-up + durable cursor — §5 "A4 — Sidecar wiring +
//! boot-hook catch-up + cursor durability" of
//! `plans/observatory-analysis-cell-spec-20260715.md` (the `sophia` hub
//! repo). **GARDEN HALF ONLY.**
//!
//! ## Scope
//!
//! This module owns ONLY the garden-side half of A4: the boot-time
//! activation check, the bounded wait for the projector's file contract, and
//! the call into A3's [`super::apply::catch_up`]. It deliberately does not
//! own — and this checkout (`~/dev/garden-observatory-materializer`) has no
//! `platform-next` tree to put it in even if it wanted to:
//!
//! - the platform-next side of the process that populates the bundle
//!   directory and its least-privilege IRSA/EFS wiring — lives on
//!   `platform-next`;
//! - the hourly CronJob schedule;
//! - any dev-cluster/docker-desktop/MinIO end-to-end wiring — a supervised
//!   rollout step, not a unit of this branch.
//!
//! This module's job ends at: given an ALREADY-OPEN real `Store` for the
//! `observatory` graph and a real on-disk bundle directory some other
//! process populated, apply the bundle.
//!
//! ## RE-SCOPE (2026-07-16) — EFS bundle dir, not a co-located sidecar
//!
//! The ratified spec put the projector in a container co-located in the
//! observatory cell's own pod, sharing a pod-local `emptyDir` at the fixed
//! path `/work` purely as a same-pod file-handoff seam. Vera-approved
//! deviation: that seam is now a **fixed subdirectory of the cell's OWN
//! EFS-backed durable mount** ([`DURABLE_DIR_ENV_VAR`], the SAME
//! `GARDEN_DURABLE_DIR` `cell_durability` already hydrates/flushes this
//! store through) instead of a pod-local `emptyDir`. A standalone hourly
//! `pn-observatory` projector CronJob (a plain helm CronJob, like the
//! existing `observatory-collector`, NOT injected into the cell pod spec)
//! drops the bundle there directly; this cell reads it through the EFS mount
//! it already has for entirely other reasons. What this preserves from the
//! ratified spec: still a separate process, still no DuckDB in gardend,
//! gardend still does the direct-on-store materialization (A2/A3,
//! unchanged). What it eliminates: the gateway/cell-pod-spec change the
//! sidecar shape needed to inject a second container and a shared
//! `emptyDir` — there is no cell-pod-spec change here at all, and this
//! module (like the rest of A4) never touches `gateway/**` or the cell pod
//! spec.
//!
//! ## RE-SCOPE r1 (2026-07-16) — no new pod env; EFS-discoverable activation
//! + generation protocol
//!
//! The first RE-SCOPE pass still gated on a dedicated
//! `GARDEN_OBSERVATORY_PROJECTOR=1` pod env var. That variable is never set
//! by anything (`gateway/cell.rs` is off-limits, by the hard rule, so no pod
//! spec can ever set it) — the gate could never open in production. r1
//! drops that requirement entirely. Activation is now discovered
//! structurally, from the SAME EFS mount the bundle itself lives on: iff
//! [`CURRENT_POINTER_FILE`] exists (non-blank) under the resolved
//! [`bundle_dir`], the projector has been turned on for this environment and
//! has published at least one generation. [`projector_gate_open`] is still
//! the ONE cheap, `Store`-free predicate every entry point below consults
//! FIRST — it is now `own_graph_id == "observatory"` AND that EFS marker,
//! rather than `own_graph_id == "observatory"` AND a pod env var. For every
//! cell that is not the observatory cell (every production graph this SAME
//! `gardend` binary also boots, e.g. `angels`) the graph_id half alone
//! already closes the gate, cheaply, with zero I/O — unchanged from before.
//!
//! r1 also replaces the flat `bundle.ready`-marks-one-slot file contract
//! with an **immutable-generation + single-pointer** protocol (closing the
//! A4-review WRONG "mid-fold read can see a partial/mixed bundle"): each
//! projector run mints a fresh, unique generation token and writes
//! `obs-bundle.<token>.json` + `raw-snapshot.<token>.ndjson` as new,
//! never-overwritten files; only once BOTH exist does it atomically
//! (temp+rename) point [`CURRENT_POINTER_FILE`] at that token. A reader
//! reads `CURRENT` once (the token to use), reads both artifacts at that
//! token's fixed paths (which — because a token's files are never mutated
//! after publish — cannot themselves tear), then re-reads `CURRENT` and
//! confirms it still names the SAME token before applying anything
//! ([`run_catch_up_from_bundle_dir`]). A mismatch means a newer generation
//! published mid-read; the read is discarded, never applied — see
//! [`CatchUpFromDirError::GenerationRaced`] — and the next boot/poll picks
//! up the fresh generation cleanly. Old generations' files are pruned by the
//! projector script (not this module) with a multi-generation safety margin
//! over this read's worst-case bound.
//!
//! ## Hot-cell freshness (A4 review SUSPECT)
//!
//! The projector runs hourly; this boot-hook otherwise runs only at boot.
//! If the cell stays warm across an hourly publish (kept alive by other
//! traffic, past the point a fresh generation lands), a boot-only hook would
//! never see it again until the next cold start. [`run_boot_hook`] closes
//! this by being cheap to call repeatedly: it re-reads `CURRENT` on every
//! call, and if the token matches the last one this process actually
//! applied ([`BootHookOutcome::AlreadyCurrent`]), it returns immediately
//! without touching the `Store` at all. `examples/gardend.rs` wires this as
//! a periodic re-check task (the same shape as the existing periodic durable
//! flush ticker) so a hot observatory cell keeps picking up fresh hourly
//! generations without a restart, while a cold/idle one pays nothing extra
//! between boots.
//!
//! ## r2 — the ticker must exist BEFORE the projector's first publish; apply
//! is single-flight
//!
//! Two review r2 fixes on top of r1's hot-cell mechanism:
//!
//! 1. (SUSPECT) `examples/gardend.rs` used to gate the periodic re-check
//!    ticker's own SPAWN on [`projector_gate_open`] at boot time — which
//!    additionally requires a `CURRENT` marker to already exist. An
//!    observatory cell that boots BEFORE the projector's first-ever publish
//!    (or was warm before the projector was turned on for this environment)
//!    never got a ticker spawned at all, and would sit warm forever never
//!    noticing even the FIRST publication. The ticker's spawn now gates on
//!    [`is_observatory_cell`] alone (own_graph_id, zero I/O); each tick still
//!    calls [`run_boot_hook_for_cell`], which re-evaluates the FULL gate
//!    fresh every time and returns [`BootHookOutcome::GateClosed`] cleanly
//!    for every tick before the first publication.
//! 2. (WRONG) [`run_boot_hook`]'s `spawn_blocking` catch_up task, wrapped in
//!    `tokio::time::timeout`, used to be silently ABANDONED on timeout — the
//!    task kept running against the same `Store`, unobserved, and a LATER
//!    call could spawn a SECOND, concurrent one. If the orphaned OLDER task
//!    committed its diff-apply AFTER the newer one already committed and
//!    recorded itself as last-applied, the Store could silently regress to
//!    stale content with no future tick ever noticing. `catch_up` is now
//!    SINGLE-FLIGHT per bundle_dir (see [`in_flight_registry`]'s own doc): a
//!    call that finds a previous call's task still running JOINS it instead
//!    of ever spawning a second one, and [`drain_in_flight_apply`] gives a
//!    still-running task one more bounded chance to finish and be captured
//!    before the cell's final shutdown flush.
//!
//! ## Bounded end-to-end, not just the `CURRENT` wait
//!
//! Three independent bounds cover the whole hook, not only the wait for a
//! ready generation — this applies equally to the EFS-mounted bundle dir the
//! RE-SCOPE moved to (if anything, an EFS mount can be SLOWER than a
//! pod-local `emptyDir`, so these bounds matter more, not less): (1)
//! [`await_bundle_ready`] bounds the wait for a ready `CURRENT` generation;
//! (2) [`read_bounded_artifact`] refuses to even `open()` anything that is
//! not a plain regular file (a FIFO/socket/device/directory can block a read
//! indefinitely — this is the actual defense, not the timeout below, since a
//! blocking syscall cannot be interrupted from async Rust) and refuses
//! anything over [`max_artifact_bytes`] (an unbounded read on a huge file
//! can exhaust cell memory); (3) the read+parse+apply step as a whole runs
//! on a `spawn_blocking` task wrapped in `tokio::time::timeout(apply_budget(),
//! …)` — belt-and-suspenders for a pathological SLOW (but regular) file,
//! e.g. a stalled NFS/EFS mount, and the mechanism that also turns an
//! unexpected panic inside `catch_up` into `BootHookOutcome::Failed` instead
//! of silently vanishing. Every wait/budget/size env override is clamped to
//! a hard ceiling ([`clamped_duration_seconds`]/[`max_artifact_bytes`]) so a
//! pathological override value cannot overflow `Instant + Duration`
//! arithmetic or defeat the size cap.
//!
//! ## The durable cursor
//!
//! §A.3 is explicit that there is "no fragile numeric offset": the cursor is
//! the `obs:cursorHighWaterMark` literal A3's `apply_rollups` already
//! attaches to the same `obs:ProjectionRun` subject as every other rollup
//! triple, in the SAME gated apply (`apply.rs`). This module adds **no**
//! second cursor-storage mechanism on top of that triple — durability for it
//! comes entirely from gardend's EXISTING `cell_durability` flush/hydrate
//! plane (`GARDEN_DURABLE_DIR`, the EFS mount in cloud-2): the observatory
//! graph's `Store` is opened through the SAME
//! `rdf_store_service::open_graph_store` seam every other RDF request uses
//! (see [`run_boot_hook_for_cell`]), so it is enumerated and
//! RocksDB-checkpointed by `cell_durability::flush`/`flush_forced` exactly
//! like every other graph's store, and restored by
//! `cell_durability::hydrate`/`hydrate_detailed` on the next boot — no new
//! flush/hydrate code, no bespoke `ledger-cache/` file format invented here.
//! (The *projector's own* `ledger-cache/` — the S3-object-key cache that
//! lets it skip already-fetched ledger objects — is the OTHER durable
//! artifact §A.3 names; it is platform-next/projector state this crate
//! never reads or writes, consistent with §A.1's "gardend stays free of AWS
//! SDK" rationale.) `tests/observatory_boot.rs` proves the round trip using
//! the REAL `cell_durability::flush_forced`/`hydrate_detailed` primitives
//! against a real on-disk `Store`, not a shortcut.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use oxigraph::store::Store;

use crate::app_runtime::AppHandle;

use super::apply::{catch_up, CatchUpReport};
use super::graph_identity::GRAPH_ID;

/// Configurable bundle directory (RE-SCOPE, see module doc): the file-contract
/// directory a standalone projector populates and this hook awaits+reads.
/// When set (trimmed, non-empty) this is used verbatim — the production
/// override, wiring this to wherever platform-next's helm values put the
/// projector's EFS output, AND the test/dev override so
/// `tests/observatory_boot.rs` can point the hook at a real temp directory.
/// When unset, [`bundle_dir`] derives a default from [`DURABLE_DIR_ENV_VAR`]
/// — see that function's doc for the full resolution order.
pub const BUNDLE_DIR_ENV_VAR: &str = "GARDEN_OBSERVATORY_BUNDLE_DIR";

/// Bounded-wait budget for a ready generation, in whole seconds (default
/// [`DEFAULT_BUNDLE_WAIT`], clamped to at most [`MAX_BUNDLE_WAIT`]). Test/dev
/// override, same rationale as [`BUNDLE_DIR_ENV_VAR`].
pub const BUNDLE_WAIT_SECONDS_ENV_VAR: &str = "GARDEN_OBSERVATORY_BUNDLE_WAIT_SECONDS";

/// Bounded-budget, in whole seconds, for the WHOLE read+parse+apply step
/// once a generation is ready (default [`DEFAULT_APPLY_BUDGET`], clamped to
/// at most [`MAX_APPLY_BUDGET`]).
pub const APPLY_BUDGET_SECONDS_ENV_VAR: &str = "GARDEN_OBSERVATORY_APPLY_BUDGET_SECONDS";

/// Per-file byte-size cap for `obs-bundle.<token>.json`/
/// `raw-snapshot.<token>.ndjson` (default [`DEFAULT_MAX_ARTIFACT_BYTES`],
/// clamped to at most [`MAX_MAX_ARTIFACT_BYTES`]) — an unbounded
/// `read_to_string` on a huge file could exhaust cell memory.
pub const MAX_ARTIFACT_BYTES_ENV_VAR: &str = "GARDEN_OBSERVATORY_MAX_ARTIFACT_BYTES";

/// How often the hot-cell periodic re-check ([`run_boot_hook`] called again,
/// outside of boot) polls for a newer generation, in whole seconds (default
/// [`DEFAULT_REFRESH_INTERVAL`], clamped to at most
/// [`MAX_REFRESH_INTERVAL`]). Wired by `examples/gardend.rs`'s periodic
/// ticker — see the module doc's "Hot-cell freshness" section.
pub const REFRESH_INTERVAL_SECONDS_ENV_VAR: &str = "GARDEN_OBSERVATORY_REFRESH_INTERVAL_SECONDS";

/// The cell's OWN existing EFS-backed durable-plane mount — the SAME
/// `GARDEN_DURABLE_DIR` `cell_durability` already hydrates this store's
/// `Store` from at boot and flushes it to periodically (see "The durable
/// cursor" in the module doc). Read directly here via `std::env::var` (not
/// via `cell_durability`'s internal `DURABLE_DIRS` registry, which exposes no
/// public getter and is only guaranteed populated once
/// `examples/gardend.rs`'s observatory branch has already called
/// `set_durable_dirs`) so [`bundle_dir`]'s default stays a pure,
/// call-order-independent env read, exactly like every other var this module
/// consults.
const DURABLE_DIR_ENV_VAR: &str = "GARDEN_DURABLE_DIR";

/// Fixed subdirectory of [`DURABLE_DIR_ENV_VAR`] that [`bundle_dir`]'s
/// default resolves to when [`BUNDLE_DIR_ENV_VAR`] is unset — a sibling of
/// (never inside) `cell_durability`'s own `CURRENT`/`snap-NNNNNN` snapshot
/// namespace, so a standalone projector's writes here can never collide with
/// a flush/hydrate cycle (`cell_durability::prune_snapshots` only ever
/// removes entries that parse as a valid `snap-NNNNNN` name or a stale
/// `.building-*` dir; this name is neither, so it is left alone). This
/// EXACT literal must match platform-next helm's `observatory.projector.
/// bundleSubdir` default — `tests/observatory_boot.rs`'s
/// `observatory_boot_default_bundle_dir_derives_from_the_cells_existing_durable_dir_when_no_override_is_set`
/// and the cross-repo acceptance test both prove the two sides agree without
/// a manual override on either end. `pub` so
/// `tests/observatory_gardend_process.rs` (an external, real-subprocess
/// test) can point a real fixture drop at the SAME path production's
/// default derives, without a second hand-copied literal.
pub const DEFAULT_BUNDLE_DIR_SUBPATH: &str = "observatory-bundle";

/// Ultimate fallback when NEITHER [`BUNDLE_DIR_ENV_VAR`] NOR
/// [`DURABLE_DIR_ENV_VAR`] is set. A production observatory cell always has
/// `GARDEN_DURABLE_DIR` configured (§A.3 — durability for the whole
/// observatory graph depends on it), so this is a defensive degenerate case,
/// not a real deployment shape: it resolves to a path that simply never
/// exists, so the hook degrades to the SAME already-tested "gate closed —
/// no CURRENT marker" outcome — never a panic, never a hang.
const UNCONFIGURED_BUNDLE_DIR_FALLBACK: &str = "/var/run/garden-observatory-bundle-unconfigured";

const DEFAULT_BUNDLE_WAIT: Duration = Duration::from_secs(30);
/// Clamp ceiling for [`BUNDLE_WAIT_SECONDS_ENV_VAR`] — 5 minutes. Also closes
/// the hazard that an unclamped `u64` override could overflow
/// `Instant + Duration` arithmetic (`Duration::from_secs(u64::MAX)` added to
/// `Instant::now()` panics).
const MAX_BUNDLE_WAIT: Duration = Duration::from_secs(300);
const BUNDLE_POLL_INTERVAL: Duration = Duration::from_millis(200);

const DEFAULT_APPLY_BUDGET: Duration = Duration::from_secs(120);
/// Clamp ceiling for [`APPLY_BUDGET_SECONDS_ENV_VAR`] — 10 minutes, safely
/// under the cell's 900s default idle-reap window (a large backlog could
/// approach the 900s idle window).
const MAX_APPLY_BUDGET: Duration = Duration::from_secs(600);

const DEFAULT_MAX_ARTIFACT_BYTES: u64 = 32 * 1024 * 1024;
/// Clamp ceiling for [`MAX_ARTIFACT_BYTES_ENV_VAR`] — 512 MiB hard ceiling
/// regardless of override, so a misconfigured huge cap cannot reintroduce the
/// unbounded-memory hazard this cap exists to close.
const MAX_MAX_ARTIFACT_BYTES: u64 = 512 * 1024 * 1024;

const DEFAULT_REFRESH_INTERVAL: Duration = Duration::from_secs(300);
/// Clamp ceiling for [`REFRESH_INTERVAL_SECONDS_ENV_VAR`] — 30 minutes; a
/// hot cell should never go longer than this without re-checking for a
/// fresh hourly generation.
const MAX_REFRESH_INTERVAL: Duration = Duration::from_secs(1800);

/// The single mutable file in the bundle root (RE-SCOPE r1): its trimmed
/// content, when present and non-blank, both ACTIVATES the projector gate
/// (replaces the old `GARDEN_OBSERVATORY_PROJECTOR` pod env, which no pod
/// spec can ever set) and names the current generation TOKEN to read. Always
/// written last, via temp+rename, by the projector — see the module doc's
/// "generation protocol" section. `pub` so the cross-repo acceptance test
/// and `tests/observatory_boot.rs` can assemble a real fixture using the
/// SAME literal this module reads.
pub const CURRENT_POINTER_FILE: &str = "CURRENT";

/// Filename prefix for the bundle artifact — the real path for a given
/// token is [`bundle_json_path`]. `pub` so external tests and the
/// cross-repo acceptance harness can construct the exact same filename this
/// module reads without a second hand-copied format string.
pub const BUNDLE_JSON_PREFIX: &str = "obs-bundle";
/// Filename prefix for the raw-snapshot artifact — see [`BUNDLE_JSON_PREFIX`].
pub const RAW_SNAPSHOT_PREFIX: &str = "raw-snapshot";

/// `<bundle_dir>/obs-bundle.<token>.json` — the rollups-lane wire artifact
/// for one immutable generation.
pub fn bundle_json_path(bundle_dir: &Path, token: &str) -> PathBuf {
    bundle_dir.join(format!("{BUNDLE_JSON_PREFIX}.{token}.json"))
}

/// `<bundle_dir>/raw-snapshot.<token>.ndjson` — the raw-lane wire artifact
/// for one immutable generation.
pub fn raw_snapshot_path(bundle_dir: &Path, token: &str) -> PathBuf {
    bundle_dir.join(format!("{RAW_SNAPSHOT_PREFIX}.{token}.ndjson"))
}

/// Every branch a caller/test can assert on directly, never inferred from
/// log string content.
#[derive(Debug)]
pub enum BootHookOutcome {
    /// The gate was closed: either `own_graph_id` is not the observatory
    /// graph (cheap, zero I/O — the load-bearing no-op for every other cell,
    /// e.g. `angels`), or it IS but no [`CURRENT_POINTER_FILE`] marker
    /// exists yet on the EFS mount (the projector has never been activated
    /// for this environment). Either way: no bounded wait ran, no file was
    /// read, no `Store` was touched.
    GateClosed,
    /// The gate was open but no generation became ready within the bound —
    /// logged and skipped cleanly. Never a hang: the wait is always bounded
    /// by [`bundle_wait_budget`].
    BundleNotReady { waited: Duration },
    /// A generation was ready but it is the SAME token this process already
    /// applied — a cheap no-op (no `Store` touch) that lets the hot-cell
    /// periodic re-check ([`run_boot_hook`] called again after boot) poll
    /// often without cost. See the module doc's "Hot-cell freshness"
    /// section.
    AlreadyCurrent { token: String },
    /// A newer generation's [`CURRENT_POINTER_FILE`] published WHILE this
    /// call was reading the artifacts for an older one. The read is
    /// discarded — never applied, never a partial/mixed write — and the
    /// next boot/poll will pick up the fresh generation cleanly. Not an
    /// error: an expected, benign outcome of the hot-cell periodic re-check
    /// racing an hourly publish.
    GenerationChangedDuringRead { attempted_token: String },
    /// The gate was open, a generation was ready and NEWER than the last one
    /// applied, and `catch_up` ran and applied it (possibly a no-delta /
    /// already-converged apply if the content happened to be identical).
    Applied(CatchUpReport),
    /// review r2 WRONG "run_boot_hook is safe to call periodically under its
    /// timeout": an apply for `token` was ALREADY running (spawned by an
    /// earlier call to this function that gave up waiting on it) and has
    /// STILL not finished within this call's own budget either. This call
    /// did NOT spawn a second, concurrent `catch_up` against the `Store` —
    /// single-flight is the whole point (see the in-flight registry's own
    /// doc) — it re-registered the SAME still-running task for the next
    /// call to join. Never fatal, never a duplicate write: the Store is
    /// touched by at most one `catch_up` at a time, always.
    InProgress { token: String },
    /// The gate was open and a generation appeared ready, but reading the
    /// bundle directory or applying it failed. Loud (logged as an error by
    /// the caller), but never fatal to cell boot — a bad or partial bundle
    /// must not prevent the cell from serving its loopback API.
    Failed(String),
}

/// [`run_catch_up_from_bundle_dir`]'s error surface — typed so
/// [`run_boot_hook`] can distinguish "a newer generation raced this read"
/// (benign, retry next cycle) from "no generation is published at all"
/// (benign, same) from "a real failure" (loud). `Clone` (review r3): a
/// [`SharedApplyOutcome`] is broadcast to every concurrent waiter of an
/// in-flight apply, not consumed by exactly one, so its payload must be
/// duplicable — every field here is already an owned `String`, so this is
/// free.
#[derive(Debug, Clone)]
pub enum CatchUpFromDirError {
    /// No [`CURRENT_POINTER_FILE`] (or a blank/unsafe one) — nothing
    /// published yet at this path.
    NotReady,
    /// The generation token named by [`CURRENT_POINTER_FILE`] changed
    /// between the pre-read and post-read check (RE-SCOPE r1's
    /// generation-token protocol — see the module doc). The two artifact
    /// reads that just happened are discarded, not applied.
    GenerationRaced { attempted_token: String },
    /// A missing/oversized/non-regular artifact file, a JSON-parse failure,
    /// a SHACL violation, or a SPARQL failure from `catch_up` — a genuine
    /// failure, not a race.
    Other(String),
}

impl std::fmt::Display for CatchUpFromDirError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotReady => write!(f, "no ready generation published at this bundle dir"),
            Self::GenerationRaced { attempted_token } => write!(
                f,
                "generation {attempted_token:?} was superseded mid-read — discarding, not applying"
            ),
            Self::Other(message) => write!(f, "{message}"),
        }
    }
}

/// Process-wide "last (bundle_dir, generation) this process actually
/// applied" — the hot-cell short-circuit's only state. Keyed by
/// `bundle_dir` as well as the token (not the token alone): one `gardend`
/// process serves exactly one graph and, in production, one fixed bundle
/// dir for its whole lifetime, so a single global (mirroring
/// `rdf_store_service::GRAPH_STORES`'s own precedent for process-scoped cell
/// state) is the right shape, not a parameter threaded through every
/// caller — but keying on the path too means a coincidentally-identical
/// token string at a DIFFERENT bundle dir is never mistaken for
/// already-applied (the only way this could ever matter in production is a
/// mid-process env-var change, which does not happen — but it is exactly
/// what independent `cargo test` cases in the same process look like if
/// they happen to reuse the same token literal at different temp dirs, so
/// this is load-bearing for test isolation too).
fn last_applied_registry() -> &'static Mutex<Option<(PathBuf, String)>> {
    static REGISTRY: OnceLock<Mutex<Option<(PathBuf, String)>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(None))
}

fn last_applied_token(bundle_dir: &Path) -> Option<String> {
    let guard = last_applied_registry()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    match guard.as_ref() {
        Some((dir, token)) if dir == bundle_dir => Some(token.clone()),
        _ => None,
    }
}

fn set_last_applied_token(bundle_dir: &Path, token: String) {
    *last_applied_registry()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some((bundle_dir.to_path_buf(), token));
}

/// Test-support only: clear the process-wide "last applied generation"
/// registry. A real `gardend` process never calls this (a fresh process
/// starts with an empty registry by construction — that IS the reap+reboot
/// durability property). It exists so an external, same-OS-process test
/// suite that deliberately simulates crossing a process boundary (a
/// reap+reboot into a genuinely new `Store`/profile dir, but necessarily
/// within the SAME `cargo test` binary) can accurately model "this is a new
/// incarnation" for the one piece of state that is, unavoidably, in-process
/// rather than durable — without it, a reap+reboot test would spuriously
/// observe `AlreadyCurrent` (this process happens to remember the token from
/// its OWN earlier, logically-unrelated "incarnation") instead of exercising
/// the real content-level idempotency `catch_up`'s survey→diff→apply
/// provides. `pub`, not `#[cfg(test)]`-gated: external integration-test
/// crates under `tests/` link the plain library, not its test-cfg'd
/// variant — mirrors `authority_harness`'s own precedent for a
/// production-inert, always-compiled test-support seam.
pub fn reset_last_applied_token_for_tests() {
    *last_applied_registry()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
}

/// The token [`run_catch_up_from_bundle_dir_with_token`] itself read
/// (verified, when it got that far) plus the `catch_up` outcome — review r3
/// WRONG "the worker returns the token captured before spawn": the OLD shape
/// captured `ready_token` in the CALLER, before the blocking task ever ran,
/// and handed that back verbatim regardless of what the task itself actually
/// read once it started (a real gap on a busy `spawn_blocking` pool, where
/// queueing delay lets `CURRENT` move between the caller's read and the
/// task's own). `None` only for [`CatchUpFromDirError::NotReady`] (no
/// `CURRENT` was ever read at all); every other outcome carries the EXACT
/// token the task itself determined and (for a completed apply) verified via
/// [`verify_current_still_names`] — never a caller-supplied hint.
type ApplyOutcome = (Option<String>, Result<CatchUpReport, CatchUpFromDirError>);

/// What every waiter (however many are concurrently awaiting the SAME
/// in-flight task) ends up with: the real [`ApplyOutcome`] on a clean finish,
/// or a panic message when the blocking task itself panicked (mirrors
/// `JoinError`'s `Display`, converted to an owned `String` here because
/// `JoinError` is not `Clone` and this value is broadcast to N waiters, not
/// consumed once). `Result`/`Option`/`String`/[`CatchUpReport`]/
/// [`CatchUpFromDirError`] are all `Clone`, so this whole type is — no `Arc`
/// wrapping needed to broadcast it cheaply via [`tokio::sync::watch`].
type SharedApplyOutcome = Result<ApplyOutcome, String>;

/// One in-flight `catch_up` task's shared state — review r3 WRONG "a newly
/// spawned handle is not registered while its caller awaits it" / "joining
/// removes the handle from the registry before awaiting": the OLD registry
/// stored the raw `JoinHandle` itself and popped it the instant ANY caller
/// wanted to await it, so (a) a freshly spawned task was only ever recorded
/// on a LATER timeout, leaving a window right after spawn where the registry
/// looked completely empty, and (b) even once registered, the FIRST caller to
/// join it removed it immediately, so a second, concurrent caller arriving
/// during that same await window also saw nothing registered. Either gap let
/// two concurrent callers both conclude "nothing running" and both spawn —
/// exactly the double-apply [`in_flight_registry`]'s whole design exists to
/// rule out.
///
/// The fix: never hand the raw `JoinHandle` to a caller at all. A single
/// internal "driver" task (spawned alongside the blocking `catch_up` task,
/// under the SAME registry-lock critical section — see
/// [`ensure_in_flight_apply`]) owns the `JoinHandle` exclusively and is the
/// ONLY thing that ever awaits it; every external caller instead clones a
/// cheap [`tokio::sync::watch::Receiver`] and watches for the driver to
/// publish the outcome. Cloning a `watch::Receiver` is unlimited and
/// non-exclusive, so any number of concurrent callers can share the SAME
/// task without any of them removing it from the registry — the entry stays
/// registered for the task's ENTIRE lifetime, from the moment it is spawned
/// (still holding the registry lock) until the driver publishes the outcome
/// and clears its own entry.
struct InFlightApply {
    bundle_dir: PathBuf,
    /// Informational only (surfaced in [`BootHookOutcome::InProgress`] /
    /// log lines while the task is still running) — the token that was
    /// READY when this task was spawned, never used to attribute the
    /// eventual outcome (see [`ApplyOutcome`]'s own doc).
    started_for_token: String,
    /// A monotonic identity distinguishing this registration from any LATER
    /// one that might occupy the same slot after this one clears itself —
    /// defense in depth so the driver task's own cleanup never accidentally
    /// clears a DIFFERENT (newer) registration; not load-bearing for the
    /// single-flight property itself, which the registry lock alone already
    /// guarantees (see [`ensure_in_flight_apply`]).
    id: u64,
    outcome: tokio::sync::watch::Receiver<Option<SharedApplyOutcome>>,
}

static NEXT_IN_FLIGHT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// Process-wide "an apply task is currently running against `Store` for this
/// bundle_dir" — review r2 WRONG "run_boot_hook is safe to call periodically
/// under its timeout", tightened by review r3 (see [`InFlightApply`]'s own
/// doc for the exact registration-window gap r3 closes). The OLD shape
/// wrapped a `spawn_blocking` `catch_up` task in `tokio::time::timeout` and,
/// on timeout, simply stopped AWAITING it — Rust cannot force a
/// blocking-thread computation to stop, so the task kept running to
/// completion, unobserved, against the SAME `Store`. A LATER call (the next
/// periodic tick, itself calling `run_boot_hook` fresh) would then spawn a
/// SECOND, concurrent `catch_up` against the same `Store`. If the first
/// (orphaned) task's diff-apply committed AFTER the second, newer one already
/// committed and recorded itself as the last-applied token, the Store would
/// silently regress to the stale generation's content with NO future tick
/// ever noticing.
///
/// This registry makes `catch_up` SINGLE-FLIGHT per bundle_dir: a call that
/// finds a task already registered here for this path NEVER spawns a second
/// one — it clones the SAME [`InFlightApply::outcome`] receiver and awaits
/// that instead (bounded by its OWN fresh budget). The entry is registered
/// BEFORE [`ensure_in_flight_apply`] ever returns to a caller and is cleared
/// ONLY by the driver task itself, once the outcome has been published — see
/// that function's doc for why this closes the race completely rather than
/// merely narrowing it.
///
/// Single global slot (not a map), same precedent as
/// [`last_applied_registry`] and the SAME rationale: production has exactly
/// one bundle_dir per process for its whole lifetime; only independent
/// `cargo test` cases at different temp dirs share this process, and each
/// test in this module that touches it holds `env_serial()` for its whole
/// body.
fn in_flight_registry() -> &'static Mutex<Option<InFlightApply>> {
    static REGISTRY: OnceLock<Mutex<Option<InFlightApply>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(None))
}

/// Return `(started_for_token, receiver)` for the in-flight `catch_up` task
/// for `bundle_dir` — spawning a NEW task (and registering it, under the
/// SAME lock acquisition, before this function ever returns) only when
/// nothing is already running for this path. `std::sync::Mutex::lock` fully
/// serializes concurrent callers: whichever call acquires the lock first
/// either finds the registry empty (and, still holding the lock, spawns +
/// registers) or finds the entry the FIRST call just registered (and clones
/// its receiver) — there is no interleaving in which two concurrent callers
/// both observe an empty registry, because spawning and registering happen
/// atomically with respect to every other caller of this function. Neither
/// spawning the blocking `catch_up` task ([`spawn_apply`]) nor spawning the
/// driver task below ever awaits anything, so holding the
/// `std::sync::Mutex` across both is safe (no `.await` point inside the
/// critical section). `started_for_token` is purely informational (see
/// [`InFlightApply::started_for_token`]'s own doc) — it is read back out of
/// the SAME registered entry the receiver came from, so a caller never needs
/// a second, separately-racing lookup just to log/report it.
fn ensure_in_flight_apply(
    bundle_dir: &Path,
    ready_token: &str,
    store: &Arc<Store>,
) -> (
    String,
    tokio::sync::watch::Receiver<Option<SharedApplyOutcome>>,
) {
    let mut guard = in_flight_registry()
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    if let Some(entry) = guard.as_ref() {
        if entry.bundle_dir == bundle_dir {
            return (entry.started_for_token.clone(), entry.outcome.clone());
        }
    }
    let id = NEXT_IN_FLIGHT_ID.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let (tx, rx) = tokio::sync::watch::channel(None);
    let handle = spawn_apply(bundle_dir, store);
    tokio::spawn(async move {
        // This driver is the SOLE owner of `handle` — the only future in the
        // whole process that ever polls it — so awaiting it here is safe
        // regardless of how many external callers are concurrently watching
        // `rx`; none of them touch `handle` at all.
        let outcome: SharedApplyOutcome = match handle.await {
            Ok(applied) => Ok(applied),
            Err(join_error) => Err(format!("catch_up task panicked: {join_error}")),
        };
        // Clear the registry slot BEFORE publishing the outcome (clearing
        // only if it still names THIS registration, never a later one) —
        // deliberately the OPPOSITE order from "publish then clear". By the
        // time `handle.await` above resolves, `catch_up`'s Store mutation
        // (if any) has ALREADY been committed — the blocking closure only
        // returns after that work is done — so clearing here is safe: any
        // caller that races in through `ensure_in_flight_apply` immediately
        // after and finds the registry empty would spawn a SEQUENTIAL, not
        // concurrent, follow-up (the first apply is already fully finished),
        // which `catch_up`'s own idempotency makes harmless. The alternative
        // order (publish first) would let a waiter observe a "done" outcome
        // via `rx` while the registry still claimed a task in flight — an
        // observable inconsistency this ordering avoids entirely, including
        // for tests that assert on registry state immediately after
        // `wait_for_shared_outcome` returns.
        let mut guard = in_flight_registry()
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        if matches!(guard.as_ref(), Some(entry) if entry.id == id) {
            *guard = None;
        }
        drop(guard);
        let _ = tx.send(Some(outcome));
    });
    *guard = Some(InFlightApply {
        bundle_dir: bundle_dir.to_path_buf(),
        started_for_token: ready_token.to_string(),
        id,
        outcome: rx.clone(),
    });
    (ready_token.to_string(), rx)
}

/// Peek the CURRENTLY registered in-flight task for `bundle_dir` without
/// spawning one — `None` when nothing is running. Used by
/// [`drain_in_flight_apply`] (a shutdown-time observer, never a spawner: it
/// must never START new work) and by tests asserting on registry state.
fn peek_in_flight(bundle_dir: &Path) -> Option<InFlightApply> {
    let guard = in_flight_registry()
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    match guard.as_ref() {
        Some(entry) if entry.bundle_dir == bundle_dir => Some(InFlightApply {
            bundle_dir: entry.bundle_dir.clone(),
            started_for_token: entry.started_for_token.clone(),
            id: entry.id,
            outcome: entry.outcome.clone(),
        }),
        _ => None,
    }
}

/// Wait for a [`tokio::sync::watch`] receiver's value to become `Some` —
/// checks the CURRENT value first (a clone of an already-`Some` receiver
/// must not block on `changed()`, which only fires on a FUTURE transition and
/// would never resolve if the transition already happened before this
/// receiver was cloned) and only then awaits further changes.
async fn wait_for_shared_outcome(
    mut rx: tokio::sync::watch::Receiver<Option<SharedApplyOutcome>>,
) -> SharedApplyOutcome {
    loop {
        if let Some(outcome) = rx.borrow().clone() {
            return outcome;
        }
        if rx.changed().await.is_err() {
            // The sender was dropped without ever publishing — the driver
            // above always sends before it can be dropped, so this is only
            // reachable if the driver task itself was aborted before running
            // (never happens in this module: nothing ever calls `.abort()`
            // on it). Degrade to a typed failure rather than looping forever.
            return Err(
                "in-flight apply's outcome channel closed without publishing a result".to_string(),
            );
        }
    }
}

/// Test-only spy: how many times [`spawn_apply`] has actually started a NEW
/// blocking `catch_up` task in this process — the single-flight regression
/// signal a real concurrent-timing test can assert on directly ("still only
/// 1" across two overlapping calls) rather than inferring non-duplication
/// from timing alone.
#[cfg(test)]
static SPAWN_APPLY_CALL_COUNT: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// Test-only: an artificial delay `spawn_apply`'s closure sleeps for BEFORE
/// calling the real [`run_catch_up_from_bundle_dir`] — standing in for the
/// module doc's own documented slow case ("a pathological SLOW (but
/// regular) file, e.g. a stalled NFS/EFS mount"), so a test can
/// deterministically create the "still running when the next call arrives"
/// window without depending on real `catch_up` performance (which, against
/// the small fixtures these tests use, is far too fast to race reliably).
/// The `catch_up` call itself is never faked — only its start is delayed.
#[cfg(test)]
static TEST_INJECTED_APPLY_DELAY_MS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

#[cfg(test)]
fn spawn_apply_call_count() -> usize {
    SPAWN_APPLY_CALL_COUNT.load(std::sync::atomic::Ordering::SeqCst)
}

#[cfg(test)]
fn set_test_injected_apply_delay_ms(ms: u64) {
    TEST_INJECTED_APPLY_DELAY_MS.store(ms, std::sync::atomic::Ordering::SeqCst);
}

/// Start a NEW `catch_up` task against `store` — the ONLY place this module
/// spawns one, and ONLY ever called from inside
/// [`ensure_in_flight_apply`]'s own registry-lock critical section, which is
/// what makes single-flight airtight (see that function's doc): no caller of
/// THIS function needs to re-check anything itself. Takes no `ready_token`:
/// the outcome this task reports names the token
/// [`run_catch_up_from_bundle_dir_with_token`] itself reads once it actually
/// runs, which is authoritative (see [`ApplyOutcome`]'s doc) — a
/// caller-supplied hint would only invite the exact "recorded token doesn't
/// match what landed" bug review r3 closes.
fn spawn_apply(bundle_dir: &Path, store: &Arc<Store>) -> tokio::task::JoinHandle<ApplyOutcome> {
    #[cfg(test)]
    SPAWN_APPLY_CALL_COUNT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let task_store = Arc::clone(store);
    let task_bundle_dir = bundle_dir.to_path_buf();
    tokio::task::spawn_blocking(move || {
        #[cfg(test)]
        {
            let delay_ms = TEST_INJECTED_APPLY_DELAY_MS.load(std::sync::atomic::Ordering::SeqCst);
            if delay_ms > 0 {
                std::thread::sleep(Duration::from_millis(delay_ms));
            }
        }
        run_catch_up_from_bundle_dir_with_token(&task_store, &task_bundle_dir)
    })
}

/// Is the projector gate open for a cell whose own graph_id is
/// `own_graph_id`? The single predicate every other function in this module
/// consults FIRST. Cheap: `own_graph_id == GRAPH_ID` short-circuits (zero
/// I/O) for every non-observatory cell; only for the observatory graph
/// itself does this go on to do one small file read (never the bounded
/// multi-second wait — see [`projector_activated`]).
pub fn projector_gate_open(own_graph_id: &str) -> bool {
    is_observatory_cell(own_graph_id) && projector_activated(&bundle_dir())
}

/// The cheap, zero-I/O half of [`projector_gate_open`] — `true` iff
/// `own_graph_id` names the observatory graph, independent of whether the
/// projector has ever published anything yet. `pub` (review r2 SUSPECT,
/// "the hot-cell periodic re-check covers already-running cells"): a caller
/// that wants to arm something for the LIFE of an observatory cell —
/// e.g. `examples/gardend.rs`'s periodic re-check ticker — must gate on
/// this alone, NOT on [`projector_gate_open`] (which additionally requires
/// a `CURRENT` marker to already exist). Gating the ticker's own SPAWN on
/// the full gate meant an observatory cell that booted before the
/// projector's first-ever publication (or was warm before the projector was
/// turned on for this environment) never got a ticker at all — it would sit
/// warm forever, never noticing even the projector's first publication,
/// until its next cold start. Each tick still re-evaluates the FULL gate
/// itself (via [`run_boot_hook`]/[`run_boot_hook_for_cell`], which call
/// [`projector_gate_open`] fresh every time), so a tick before the first
/// publication correctly costs one cheap file-absent check and returns
/// [`BootHookOutcome::GateClosed`] — never a panic, never the bounded wait.
pub fn is_observatory_cell(own_graph_id: &str) -> bool {
    own_graph_id == GRAPH_ID
}

/// `true` iff [`CURRENT_POINTER_FILE`] exists under `bundle_dir` and names a
/// non-blank, path-safe token — RE-SCOPE r1's EFS-discoverable replacement
/// for the old `GARDEN_OBSERVATORY_PROJECTOR=1` pod env gate (no pod spec
/// can ever set that var; this reads the SAME EFS mount the bundle itself
/// lives on instead). A single `fs::read_to_string` on a small pointer file
/// — bounded in practice (not wrapped in the artifact size/regular-file
/// checks [`read_bounded_artifact`] applies to the much larger JSON/NDJSON
/// artifacts), consistent with [`await_bundle_ready`]'s own poll loop
/// already doing unwrapped sync `Path` I/O in this module.
fn projector_activated(bundle_dir: &Path) -> bool {
    read_current_token(bundle_dir).is_some()
}

/// Read and validate [`CURRENT_POINTER_FILE`]'s content. `None` when the
/// file is absent, blank/whitespace-only, or names something that is not a
/// safe bare filename-token (defense in depth: a corrupt or hostile pointer
/// value must never be joined onto a path and opened).
fn read_current_token(bundle_dir: &Path) -> Option<String> {
    let raw = std::fs::read_to_string(bundle_dir.join(CURRENT_POINTER_FILE)).ok()?;
    let token = raw.trim();
    if token.is_empty() || !is_safe_token(token) {
        return None;
    }
    Some(token.to_string())
}

/// A safe bare filename-token: ASCII alphanumeric plus `-`/`_`/`.`, and never
/// containing `..` — never a path separator, never a traversal sequence.
fn is_safe_token(token: &str) -> bool {
    !token.is_empty()
        && token
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        && !token.contains("..")
}

/// Resolve the bundle directory to await/read. Resolution order:
///
/// 1. [`BUNDLE_DIR_ENV_VAR`] (`GARDEN_OBSERVATORY_BUNDLE_DIR`), trimmed, if
///    non-empty — the operator/test override. Production wires this (via
///    helm values, OFF by default) to wherever the standalone projector
///    actually drops the bundle on the shared EFS mount, if it needs
///    something other than the default below.
/// 2. `<GARDEN_DURABLE_DIR>/`[`DEFAULT_BUNDLE_DIR_SUBPATH`] — a fixed
///    subdirectory of the cell's OWN existing EFS mount (RE-SCOPE, see
///    module doc): no new mount, no cell-pod-spec change, because the
///    observatory cell already runs with `GARDEN_DURABLE_DIR` set for its
///    own durability.
/// 3. [`UNCONFIGURED_BUNDLE_DIR_FALLBACK`] — only when NEITHER of the above
///    is set; see that constant's doc for why this is a safe, inert
///    degenerate case rather than a real deployment shape.
fn bundle_dir() -> PathBuf {
    if let Some(overridden) = non_empty_env(BUNDLE_DIR_ENV_VAR) {
        return PathBuf::from(overridden);
    }
    if let Some(durable_dir) = non_empty_env(DURABLE_DIR_ENV_VAR) {
        return PathBuf::from(durable_dir).join(DEFAULT_BUNDLE_DIR_SUBPATH);
    }
    PathBuf::from(UNCONFIGURED_BUNDLE_DIR_FALLBACK)
}

/// `var`, trimmed, with an empty result normalized to `None` — the shared
/// "unset or blank counts as unset" rule every override in this module
/// follows.
fn non_empty_env(var: &str) -> Option<String> {
    std::env::var(var)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// Parse a whole-seconds env override, falling back to `default` on
/// missing/unparsable/zero, then CLAMP to `max` — closes the unclamped
/// `u64` → `Instant + Duration` overflow-panic hazard and caps how long any
/// single boot-hook stage can block boot regardless of misconfiguration.
fn clamped_duration_seconds(var: &str, default: Duration, max: Duration) -> Duration {
    std::env::var(var)
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|secs| *secs > 0)
        .map(Duration::from_secs)
        .unwrap_or(default)
        .min(max)
}

fn bundle_wait_budget() -> Duration {
    clamped_duration_seconds(
        BUNDLE_WAIT_SECONDS_ENV_VAR,
        DEFAULT_BUNDLE_WAIT,
        MAX_BUNDLE_WAIT,
    )
}

/// The end-to-end budget for the read+parse+apply step.
fn apply_budget() -> Duration {
    clamped_duration_seconds(
        APPLY_BUDGET_SECONDS_ENV_VAR,
        DEFAULT_APPLY_BUDGET,
        MAX_APPLY_BUDGET,
    )
}

/// The hot-cell periodic re-check interval — see the module doc's "Hot-cell
/// freshness" section and `examples/gardend.rs`'s ticker.
pub fn refresh_interval() -> Duration {
    clamped_duration_seconds(
        REFRESH_INTERVAL_SECONDS_ENV_VAR,
        DEFAULT_REFRESH_INTERVAL,
        MAX_REFRESH_INTERVAL,
    )
}

/// The per-file byte-size cap, clamped to at most [`MAX_MAX_ARTIFACT_BYTES`]
/// regardless of override.
fn max_artifact_bytes() -> u64 {
    std::env::var(MAX_ARTIFACT_BYTES_ENV_VAR)
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|bytes| *bytes > 0)
        .unwrap_or(DEFAULT_MAX_ARTIFACT_BYTES)
        .min(MAX_MAX_ARTIFACT_BYTES)
}

/// Read `path` as UTF-8 text, refusing anything that is not a plain regular
/// file — a FIFO/socket/device/directory can block `read_to_string`
/// indefinitely — and refusing anything larger than [`max_artifact_bytes`].
/// Both checks run via `fs::metadata` BEFORE the read itself: a rejected
/// file is never `open()`-ed for reading, so this check alone (not the
/// timeout in [`run_boot_hook`]) is the real defense against a FIFO hang —
/// a blocking syscall already in progress cannot be interrupted from async
/// Rust, only abandoned.
fn read_bounded_artifact(path: &Path) -> Result<String, String> {
    let metadata =
        std::fs::metadata(path).map_err(|error| format!("stat {}: {error}", path.display()))?;
    if !metadata.is_file() {
        return Err(format!(
            "{} is not a regular file (refusing to read a FIFO/socket/device/directory — it \
             could block indefinitely)",
            path.display()
        ));
    }
    let max_bytes = max_artifact_bytes();
    if metadata.len() > max_bytes {
        return Err(format!(
            "{} is {} bytes, exceeding the {max_bytes}-byte observatory artifact cap",
            path.display(),
            metadata.len()
        ));
    }
    std::fs::read_to_string(path).map_err(|error| format!("read {}: {error}", path.display()))
}

/// Bounded poll for a ready generation under `bundle_dir` — NEVER hangs past
/// `budget`. Returns the ready token once BOTH artifact files for the
/// [`CURRENT_POINTER_FILE`]-named token exist as regular files (the
/// projector's own publish ordering already guarantees this by the time
/// `CURRENT` names a token — see the module doc — but re-checking here
/// closes any residual cross-client EFS visibility-ordering window rather
/// than trusting write-ordering alone). `async` + `tokio::time::sleep` so it
/// yields the runtime rather than blocking a worker thread (gardend's
/// loopback server must stay responsive while this runs).
pub async fn await_bundle_ready(bundle_dir: &Path, budget: Duration) -> Option<String> {
    let deadline = tokio::time::Instant::now() + budget;
    loop {
        if let Some(token) = read_current_token(bundle_dir) {
            if bundle_json_path(bundle_dir, &token).is_file()
                && raw_snapshot_path(bundle_dir, &token).is_file()
            {
                return Some(token);
            }
        }
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return None;
        }
        tokio::time::sleep(BUNDLE_POLL_INTERVAL.min(deadline - now)).await;
    }
}

/// Read one generation's two file-contract artifacts from `bundle_dir` —
/// bounded (regular-file + size-capped, via [`read_bounded_artifact`]) —
/// verify [`CURRENT_POINTER_FILE`] still names the SAME token afterward
/// (RE-SCOPE r1's generation-token protocol: closes the "mid-fold read sees
/// a partial/mixed bundle" hazard — see the module doc), and run A3's real
/// [`catch_up`] against `store`. The core, `Store`+`Path`-only (no
/// `AppHandle`) surface — directly testable with a real on-disk store and
/// real fixture bytes, no tauri mocking required. Synchronous/blocking by
/// design: [`run_boot_hook`] is the caller that wraps this in a
/// `spawn_blocking` + timeout for the async, bounded, panic-safe production
/// path.
///
/// Thin wrapper over [`run_catch_up_from_bundle_dir_with_token`], discarding
/// the token it read — kept so every existing direct caller (this module's
/// own unit tests, `tests/observatory_boot.rs`) stays source-compatible.
/// [`spawn_apply`] calls the `_with_token` core directly instead, because it
/// is exactly the token this call itself reads+verifies (review r3) that
/// must end up in [`ApplyOutcome`], never a value some OTHER caller read
/// earlier.
pub fn run_catch_up_from_bundle_dir(
    store: &Store,
    bundle_dir: &Path,
) -> Result<CatchUpReport, CatchUpFromDirError> {
    run_catch_up_from_bundle_dir_with_token(store, bundle_dir).1
}

/// Same read+verify+apply sequence as [`run_catch_up_from_bundle_dir`], but
/// also returns the token THIS call itself read via [`read_current_token`] —
/// `None` only for the [`CatchUpFromDirError::NotReady`] case, where no
/// `CURRENT` was ever read at all; every other outcome (success,
/// `GenerationRaced`, or any other failure) carries `Some` of the exact token
/// this call determined, because by the time any of those outcomes is
/// reachable the token has already been read.
fn run_catch_up_from_bundle_dir_with_token(
    store: &Store,
    bundle_dir: &Path,
) -> (Option<String>, Result<CatchUpReport, CatchUpFromDirError>) {
    let Some(token) = read_current_token(bundle_dir) else {
        return (None, Err(CatchUpFromDirError::NotReady));
    };

    let bundle = match read_bounded_artifact(&bundle_json_path(bundle_dir, &token)) {
        Ok(bundle) => bundle,
        Err(error) => return (Some(token), Err(CatchUpFromDirError::Other(error))),
    };
    // Test-only seam (MISSING "a real reader-versus-second-publication
    // rollover test... does not race run_catch_up_from_bundle_dir"): a no-op
    // in production, this is the ONE point a test can inject a REAL second
    // publish landing BETWEEN this function's own two artifact reads — the
    // exact window `verify_current_still_names` below exists to catch, but
    // which the module's existing unit test only exercised by calling that
    // verifier directly, never through this function's own real read
    // sequence. See `run_catch_up_from_bundle_dir_discards_a_real_second_publish_landing_between_its_own_reads`.
    #[cfg(test)]
    run_mid_read_test_hook();
    let raw_snapshot = match read_bounded_artifact(&raw_snapshot_path(bundle_dir, &token)) {
        Ok(raw_snapshot) => raw_snapshot,
        Err(error) => return (Some(token), Err(CatchUpFromDirError::Other(error))),
    };

    if let Err(error) = verify_current_still_names(bundle_dir, &token) {
        return (Some(token), Err(error));
    }

    let outcome = catch_up(store, &bundle, &raw_snapshot).map_err(CatchUpFromDirError::Other);
    (Some(token), outcome)
}

/// Test-only hook invoked by [`run_catch_up_from_bundle_dir`] between its two
/// artifact reads — see that function's own doc. `None` (the default) is a
/// true no-op; a test sets it via [`set_mid_read_test_hook`] to deterministically
/// land a second publish in the exact window a real race would occupy,
/// without depending on real thread timing (this module's own established
/// convention — see `verify_current_still_names_reports_generation_raced_when_current_changed`'s
/// doc for the same rationale).
#[cfg(test)]
static MID_READ_TEST_HOOK: Mutex<Option<Box<dyn Fn() + Send + Sync>>> = Mutex::new(None);

#[cfg(test)]
fn set_mid_read_test_hook(hook: Option<Box<dyn Fn() + Send + Sync>>) {
    *MID_READ_TEST_HOOK.lock().unwrap_or_else(|p| p.into_inner()) = hook;
}

#[cfg(test)]
fn run_mid_read_test_hook() {
    if let Some(hook) = MID_READ_TEST_HOOK
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .as_ref()
    {
        hook();
    }
}

/// The generation-token protocol's post-read half (RE-SCOPE r1 — see the
/// module doc): `Ok(())` iff [`CURRENT_POINTER_FILE`] still names
/// `expected_token`. Split out from [`run_catch_up_from_bundle_dir`] so the
/// exact interleaving the A4 review named — a publish landing BETWEEN the
/// pre-read and post-read check — can be tested deterministically (by
/// calling this directly after manually rewriting `CURRENT`) rather than via
/// real thread timing, which would be flaky.
fn verify_current_still_names(
    bundle_dir: &Path,
    expected_token: &str,
) -> Result<(), CatchUpFromDirError> {
    match read_current_token(bundle_dir) {
        Some(after) if after == expected_token => Ok(()),
        _ => Err(CatchUpFromDirError::GenerationRaced {
            attempted_token: expected_token.to_string(),
        }),
    }
}

/// The boot-hook core (§5 A4): gate → bounded wait for a ready generation →
/// hot-cell short-circuit if unchanged → bounded read+parse+`catch_up` →
/// outcome. `store` is ALREADY the caller's real, opened `Store` for the
/// `observatory` graph — this function owns none of that resolution (see
/// [`run_boot_hook_for_cell`] for the production wiring that resolves it),
/// so it stays testable without an `AppHandle`.
///
/// **The gate check happens FIRST and is the only thing that runs when the
/// gate is closed.** Callable repeatedly (not just at boot) — see the module
/// doc's "Hot-cell freshness" section; `examples/gardend.rs` wires a
/// periodic re-check on this same function.
///
/// The read+parse+apply step runs on a `spawn_blocking` task — moved off the
/// async worker thread gardend's loopback server also runs on, and wrapped
/// in `tokio::time::timeout(apply_budget(), …)` so it can NEVER hang THIS
/// CALL past [`apply_budget`]'s bound, and so a panic inside `catch_up` (or
/// anything it calls) surfaces as a `JoinError` this function inspects and
/// converts to `BootHookOutcome::Failed`, rather than vanishing silently the
/// way a detached, un-joined `crate::app_runtime::async_runtime::spawn` task's panic
/// would.
///
/// Single-flight (review r2 WRONG "run_boot_hook is safe to call
/// periodically under its timeout"; review r3 closed a REMAINING window in
/// this same mechanism — see [`InFlightApply`]'s own doc): on timeout the
/// spawned task is NEVER silently abandoned. It keeps running to completion
/// in the background against the SAME `store` handle regardless (Rust cannot
/// force a blocking-thread computation to stop), and it stays registered in
/// [`in_flight_registry`] — for its ENTIRE lifetime, not merely from a
/// caller's timeout onward — so that ANY call to this function for the SAME
/// bundle_dir, whether it arrives before or after this one gives up waiting,
/// shares that SAME task via [`ensure_in_flight_apply`] instead of ever
/// spawning a second, concurrent one. The Store is therefore mutated by at
/// most one `catch_up` call at a time, always — a timeout only ever changes
/// who is WAITING on it, never how many are RUNNING.
pub async fn run_boot_hook(own_graph_id: &str, store: &Arc<Store>) -> BootHookOutcome {
    if !projector_gate_open(own_graph_id) {
        return BootHookOutcome::GateClosed;
    }

    let bundle_dir = bundle_dir();
    let ready_budget = bundle_wait_budget();
    let Some(ready_token) = await_bundle_ready(&bundle_dir, ready_budget).await else {
        log::warn!(
            "observatory boot-hook: no ready generation under {} within {}s — skipping this \
             cycle's catch_up cleanly (never hanging cell boot)",
            bundle_dir.display(),
            ready_budget.as_secs()
        );
        return BootHookOutcome::BundleNotReady {
            waited: ready_budget,
        };
    };

    if last_applied_token(&bundle_dir).as_deref() == Some(ready_token.as_str()) {
        return BootHookOutcome::AlreadyCurrent { token: ready_token };
    }

    let budget = apply_budget();
    // Joins an ALREADY-registered task for this bundle_dir, or spawns +
    // registers a new one — atomically, under one lock acquisition (see
    // `ensure_in_flight_apply`'s own doc) — so no concurrent caller can ever
    // observe an empty registry mid-spawn, and no caller ever removes the
    // entry merely by choosing to await it (any number of callers can await
    // the SAME `rx` concurrently).
    let (in_flight_hint, rx) = ensure_in_flight_apply(&bundle_dir, &ready_token, store);

    match tokio::time::timeout(budget, wait_for_shared_outcome(rx)).await {
        Ok(Ok((token, Ok(report)))) => {
            // `token` is `Some` in every real success case — the worker
            // itself read+verified it (review r3: never the caller-supplied
            // `ready_token` — see `ApplyOutcome`'s doc). The `None` fallback
            // below is unreachable in practice (`run_catch_up_from_bundle_dir_with_token`
            // only ever returns `None` alongside `Err(NotReady)`) but is
            // handled explicitly rather than panicking, in keeping with this
            // whole module's "never hang/panic cell boot" discipline.
            let applied_token = token.unwrap_or_else(|| {
                log::warn!(
                    "observatory boot-hook: catch_up succeeded but reported no token (unexpected) \
                     — recording the originally-ready token {ready_token:?} instead"
                );
                ready_token.clone()
            });
            log::info!(
                "observatory boot-hook: catch_up applied generation {applied_token} — raw +{}/-{}, rollups +{}/-{}",
                report.raw.added,
                report.raw.removed,
                report.rollups.added,
                report.rollups.removed,
            );
            set_last_applied_token(&bundle_dir, applied_token);
            BootHookOutcome::Applied(report)
        }
        Ok(Ok((_, Err(CatchUpFromDirError::GenerationRaced { attempted_token })))) => {
            log::info!(
                "observatory boot-hook: generation {attempted_token} was superseded mid-read — \
                 discarding this cycle's read (not applied); the next cycle picks up the fresh \
                 generation cleanly"
            );
            BootHookOutcome::GenerationChangedDuringRead { attempted_token }
        }
        Ok(Ok((_, Err(CatchUpFromDirError::NotReady)))) => {
            // The bounded wait above just confirmed readiness; a prune
            // racing the marker away in the narrow window between that wait
            // and this read is exotic but not impossible on EFS. Same
            // "skip cleanly, retry next cycle" treatment as a normal
            // not-ready outcome.
            log::warn!(
                "observatory boot-hook: generation vanished between the readiness wait and the \
                 read (a prune raced this cycle) — skipping cleanly"
            );
            BootHookOutcome::BundleNotReady {
                waited: Duration::ZERO,
            }
        }
        Ok(Ok((_, Err(CatchUpFromDirError::Other(error))))) => {
            log::error!("observatory boot-hook: catch_up failed (cell boot continues): {error}");
            BootHookOutcome::Failed(error)
        }
        Ok(Err(panic_message)) => {
            // The task itself finished (panicked) and the driver already
            // cleared its own registry entry — nothing to re-register.
            log::error!("observatory boot-hook: {panic_message} (cell boot continues)");
            BootHookOutcome::Failed(panic_message)
        }
        Err(_elapsed) => {
            // This call gives up WAITING, but the task itself is neither
            // abandoned NOR removed from the registry — it was never removed
            // in the first place (review r3 — see `InFlightApply`'s doc).
            // The NEXT call (this cycle's periodic re-check, or the next
            // one) finds it still registered via `ensure_in_flight_apply`
            // and shares the SAME `rx`, never spawning a second, concurrent
            // task against `store`.
            log::warn!(
                "observatory boot-hook: apply of generation {in_flight_hint} still running past \
                 its {budget:?} budget under {} — not spawning a second one; a later call (this \
                 cycle's periodic re-check, or the next one) will join the SAME still-running task",
                bundle_dir.display()
            );
            BootHookOutcome::InProgress {
                token: in_flight_hint,
            }
        }
    }
}

/// Shutdown-time drain (review r2 WRONG "run_boot_hook is safe to call
/// periodically under its timeout" — "track and join the worker before
/// retry OR SHUTDOWN"): if a `catch_up` task is still in flight for
/// [`bundle_dir`]'s resolved path when the cell is shutting down, wait for
/// it (bounded by [`apply_budget`], the SAME ceiling every other apply
/// respects) rather than letting the process exit and orphan it entirely.
/// Applying its result (if it finishes in time) means the imminent final
/// durable flush captures that work instead of losing it. A cheap no-op —
/// one `std::sync::Mutex` lock — when nothing is in flight (the overwhelming
/// common case: single-flight means a task is normally only ever "in
/// flight" across the narrow window between two ticks, not for the whole
/// process lifetime), so `examples/gardend.rs` can call this unconditionally
/// right before its final flush, for every cell, without a graph_id gate.
pub async fn drain_in_flight_apply() {
    let dir = bundle_dir();
    let Some(entry) = peek_in_flight(&dir) else {
        return;
    };
    // A pure OBSERVER (review r3): never spawns anything, only watches the
    // ALREADY-registered task's shared outcome. The entry stays registered
    // regardless of whether this drain (or `run_boot_hook`, or both
    // concurrently) is watching it — its own driver task clears it once
    // done, independent of any of them.
    match tokio::time::timeout(apply_budget(), wait_for_shared_outcome(entry.outcome)).await {
        Ok(Ok((token, Ok(report)))) => {
            let applied_token = token.unwrap_or(entry.started_for_token);
            log::info!(
                "observatory shutdown drain: a still-in-flight catch_up for generation \
                 {applied_token} finished just in time — raw +{}/-{}, rollups +{}/-{} \
                 (captured by the imminent final flush)",
                report.raw.added,
                report.raw.removed,
                report.rollups.added,
                report.rollups.removed,
            );
            set_last_applied_token(&dir, applied_token);
        }
        Ok(Ok((_, Err(error)))) => {
            log::warn!(
                "observatory shutdown drain: the in-flight apply for generation \
                 {} finished with a non-fatal outcome, not applied: {error}",
                entry.started_for_token
            );
        }
        Ok(Err(panic_message)) => {
            log::error!(
                "observatory shutdown drain: in-flight catch_up task panicked: {panic_message}"
            );
        }
        Err(_elapsed) => {
            // Still not done even at shutdown's own budget. Nothing to
            // re-register — it was never removed (review r3): the entry
            // remains exactly as it was, still owned by its own driver task.
            // In production the process exits shortly after this regardless,
            // so the task is finally, genuinely orphaned here — an accepted,
            // documented residual (a strict improvement over EVERY timeout
            // orphaning, which was the r1 hazard this whole mechanism
            // closes).
            log::warn!(
                "observatory shutdown drain: apply for generation {} still running past its own \
                 {:?} budget at shutdown — proceeding to final flush without it",
                entry.started_for_token,
                apply_budget()
            );
        }
    }
}

/// Production entry point (wired from `examples/gardend.rs`): resolve the
/// REAL, self-healing store path for the `observatory` graph via the SAME
/// `existing_graph_dir`/`open_graph_store` seam every RDF request uses, then
/// run [`run_boot_hook`]. Split out from `run_boot_hook` so the pure/async
/// core above stays testable without a mocked `AppHandle` (this crate's
/// existing observatory tests never mock tauri either — see
/// `observatory::authority_harness::open_real_store`). Safe (and cheap when
/// already-current) to call repeatedly, not just once at boot —
/// `open_graph_store` is a process-wide cache keyed by path, so a later call
/// with the same graph_id resolves the SAME `Arc<Store>` the loopback server
/// itself uses, never a second competing handle. `examples/gardend.rs` uses
/// exactly this repeatability for the hot-cell periodic re-check.
///
/// The gate is checked FIRST, before `existing_graph_dir` (which can create
/// a `graph.json` via self-heal) or `open_graph_store` (which opens/creates
/// an on-disk RocksDB store) ever run — so a non-observatory cell never
/// touches either.
pub async fn run_boot_hook_for_cell(app: &AppHandle, own_graph_id: &str) -> BootHookOutcome {
    if !projector_gate_open(own_graph_id) {
        return BootHookOutcome::GateClosed;
    }

    let graph_dir = match crate::graph_paths::existing_graph_dir(app, GRAPH_ID) {
        Ok(dir) => dir,
        Err(error) => {
            log::error!("observatory boot-hook: cannot resolve graph dir for {GRAPH_ID}: {error}");
            return BootHookOutcome::Failed(error);
        }
    };
    let store = match crate::rdf_store_service::open_graph_store(&graph_dir) {
        Ok(store) => store,
        Err(error) => {
            log::error!("observatory boot-hook: cannot open store for {GRAPH_ID}: {error}");
            return BootHookOutcome::Failed(error);
        }
    };

    run_boot_hook(own_graph_id, &store).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tauri_runtime::build_mock_app_for_tests;
    use std::time::Instant;

    /// Env vars this module's tests touch are process-global; every test
    /// that sets any of them holds this lock for its whole body — mirrors
    /// `tauri_runtime::profile_env_serial()`'s own precedent for the SAME
    /// class of hazard (Rust runs `#[test]`/`#[tokio::test]` fns in this
    /// binary on a thread pool by default).
    fn env_serial() -> &'static std::sync::Mutex<()> {
        static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| std::sync::Mutex::new(()))
    }

    fn set_env(var: &str, value: Option<&str>) {
        // SAFETY: serialized by `env_serial()` — no other thread in this
        // binary reads/writes these vars concurrently (every caller in this
        // module goes through it).
        unsafe {
            match value {
                Some(value) => std::env::set_var(var, value),
                None => std::env::remove_var(var),
            }
        }
    }

    /// [`bundle_dir`]'s resolution order — the `GARDEN_OBSERVATORY_BUNDLE_DIR`
    /// override wins outright when non-blank; otherwise a fixed subdirectory
    /// of `GARDEN_DURABLE_DIR` (the cell's OWN existing EFS mount) is used;
    /// only when NEITHER is set does it fall back to the inert placeholder.
    /// Pure env-only, no filesystem I/O.
    #[test]
    fn bundle_dir_resolves_override_then_durable_dir_then_falls_back_when_neither_is_set() {
        let _lock = env_serial().lock().unwrap_or_else(|p| p.into_inner());

        set_env(BUNDLE_DIR_ENV_VAR, None);
        set_env(DURABLE_DIR_ENV_VAR, None);
        assert_eq!(
            bundle_dir(),
            PathBuf::from(UNCONFIGURED_BUNDLE_DIR_FALLBACK),
            "with neither var set, bundle_dir must resolve to the inert fallback"
        );

        set_env(DURABLE_DIR_ENV_VAR, Some("/mnt/efs/observatory"));
        assert_eq!(
            bundle_dir(),
            PathBuf::from("/mnt/efs/observatory").join(DEFAULT_BUNDLE_DIR_SUBPATH),
            "with only GARDEN_DURABLE_DIR set, bundle_dir must derive the fixed subdir default"
        );

        set_env(BUNDLE_DIR_ENV_VAR, Some("/mnt/efs/custom-bundle-drop"));
        assert_eq!(
            bundle_dir(),
            PathBuf::from("/mnt/efs/custom-bundle-drop"),
            "a non-blank override must win over the GARDEN_DURABLE_DIR-derived default"
        );

        set_env(BUNDLE_DIR_ENV_VAR, Some("   "));
        assert_eq!(
            bundle_dir(),
            PathBuf::from("/mnt/efs/observatory").join(DEFAULT_BUNDLE_DIR_SUBPATH),
            "a whitespace-only override must fall through to the durable-dir default"
        );

        set_env(BUNDLE_DIR_ENV_VAR, None);
        set_env(DURABLE_DIR_ENV_VAR, None);
    }

    /// RE-SCOPE r1: activation is discovered via `CURRENT`, not a pod env
    /// var — no `CURRENT` file (or a blank one) must close the gate, and a
    /// safe non-blank token must open it (subject to the graph_id check
    /// too).
    #[test]
    fn projector_gate_open_requires_graph_id_and_a_real_current_marker() {
        let _lock = env_serial().lock().unwrap_or_else(|p| p.into_inner());
        let dir = std::env::temp_dir().join(format!(
            "sophia-observatory-gate-unit-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).expect("create bundle dir");
        set_env(BUNDLE_DIR_ENV_VAR, Some(dir.to_str().unwrap()));

        assert!(
            !projector_gate_open(GRAPH_ID),
            "no CURRENT file at all must close the gate even for the observatory graph_id"
        );

        std::fs::write(dir.join(CURRENT_POINTER_FILE), "   ").expect("write blank CURRENT");
        assert!(
            !projector_gate_open(GRAPH_ID),
            "a blank CURRENT must close the gate"
        );

        std::fs::write(dir.join(CURRENT_POINTER_FILE), "gen-20260716T000000Z-1")
            .expect("write real CURRENT");
        assert!(
            projector_gate_open(GRAPH_ID),
            "a real, non-blank CURRENT must open the gate for the observatory graph_id"
        );
        assert!(
            !projector_gate_open("angels"),
            "a real CURRENT must still NOT open the gate for a non-observatory graph_id"
        );

        std::fs::write(dir.join(CURRENT_POINTER_FILE), "../etc/passwd")
            .expect("write unsafe CURRENT");
        assert!(
            !projector_gate_open(GRAPH_ID),
            "a path-traversal-shaped CURRENT value must never open the gate"
        );

        let _ = std::fs::remove_dir_all(&dir);
        set_env(BUNDLE_DIR_ENV_VAR, None);
    }

    /// A production `run_boot_hook_for_cell` proof (not just the pure
    /// `projector_gate_open` predicate) that the non-observatory path opens
    /// NO graph directory and NO Store: `build_mock_app_for_tests(false)`
    /// hands this a REAL `AppHandle` carrying NONE of the managed
    /// profile/graph-service state `garden_lib::headless::setup` would
    /// normally register — so if the gate were ever bypassed (a future
    /// regression reordering this function's checks), the FIRST thing that
    /// would happen is a real attempt to resolve a graph directory against
    /// that bare app. `graph_paths::profile_dir` falls back to
    /// `app.path().app_data_dir()` (never panics on a bare mock app) and
    /// `existing_graph_dir` only STATs a `graph.json` (never creates one
    /// unless self-heal is explicitly enabled, which this test never sets),
    /// so the bypass signature is unambiguous: the outcome would stop being
    /// `GateClosed` (it would become `Failed("graph not found: …")` instead)
    /// — a real, non-mocked regression signal, not an assumption.
    #[tokio::test]
    async fn run_boot_hook_for_cell_never_touches_a_graph_dir_or_store_when_the_gate_is_closed() {
        let _lock = env_serial().lock().unwrap_or_else(|p| p.into_inner());
        set_env(BUNDLE_DIR_ENV_VAR, None);
        set_env(DURABLE_DIR_ENV_VAR, None);
        let app = build_mock_app_for_tests(false);

        // The load-bearing case the A4 dispatch itself names: cloud-2 runs
        // this SAME binary for production graphs like `angels` too.
        let outcome = run_boot_hook_for_cell(&app, "angels").await;
        assert!(
            matches!(outcome, BootHookOutcome::GateClosed),
            "expected GateClosed for a non-observatory graph_id with no CURRENT marker, got {outcome:?}"
        );

        // Observatory graph_id, but no CURRENT marker anywhere (fallback
        // path never exists).
        let outcome = run_boot_hook_for_cell(&app, GRAPH_ID).await;
        assert!(
            matches!(outcome, BootHookOutcome::GateClosed),
            "expected GateClosed for the observatory graph_id with no CURRENT marker, got {outcome:?}"
        );
    }

    /// RE-SCOPE r1's generation-token protocol, exercised directly and
    /// deterministically (not via real thread timing, which would be
    /// flaky): simulate a racing publish by rewriting `CURRENT` to a new
    /// token BETWEEN a caller's pre-read and its post-read check via
    /// [`verify_current_still_names`] — exactly the interleaving
    /// [`run_catch_up_from_bundle_dir`] must catch (it calls this SAME
    /// function as its own post-read step, so this proves the real
    /// mechanism, not a re-implementation of it). Real file I/O throughout;
    /// only the INTERLEAVING is hand-driven, not the verification logic.
    #[test]
    fn verify_current_still_names_reports_generation_raced_when_current_changed() {
        let dir = std::env::temp_dir().join(format!(
            "sophia-observatory-race-unit-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).expect("create bundle dir");

        // The caller's pre-read: CURRENT names "gen-a".
        std::fs::write(dir.join(CURRENT_POINTER_FILE), "gen-a").expect("publish gen-a");
        let pre_read_token = read_current_token(&dir).expect("gen-a must be readable");
        assert_eq!(pre_read_token, "gen-a");

        // A second projector run races in and publishes "gen-b" BETWEEN the
        // caller's pre-read and its post-read check.
        std::fs::write(dir.join(CURRENT_POINTER_FILE), "gen-b").expect("publish gen-b");

        let result = verify_current_still_names(&dir, &pre_read_token);
        match result {
            Err(CatchUpFromDirError::GenerationRaced { attempted_token }) => {
                assert_eq!(
                    attempted_token, "gen-a",
                    "must report the token the caller STARTED reading, not the one that superseded it"
                );
            }
            other => panic!("expected GenerationRaced, got {other:?}"),
        }

        // The non-racing case, same helper: CURRENT unchanged verifies clean.
        assert!(
            verify_current_still_names(&dir, "gen-b").is_ok(),
            "an unchanged CURRENT must verify Ok"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The other half of the generation-token protocol: no `CURRENT` at all
    /// is [`CatchUpFromDirError::NotReady`], not a panic and not
    /// `GenerationRaced`.
    #[test]
    fn run_catch_up_from_bundle_dir_reports_not_ready_when_current_is_absent() {
        let dir = std::env::temp_dir().join(format!(
            "sophia-observatory-not-ready-unit-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).expect("create bundle dir");
        let profile_dir = std::env::temp_dir().join(format!(
            "sophia-observatory-not-ready-unit-profile-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(profile_dir.join("graphs").join(GRAPH_ID))
            .expect("create graph dir");
        let store =
            crate::rdf_store_service::open_graph_store(&profile_dir.join("graphs").join(GRAPH_ID))
                .expect("open real store");

        let result = run_catch_up_from_bundle_dir(&store, &dir);
        assert!(
            matches!(result, Err(CatchUpFromDirError::NotReady)),
            "expected NotReady with no CURRENT file, got {result:?}"
        );

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&profile_dir);
    }

    // -----------------------------------------------------------------
    // review r2 — single-flight apply (WRONG "run_boot_hook is safe to
    // call periodically under its timeout" / MISSING "a timeout/orphan
    // race test proving an old apply cannot overwrite a newer
    // generation"). Real fixture bytes, real `Store`, real `catch_up` —
    // ONLY its start is delayed (`set_test_injected_apply_delay_ms`,
    // standing in for the module doc's own documented slow case, "a
    // stalled NFS/EFS mount"), so the race window is deterministic
    // without depending on real `catch_up`'s (far too fast, against these
    // small fixtures) performance. `spawn_apply_call_count` is the
    // falsifiable signal: the OLD (r1) behavior would have incremented it
    // TWICE across the two calls below; this fix must keep it at 1.
    // -----------------------------------------------------------------

    const RACE_LIFECYCLE_JSON: &str = include_str!("fixtures/lifecycle_evaluation.input.json");
    const RACE_BILLING_JSON: &str = include_str!("fixtures/billing_llm_dau_v1.ok.json");
    const RACE_RAW_NDJSON: &str = include_str!("fixtures/valid.ndjson");

    fn race_full_bundle(cursor: &str) -> String {
        format!(
            "{{\"cursor_high_water_mark\":{cursor:?},\"lifecycle\":[{RACE_LIFECYCLE_JSON}],\"billing\":[{RACE_BILLING_JSON}]}}"
        )
    }

    fn race_publish_generation(dir: &Path, token: &str, cursor: &str) {
        std::fs::create_dir_all(dir).expect("create bundle dir");
        std::fs::write(bundle_json_path(dir, token), race_full_bundle(cursor))
            .expect("write obs-bundle.json");
        std::fs::write(raw_snapshot_path(dir, token), RACE_RAW_NDJSON)
            .expect("write raw-snapshot.ndjson");
        std::fs::write(dir.join(CURRENT_POINTER_FILE), token).expect("publish CURRENT");
    }

    #[tokio::test]
    async fn run_boot_hook_never_spawns_a_second_apply_while_a_slow_one_is_still_in_flight() {
        let _lock = env_serial().lock().unwrap_or_else(|p| p.into_inner());
        reset_last_applied_token_for_tests();
        set_test_injected_apply_delay_ms(0);
        // `SPAWN_APPLY_CALL_COUNT` is a process-wide counter shared with the
        // sibling `drain_in_flight_apply_...` test below (same `env_serial()`
        // lock, so never concurrent, but test EXECUTION ORDER is not
        // guaranteed) — assert on the DELTA this test itself causes, not an
        // absolute value.
        let baseline_spawn_count = spawn_apply_call_count();

        let dir = std::env::temp_dir().join(format!(
            "sophia-observatory-single-flight-unit-{}",
            uuid::Uuid::new_v4()
        ));
        race_publish_generation(&dir, "gen-slow", "cursor-single-flight");
        let profile_dir = std::env::temp_dir().join(format!(
            "sophia-observatory-single-flight-unit-profile-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(profile_dir.join("graphs").join(GRAPH_ID))
            .expect("create graph dir");
        let store =
            crate::rdf_store_service::open_graph_store(&profile_dir.join("graphs").join(GRAPH_ID))
                .expect("open real store");

        set_env(BUNDLE_DIR_ENV_VAR, Some(dir.to_str().unwrap()));
        // Real, genuine sleep BEFORE the real catch_up runs — well longer
        // than the tight budget the first call below gets.
        set_test_injected_apply_delay_ms(1200);
        set_env(APPLY_BUDGET_SECONDS_ENV_VAR, Some("1"));

        let call1_started = Instant::now();
        let outcome1 = run_boot_hook(GRAPH_ID, &store).await;
        let elapsed1 = call1_started.elapsed();
        match &outcome1 {
            BootHookOutcome::InProgress { token } => assert_eq!(token, "gen-slow"),
            other => panic!("expected InProgress (the slow apply outlives this call's 1s budget), got {other:?}"),
        }
        assert!(
            elapsed1 < Duration::from_millis(1100),
            "call must return within roughly its own 1s budget, not block for the full 1.2s \
             injected delay + catch_up; took {elapsed1:?}"
        );
        assert_eq!(
            spawn_apply_call_count() - baseline_spawn_count,
            1,
            "exactly one real apply task must have been spawned by this test so far"
        );

        // A generous budget for the second call — long enough for the SAME
        // (already ~1.2s along) task to finish.
        set_env(APPLY_BUDGET_SECONDS_ENV_VAR, Some("5"));
        let outcome2 = run_boot_hook(GRAPH_ID, &store).await;
        match outcome2 {
            BootHookOutcome::Applied(report) => {
                assert!(report.raw.added > 0, "the joined apply must materialize CaptureEvents");
                assert!(report.rollups.added > 0, "the joined apply must materialize rollup subjects");
            }
            other => panic!(
                "expected the SECOND call to join the SAME in-flight task and observe Applied, got {other:?}"
            ),
        }
        // THE crux assertion: still only ONE real apply was ever spawned —
        // the second call joined the SAME task rather than starting its
        // own. The r1 bug would have spawned a second one here, and (had
        // this been an OLDER generation racing a NEWER one instead) could
        // have silently regressed the Store after the fact.
        assert_eq!(
            spawn_apply_call_count() - baseline_spawn_count,
            1,
            "a second call while the first apply is still in flight must NEVER spawn a second, \
             concurrent one"
        );
        assert!(
            peek_in_flight(&dir).is_none(),
            "the in-flight slot must be empty once the task has actually finished and been observed"
        );

        set_test_injected_apply_delay_ms(0);
        set_env(BUNDLE_DIR_ENV_VAR, None);
        set_env(APPLY_BUDGET_SECONDS_ENV_VAR, None);
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&profile_dir);
    }

    /// [`drain_in_flight_apply`]'s own contract, exercised directly: a task
    /// still in flight at "shutdown" is joined (bounded by the SAME apply
    /// budget), and its outcome is reflected in
    /// [`last_applied_token`]/the Store — not silently dropped.
    #[tokio::test]
    async fn drain_in_flight_apply_joins_a_still_running_task_before_returning() {
        let _lock = env_serial().lock().unwrap_or_else(|p| p.into_inner());
        reset_last_applied_token_for_tests();
        set_test_injected_apply_delay_ms(0);

        let dir = std::env::temp_dir().join(format!(
            "sophia-observatory-drain-unit-{}",
            uuid::Uuid::new_v4()
        ));
        race_publish_generation(&dir, "gen-drain", "cursor-drain");
        let profile_dir = std::env::temp_dir().join(format!(
            "sophia-observatory-drain-unit-profile-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(profile_dir.join("graphs").join(GRAPH_ID))
            .expect("create graph dir");
        let store =
            crate::rdf_store_service::open_graph_store(&profile_dir.join("graphs").join(GRAPH_ID))
                .expect("open real store");

        set_env(BUNDLE_DIR_ENV_VAR, Some(dir.to_str().unwrap()));
        // Delay must outlive call 1's own tight budget, or call 1 would
        // simply observe completion itself and return `Applied` — never
        // reaching the `InProgress`/re-registration path this test needs.
        set_test_injected_apply_delay_ms(1200);
        set_env(APPLY_BUDGET_SECONDS_ENV_VAR, Some("1"));

        let outcome = run_boot_hook(GRAPH_ID, &store).await;
        assert!(
            matches!(outcome, BootHookOutcome::InProgress { .. }),
            "expected the apply to still be running past the 1s budget, got {outcome:?}"
        );
        // `run_boot_hook`'s own `InProgress` arm already re-registered the
        // still-running task in `in_flight_registry` (see its doc) — exactly
        // the state a real shutdown sequence would find. `drain_in_flight_apply`
        // resolves the SAME `bundle_dir()` (this test's env override), so it
        // picks up that registration without any extra setup here.
        set_env(APPLY_BUDGET_SECONDS_ENV_VAR, Some("5"));
        drain_in_flight_apply().await;

        assert_eq!(
            last_applied_token(&dir).as_deref(),
            Some("gen-drain"),
            "drain_in_flight_apply must apply the finished task's result before returning"
        );

        set_test_injected_apply_delay_ms(0);
        set_env(BUNDLE_DIR_ENV_VAR, None);
        set_env(APPLY_BUDGET_SECONDS_ENV_VAR, None);
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&profile_dir);
    }

    /// MISSING (review r2) "a real reader-versus-second-publication rollover
    /// test; the current helper only rewrites CURRENT before calling the
    /// verifier and does not race run_catch_up_from_bundle_dir" — this test
    /// races a REAL second publish through [`run_catch_up_from_bundle_dir`]'s
    /// OWN read sequence (via [`set_mid_read_test_hook`], landing it exactly
    /// between the bundle-json and raw-snapshot reads), not merely against
    /// [`verify_current_still_names`] in isolation. Real fixture bytes, real
    /// `Store`, real file writes for both generations — only the
    /// INTERLEAVING is hand-driven (this module's own established
    /// convention for race coverage, avoiding real-thread-timing flakiness).
    #[test]
    fn run_catch_up_from_bundle_dir_discards_a_real_second_publish_landing_between_its_own_reads() {
        // `MID_READ_TEST_HOOK` is a process-wide static, shared with any
        // OTHER test in this module that calls `run_catch_up_from_bundle_dir`
        // (directly, or via `run_boot_hook`'s `spawn_apply`) — hold the SAME
        // lock those tests hold for their whole bodies so this test's
        // hook-active window can never overlap with theirs.
        let _lock = env_serial().lock().unwrap_or_else(|p| p.into_inner());
        let dir = std::env::temp_dir().join(format!(
            "sophia-observatory-mid-read-race-unit-{}",
            uuid::Uuid::new_v4()
        ));
        race_publish_generation(&dir, "gen-a", "cursor-mid-read-race-a");
        let profile_dir = std::env::temp_dir().join(format!(
            "sophia-observatory-mid-read-race-unit-profile-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(profile_dir.join("graphs").join(GRAPH_ID))
            .expect("create graph dir");
        let store =
            crate::rdf_store_service::open_graph_store(&profile_dir.join("graphs").join(GRAPH_ID))
                .expect("open real store");

        let dir_for_hook = dir.clone();
        set_mid_read_test_hook(Some(Box::new(move || {
            // A second, genuinely different projector run races in and
            // publishes "gen-b" — a REAL file write via the same
            // `race_publish_generation` helper every other test in this
            // module uses — landing exactly between the two reads
            // `run_catch_up_from_bundle_dir` performs.
            race_publish_generation(&dir_for_hook, "gen-b", "cursor-mid-read-race-b");
        })));

        let result = run_catch_up_from_bundle_dir(&store, &dir);
        set_mid_read_test_hook(None);

        match result {
            Err(CatchUpFromDirError::GenerationRaced { attempted_token }) => {
                assert_eq!(
                    attempted_token, "gen-a",
                    "must report the token this call STARTED reading, not the one that raced it"
                );
            }
            other => panic!("expected GenerationRaced, got {other:?}"),
        }

        // THE crux assertion: catch_up must NEVER have been reached — the
        // Store must still be completely empty. Without the fix, this
        // window either silently applied "gen-a" (already stale — CURRENT
        // had already moved to "gen-b" by then) or crashed on a torn read;
        // either way something would have landed in the Store.
        let raw = crate::observatory::graph_identity::raw_graph_iri();
        let rollups = crate::observatory::graph_identity::rollups_graph_iri();
        assert!(
            !crate::observatory::authority_harness::graph_has_any_quad(&store, &raw)
                .expect("graph_has_any_quad(raw)"),
            "a raced read must write NOTHING to the raw graph"
        );
        assert!(
            !crate::observatory::authority_harness::graph_has_any_quad(&store, &rollups)
                .expect("graph_has_any_quad(rollups)"),
            "a raced read must write NOTHING to the rollups graph"
        );

        // The next, clean call (no race) picks up "gen-b" — the raced read
        // was discarded, not lost track of.
        let clean_result = run_catch_up_from_bundle_dir(&store, &dir);
        assert!(
            clean_result.is_ok(),
            "the next unraced call must cleanly apply the now-current generation, got {clean_result:?}"
        );
        let report = clean_result.unwrap();
        assert!(
            report.raw.added > 0,
            "the clean follow-up call must materialize CaptureEvents"
        );
        assert!(
            report.rollups.added > 0,
            "the clean follow-up call must materialize rollup subjects"
        );

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&profile_dir);
    }

    /// A single literal `?v` for the unique `(?s a type_iri; predicate_iri
    /// ?v)` pattern in `graph_iri` — the SAME small helper
    /// `tests/observatory_boot.rs` uses to read back `obs:cursorHighWaterMark`
    /// after a real `catch_up`, reproduced here (not imported: this is an
    /// in-crate unit test module, that one is a separate external `tests/`
    /// crate) so this module's own race test can assert on the Store's
    /// ACTUAL recorded cursor, not merely on `BootHookOutcome`'s shape.
    fn single_literal(
        store: &Store,
        graph_iri: &str,
        type_iri: &str,
        predicate_iri: &str,
    ) -> String {
        let query = format!(
            "SELECT ?v WHERE {{ GRAPH <{graph_iri}> {{ ?s a <{type_iri}> ; <{predicate_iri}> ?v }} }}"
        );
        let results = oxigraph::sparql::SparqlEvaluator::new()
            .parse_query(&query)
            .expect("parse literal query")
            .on_store(store)
            .execute()
            .expect("execute literal query");
        let oxigraph::sparql::QueryResults::Solutions(solutions) = results else {
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
            oxigraph::model::Term::Literal(lit) => lit.value().to_string(),
            other => panic!("{predicate_iri} is not a literal: {other:?}"),
        }
    }

    /// review r3 MISSING "generation B published while generation A's apply
    /// is in flight" — the exact race review r2's single-flight mechanism
    /// was supposed to handle correctly, but which no EXISTING test actually
    /// drove: [`run_boot_hook_never_spawns_a_second_apply_while_a_slow_one_is_still_in_flight`]
    /// only ever exercises ONE generation throughout (the second call joins
    /// the SAME task waiting on the SAME "gen-slow"); it never publishes a
    /// DIFFERENT, newer generation WHILE that task is still running.
    ///
    /// This test does: generation "gen-a" is published, an apply for it is
    /// spawned (with a real, injected delay standing in for a slow
    /// EFS/catch_up), and — while that blocking task is STILL asleep,
    /// genuinely in flight, well before it has read anything — a REAL
    /// second publish lands: "gen-b", a DIFFERENT generation with a
    /// DIFFERENT (distinguishable) cursor. A second call to
    /// [`run_boot_hook`] then arrives, sees CURRENT now naming "gen-b", and
    /// — per single-flight — must NOT spawn a second, concurrent apply; it
    /// joins the SAME in-flight task instead. That task, once its delay
    /// elapses, runs [`run_catch_up_from_bundle_dir_with_token`], which
    /// re-reads `CURRENT` for itself at that point (see the module doc's
    /// "generation-token protocol") — finding "gen-b", not the stale
    /// "gen-a" it was originally spawned for — and applies GEN-B's content.
    ///
    /// Asserts the exact three things review r3 named:
    ///   1. at most one concurrent apply EVER runs (`spawn_apply_call_count`
    ///      never exceeds baseline+1, across BOTH calls);
    ///   2. the recorded token ([`last_applied_token`]) matches what the
    ///      worker ITSELF read and verified ("gen-b"), never the stale
    ///      `ready_token` hint call 1 captured before "gen-b" even existed;
    ///   3. the Store's own recorded state (`obs:cursorHighWaterMark`,
    ///      queried directly, not inferred from `BootHookOutcome`) reflects
    ///      "cursor-b" — gen-b's content — never regressing to gen-a's,
    ///      even though the apply task was ORIGINALLY spawned against gen-a.
    #[tokio::test]
    async fn run_boot_hook_applies_the_generation_published_during_an_in_flight_apply_never_regressing(
    ) {
        let _lock = env_serial().lock().unwrap_or_else(|p| p.into_inner());
        reset_last_applied_token_for_tests();
        set_test_injected_apply_delay_ms(0);
        let baseline_spawn_count = spawn_apply_call_count();

        let dir = std::env::temp_dir().join(format!(
            "sophia-observatory-mid-flight-republish-unit-{}",
            uuid::Uuid::new_v4()
        ));
        race_publish_generation(&dir, "gen-a", "cursor-mid-flight-a");
        let profile_dir = std::env::temp_dir().join(format!(
            "sophia-observatory-mid-flight-republish-unit-profile-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(profile_dir.join("graphs").join(GRAPH_ID))
            .expect("create graph dir");
        let store =
            crate::rdf_store_service::open_graph_store(&profile_dir.join("graphs").join(GRAPH_ID))
                .expect("open real store");

        set_env(BUNDLE_DIR_ENV_VAR, Some(dir.to_str().unwrap()));
        // Long enough that call 1's own short budget elapses well before the
        // blocking task even starts reading anything, leaving a wide real
        // window to publish "gen-b" while it is still genuinely asleep.
        set_test_injected_apply_delay_ms(2000);
        set_env(APPLY_BUDGET_SECONDS_ENV_VAR, Some("1"));

        let outcome1 = run_boot_hook(GRAPH_ID, &store).await;
        match &outcome1 {
            BootHookOutcome::InProgress { token } => assert_eq!(token, "gen-a"),
            other => panic!(
                "expected InProgress (the 2s-delayed apply outlives this call's 1s budget), got {other:?}"
            ),
        }
        assert_eq!(
            spawn_apply_call_count() - baseline_spawn_count,
            1,
            "exactly one real apply task must have been spawned so far (for gen-a)"
        );

        // THE race: a REAL second publish lands — a genuinely different
        // generation, with a genuinely different cursor — WHILE the gen-a
        // apply task is still asleep (only ~1s of its 2s delay has elapsed).
        race_publish_generation(&dir, "gen-b", "cursor-mid-flight-b");

        // A generous budget — long enough for the remaining ~1s of delay
        // plus the real catch_up itself.
        set_env(APPLY_BUDGET_SECONDS_ENV_VAR, Some("5"));
        let outcome2 = run_boot_hook(GRAPH_ID, &store).await;
        match outcome2 {
            BootHookOutcome::Applied(report) => {
                assert!(
                    report.raw.added > 0,
                    "the joined apply must materialize CaptureEvents"
                );
                assert!(
                    report.rollups.added > 0,
                    "the joined apply must materialize rollup subjects"
                );
            }
            other => panic!(
                "expected the second call to join the in-flight task and observe it apply the \
                 now-current generation (gen-b), got {other:?}"
            ),
        }

        // 1. At most one concurrent apply EVER ran — single-flight held
        // across the republish, not merely across two calls for the SAME
        // generation.
        assert_eq!(
            spawn_apply_call_count() - baseline_spawn_count,
            1,
            "a generation published while an apply is already in flight must NEVER cause a second, \
             concurrent apply to spawn"
        );

        // 2. The recorded token matches what the worker itself read and
        // verified (gen-b) — never the stale ready_token (gen-a) call 1
        // captured before gen-b was even published.
        assert_eq!(
            last_applied_token(&dir).as_deref(),
            Some("gen-b"),
            "the recorded last-applied token must be gen-b (what the worker itself read+verified \
             once it actually ran), not gen-a (the stale hint captured before gen-b existed)"
        );

        // 3. The Store's OWN recorded state reflects gen-b's content, never
        // regressing to gen-a's — queried directly, not inferred.
        let rollups = crate::observatory::graph_identity::rollups_graph_iri();
        let obs_ns = crate::observatory::mapping::OBS_NS;
        let cursor = single_literal(
            &store,
            &rollups,
            &format!("{obs_ns}ProjectionRun"),
            &format!("{obs_ns}cursorHighWaterMark"),
        );
        assert_eq!(
            cursor, "cursor-mid-flight-b",
            "the Store's own obs:cursorHighWaterMark must reflect gen-b (the generation actually \
             applied), never gen-a (a regression to stale content)"
        );

        assert!(
            peek_in_flight(&dir).is_none(),
            "the in-flight slot must be empty once the task has actually finished and been observed"
        );

        set_test_injected_apply_delay_ms(0);
        set_env(BUNDLE_DIR_ENV_VAR, None);
        set_env(APPLY_BUDGET_SECONDS_ENV_VAR, None);
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&profile_dir);
    }
}
