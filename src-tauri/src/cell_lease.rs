//! Cross-process write lease — cell-side bridge client (U8 spec §3.3).
//!
//! `platform-next`'s gateway is the sole authority for a `platform-next-cell-
//! leases` DynamoDB item per graph, mutated only by conditional `UpdateItem`.
//! This module is the *cell's* half of that protocol: it renews the lease on
//! a timer, exposes a zero-I/O validity check to the flush funnel (Gate A,
//! `cell_durability.rs`), and bridges the funnel's two remaining gates (Gate
//! B/C, the real fencing CAS around the snapshot rename) from the synchronous
//! flush thread to the async runtime that actually speaks HTTP. It never
//! talks to DynamoDB directly — only through the gateway's `/internal/lease/*`
//! routes (U8-3) — and it never links an AWS SDK: **no AWS crate may enter
//! `Cargo.toml`** because of this module.
//!
//! # Naming (load-bearing)
//!
//! The type is `WriteLease`, never `LeaseHandle`. Three unrelated things in
//! this crate are already called "lease": `crdt_engine::persistence_
//! coordinator::GraphPersistenceLease` (a purely local, in-process mutex
//! guard around CRDT persistence), the process-local `STORE_WRITE_EPOCH`
//! counter (`cell_durability.rs`, resets every boot), and `GraphGate::
//! generation`. None of those cross a process boundary; `WriteLease` is
//! reserved exclusively for this cross-process, DynamoDB-backed bridge lease
//! (spec §1.3, ambiguity A5).
//!
//! # The desktop null path
//!
//! [`handle`] returns `None` unless *both* `GARDEN_DURABLE_DIR` and
//! `GARDEN_DURABLE_EPOCH` are set. The Tauri desktop app never sets either,
//! so every downstream gate that consults [`handle`] is a no-op there — the
//! desktop build is untouched by this module's existence.
//!
//! # Credential
//!
//! The gateway already forwards its own `PN_CELL_TOKEN` onto every cell as
//! `GARDEN_LOOPBACK_TOKEN` (platform-next `gateway/src/cell.rs:49`, worktree
//! `pn-u8-lease` @ `d8acfa9`: `env_var("GARDEN_LOOPBACK_TOKEN", &cfg.cell_token)`,
//! where `cfg.cell_token` is `PN_CELL_TOKEN`). That value is reused verbatim
//! as the `Authorization: Bearer` credential this module presents to `/
//! internal/lease/*` — no second secret, no new env var. Per spec §3.1, the
//! credential only gets you in the door; the actual authorization *is* the
//! `(holder, epoch)` CAS the gateway checks server-side.
//!
//! # The sync/async bridge (Gate B/C)
//!
//! The flush funnel (`cell_durability::flush_inner_with_gate_wait`, this
//! worktree @ 4ad36a1 `:679`, serialized by a plain `std::sync::Mutex` at
//! `:687`) runs on a blocking thread reached via `run_flush_blocking`
//! (`examples/gardend.rs:753`, per the ratified spec). It must never call
//! `.await` or `block_on` — both would either deadlock against the very
//! runtime this module's renew task needs, or violate the "flush thread never
//! touches the executor" rule the buildability judge flagged as the single
//! most likely wedge point in this design. The bridge is therefore:
//!
//! - flush thread → async task: [`WriteLease::publish`] sends a
//!   [`PublishRequest`] over a [`tokio::sync::mpsc::UnboundedSender`].
//!   Sending on an unbounded sender is a **plain, non-async function** — it
//!   requires no runtime and never blocks — so calling it from a thread with
//!   no Tokio context is sound.
//! - async task → flush thread: the async publish worker performs the real
//!   HTTP call and replies over a `std::sync::mpsc::SyncSender`, a channel
//!   built exactly for being read by a blocking `recv_timeout` — no `.await`
//!   anywhere on the flush thread's side of the round trip.
//!
//! Timeout on the flush thread's `recv_timeout` is treated identically to a
//! refusal (spec §3.3): lost work, never corruption.
//!
//! # Relative time only
//!
//! The cell never parses an absolute timestamp from the gateway (spec §1.4).
//! Renew responses carry `ttl_remaining_ms`/`effective_in_ms`; this module
//! records `t0 = Instant::now()` **before** sending each renew and derives the
//! new deadline from `t0`, which is conservative by the full request RTT in
//! the safe direction. `deadline_ms` is therefore not wall-clock time — it is
//! milliseconds since this process's own monotonic epoch (see
//! [`process_epoch`]), comparable only against `Instant::now()` read in this
//! same process.
//!
//! # Scope boundary
//!
//! This module owns: the `WriteLease` type, the renew loop, the publish
//! bridge, and the two boot rules spec §3.2 assigns to it explicitly
//! (legacy-unfenced-with-warning when the epoch env is absent; refuse-to-boot
//! when `GARDEN_LEASE_MODE=enforce` with the epoch absent/invalid). It does
//! **not** own: Gates A/B/C themselves (those splice into
//! `cell_durability::flush_inner_with_gate_wait`), the boot-time "wait out the
//! effective deadline → repair → hydrate → serve" orchestration, or the
//! lease-loss reaction in `examples/gardend.rs` (`FENCED`/`exit(4)`/guarded
//! final flush) — those consume this module's public surface
//! ([`handle`], [`init`], [`WriteLease::terminal_reason`],
//! [`WriteLease::is_fenced`]) but are built elsewhere in the U8 task set.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::{mpsc as std_mpsc, OnceLock};
use std::time::{Duration, Instant};

use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc as tokio_mpsc, oneshot, Notify};

// ---------------------------------------------------------------------------
// Env vars
// ---------------------------------------------------------------------------

const DURABLE_DIR_ENV: &str = "GARDEN_DURABLE_DIR";
const EPOCH_ENV: &str = "GARDEN_DURABLE_EPOCH";
const LEASE_URL_ENV: &str = "GARDEN_LEASE_URL";
const LEASE_MODE_ENV: &str = "GARDEN_LEASE_MODE";
const CELL_ID_ENV: &str = "GARDEN_CELL_ID";
const GRAPH_ID_ENV: &str = "GARDEN_CELL_GRAPH_ID";
const MACHINE_RUN_ID_ENV: &str = "GARDEN_CELL_MACHINE_RUN_ID";
const LOOPBACK_TOKEN_ENV: &str = "GARDEN_LOOPBACK_TOKEN";
const RENEW_MS_ENV: &str = "GARDEN_LEASE_RENEW_MS";
const MARGIN_MS_ENV: &str = "GARDEN_LEASE_MARGIN_MS";
const FENCED_MAX_MS_ENV: &str = "GARDEN_LEASE_FENCED_MAX_MS";
const PUBLISH_TIMEOUT_MS_ENV: &str = "GARDEN_LEASE_PUBLISH_TIMEOUT_MS";
const LAST_SNAP_ENV: &str = "GARDEN_LEASE_LAST_SNAP";

// Defaults per spec §1.4 (the ratified 20/5/8, decision D2).
const DEFAULT_RENEW_MS: u64 = 5_000;
const DEFAULT_MARGIN_MS: u64 = 8_000;
const DEFAULT_FENCED_MAX_MS: u64 = 300_000;
const DEFAULT_PUBLISH_TIMEOUT_MS: u64 = 5_000;
/// Bounded retry backoff on a transient renew failure (spec §1.5) — a
/// rolling gateway restart (`maxUnavailable: 1` of 2 replicas) must not eat
/// the write margin.
const RENEW_RETRY_BACKOFF_MS: u64 = 250;

// ---------------------------------------------------------------------------
// Mode
// ---------------------------------------------------------------------------

/// `GARDEN_LEASE_MODE`. Observe evaluates and testifies would-have-fenced
/// outcomes but never refuses (spec §3.4's "observe mode" row); enforce
/// refuses. This module only *carries* the mode — the refuse/testify
/// decision is made where the gates live (`cell_durability.rs`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeaseMode {
    Observe,
    Enforce,
}

impl LeaseMode {
    /// Pure parse — no env access, so it is directly unit-testable. Missing
    /// or blank defaults to `Observe` (the gateway always sets this
    /// explicitly once a graph is lease-aware; defaulting the *unset* case to
    /// the non-refusing mode matches "observe must not regress availability",
    /// spec §3.2). An unrecognized value also falls back to `Observe` with a
    /// loud warning rather than silently becoming `Enforce`.
    fn parse(raw: Option<&str>) -> Self {
        match raw.map(str::trim) {
            Some(value) if value.eq_ignore_ascii_case("enforce") => LeaseMode::Enforce,
            Some(value) if value.eq_ignore_ascii_case("observe") => LeaseMode::Observe,
            None | Some("") => LeaseMode::Observe,
            Some(other) => {
                log::warn!(
                    "{LEASE_MODE_ENV}={other:?} is not observe|enforce; defaulting to observe"
                );
                LeaseMode::Observe
            }
        }
    }
}

/// Content-free observe-mode dependency testimony categories. The numeric
/// values are stored atomically so concurrent renew and flush threads emit a
/// Process-wide monotone count of observe-mode lease-degradation testimonies.
/// Read via [`observe_degradation_events`] by the durable receipt watermark.
static OBSERVE_DEGRADATION_EVENTS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Snapshot of the observe-degradation event count (monotone).
pub(crate) fn observe_degradation_events() -> u64 {
    OBSERVE_DEGRADATION_EVENTS.load(Ordering::Acquire)
}

