//! Bounded native CaptureEvent writer for a headless Garden cell.
//!
//! This is deliberately not a logging facade. Callers can emit only the closed
//! lifecycle variants below; application diagnostics continue to use `log` on
//! stderr while one dedicated thread owns the stdout NDJSON testimony lane.

use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _};
use serde::{Deserialize, Serialize};
use std::{
    io::{self, Write},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{self, SyncSender, TrySendError},
        Arc, Condvar, Mutex, OnceLock, Weak,
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use ulid::Ulid;

const DEFAULT_QUEUE_CAPACITY: usize = 256;
const DEFAULT_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(60);
const MAX_LINE_BYTES: usize = 4096;
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
// Packed layout for `WriterInner::admission` (see its field comment): the
// single high bit marks the writer permanently closed, the remaining 63
// bits are the in-flight-emit count. 63 bits of headroom means the count
// can never overflow into the closed bit in practice.
const ADMISSION_CLOSED_BIT: u64 = 1 << 63;
const ADMISSION_COUNT_MASK: u64 = ADMISSION_CLOSED_BIT - 1;
const CAPTURE_ENABLED_ENV: &str = "SOPHIA_OBSERVATORY_CAPTURE_ENABLED";
const CONTRACT_BUNDLE_SHA256_ENV: &str = "SOPHIA_OBSERVATORY_CONTRACT_BUNDLE_SHA256";
const EXPECTED_CONTRACT_BUNDLE_SHA256: &str =
    "c60d81fe4b431c3a88bd2450189a16da97627cc6ba36c4b127942d593a6c2bbb";

#[derive(Clone, Debug, PartialEq, Eq)]
struct WriterIdentity {
    graph_id: String,
    machine_id: String,
    machine_run_id: String,
    witness: String,
}

impl WriterIdentity {
    fn configured(
        enabled: Option<&str>,
        contract_bundle_sha256: Option<&str>,
        graph_id: Option<&str>,
        cell_id: Option<&str>,
        machine_id: Option<&str>,
        machine_run_id: Option<&str>,
    ) -> Result<Option<Self>, String> {
        let enabled = match enabled.map(str::trim) {
            None | Some("") | Some("0") => false,
            Some(value) if value.eq_ignore_ascii_case("false") => false,
            Some("1") => true,
            Some(value) if value.eq_ignore_ascii_case("true") => true,
            Some(_) => {
                return Err(format!("{CAPTURE_ENABLED_ENV} must be true/false or 1/0"));
            }
        };
        if !enabled {
            return Ok(None);
        }

        let required = |name: &str, value: Option<&str>| -> Result<String, String> {
            value
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string)
                .ok_or_else(|| format!("{CAPTURE_ENABLED_ENV}=true requires {name}"))
        };
        let contract_bundle_sha256 = required(CONTRACT_BUNDLE_SHA256_ENV, contract_bundle_sha256)?;
        if contract_bundle_sha256 != EXPECTED_CONTRACT_BUNDLE_SHA256 {
            return Err(format!(
                "{CONTRACT_BUNDLE_SHA256_ENV} must equal the ratified CaptureEvent v0.3 bundle {EXPECTED_CONTRACT_BUNDLE_SHA256}"
            ));
        }

        let public_graph_id = required("GARDEN_CELL_GRAPH_ID", graph_id)?;
        // CaptureEvent v0.1 binds `machine_id` to its event `graph_id`.
        // Owner-scoped cells therefore testify with the opaque physical cell
        // identity used by the gateway's spawn/reap events, while the public
        // owner/local-id tuple remains the separate Garden graph boundary.
        // Legacy cells have no GARDEN_CELL_ID and retain cell:{graph_id}.
        let graph_id = cell_id
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or(&public_graph_id)
            .to_string();
        if !valid_graph_id(&graph_id) {
            let source = if cell_id.is_some() {
                "GARDEN_CELL_ID"
            } else {
                "GARDEN_CELL_GRAPH_ID"
            };
            return Err(format!("{source} is not a valid graph identifier"));
        }

        let expected_machine = format!("cell:{graph_id}");
        let machine_id = required("GARDEN_CELL_MACHINE_ID", machine_id)?;
        if machine_id != expected_machine || !valid_opaque_id(&machine_id) {
            return Err(format!(
                "GARDEN_CELL_MACHINE_ID must equal {expected_machine:?}"
            ));
        }

        let machine_run_id = required("GARDEN_CELL_MACHINE_RUN_ID", machine_run_id)?;
        if !valid_ulid(&machine_run_id) {
            return Err("GARDEN_CELL_MACHINE_RUN_ID is not a valid ULID".into());
        }

        Ok(Some(Self {
            witness: format!("cell:{graph_id}/{machine_run_id}"),
            graph_id,
            machine_id,
            machine_run_id,
        }))
    }

    fn from_env() -> Result<Option<Self>, String> {
        let enabled = std::env::var(CAPTURE_ENABLED_ENV).ok();
        let contract_bundle_sha256 = std::env::var(CONTRACT_BUNDLE_SHA256_ENV).ok();
        let graph_id = std::env::var("GARDEN_CELL_GRAPH_ID").ok();
        let cell_id = std::env::var("GARDEN_CELL_ID").ok();
        let machine_id = std::env::var("GARDEN_CELL_MACHINE_ID").ok();
        let machine_run_id = std::env::var("GARDEN_CELL_MACHINE_RUN_ID").ok();
        Self::configured(
            enabled.as_deref(),
            contract_bundle_sha256.as_deref(),
            graph_id.as_deref(),
            cell_id.as_deref(),
            machine_id.as_deref(),
            machine_run_id.as_deref(),
        )
    }

    #[cfg(test)]
    fn fixed(graph_id: &str, machine_run_id: &str) -> Self {
        Self {
            graph_id: graph_id.into(),
            machine_id: format!("cell:{graph_id}"),
            machine_run_id: machine_run_id.into(),
            witness: format!("cell:{graph_id}/{machine_run_id}"),
        }
    }
}

fn valid_graph_id(value: &str) -> bool {
    value.len() <= 128
        && value
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphanumeric)
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

fn valid_opaque_id(value: &str) -> bool {
    value.len() <= 128
        && value
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphanumeric)
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b':' | b'-'))
}

fn valid_ulid(value: &str) -> bool {
    value.len() == 26 && Ulid::from_string(value).is_ok()
}

/// Matches the ratified contract's `subject` string domain:
/// `^[A-Za-z0-9][A-Za-z0-9._:@-]{0,255}$`. Also used as the fallback branch
/// of the `principal` domain (below), which is textually identical except
/// for the two additional closed alternatives (`anon`, `service:...`).
fn valid_subject(value: &str) -> bool {
    value.len() <= 256
        && value
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphanumeric)
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b':' | b'@' | b'-')
        })
}

/// Matches the ratified contract's `principal` string domain:
/// `^(anon|service:[a-z][a-z0-9-]{0,63}|[A-Za-z0-9][A-Za-z0-9._:@-]{0,255})$`.
fn valid_principal(value: &str) -> bool {
    if value == "anon" {
        return true;
    }
    if let Some(rest) = value.strip_prefix("service:") {
        return rest.len() <= 64
            && rest.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
            && rest
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-');
    }
    valid_subject(value)
}

/// Matches the ratified contract's `tool_name` string domain:
/// `^[A-Za-z][A-Za-z0-9_.-]{0,127}$`.
fn valid_tool_name(value: &str) -> bool {
    value.len() <= 128
        && value
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphabetic)
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'-'))
}

trait Clock: Send + Sync + 'static {
    fn now_ms(&self) -> u64;
}

struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            .min(u64::MAX as u128) as u64
    }
}

trait IdGenerator: Send + Sync + 'static {
    fn next_ulid(&self) -> String;
}

struct SystemIds;

impl IdGenerator for SystemIds {
    fn next_ulid(&self) -> String {
        Ulid::new().to_string()
    }
}

trait LineSink: Send + 'static {
    fn write_line(&mut self, line: &[u8]) -> io::Result<()>;
}

struct StdoutSink;

