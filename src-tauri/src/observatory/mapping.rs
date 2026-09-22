//! JSON Meaningful-Object → RDF triple mapping — §A.2 (PURE, no I/O).
//!
//! This module owns exactly the mapping contract from the projector's JSON
//! output shapes to RDF triples: `map_lifecycle` / `map_billing` /
//! `map_capture`, the `obs:`/`mach:`/`prov:` IRI builders, and the
//! deterministic subject-mint rules for `SequenceGap` / `ProjectionRun` /
//! `MetricEvaluation` (§A.2). It does **not** touch a `Store`, call
//! `reconcile`, or perform any I/O — `apply.rs` (A3, a separate slice) is
//! where these triples actually get written, via the two reconcile scopes
//! §A.7 settles (raw = windowed-replace, rollups = subject-scoped upsert).
//!
//! **Namespaces (PROPOSED-HEREIN, §A.2):** `obs:
//! <http://mnemosyne.dev/observatory#>`, `mach: <http://mnemosyne.dev/machine#>`
//! (reused verbatim from `sophia-machine-core` — the join this module MUST
//! preserve), `prov: <http://www.w3.org/ns/prov#>`,
//! `xsd: <http://www.w3.org/2001/XMLSchema#>`.
//!
//! **The triple representation is `crate::emporium::terms::{Term, Triple}`**
//! — `Triple = (String, String, Term)` — not a bespoke type. This is the
//! SAME shape `geist_song_rdf.rs`'s reserved-projection precedent builds and
//! feeds straight into `reconcile_classes_validated`
//! (`ba1a97eccc83:src-tauri/src/geist_song_rdf.rs:297-353`), so A3 can
//! consume this module's output with zero translation layer. Reusing
//! `emporium::terms::{Term, Triple}` is reuse of a pure value type, not use
//! of the Emporium generic *ingest write path* (`emporium/write.rs`) that
//! §A.7 explicitly routes around for this materializer — no `Store`, no
//! survey/apply, no ingest planner is touched from here.
//!
//! **Predicate/datatype/rdf_types ground truth:** every predicate name,
//! datatype, and `rdf_types` list below is copied verbatim from the ALREADY
//! LANDED (A1) `src-tauri/src/emporium/vocabs/emporium-observatory.golden.json`
//! pack — not re-derived from the prose in §A.2, which is necessarily a
//! summary. Where A1's pack declares a float-shaped predicate (USD/hours/
//! rates) as `"datatype": "double"`, this module emits `xsd:double`
//! (matching A1's committed, reviewed precedent), even though §A.2's prose
//! says "USD/hours/rates → xsd:decimal" — `Datatype` (`emporium/contract.rs`)
//! has no `decimal` variant and A1 already resolved this the same way.
//!
//! **Where this module had to design past the spec's explicit text (documented
//! at each call site):**
//! - `SequenceGap`'s minted subject formula IS given verbatim in §A.2/A1's
//!   pack comment (`sequence-gap:sha256:{sha256(env|witness|expected|observed|gap|snapshot)}`)
//!   — implemented literally in [`push_sequence_gap`].
//! - `ProjectionRun`'s minted subject formula IS given verbatim
//!   (`projection-run:{env}:{evaluated_at}`) — implemented literally in
//!   [`push_projection_run`].
//! - `MetricEvaluation`'s minted subject formula is **not** given an explicit
//!   formula anywhere in §A.2 or A1's pack (unlike the two above) — this
//!   module mints `metric-evaluation:{metric_id}:{evaluated_at}` for
//!   failures and `metric-evaluation:{metric_id}:{evaluated_at}:{observation
//!   sha256 hex}` for successes (one wire evaluation object = one
//!   `prov:Activity`; capture metrics emit one Success PER SLICE at the same
//!   instant, so the bare pair is not unique there — caught by the first
//!   REAL-ledger catch_up, see [`metric_evaluation_success_subject`]).
//! - `obs:cursorHighWaterMark` (declared optional on `ProjectionRun` in A1's
//!   pack) is deliberately **never emitted here** — it is A3's durable-cursor
//!   freshness triple (§A.3), computed from the sidecar's EFS object-cache
//!   state, which is I/O this pure module never touches.

use std::collections::BTreeSet;

use chrono::{DateTime, SecondsFormat, Utc};
use oxigraph::model::{Literal, NamedNode};
use serde::Deserialize;
use serde_json::Value as Json;
use sha2::{Digest, Sha256};

use crate::emporium::terms::{Term, Triple};

use super::graph_identity::{raw_graph_iri, rollups_graph_iri};

// ---------------------------------------------------------------------------
// Namespaces + IRI builders
// ---------------------------------------------------------------------------

/// `http://mnemosyne.dev/observatory#` (§A.2, PROPOSED-HEREIN, ratified by
/// the Lane A↔B reconciliation note).
pub(crate) const OBS_NS: &str = "http://mnemosyne.dev/observatory#";
/// `http://mnemosyne.dev/machine#` — reused VERBATIM from
/// `sophia-machine-core.golden.json` to preserve the machine-core join (§A.2,
/// §A.7).
pub(crate) const MACH_NS: &str = "http://mnemosyne.dev/machine#";
/// `http://www.w3.org/ns/prov#`.
pub(crate) const PROV_NS: &str = "http://www.w3.org/ns/prov#";
/// `http://www.w3.org/2000/01/rdf-schema#`.
pub(crate) const RDFS_NS: &str = "http://www.w3.org/2000/01/rdf-schema#";
/// `http://www.w3.org/2001/XMLSchema#`.
pub(crate) const XSD_NS: &str = "http://www.w3.org/2001/XMLSchema#";

const RDF_TYPE: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";

/// The observer-relative testimony stamp every rollup subject carries (§A.2):
/// `prov:wasAttributedTo urn:sophia:agent:obs-projector`.
const OBS_PROJECTOR_AGENT: &str = "urn:sophia:agent:obs-projector";

fn obs(local: &str) -> String {
    format!("{OBS_NS}{local}")
}

fn mach(local: &str) -> String {
    format!("{MACH_NS}{local}")
}

fn prov(local: &str) -> String {
    format!("{PROV_NS}{local}")
}

fn rdfs(local: &str) -> String {
    format!("{RDFS_NS}{local}")
}

// ---------------------------------------------------------------------------
// GraphTarget — which reserved `:projection:obs:*` graph a triple set lands in
// ---------------------------------------------------------------------------

/// Which of the two reserved, gate-protected `:projection:obs:*` graphs
/// (§A.2, §A.5) a triple set targets. A3's `apply.rs` uses this to pick the
/// reconcile scope: [`GraphTarget::Raw`] is windowed-replace (72h),
/// [`GraphTarget::Rollups`] is subject-scoped upsert.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GraphTarget {
    /// `urn:mnemosyne:local:graph:observatory:projection:obs:raw`.
    Raw,
    /// `urn:mnemosyne:local:graph:observatory:projection:obs:rollups`.
    Rollups,
}

impl GraphTarget {
    /// The full reserved named-graph IRI this target resolves to, via
    /// [`super::graph_identity`] (never hand-rolled — see that module's own
    /// doc comment on why).
    pub(crate) fn graph_iri(self) -> String {
        match self {
            GraphTarget::Raw => raw_graph_iri(),
            GraphTarget::Rollups => rollups_graph_iri(),
        }
    }
}

// ---------------------------------------------------------------------------
// Term constructors
// ---------------------------------------------------------------------------

fn xsd(local: &str) -> NamedNode {
    NamedNode::new(format!("{XSD_NS}{local}")).expect("xsd datatype IRI is valid")
}

/// A URI-object term for a value the projector's own typed JSON is SUPPOSED
/// to have already validated (an `urn:sophia:*` or
/// `http://mnemosyne.dev/machine#*` reference) — but bundle bytes are
/// untrusted I/O, not a compile-time invariant. **Fallible** (A4 review WRONG
/// #2): a malformed upstream value must become a loud `Result::Err`
/// propagated all the way to `BootHookOutcome::Failed`, never a `panic!` that
/// can kill the whole cell process (a panic that somehow escapes the A4
/// boot-hook's `spawn_blocking` timeout wrapper would otherwise vanish with
/// no `Failed` outcome — see `boot.rs`). Every caller here feeds this
/// genuinely bundle-derived data; see [`uri_term_const`] for the sibling used
/// where the value is this module's OWN fixed, compile-time-controlled
/// string.
fn uri_term(value: impl AsRef<str>) -> Result<Term, String> {
    let value = value.as_ref();
    NamedNode::new(value)
        .map(Term::Uri)
        .map_err(|error| format!("invalid IRI {value:?}: {error}"))
}

/// The infallible sibling of [`uri_term`] for the two call sites whose IRI is
/// built entirely from this module's OWN fixed, compile-time-controlled
/// strings — [`push_type`]'s `type_iri` (always `obs("X")`/`mach("X")`/
/// `prov("X")` for a hardcoded local name) and [`stamp_provenance`]'s literal
/// `OBS_PROJECTOR_AGENT` constant. Neither is ever bundle-supplied data, so
/// there is no "malformed upstream value" for these two to guard against; the
/// `expect` documents that invariant (a failure here is this module's OWN
/// bug, not a materializer-time data problem) rather than laundering a real
/// failure mode through an unqualified `unwrap`.
fn uri_term_const(value: &str) -> Term {
    Term::Uri(NamedNode::new(value).unwrap_or_else(|error| {
        panic!("mapping.rs's own constant IRI {value:?} is invalid: {error}")
    }))
}

fn text_term(value: impl Into<String>) -> Term {
    Term::Lit(Literal::new_simple_literal(value.into()))
}

fn integer_term(value: i64) -> Term {
    Term::Lit(Literal::new_typed_literal(
        value.to_string(),
        xsd("integer"),
    ))
}

/// `xsd:double` — matches A1's landed `emporium-observatory.golden.json`
/// (every USD/hours/rate predicate is declared `"datatype": "double"` there),
/// not `xsd:decimal` (no such `Datatype` variant exists) and not `xsd:float`
/// (the salience materializer's convention, unused by this pack).
fn double_term(value: f64) -> Term {
    Term::Lit(Literal::new_typed_literal(
        format!("{value:.6}"),
        xsd("double"),
    ))
}