/// degradation only when its reason changes, and recovery only once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum ObserveLeaseDegradation {
    GateAMargin = 1,
    GateBPublishIntent = 2,
    GateCPublishCommit = 3,
    RenewLost = 4,
    RenewForfeit = 5,
    RenewUnavailable = 6,
}

impl ObserveLeaseDegradation {
    #[cfg(feature = "headless")]
    fn testimony_code(self) -> crate::capture_event::DependencyDetailCode {
        use crate::capture_event::DependencyDetailCode;
        match self {
            Self::GateAMargin => DependencyDetailCode::LeaseGateAMargin,
            Self::GateBPublishIntent => DependencyDetailCode::LeaseGateBPublishIntent,
            Self::GateCPublishCommit => DependencyDetailCode::LeaseGateCPublishCommit,
            Self::RenewLost => DependencyDetailCode::LeaseRenewLost,
            Self::RenewForfeit => DependencyDetailCode::LeaseRenewForfeit,
            Self::RenewUnavailable => DependencyDetailCode::LeaseRenewUnavailable,
        }
    }
}

impl std::fmt::Display for LeaseMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            LeaseMode::Observe => "observe",
            LeaseMode::Enforce => "enforce",
        })
    }
}

// ---------------------------------------------------------------------------
// Boot decision (spec §3.2)
// ---------------------------------------------------------------------------

/// Every env var this module's boot decision consults, gathered in one place
/// so the decision itself ([`decide_boot`]) can be a pure function tested
/// with plain literals — never by mutating real process env vars (several of
/// these names, e.g. `GARDEN_DURABLE_DIR`, are already read by other modules'
/// own env-mutating tests elsewhere in this crate; sharing process-global
/// state across independent test suites is a real hazard this design avoids
/// entirely rather than serializing around).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct RawEnv {
    pub durable_dir: Option<String>,
    pub epoch: Option<String>,
    pub mode: Option<String>,
    pub graph_id: Option<String>,
    pub holder: Option<String>,
    pub lease_url: Option<String>,
    pub token: Option<String>,
    pub renew_ms: Option<String>,
    pub margin_ms: Option<String>,
    pub fenced_max_ms: Option<String>,
    pub publish_timeout_ms: Option<String>,
    pub last_snap: Option<String>,
}

impl RawEnv {
    fn from_process_env() -> Self {
        let var = |name: &str| std::env::var(name).ok();
        Self {
            durable_dir: var(DURABLE_DIR_ENV),
            epoch: var(EPOCH_ENV),
            mode: var(LEASE_MODE_ENV),
            // Owner-scoped cells lease their opaque physical generation.
            // Legacy cells have no GARDEN_CELL_ID and retain graph-id keyed
            // leases byte-for-byte.
            graph_id: lease_graph_identity(var(CELL_ID_ENV), var(GRAPH_ID_ENV)),
            holder: var(MACHINE_RUN_ID_ENV),
            lease_url: var(LEASE_URL_ENV),
            token: var(LOOPBACK_TOKEN_ENV),
            renew_ms: var(RENEW_MS_ENV),
            margin_ms: var(MARGIN_MS_ENV),
            fenced_max_ms: var(FENCED_MAX_MS_ENV),
            publish_timeout_ms: var(PUBLISH_TIMEOUT_MS_ENV),
            last_snap: var(LAST_SNAP_ENV),
        }
    }
}

fn lease_graph_identity(cell_id: Option<String>, graph_id: Option<String>) -> Option<String> {
    cell_id
        .filter(|value| !value.trim().is_empty())
        .or(graph_id)
}

fn non_blank(raw: Option<&str>) -> Option<&str> {
    raw.map(str::trim).filter(|value| !value.is_empty())
}

fn parse_env_u64(raw: Option<&str>, default: u64, var_name: &str) -> u64 {
    match non_blank(raw) {
        None => default,
        Some(value) => match value.parse::<u64>() {
            Ok(parsed) => parsed,
            Err(_) => {
                log::warn!("{var_name}={value:?} is not a valid u64; using default {default}");
                default
            }
        },
    }
}

fn parse_optional_env_u64(raw: Option<&str>, var_name: &str) -> Option<u64> {
    let value = non_blank(raw)?;
    match value.parse::<u64>() {
        Ok(parsed) => Some(parsed),
        Err(_) => {
            log::warn!("{var_name}={value:?} is not a valid u64; treating it as absent");
            None
        }
    }
}

/// Fully resolved settings for a durable, lease-aware boot. Built only by
/// [`decide_boot`]'s `Lease` arm.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LeaseConfig {
    pub graph_id: String,
    pub holder: String,
    pub epoch: u64,
    pub mode: LeaseMode,
    pub lease_url: String,
    pub token: String,
    pub renew_ms: u64,
    pub margin_ms: u64,
    pub fenced_max_ms: u64,
    pub publish_timeout_ms: u64,
    /// Claim-time authority high-water mark. Kept locally so observe-mode
    /// pruning cannot delete the snapshot a later enforce boot must restore.
    pub last_snap: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BootDecision {
    /// `GARDEN_DURABLE_DIR` absent: desktop / non-cell process. `handle()`
    /// stays `None` forever; nothing else in this module ever runs.
    Desktop,
    /// `GARDEN_DURABLE_DIR` set, `GARDEN_DURABLE_EPOCH` absent: an old
    /// gateway spawned a new cell image (rollout-safe by design, spec §3.2).
    /// Legacy-unfenced — every gate downstream is a no-op, with a loud
    /// warning testimony. `handle()` stays `None`.
    LegacyUnfenced,
    /// A misconfiguration that must not boot: `GARDEN_LEASE_MODE=enforce`
    /// with the epoch absent/invalid (the spec's explicit tripwire), or —
    /// this module's own extension of the same fail-closed principle — the
    /// epoch present but one of the other fields the lease protocol cannot
    /// function without (`holder`, `graph_id`, lease URL, credential) is
    /// missing, in EITHER mode. An epoch with no way to renew it is not a
    /// safe "legacy" fallback; it is a lease this process can never keep.
    RefuseBoot(String),
    /// Durable dir + epoch + everything else required are present: mint a
    /// `WriteLease` under this config.
    Lease(LeaseConfig),
}

/// Pure — no I/O, no global state, no env access. See [`RawEnv`]'s doc for
/// why the fields are gathered before this is called rather than read here.
pub(crate) fn decide_boot(raw: &RawEnv) -> BootDecision {
    if non_blank(raw.durable_dir.as_deref()).is_none() {
        return BootDecision::Desktop;
    }

    let mode = LeaseMode::parse(raw.mode.as_deref());

    let epoch = match non_blank(raw.epoch.as_deref()) {
        None => {
            return if mode == LeaseMode::Enforce {
                BootDecision::RefuseBoot(format!(
                    "{LEASE_MODE_ENV}=enforce requires {EPOCH_ENV}, which is absent"
                ))
            } else {
                BootDecision::LegacyUnfenced
            };
        }
        Some(value) => match value.parse::<u64>() {
            Ok(epoch) => epoch,
            Err(_) => {
                let reason = format!("{EPOCH_ENV}={value:?} is not a valid u64");
                return if mode == LeaseMode::Enforce {
                    BootDecision::RefuseBoot(reason)
                } else {
                    log::warn!("{reason}; treating this boot as legacy-unfenced");
                    BootDecision::LegacyUnfenced
                };
            }
        },
    };

    // Beyond this point the epoch is present, which the spec treats as
    // "this is a lease-aware spawn" — the remaining fields are not optional
    // in either mode: an enforce boot obviously cannot renew without them,
    // and an observe boot that silently ran unfenced despite an epoch being
    // present would defeat the entire point of dress-rehearsing the
    // protocol before the enforce flip.
    let holder = match non_blank(raw.holder.as_deref()) {
        Some(value) => value.to_string(),
        None => {
            return BootDecision::RefuseBoot(format!(
                "{EPOCH_ENV} is set but {MACHINE_RUN_ID_ENV} is absent — cannot derive a holder identity"
            ));
        }
    };
    let graph_id = match non_blank(raw.graph_id.as_deref()) {
        Some(value) => value.to_string(),
        None => {
            return BootDecision::RefuseBoot(format!(
                "{EPOCH_ENV} is set but neither {CELL_ID_ENV} nor {GRAPH_ID_ENV} is present"
            ));
        }
    };
    let lease_url = match non_blank(raw.lease_url.as_deref()) {
        Some(value) => value.trim_end_matches('/').to_string(),
        None => {
            return BootDecision::RefuseBoot(format!(
                "{EPOCH_ENV} is set but {LEASE_URL_ENV} is absent"
            ));
        }
    };
    let token = match non_blank(raw.token.as_deref()) {
        Some(value) => value.to_string(),
        None => {
            return BootDecision::RefuseBoot(format!(
                "{EPOCH_ENV} is set but {LOOPBACK_TOKEN_ENV} is absent — no credential to present to the lease authority"
            ));
        }
    };

    let renew_ms = parse_env_u64(raw.renew_ms.as_deref(), DEFAULT_RENEW_MS, RENEW_MS_ENV);
    let margin_ms = parse_env_u64(raw.margin_ms.as_deref(), DEFAULT_MARGIN_MS, MARGIN_MS_ENV);
    let fenced_max_ms = parse_env_u64(
        raw.fenced_max_ms.as_deref(),
        DEFAULT_FENCED_MAX_MS,
        FENCED_MAX_MS_ENV,
    );
    let publish_timeout_ms = parse_env_u64(
        raw.publish_timeout_ms.as_deref(),
        DEFAULT_PUBLISH_TIMEOUT_MS,
        PUBLISH_TIMEOUT_MS_ENV,
    );
    let last_snap = parse_optional_env_u64(raw.last_snap.as_deref(), LAST_SNAP_ENV);

    BootDecision::Lease(LeaseConfig {
        graph_id,
        holder,
        epoch,
        mode,
        lease_url,
        token,
        renew_ms,
        margin_ms,
        fenced_max_ms,
        publish_timeout_ms,
        last_snap,
    })
}