impl LineSink for StdoutSink {
    fn write_line(&mut self, line: &[u8]) -> io::Result<()> {
        let stdout = io::stdout();
        let mut stdout = stdout.lock();
        stdout.write_all(line)?;
        stdout.flush()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ActivityCounts {
    pub in_flight_requests: u64,
    pub open_websockets: u64,
    pub background_jobs: u64,
    pub background_leases: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BootMode {
    Fresh,
    Restored,
    WarmProfile,
    NoDurablePlane,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BootFailureStage {
    Hydrate,
    Setup,
    /// U8 cross-process write lease (spec §3.2): the boot-time lease dance
    /// (`cell_lease::init`, waiting for the first successful renew, or
    /// `cell_durability::boot_repair`) refused to let this boot continue.
    LeaseBoot,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FlushMode {
    Periodic,
    Final,
    PostImport,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    Idle,
    Signal,
    StartupFailure,
    RuntimeFailure,
    /// U8 cross-process write lease (spec §3.5): positive terminal evidence
    /// (`409 lease_lost`/`lease_forfeit`, or the `GARDEN_LEASE_FENCED_MAX_MS`
    /// cap with no successor evidence at all) forced an immediate exit with
    /// the final durable flush skipped entirely.
    LeaseTerminal,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    DurableHydrateFailed,
    CoreSetupFailed,
    InvalidRuntimeMode,
    FlushGateTimeout,
    DurableFlushFailed,
    FinalFlushFailed,
    /// A headless durable flush was refused by write-lease Gate A, B, or C.
    /// This is an explicit failed `cell.flush` testimony, never an ordinary
    /// `ok` event with `published=false`.
    LeaseFenced,
    /// `cell_lease::init` refused to boot (misconfiguration — spec §3.2's
    /// tripwires), or `cell_durability::boot_repair` refused to boot (an
    /// escaped publish's `LAST_SNAP` is missing on disk — spec §3.6).
    LeaseBootRefused,
    /// The write lease was never renewed successfully before a terminal
    /// event (or the renew task ended without ever answering) during the
    /// boot-time lease dance — this incarnation never serves.
    LeaseUnavailableAtBoot,
    /// `409 lease_lost` at runtime: a successor already holds a higher
    /// epoch.
    LeaseLost,
    /// `409 lease_forfeit` at runtime: the gateway judged this flush wedged
    /// past `PN_LEASE_STUCK_FLUSH_MS` (spec §1.6).
    LeaseForfeit,
    /// `GARDEN_LEASE_FENCED_MAX_MS` elapsed with no successor evidence at
    /// all (spec §3.5's 300s cap).
    LeaseFencedTimeout,
}

/// Closed dependency-state detail identifiers used by write-lease observe
/// mode. These contain no graph content and serialize into the existing v0.1
/// `dep.state` payload's `detail_code` field.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DependencyDetailCode {
    LeaseGateAMargin,
    LeaseGateBPublishIntent,
    LeaseGateCPublishCommit,
    LeaseRenewLost,
    LeaseRenewForfeit,
    LeaseRenewUnavailable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum DependencyState {
    Degraded,
    Recovered,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SuccessfulFinalFlush {
    Completed,
    NotConfigured,
    /// The write lease was FENCED (margin exhausted, no successor evidence
    /// yet — the recoverable state, not a terminal one): the final flush
    /// would have been a zombie write, so it was skipped by design. The
    /// overall shutdown still succeeds (spec §3.5 does not treat a merely
    /// fenced-but-not-terminal lease as a failure).
    SkippedFenced,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FailedFinalFlush {
    Completed,
    Failed,
    NotConfigured,
    /// Positive lease-terminal evidence (spec §3.5): the final flush was
    /// never attempted at all, by design — it would be the zombie write
    /// this protocol exists to forbid.
    SkippedFenced,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct SnapshotId(String);

impl SnapshotId {
    pub fn new(value: impl Into<String>) -> Result<Self, String> {
        let value = value.into();
        if valid_opaque_id(&value) {
            Ok(Self(value))
        } else {
            Err("snapshot ID is not a valid opaque identifier".into())
        }
    }

    pub fn from_sequence(sequence: u64) -> Self {
        Self(format!("snap-{sequence:06}"))
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FlushMeasurements {
    pub snapshot_id: Option<SnapshotId>,
    pub bytes: u64,
    pub files_copied: u64,
    pub files_linked: u64,
    pub stores_backed_up: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WriterCounters {
    pub next_sequence: u64,
    pub emitted_total: u64,
    pub dropped_total: u64,
    pub written_total: u64,
    pub write_failed_total: u64,
}

// --- Phase-1 interaction spine -------------------------------------------
//
// `CaptureIdentity` + `emit_interaction` are the cell half of the shared
// Observatory Capture spine (see
// plans/observatory-capture-phase1-spine-spec-20260718.md). The gateway is
// the identity *authority* (it reads Cognito/service-token claims); the cell
// never classifies a caller itself. It only re-normalizes the gateway's
// already-computed identity — forwarded across the loopback boundary as the
// trusted `x-sophia-capture-identity` header (see `loopback_capture_identity`
// for the HTTP-boundary half: header parsing + the trust-boundary gate) —
// into this closed, contract-validated shape, then makes it available to
// call sites with zero further plumbing.

/// The ratified contract's `client_class` enum domain, verbatim
/// (`obs.golden.json` `enum_domains.client_class`). The cell does not derive
/// this — the gateway does, from auth — the cell only validates+forwards it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClientClass {
    Browser,
    Api,
    Agent,
    Service,
    Probe,
    Anon,
}

impl ClientClass {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "browser" => Some(Self::Browser),
            "api" => Some(Self::Api),
            "agent" => Some(Self::Agent),
            "service" => Some(Self::Service),
            "probe" => Some(Self::Probe),
            "anon" => Some(Self::Anon),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Browser => "browser",
            Self::Api => "api",
            Self::Agent => "agent",
            Self::Service => "service",
            Self::Probe => "probe",
            Self::Anon => "anon",
        }
    }
}

/// The caller identity an interaction event testifies about: who acted
/// (`principal`), how their client was classified (`client_class`), and
/// whether they were acting as a delegate for a human (`on_behalf_of`).
///
/// Constructed only through [`CaptureIdentity::new`] or
/// [`CaptureIdentity::from_forwarded_header`], both of which enforce the
/// ratified contract's `principal`/`subject` domains and the
/// service-principal delegation invariant
/// (`validator.py`: `on_behalf_of` requires a `service:`-prefixed principal
/// and must differ from it) — so an event built from a `CaptureIdentity` can
/// never carry a contract-invalid identity shape.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CaptureIdentity {
    principal: String,
    client_class: ClientClass,
    on_behalf_of: Option<String>,
}

impl CaptureIdentity {
    pub fn new(
        principal: impl Into<String>,
        client_class: ClientClass,
        on_behalf_of: Option<String>,
    ) -> Result<Self, String> {
        let principal = principal.into();
        if !valid_principal(&principal) {
            return Err("CaptureIdentity principal is not contract-valid".into());
        }
        if let Some(subject) = &on_behalf_of {
            if !valid_subject(subject) {
                return Err("CaptureIdentity on_behalf_of is not contract-valid".into());
            }
            if !principal.starts_with("service:") {
                return Err("CaptureIdentity on_behalf_of requires a service: principal".into());
            }
            if subject == &principal {
                return Err("CaptureIdentity on_behalf_of must differ from principal".into());
            }
        }
        Ok(Self {
            principal,
            client_class,
            on_behalf_of,
        })
    }

    pub fn principal(&self) -> &str {
        &self.principal
    }

    pub fn client_class(&self) -> ClientClass {
        self.client_class
    }

    pub fn on_behalf_of(&self) -> Option<&str> {
        self.on_behalf_of.as_deref()
    }

    /// Normalizes the gateway-forwarded `x-sophia-capture-identity` header
    /// value: `base64(json({principal, client_class, on_behalf_of}))`.
    ///
    /// This function is pure (no I/O, no trust decision) — it only proves
    /// the bytes decode to a contract-valid identity. Callers at the HTTP
    /// boundary (`loopback_capture_identity::resolve_forwarded_identity`)
    /// are responsible for calling it ONLY after the loopback token gate has
    /// already authenticated the request; that is the actual trust boundary.
    pub fn from_forwarded_header(raw_header_value: &str) -> Result<Self, String> {
        let decoded = BASE64_STANDARD
            .decode(raw_header_value.trim())
            .map_err(|error| format!("x-sophia-capture-identity is not valid base64: {error}"))?;
        let raw: ForwardedCaptureIdentity = serde_json::from_slice(&decoded)
            .map_err(|error| format!("x-sophia-capture-identity is not valid JSON: {error}"))?;
        let client_class = ClientClass::parse(&raw.client_class).ok_or_else(|| {
            format!(
                "x-sophia-capture-identity unknown client_class: {}",
                raw.client_class
            )
        })?;
        Self::new(raw.principal, client_class, raw.on_behalf_of)
    }
}

#[derive(Deserialize)]
struct ForwardedCaptureIdentity {
    principal: String,
    client_class: String,
    #[serde(default)]
    on_behalf_of: Option<String>,
}

/// The ratified contract's `outcome` enum domain, verbatim. Note this is
/// four-valued (`ok | denied | error | timeout`) — the spec's conceptual
/// five-way call-site outcome (which also names `not_found`) collapses
/// `not_found` onto `error` at the wire, since the ratified contract has no
/// `not_found` member. Callers map 404-shaped results to `Error`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Ok,
    Denied,
    Error,
    Timeout,
}

impl Outcome {
    fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Denied => "denied",
            Self::Error => "error",
            Self::Timeout => "timeout",
        }
    }
}

/// The graph.* interaction kinds this cell's `emit_interaction` accepts.
///
/// `Create`/`Delete` exist here for taxonomy completeness (Artifact 3's "one
/// signature, reused everywhere" — the conceptual `InteractionKind` is
/// shared vocabulary across gateway and cell) and because their payload
/// shapes are already contract-defined. BUT: per `obs.golden.json`,
/// `graph.create`/`graph.delete` have `authorized_witnesses: ["gateway"]`
/// ONLY — this cell's witness is always `cell:...`, which is never in that
/// list. `emit_interaction` therefore refuses (drops) any attempt to emit
/// `Create`/`Delete` from this cell — see `Capture::is_authorized_and_well_formed`.
/// Phase 2 must never wire a cell call-site to these two variants; they are
/// the gateway's own `graph.create`/`graph.delete` emits
/// (`routes.rs:402-414,:557-566` per the spec), not this cell's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GraphInteractionKind {
    Read,
    Write,
    Create,
    Delete,
}

impl GraphInteractionKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Read => "graph.read",
            Self::Write => "graph.write",
            Self::Create => "graph.create",
            Self::Delete => "graph.delete",
        }
    }

    /// `graph.create`/`graph.delete` are not authorized for a `cell:` witness.
    fn cell_may_witness(self) -> bool {
        matches!(self, Self::Read | Self::Write)
    }
}

/// A validated `tool_name` — a REGISTERED MCP tool identifier (member of the
/// embedded `mcp_tool_catalog.json` allowlist, e.g. `"sparql_query"`), never
/// free text.
///
/// The contract's `tool_name` string domain
/// (`^[A-Za-z][A-Za-z0-9_.-]{0,127}$`) is a shape check only — it happily
/// matches identifier-shaped content too (e.g. `"MyPrivateDiagnosis"`),
/// which would satisfy "aggregate-only" in type but not in fact. `new`
/// therefore requires BOTH the shape AND closed-catalog membership: a value
/// that isn't a real, registered tool name is rejected outright, not merely
/// discouraged.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct ToolName(String);

impl ToolName {
    pub fn new(value: impl Into<String>) -> Result<Self, String> {
        let value = value.into();
        if !valid_tool_name(&value) {
            return Err("tool_name is not a valid opaque tool identifier".into());
        }
        if !crate::mcp_tool_registry::is_known_mcp_tool_name(&value) {
            return Err(format!("tool_name is not a registered MCP tool: {value}"));
        }
        Ok(Self(value))
    }
}

/// `graph.read`'s `op` enum (`obs.golden.json` `kinds.graph.read.payload`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GraphReadOp {
    SparqlSelect,
    SparqlConstruct,
    DocumentRead,
    McpTool,
}

/// `graph.write`'s `op` enum domain (`enum_domains.graph_operation`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GraphWriteOp {
    Crdt,
    YjsUpdate,
    SparqlUpdate,
    EmporiumIngest,
    McpTool,
}

/// Shared `stage` enum used by `graph.create`/`graph.delete`'s optional field.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GraphLifecycleStage {
    Accepted,
    Completed,
    Failed,
}

/// `graph.delete`'s required `durable_disposition` enum domain
/// (`enum_domains.delete_disposition`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DeleteDisposition {
    Trashed,
    Purged,
}

/// `graph.read`'s payload — aggregate-only by construction: an operation
/// class, an optional validated tool identifier, and an optional result
/// cardinality. There is no free-form string field a query or its results
/// could be smuggled through.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct GraphReadPayload {
    pub op: GraphReadOp,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<ToolName>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result_count: Option<u64>,
}

/// `graph.write`'s payload — aggregate-only: an operation class, an optional
/// validated tool identifier, and triple/byte counts. Same non-negotiable:
/// no content field.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct GraphWritePayload {
    pub op: GraphWriteOp,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<ToolName>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub triples_added: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub triples_removed: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bytes: Option<u64>,
}

/// `graph.create`'s payload. Present for taxonomy completeness; the cell
/// never actually emits this kind (see [`GraphInteractionKind::cell_may_witness`]).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Default)]
pub struct GraphCreatePayload {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stage: Option<GraphLifecycleStage>,
}

/// `graph.delete`'s payload. Present for taxonomy completeness; the cell
/// never actually emits this kind (see [`GraphInteractionKind::cell_may_witness`]).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct GraphDeletePayload {
    pub durable_disposition: DeleteDisposition,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stage: Option<GraphLifecycleStage>,
}

/// The `emit_interaction` payload union — one variant per
/// [`GraphInteractionKind`]. Self-describing: `emit_interaction` checks the
/// caller's `kind` argument against the variant actually supplied here and
/// drops the event on any mismatch, so `kind` and `payload` can never
/// disagree in emitted testimony.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub enum GraphInteractionPayload {
    Read(GraphReadPayload),
    Write(GraphWritePayload),
    Create(GraphCreatePayload),
    Delete(GraphDeletePayload),
}

impl GraphInteractionPayload {
    fn kind(&self) -> GraphInteractionKind {
        match self {
            Self::Read(_) => GraphInteractionKind::Read,
            Self::Write(_) => GraphInteractionKind::Write,
            Self::Create(_) => GraphInteractionKind::Create,
            Self::Delete(_) => GraphInteractionKind::Delete,
        }
    }
}

struct WriterInner {
    identity: WriterIdentity,
    clock: Arc<dyn Clock>,
    ids: Arc<dyn IdGenerator>,
    // Plain, never-locked field: `SyncSender::try_send` takes `&self`, and a
    // `SyncSender` is already `Clone + Send + Sync` — internally safe for
    // concurrent senders. Wrapping it in a `Mutex<Option<_>>` (the previous
    // design) forced every single `emit()` call to contend on a lock just to
    // clone out a value that didn't need cloning at all; the hot path now
    // touches this field with zero synchronization.
    sender: SyncSender<Vec<u8>>,
    // Lock-free shutdown barrier, packed into ONE atomic word (high bit =
    // permanently-closed flag, low 63 bits = in-flight-emit count; see
    // `ADMISSION_CLOSED_BIT` / `ADMISSION_COUNT_MASK`).
    //
    // An earlier version of this barrier used TWO independent atomics — a
    // `closed: AtomicBool` and a separate `in_flight_emits: AtomicU64` —
    // and was not linearizable: Acquire/Release on two unrelated atomic
    // objects gives no single modification order between "set closed" and
    // "increment in-flight", so a classic store-buffering interleaving was
    // possible. `shutdown()` could load `in_flight_emits` and see a stale
    // 0 (that emitter's increment not yet visible to it) and declare
    // "drained", while *concurrently* that emitter had already loaded
    // `closed == false` (`shutdown()`'s store not yet visible to it) and
    // went on to `try_send` — an emit accepted after shutdown believed it
    // had drained. Two atomics simply have no shared order to race on.
    //
    // Packing both signals into one atomic fixes this: every modification
    // to a single atomic object has one real total order (guaranteed by
    // the memory model regardless of ordering used), so "am I allowed to
    // send" and "I am now counted as in-flight" become one atomic
    // transition — see `InFlightGuard::admit`. Either an admission CAS
    // lands before `shutdown()`'s `fetch_or(ADMISSION_CLOSED_BIT)` in that
    // order (in which case `shutdown()`'s poll loop, reading the same
    // word, is guaranteed to observe the incremented count before it can
    // report "drained"), or it lands after (in which case the CAS itself
    // observes the closed bit already set and rejects). There is no third,
    // lost-emit outcome. See
    // `admission_race_between_a_paused_emit_and_concurrent_shutdown_is_always_safe`
    // and `many_concurrent_emitters_racing_shutdown_never_lose_or_double_count`.
    admission: AtomicU64,
    next_sequence: AtomicU64,
    emitted_total: AtomicU64,
    dropped_total: AtomicU64,
    written_total: AtomicU64,
    write_failed_total: AtomicU64,
    heartbeat_stop: AtomicBool,
    heartbeat_wake: (Mutex<()>, Condvar),
}

/// RAII in-flight registration for one admitted `emit()` call. Obtained
/// only via `admit`, which performs the actual admission decision;
/// decrementing on every drop path (normal return, early return, or panic
/// unwind) means `emit()` never has to remember to decrement at each of
/// its several early-return sites once it holds one.
struct InFlightGuard<'a>(&'a WriterInner);