fn boolean_term(value: bool) -> Term {
    Term::Lit(Literal::new_typed_literal(
        if value { "true" } else { "false" },
        xsd("boolean"),
    ))
}

/// Wraps an already-RFC-3339 string as `xsd:dateTime`. Every caller here
/// receives its datetime fields as JSON *strings* (the projector's own
/// epoch-ms → RFC-3339 conversion already happened upstream, before this
/// pure module ever sees the JSON), so there is no epoch-ms arithmetic to
/// reproduce for the lifecycle/billing MO shapes — only [`map_capture`]'s raw
/// wire envelope carries an epoch-ms integer (`ts`), converted by
/// [`epoch_ms_to_rfc3339`].
fn datetime_term(value: impl AsRef<str>) -> Term {
    Term::Lit(Literal::new_typed_literal(value.as_ref(), xsd("dateTime")))
}

fn push_type(triples: &mut Vec<Triple>, subject: &str, type_iri: &str) {
    triples.push((
        subject.to_string(),
        RDF_TYPE.to_string(),
        uri_term_const(type_iri),
    ));
}

/// Every rollup subject additionally carries `prov:wasAttributedTo
/// urn:sophia:agent:obs-projector`, `obs:objectType "<ObjectType>"`,
/// `obs:schemaVersion "<n>"^^xsd:integer` (§A.2) — the observer-relative
/// testimony stamp. Raw `CaptureEvent` triples do NOT carry this stamp (A1's
/// pack declares no such predicates on `CaptureEvent` — the stamp is a
/// rollup-only convention).
fn stamp_provenance(
    triples: &mut Vec<Triple>,
    subject: &str,
    object_type: &str,
    schema_version: u8,
) {
    triples.push((
        subject.to_string(),
        prov("wasAttributedTo"),
        uri_term_const(OBS_PROJECTOR_AGENT),
    ));
    triples.push((
        subject.to_string(),
        obs("objectType"),
        text_term(object_type),
    ));
    triples.push((
        subject.to_string(),
        obs("schemaVersion"),
        integer_term(i64::from(schema_version)),
    ));
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// **Fallible** (A4 review WRONG #2): `ms` is a bundle-supplied `i64` with no
/// upstream range guarantee — an out-of-range value must become a loud
/// `Result::Err`, never a `panic!` (see [`uri_term`]'s doc for why that
/// distinction matters for cell-boot safety).
fn epoch_ms_to_rfc3339(ms: i64) -> Result<String, String> {
    DateTime::<Utc>::from_timestamp_millis(ms)
        .map(|dt| dt.to_rfc3339_opts(SecondsFormat::Millis, true))
        .ok_or_else(|| format!("CaptureEvent.ts {ms} is outside the supported UTC range"))
}

// ---------------------------------------------------------------------------
// Raw CaptureEvent wire envelope (§A.2's raw lane; A1 pack class `CaptureEvent`)
// ---------------------------------------------------------------------------

/// The wire envelope `sophia-observatory-capture` contract defines
/// (`obs.golden.json`'s `canonical_key_order`/`required_fields`). Field names
/// match the wire JSON verbatim — no `#[serde(rename...)]` needed.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct CaptureEventJson {
    pub v: i64,
    pub event_id: String,
    pub ts: i64,
    pub witness: String,
    pub kind: String,
    pub principal: String,
    #[serde(default)]
    pub on_behalf_of: Option<String>,
    #[serde(default)]
    pub via_service: Option<String>,
    #[serde(default)]
    pub auth_method: Option<String>,
    pub client_class: String,
    #[serde(default)]
    pub graph_id: Option<String>,
    pub outcome: String,
    #[serde(default)]
    pub duration_ms: Option<i64>,
    pub seq: i64,
    pub payload: Json,
}

/// The 15 `lifecycle-90d` + `governance-395d` kinds v1 raw-materializes,
/// plus the two deliberately low-volume Hoja index events whose payloads
/// carry the thin evidence refs the Observatory filmstrip resolves. Every
/// other `interaction-30d` kind remains aggregate-only. Order starts with
/// the original spec prose
/// ("cell.spawn/reap/flush/boot/stop, sandbox.spawn/reap, graph.create/delete,
/// dep.state, alarm.fired/resolved, acl.change, obs.correction, obs.gap").
const RAW_MATERIALIZED_KINDS: &[&str] = &[
    "cell.spawn",
    "cell.reap",
    "cell.flush",
    "cell.boot",
    "cell.stop",
    "sandbox.spawn",
    "sandbox.reap",
    "graph.create",
    "graph.delete",
    "dep.state",
    "alarm.fired",
    "alarm.resolved",
    "acl.change",
    "obs.correction",
    "obs.gap",
    "test.beat",
    "test.verdict",
];

/// `true` iff `kind` is one of the raw-materialized lifecycle/governance
/// kinds or one of the two low-volume Hoja evidence-index kinds — the *only*
/// thing standing between v1's raw lane and accidentally windowed-replacing
/// high-volume interaction testimony (`request.http`, `chat.turn`,
/// `llm.call`, `capability.call`, …) into `:projection:obs:raw`.
pub(crate) fn is_raw_materialized_kind(kind: &str) -> bool {
    RAW_MATERIALIZED_KINDS.contains(&kind)
}

/// Map one raw `CaptureEvent` wire envelope to its `:projection:obs:raw`
/// triples (§A.2). Returns an empty `Vec` for every interaction kind except
/// the low-volume `test.beat`/`test.verdict` evidence index; high-volume
/// kinds exist solely as rollup aggregates. Subject
/// `urn:sophia:observatory:capture:{event_id}`; the typed payload rides as
/// one canonical `obs:payloadJson` literal (§A.2). No `prov:`/`obs:objectType`/
/// `obs:schemaVersion` stamp — that convention is rollup-only (A1's pack
/// declares no such predicates on `CaptureEvent`).
pub(crate) fn map_capture(ev: &CaptureEventJson) -> Result<Vec<Triple>, String> {
    if !is_raw_materialized_kind(&ev.kind) {
        return Ok(Vec::new());
    }

    let subject = format!("urn:sophia:observatory:capture:{}", ev.event_id);
    let mut triples = Vec::new();
    push_type(&mut triples, &subject, &obs("CaptureEvent"));
    triples.push((subject.clone(), obs("wireVersion"), integer_term(ev.v)));
    triples.push((
        subject.clone(),
        obs("capturedAt"),
        datetime_term(epoch_ms_to_rfc3339(ev.ts)?),
    ));
    triples.push((
        subject.clone(),
        obs("witness"),
        text_term(ev.witness.clone()),
    ));
    triples.push((subject.clone(), obs("kind"), text_term(ev.kind.clone())));
    triples.push((
        subject.clone(),
        obs("principal"),
        text_term(ev.principal.clone()),
    ));
    if let Some(value) = &ev.on_behalf_of {
        triples.push((subject.clone(), obs("onBehalfOf"), text_term(value.clone())));
    }
    if let Some(value) = &ev.via_service {
        triples.push((subject.clone(), obs("viaService"), text_term(value.clone())));
    }
    if let Some(value) = &ev.auth_method {
        triples.push((subject.clone(), obs("authMethod"), text_term(value.clone())));
    }
    triples.push((
        subject.clone(),
        obs("clientClass"),
        text_term(ev.client_class.clone()),
    ));
    if let Some(value) = &ev.graph_id {
        triples.push((subject.clone(), obs("graphId"), text_term(value.clone())));
    }
    triples.push((
        subject.clone(),
        obs("outcome"),
        text_term(ev.outcome.clone()),
    ));
    if let Some(value) = ev.duration_ms {
        triples.push((subject.clone(), obs("durationMs"), integer_term(value)));
    }
    triples.push((subject.clone(), obs("seq"), integer_term(ev.seq)));
    let payload_json = serde_json::to_string(&ev.payload).expect("payload re-serializes to JSON");
    triples.push((subject, obs("payloadJson"), text_term(payload_json)));
    Ok(triples)
}