// ---------------------------------------------------------------------------
// Process-relative clock (the relative-time contract, spec §1.4)
// ---------------------------------------------------------------------------

static PROCESS_EPOCH: OnceLock<Instant> = OnceLock::new();

fn process_epoch() -> Instant {
    *PROCESS_EPOCH.get_or_init(Instant::now)
}

fn instant_to_millis(instant: Instant) -> u64 {
    instant
        .saturating_duration_since(process_epoch())
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}

fn now_ms() -> u64 {
    instant_to_millis(Instant::now())
}

/// The renew task's deadline math: anchored to `t0` (recorded **before** the
/// request was sent), never to whenever the response happens to arrive —
/// conservative by the full RTT in the safe direction (spec §1.4). Pure and
/// directly unit-tested.
fn deadline_from_send_time(t0: Instant, ttl_remaining_ms: u64) -> u64 {
    instant_to_millis(t0).saturating_add(ttl_remaining_ms)
}

// ---------------------------------------------------------------------------
// Terminal reasons (spec §3.5)
// ---------------------------------------------------------------------------

/// Positive evidence that this incarnation must exit(4) without a final
/// flush — the final flush would be the zombie write the whole protocol
/// exists to forbid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalReason {
    /// `409 lease_lost` — a successor has claimed a higher epoch.
    LeaseLost { current_epoch: Option<u64> },
    /// `409 lease_forfeit` — the gateway judged the flush wedged past
    /// `PN_LEASE_STUCK_FLUSH_MS` (spec §1.6).
    LeaseForfeit,
    /// No successor evidence at all — just `GARDEN_LEASE_FENCED_MAX_MS`
    /// straight of no successful renew (spec §3.5's 300 s cap).
    FencedTimeout,
}

// ---------------------------------------------------------------------------
// Publish bridge types (Gate B/C, spec §3.3/§3.4)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublishPhase {
    Intent,
    Commit,
    Abort,
}

impl PublishPhase {
    fn as_wire(self) -> &'static str {
        match self {
            PublishPhase::Intent => "intent",
            PublishPhase::Commit => "commit",
            PublishPhase::Abort => "abort",
        }
    }
}

/// The authority's machine-readable reason for refusing a publish CAS.
///
/// Retaining this code is correctness-critical at Gate B: platform-next can
/// return `lease_lost` with the *same* epoch when the row became `retiring`,
/// and `lease_forfeit` without any epoch. Neither can be inferred from
/// `current_epoch` alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublishRefusalCode {
    LeaseLost,
    LeaseForfeit,
    LeaseContended,
    Other,
}

impl PublishRefusalCode {
    fn from_wire(code: &str) -> Self {
        match code {
            "lease_lost" => Self::LeaseLost,
            "lease_forfeit" => Self::LeaseForfeit,
            "lease_contended" => Self::LeaseContended,
            _ => Self::Other,
        }
    }
}

/// Gate B/C's outcome. Both variants mean the same thing to a caller: do not
/// treat the durable tree as advanced. `Refused` (409) and
/// `AuthorityUnavailable` (503, transport error, or a timed-out reply) are
/// kept distinct because a refusal's code and/or a strictly newer epoch can
/// be positive terminal evidence, while an unavailable authority is only
/// enough evidence to fence (spec §3.3/§3.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublishError {
    Refused {
        code: PublishRefusalCode,
        current_epoch: Option<u64>,
    },
    AuthorityUnavailable,
}

struct PublishRequest {
    seq: u64,
    phase: PublishPhase,
    reply: std_mpsc::SyncSender<Result<(), PublishError>>,
}

// ---------------------------------------------------------------------------
// WriteLease
// ---------------------------------------------------------------------------

/// The cell-side handle on one incarnation's claim. Constructed once per
/// process (by [`init`]) and never rebuilt — a new epoch means a new process,
/// never a mutation of this one's `epoch`/`holder`.
///
/// `epoch`, `holder`, `mode`, `deadline_ms`, `fenced` are the fields spec
/// §3.3 names explicitly; everything below them is the plumbing the same
/// section's prose requires (the renew task, the sync/async publish bridge)
/// but does not spell out as struct fields.
pub struct WriteLease {
    epoch: u64,
    holder: String,
    mode: LeaseMode,
    /// Millis since [`process_epoch`]; `0` means "never yet renewed
    /// successfully". Updated only by the renew task.
    deadline_ms: AtomicU64,
    /// A separate publish/renew fence latch. It is not folded into
    /// [`WriteLease::valid_with_margin`] (that remains the single deadline
    /// load spec §3.3 requires), but Gate A consults both values. Starts
    /// `true`: this incarnation is fenced by definition until its first
    /// successful renew proves otherwise (fail-closed).
    fenced: AtomicBool,

    graph_id: String,
    lease_url: String,
    token: String,
    renew_ms: u64,
    margin_ms: u64,
    fenced_max_ms: u64,
    publish_timeout_ms: u64,

    /// The newest snapshot the authority has confirmed committed. `u64::MAX`
    /// represents an absent claim-time carrier. A successful Gate-C commit
    /// advances this before pruning; a refused commit deliberately leaves it
    /// unchanged so the authority snapshot remains recoverable on disk.
    authority_last_snap: AtomicU64,

    /// Millis since [`process_epoch`] at the most recent `false -> true`
    /// fencing transition; `0` covers both "never fenced" and "fenced since
    /// construction" (the two coincide at process start, see `WriteLease::new`).
    fenced_since_ms: AtomicU64,
    /// Set at most once: the first positive-terminal-evidence event this
    /// incarnation observes. Never cleared.
    terminal: OnceLock<TerminalReason>,
    /// Fired whenever `fenced` toggles or `terminal` is set, so a consumer
    /// (the gardend main loop) can `.notified().await` instead of polling.
    state_changed: Notify,

    /// `0` = no flush currently running. Set/cleared by the flush funnel via
    /// [`WriteLease::mark_flush_started`]/[`WriteLease::mark_flush_finished`]
    /// so the renew task can report `flush_in_progress_ms` (spec §1.6, the
    /// stuck-flush forfeit).
    flush_started_ms: AtomicU64,
    /// `0` = no successful flush yet this incarnation.
    last_flush_ok_ms: AtomicU64,
    /// Observe-only `dep.state` transition latch. `0` means healthy; nonzero
    /// is an [`ObserveLeaseDegradation`] discriminant.
    observe_degradation: AtomicU8,

    client: reqwest::Client,
    publish_tx: tokio_mpsc::UnboundedSender<PublishRequest>,
}

impl WriteLease {
    fn new(
        config: LeaseConfig,
        client: reqwest::Client,
        publish_tx: tokio_mpsc::UnboundedSender<PublishRequest>,
    ) -> Self {
        Self {
            epoch: config.epoch,
            holder: config.holder,
            mode: config.mode,
            deadline_ms: AtomicU64::new(0),
            fenced: AtomicBool::new(true),
            graph_id: config.graph_id,
            lease_url: config.lease_url,
            token: config.token,
            renew_ms: config.renew_ms,
            margin_ms: config.margin_ms,
            fenced_max_ms: config.fenced_max_ms,
            publish_timeout_ms: config.publish_timeout_ms,
            authority_last_snap: AtomicU64::new(config.last_snap.unwrap_or(u64::MAX)),
            fenced_since_ms: AtomicU64::new(0),
            terminal: OnceLock::new(),
            state_changed: Notify::new(),
            flush_started_ms: AtomicU64::new(0),
            last_flush_ok_ms: AtomicU64::new(0),
            observe_degradation: AtomicU8::new(0),
            client,
            publish_tx,
        }
    }

    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    pub fn holder(&self) -> &str {
        &self.holder
    }

    pub fn graph_id(&self) -> &str {
        &self.graph_id
    }

    pub fn mode(&self) -> LeaseMode {
        self.mode
    }

    pub fn authority_last_snap(&self) -> Option<u64> {
        match self.authority_last_snap.load(Ordering::Acquire) {
            u64::MAX => None,
            seq => Some(seq),
        }
    }

    /// Gate A (spec §3.4): one atomic load, ZERO I/O. `true` iff this
    /// incarnation has renewed successfully and at least `margin_ms`
    /// remains before the deadline that renewal established.
    pub fn valid_with_margin(&self) -> bool {
        let deadline_ms = self.deadline_ms.load(Ordering::Acquire);
        if deadline_ms == 0 {
            return false;
        }
        now_ms().saturating_add(self.margin_ms) < deadline_ms
    }

    /// The explicit publish/renew fence latch consulted by Gate A and health.
    pub fn is_fenced(&self) -> bool {
        self.fenced.load(Ordering::Acquire)
    }