impl<'a> InFlightGuard<'a> {
    /// Attempts to admit one `emit()` call. A CAS loop against the packed
    /// `admission` word makes "may this call proceed" and "is it now
    /// counted as in-flight" a single atomic transition, closing the
    /// lost-emit-after-drain race described on the `admission` field:
    /// load the word; if the closed bit is already set, refuse admission
    /// (the caller must drop the attempt, uncounted — a closed writer was
    /// never in flight and `shutdown()` need not wait for it); otherwise
    /// CAS to increment the count while the closed bit is still clear.
    /// Only a successful CAS — one that itself observed the closed bit
    /// clear — returns a guard and lets the caller proceed to `try_send`.
    fn admit(inner: &'a WriterInner) -> Option<Self> {
        let mut state = inner.admission.load(Ordering::Acquire);
        loop {
            if state & ADMISSION_CLOSED_BIT != 0 {
                return None;
            }
            // Real (non-debug) boundary guard: at `count ==
            // ADMISSION_COUNT_MASK`, `state + 1` would carry out of the
            // count's 63 bits and into `ADMISSION_CLOSED_BIT` above it,
            // corrupting both packed signals at once — spuriously
            // "closing" the writer with a bogus zero count, which a later
            // guard drop would then `fetch_sub`, clearing that bogus
            // closed bit again. 2^63-1 simultaneous in-flight emits is
            // physically impossible, but a release build must not rely on
            // that impossibility for correctness — refuse admission here,
            // exactly like a full channel: no guard, caller accounts it as
            // dropped. One extra comparison on the CAS path, no
            // measurable hot-path cost. See
            // `admission_count_saturation_is_refused_without_flipping_closed`.
            if state & ADMISSION_COUNT_MASK == ADMISSION_COUNT_MASK {
                return None;
            }
            match inner.admission.compare_exchange_weak(
                state,
                state + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Some(InFlightGuard(inner)),
                Err(actual) => state = actual,
            }
        }
    }
}

impl Drop for InFlightGuard<'_> {
    fn drop(&mut self) {
        // Decrement only — never touch the closed bit. This is safe
        // plain-integer arithmetic on the shared word: every decrement is
        // paired with the increment `admit` performed to hand out this
        // guard, so the count portion can never underflow past zero (and
        // therefore never borrows into the closed bit above it).
        self.0.admission.fetch_sub(1, Ordering::AcqRel);
    }
}

impl Drop for WriterInner {
    fn drop(&mut self) {
        self.heartbeat_stop.store(true, Ordering::Release);
        self.heartbeat_wake.1.notify_all();
        self.admission
            .fetch_or(ADMISSION_CLOSED_BIT, Ordering::Release);
    }
}

#[derive(Clone)]
pub struct CaptureWriter {
    inner: Option<Arc<WriterInner>>,
}

impl CaptureWriter {
    fn production() -> Result<Self, String> {
        let Some(identity) = WriterIdentity::from_env()? else {
            return Ok(Self { inner: None });
        };
        let queue_capacity = std::env::var("GARDEN_CAPTURE_QUEUE_CAPACITY")
            .ok()
            .and_then(|value| value.trim().parse::<usize>().ok())
            .filter(|capacity| *capacity > 0)
            .unwrap_or(DEFAULT_QUEUE_CAPACITY);
        let heartbeat_interval = std::env::var("GARDEN_CAPTURE_HEARTBEAT_SECONDS")
            .ok()
            .and_then(|value| value.trim().parse::<u64>().ok())
            .filter(|seconds| *seconds > 0)
            .map(Duration::from_secs)
            .unwrap_or(DEFAULT_HEARTBEAT_INTERVAL);
        Ok(Self::with_parts(
            identity,
            queue_capacity,
            Arc::new(SystemClock),
            Arc::new(SystemIds),
            Box::new(StdoutSink),
            Some(heartbeat_interval),
        ))
    }

    fn with_parts(
        identity: WriterIdentity,
        queue_capacity: usize,
        clock: Arc<dyn Clock>,
        ids: Arc<dyn IdGenerator>,
        mut sink: Box<dyn LineSink>,
        heartbeat_interval: Option<Duration>,
    ) -> Self {
        let (sender, receiver) = mpsc::sync_channel(queue_capacity.max(1));
        let inner = Arc::new(WriterInner {
            identity,
            clock,
            ids,
            sender,
            admission: AtomicU64::new(0),
            next_sequence: AtomicU64::new(0),
            emitted_total: AtomicU64::new(0),
            dropped_total: AtomicU64::new(0),
            written_total: AtomicU64::new(0),
            write_failed_total: AtomicU64::new(0),
            heartbeat_stop: AtomicBool::new(false),
            heartbeat_wake: (Mutex::new(()), Condvar::new()),
        });

        let counters = Arc::downgrade(&inner);
        thread::Builder::new()
            .name("garden-capture-stdout".into())
            .spawn(move || {
                while let Ok(line) = receiver.recv() {
                    let result = sink.write_line(&line);
                    let Some(inner) = counters.upgrade() else {
                        break;
                    };
                    if result.is_ok() {
                        inner.written_total.fetch_add(1, Ordering::Relaxed);
                    } else {
                        inner.write_failed_total.fetch_add(1, Ordering::Relaxed);
                    }
                }
            })
            .expect("spawn bounded CaptureEvent stdout writer");

        let writer = Self { inner: Some(inner) };
        if let Some(interval) = heartbeat_interval {
            writer.spawn_heartbeat(interval);
        }
        writer
    }

    fn spawn_heartbeat(&self, interval: Duration) {
        let Some(inner) = &self.inner else {
            return;
        };
        let weak: Weak<WriterInner> = Arc::downgrade(inner);
        thread::Builder::new()
            .name("garden-capture-heartbeat".into())
            .spawn(move || loop {
                let Some(inner) = weak.upgrade() else {
                    return;
                };
                let guard = inner
                    .heartbeat_wake
                    .0
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                let (_guard, timed) = inner
                    .heartbeat_wake
                    .1
                    .wait_timeout_while(guard, interval, |_| {
                        !inner.heartbeat_stop.load(Ordering::Acquire)
                    })
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                if inner.heartbeat_stop.load(Ordering::Acquire) {
                    return;
                }
                drop(_guard);
                drop(inner);
                if timed.timed_out() {
                    let Some(inner) = weak.upgrade() else {
                        return;
                    };
                    CaptureWriter { inner: Some(inner) }.emit_heartbeat();
                }
            })
            .expect("spawn CaptureEvent heartbeat timer");
    }

    pub fn enabled(&self) -> bool {
        self.inner.is_some()
    }

    pub fn counters(&self) -> WriterCounters {
        let Some(inner) = &self.inner else {
            return WriterCounters::default();
        };
        WriterCounters {
            next_sequence: inner.next_sequence.load(Ordering::Acquire),
            emitted_total: inner.emitted_total.load(Ordering::Acquire),
            dropped_total: inner.dropped_total.load(Ordering::Acquire),
            written_total: inner.written_total.load(Ordering::Acquire),
            write_failed_total: inner.write_failed_total.load(Ordering::Acquire),
        }
    }

    pub fn emit_boot_ready(
        &self,
        mode: BootMode,
        duration: Duration,
        hydrate_duration: Duration,
        setup_duration: Duration,
        snapshot_id: Option<SnapshotId>,
        hydrated_bytes: Option<u64>,
    ) {
        self.emit(Capture::BootReady {
            mode,
            duration_ms: millis(duration),
            hydrate_ms: millis(hydrate_duration),
            setup_ms: millis(setup_duration),
            snapshot_id,
            hydrated_bytes,
        });
    }

    pub fn emit_boot_failed(
        &self,
        stage: BootFailureStage,
        error_code: ErrorCode,
        duration: Duration,
    ) {
        self.emit(Capture::BootFailed {
            stage,
            error_code,
            duration_ms: millis(duration),
        });
    }

    pub fn emit_flush_succeeded(
        &self,
        mode: FlushMode,
        published: bool,
        duration: Duration,
        measurements: FlushMeasurements,
    ) {
        self.emit(Capture::FlushSucceeded {
            mode,
            published,
            duration_ms: millis(duration),
            measurements,
        });
    }

    pub fn emit_flush_failed(&self, mode: FlushMode, error_code: ErrorCode, duration: Duration) {
        self.emit(Capture::FlushFailed {
            mode,
            error_code,
            duration_ms: millis(duration),
        });
    }

    /// Emit the existing v0.1 `dep.state` degraded shape for the write-lease
    /// dependency. Transition de-duplication belongs to the lease state
    /// machine, which knows whether a report is genuinely new.
    pub fn emit_write_lease_degraded(&self, detail_code: DependencyDetailCode) {
        self.emit(Capture::DependencyState {
            state: DependencyState::Degraded,
            detail_code: Some(detail_code),
        });
    }

    /// Emit recovery of a previously degraded write-lease dependency.
    pub fn emit_write_lease_recovered(&self) {
        self.emit(Capture::DependencyState {
            state: DependencyState::Recovered,
            detail_code: None,
        });
    }

    pub fn emit_draining(
        &self,
        reason: StopReason,
        counts: ActivityCounts,
        idle_duration: Option<Duration>,
    ) {
        self.emit(Capture::Draining {
            reason,
            counts,
            idle_ms: idle_duration.map(millis),
        });
    }

    pub fn emit_quiesced(
        &self,
        reason: StopReason,
        timed_out: bool,
        counts: ActivityCounts,
        duration: Duration,
    ) {
        self.emit(Capture::Quiesced {
            reason,
            timed_out,
            counts,
            duration_ms: millis(duration),
        });
    }

    pub fn emit_terminal_succeeded(&self, reason: StopReason, final_flush: SuccessfulFinalFlush) {
        self.emit(Capture::TerminalSucceeded {
            reason,
            final_flush,
        });
    }

    pub fn emit_terminal_failed(
        &self,
        reason: StopReason,
        final_flush: FailedFinalFlush,
        error_code: ErrorCode,
    ) {
        self.emit(Capture::TerminalFailed {
            reason,
            final_flush,
            error_code,
        });
    }

    /// Emits a `graph.*` interaction event: the cell half of the shared
    /// Observatory Capture spine (`emit_interaction`, Artifact 1 of
    /// plans/observatory-capture-phase1-spine-spec-20260718.md). Uses this
    /// writer's existing bounded channel — no new queue, no new thread.
    ///
    /// `graph_id` overrides the writer's own bound graph when supplied;
    /// pass `None` to default to it (this cell serves exactly one graph, so
    /// `None` is the expected call shape and costs the caller nothing).
    ///
    /// Aggregate-only is enforced by `payload`'s type: every variant is a
    /// closed enum/newtype/integer — there is no field a query, a document,
    /// or any other content could be smuggled through.
    ///
    /// Two conditions make this a no-op (dropped, like any other invalid
    /// shape — see `Capture::is_authorized_and_well_formed`):
    /// - `kind` names a different interaction than `payload` actually is;
    /// - `kind` is `Create`/`Delete`, which this cell is never an authorized
    ///   witness for (`obs.golden.json` `authorized_witnesses: ["gateway"]`
    ///   only) — Phase 2 must not call this with those kinds.
    pub fn emit_interaction(
        &self,
        kind: GraphInteractionKind,
        identity: &CaptureIdentity,
        graph_id: Option<&str>,
        outcome: Outcome,
        duration_ms: Option<u64>,
        payload: GraphInteractionPayload,
    ) {
        self.emit(Capture::GraphInteraction {
            kind,
            identity: identity.clone(),
            graph_id: graph_id.map(str::to_string),
            outcome,
            duration_ms,
            payload,
        });
    }

    fn emit_heartbeat(&self) {
        let Some(inner) = &self.inner else {
            return;
        };
        self.emit(Capture::Heartbeat {
            emitted_total: inner.emitted_total.load(Ordering::Acquire),
            dropped_total: inner.dropped_total.load(Ordering::Acquire),
        });
    }