// ---------------------------------------------------------------------------
// Lifecycle projection JSON mirrors (mirrors lifecycle-model.rs field-for-field)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct EngineJson {
    pub name: String,
    pub version: String,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct ProjectionExecutionJson {
    #[allow(dead_code)]
    pub definition: String,
    pub machine_runs_query_sha256: String,
    pub fleet_summary_query_sha256: String,
    pub engine: EngineJson,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct LifecycleSourceSnapshotJson {
    pub object_type: String,
    pub schema_version: u8,
    pub subject: String,
    pub source_system: String,
    pub source_authority: String,
    pub source_ref: String,
    pub captured_at: String,
    pub sha256: String,
    pub bytes: u64,
    pub line_count: u64,
    pub unique_event_count: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct ProjectionEvidenceJson {
    pub source_snapshot: String,
    #[allow(dead_code)]
    pub projection_definition: String,
    pub event_ids: Vec<String>,
    pub event_count: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct PlacementJson {
    pub pod_name: Option<String>,
    pub pod_uid: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct BootObservationJson {
    pub mode: Option<String>,
    pub ready_at: Option<String>,
    pub duration_ms: Option<u64>,
    pub hydrate_ms: Option<u64>,
    pub setup_ms: Option<u64>,
    pub snapshot_id: Option<String>,
    pub hydrated_bytes: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct OutstandingWorkJson {
    pub requests: Option<u64>,
    pub websockets: Option<u64>,
    pub jobs: Option<u64>,
    pub leases: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct StopObservationJson {
    pub reason: Option<String>,
    pub draining_at: Option<String>,
    pub quiesced_at: Option<String>,
    pub quiesce_timed_out: Option<bool>,
    pub idle_ms: Option<u64>,
    pub outstanding: OutstandingWorkJson,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct DurabilityObservationJson {
    pub final_flush: String,
    pub snapshot_id: Option<String>,
    pub published: Option<bool>,
    pub duration_ms: Option<u64>,
    pub bytes: Option<u64>,
    pub error_code: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct MachineRunJson {
    pub object_type: String,
    pub schema_version: u8,
    pub subject: String,
    pub as_of: String,
    pub run_id: String,
    pub machine: String,
    #[allow(dead_code)]
    pub machine_id: String,
    pub graph_id: String,
    pub run_state: String,
    pub started_at: Option<String>,
    pub first_observed_at: String,
    #[serde(default)]
    pub ended_at: Option<String>,
    pub runtime_ms: Option<u64>,
    pub placement: PlacementJson,
    pub boot: BootObservationJson,
    pub stop: StopObservationJson,
    pub durability: DurabilityObservationJson,
    pub evidence: ProjectionEvidenceJson,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct SpawnAttemptJson {
    pub object_type: String,
    pub schema_version: u8,
    pub subject: String,
    pub as_of: String,
    pub candidate_run_id: String,
    pub machine: String,
    #[allow(dead_code)]
    pub machine_id: String,
    pub graph_id: String,
    pub state: String,
    pub requested_at: String,
    pub last_observed_at: String,
    pub evidence: ProjectionEvidenceJson,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct FleetObservationJson {
    pub object_type: String,
    pub schema_version: u8,
    pub subject: String,
    pub environment_id: String,
    pub as_of: String,
    pub candidate_count: u64,
    pub run_count: u64,
    pub unmaterialized_attempts: u64,
    pub active_runs: u64,
    pub cold_or_starting_runs: u64,
    pub draining_runs: u64,
    pub failed_runs: u64,
    pub failed_attempts: u64,
    pub succeeded_runs: u64,
    pub created_runs: u64,
    pub failed_final_flushes: u64,
    pub unobserved_final_flushes: u64,
    pub cold_start_p50_ms: Option<u64>,
    pub cold_start_p95_ms: Option<u64>,
    pub hydrate_p50_ms: Option<u64>,
    pub hydrate_p95_ms: Option<u64>,
    pub final_flush_p50_ms: Option<u64>,
    pub final_flush_p95_ms: Option<u64>,
    pub observed_runtime_ms: u64,
    pub observed_through: Option<String>,
    pub source_snapshot: String,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct SequenceGapJson {
    pub witness: String,
    pub expected_seq: u64,
    pub observed_seq: u64,
    pub gap_count: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct ResourceShapeJson {
    pub memory_gib: f64,
    pub cpu_cores: f64,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct ComputeRatesJson {
    pub memory_gib_hour: f64,
    pub cpu_core_hour: f64,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct CapacityCoverageJson {
    pub materialized_runs: u64,
    pub runtime_observed_runs: u64,
    pub idle_terminated_runs: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct CapacityEstimateJson {
    pub object_type: String,
    pub schema_version: u8,
    pub subject: String,
    pub environment_id: String,
    pub as_of: String,
    pub target_idle_ttl_seconds: u64,
    pub resource_shape: ResourceShapeJson,
    pub rates_usd: ComputeRatesJson,
    pub coverage: CapacityCoverageJson,
    pub observed_runtime_hours: f64,
    pub estimated_runtime_hours: f64,
    pub estimated_compute_usd: f64,
    pub assumptions: Vec<String>,
    pub source_snapshot: String,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct LifecycleEvalJson {
    pub object_type: String,
    pub schema_version: u8,
    pub environment_id: String,
    pub evaluated_at: String,
    pub as_of: String,
    pub projected_through: Option<String>,
    pub outcome: String,
    pub computation_status: String,
    pub projection: ProjectionExecutionJson,
    pub source_snapshot: LifecycleSourceSnapshotJson,
    pub machine_runs: Vec<MachineRunJson>,
    pub spawn_attempts: Vec<SpawnAttemptJson>,
    pub fleet: FleetObservationJson,
    pub sequence_gaps: Vec<SequenceGapJson>,
    #[serde(default)]
    pub capacity_estimate: Option<CapacityEstimateJson>,
}

fn push_lifecycle_source_snapshot(triples: &mut Vec<Triple>, snap: &LifecycleSourceSnapshotJson) {
    let s = snap.subject.as_str();
    push_type(triples, s, &obs("SourceSnapshot"));
    push_type(triples, s, &prov("Entity"));
    triples.push((s.to_string(), obs("bytes"), integer_term(snap.bytes as i64)));
    triples.push((
        s.to_string(),
        obs("capturedAt"),
        datetime_term(&snap.captured_at),
    ));
    triples.push((
        s.to_string(),
        obs("lineCount"),
        integer_term(snap.line_count as i64),
    ));
    triples.push((
        s.to_string(),
        obs("uniqueEventCount"),
        integer_term(snap.unique_event_count as i64),
    ));
    triples.push((s.to_string(), obs("sha256"), text_term(snap.sha256.clone())));
    triples.push((
        s.to_string(),
        obs("sourceAuthority"),
        text_term(snap.source_authority.clone()),
    ));
    triples.push((
        s.to_string(),
        obs("sourceRef"),
        text_term(snap.source_ref.clone()),
    ));
    triples.push((
        s.to_string(),
        obs("sourceSystem"),
        text_term(snap.source_system.clone()),
    ));
    stamp_provenance(triples, s, &snap.object_type, snap.schema_version);
}

fn push_machine_run(triples: &mut Vec<Triple>, run: &MachineRunJson) -> Result<(), String> {
    let s = run.subject.as_str();
    push_type(triples, s, &mach("MachineRun"));
    push_type(triples, s, &prov("Entity"));
    triples.push((s.to_string(), mach("machine"), uri_term(&run.machine)?));
    triples.push((s.to_string(), mach("runState"), uri_term(&run.run_state)?));
    triples.push((s.to_string(), obs("asOf"), datetime_term(&run.as_of)));
    if let Some(value) = run.boot.duration_ms {
        triples.push((
            s.to_string(),
            obs("bootDurationMs"),
            integer_term(value as i64),
        ));
    }
    if let Some(value) = &run.boot.mode {
        triples.push((s.to_string(), obs("bootMode"), text_term(value.clone())));
    }
    if let Some(value) = &run.boot.ready_at {
        triples.push((s.to_string(), obs("bootReadyAt"), datetime_term(value)));
    }
    if let Some(value) = &run.boot.snapshot_id {
        triples.push((
            s.to_string(),
            obs("bootSnapshotId"),
            text_term(value.clone()),
        ));
    }
    if let Some(value) = &run.stop.draining_at {
        triples.push((s.to_string(), obs("drainingAt"), datetime_term(value)));
    }
    if let Some(value) = &run.ended_at {
        triples.push((s.to_string(), obs("endedAt"), datetime_term(value)));
    }
    triples.push((
        s.to_string(),
        obs("evidenceEventCount"),
        integer_term(run.evidence.event_count as i64),
    ));
    for id in &run.evidence.event_ids {
        triples.push((s.to_string(), obs("evidenceEventId"), text_term(id.clone())));
    }
    triples.push((
        s.to_string(),
        obs("finalFlush"),
        text_term(run.durability.final_flush.clone()),
    ));
    triples.push((
        s.to_string(),
        obs("firstObservedAt"),
        datetime_term(&run.first_observed_at),
    ));
    if let Some(value) = run.durability.bytes {
        triples.push((s.to_string(), obs("flushBytes"), integer_term(value as i64)));
    }
    if let Some(value) = run.durability.duration_ms {
        triples.push((
            s.to_string(),
            obs("flushDurationMs"),
            integer_term(value as i64),
        ));
    }
    if let Some(value) = &run.durability.error_code {
        triples.push((
            s.to_string(),
            obs("flushErrorCode"),
            text_term(value.clone()),
        ));
    }
    if let Some(value) = run.durability.published {
        triples.push((s.to_string(), obs("flushPublished"), boolean_term(value)));
    }
    if let Some(value) = &run.durability.snapshot_id {
        triples.push((
            s.to_string(),
            obs("flushSnapshotId"),
            text_term(value.clone()),
        ));
    }
    triples.push((
        s.to_string(),
        obs("graphId"),
        text_term(run.graph_id.clone()),
    ));
    if let Some(value) = run.boot.hydrate_ms {
        triples.push((s.to_string(), obs("hydrateMs"), integer_term(value as i64)));
    }
    if let Some(value) = run.boot.hydrated_bytes {
        triples.push((
            s.to_string(),
            obs("hydratedBytes"),
            integer_term(value as i64),
        ));
    }
    if let Some(value) = run.stop.idle_ms {
        triples.push((s.to_string(), obs("idleMs"), integer_term(value as i64)));
    }
    if let Some(value) = run.stop.outstanding.jobs {
        triples.push((
            s.to_string(),
            obs("outstandingJobs"),
            integer_term(value as i64),
        ));
    }
    if let Some(value) = run.stop.outstanding.leases {
        triples.push((
            s.to_string(),
            obs("outstandingLeases"),
            integer_term(value as i64),
        ));
    }
    if let Some(value) = run.stop.outstanding.requests {
        triples.push((
            s.to_string(),
            obs("outstandingRequests"),
            integer_term(value as i64),
        ));
    }
    if let Some(value) = run.stop.outstanding.websockets {
        triples.push((
            s.to_string(),
            obs("outstandingWebsockets"),
            integer_term(value as i64),
        ));
    }
    if let Some(value) = &run.placement.pod_name {
        triples.push((s.to_string(), obs("podName"), text_term(value.clone())));
    }
    if let Some(value) = &run.placement.pod_uid {
        triples.push((s.to_string(), obs("podUid"), text_term(value.clone())));
    }
    if let Some(value) = run.stop.quiesce_timed_out {
        triples.push((s.to_string(), obs("quiesceTimedOut"), boolean_term(value)));
    }
    if let Some(value) = &run.stop.quiesced_at {
        triples.push((s.to_string(), obs("quiescedAt"), datetime_term(value)));
    }
    triples.push((s.to_string(), obs("runId"), text_term(run.run_id.clone())));
    if let Some(value) = run.runtime_ms {
        triples.push((s.to_string(), obs("runtimeMs"), integer_term(value as i64)));
    }
    if let Some(value) = run.boot.setup_ms {
        triples.push((s.to_string(), obs("setupMs"), integer_term(value as i64)));
    }
    if let Some(value) = &run.started_at {
        triples.push((s.to_string(), obs("startedAt"), datetime_term(value)));
    }
    if let Some(value) = &run.stop.reason {
        triples.push((s.to_string(), obs("stopReason"), text_term(value.clone())));
    }
    triples.push((
        s.to_string(),
        prov("wasDerivedFrom"),
        uri_term(&run.evidence.source_snapshot)?,
    ));
    stamp_provenance(triples, s, &run.object_type, run.schema_version);
    Ok(())
}

fn push_spawn_attempt(triples: &mut Vec<Triple>, attempt: &SpawnAttemptJson) -> Result<(), String> {
    let s = attempt.subject.as_str();
    push_type(triples, s, &obs("SpawnAttempt"));
    push_type(triples, s, &prov("Entity"));
    triples.push((s.to_string(), mach("machine"), uri_term(&attempt.machine)?));
    triples.push((s.to_string(), obs("asOf"), datetime_term(&attempt.as_of)));
    triples.push((
        s.to_string(),
        obs("candidateRunId"),
        text_term(attempt.candidate_run_id.clone()),
    ));
    triples.push((
        s.to_string(),
        obs("evidenceEventCount"),
        integer_term(attempt.evidence.event_count as i64),
    ));
    for id in &attempt.evidence.event_ids {
        triples.push((s.to_string(), obs("evidenceEventId"), text_term(id.clone())));
    }
    triples.push((
        s.to_string(),
        obs("graphId"),
        text_term(attempt.graph_id.clone()),
    ));
    triples.push((
        s.to_string(),
        obs("lastObservedAt"),
        datetime_term(&attempt.last_observed_at),
    ));
    triples.push((
        s.to_string(),
        obs("requestedAt"),
        datetime_term(&attempt.requested_at),
    ));
    triples.push((
        s.to_string(),
        obs("state"),
        text_term(attempt.state.clone()),
    ));
    triples.push((
        s.to_string(),
        prov("wasDerivedFrom"),
        uri_term(&attempt.evidence.source_snapshot)?,
    ));
    stamp_provenance(triples, s, &attempt.object_type, attempt.schema_version);
    Ok(())
}

fn push_fleet(triples: &mut Vec<Triple>, fleet: &FleetObservationJson) -> Result<(), String> {
    let s = fleet.subject.as_str();
    push_type(triples, s, &obs("FleetObservation"));
    push_type(triples, s, &prov("Entity"));
    triples.push((
        s.to_string(),
        obs("activeRuns"),
        integer_term(fleet.active_runs as i64),
    ));
    triples.push((s.to_string(), obs("asOf"), datetime_term(&fleet.as_of)));
    triples.push((
        s.to_string(),
        obs("candidateCount"),
        integer_term(fleet.candidate_count as i64),
    ));
    triples.push((
        s.to_string(),
        obs("coldOrStartingRuns"),
        integer_term(fleet.cold_or_starting_runs as i64),
    ));
    if let Some(value) = fleet.cold_start_p50_ms {
        triples.push((
            s.to_string(),
            obs("coldStartP50Ms"),
            integer_term(value as i64),
        ));
    }
    if let Some(value) = fleet.cold_start_p95_ms {
        triples.push((
            s.to_string(),
            obs("coldStartP95Ms"),
            integer_term(value as i64),
        ));
    }
    triples.push((
        s.to_string(),
        obs("createdRuns"),
        integer_term(fleet.created_runs as i64),
    ));
    triples.push((
        s.to_string(),
        obs("drainingRuns"),
        integer_term(fleet.draining_runs as i64),
    ));
    triples.push((
        s.to_string(),
        obs("environment"),
        text_term(fleet.environment_id.clone()),
    ));
    triples.push((
        s.to_string(),
        obs("failedAttempts"),
        integer_term(fleet.failed_attempts as i64),
    ));
    triples.push((
        s.to_string(),
        obs("failedFinalFlushes"),
        integer_term(fleet.failed_final_flushes as i64),
    ));
    triples.push((
        s.to_string(),
        obs("failedRuns"),
        integer_term(fleet.failed_runs as i64),
    ));
    if let Some(value) = fleet.final_flush_p50_ms {
        triples.push((
            s.to_string(),
            obs("finalFlushP50Ms"),
            integer_term(value as i64),
        ));
    }
    if let Some(value) = fleet.final_flush_p95_ms {
        triples.push((
            s.to_string(),
            obs("finalFlushP95Ms"),
            integer_term(value as i64),
        ));
    }
    if let Some(value) = fleet.hydrate_p50_ms {
        triples.push((
            s.to_string(),
            obs("hydrateP50Ms"),
            integer_term(value as i64),
        ));
    }
    if let Some(value) = fleet.hydrate_p95_ms {
        triples.push((
            s.to_string(),
            obs("hydrateP95Ms"),
            integer_term(value as i64),
        ));
    }
    triples.push((
        s.to_string(),
        obs("observedRuntimeMs"),
        integer_term(fleet.observed_runtime_ms as i64),
    ));
    if let Some(value) = &fleet.observed_through {
        triples.push((s.to_string(), obs("observedThrough"), datetime_term(value)));
    }
    triples.push((
        s.to_string(),
        obs("runCount"),
        integer_term(fleet.run_count as i64),
    ));
    triples.push((
        s.to_string(),
        obs("succeededRuns"),
        integer_term(fleet.succeeded_runs as i64),
    ));
    triples.push((
        s.to_string(),
        obs("unmaterializedAttempts"),
        integer_term(fleet.unmaterialized_attempts as i64),
    ));
    triples.push((
        s.to_string(),
        obs("unobservedFinalFlushes"),
        integer_term(fleet.unobserved_final_flushes as i64),
    ));
    triples.push((
        s.to_string(),
        prov("wasDerivedFrom"),
        uri_term(&fleet.source_snapshot)?,
    ));
    stamp_provenance(triples, s, &fleet.object_type, fleet.schema_version);
    Ok(())
}

fn push_capacity_estimate(
    triples: &mut Vec<Triple>,
    capacity: &CapacityEstimateJson,
) -> Result<(), String> {
    let s = capacity.subject.as_str();
    push_type(triples, s, &obs("CapacityEstimate"));
    push_type(triples, s, &prov("Entity"));
    triples.push((s.to_string(), obs("asOf"), datetime_term(&capacity.as_of)));
    for assumption in &capacity.assumptions {
        triples.push((
            s.to_string(),
            obs("assumption"),
            text_term(assumption.clone()),
        ));
    }
    triples.push((
        s.to_string(),
        obs("coverageIdleTerminatedRuns"),
        integer_term(capacity.coverage.idle_terminated_runs as i64),
    ));
    triples.push((
        s.to_string(),
        obs("coverageMaterializedRuns"),
        integer_term(capacity.coverage.materialized_runs as i64),
    ));
    triples.push((
        s.to_string(),
        obs("coverageRuntimeObservedRuns"),
        integer_term(capacity.coverage.runtime_observed_runs as i64),
    ));
    triples.push((
        s.to_string(),
        obs("environment"),
        text_term(capacity.environment_id.clone()),
    ));
    triples.push((
        s.to_string(),
        obs("estimatedComputeUsd"),
        double_term(capacity.estimated_compute_usd),
    ));
    triples.push((
        s.to_string(),
        obs("estimatedRuntimeHours"),
        double_term(capacity.estimated_runtime_hours),
    ));
    triples.push((
        s.to_string(),
        obs("observedRuntimeHours"),
        double_term(capacity.observed_runtime_hours),
    ));
    triples.push((
        s.to_string(),
        obs("rateCpuCoreHourUsd"),
        double_term(capacity.rates_usd.cpu_core_hour),
    ));
    triples.push((
        s.to_string(),
        obs("rateMemoryGibHourUsd"),
        double_term(capacity.rates_usd.memory_gib_hour),
    ));
    triples.push((
        s.to_string(),
        obs("resourceCpuCores"),
        double_term(capacity.resource_shape.cpu_cores),
    ));
    triples.push((
        s.to_string(),
        obs("resourceMemoryGib"),
        double_term(capacity.resource_shape.memory_gib),
    ));
    triples.push((
        s.to_string(),
        obs("targetIdleTtlSeconds"),
        integer_term(capacity.target_idle_ttl_seconds as i64),
    ));
    triples.push((
        s.to_string(),
        prov("wasDerivedFrom"),
        uri_term(&capacity.source_snapshot)?,
    ));
    stamp_provenance(triples, s, &capacity.object_type, capacity.schema_version);
    Ok(())
}

/// `SequenceGap` carries no subject in the projector struct (§A.2) — mint
/// `urn:sophia:observatory:sequence-gap:sha256:{sha256(env|witness|expected|observed|gap|snapshot)}`,
/// the EXACT formula §A.2's prose and A1's landed pack comment both give
/// verbatim.
fn push_sequence_gap(
    triples: &mut Vec<Triple>,
    gap: &SequenceGapJson,
    environment_id: &str,
    source_snapshot: &str,
) -> Result<(), String> {
    let hash_input = format!(
        "{environment_id}|{}|{}|{}|{}|{source_snapshot}",
        gap.witness, gap.expected_seq, gap.observed_seq, gap.gap_count
    );
    let subject = format!(
        "urn:sophia:observatory:sequence-gap:sha256:{}",
        sha256_hex(hash_input.as_bytes())
    );
    let s = subject.as_str();
    push_type(triples, s, &obs("SequenceGap"));
    push_type(triples, s, &prov("Entity"));
    triples.push((
        s.to_string(),
        obs("environment"),
        text_term(environment_id.to_string()),
    ));
    triples.push((
        s.to_string(),
        obs("expectedSeq"),
        integer_term(gap.expected_seq as i64),
    ));
    triples.push((
        s.to_string(),
        obs("gapCount"),
        integer_term(gap.gap_count as i64),
    ));
    triples.push((
        s.to_string(),
        obs("observedSeq"),
        integer_term(gap.observed_seq as i64),
    ));
    triples.push((
        s.to_string(),
        obs("witness"),
        text_term(gap.witness.clone()),
    ));
    triples.push((
        s.to_string(),
        prov("wasDerivedFrom"),
        uri_term(source_snapshot)?,
    ));
    // SequenceGap carries no `object_type`/`schema_version` fields in the
    // projector struct at all (unlike every other class) — the stamp's
    // literal values are minted here, matching every sibling class's
    // schema_version=1 convention.
    stamp_provenance(triples, s, "SequenceGap", 1);
    Ok(())
}

/// The freshness object every face reads (§A.4): mint
/// `urn:sophia:observatory:projection-run:{env}:{evaluated_at}` — the EXACT
/// formula §A.2's prose and A1's landed pack comment both give verbatim.
fn push_projection_run(triples: &mut Vec<Triple>, eval: &LifecycleEvalJson) -> Result<(), String> {
    let subject = format!(
        "urn:sophia:observatory:projection-run:{}:{}",
        eval.environment_id, eval.evaluated_at
    );
    let s = subject.as_str();
    push_type(triples, s, &obs("ProjectionRun"));
    push_type(triples, s, &prov("Activity"));
    triples.push((s.to_string(), obs("asOf"), datetime_term(&eval.as_of)));
    triples.push((
        s.to_string(),
        obs("computationStatus"),
        text_term(eval.computation_status.clone()),
    ));
    triples.push((
        s.to_string(),
        obs("engineName"),
        text_term(eval.projection.engine.name.clone()),
    ));
    triples.push((
        s.to_string(),
        obs("engineVersion"),
        text_term(eval.projection.engine.version.clone()),
    ));
    triples.push((
        s.to_string(),
        obs("environment"),
        text_term(eval.environment_id.clone()),
    ));
    triples.push((
        s.to_string(),
        obs("evaluatedAt"),
        datetime_term(&eval.evaluated_at),
    ));
    triples.push((
        s.to_string(),
        obs("fleetSummaryQuerySha256"),
        text_term(eval.projection.fleet_summary_query_sha256.clone()),
    ));
    triples.push((
        s.to_string(),
        obs("machineRunsQuerySha256"),
        text_term(eval.projection.machine_runs_query_sha256.clone()),
    ));
    triples.push((
        s.to_string(),
        obs("outcome"),
        text_term(eval.outcome.clone()),
    ));
    if let Some(value) = &eval.projected_through {
        triples.push((s.to_string(), obs("projectedThrough"), datetime_term(value)));
    }
    // prov:wasDerivedFrom (multi) — every constituent subject this run
    // evaluated (the vocab's own gloss). Deduplicated + sorted so a
    // converged re-run's triple SET is identical (idempotency, §A.3).
    let mut derived: BTreeSet<String> = BTreeSet::new();
    derived.insert(eval.source_snapshot.subject.clone());
    derived.insert(eval.fleet.subject.clone());
    for run in &eval.machine_runs {
        derived.insert(run.subject.clone());
    }
    for attempt in &eval.spawn_attempts {
        derived.insert(attempt.subject.clone());
    }
    if let Some(capacity) = &eval.capacity_estimate {
        derived.insert(capacity.subject.clone());
    }
    for value in derived {
        triples.push((s.to_string(), prov("wasDerivedFrom"), uri_term(&value)?));
    }
    // obs:cursorHighWaterMark deliberately NOT emitted — see module doc.
    stamp_provenance(triples, s, &eval.object_type, eval.schema_version);
    Ok(())
}

/// Map one `LifecycleProjectionEvaluation` wrapper to its
/// `:projection:obs:rollups` triples (§A.2): the source snapshot, every
/// `MachineRunProjection`/`SpawnAttemptProjection`, the fleet observation,
/// the optional capacity estimate, every sequence gap (minted subject), and
/// the wrapper itself as `obs:ProjectionRun` (minted subject, the freshness
/// object §A.4 reads).
pub(crate) fn map_lifecycle(
    eval: &LifecycleEvalJson,
) -> Result<Vec<(GraphTarget, Vec<Triple>)>, String> {
    let mut triples = Vec::new();

    push_lifecycle_source_snapshot(&mut triples, &eval.source_snapshot);
    for run in &eval.machine_runs {
        push_machine_run(&mut triples, run)?;
    }
    for attempt in &eval.spawn_attempts {
        push_spawn_attempt(&mut triples, attempt)?;
    }
    push_fleet(&mut triples, &eval.fleet)?;
    if let Some(capacity) = &eval.capacity_estimate {
        push_capacity_estimate(&mut triples, capacity)?;
    }
    for gap in &eval.sequence_gaps {
        push_sequence_gap(
            &mut triples,
            gap,
            &eval.environment_id,
            &eval.source_snapshot.subject,
        )?;
    }
    push_projection_run(&mut triples, eval)?;

    Ok(vec![(GraphTarget::Rollups, triples)])
}

// ---------------------------------------------------------------------------
// Metric catalogue JSON mirror (definitions-only projector bundle member)
// ---------------------------------------------------------------------------

/// One authored metric definition after the platform projector has joined it
/// with the governance overlay and verified its declared SQL SHA. The JSON
/// uses the existing catalogue's camelCase wire names; this mirror stays pure
/// and has no graph/store authority on its own.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MetricDefinitionJson {
    pub subject: String,
    pub metric_id: String,
    pub label: String,
    pub description: String,
    pub pack: String,
    pub unit: String,
    pub lifecycle_status: String,
    pub version: u64,
    pub computation_status: String,
    pub catalog_binding: String,
    pub query_sha256: String,
    pub query_sha256_provenance: String,
    pub owner: String,
}

fn require_nonempty_metric_field(value: &str, field: &str) -> Result<(), String> {
    if value.trim().is_empty() {
        Err(format!("MetricDefinition.{field} must be non-empty"))
    } else {
        Ok(())
    }
}

/// Map the definitions-only catalogue into the reserved rollups graph. A
/// PENDING/spec-intent entity is rejected even though it could satisfy the
/// structural SHACL: only definitions already bound to a verified SQL digest
/// are publishable through this authority path.
pub(crate) fn map_metric_definition(
    definition: &MetricDefinitionJson,
) -> Result<Vec<Triple>, String> {
    if definition.metric_id.is_empty()
        || !definition
            .metric_id
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
    {
        return Err(format!(
            "MetricDefinition.metricId {:?} is not a lowercase identifier",
            definition.metric_id
        ));
    }
    let expected_subject = format!("urn:sophia:observatory:metric:{}", definition.metric_id);
    if definition.subject != expected_subject {
        return Err(format!(
            "MetricDefinition subject {:?} does not match metricId {:?}",
            definition.subject, definition.metric_id
        ));
    }
    if definition.catalog_binding != "definition-bound" {
        return Err(format!(
            "MetricDefinition {} has non-authoritative catalogBinding {:?}",
            definition.metric_id, definition.catalog_binding
        ));
    }
    if definition.query_sha256.len() != 64
        || !definition
            .query_sha256
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(format!(
            "MetricDefinition {} querySha256 is not lowercase SHA-256 hex",
            definition.metric_id
        ));
    }
    for (value, field) in [
        (&definition.label, "label"),
        (&definition.description, "description"),
        (&definition.pack, "pack"),
        (&definition.unit, "unit"),
        (&definition.lifecycle_status, "lifecycleStatus"),
        (&definition.computation_status, "computationStatus"),
        (&definition.query_sha256_provenance, "querySha256Provenance"),
    ] {
        require_nonempty_metric_field(value, field)?;
    }
    let version = i64::try_from(definition.version).map_err(|_| {
        format!(
            "MetricDefinition {} version overflows i64",
            definition.metric_id
        )
    })?;
    let owner = uri_term(&definition.owner)?;
    let subject = definition.subject.as_str();
    let mut triples = Vec::new();
    push_type(&mut triples, subject, &obs("MetricDefinition"));
    push_type(&mut triples, subject, &prov("Entity"));
    triples.push((
        subject.to_owned(),
        obs("metricId"),
        text_term(definition.metric_id.clone()),
    ));
    triples.push((
        subject.to_owned(),
        obs("label"),
        text_term(definition.label.clone()),
    ));
    triples.push((
        subject.to_owned(),
        rdfs("label"),
        text_term(definition.label.clone()),
    ));
    triples.push((
        subject.to_owned(),
        obs("description"),
        text_term(definition.description.clone()),
    ));
    triples.push((
        subject.to_owned(),
        obs("pack"),
        text_term(definition.pack.clone()),
    ));
    triples.push((
        subject.to_owned(),
        obs("unit"),
        text_term(definition.unit.clone()),
    ));
    triples.push((
        subject.to_owned(),
        obs("lifecycleStatus"),
        text_term(definition.lifecycle_status.clone()),
    ));
    triples.push((subject.to_owned(), obs("version"), integer_term(version)));
    triples.push((
        subject.to_owned(),
        obs("computationStatus"),
        text_term(definition.computation_status.clone()),
    ));
    triples.push((
        subject.to_owned(),
        obs("catalogBinding"),
        text_term(definition.catalog_binding.clone()),
    ));
    triples.push((
        subject.to_owned(),
        obs("querySha256"),
        text_term(definition.query_sha256.clone()),
    ));
    triples.push((
        subject.to_owned(),
        obs("querySha256Provenance"),
        text_term(definition.query_sha256_provenance.clone()),
    ));
    triples.push((subject.to_owned(), obs("owner"), owner.clone()));
    triples.push((subject.to_owned(), prov("wasAttributedTo"), owner));
    Ok(triples)
}

// ---------------------------------------------------------------------------
// Billing evaluation JSON mirrors (mirrors billing-model.rs field-for-field)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct MetricWindowJson {
    pub start: String,
    pub end: String,
    pub boundary: String,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct AnalysisEngineJson {
    pub name: String,
    pub version: String,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct BillingSourceSnapshotJson {
    pub object_type: String,
    pub schema_version: u8,
    pub subject: String,
    pub source_system: String,
    pub source_authority: String,
    pub source_ref: String,
    pub captured_at: String,
    pub sha256: String,
    pub bytes: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct MetricObservationJson {
    pub object_type: String,
    pub schema_version: u8,
    pub subject: String,
    pub logical_id: String,
    pub metric_definition: String,
    #[serde(default)]
    pub lifecycle_status: Option<String>,
    pub computation_status: String,
    pub value: u64,
    pub unit: String,
    pub window: MetricWindowJson,
    pub computed_at: String,
    #[serde(default)]
    pub observed_through: Option<String>,
    pub source_snapshot: String,
    pub source_event_count: u64,
    pub query_sha256: String,
    pub engine: AnalysisEngineJson,
    pub coverage: String,
    pub derived_from: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct MetricErrorBodyJson {
    pub code: String,
    pub retryable: bool,
    pub safe_message: String,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct BillingMetricSuccessJson {
    pub object_type: String,
    pub schema_version: u8,
    pub metric_definition: String,
    pub requested_window: MetricWindowJson,
    pub evaluated_at: String,
    pub outcome: String,
    pub engine: AnalysisEngineJson,
    pub source_snapshot: BillingSourceSnapshotJson,
    pub observation: MetricObservationJson,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct BillingMetricFailureJson {
    pub object_type: String,
    pub schema_version: u8,
    pub metric_definition: String,
    pub requested_window: MetricWindowJson,
    pub evaluated_at: String,
    pub outcome: String,
    pub engine: AnalysisEngineJson,
    #[serde(default)]
    pub source_snapshot: Option<BillingSourceSnapshotJson>,
    pub error: MetricErrorBodyJson,
}

/// The untagged Success/Failure union (`billing/model.rs`
/// `BillingMetricEvaluation`). Discriminated structurally on deserialize:
/// `Success` requires an `observation` key, `Failure` requires an `error`
/// key — the two shapes never both parse.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub(crate) enum BillingEvalJson {
    Success(Box<BillingMetricSuccessJson>),
    Failure(Box<BillingMetricFailureJson>),
}

fn push_billing_source_snapshot(triples: &mut Vec<Triple>, snap: &BillingSourceSnapshotJson) {
    let s = snap.subject.as_str();
    push_type(triples, s, &obs("SourceSnapshot"));
    push_type(triples, s, &prov("Entity"));
    triples.push((s.to_string(), obs("bytes"), integer_term(snap.bytes as i64)));
    triples.push((
        s.to_string(),
        obs("capturedAt"),
        datetime_term(&snap.captured_at),
    ));
    triples.push((s.to_string(), obs("sha256"), text_term(snap.sha256.clone())));
    triples.push((
        s.to_string(),
        obs("sourceAuthority"),
        text_term(snap.source_authority.clone()),
    ));
    triples.push((
        s.to_string(),
        obs("sourceRef"),
        text_term(snap.source_ref.clone()),
    ));
    triples.push((
        s.to_string(),
        obs("sourceSystem"),
        text_term(snap.source_system.clone()),
    ));
    stamp_provenance(triples, s, &snap.object_type, snap.schema_version);
}

fn push_metric_observation(
    triples: &mut Vec<Triple>,
    observation: &MetricObservationJson,
) -> Result<(), String> {
    let s = observation.subject.as_str();
    push_type(triples, s, &obs("MetricObservation"));
    push_type(triples, s, &prov("Entity"));
    triples.push((
        s.to_string(),
        obs("computationStatus"),
        text_term(observation.computation_status.clone()),
    ));
    triples.push((
        s.to_string(),
        obs("computedAt"),
        datetime_term(&observation.computed_at),
    ));
    triples.push((
        s.to_string(),
        obs("coverage"),
        text_term(observation.coverage.clone()),
    ));
    triples.push((
        s.to_string(),
        obs("engineName"),
        text_term(observation.engine.name.clone()),
    ));
    triples.push((
        s.to_string(),
        obs("engineVersion"),
        text_term(observation.engine.version.clone()),
    ));
    if let Some(value) = &observation.lifecycle_status {
        triples.push((
            s.to_string(),
            obs("lifecycleStatus"),
            text_term(value.clone()),
        ));
    }
    triples.push((
        s.to_string(),
        obs("logicalId"),
        uri_term(&observation.logical_id)?,
    ));
    triples.push((
        s.to_string(),
        obs("metricDefinition"),
        uri_term(&observation.metric_definition)?,
    ));
    if let Some(value) = &observation.observed_through {
        triples.push((s.to_string(), obs("observedThrough"), datetime_term(value)));
    }
    triples.push((
        s.to_string(),
        obs("querySha256"),
        text_term(observation.query_sha256.clone()),
    ));
    triples.push((
        s.to_string(),
        obs("sourceEventCount"),
        integer_term(observation.source_event_count as i64),
    ));
    triples.push((
        s.to_string(),
        obs("unit"),
        text_term(observation.unit.clone()),
    ));
    triples.push((
        s.to_string(),
        obs("value"),
        integer_term(observation.value as i64),
    ));
    triples.push((
        s.to_string(),
        obs("windowBoundary"),
        text_term(observation.window.boundary.clone()),
    ));
    triples.push((
        s.to_string(),
        obs("windowEnd"),
        datetime_term(&observation.window.end),
    ));
    triples.push((
        s.to_string(),
        obs("windowStart"),
        datetime_term(&observation.window.start),
    ));
    // prov:wasDerivedFrom (multi, required): source_snapshot + derived_from,
    // deduplicated + sorted (A1 pack: "MetricObservation.source_snapshot +
    // derived_from (multi)").
    let mut derived: BTreeSet<String> = BTreeSet::new();
    derived.insert(observation.source_snapshot.clone());
    for value in &observation.derived_from {
        derived.insert(value.clone());
    }
    for value in derived {
        triples.push((s.to_string(), prov("wasDerivedFrom"), uri_term(&value)?));
    }
    stamp_provenance(
        triples,
        s,
        &observation.object_type,
        observation.schema_version,
    );
    Ok(())
}

/// `MetricEvaluation` carries no subject in either projector struct (§A.2:
/// "subject-upsert (**minted**)", with no explicit formula given — unlike
/// `SequenceGap`/`ProjectionRun`, which the spec spells out verbatim). This
/// module mints `urn:sophia:observatory:metric-evaluation:{metric_id}:{evaluated_at}`,
/// mirroring `ProjectionRun`'s `{scope}:{evaluated_at}` shape (§A.4) with the
/// billing metric's own id — parsed from `metric_definition`'s trailing
/// segment (`urn:sophia:observatory:metric:billing_llm_dau_v1` →
/// `billing_llm_dau_v1`) — standing in for `ProjectionRun`'s
/// `environment_id`. A documented design choice, not a spec-given formula.
///
/// This bare `{metric_id}:{evaluated_at}` mint is the FAILURE-side subject
/// only: a failed evaluation produces exactly one packet per metric per
/// `evaluated_at` (the evaluator collapses every per-metric failure to a
/// single fail-honest packet), so the pair is unique there. Success-side
/// subjects go through [`metric_evaluation_success_subject`] instead — see
/// its own doc for the real-ledger collision this bare mint has.
fn metric_evaluation_subject(metric_definition: &str, evaluated_at: &str) -> String {
    let metric_id = metric_definition
        .rsplit(':')
        .next()
        .unwrap_or(metric_definition);
    format!("urn:sophia:observatory:metric-evaluation:{metric_id}:{evaluated_at}")
}

/// Success-side `MetricEvaluation` subject: the bare mint plus the produced
/// observation's own content-hash discriminator.
///
/// WRONG (caught 2026-07-21, first catch_up over a REAL canary-ledger bundle
/// — never by the fixture path, whose dry-run bundle carries an EMPTY
/// `billing[]`): the bare `{metric_id}:{evaluated_at}` mint assumed ONE
/// evaluation per metric per instant — true for the single-observation
/// billing family it was written against, FALSE for the capture-metric packs
/// riding the same `billing[]` array since metric-packs v1: a capture metric
/// emits one `MetricEvaluation` PER SLICE (`usage_leverage_ratio_v1` alone
/// emits 4, all sharing metric_id + evaluated_at). Every slice of a metric
/// then minted the SAME `prov:Activity` subject, each attaching its own
/// distinct `obs:producedObservation` — and the SHACL gate refused the whole
/// rollups lane (`MaxCount(1) not satisfied`, 20 violations at the real
/// ledger). That refusal was the gate working exactly as designed (loud
/// halt, no partial write, no forged rows) — the mint, not the gate, was
/// wrong.
///
/// FIX: one wire `MetricEvaluation` object = one RDF `prov:Activity`. The
/// discriminator is the observation's OWN content-hash subject suffix
/// (`urn:sophia:observatory:observation:sha256:{hex}` → `{hex}`) — already
/// unique per slice (`logical_id` is folded into that hash upstream, the
/// projector's `identity.rs`), already IRI-safe hex, and deterministic: an
/// unchanged re-evaluation reproduces the same observation hash and
/// therefore the same evaluation subject, keeping the rollups upsert
/// idempotent (re-apply → 0 ops). Failures keep the bare mint (no
/// observation exists to discriminate by, and they cannot collide — see
/// [`metric_evaluation_subject`]).
fn metric_evaluation_success_subject(
    metric_definition: &str,
    evaluated_at: &str,
    observation_subject: &str,
) -> String {
    let base = metric_evaluation_subject(metric_definition, evaluated_at);
    let discriminator = observation_subject
        .rsplit(':')
        .next()
        .unwrap_or(observation_subject);
    format!("{base}:{discriminator}")
}

fn push_metric_evaluation_success(
    triples: &mut Vec<Triple>,
    success: &BillingMetricSuccessJson,
) -> Result<(), String> {
    let subject = metric_evaluation_success_subject(
        &success.metric_definition,
        &success.evaluated_at,
        &success.observation.subject,
    );
    let s = subject.as_str();
    push_type(triples, s, &obs("MetricEvaluation"));
    push_type(triples, s, &prov("Activity"));
    triples.push((
        s.to_string(),
        obs("engineName"),
        text_term(success.engine.name.clone()),
    ));
    triples.push((
        s.to_string(),
        obs("engineVersion"),
        text_term(success.engine.version.clone()),
    ));
    triples.push((
        s.to_string(),
        obs("evaluatedAt"),
        datetime_term(&success.evaluated_at),
    ));
    triples.push((
        s.to_string(),
        obs("metricDefinition"),
        uri_term(&success.metric_definition)?,
    ));
    triples.push((
        s.to_string(),
        obs("outcome"),
        text_term(success.outcome.clone()),
    ));
    triples.push((
        s.to_string(),
        obs("producedObservation"),
        uri_term(&success.observation.subject)?,
    ));
    triples.push((
        s.to_string(),
        obs("requestedWindowBoundary"),
        text_term(success.requested_window.boundary.clone()),
    ));
    triples.push((
        s.to_string(),
        obs("requestedWindowEnd"),
        datetime_term(&success.requested_window.end),
    ));
    triples.push((
        s.to_string(),
        obs("requestedWindowStart"),
        datetime_term(&success.requested_window.start),
    ));
    triples.push((
        s.to_string(),
        prov("wasDerivedFrom"),
        uri_term(&success.source_snapshot.subject)?,
    ));
    stamp_provenance(triples, s, &success.object_type, success.schema_version);
    Ok(())
}

fn push_metric_evaluation_failure(
    triples: &mut Vec<Triple>,
    failure: &BillingMetricFailureJson,
) -> Result<(), String> {
    let subject = metric_evaluation_subject(&failure.metric_definition, &failure.evaluated_at);
    let s = subject.as_str();
    push_type(triples, s, &obs("MetricEvaluation"));
    push_type(triples, s, &prov("Activity"));
    triples.push((
        s.to_string(),
        obs("engineName"),
        text_term(failure.engine.name.clone()),
    ));
    triples.push((
        s.to_string(),
        obs("engineVersion"),
        text_term(failure.engine.version.clone()),
    ));
    triples.push((
        s.to_string(),
        obs("errorCode"),
        text_term(failure.error.code.clone()),
    ));
    triples.push((
        s.to_string(),
        obs("errorRetryable"),
        boolean_term(failure.error.retryable),
    ));
    triples.push((
        s.to_string(),
        obs("errorSafeMessage"),
        text_term(failure.error.safe_message.clone()),
    ));
    triples.push((
        s.to_string(),
        obs("evaluatedAt"),
        datetime_term(&failure.evaluated_at),
    ));
    triples.push((
        s.to_string(),
        obs("metricDefinition"),
        uri_term(&failure.metric_definition)?,
    ));
    triples.push((
        s.to_string(),
        obs("outcome"),
        text_term(failure.outcome.clone()),
    ));
    triples.push((
        s.to_string(),
        obs("requestedWindowBoundary"),
        text_term(failure.requested_window.boundary.clone()),
    ));
    triples.push((
        s.to_string(),
        obs("requestedWindowEnd"),
        datetime_term(&failure.requested_window.end),
    ));
    triples.push((
        s.to_string(),
        obs("requestedWindowStart"),
        datetime_term(&failure.requested_window.start),
    ));
    if let Some(snap) = &failure.source_snapshot {
        triples.push((
            s.to_string(),
            prov("wasDerivedFrom"),
            uri_term(&snap.subject)?,
        ));
    }
    stamp_provenance(triples, s, &failure.object_type, failure.schema_version);
    Ok(())
}

/// Map one `BillingMetricEvaluation` (the untagged Success/Failure union) to
/// its `:projection:obs:rollups` triples (§A.2). A failure NEVER manufactures
/// a synthetic zero `MetricObservation` (`contract/DECISIONS.md`) — only
/// `Success` emits one; `Failure` emits only the `MetricEvaluation` activity
/// (plus its source snapshot, if one was captured before the failure).
pub(crate) fn map_billing(
    eval: &BillingEvalJson,
) -> Result<Vec<(GraphTarget, Vec<Triple>)>, String> {
    let mut triples = Vec::new();
    match eval {
        BillingEvalJson::Success(success) => {
            push_billing_source_snapshot(&mut triples, &success.source_snapshot);
            push_metric_observation(&mut triples, &success.observation)?;
            push_metric_evaluation_success(&mut triples, success)?;
        }
        BillingEvalJson::Failure(failure) => {
            if let Some(snapshot) = &failure.source_snapshot {
                push_billing_source_snapshot(&mut triples, snapshot);
            }
            push_metric_evaluation_failure(&mut triples, failure)?;
        }
    }
    Ok(vec![(GraphTarget::Rollups, triples)])
}

// ---------------------------------------------------------------------------
// Shared test helpers (also used by `super::apply` in a later slice, if it
// wants a golden-comparable render — kept `pub(crate)` for that reuse).
// ---------------------------------------------------------------------------

/// One N-Triples line for `triple`, byte-compatible with the crate's own
/// `rdf::format_rdf_triple` convention (`<s> <p> o .`) but operating on
/// `emporium::terms::Triple` (`Term::as_nt()` already renders the object
/// part with oxigraph's own, pyoxigraph-byte-identical escaping).
pub(crate) fn nt_line(triple: &Triple) -> String {
    format!("<{}> <{}> {} .", triple.0, triple.1, triple.2.as_nt())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    const VALID_NDJSON: &str = include_str!("fixtures/valid.ndjson");
    const BILLING_SUCCESS_JSON: &str = include_str!("fixtures/billing_llm_dau_v1.ok.json");
    const LIFECYCLE_INPUT_JSON: &str = include_str!("fixtures/lifecycle_evaluation.input.json");
    const CAPTURE_GOLDEN_NT: &str = include_str!("fixtures/capture_events.golden.nt");
    const BILLING_GOLDEN_NT: &str = include_str!("fixtures/billing_metric_evaluation.golden.nt");
    const LIFECYCLE_GOLDEN_NT: &str = include_str!("fixtures/lifecycle_evaluation.golden.nt");

    fn capture_events() -> Vec<CaptureEventJson> {
        VALID_NDJSON
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| serde_json::from_str(line).expect("valid.ndjson line deserializes"))
            .collect()
    }

    fn nt_set(lines: &str) -> BTreeSet<String> {
        lines
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_string)
            .collect()
    }

    fn triples_to_nt_set(triples: &[Triple]) -> BTreeSet<String> {
        triples.iter().map(nt_line).collect()
    }

    // -- map_capture -----------------------------------------------------

    #[test]
    fn map_capture_materializes_only_the_bounded_raw_kind_register() {
        let events = capture_events();
        assert_eq!(
            events.len(),
            36,
            "the real fixture carries 36 CaptureEvent lines"
        );

        let materialized: Vec<&CaptureEventJson> = events
            .iter()
            .filter(|ev| {
                !map_capture(ev)
                    .expect("valid fixture event maps without error")
                    .is_empty()
            })
            .collect();
        // 26 of the fixture's 36 lines carry one of the 15 original whitelisted
        // lifecycle-90d/governance-395d KINDS (several kinds repeat across
        // multiple lines, e.g. 5 cell.stop lines); the other 10 lines carry
        // one of the 10 interaction-30d kinds and must map to nothing.
        assert_eq!(
            materialized.len(),
            26,
            "26 real lifecycle-90d+governance-395d lines in the fixture must materialize; \
             the other 10 are interaction-30d and must not"
        );
        for ev in &materialized {
            assert!(
                is_raw_materialized_kind(&ev.kind),
                "materialized event {} carries a non-whitelisted kind {}",
                ev.event_id,
                ev.kind
            );
        }

        // Direct negative control: a real interaction-30d line (request.http)
        // from the fixture must map to nothing.
        let interaction = events
            .iter()
            .find(|ev| ev.kind == "request.http")
            .expect("fixture carries a request.http line");
        assert!(map_capture(interaction)
            .expect("interaction event maps without error")
            .is_empty());

        // Hoja's two low-volume interaction kinds are explicit exceptions:
        // their payloadJson is the graph index for private, fat S3 evidence.
        let mut hoja_beat = interaction.clone();
        hoja_beat.kind = "test.beat".to_string();
        hoja_beat.payload = serde_json::json!({
            "run_id": "run-1",
            "scenario_id": "scenario-1",
            "beat_id": "beat-1",
            "engine": "camoufox",
            "operation": "type",
            "action_count": 1,
            "evidence_uri": "s3://evidence/hoja/run-1/scenario-1/beat-1/camoufox/manifest.json",
            "evidence_sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        });
        let mapped = map_capture(&hoja_beat).expect("Hoja beat maps without error");
        assert!(!mapped.is_empty());
        assert!(mapped.iter().any(|(_, predicate, term)| {
            predicate == &obs("payloadJson")
                && matches!(term, Term::Lit(value) if value.value().contains("evidence_uri"))
        }));
        assert!(!is_raw_materialized_kind("capability.call"));
    }

    #[test]
    fn map_capture_raw_subjects_use_only_lifecycle_and_governance_kinds() {
        for ev in capture_events() {
            for (subject, predicate, term) in
                map_capture(&ev).expect("valid fixture event maps without error")
            {
                assert!(
                    subject.starts_with("urn:sophia:observatory:capture:"),
                    "unexpected raw subject shape: {subject}"
                );
                if predicate == format!("{OBS_NS}kind") {
                    let Term::Lit(literal) = &term else {
                        panic!("obs:kind must be a literal");
                    };
                    assert!(is_raw_materialized_kind(literal.value()));
                }
            }
        }
    }

    #[test]
    fn map_capture_matches_golden_nt() {
        let mut all: Vec<Triple> = Vec::new();
        for ev in capture_events() {
            all.extend(map_capture(&ev).expect("valid fixture event maps without error"));
        }
        assert_eq!(triples_to_nt_set(&all), nt_set(CAPTURE_GOLDEN_NT));
    }

    // -- map_billing -------------------------------------------------------

    #[test]
    fn map_billing_matches_golden_nt() {
        let eval: BillingEvalJson =
            serde_json::from_str(BILLING_SUCCESS_JSON).expect("billing fixture deserializes");
        let mapped = map_billing(&eval).expect("valid billing fixture maps without error");
        assert_eq!(mapped.len(), 1);
        assert_eq!(mapped[0].0, GraphTarget::Rollups);
        assert_eq!(triples_to_nt_set(&mapped[0].1), nt_set(BILLING_GOLDEN_NT));
    }

    #[test]
    fn map_billing_rollup_subjects_are_sophia_urns() {
        let eval: BillingEvalJson =
            serde_json::from_str(BILLING_SUCCESS_JSON).expect("billing fixture deserializes");
        let mapped = map_billing(&eval).expect("valid billing fixture maps without error");
        for (subject, _predicate, _term) in &mapped[0].1 {
            assert!(
                subject.starts_with("urn:sophia:"),
                "billing rollup subject is not a urn:sophia:* URN: {subject}"
            );
        }
    }

    #[test]
    fn map_billing_failure_never_emits_a_synthetic_metric_observation() {
        let failure = BillingMetricFailureJson {
            object_type: "MetricEvaluation".to_string(),
            schema_version: 1,
            metric_definition: "urn:sophia:observatory:metric:billing_llm_dau_v1".to_string(),
            requested_window: MetricWindowJson {
                start: "2026-07-12T00:00:00Z".to_string(),
                end: "2026-07-13T00:00:00Z".to_string(),
                boundary: "[start,end)".to_string(),
            },
            evaluated_at: "2026-07-13T00:06:00Z".to_string(),
            outcome: "error".to_string(),
            engine: AnalysisEngineJson {
                name: "duckdb".to_string(),
                version: "1.5.4".to_string(),
            },
            source_snapshot: None,
            error: MetricErrorBodyJson {
                code: "source_read_failed".to_string(),
                retryable: true,
                safe_message: "The testimony snapshot could not be read.".to_string(),
            },
        };
        let mapped = map_billing(&BillingEvalJson::Failure(Box::new(failure)))
            .expect("valid failure fixture maps without error");
        assert_eq!(mapped.len(), 1);
        // No triple typed obs:MetricObservation may appear on a failure.
        let metric_observation_type = obs("MetricObservation");
        let has_metric_observation = mapped[0].1.iter().any(|(_, predicate, term)| {
            predicate == RDF_TYPE
                && matches!(term, Term::Uri(node) if node.as_str() == metric_observation_type)
        });
        assert!(
            !has_metric_observation,
            "a failed evaluation must never mint an obs:MetricObservation"
        );
    }

    // -- map_lifecycle -----------------------------------------------------

    #[test]
    fn map_lifecycle_matches_golden_nt() {
        let eval: LifecycleEvalJson =
            serde_json::from_str(LIFECYCLE_INPUT_JSON).expect("lifecycle fixture deserializes");
        let mapped = map_lifecycle(&eval).expect("valid lifecycle fixture maps without error");
        assert_eq!(mapped.len(), 1);
        assert_eq!(mapped[0].0, GraphTarget::Rollups);
        assert_eq!(triples_to_nt_set(&mapped[0].1), nt_set(LIFECYCLE_GOLDEN_NT));
    }

    #[test]
    fn map_lifecycle_rollup_subjects_are_sophia_urns() {
        let eval: LifecycleEvalJson =
            serde_json::from_str(LIFECYCLE_INPUT_JSON).expect("lifecycle fixture deserializes");
        let mapped = map_lifecycle(&eval).expect("valid lifecycle fixture maps without error");
        for (subject, _predicate, _term) in &mapped[0].1 {
            assert!(
                subject.starts_with("urn:sophia:"),
                "lifecycle rollup subject is not a urn:sophia:* URN: {subject}"
            );
        }
    }

    #[test]
    fn map_lifecycle_preserves_the_machine_core_urn_join_verbatim() {
        let eval: LifecycleEvalJson =
            serde_json::from_str(LIFECYCLE_INPUT_JSON).expect("lifecycle fixture deserializes");
        assert_eq!(eval.machine_runs.len(), 1);
        let run = &eval.machine_runs[0];
        // Byte-identical to sophia-machine-core.golden.json's MachineRun
        // subject_rule: urn:sophia:machine-run:{environmentId}:{runId}.
        assert_eq!(
            run.subject,
            format!(
                "urn:sophia:machine-run:{}:{}",
                eval.environment_id, run.run_id
            )
        );
        let mapped = map_lifecycle(&eval).expect("valid lifecycle fixture maps without error");
        assert!(mapped[0]
            .1
            .iter()
            .any(|(s, p, _)| s == &run.subject && p == &format!("{MACH_NS}machine")));
    }

    // -- cross-check against A1's REAL, landed SHACL pack -------------------
    //
    // A2 owns no SHACL gate itself (§A.7: the explicit pre-write SHACL check
    // is A3's job, `shacl_validator::validate_desired`). But `validate_desired`
    // is pure (no Store, no I/O — an in-memory rudof conformance check), so
    // nothing stops this test module from running it as an extra, honest
    // cross-check that the predicate/datatype/rdf_types choices above
    // actually conform to the ALREADY-LANDED `emporium-observatory` pack,
    // not just that they match its predicate names by inspection.

    #[test]
    fn emitted_triples_conform_to_the_landed_observatory_shacl_pack() {
        use crate::emporium::contract::get_vocabulary;
        use crate::emporium::shacl_validator::validate_desired;

        let contract =
            get_vocabulary("emporium-observatory").expect("emporium-observatory registered");

        let billing: BillingEvalJson =
            serde_json::from_str(BILLING_SUCCESS_JSON).expect("billing fixture deserializes");
        let billing_mapped =
            map_billing(&billing).expect("valid billing fixture maps without error");
        validate_desired(&billing_mapped[0].1, contract)
            .expect("billing rollup triples conform to the landed observatory SHACL shapes");

        let lifecycle: LifecycleEvalJson =
            serde_json::from_str(LIFECYCLE_INPUT_JSON).expect("lifecycle fixture deserializes");
        let lifecycle_mapped =
            map_lifecycle(&lifecycle).expect("valid lifecycle fixture maps without error");
        validate_desired(&lifecycle_mapped[0].1, contract)
            .expect("lifecycle rollup triples conform to the landed observatory SHACL shapes");

        let mut capture_triples: Vec<Triple> = Vec::new();
        for ev in capture_events() {
            capture_triples
                .extend(map_capture(&ev).expect("valid fixture event maps without error"));
        }
        validate_desired(&capture_triples, contract)
            .expect("raw CaptureEvent triples conform to the landed observatory SHACL shapes");
    }

    // -- GraphTarget ---------------------------------------------------------

    #[test]
    fn graph_target_resolves_the_two_reserved_projection_graphs() {
        assert_eq!(
            GraphTarget::Raw.graph_iri(),
            "urn:mnemosyne:local:graph:observatory:projection:obs:raw"
        );
        assert_eq!(
            GraphTarget::Rollups.graph_iri(),
            "urn:mnemosyne:local:graph:observatory:projection:obs:rollups"
        );
    }

    // -- malformed semantic values: Err, never panic (A4 review WRONG #2) ---

    #[test]
    fn uri_term_returns_err_not_panic_on_a_malformed_iri() {
        let result = uri_term("not a valid iri (has spaces and no scheme)");
        assert!(
            result.is_err(),
            "a malformed IRI must be a clean Err, never a panic"
        );
    }

    #[test]
    fn epoch_ms_to_rfc3339_returns_err_not_panic_when_out_of_chronos_representable_range() {
        let result = epoch_ms_to_rfc3339(i64::MAX);
        assert!(
            result.is_err(),
            "an out-of-range epoch-ms value must be a clean Err, never a panic"
        );
    }

    #[test]
    fn map_capture_returns_err_not_panic_on_an_out_of_range_timestamp() {
        // A real materialized-kind event (cell.spawn) with its `ts` corrupted
        // to an epoch-ms value chrono cannot represent as a UTC DateTime.
        let mut events = capture_events();
        let event = events
            .iter_mut()
            .find(|ev| ev.kind == "cell.spawn")
            .expect("fixture carries a cell.spawn line");
        event.ts = i64::MAX;
        let result = map_capture(event);
        assert!(
            result.is_err(),
            "a semantically malformed CaptureEvent.ts must be a clean Err, never a panic \
             (and never a silently-materialized garbage timestamp)"
        );
    }

    #[test]
    fn map_lifecycle_returns_err_not_panic_on_a_malformed_machine_iri() {
        let mut eval: LifecycleEvalJson =
            serde_json::from_str(LIFECYCLE_INPUT_JSON).expect("lifecycle fixture deserializes");
        // The projector is SUPPOSED to hand this module an already-IRI-shaped
        // `machine` reference (§A.2) — corrupt it to prove a malformed
        // upstream value fails loudly instead of panicking mid-materialize.
        eval.machine_runs[0].machine = "not a valid iri (has spaces)".to_string();
        let result = map_lifecycle(&eval);
        assert!(
            result.is_err(),
            "a semantically malformed MachineRun.machine IRI must be a clean Err, never a panic"
        );
    }

    #[test]
    fn map_billing_returns_err_not_panic_on_a_malformed_metric_definition_iri() {
        let mut eval: BillingEvalJson =
            serde_json::from_str(BILLING_SUCCESS_JSON).expect("billing fixture deserializes");
        let BillingEvalJson::Success(success) = &mut eval else {
            panic!("billing fixture is the Success variant");
        };
        success.metric_definition = "not a valid iri (has spaces)".to_string();
        let result = map_billing(&eval);
        assert!(
            result.is_err(),
            "a semantically malformed metric_definition IRI must be a clean Err, never a panic"
        );
    }

    // -- golden regeneration helper (documented, not run by default) --------
    //
    // The three golden .nt fixtures are GENERATED, not hand-typed: run each
    // `map_*` fn against its real input fixture, render every emitted triple
    // via `nt_line`, sort, and write. Regenerate with:
    //   cargo test -p garden observatory::mapping::tests::regen_goldens -- --ignored --nocapture
    // then inspect the diff before committing.
    #[test]
    #[ignore = "run manually to regenerate the golden .nt fixtures"]
    fn regen_goldens() {
        let mut capture_triples: Vec<Triple> = Vec::new();
        for ev in capture_events() {
            capture_triples
                .extend(map_capture(&ev).expect("valid fixture event maps without error"));
        }
        write_golden("capture_events.golden.nt", &capture_triples);

        let billing: BillingEvalJson =
            serde_json::from_str(BILLING_SUCCESS_JSON).expect("billing fixture deserializes");
        let billing_mapped =
            map_billing(&billing).expect("valid billing fixture maps without error");
        write_golden("billing_metric_evaluation.golden.nt", &billing_mapped[0].1);

        let lifecycle: LifecycleEvalJson =
            serde_json::from_str(LIFECYCLE_INPUT_JSON).expect("lifecycle fixture deserializes");
        let lifecycle_mapped =
            map_lifecycle(&lifecycle).expect("valid lifecycle fixture maps without error");
        write_golden("lifecycle_evaluation.golden.nt", &lifecycle_mapped[0].1);
    }

    #[cfg(test)]
    fn write_golden(name: &str, triples: &[Triple]) {
        let mut lines: Vec<String> = triples.iter().map(nt_line).collect();
        lines.sort();
        let path = format!(
            "{}/src/observatory/fixtures/{name}",
            env!("CARGO_MANIFEST_DIR")
        );
        std::fs::write(&path, format!("{}\n", lines.join("\n"))).expect("write golden fixture");
    }
}