    /// Called by Gate B/C (`cell_durability::flush_inner_with_gate_wait`,
    /// spec §3.4) when a publish CAS fails. Every failure fences immediately.
    /// A `lease_lost`, `lease_forfeit`, or `lease_contended` code additionally
    /// supplies positive terminal evidence, so it is terminal immediately
    /// rather than waiting for a later renew tick. At Gate B, contended means
    /// this token already has a different unresolved `pending_snap`; retrying
    /// with a fresh sequence would otherwise overwrite the repair carrier.
    ///
    /// A transport failure or timeout remains a recoverable fence. An
    /// unrecognized 409 is terminal only when it carries a strictly newer
    /// epoch. Idempotent, zero I/O, and safe on the blocking flush thread.
    pub fn mark_fenced_by_publish_error(&self, error: PublishError) {
        if !self.fenced.swap(true, Ordering::AcqRel) {
            self.fenced_since_ms
                .store(now_ms().max(1), Ordering::Release);
            log::warn!(
                "write lease for {} epoch {} FENCED by a publish refusal (Gate B/C)",
                self.graph_id,
                self.epoch
            );
            self.state_changed.notify_waiters();
        }
        if let PublishError::Refused {
            code,
            current_epoch,
        } = error
        {
            match code {
                PublishRefusalCode::LeaseForfeit => {
                    self.set_terminal(TerminalReason::LeaseForfeit);
                }
                PublishRefusalCode::LeaseLost | PublishRefusalCode::LeaseContended => {
                    self.set_terminal(TerminalReason::LeaseLost { current_epoch });
                }
                PublishRefusalCode::Other => {
                    if current_epoch.is_some_and(|epoch| epoch > self.epoch) {
                        self.set_terminal(TerminalReason::LeaseLost { current_epoch });
                    }
                }
            }
        }
    }

    /// Gate C has stronger evidence than Gate B: `CURRENT` was advanced after
    /// intent succeeded, then the authority rejected the matching commit CAS.
    /// Platform-next currently reports that condition as
    /// `{code:"lease_contended"}` without `current_epoch`; the 409 itself is
    /// therefore the positive authority inconsistency. Make any such refusal
    /// terminal, while a transport/unavailability failure remains recoverable.
    pub fn mark_terminal_by_commit_refusal(&self, error: PublishError) {
        self.mark_fenced_by_publish_error(error);
        if let PublishError::Refused {
            code,
            current_epoch,
        } = error
        {
            let reason = match code {
                PublishRefusalCode::LeaseForfeit => TerminalReason::LeaseForfeit,
                PublishRefusalCode::LeaseLost
                | PublishRefusalCode::LeaseContended
                | PublishRefusalCode::Other => TerminalReason::LeaseLost { current_epoch },
            };
            self.set_terminal(reason);
        }
    }

    pub fn terminal_reason(&self) -> Option<TerminalReason> {
        self.terminal.get().cloned()
    }

    /// Await this to be woken on the next fenced-toggle or terminal event,
    /// instead of polling [`WriteLease::is_fenced`]/[`WriteLease::terminal_reason`].
    pub fn state_changed(&self) -> &Notify {
        &self.state_changed
    }

    fn set_terminal(&self, reason: TerminalReason) {
        if self.mode == LeaseMode::Observe {
            self.testify_observe_degraded(match &reason {
                TerminalReason::LeaseLost { .. } => ObserveLeaseDegradation::RenewLost,
                TerminalReason::LeaseForfeit => ObserveLeaseDegradation::RenewForfeit,
                TerminalReason::FencedTimeout => ObserveLeaseDegradation::RenewUnavailable,
            });
            self.keep_observe_mode_unfenced();
            return;
        }
        // In enforce mode terminal evidence is a strict superset of fencing.
        // Set both before waking the main loop so a concurrent SIGTERM/idle
        // branch cannot observe terminal=true with fenced=false and enter a
        // final flush.
        let newly_fenced = !self.fenced.swap(true, Ordering::AcqRel);
        if newly_fenced {
            self.fenced_since_ms
                .store(now_ms().max(1), Ordering::Release);
        }
        let newly_terminal = self.terminal.set(reason).is_ok();
        // Same-stroke revocation (Lane 2 repair of 2026-08-21): this latch is
        // the discard. Once terminal, Gate A refuses every future flush, the
        // loopback router refuses every request, and Gardend skips the final
        // flush entirely — so every acked-but-unflushed write is stranded at
        // this exact instant. The honest protocol revokes in the same stroke
        // as the transition that strands the ack (a standalone revoke would
        // arrive one step after the dishonest window it closes), and BEFORE
        // waking any waiter, so no observer of the terminal state can run
        // ahead of the revocation receipt. Recorded unconditionally (the
        // watermark is monotone and idempotent): a repeat terminal event with
        // a higher write epoch truthfully extends the discarded range.
        crate::cell_durability::record_fence_discard_revocation(&self.graph_id, self.epoch);
        if newly_terminal || newly_fenced {
            self.state_changed.notify_waiters();
        }
    }

    /// Observe mode is testimony-only: lease trouble must never change
    /// availability. Keep health unfenced and report the would-have terminal
    /// in the renew loop without setting the process terminal latch.
    fn keep_observe_mode_unfenced(&self) {
        if self.fenced.swap(false, Ordering::AcqRel) {
            self.state_changed.notify_waiters();
        }
    }

    /// Emit a `dep.state` degraded event only when observe mode enters this
    /// degradation (or changes from a different degradation). Returns
    /// whether an event was emitted, which keeps transition behavior directly
    /// unit-testable even when the process CaptureWriter is disabled.
    pub(crate) fn testify_observe_degraded(&self, degradation: ObserveLeaseDegradation) -> bool {
        if self.mode != LeaseMode::Observe {
            return false;
        }
        // Monotone event count, bumped on EVERY observe-mode degradation call
        // (deliberately before the same-reason dedupe below): the durable
        // receipt watermark snapshots this around a flush attempt and
        // withholds the awaitDurable claim if any lease doubt was testified
        // mid-attempt — the sealed examination's S2 (2026-08-24): in observe
        // mode a refused Gate B/C proceeds, so the claim must not.
        OBSERVE_DEGRADATION_EVENTS.fetch_add(1, Ordering::AcqRel);
        let current = degradation as u8;
        if self.observe_degradation.swap(current, Ordering::AcqRel) == current {
            return false;
        }
        #[cfg(feature = "headless")]
        crate::capture_event::process_writer()
            .emit_write_lease_degraded(degradation.testimony_code());
        true
    }

    /// Emit `dep.state` recovered once, and only after an actual observe-mode
    /// degraded transition.
    fn testify_observe_recovered(&self) -> bool {
        if self.mode != LeaseMode::Observe
            || self.observe_degradation.swap(0, Ordering::AcqRel) == 0
        {
            return false;
        }
        #[cfg(feature = "headless")]
        crate::capture_event::process_writer().emit_write_lease_recovered();
        true
    }

    /// Apply a terminal renew result according to mode. Returns `true` when
    /// the caller must stop the renew task (enforce) and `false` when it must
    /// keep retrying as testimony only (observe).
    fn apply_renew_terminal(&self, reason: TerminalReason) -> bool {
        if self.mode == LeaseMode::Observe {
            self.testify_observe_degraded(match reason {
                TerminalReason::LeaseLost { .. } => ObserveLeaseDegradation::RenewLost,
                TerminalReason::LeaseForfeit => ObserveLeaseDegradation::RenewForfeit,
                TerminalReason::FencedTimeout => ObserveLeaseDegradation::RenewUnavailable,
            });
            self.keep_observe_mode_unfenced();
            false
        } else {
            self.set_terminal(reason);
            true
        }
    }

    /// Called by the flush funnel when a flush begins, so the renew task can
    /// report an accurate `flush_in_progress_ms` (spec §1.6). Zero I/O.
    pub fn mark_flush_started(&self) {
        self.flush_started_ms
            .store(now_ms().max(1), Ordering::Release);
    }

    /// Called by the flush funnel when a flush ends. `ok` marks whether it
    /// published (used only to advance `last_flush_ok_ms`); a failed/fenced
    /// attempt still clears the in-progress marker.
    pub fn mark_flush_finished(&self, ok: bool) {
        self.flush_started_ms.store(0, Ordering::Release);
        if ok {
            self.last_flush_ok_ms
                .store(now_ms().max(1), Ordering::Release);
        }
    }

    fn flush_in_progress_ms(&self) -> u64 {
        match self.flush_started_ms.load(Ordering::Acquire) {
            0 => 0,
            started => now_ms().saturating_sub(started),
        }
    }

    /// Age of the last successful flush, in ms; `None` if none yet this
    /// incarnation. Sent to the gateway as a duration, never an absolute
    /// timestamp — the relative-time contract applies symmetrically.
    fn last_flush_ok_ms(&self) -> Option<u64> {
        match self.last_flush_ok_ms.load(Ordering::Acquire) {
            0 => None,
            ok => Some(now_ms().saturating_sub(ok)),
        }
    }

    /// Gate B/C's fence (spec §3.3/§3.4). **Synchronous** — safe to call
    /// from the blocking flush thread: sends over an unbounded Tokio sender
    /// (a plain, non-async function call) and blocks only on a
    /// `std::sync::mpsc` reply with a bounded timeout. Never calls `.await`
    /// or `block_on`.
    pub fn publish(&self, seq: u64, phase: PublishPhase) -> Result<(), PublishError> {
        let (reply_tx, reply_rx) = std_mpsc::sync_channel(1);
        if self
            .publish_tx
            .send(PublishRequest {
                seq,
                phase,
                reply: reply_tx,
            })
            .is_err()
        {
            // The async publish worker is gone (process shutting down).
            return Err(PublishError::AuthorityUnavailable);
        }
        match reply_rx.recv_timeout(Duration::from_millis(self.publish_timeout_ms)) {
            Ok(result) => {
                if result.is_ok() && phase == PublishPhase::Commit {
                    self.authority_last_snap.store(seq, Ordering::Release);
                }
                result
            }
            // Timeout or the worker dropped the reply sender without
            // answering — spec §3.3: treated identically to a refusal.
            Err(_) => Err(PublishError::AuthorityUnavailable),
        }
    }