    fn emit(&self, capture: Capture) {
        let Some(inner) = &self.inner else {
            return;
        };
        // Sequence is the first operation by contract. Every later failure
        // consumes it and therefore becomes observable as a gap.
        let sequence = inner.next_sequence.fetch_add(1, Ordering::Relaxed);
        let event_id = inner.ids.next_ulid();
        let timestamp = inner.clock.now_ms();
        // A writer that has begun shutdown (or any other invalid-shape
        // condition) still mints an id/timestamp and consumes the sequence
        // number reserved above — every later failure is a visible gap —
        // but is spared the strictly more expensive JSON serialization +
        // queue send below. The admission check itself (below) is what
        // decides the shutdown case; it is intentionally not folded into
        // this boolean condition because it is a CAS loop, not a plain
        // load — see `InFlightGuard::admit`.
        if !valid_ulid(&event_id)
            || timestamp == 0
            || timestamp > MAX_SAFE_INTEGER
            || sequence > MAX_SAFE_INTEGER
            || !capture.integers_are_safe()
            || !capture.is_authorized_and_well_formed()
        {
            inner.dropped_total.fetch_add(1, Ordering::Relaxed);
            return;
        }

        // Admission: the single atomic transition that decides whether
        // this call may proceed to `try_send`, and — in the same CAS —
        // registers it as in-flight so `shutdown()` cannot linearize past
        // it until the guard drops (send completed, or this call takes an
        // early return below). See the `admission` field comment for the
        // race this replaces.
        let Some(_in_flight) = InFlightGuard::admit(inner) else {
            inner.dropped_total.fetch_add(1, Ordering::Relaxed);
            return;
        };

        let event = capture.into_event(&inner.identity, event_id, timestamp, sequence);
        let mut line = match serde_json::to_vec(&event) {
            Ok(line) => line,
            Err(_) => {
                inner.dropped_total.fetch_add(1, Ordering::Relaxed);
                return;
            }
        };
        line.push(b'\n');
        if line.len() > MAX_LINE_BYTES {
            inner.dropped_total.fetch_add(1, Ordering::Relaxed);
            return;
        }

        // No lock, no clone: `SyncSender::try_send` takes `&self` and the
        // channel's own internal synchronization already makes concurrent
        // senders safe. This is the entire hot-path fix — every previous
        // `emit()` call serialized on a `Mutex` here even though nothing
        // about a bounded mpsc channel requires one.
        match inner.sender.try_send(line) {
            Ok(()) => {
                inner.emitted_total.fetch_add(1, Ordering::Relaxed);
            }
            Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => {
                inner.dropped_total.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// Stop timer emission, mark the writer closed, and wait only up to
    /// `budget` for already-accepted lines. A stalled stdout consumer can
    /// never extend the cell's shutdown beyond this explicit budget.
    pub fn shutdown(&self, budget: Duration) -> bool {
        let Some(inner) = &self.inner else {
            return true;
        };
        inner.heartbeat_stop.store(true, Ordering::Release);
        inner.heartbeat_wake.1.notify_all();
        // Sets the closed bit in the same packed word `emit()`'s admission
        // CAS reads. Because both operations act on one atomic object, they
        // share a single real modification order: any admission that lands
        // after this in that order will observe the bit and refuse; any
        // that already landed before it is already reflected in the count
        // this loop polls below. No admission can be invisible to both.
        inner
            .admission
            .fetch_or(ADMISSION_CLOSED_BIT, Ordering::AcqRel);
        let deadline = Instant::now() + budget;
        loop {
            // The drain counters can only be trusted once no admitted
            // `emit()` call is still in flight. Once the count portion of
            // `admission` is observed at zero, every admitted call has
            // already either sent (and its `emitted_total` increment
            // already happened-before this read, via the guard's `AcqRel`
            // decrement) or is a contradiction (an admission implies the
            // closed bit was clear at CAS time, but the call still runs to
            // completion and drops its guard regardless) — no future call
            // can ever be admitted again, since the closed bit, once set,
            // is never cleared. This is what makes it safe to check the
            // drain counters immediately after.
            let in_flight = inner.admission.load(Ordering::Acquire) & ADMISSION_COUNT_MASK;
            if in_flight == 0 {
                let counters = self.counters();
                if counters.written_total + counters.write_failed_total >= counters.emitted_total {
                    return true;
                }
            }
            if Instant::now() >= deadline {
                return false;
            }
            thread::sleep(Duration::from_millis(1));
        }
    }
}

static PROCESS_WRITER: OnceLock<CaptureWriter> = OnceLock::new();

pub fn install_process_writer() -> Result<CaptureWriter, String> {
    if PROCESS_WRITER.get().is_some() {
        return Err("CaptureEvent process writer is already installed".into());
    }
    let writer = CaptureWriter::production()?;
    PROCESS_WRITER
        .set(writer.clone())
        .map_err(|_| "CaptureEvent process writer is already installed".to_string())?;
    Ok(writer)
}

/// The process-wide `CaptureWriter` singleton installed by
/// `install_process_writer` at boot, or an inert no-op writer if capture is
/// disabled or not yet installed. This is how request-handler code (Phase 2's
/// `emit_interaction` call sites) reaches the same bounded writer/channel
/// `gardend`'s boot/flush/stop lifecycle already uses, without threading a
/// `CaptureWriter` through every handler's arguments.
pub fn process_writer() -> CaptureWriter {
    PROCESS_WRITER
        .get()
        .cloned()
        .unwrap_or(CaptureWriter { inner: None })
}

fn millis(duration: Duration) -> u64 {
    duration.as_millis().min(u64::MAX as u128) as u64
}

enum Capture {
    Heartbeat {
        emitted_total: u64,
        dropped_total: u64,
    },
    BootReady {
        mode: BootMode,
        duration_ms: u64,
        hydrate_ms: u64,
        setup_ms: u64,
        snapshot_id: Option<SnapshotId>,
        hydrated_bytes: Option<u64>,
    },
    BootFailed {
        stage: BootFailureStage,
        error_code: ErrorCode,
        duration_ms: u64,
    },
    FlushSucceeded {
        mode: FlushMode,
        published: bool,
        duration_ms: u64,
        measurements: FlushMeasurements,
    },
    FlushFailed {
        mode: FlushMode,
        error_code: ErrorCode,
        duration_ms: u64,
    },
    DependencyState {
        state: DependencyState,
        detail_code: Option<DependencyDetailCode>,
    },
    Draining {
        reason: StopReason,
        counts: ActivityCounts,
        idle_ms: Option<u64>,
    },
    Quiesced {
        reason: StopReason,
        timed_out: bool,
        counts: ActivityCounts,
        duration_ms: u64,
    },
    TerminalSucceeded {
        reason: StopReason,
        final_flush: SuccessfulFinalFlush,
    },
    TerminalFailed {
        reason: StopReason,
        final_flush: FailedFinalFlush,
        error_code: ErrorCode,
    },
    GraphInteraction {
        kind: GraphInteractionKind,
        identity: CaptureIdentity,
        graph_id: Option<String>,
        outcome: Outcome,
        duration_ms: Option<u64>,
        payload: GraphInteractionPayload,
    },
}

impl Capture {
    fn integers_are_safe(&self) -> bool {
        let safe = |value: u64| value <= MAX_SAFE_INTEGER;
        let safe_optional = |value: Option<u64>| value.is_none_or(safe);
        let safe_counts = |counts: &ActivityCounts| {
            safe(counts.in_flight_requests)
                && safe(counts.open_websockets)
                && safe(counts.background_jobs)
                && safe(counts.background_leases)
        };
        match self {
            Self::Heartbeat {
                emitted_total,
                dropped_total,
            } => safe(*emitted_total) && safe(*dropped_total),
            Self::BootReady {
                duration_ms,
                hydrate_ms,
                setup_ms,
                hydrated_bytes,
                ..
            } => {
                safe(*duration_ms)
                    && safe(*hydrate_ms)
                    && safe(*setup_ms)
                    && safe_optional(*hydrated_bytes)
            }
            Self::BootFailed { duration_ms, .. } | Self::FlushFailed { duration_ms, .. } => {
                safe(*duration_ms)
            }
            Self::FlushSucceeded {
                duration_ms,
                measurements,
                ..
            } => {
                safe(*duration_ms)
                    && safe(measurements.bytes)
                    && safe(measurements.files_copied)
                    && safe(measurements.files_linked)
                    && safe(measurements.stores_backed_up)
            }
            Self::Draining {
                counts, idle_ms, ..
            } => safe_counts(counts) && safe_optional(*idle_ms),
            Self::Quiesced {
                counts,
                duration_ms,
                ..
            } => safe_counts(counts) && safe(*duration_ms),
            Self::TerminalSucceeded { .. }
            | Self::TerminalFailed { .. }
            | Self::DependencyState { .. } => true,
            Self::GraphInteraction {
                duration_ms,
                payload,
                ..
            } => {
                safe_optional(*duration_ms)
                    && match payload {
                        GraphInteractionPayload::Read(read) => safe_optional(read.result_count),
                        GraphInteractionPayload::Write(write) => {
                            safe_optional(write.triples_added)
                                && safe_optional(write.triples_removed)
                                && safe_optional(write.bytes)
                        }
                        GraphInteractionPayload::Create(_) | GraphInteractionPayload::Delete(_) => {
                            true
                        }
                    }
            }
        }
    }

    /// Guards the three properties `integers_are_safe` does not: that `kind`
    /// and `payload` agree (so a caller cannot label a `graph.write` as a
    /// `graph.read` or vice versa); that this witness (always `cell:...`) is
    /// contract-authorized for the kind being emitted (`graph.create`/
    /// `graph.delete` are gateway-only witnesses per `obs.golden.json`, so a
    /// cell attempt to emit either is refused here exactly like any other
    /// invalid shape, before it ever reaches the queue); and that an
    /// explicit `graph_id` override matches the contract's `graph_id` string
    /// domain (`^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$`, identical to this
    /// file's existing `valid_graph_id` — the same rule `GARDEN_CELL_GRAPH_ID`
    /// is already held to), so a caller passing a malformed graph_id can
    /// never enqueue a validator-rejected event. `None` is always fine — it
    /// defers to the writer's own bound graph (see `into_event`). Every
    /// other `Capture` variant is this cell's own lifecycle testimony
    /// (`cell.*`/`obs.heartbeat`), which the cell is always authorized to
    /// witness and which never carries a caller-supplied `graph_id`.
    fn is_authorized_and_well_formed(&self) -> bool {
        match self {
            Self::GraphInteraction {
                kind,
                payload,
                graph_id,
                ..
            } => {
                *kind == payload.kind()
                    && kind.cell_may_witness()
                    && graph_id.as_deref().is_none_or(valid_graph_id)
            }
            _ => true,
        }
    }

    fn into_event(
        self,
        identity: &WriterIdentity,
        event_id: String,
        timestamp: u64,
        sequence: u64,
    ) -> CaptureEvent {
        let (kind, outcome, duration_ms, payload, caller, graph_id_override) = match self {
            Self::Heartbeat {
                emitted_total,
                dropped_total,
            } => (
                "obs.heartbeat",
                "ok",
                None,
                Payload::Heartbeat(HeartbeatPayload {
                    emitted_total,
                    dropped_total,
                }),
                None,
                None,
            ),
            Self::BootReady {
                mode,
                duration_ms,
                hydrate_ms,
                setup_ms,
                snapshot_id,
                hydrated_bytes,
            } => (
                "cell.boot",
                "ok",
                Some(duration_ms),
                Payload::BootReady(BootReadyPayload {
                    stage: "ready",
                    machine_id: identity.machine_id.clone(),
                    machine_run_id: identity.machine_run_id.clone(),
                    boot_mode: mode,
                    snapshot_id,
                    hydrated_bytes,
                    hydrate_ms,
                    setup_ms,
                }),
                None,
                None,
            ),
            Self::BootFailed {
                stage,
                error_code,
                duration_ms,
            } => (
                "cell.boot",
                "error",
                Some(duration_ms),
                Payload::BootFailed(BootFailedPayload {
                    stage: "failed",
                    machine_id: identity.machine_id.clone(),
                    machine_run_id: identity.machine_run_id.clone(),
                    failure_stage: stage,
                    error_code,
                }),
                None,
                None,
            ),
            Self::FlushSucceeded {
                mode,
                published,
                duration_ms,
                measurements,
            } => (
                "cell.flush",
                "ok",
                Some(duration_ms),
                Payload::Flush(FlushPayload {
                    machine_id: identity.machine_id.clone(),
                    machine_run_id: identity.machine_run_id.clone(),
                    mode,
                    published,
                    snapshot_id: measurements.snapshot_id,
                    bytes: Some(measurements.bytes),
                    files_copied: Some(measurements.files_copied),
                    files_linked: Some(measurements.files_linked),
                    stores_backed_up: Some(measurements.stores_backed_up),
                    error_code: None,
                }),
                None,
                None,
            ),
            Self::FlushFailed {
                mode,
                error_code,
                duration_ms,
            } => (
                "cell.flush",
                "error",
                Some(duration_ms),
                Payload::Flush(FlushPayload {
                    machine_id: identity.machine_id.clone(),
                    machine_run_id: identity.machine_run_id.clone(),
                    mode,
                    published: false,
                    snapshot_id: None,
                    bytes: None,
                    files_copied: None,
                    files_linked: None,
                    stores_backed_up: None,
                    error_code: Some(error_code),
                }),
                None,
                None,
            ),
            Self::DependencyState { state, detail_code } => (
                "dep.state",
                "ok",
                None,
                Payload::DependencyState(DependencyStatePayload {
                    dependency: "write_lease",
                    state,
                    detail_code,
                }),
                None,
                None,
            ),
            Self::Draining {
                reason,
                counts,
                idle_ms,
            } => (
                "cell.stop",
                "ok",
                None,
                Payload::Draining(DrainingPayload::new(identity, reason, counts, idle_ms)),
                None,
                None,
            ),
            Self::Quiesced {
                reason,
                timed_out,
                counts,
                duration_ms,
            } => (
                "cell.stop",
                if timed_out { "timeout" } else { "ok" },
                Some(duration_ms),
                Payload::Quiesced(QuiescedPayload::new(identity, reason, timed_out, counts)),
                None,
                None,
            ),
            Self::TerminalSucceeded {
                reason,
                final_flush,
            } => (
                "cell.stop",
                "ok",
                None,
                Payload::Terminal(TerminalPayload {
                    stage: "terminal",
                    machine_id: identity.machine_id.clone(),
                    machine_run_id: identity.machine_run_id.clone(),
                    reason,
                    terminal_state: "succeeded",
                    final_flush: TerminalFinalFlush::Successful(final_flush),
                    error_code: None,
                }),
                None,
                None,
            ),
            Self::TerminalFailed {
                reason,
                final_flush,
                error_code,
            } => (
                "cell.stop",
                "error",
                None,
                Payload::Terminal(TerminalPayload {
                    stage: "terminal",
                    machine_id: identity.machine_id.clone(),
                    machine_run_id: identity.machine_run_id.clone(),
                    reason,
                    terminal_state: "failed",
                    final_flush: TerminalFinalFlush::Failed(final_flush),
                    error_code: Some(error_code),
                }),
                None,
                None,
            ),
            Self::GraphInteraction {
                kind,
                identity: caller,
                graph_id,
                outcome,
                duration_ms,
                payload,
            } => (
                kind.as_str(),
                outcome.as_str(),
                duration_ms,
                match payload {
                    GraphInteractionPayload::Read(read) => Payload::GraphRead(read),
                    GraphInteractionPayload::Write(write) => Payload::GraphWrite(write),
                    GraphInteractionPayload::Create(create) => Payload::GraphCreate(create),
                    GraphInteractionPayload::Delete(delete) => Payload::GraphDelete(delete),
                },
                Some(caller),
                graph_id,
            ),
        };
        let (principal, client_class, on_behalf_of) = match caller {
            Some(caller) => (
                caller.principal,
                caller.client_class.as_str(),
                caller.on_behalf_of,
            ),
            None => ("service:gardend".to_string(), "service", None),
        };
        let graph_id = graph_id_override.unwrap_or_else(|| identity.graph_id.clone());
        CaptureEvent {
            v: 1,
            event_id,
            ts: timestamp,
            witness: identity.witness.clone(),
            kind,
            principal,
            on_behalf_of,
            client_class,
            graph_id,
            outcome,
            duration_ms,
            seq: sequence,
            payload,
        }
    }
}

#[derive(Serialize)]
struct CaptureEvent {
    v: u8,
    event_id: String,
    ts: u64,
    witness: String,
    kind: &'static str,
    principal: String,
    // Canonical key order (obs.golden.json `canonical_key_order`) places
    // `on_behalf_of` immediately after `principal` and before `client_class`
    // — field declaration order here IS wire order (no custom key sort), so
    // this position is load-bearing, not cosmetic.
    #[serde(skip_serializing_if = "Option::is_none")]
    on_behalf_of: Option<String>,
    client_class: &'static str,
    graph_id: String,
    outcome: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    duration_ms: Option<u64>,
    seq: u64,
    payload: Payload,
}

#[derive(Serialize)]
#[serde(untagged)]
enum Payload {
    Heartbeat(HeartbeatPayload),
    BootReady(BootReadyPayload),
    BootFailed(BootFailedPayload),
    Flush(FlushPayload),
    DependencyState(DependencyStatePayload),
    Draining(DrainingPayload),
    Quiesced(QuiescedPayload),
    Terminal(TerminalPayload),
    GraphRead(GraphReadPayload),
    GraphWrite(GraphWritePayload),
    GraphCreate(GraphCreatePayload),
    GraphDelete(GraphDeletePayload),
}

#[derive(Serialize)]
struct HeartbeatPayload {
    emitted_total: u64,
    dropped_total: u64,
}

#[derive(Serialize)]
struct BootReadyPayload {
    stage: &'static str,
    machine_id: String,
    machine_run_id: String,
    boot_mode: BootMode,
    #[serde(skip_serializing_if = "Option::is_none")]
    snapshot_id: Option<SnapshotId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    hydrated_bytes: Option<u64>,
    hydrate_ms: u64,
    setup_ms: u64,
}

#[derive(Serialize)]
struct BootFailedPayload {
    stage: &'static str,
    machine_id: String,
    machine_run_id: String,
    failure_stage: BootFailureStage,
    error_code: ErrorCode,
}

#[derive(Serialize)]
struct FlushPayload {
    machine_id: String,
    machine_run_id: String,
    mode: FlushMode,
    published: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    snapshot_id: Option<SnapshotId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    files_copied: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    files_linked: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stores_backed_up: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error_code: Option<ErrorCode>,
}

#[derive(Serialize)]
struct DependencyStatePayload {
    dependency: &'static str,
    state: DependencyState,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail_code: Option<DependencyDetailCode>,
}

#[derive(Serialize)]
struct DrainingPayload {
    stage: &'static str,
    machine_id: String,
    machine_run_id: String,
    reason: StopReason,
    in_flight_requests: u64,
    open_websockets: u64,
    background_jobs: u64,
    background_leases: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    idle_ms: Option<u64>,
}

impl DrainingPayload {
    fn new(
        identity: &WriterIdentity,
        reason: StopReason,
        counts: ActivityCounts,
        idle_ms: Option<u64>,
    ) -> Self {
        Self {
            stage: "draining",
            machine_id: identity.machine_id.clone(),
            machine_run_id: identity.machine_run_id.clone(),
            reason,
            in_flight_requests: counts.in_flight_requests,
            open_websockets: counts.open_websockets,
            background_jobs: counts.background_jobs,
            background_leases: counts.background_leases,
            idle_ms,
        }
    }
}

#[derive(Serialize)]
struct QuiescedPayload {
    stage: &'static str,
    machine_id: String,
    machine_run_id: String,
    reason: StopReason,
    timed_out: bool,
    in_flight_requests: u64,
    open_websockets: u64,
    background_jobs: u64,
    background_leases: u64,
}

impl QuiescedPayload {
    fn new(
        identity: &WriterIdentity,
        reason: StopReason,
        timed_out: bool,
        counts: ActivityCounts,
    ) -> Self {
        Self {
            stage: "quiesced",
            machine_id: identity.machine_id.clone(),
            machine_run_id: identity.machine_run_id.clone(),
            reason,
            timed_out,
            in_flight_requests: counts.in_flight_requests,
            open_websockets: counts.open_websockets,
            background_jobs: counts.background_jobs,
            background_leases: counts.background_leases,
        }
    }
}

#[derive(Serialize)]
#[serde(untagged)]
enum TerminalFinalFlush {
    Successful(SuccessfulFinalFlush),
    Failed(FailedFinalFlush),
}

#[derive(Serialize)]
struct TerminalPayload {
    stage: &'static str,
    machine_id: String,
    machine_run_id: String,
    reason: StopReason,
    terminal_state: &'static str,
    final_flush: TerminalFinalFlush,
    #[serde(skip_serializing_if = "Option::is_none")]
    error_code: Option<ErrorCode>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{collections::VecDeque, sync::mpsc as test_mpsc};

    #[derive(Clone)]
    struct FixedClock {
        values: Arc<Mutex<VecDeque<u64>>>,
    }

    impl Clock for FixedClock {
        fn now_ms(&self) -> u64 {
            self.values.lock().unwrap().pop_front().unwrap()
        }
    }

    #[derive(Clone)]
    struct FixedIds {
        values: Arc<Mutex<VecDeque<String>>>,
    }

    impl IdGenerator for FixedIds {
        fn next_ulid(&self) -> String {
            self.values.lock().unwrap().pop_front().unwrap()
        }
    }

    struct VecSink(Arc<Mutex<Vec<Vec<u8>>>>);

    impl LineSink for VecSink {
        fn write_line(&mut self, line: &[u8]) -> io::Result<()> {
            self.0.lock().unwrap().push(line.to_vec());
            Ok(())
        }
    }

    fn fixed_writer(
        graph: &str,
        run: &str,
        ids: &[&str],
        timestamps: &[u64],
    ) -> (CaptureWriter, Arc<Mutex<Vec<Vec<u8>>>>) {
        let output = Arc::new(Mutex::new(Vec::new()));
        let writer = CaptureWriter::with_parts(
            WriterIdentity::fixed(graph, run),
            64,
            Arc::new(FixedClock {
                values: Arc::new(Mutex::new(timestamps.iter().copied().collect())),
            }),
            Arc::new(FixedIds {
                values: Arc::new(Mutex::new(
                    ids.iter().map(|value| (*value).into()).collect(),
                )),
            }),
            Box::new(VecSink(Arc::clone(&output))),
            None,
        );
        (writer, output)
    }

    fn lines(output: &Arc<Mutex<Vec<Vec<u8>>>>) -> Vec<String> {
        output
            .lock()
            .unwrap()
            .iter()
            .map(|line| String::from_utf8(line.clone()).unwrap())
            .collect()
    }

    fn fixture_lines() -> Vec<String> {
        // Byte-identical cell-lifecycle excerpt of the ratified CaptureEvent
        // Cell-lifecycle excerpt inherited from the prior v0.1 bundle. These
        // events remain byte-identical and valid under the current 8d83317d…
        // bundle; the new lease identifiers are covered by focused tests
        // below.
        include_str!("../observatory-fixtures/cell-v0.1-ea423e43.ndjson")
            .lines()
            .map(|line| format!("{line}\n"))
            .collect()
    }

    #[test]
    fn serializer_matches_reviewed_success_lifecycle_bytes() {
        let fixture = fixture_lines();
        let (writer, output) = fixed_writer(
            "organism-dev",
            "01J0000000000000000000000R",
            &[
                "01J0000000000000000000000C",
                "01J0000000000000000000000E",
                "01J0000000000000000000000F",
                "01J0000000000000000000000D",
                "01J0000000000000000000000G",
            ],
            &[
                1783890000012,
                1783890000013,
                1783890000014,
                1783890000015,
                1783890000016,
            ],
        );
        writer.emit_boot_ready(
            BootMode::Restored,
            Duration::from_millis(812),
            Duration::from_millis(641),
            Duration::from_millis(171),
            Some(SnapshotId::new("snap-000042").unwrap()),
            Some(40960),
        );
        let zero = ActivityCounts {
            in_flight_requests: 0,
            open_websockets: 0,
            background_jobs: 0,
            background_leases: 0,
        };
        writer.emit_draining(StopReason::Idle, zero, Some(Duration::from_millis(900000)));
        writer.emit_quiesced(StopReason::Idle, false, zero, Duration::ZERO);
        writer.emit_flush_succeeded(
            FlushMode::Final,
            true,
            Duration::from_millis(92),
            FlushMeasurements {
                snapshot_id: Some(SnapshotId::new("snap-000043").unwrap()),
                bytes: 8192,
                files_copied: 3,
                files_linked: 8,
                stores_backed_up: 1,
            },
        );
        writer.emit_terminal_succeeded(StopReason::Idle, SuccessfulFinalFlush::Completed);
        assert!(writer.shutdown(Duration::from_secs(1)));
        assert_eq!(lines(&output), fixture[0..5]);
    }

    #[test]
    fn serializer_matches_reviewed_startup_failure_bytes() {
        let fixture = fixture_lines();
        let (writer, output) = fixed_writer(
            "startup-failure",
            "01J0000000000000000000000S",
            &["01J0000000000000000000000K", "01J0000000000000000000000S"],
            &[1783890000019, 1783890000022],
        );
        writer.emit_boot_failed(
            BootFailureStage::Hydrate,
            ErrorCode::DurableHydrateFailed,
            Duration::from_millis(17),
        );
        writer.emit_terminal_failed(
            StopReason::StartupFailure,
            FailedFinalFlush::NotConfigured,
            ErrorCode::DurableHydrateFailed,
        );
        assert!(writer.shutdown(Duration::from_secs(1)));
        assert_eq!(lines(&output), vec![fixture[5].clone(), fixture[8].clone()]);
    }

    #[test]
    fn serializer_matches_reviewed_runtime_failure_bytes() {
        let fixture = fixture_lines();
        let (writer, output) = fixed_writer(
            "runtime-failure",
            "01J0000000000000000000000V",
            &["01J0000000000000000000000M", "01J0000000000000000000000N"],
            &[1783890000020, 1783890000021],
        );
        writer.emit_flush_failed(
            FlushMode::Final,
            ErrorCode::FlushGateTimeout,
            Duration::from_millis(20000),
        );
        writer.emit_terminal_failed(
            StopReason::RuntimeFailure,
            FailedFinalFlush::Failed,
            ErrorCode::FinalFlushFailed,
        );
        assert!(writer.shutdown(Duration::from_secs(1)));
        assert_eq!(lines(&output), fixture[6..8]);
    }

    #[test]
    fn serializer_names_a_lease_fenced_flush_as_an_error() {
        let (writer, output) = fixed_writer(
            "organism-dev",
            "01J0000000000000000000000R",
            &["01J0000000000000000000000Z"],
            &[1783890000099],
        );
        writer.emit_flush_failed(
            FlushMode::Periodic,
            ErrorCode::LeaseFenced,
            Duration::from_millis(23),
        );
        assert!(writer.shutdown(Duration::from_secs(1)));

        let value: serde_json::Value = serde_json::from_str(lines(&output)[0].trim_end()).unwrap();
        assert_eq!(value["kind"], "cell.flush");
        assert_eq!(value["outcome"], "error");
        assert_eq!(value["payload"]["mode"], "periodic");
        assert_eq!(value["payload"]["published"], false);
        assert_eq!(value["payload"]["error_code"], "lease_fenced");
    }

    #[test]
    fn serializer_emits_existing_write_lease_dependency_state_shape() {
        let (writer, output) = fixed_writer(
            "organism-dev",
            "01J0000000000000000000000R",
            &["01J0000000000000000000001B", "01J0000000000000000000001C"],
            &[1783890000100, 1783890000101],
        );
        writer.emit_write_lease_degraded(DependencyDetailCode::LeaseGateBPublishIntent);
        writer.emit_write_lease_recovered();
        assert!(writer.shutdown(Duration::from_secs(1)));

        let values = lines(&output)
            .into_iter()
            .map(|line| serde_json::from_str::<serde_json::Value>(line.trim_end()).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(values[0]["kind"], "dep.state");
        assert_eq!(values[0]["outcome"], "ok");
        assert_eq!(values[0]["payload"]["dependency"], "write_lease");
        assert_eq!(values[0]["payload"]["state"], "degraded");
        assert_eq!(
            values[0]["payload"]["detail_code"],
            "lease_gate_b_publish_intent"
        );
        assert_eq!(values[1]["kind"], "dep.state");
        assert_eq!(values[1]["payload"]["dependency"], "write_lease");
        assert_eq!(values[1]["payload"]["state"], "recovered");
        assert!(values[1]["payload"].get("detail_code").is_none());
    }

    #[test]
    fn invalid_identifiers_and_content_shaped_values_are_unrepresentable() {
        const CONTENT_SENTINEL: &str = "private document text must never enter testimony";
        assert!(SnapshotId::new(CONTENT_SENTINEL).is_err());
        assert!(!valid_graph_id(CONTENT_SENTINEL));
        assert!(!valid_opaque_id(CONTENT_SENTINEL));

        let (writer, output) = fixed_writer(
            "organism-dev",
            "01J0000000000000000000000R",
            &["01J0000000000000000000000C"],
            &[1783890000012],
        );
        writer.emit_boot_ready(
            BootMode::Fresh,
            Duration::ZERO,
            Duration::ZERO,
            Duration::ZERO,
            None,
            None,
        );
        assert!(writer.shutdown(Duration::from_secs(1)));
        assert!(!lines(&output).join("").contains(CONTENT_SENTINEL));
    }

    #[test]
    fn capture_enablement_is_default_off_and_strict_when_enabled() {
        assert_eq!(
            WriterIdentity::configured(None, None, None, None, None, None).unwrap(),
            None
        );
        assert_eq!(
            WriterIdentity::configured(
                Some("false"),
                Some("ignored bundle"),
                Some("ignored graph"),
                Some("ignored cell"),
                Some("ignored machine"),
                Some("ignored run"),
            )
            .unwrap(),
            None
        );
        assert!(
            WriterIdentity::configured(Some("sometimes"), None, None, None, None, None)
                .unwrap_err()
                .contains("must be true/false")
        );
        assert!(
            WriterIdentity::configured(Some("true"), None, None, None, None, None)
                .unwrap_err()
                .contains(CONTRACT_BUNDLE_SHA256_ENV)
        );
        assert!(WriterIdentity::configured(
            Some("true"),
            Some("not-the-ratified-bundle"),
            Some("organism-dev"),
            None,
            Some("cell:organism-dev"),
            Some("01J0000000000000000000000R"),
        )
        .unwrap_err()
        .contains("must equal the ratified CaptureEvent v0.3 bundle"));
        assert!(WriterIdentity::configured(
            Some("true"),
            Some(EXPECTED_CONTRACT_BUNDLE_SHA256),
            None,
            None,
            None,
            None,
        )
        .unwrap_err()
        .contains("GARDEN_CELL_GRAPH_ID"));
        assert!(WriterIdentity::configured(
            Some("true"),
            Some(EXPECTED_CONTRACT_BUNDLE_SHA256),
            Some("organism-dev"),
            None,
            Some("cell:different"),
            Some("01J0000000000000000000000R"),
        )
        .unwrap_err()
        .contains("must equal \"cell:organism-dev\""));
        assert!(WriterIdentity::configured(
            Some("true"),
            Some(EXPECTED_CONTRACT_BUNDLE_SHA256),
            Some("organism-dev"),
            None,
            Some("cell:organism-dev"),
            Some("not-a-run"),
        )
        .unwrap_err()
        .contains("valid ULID"));

        let identity = WriterIdentity::configured(
            Some("1"),
            Some(EXPECTED_CONTRACT_BUNDLE_SHA256),
            Some("organism-dev"),
            None,
            Some("cell:organism-dev"),
            Some("01J0000000000000000000000R"),
        )
        .unwrap()
        .unwrap();
        assert_eq!(identity.graph_id, "organism-dev");
        assert_eq!(
            identity.witness,
            "cell:organism-dev/01J0000000000000000000000R"
        );

        let identity = WriterIdentity::configured(
            Some("true"),
            Some(EXPECTED_CONTRACT_BUNDLE_SHA256),
            Some("notes"),
            Some("c-cd429698f332a8a0b3ebd19ae2d2a032fecb1990"),
            Some("cell:c-cd429698f332a8a0b3ebd19ae2d2a032fecb1990"),
            Some("01J0000000000000000000000R"),
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            identity.graph_id,
            "c-cd429698f332a8a0b3ebd19ae2d2a032fecb1990"
        );
        assert_eq!(
            identity.witness,
            "cell:c-cd429698f332a8a0b3ebd19ae2d2a032fecb1990/01J0000000000000000000000R"
        );
    }

    #[test]
    fn disabled_writer_is_a_true_noop() {
        let writer = CaptureWriter { inner: None };
        assert!(!writer.enabled());
        writer.emit_boot_ready(
            BootMode::Fresh,
            Duration::ZERO,
            Duration::ZERO,
            Duration::ZERO,
            None,
            None,
        );
        assert_eq!(writer.counters(), WriterCounters::default());
        assert!(writer.shutdown(Duration::ZERO));
    }

    #[test]
    fn integers_outside_the_language_neutral_range_are_dropped() {
        let (writer, output) = fixed_writer(
            "organism-dev",
            "01J0000000000000000000000R",
            &["01J0000000000000000000000C"],
            &[1783890000012],
        );
        writer.emit_boot_ready(
            BootMode::Restored,
            Duration::ZERO,
            Duration::ZERO,
            Duration::ZERO,
            None,
            Some(MAX_SAFE_INTEGER + 1),
        );
        assert!(writer.shutdown(Duration::from_secs(1)));
        assert!(lines(&output).is_empty());
        assert_eq!(writer.counters().dropped_total, 1);
    }

    struct StalledSink {
        reached: Option<test_mpsc::Sender<()>>,
        release: test_mpsc::Receiver<()>,
        output: Arc<Mutex<Vec<Vec<u8>>>>,
    }

    impl LineSink for StalledSink {
        fn write_line(&mut self, line: &[u8]) -> io::Result<()> {
            if let Some(reached) = self.reached.take() {
                let _ = reached.send(());
                let _ = self.release.recv();
            }
            self.output.lock().unwrap().push(line.to_vec());
            Ok(())
        }
    }

    struct IncrementingClock(AtomicU64);

    impl Clock for IncrementingClock {
        fn now_ms(&self) -> u64 {
            self.0.fetch_add(1, Ordering::Relaxed)
        }
    }

    struct IncrementingIds(AtomicU64);

    impl IdGenerator for IncrementingIds {
        fn next_ulid(&self) -> String {
            let value = self.0.fetch_add(1, Ordering::Relaxed);
            Ulid::from_parts(1_783_890_000_000 + value, value as u128).to_string()
        }
    }

    /// A `Clock` that, on its first call only, signals `reached` and blocks
    /// until `release` is sent — giving a test deterministic control over a
    /// point deep inside `emit()` (`now_ms()` runs after sequence/id are
    /// reserved but strictly *before* the `InFlightGuard::admit` CAS —
    /// i.e. this call has not yet touched `admission` at all), without
    /// touching production code with a test-only hook.
    struct PausingClock {
        reached: Mutex<Option<test_mpsc::Sender<()>>>,
        release: Mutex<Option<test_mpsc::Receiver<()>>>,
        next: AtomicU64,
    }

    impl Clock for PausingClock {
        fn now_ms(&self) -> u64 {
            if let Some(reached) = self
                .reached
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .take()
            {
                let _ = reached.send(());
                if let Some(release) = self
                    .release
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .take()
                {
                    let _ = release.recv();
                }
            }
            1_783_890_000_000 + self.next.fetch_add(1, Ordering::Relaxed)
        }
    }

    #[test]
    fn live_writer_attempts_heartbeat_without_caller_activity() {
        let output = Arc::new(Mutex::new(Vec::new()));
        let writer = CaptureWriter::with_parts(
            WriterIdentity::fixed("heartbeat", "01J0000000000000000000000R"),
            4,
            Arc::new(IncrementingClock(AtomicU64::new(1_783_890_000_000))),
            Arc::new(IncrementingIds(AtomicU64::new(1))),
            Box::new(VecSink(Arc::clone(&output))),
            Some(Duration::from_millis(5)),
        );

        let deadline = Instant::now() + Duration::from_secs(1);
        while output.lock().unwrap().is_empty() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(1));
        }
        assert!(writer.shutdown(Duration::from_secs(1)));

        let parsed: Vec<serde_json::Value> = lines(&output)
            .iter()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert!(!parsed.is_empty(), "timer must attempt a heartbeat");
        assert!(parsed.iter().all(|event| event["kind"] == "obs.heartbeat"));
        assert_eq!(parsed[0]["seq"], 0);
        assert_eq!(parsed[0]["payload"]["emitted_total"], 0);
        assert_eq!(parsed[0]["payload"]["dropped_total"], 0);
    }

    #[test]
    fn stalled_sink_is_bounded_and_sequence_gaps_report_cumulative_loss() {
        let output = Arc::new(Mutex::new(Vec::new()));
        let (reached_tx, reached_rx) = test_mpsc::channel();
        let (release_tx, release_rx) = test_mpsc::channel();
        let writer = CaptureWriter::with_parts(
            WriterIdentity::fixed("load-test", "01J0000000000000000000000R"),
            1,
            Arc::new(IncrementingClock(AtomicU64::new(1_783_890_000_000))),
            Arc::new(IncrementingIds(AtomicU64::new(1))),
            Box::new(StalledSink {
                reached: Some(reached_tx),
                release: release_rx,
                output: Arc::clone(&output),
            }),
            None,
        );

        writer.emit_boot_ready(
            BootMode::Fresh,
            Duration::ZERO,
            Duration::ZERO,
            Duration::ZERO,
            None,
            None,
        );
        reached_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        writer.emit_terminal_succeeded(StopReason::Idle, SuccessfulFinalFlush::NotConfigured);
        let started = Instant::now();
        for _ in 0..10_000 {
            writer.emit_terminal_succeeded(StopReason::Idle, SuccessfulFinalFlush::NotConfigured);
        }
        assert!(started.elapsed() < Duration::from_secs(1));
        let saturated = writer.counters();
        assert_eq!(saturated.emitted_total, 2);
        assert_eq!(saturated.dropped_total, 10_000);
        assert_eq!(saturated.next_sequence, 10_002);

        release_tx.send(()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(1);
        while writer.counters().written_total < 2 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(1));
        }
        writer.emit_heartbeat();
        assert!(writer.shutdown(Duration::from_secs(1)));

        let parsed: Vec<serde_json::Value> = lines(&output)
            .iter()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(parsed.len(), 3);
        assert_eq!(parsed[0]["seq"], 0);
        assert_eq!(parsed[1]["seq"], 1);
        assert_eq!(parsed[2]["seq"], 10_002);
        assert_eq!(parsed[2]["kind"], "obs.heartbeat");
        assert_eq!(parsed[2]["payload"]["emitted_total"], 2);
        assert_eq!(parsed[2]["payload"]["dropped_total"], 10_000);
    }

    #[test]
    fn shutdown_budget_is_bounded_when_consumer_never_resumes() {
        let output = Arc::new(Mutex::new(Vec::new()));
        let (reached_tx, reached_rx) = test_mpsc::channel();
        let (_release_tx, release_rx) = test_mpsc::channel();
        let writer = CaptureWriter::with_parts(
            WriterIdentity::fixed("stalled", "01J0000000000000000000000R"),
            1,
            Arc::new(IncrementingClock(AtomicU64::new(1_783_890_000_000))),
            Arc::new(IncrementingIds(AtomicU64::new(1))),
            Box::new(StalledSink {
                reached: Some(reached_tx),
                release: release_rx,
                output,
            }),
            None,
        );
        writer.emit_terminal_succeeded(StopReason::Idle, SuccessfulFinalFlush::NotConfigured);
        reached_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        let started = Instant::now();
        assert!(!writer.shutdown(Duration::from_millis(25)));
        assert!(started.elapsed() < Duration::from_millis(100));
    }

    // --- Phase-1 interaction spine ---------------------------------------

    fn parsed_lines(output: &Arc<Mutex<Vec<Vec<u8>>>>) -> Vec<serde_json::Value> {
        lines(output)
            .iter()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    #[test]
    fn emit_interaction_produces_a_contract_shaped_graph_write_event_with_on_behalf_of() {
        let (writer, output) = fixed_writer(
            "organism-dev",
            "01J0000000000000000000000R",
            &["01J0000000000000000000000C"],
            &[1783890000012],
        );
        let identity = CaptureIdentity::new(
            "service:choreograph",
            ClientClass::Agent,
            Some("user-42".into()),
        )
        .unwrap();
        writer.emit_interaction(
            GraphInteractionKind::Write,
            &identity,
            Some("g1"),
            Outcome::Ok,
            Some(42),
            GraphInteractionPayload::Write(GraphWritePayload {
                op: GraphWriteOp::SparqlUpdate,
                tool_name: Some(ToolName::new("sparql_query").unwrap()),
                triples_added: Some(3),
                triples_removed: Some(1),
                bytes: Some(256),
            }),
        );
        assert!(writer.shutdown(Duration::from_secs(1)));

        let parsed = parsed_lines(&output);
        assert_eq!(parsed.len(), 1);
        let event = &parsed[0];
        assert_eq!(event["kind"], "graph.write");
        assert_eq!(event["principal"], "service:choreograph");
        assert_eq!(event["client_class"], "agent");
        assert_eq!(event["on_behalf_of"], "user-42");
        assert_eq!(event["graph_id"], "g1");
        assert_eq!(event["outcome"], "ok");
        assert_eq!(event["duration_ms"], 42);
        assert_eq!(
            event["witness"],
            "cell:organism-dev/01J0000000000000000000000R"
        );
        assert_eq!(event["payload"]["op"], "sparql_update");
        assert_eq!(event["payload"]["tool_name"], "sparql_query");
        assert_eq!(event["payload"]["triples_added"], 3);
        assert_eq!(event["payload"]["triples_removed"], 1);
        assert_eq!(event["payload"]["bytes"], 256);

        // Canonical wire order (obs.golden.json `canonical_key_order`, with
        // `via_service`/`auth_method` simply absent — the validator only
        // orders keys that are present). `serde_json`'s crate-wide
        // `preserve_order` feature keeps this reflecting real field order.
        let keys: Vec<&str> = event
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            keys,
            vec![
                "v",
                "event_id",
                "ts",
                "witness",
                "kind",
                "principal",
                "on_behalf_of",
                "client_class",
                "graph_id",
                "outcome",
                "duration_ms",
                "seq",
                "payload",
            ]
        );
    }

    #[test]
    fn emit_interaction_defaults_graph_id_to_the_writers_own_bound_graph() {
        let (writer, output) = fixed_writer(
            "organism-dev",
            "01J0000000000000000000000R",
            &["01J0000000000000000000000C"],
            &[1783890000012],
        );
        let identity = CaptureIdentity::new("user-7", ClientClass::Browser, None).unwrap();
        writer.emit_interaction(
            GraphInteractionKind::Read,
            &identity,
            None,
            Outcome::Ok,
            None,
            GraphInteractionPayload::Read(GraphReadPayload {
                op: GraphReadOp::SparqlSelect,
                tool_name: None,
                result_count: Some(0),
            }),
        );
        assert!(writer.shutdown(Duration::from_secs(1)));
        let parsed = parsed_lines(&output);
        assert_eq!(parsed[0]["graph_id"], "organism-dev");
        assert!(parsed[0].get("on_behalf_of").is_none());
    }

    #[test]
    fn tool_name_rejects_content_shaped_values() {
        const CONTENT_SENTINEL: &str = "private document text must never enter testimony";
        assert!(ToolName::new(CONTENT_SENTINEL).is_err());
        assert!(ToolName::new("sparql_query").is_ok());
        assert!(ToolName::new("read_document").is_ok());
    }

    #[test]
    fn tool_name_rejects_identifier_shaped_values_not_in_the_mcp_catalog() {
        // Shape-valid (matches the tool_name domain regex exactly) but not a
        // registered tool: the closed-catalog check must still reject it.
        // This is the concrete case the shape-only regex used to miss.
        assert!(ToolName::new("MyPrivateDiagnosis").is_err());
        assert!(ToolName::new("sparql_query_but_not_real").is_err());
        // A registered tool remains accepted.
        assert!(ToolName::new("write_document").is_ok());
    }

    #[test]
    fn emit_interaction_drops_on_kind_payload_mismatch() {
        let (writer, output) = fixed_writer(
            "organism-dev",
            "01J0000000000000000000000R",
            &["01J0000000000000000000000C"],
            &[1783890000012],
        );
        let identity = CaptureIdentity::new("user-7", ClientClass::Browser, None).unwrap();
        writer.emit_interaction(
            GraphInteractionKind::Read,
            &identity,
            None,
            Outcome::Ok,
            None,
            // Payload says Write; kind says Read. Must never be emitted.
            GraphInteractionPayload::Write(GraphWritePayload {
                op: GraphWriteOp::Crdt,
                tool_name: None,
                triples_added: None,
                triples_removed: None,
                bytes: None,
            }),
        );
        assert!(writer.shutdown(Duration::from_secs(1)));
        assert!(lines(&output).is_empty());
        assert_eq!(writer.counters().dropped_total, 1);
    }

    #[test]
    fn emit_interaction_refuses_graph_create_and_graph_delete_from_the_cell() {
        // `obs.golden.json` authorizes only the `gateway` witness for
        // graph.create/graph.delete. This cell's witness is always
        // `cell:...`; emitting either must be structurally impossible, even
        // with an otherwise well-formed, kind-matched payload.
        let (writer, output) = fixed_writer(
            "organism-dev",
            "01J0000000000000000000000R",
            &["01J0000000000000000000000C", "01J0000000000000000000000D"],
            &[1783890000012, 1783890000013],
        );
        let identity = CaptureIdentity::new("service:gardend", ClientClass::Service, None).unwrap();
        writer.emit_interaction(
            GraphInteractionKind::Create,
            &identity,
            None,
            Outcome::Ok,
            None,
            GraphInteractionPayload::Create(GraphCreatePayload {
                stage: Some(GraphLifecycleStage::Completed),
            }),
        );
        writer.emit_interaction(
            GraphInteractionKind::Delete,
            &identity,
            None,
            Outcome::Ok,
            None,
            GraphInteractionPayload::Delete(GraphDeletePayload {
                durable_disposition: DeleteDisposition::Purged,
                stage: Some(GraphLifecycleStage::Completed),
            }),
        );
        assert!(writer.shutdown(Duration::from_secs(1)));
        assert!(lines(&output).is_empty());
        assert_eq!(writer.counters().dropped_total, 2);
    }

    #[test]
    fn graph_interaction_kind_authorization_matches_the_ratified_contract() {
        assert!(GraphInteractionKind::Read.cell_may_witness());
        assert!(GraphInteractionKind::Write.cell_may_witness());
        assert!(!GraphInteractionKind::Create.cell_may_witness());
        assert!(!GraphInteractionKind::Delete.cell_may_witness());
    }

    #[test]
    fn capture_identity_new_validates_the_principal_domain() {
        assert!(CaptureIdentity::new("anon", ClientClass::Anon, None).is_ok());
        assert!(CaptureIdentity::new("service:choreograph", ClientClass::Service, None).is_ok());
        assert!(CaptureIdentity::new("user-abc_123", ClientClass::Browser, None).is_ok());
        assert!(CaptureIdentity::new("", ClientClass::Browser, None).is_err());
        assert!(CaptureIdentity::new("service:", ClientClass::Service, None).is_err());
        assert!(
            CaptureIdentity::new("service:CapitalNotAllowed", ClientClass::Service, None).is_err()
        );
        assert!(
            CaptureIdentity::new(" leading-space-not-alnum", ClientClass::Browser, None).is_err()
        );
    }

    #[test]
    fn capture_identity_on_behalf_of_requires_a_service_principal_and_must_differ() {
        assert!(
            CaptureIdentity::new("user-7", ClientClass::Browser, Some("user-8".into())).is_err()
        );
        assert!(CaptureIdentity::new(
            "service:choreograph",
            ClientClass::Agent,
            Some("service:choreograph".into())
        )
        .is_err());
        assert!(CaptureIdentity::new(
            "service:choreograph",
            ClientClass::Agent,
            Some("user-8".into())
        )
        .is_ok());
    }

    #[test]
    fn capture_identity_from_forwarded_header_round_trips() {
        let json = serde_json::json!({
            "principal": "service:choreograph",
            "client_class": "agent",
            "on_behalf_of": "user-42",
        });
        let encoded = BASE64_STANDARD.encode(serde_json::to_vec(&json).unwrap());
        let identity = CaptureIdentity::from_forwarded_header(&encoded).unwrap();
        assert_eq!(identity.principal(), "service:choreograph");
        assert_eq!(identity.client_class().as_str(), "agent");
        assert_eq!(identity.on_behalf_of(), Some("user-42"));
    }

    #[test]
    fn capture_identity_from_forwarded_header_rejects_malformed_input() {
        assert!(CaptureIdentity::from_forwarded_header("not-base64!!").is_err());

        let not_json = BASE64_STANDARD.encode("not json");
        assert!(CaptureIdentity::from_forwarded_header(&not_json).is_err());

        let unknown_client_class = BASE64_STANDARD.encode(
            serde_json::to_vec(&serde_json::json!({
                "principal": "user-7",
                "client_class": "robot",
            }))
            .unwrap(),
        );
        assert!(CaptureIdentity::from_forwarded_header(&unknown_client_class).is_err());

        let bad_delegation = BASE64_STANDARD.encode(
            serde_json::to_vec(&serde_json::json!({
                "principal": "user-7",
                "client_class": "browser",
                "on_behalf_of": "user-8",
            }))
            .unwrap(),
        );
        assert!(CaptureIdentity::from_forwarded_header(&bad_delegation).is_err());
    }

    #[test]
    fn graph_id_domain_is_enforced_at_emit_not_left_to_the_validator() {
        let (writer, output) = fixed_writer(
            "organism-dev",
            "01J0000000000000000000000R",
            &["01J0000000000000000000000C"],
            &[1783890000012],
        );
        let identity = CaptureIdentity::new("user-7", ClientClass::Browser, None).unwrap();
        writer.emit_interaction(
            GraphInteractionKind::Read,
            &identity,
            // Contains `/`, which the graph_id domain
            // (`^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$`) forbids.
            Some("bad/graph"),
            Outcome::Ok,
            None,
            GraphInteractionPayload::Read(GraphReadPayload {
                op: GraphReadOp::SparqlSelect,
                tool_name: None,
                result_count: None,
            }),
        );
        assert!(writer.shutdown(Duration::from_secs(1)));
        assert!(lines(&output).is_empty());
        assert_eq!(writer.counters().dropped_total, 1);
    }

    #[test]
    fn emit_after_shutdown_is_dropped_without_blocking_or_panicking() {
        // Regression for the hot-path fix: `emit()` no longer clones a
        // sender out of a `Mutex<Option<_>>` — it checks a lock-free
        // `closed` flag instead. Prove the observable behavior is
        // unchanged: once `shutdown` returns, further emits are silently
        // dropped, never written, never panicking.
        // `emit()` still mints an id/timestamp per attempt even when closed
        // (see the comment in `emit()`), so the fixed clock/id queues need
        // one entry per emit() call this test makes — 1 pre-shutdown + 2
        // post-shutdown (both dropped) = 3.
        let (writer, output) = fixed_writer(
            "organism-dev",
            "01J0000000000000000000000R",
            &[
                "01J0000000000000000000000C",
                "01J0000000000000000000000D",
                "01J0000000000000000000000E",
            ],
            &[1783890000012, 1783890000013, 1783890000014],
        );
        writer.emit_boot_ready(
            BootMode::Fresh,
            Duration::ZERO,
            Duration::ZERO,
            Duration::ZERO,
            None,
            None,
        );
        assert!(writer.shutdown(Duration::from_secs(1)));
        let before = writer.counters();

        writer.emit_boot_ready(
            BootMode::Fresh,
            Duration::ZERO,
            Duration::ZERO,
            Duration::ZERO,
            None,
            None,
        );
        writer.emit_terminal_succeeded(StopReason::Idle, SuccessfulFinalFlush::Completed);

        let after = writer.counters();
        assert_eq!(after.dropped_total, before.dropped_total + 2);
        assert_eq!(after.emitted_total, before.emitted_total);
        // Sequence still advances (a post-shutdown emit is still a
        // consumed, gap-visible sequence number, same as any other invalid
        // shape) — but nothing new was written.
        assert_eq!(after.next_sequence, before.next_sequence + 2);
        assert_eq!(lines(&output).len(), 1);
    }

    #[test]
    fn concurrent_emits_do_not_contend_or_lose_accounting() {
        // The hot-path fix replaced a per-call `Mutex<Option<SyncSender>>`
        // lock+clone with a direct, lock-free `SyncSender::try_send`. Prove
        // concurrent emitters from real OS threads still produce correct,
        // non-racing counters and a fully-drained, gap-free sequence — the
        // property the mutex existed to protect was never actually
        // "correctness under concurrency" (the channel already handles
        // that), so removing it must not have broken anything.
        //
        // Real `SystemClock`/`SystemIds` (not the test `FixedClock`, which
        // pops a pre-queued value per call and would panic under 400
        // concurrent calls) — this is production concurrency behavior, not
        // a mock of it.
        let output: Arc<Mutex<Vec<Vec<u8>>>> = Arc::new(Mutex::new(Vec::new()));
        // Capacity must exceed the 400 total emits below: with a smaller
        // queue the no-drop assertions depend on the drain outrunning the
        // producers, which fails on fast machines (release + 16 cores
        // dropped 3/400 at capacity 256 on the native builder). This test
        // pins drop-free accounting; overflow drops are a separate,
        // counted path (dropped_total).
        let writer = CaptureWriter::with_parts(
            WriterIdentity::fixed("organism-dev", "01J0000000000000000000000R"),
            1024,
            Arc::new(SystemClock),
            Arc::new(SystemIds),
            Box::new(VecSink(Arc::clone(&output))),
            None,
        );
        let threads: Vec<_> = (0..8)
            .map(|_| {
                // `CaptureWriter` is cheaply `Clone` (an `Arc<WriterInner>`
                // underneath) — this is exactly the shape production code
                // uses to share one writer across concurrently-running
                // request handlers.
                let writer = writer.clone();
                thread::spawn(move || {
                    for _ in 0..50 {
                        writer.emit_heartbeat();
                    }
                })
            })
            .collect();
        for handle in threads {
            handle.join().unwrap();
        }
        assert!(writer.shutdown(Duration::from_secs(2)));

        let counters = writer.counters();
        assert_eq!(counters.emitted_total, 400);
        assert_eq!(counters.dropped_total, 0);
        assert_eq!(counters.next_sequence, 400);

        let parsed = parsed_lines(&output);
        assert_eq!(parsed.len(), 400);
        let mut seqs: Vec<u64> = parsed
            .iter()
            .map(|event| event["seq"].as_u64().unwrap())
            .collect();
        seqs.sort_unstable();
        seqs.dedup();
        assert_eq!(seqs.len(), 400, "no duplicate or dropped sequence numbers");
        assert_eq!(seqs[0], 0);
        assert_eq!(seqs[399], 399);
    }

    #[test]
    fn admission_race_between_a_paused_emit_and_concurrent_shutdown_is_always_safe() {
        // Reproduces, at the scheduling level, the exact interleaving the
        // joint refute flagged for the OLD two-atomic design (`closed:
        // AtomicBool` + a separate `in_flight_emits: AtomicU64`): an
        // `emit()` call caught between reserving its sequence/id and
        // attempting admission, racing a concurrent `shutdown()`. The bug
        // was that `shutdown()` could observe a stale, not-yet-visible
        // in-flight count and declare "drained" while that same call had
        // separately observed a stale `closed == false` and still went on
        // to `try_send` — a send accepted after "drained" was already
        // reported, because the two signals lived on independent atomics
        // with no shared modification order.
        //
        // `PausingClock` blocks the emitter thread exactly there — after
        // sequence/id are reserved, strictly *before*
        // `InFlightGuard::admit` has touched `admission` at all — under a
        // controlled rendezvous. Unlike the previous version of this test
        // (which called `shutdown()` fully synchronously, so it always
        // completed before the paused call was ever released, and so only
        // ever proved shutdown waits for an already-visible counter), this
        // version runs `shutdown()` on its own thread and only then
        // releases the paused call, with no ordering imposed between the
        // two — so the actual admission CAS races `shutdown()`'s
        // `fetch_or` for real, and either can win.
        //
        // With one packed atomic, both outcomes are legitimate and safe:
        // (a) closed wins the race — the call is cleanly rejected,
        //     nothing sent, nothing counted; or
        // (b) admission wins the race — the call is counted in-flight,
        //     and `shutdown()` (polling the very same word) is provably
        //     obligated to have waited for it before returning `true`.
        // There is no third, lost-emit outcome. Run many trials, since
        // which branch a given trial takes is up to the OS scheduler —
        // each trial independently proves the safety property regardless
        // of which branch it happens to exercise.
        for trial in 0..200 {
            let output = Arc::new(Mutex::new(Vec::new()));
            let (reached_tx, reached_rx) = test_mpsc::channel();
            let (release_tx, release_rx) = test_mpsc::channel();
            let writer = CaptureWriter::with_parts(
                WriterIdentity::fixed("admission-race", "01J0000000000000000000000R"),
                8,
                Arc::new(PausingClock {
                    reached: Mutex::new(Some(reached_tx)),
                    release: Mutex::new(Some(release_rx)),
                    next: AtomicU64::new(0),
                }),
                Arc::new(IncrementingIds(AtomicU64::new(1))),
                Box::new(VecSink(Arc::clone(&output))),
                None,
            );

            let emitter = {
                let writer = writer.clone();
                thread::spawn(move || writer.emit_heartbeat())
            };
            // Block here until the emitter is paused: sequence/id already
            // reserved, `admission` not yet touched.
            reached_rx.recv_timeout(Duration::from_secs(1)).unwrap();

            // Race `shutdown()` against the release — no artificial
            // ordering between them.
            let shutdown_writer = writer.clone();
            let shutdown = thread::spawn(move || shutdown_writer.shutdown(Duration::from_secs(2)));
            release_tx.send(()).unwrap();

            emitter.join().unwrap();
            assert!(
                shutdown.join().unwrap(),
                "trial {trial}: shutdown must report drained within its 2s budget"
            );

            let counters = writer.counters();
            // Exactly one of the two admission outcomes happened for the
            // raced call — never neither, never both.
            assert_eq!(
                counters.emitted_total + counters.dropped_total,
                1,
                "trial {trial}: the raced call must be accounted exactly once"
            );
            // `shutdown()` returning `true` is itself proof it waited: its
            // return condition (`written_total + write_failed_total >=
            // emitted_total`) cannot hold otherwise.
            assert!(
                counters.written_total + counters.write_failed_total >= counters.emitted_total,
                "trial {trial}: shutdown must never report drained with a send outstanding"
            );
            let sunk = lines(&output).len() as u64;
            assert_eq!(
                sunk, counters.written_total,
                "trial {trial}: sink contents must match what shutdown accounted as written"
            );
            if counters.emitted_total == 1 {
                assert_eq!(
                    sunk, 1,
                    "trial {trial}: admission won — the send must have actually reached the \
                     sink before shutdown reported drained, never lost"
                );
            } else {
                assert_eq!(counters.dropped_total, 1);
                assert_eq!(
                    sunk, 0,
                    "trial {trial}: closed won — nothing may ever reach the sink"
                );
            }
        }
    }

    #[test]
    fn many_concurrent_emitters_racing_shutdown_never_lose_or_double_count() {
        // Higher-volume companion to the hook-based test above: no
        // deterministic pause point, just raw thread-scheduler
        // nondeterminism — many emitter threads contending with a
        // concurrent `shutdown()` call, repeated across many fresh
        // writers. This is the fallback the fix's spec allows for a
        // reordering-class bug that a scheduling hook alone cannot force
        // deterministically (the store-buffering interleaving the joint
        // refute described is a memory-visibility phenomenon across two
        // independent atomics, not just a thread-interleaving one — see
        // the `admission` field comment): high iteration count, real
        // threads, real atomics, asserting the same accounting-consistency
        // invariant plus "never panics, never lost".
        for iteration in 0..150 {
            let output = Arc::new(Mutex::new(Vec::new()));
            let writer = CaptureWriter::with_parts(
                WriterIdentity::fixed("admission-stress", "01J0000000000000000000000R"),
                4,
                Arc::new(SystemClock),
                Arc::new(SystemIds),
                Box::new(VecSink(Arc::clone(&output))),
                None,
            );

            let emitters: Vec<_> = (0..6)
                .map(|_| {
                    let writer = writer.clone();
                    thread::spawn(move || {
                        for _ in 0..20 {
                            writer.emit_heartbeat();
                        }
                    })
                })
                .collect();
            // Race `shutdown()` against the still-running emitters — most
            // will be admitted before it sets the closed bit, some may
            // race it exactly at the boundary, some (if shutdown wins
            // early) may be cleanly rejected. All three are legal; none
            // may ever be lost.
            let shutdown_writer = writer.clone();
            let shutdown = thread::spawn(move || shutdown_writer.shutdown(Duration::from_secs(2)));

            for handle in emitters {
                handle.join().unwrap();
            }
            assert!(
                shutdown.join().unwrap(),
                "iteration {iteration}: shutdown must always report drained within its budget, \
                 even under contention"
            );

            let counters = writer.counters();
            assert_eq!(
                counters.written_total + counters.write_failed_total,
                counters.emitted_total,
                "iteration {iteration}: shutdown must never declare drained with a send still \
                 outstanding"
            );
            assert_eq!(
                lines(&output).len() as u64,
                counters.written_total,
                "iteration {iteration}: everything shutdown() accounted as written must actually \
                 be in the sink"
            );
        }
    }

    #[test]
    fn admission_count_saturation_is_refused_without_flipping_closed() {
        // COUNT-INTEGRITY: a further adversarial pass (gpt-5.6-sol) on the
        // single-atomic barrier found its only overflow guard was a
        // `debug_assert!` — in a release build, an `admit()` CAS at
        // exactly `count == ADMISSION_COUNT_MASK` (2^63 - 1) would carry
        // `state + 1` out of the count's 63 bits and into
        // `ADMISSION_CLOSED_BIT`, corrupting both packed signals at once:
        // the word would read "closed, count == 0" even though nothing
        // ever set the closed bit, and a later guard drop would then
        // `fetch_sub` that bogus state — clearing the spurious closed bit
        // right back off again. 2^63-1 simultaneous in-flight emits is
        // physically impossible (never a real data-loss path), but a
        // release build must not rely on that impossibility for its
        // correctness claim. `admit()` now checks this boundary for real
        // (not just via `debug_assert!`) and refuses admission exactly
        // like a full channel: no guard, nothing incremented.
        //
        // No real writer can ever reach this boundary, so this test
        // forces it directly by poking `admission` through the private
        // field this test module shares a crate with (`mod tests` is a
        // descendant of the module that defines `WriterInner` /
        // `InFlightGuard` / `CaptureWriter`) — not a mock of `admit()`'s
        // logic, the real CAS loop runs against a real atomic.
        let output = Arc::new(Mutex::new(Vec::new()));
        let writer = CaptureWriter::with_parts(
            WriterIdentity::fixed("saturation-boundary", "01J0000000000000000000000R"),
            4,
            Arc::new(SystemClock),
            Arc::new(SystemIds),
            Box::new(VecSink(Arc::clone(&output))),
            None,
        );
        let inner = writer.inner.as_ref().expect("writer must be enabled");

        // Force the boundary: count saturated, closed bit clear.
        inner
            .admission
            .store(ADMISSION_COUNT_MASK, Ordering::SeqCst);

        assert!(
            InFlightGuard::admit(inner).is_none(),
            "admission must refuse at the count boundary rather than carry into the closed bit"
        );
        // The word must be completely unchanged: no partial increment, and
        // critically, no bit ever carried into ADMISSION_CLOSED_BIT.
        assert_eq!(
            inner.admission.load(Ordering::SeqCst),
            ADMISSION_COUNT_MASK,
            "a refused boundary admission must leave the packed word untouched"
        );
        assert_eq!(
            inner.admission.load(Ordering::SeqCst) & ADMISSION_CLOSED_BIT,
            0,
            "the closed bit must never be spuriously set by a count-side overflow"
        );

        // Restore a normal state (this test's artificial saturation was
        // never a real in-flight count) and confirm the word was never
        // corrupted: shutdown behaves exactly as it would from a fresh
        // writer, proving the earlier refusal didn't leave the barrier in
        // a broken state.
        inner.admission.store(0, Ordering::SeqCst);
        assert!(
            writer.shutdown(Duration::from_secs(1)),
            "shutdown must still complete normally after a refused boundary admission"
        );
    }
}