    /// Clean-exit release (spec §1.2). Async — called from the shutdown
    /// path, which already runs on the Tokio runtime (`gardend.rs`'s
    /// `crate::app_runtime::async_runtime::block_on(async move { ... })` shutdown block),
    /// never from the flush thread.
    pub async fn release(&self, reason: &str) -> Result<(), String> {
        #[derive(Serialize)]
        struct Body<'a> {
            graph_id: &'a str,
            holder: &'a str,
            epoch: u64,
            reason: &'a str,
        }
        let body = Body {
            graph_id: &self.graph_id,
            holder: &self.holder,
            epoch: self.epoch,
            reason,
        };
        let response = self
            .client
            .post(format!("{}/release", self.lease_url))
            .bearer_auth(&self.token)
            .json(&body)
            .send()
            .await
            .map_err(|error| format!("lease release request failed: {error}"))?;
        match response.status() {
            StatusCode::NO_CONTENT | StatusCode::OK => Ok(()),
            StatusCode::CONFLICT => {
                // Spec §1.2: "A 409 here means someone already took over —
                // log, never retry."
                log::info!(
                    "lease release for {} epoch {}: 409 — a successor already took over; not retrying",
                    self.graph_id,
                    self.epoch
                );
                Ok(())
            }
            other => Err(format!("lease release returned unexpected status {other}")),
        }
    }
}

// ---------------------------------------------------------------------------
// Wire types (spec §3.1)
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct RenewRequestBody<'a> {
    graph_id: &'a str,
    holder: &'a str,
    epoch: u64,
    flush_in_progress_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_flush_ok_ms: Option<u64>,
}

#[derive(Deserialize)]
struct RenewResponseBody {
    #[allow(dead_code)]
    epoch: u64,
    ttl_remaining_ms: u64,
    effective_in_ms: u64,
    #[allow(dead_code)]
    #[serde(default)]
    state: String,
}

#[derive(Deserialize, Default)]
struct LeaseErrorBody {
    #[serde(default)]
    code: String,
    #[serde(default)]
    current_epoch: Option<u64>,
}

#[derive(Serialize)]
struct PublishRequestBody<'a> {
    graph_id: &'a str,
    holder: &'a str,
    epoch: u64,
    seq: u64,
    phase: &'static str,
}

enum RenewError {
    Transient,
    Terminal(TerminalReason),
}

/// What a successful first renew hands to the boot orchestration (owned by
/// `examples/gardend.rs`, not this module) so it can wait out the effective
/// deadline before hydrating (spec §3.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FirstRenew {
    pub effective_in_ms: u64,
    pub ttl_remaining_ms: u64,
}

async fn http_renew(lease: &WriteLease) -> Result<RenewResponseBody, RenewError> {
    let body = RenewRequestBody {
        graph_id: &lease.graph_id,
        holder: &lease.holder,
        epoch: lease.epoch,
        flush_in_progress_ms: lease.flush_in_progress_ms(),
        last_flush_ok_ms: lease.last_flush_ok_ms(),
    };
    let response = lease
        .client
        .post(format!("{}/renew", lease.lease_url))
        .bearer_auth(&lease.token)
        .json(&body)
        .send()
        .await;
    let response = match response {
        Ok(response) => response,
        Err(error) => {
            log::debug!("lease renew transport error: {error}");
            return Err(RenewError::Transient);
        }
    };
    match response.status() {
        StatusCode::OK => response.json::<RenewResponseBody>().await.map_err(|error| {
            log::warn!("lease renew returned an unparseable 200 body: {error}");
            RenewError::Transient
        }),
        StatusCode::CONFLICT => {
            let body = response.json::<LeaseErrorBody>().await.unwrap_or_default();
            match body.code.as_str() {
                "lease_lost" => Err(RenewError::Terminal(TerminalReason::LeaseLost {
                    current_epoch: body.current_epoch,
                })),
                "lease_forfeit" => Err(RenewError::Terminal(TerminalReason::LeaseForfeit)),
                other => {
                    log::warn!(
                        "lease renew 409 with unrecognized code {other:?}; treating as transient"
                    );
                    Err(RenewError::Transient)
                }
            }
        }
        // 503 authority_unavailable, 401 (matrix #16 — a rotated
        // PN_CELL_TOKEN is treated like case 1: fence, not abort), and any
        // other status are all transient from the cell's point of view.
        _ => Err(RenewError::Transient),
    }
}

async fn http_publish(
    lease: &WriteLease,
    seq: u64,
    phase: PublishPhase,
) -> Result<(), PublishError> {
    let body = PublishRequestBody {
        graph_id: &lease.graph_id,
        holder: &lease.holder,
        epoch: lease.epoch,
        seq,
        phase: phase.as_wire(),
    };
    let response = lease
        .client
        .post(format!("{}/publish", lease.lease_url))
        .bearer_auth(&lease.token)
        .json(&body)
        .send()
        .await;
    match response {
        Ok(response) if response.status() == StatusCode::OK => Ok(()),
        Ok(response) if response.status() == StatusCode::CONFLICT => {
            let body = response.json::<LeaseErrorBody>().await.unwrap_or_default();
            Err(PublishError::Refused {
                code: PublishRefusalCode::from_wire(&body.code),
                current_epoch: body.current_epoch,
            })
        }
        Ok(response) => {
            log::warn!(
                "lease publish({phase:?}, seq={seq}) returned {}",
                response.status()
            );
            Err(PublishError::AuthorityUnavailable)
        }
        Err(error) => {
            log::debug!("lease publish({phase:?}, seq={seq}) transport error: {error}");
            Err(PublishError::AuthorityUnavailable)
        }
    }
}

async fn run_publish_worker(
    lease: &'static WriteLease,
    mut rx: tokio_mpsc::UnboundedReceiver<PublishRequest>,
) {
    while let Some(request) = rx.recv().await {
        let outcome = http_publish(lease, request.seq, request.phase).await;
        // Ignore send failure: the flush thread already gave up (its
        // `recv_timeout` elapsed) and is no longer listening.
        let _ = request.reply.send(outcome);
    }
}

/// The renew loop (spec §1.4/§1.5/§3.2). Runs until a terminal event, then
/// stops — renewing a dead-holder lease afterward would be wasted work, and
/// the process is expected to `exit(4)` shortly.
///
/// `first_renew`, when present, is fulfilled exactly once: `Ok` on the first
/// successful renew (carrying `effective_in_ms` for the boot orchestration to
/// wait out), `Err` if a terminal event arrives before any renew succeeds.
/// If the authority is simply unreachable, the sender is neither fulfilled
/// nor dropped early — this task keeps retrying at the bounded backoff
/// indefinitely (spec §3.2: "authority unreachable at boot in enforce mode ⇒
/// keep retrying, never serve writes"; the caller decides, via its own
/// timeout on the receiver, how long "observe mode proceeds unfenced after a
/// bounded wait" means).
async fn renew_task(
    lease: &'static WriteLease,
    mut first_renew: Option<oneshot::Sender<Result<FirstRenew, TerminalReason>>>,
) {
    let mut observe_failure_reported = false;
    loop {
        let t0 = Instant::now();
        match http_renew(lease).await {
            Ok(response) => {
                observe_failure_reported = false;
                let deadline = deadline_from_send_time(t0, response.ttl_remaining_ms);
                lease.deadline_ms.store(deadline, Ordering::Release);
                if lease.fenced.swap(false, Ordering::AcqRel) {
                    log::warn!(
                        "write lease for {} epoch {} recovered — un-fencing",
                        lease.graph_id,
                        lease.epoch
                    );
                    lease.state_changed.notify_waiters();
                }
                lease.testify_observe_recovered();
                if let Some(sender) = first_renew.take() {
                    let _ = sender.send(Ok(FirstRenew {
                        effective_in_ms: response.effective_in_ms,
                        ttl_remaining_ms: response.ttl_remaining_ms,
                    }));
                }
                let elapsed = t0.elapsed();
                let period = Duration::from_millis(lease.renew_ms);
                if let Some(remaining) = period.checked_sub(elapsed) {
                    tokio::time::sleep(remaining).await;
                }
            }
            Err(RenewError::Terminal(reason)) => {
                if !lease.apply_renew_terminal(reason.clone()) {
                    if !observe_failure_reported {
                        log::warn!(
                            "write lease for {} epoch {} WOULD HAVE terminated in observe mode: \
                             {reason:?} — continuing to serve unfenced",
                            lease.graph_id,
                            lease.epoch
                        );
                        observe_failure_reported = true;
                    }
                    // Observe boot never awaits this receiver, but release the
                    // sender after the first would-have-terminal response.
                    first_renew.take();
                    tokio::time::sleep(Duration::from_millis(lease.renew_ms)).await;
                    continue;
                }
                log::error!(
                    "write lease for {} epoch {} terminated: {reason:?}",
                    lease.graph_id,
                    lease.epoch
                );
                if let Some(sender) = first_renew.take() {
                    let _ = sender.send(Err(reason));
                }
                return;
            }
            Err(RenewError::Transient) => {
                if lease.mode == LeaseMode::Observe {
                    lease.testify_observe_degraded(ObserveLeaseDegradation::RenewUnavailable);
                    lease.keep_observe_mode_unfenced();
                    if !observe_failure_reported && !lease.valid_with_margin() {
                        log::warn!(
                            "write lease for {} epoch {} WOULD HAVE fenced in observe mode: \
                             authority unavailable or margin exhausted — continuing to serve unfenced",
                            lease.graph_id,
                            lease.epoch
                        );
                        observe_failure_reported = true;
                    }
                    tokio::time::sleep(Duration::from_millis(RENEW_RETRY_BACKOFF_MS)).await;
                    continue;
                }
                if !lease.valid_with_margin() {
                    let just_fenced = !lease.fenced.swap(true, Ordering::AcqRel);
                    if just_fenced {
                        lease
                            .fenced_since_ms
                            .store(now_ms().max(1), Ordering::Release);
                        log::warn!(
                            "write lease for {} epoch {} FENCED: margin exhausted with no successor evidence",
                            lease.graph_id,
                            lease.epoch
                        );
                        lease.state_changed.notify_waiters();
                    }
                    let fenced_since = lease.fenced_since_ms.load(Ordering::Acquire);
                    if now_ms().saturating_sub(fenced_since) >= lease.fenced_max_ms {
                        lease.set_terminal(TerminalReason::FencedTimeout);
                        if let Some(sender) = first_renew.take() {
                            let _ = sender.send(Err(TerminalReason::FencedTimeout));
                        }
                        return;
                    }
                }
                tokio::time::sleep(Duration::from_millis(RENEW_RETRY_BACKOFF_MS)).await;
            }
        }
    }
}

fn build_lease_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        // Generous — the publish path's own deadline is the flush thread's
        // `recv_timeout(publish_timeout_ms)`; this is a backstop against a
        // hung TCP connection, not the fence itself.
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|error| format!("build lease client: {error}"))
}

// ---------------------------------------------------------------------------
// Global handle
// ---------------------------------------------------------------------------

static LEASE: OnceLock<Option<WriteLease>> = OnceLock::new();

/// `None` on the desktop app (no `GARDEN_DURABLE_DIR`) and on legacy-unfenced
/// boots (`GARDEN_DURABLE_DIR` set, `GARDEN_DURABLE_EPOCH` absent) — every
/// gate downstream must treat `None` as "no lease exists, proceed unfenced",
/// exactly the desktop app's existing behavior. `None` also before [`init`]
/// has been called at all.
pub fn handle() -> Option<&'static WriteLease> {
    LEASE.get().and_then(|slot| slot.as_ref())
}

/// What [`init`] decided. The boot orchestration (owned outside this module —
/// `examples/gardend.rs`) matches on this to sequence "start renew task →
/// first successful renew → wait out effective deadline → repair → hydrate →
/// serve" (spec §3.2).
pub enum InitOutcome {
    Desktop,
    LegacyUnfenced,
    /// The renew task has been spawned; [`handle`] now returns `Some`.
    /// Await `first_renew` to learn `effective_in_ms` before hydrating.
    Lease {
        mode: LeaseMode,
        first_renew: oneshot::Receiver<Result<FirstRenew, TerminalReason>>,
    },
}

/// Boot-time entry point. Must be called at most once per process (a second
/// call returns `Err` without touching anything). `Err` means refuse-to-boot:
/// the caller should log and exit nonzero without ever serving.
pub async fn init() -> Result<InitOutcome, String> {
    if LEASE.get().is_some() {
        return Err("cell_lease::init() called more than once in this process".to_string());
    }

    match decide_boot(&RawEnv::from_process_env()) {
        BootDecision::Desktop => {
            let _ = LEASE.set(None);
            Ok(InitOutcome::Desktop)
        }
        BootDecision::LegacyUnfenced => {
            log::warn!(
                "{DURABLE_DIR_ENV} is set but {EPOCH_ENV} is absent: this cell is running \
                 UNFENCED against a pre-lease gateway. Every durability gate downstream is a \
                 no-op until the gateway spawning this graph mints {EPOCH_ENV} (U8 spec §3.2)."
            );
            let _ = LEASE.set(None);
            Ok(InitOutcome::LegacyUnfenced)
        }
        BootDecision::RefuseBoot(reason) => Err(reason),
        BootDecision::Lease(config) => {
            let client = build_lease_client()?;
            let (publish_tx, publish_rx) = tokio_mpsc::unbounded_channel();
            let mode = config.mode;
            let lease = WriteLease::new(config, client, publish_tx);
            LEASE.set(Some(lease)).map_err(|_| {
                "cell_lease::init() called more than once in this process".to_string()
            })?;
            let lease_ref = handle().expect("just initialized above");
            if mode == LeaseMode::Observe {
                lease_ref.keep_observe_mode_unfenced();
            }

            let (first_tx, first_rx) = oneshot::channel();
            tokio::spawn(renew_task(lease_ref, Some(first_tx)));
            tokio::spawn(run_publish_worker(lease_ref, publish_rx));

            Ok(InitOutcome::Lease {
                mode,
                first_renew: first_rx,
            })
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_env() -> RawEnv {
        RawEnv::default()
    }

    // ---- None path -------------------------------------------------------

    #[test]
    fn handle_is_none_until_init_is_called() {
        // This module's tests never call the real `init()` (which would
        // touch the process-global `LEASE` for the whole test binary) —
        // every other test below exercises the pure `decide_boot`/`WriteLease`
        // logic directly, so this assertion is safe and deterministic.
        assert!(handle().is_none());
    }

    #[test]
    fn decide_boot_is_desktop_when_durable_dir_is_absent_or_blank() {
        assert_eq!(decide_boot(&empty_env()), BootDecision::Desktop);

        let blank = RawEnv {
            durable_dir: Some("   ".to_string()),
            ..empty_env()
        };
        assert_eq!(decide_boot(&blank), BootDecision::Desktop);
    }

    // ---- mode parsing ------------------------------------------------------

    #[test]
    fn lease_mode_parse_defaults_to_observe_and_is_case_insensitive() {
        assert_eq!(LeaseMode::parse(None), LeaseMode::Observe);
        assert_eq!(LeaseMode::parse(Some("")), LeaseMode::Observe);
        assert_eq!(LeaseMode::parse(Some("observe")), LeaseMode::Observe);
        assert_eq!(LeaseMode::parse(Some("OBSERVE")), LeaseMode::Observe);
        assert_eq!(LeaseMode::parse(Some("enforce")), LeaseMode::Enforce);
        assert_eq!(LeaseMode::parse(Some("ENFORCE")), LeaseMode::Enforce);
        assert_eq!(LeaseMode::parse(Some(" enforce ")), LeaseMode::Enforce);
        // Unrecognized values fall back to the non-refusing mode rather than
        // silently becoming Enforce.
        assert_eq!(LeaseMode::parse(Some("bogus")), LeaseMode::Observe);
    }

    #[test]
    fn owner_scoped_cell_leases_opaque_generation_with_legacy_fallback() {
        assert_eq!(
            lease_graph_identity(Some("c-opaque".into()), Some("notes".into())),
            Some("c-opaque".into())
        );
        assert_eq!(
            lease_graph_identity(None, Some("legacy-graph".into())),
            Some("legacy-graph".into())
        );
        assert_eq!(
            lease_graph_identity(Some("  ".into()), Some("legacy-graph".into())),
            Some("legacy-graph".into())
        );
    }

    // ---- boot decision: legacy-unfenced vs refuse-boot --------------------

    fn durable_only(mode: Option<&str>) -> RawEnv {
        RawEnv {
            durable_dir: Some("/mnt/efs/graph-1".to_string()),
            mode: mode.map(str::to_string),
            ..empty_env()
        }
    }

    #[test]
    fn decide_boot_is_legacy_unfenced_when_epoch_is_absent_in_observe_or_unset_mode() {
        assert_eq!(
            decide_boot(&durable_only(None)),
            BootDecision::LegacyUnfenced
        );
        assert_eq!(
            decide_boot(&durable_only(Some("observe"))),
            BootDecision::LegacyUnfenced
        );
    }

    #[test]
    fn decide_boot_refuses_boot_when_enforce_and_epoch_is_absent() {
        match decide_boot(&durable_only(Some("enforce"))) {
            BootDecision::RefuseBoot(reason) => {
                assert!(reason.contains("enforce"));
                assert!(reason.contains(EPOCH_ENV));
            }
            other => panic!("expected RefuseBoot, got {other:?}"),
        }
    }

    #[test]
    fn decide_boot_refuses_boot_when_enforce_and_epoch_is_not_a_valid_u64() {
        let env = RawEnv {
            epoch: Some("not-a-number".to_string()),
            ..durable_only(Some("enforce"))
        };
        assert!(matches!(decide_boot(&env), BootDecision::RefuseBoot(_)));
    }

    #[test]
    fn decide_boot_falls_back_to_legacy_unfenced_when_observe_and_epoch_is_invalid() {
        let env = RawEnv {
            epoch: Some("not-a-number".to_string()),
            ..durable_only(Some("observe"))
        };
        assert_eq!(decide_boot(&env), BootDecision::LegacyUnfenced);
    }

    // ---- boot decision: refuse-boot on missing wiring, regardless of mode -

    fn lease_ready_env(mode: &str) -> RawEnv {
        RawEnv {
            durable_dir: Some("/mnt/efs/graph-1".to_string()),
            epoch: Some("7".to_string()),
            mode: Some(mode.to_string()),
            graph_id: Some("graph-1".to_string()),
            holder: Some("01ARZ3NDEKTSV4RRFFQ69G5FAV".to_string()),
            lease_url: Some(
                "http://pn-gateway.default.svc.cluster.local/internal/lease".to_string(),
            ),
            token: Some("shared-cell-token".to_string()),
            ..empty_env()
        }
    }

    #[test]
    fn decide_boot_refuses_when_holder_is_missing_even_in_observe_mode() {
        let env = RawEnv {
            holder: None,
            ..lease_ready_env("observe")
        };
        match decide_boot(&env) {
            BootDecision::RefuseBoot(reason) => assert!(reason.contains(MACHINE_RUN_ID_ENV)),
            other => panic!("expected RefuseBoot, got {other:?}"),
        }
    }

    #[test]
    fn decide_boot_refuses_when_graph_id_lease_url_or_token_are_missing() {
        assert!(matches!(
            decide_boot(&RawEnv {
                graph_id: None,
                ..lease_ready_env("enforce")
            }),
            BootDecision::RefuseBoot(_)
        ));
        assert!(matches!(
            decide_boot(&RawEnv {
                lease_url: None,
                ..lease_ready_env("enforce")
            }),
            BootDecision::RefuseBoot(_)
        ));
        assert!(matches!(
            decide_boot(&RawEnv {
                token: None,
                ..lease_ready_env("enforce")
            }),
            BootDecision::RefuseBoot(_)
        ));
    }

    // ---- boot decision: the happy path -------------------------------------

    #[test]
    fn decide_boot_produces_a_lease_config_with_defaulted_timings_when_everything_is_present() {
        match decide_boot(&lease_ready_env("enforce")) {
            BootDecision::Lease(config) => {
                assert_eq!(config.epoch, 7);
                assert_eq!(config.holder, "01ARZ3NDEKTSV4RRFFQ69G5FAV");
                assert_eq!(config.mode, LeaseMode::Enforce);
                assert_eq!(config.graph_id, "graph-1");
                assert_eq!(
                    config.lease_url,
                    "http://pn-gateway.default.svc.cluster.local/internal/lease"
                );
                assert_eq!(config.token, "shared-cell-token");
                assert_eq!(config.renew_ms, DEFAULT_RENEW_MS);
                assert_eq!(config.margin_ms, DEFAULT_MARGIN_MS);
                assert_eq!(config.fenced_max_ms, DEFAULT_FENCED_MAX_MS);
                assert_eq!(config.publish_timeout_ms, DEFAULT_PUBLISH_TIMEOUT_MS);
                assert_eq!(config.last_snap, None);
            }
            other => panic!("expected Lease, got {other:?}"),
        }
    }

    #[test]
    fn decide_boot_carries_the_authority_snapshot_for_safe_pruning() {
        let env = RawEnv {
            last_snap: Some("3178".to_string()),
            ..lease_ready_env("observe")
        };
        match decide_boot(&env) {
            BootDecision::Lease(config) => assert_eq!(config.last_snap, Some(3178)),
            other => panic!("expected Lease, got {other:?}"),
        }
    }

    #[test]
    fn decide_boot_trims_a_trailing_slash_off_the_lease_url() {
        let env = RawEnv {
            lease_url: Some("http://pn-gateway/internal/lease/".to_string()),
            ..lease_ready_env("observe")
        };
        match decide_boot(&env) {
            BootDecision::Lease(config) => {
                assert_eq!(config.lease_url, "http://pn-gateway/internal/lease");
            }
            other => panic!("expected Lease, got {other:?}"),
        }
    }

    #[test]
    fn decide_boot_honors_explicit_timing_overrides() {
        let env = RawEnv {
            renew_ms: Some("1000".to_string()),
            margin_ms: Some("2000".to_string()),
            fenced_max_ms: Some("3000".to_string()),
            publish_timeout_ms: Some("4000".to_string()),
            ..lease_ready_env("observe")
        };
        match decide_boot(&env) {
            BootDecision::Lease(config) => {
                assert_eq!(config.renew_ms, 1000);
                assert_eq!(config.margin_ms, 2000);
                assert_eq!(config.fenced_max_ms, 3000);
                assert_eq!(config.publish_timeout_ms, 4000);
            }
            other => panic!("expected Lease, got {other:?}"),
        }
    }

    // ---- deadline math from t0 ---------------------------------------------

    #[test]
    fn deadline_is_anchored_to_send_time_not_to_whenever_it_is_read() {
        let t0 = Instant::now();
        let ttl_remaining_ms = 20_000;
        let deadline_ms = deadline_from_send_time(t0, ttl_remaining_ms);

        assert_eq!(deadline_ms, instant_to_millis(t0) + ttl_remaining_ms);

        // Simulate RTT/processing delay between send (t0) and when the
        // deadline is actually consulted — the deadline must already have
        // "used up" that elapsed time, which is exactly what anchoring to
        // t0 (not to Instant::now() at storage time) buys.
        std::thread::sleep(Duration::from_millis(15));
        let remaining_after_delay = deadline_ms.saturating_sub(now_ms());
        assert!(
            remaining_after_delay <= ttl_remaining_ms,
            "elapsed time between send and read must already be reflected in the remaining budget"
        );
        assert!(
            remaining_after_delay > ttl_remaining_ms - 1_000,
            "a 15ms sleep must not have consumed anywhere near the full 20s TTL"
        );
    }

    // ---- WriteLease construction + valid_with_margin (zero I/O) ------------

    fn test_lease(margin_ms: u64) -> WriteLease {
        let (publish_tx, _publish_rx) = tokio_mpsc::unbounded_channel();
        WriteLease::new(
            LeaseConfig {
                graph_id: "graph-1".to_string(),
                holder: "01ARZ3NDEKTSV4RRFFQ69G5FAV".to_string(),
                epoch: 7,
                mode: LeaseMode::Enforce,
                lease_url: "http://pn-gateway/internal/lease".to_string(),
                token: "shared-cell-token".to_string(),
                renew_ms: DEFAULT_RENEW_MS,
                margin_ms,
                fenced_max_ms: DEFAULT_FENCED_MAX_MS,
                publish_timeout_ms: DEFAULT_PUBLISH_TIMEOUT_MS,
                last_snap: Some(6),
            },
            reqwest::Client::new(),
            publish_tx,
        )
    }

    #[test]
    fn valid_with_margin_is_false_before_the_first_successful_renew() {
        let lease = test_lease(DEFAULT_MARGIN_MS);
        assert!(!lease.valid_with_margin());
        assert_eq!(lease.authority_last_snap(), Some(6));
        // Fenced-by-default at construction, per the fail-closed comment on
        // the `fenced` field.
        assert!(lease.is_fenced());
    }

    #[test]
    fn valid_with_margin_reflects_the_stored_deadline_against_the_margin() {
        let lease = test_lease(8_000);

        // Deadline far enough in the future that the margin is comfortably
        // satisfied.
        lease
            .deadline_ms
            .store(now_ms() + 20_000, Ordering::Release);
        assert!(lease.valid_with_margin());

        // Deadline inside the margin window: not enough runway left.
        lease.deadline_ms.store(now_ms() + 3_000, Ordering::Release);
        assert!(!lease.valid_with_margin());
    }

    #[test]
    fn epoch_holder_mode_and_graph_id_accessors_report_construction_values() {
        let lease = test_lease(DEFAULT_MARGIN_MS);
        assert_eq!(lease.epoch(), 7);
        assert_eq!(lease.holder(), "01ARZ3NDEKTSV4RRFFQ69G5FAV");
        assert_eq!(lease.graph_id(), "graph-1");
        assert_eq!(lease.mode(), LeaseMode::Enforce);
    }

    #[test]
    fn mark_flush_started_and_finished_track_in_progress_duration() {
        let lease = test_lease(DEFAULT_MARGIN_MS);
        assert_eq!(lease.flush_in_progress_ms(), 0);
        assert_eq!(lease.last_flush_ok_ms(), None);

        lease.mark_flush_started();
        std::thread::sleep(Duration::from_millis(5));
        assert!(lease.flush_in_progress_ms() >= 5);

        lease.mark_flush_finished(true);
        assert_eq!(lease.flush_in_progress_ms(), 0);
        assert_eq!(lease.last_flush_ok_ms(), Some(0));
    }

    #[test]
    fn terminal_reason_is_set_at_most_once_and_fences_enforce_mode() {
        let lease = test_lease(DEFAULT_MARGIN_MS);
        assert_eq!(lease.terminal_reason(), None);
        lease.fenced.store(false, Ordering::Release);
        lease.set_terminal(TerminalReason::LeaseForfeit);
        assert_eq!(lease.terminal_reason(), Some(TerminalReason::LeaseForfeit));
        assert!(lease.is_fenced());
        // A second terminal reason never overwrites the first.
        lease.set_terminal(TerminalReason::FencedTimeout);
        assert_eq!(lease.terminal_reason(), Some(TerminalReason::LeaseForfeit));
    }

    #[test]
    fn publish_refusal_wire_codes_are_preserved() {
        assert_eq!(
            PublishRefusalCode::from_wire("lease_lost"),
            PublishRefusalCode::LeaseLost
        );
        assert_eq!(
            PublishRefusalCode::from_wire("lease_forfeit"),
            PublishRefusalCode::LeaseForfeit
        );
        assert_eq!(
            PublishRefusalCode::from_wire("lease_contended"),
            PublishRefusalCode::LeaseContended
        );
        assert_eq!(
            PublishRefusalCode::from_wire("future_code"),
            PublishRefusalCode::Other
        );
    }

    #[test]
    fn gate_b_publish_refusal_reaction_distinguishes_recoverable_and_terminal_codes() {
        let lease = test_lease(DEFAULT_MARGIN_MS);
        // Fenced-by-default at construction (see `valid_with_margin_is_false_
        // before_the_first_successful_renew`); a transport/unavailability
        // failure must remain recoverable.
        assert!(lease.is_fenced());
        lease.mark_fenced_by_publish_error(PublishError::AuthorityUnavailable);
        assert!(lease.is_fenced());
        assert_eq!(lease.terminal_reason(), None);

        // A current-token pending mismatch is an unresolved bridge, not a
        // retryable slot: restart so boot repair reconciles its original P.
        lease.mark_fenced_by_publish_error(PublishError::Refused {
            code: PublishRefusalCode::LeaseContended,
            current_epoch: None,
        });
        assert!(lease.is_fenced());
        assert_eq!(
            lease.terminal_reason(),
            Some(TerminalReason::LeaseLost {
                current_epoch: None
            })
        );

        // An unrecognized refusal remains recoverable without successor
        // evidence, but a strictly newer epoch is terminal.
        let other = test_lease(DEFAULT_MARGIN_MS);
        other.mark_fenced_by_publish_error(PublishError::Refused {
            code: PublishRefusalCode::Other,
            current_epoch: Some(7),
        });
        assert_eq!(other.terminal_reason(), None);
        other.mark_fenced_by_publish_error(PublishError::Refused {
            code: PublishRefusalCode::Other,
            current_epoch: Some(8),
        });
        assert_eq!(
            other.terminal_reason(),
            Some(TerminalReason::LeaseLost {
                current_epoch: Some(8)
            })
        );
    }

    #[test]
    fn gate_b_same_epoch_lease_lost_and_epochless_forfeit_are_terminal() {
        let lease_lost = test_lease(DEFAULT_MARGIN_MS);
        lease_lost.mark_fenced_by_publish_error(PublishError::Refused {
            code: PublishRefusalCode::LeaseLost,
            current_epoch: Some(7),
        });
        assert_eq!(
            lease_lost.terminal_reason(),
            Some(TerminalReason::LeaseLost {
                current_epoch: Some(7)
            })
        );

        let lease_forfeit = test_lease(DEFAULT_MARGIN_MS);
        lease_forfeit.mark_fenced_by_publish_error(PublishError::Refused {
            code: PublishRefusalCode::LeaseForfeit,
            current_epoch: None,
        });
        assert_eq!(
            lease_forfeit.terminal_reason(),
            Some(TerminalReason::LeaseForfeit)
        );
    }

    #[test]
    fn gate_c_409_without_current_epoch_is_terminal() {
        let lease = test_lease(DEFAULT_MARGIN_MS);
        lease.mark_terminal_by_commit_refusal(PublishError::Refused {
            code: PublishRefusalCode::LeaseContended,
            current_epoch: None,
        });
        assert!(lease.is_fenced());
        assert_eq!(
            lease.terminal_reason(),
            Some(TerminalReason::LeaseLost {
                current_epoch: None
            })
        );
    }

    #[test]
    fn observe_mode_lease_trouble_never_sets_terminal_or_fenced_health() {
        let mut lease = test_lease(DEFAULT_MARGIN_MS);
        lease.mode = LeaseMode::Observe;
        assert!(!lease.apply_renew_terminal(TerminalReason::LeaseLost {
            current_epoch: None,
        }));
        assert!(!lease.is_fenced());
        assert_eq!(lease.terminal_reason(), None);

        // The renew loop uses this same mode-gated operation for both 409
        // would-have-terminal and transient margin exhaustion.
        lease.keep_observe_mode_unfenced();
        assert!(!lease.is_fenced());
        assert_eq!(lease.terminal_reason(), None);
    }

    #[test]
    fn observe_dependency_testimony_is_transition_deduplicated() {
        let mut lease = test_lease(DEFAULT_MARGIN_MS);
        lease.mode = LeaseMode::Observe;

        assert!(lease.testify_observe_degraded(ObserveLeaseDegradation::GateAMargin));
        assert!(!lease.testify_observe_degraded(ObserveLeaseDegradation::GateAMargin));
        assert!(lease.testify_observe_degraded(ObserveLeaseDegradation::GateBPublishIntent));
        assert!(lease.testify_observe_recovered());
        assert!(!lease.testify_observe_recovered());
    }

    #[test]
    fn set_terminal_is_intrinsically_testimony_only_in_observe_mode() {
        let mut lease = test_lease(DEFAULT_MARGIN_MS);
        lease.mode = LeaseMode::Observe;
        lease.fenced.store(false, Ordering::Release);

        lease.set_terminal(TerminalReason::LeaseForfeit);
        assert_eq!(lease.terminal_reason(), None);
        assert!(!lease.is_fenced());
        assert_eq!(
            lease.observe_degradation.load(Ordering::Acquire),
            ObserveLeaseDegradation::RenewForfeit as u8
        );
    }

    // ---- Lane 2 same-stroke revocation (2026-08-21 repair) ----------------

    /// The formal model's repaired transition, in production: when the lease
    /// fence strands acked-but-unflushed writes (enforce-mode terminal
    /// evidence — after which Gate A refuses every flush, the router every
    /// request, and Gardend the final flush), the revocation is recorded in
    /// the SAME stroke as the latch, so a durability inquiry for the
    /// stranded range answers "revoked" rather than pending-forever. Without
    /// the repair this test is the incident verbatim ("the caller is never
    /// told"): production had no revocation path at all — the model's
    /// `Defect::FenceDiscardsAckedWrites` / documented absence — and the
    /// watermark stays behind the acked write forever.
    #[test]
    fn enforce_terminal_revokes_stranded_acked_range_in_the_same_stroke() {
        use crate::cell_durability::{
            advance_write_epoch_for_tests, durability_watermarks, durably_revoked_epoch,
        };
        use crate::document_mcp_write_payloads::{write_durability_verdict, WriteDurability};

        // A Garden write completed locally and its caller holds a "pending"
        // durability ack (the write epoch is above any resolved coverage).
        let acked_epoch = advance_write_epoch_for_tests();

        // The real production stroke — Gate C's authority 409 funnels
        // through `mark_terminal_by_commit_refusal` → `set_terminal`
        // (enforce). No other action follows: the same stroke must pay.
        let lease = test_lease(DEFAULT_MARGIN_MS);
        lease.mark_terminal_by_commit_refusal(PublishError::Refused {
            code: PublishRefusalCode::LeaseContended,
            current_epoch: None,
        });
        assert!(lease.terminal_reason().is_some());

        // The revocation receipt covers the acked write. Monotone-robust
        // against sibling tests (the watermark is a process global that
        // only ever advances).
        assert!(
            durably_revoked_epoch() >= acked_epoch,
            "the terminal fence must record the revocation in the same \
             stroke as the discard (revoked {}, acked {acked_epoch})",
            durably_revoked_epoch(),
        );

        // And the verdict surface answers "revoked" for the stranded write.
        // Guarded because the covering watermarks are process globals shared
        // with sibling tests: a concurrently completing real flush can
        // legitimately cover this epoch, in which case Published/Pending is
        // the truthful answer (same discipline as
        // `cell_durability::tests::published_flush_advances_durable_receipt_watermark`).
        let watermarks = durability_watermarks();
        if watermarks.resolved < acked_epoch && watermarks.plane_visible < acked_epoch {
            assert_eq!(
                write_durability_verdict(true, true, watermarks, acked_epoch),
                WriteDurability::Revoked,
                "a discarded pending write must answer revoked, not pending-forever"
            );
        }
    }

    /// Renew-loop terminal evidence (lease lost with a successor standing)
    /// pays the same-stroke revocation exactly like the Gate B/C paths — all
    /// terminal strands funnel through `set_terminal`.
    #[test]
    fn renew_terminal_evidence_also_records_the_revocation() {
        use crate::cell_durability::{advance_write_epoch_for_tests, durably_revoked_epoch};

        let acked_epoch = advance_write_epoch_for_tests();
        let lease = test_lease(DEFAULT_MARGIN_MS);
        assert!(lease.apply_renew_terminal(TerminalReason::LeaseLost {
            current_epoch: Some(9),
        }));
        assert!(
            durably_revoked_epoch() >= acked_epoch,
            "renew-loop terminal evidence must revoke the stranded range in \
             the same stroke (revoked {}, acked {acked_epoch})",
            durably_revoked_epoch(),
        );
    }

    #[test]
    fn publish_returns_authority_unavailable_when_no_worker_is_listening() {
        // No `run_publish_worker` task is running for this lease (the
        // receiver end was dropped in `test_lease`'s construction) — the
        // send itself fails, proving `publish()` never blocks forever
        // waiting on a reply that can never arrive, and never touches
        // `block_on`/`.await` to find that out.
        let lease = test_lease(DEFAULT_MARGIN_MS);
        assert_eq!(
            lease.publish(1, PublishPhase::Intent),
            Err(PublishError::AuthorityUnavailable)
        );
    }
}
