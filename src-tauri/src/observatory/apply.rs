//! Direct-on-store apply + durable freshness — §A.3 / §A.6 / §A.7 (A3) of
//! `plans/observatory-analysis-cell-spec-20260715.md` in the `sophia` hub
//! repo.
//!
//! This module owns the ONE thing A2's `mapping` module deliberately does
//! not: actually writing the mapped triples into the two reserved
//! `:projection:obs:*` graphs, direct-on-store. It builds on TWO existing,
//! already-tested primitives — never a bespoke write path:
//!
//! - [`crate::emporium::reconcile`] (`ClassScope`/`Placement`/`SpanKey` +
//!   `reconcile_class_validated`/`reconcile_classes_validated`) — the SAME
//!   survey→diff→apply machinery memory/salience/song already use
//!   (`ba1a97eccc83:src-tauri/src/emporium/reconcile.rs:181-376`). Both A3
//!   lanes are `reconcile_class*_validated` calls with an EXPLICIT
//!   `Some(contract)` (never `None`) — this IS the "explicit SHACL pre-write"
//!   §A.7 asks for: `reconcile_class_validated`/`reconcile_classes_validated`
//!   call `shacl_validator::validate_desired(desired, contract)` AFTER the
//!   diff, BEFORE any write, loud-halting (`Err("SHACL: …")`) with NO partial
//!   write on the first violation — proven by `reconcile.rs`'s own
//!   `shacl_seam_tests` module. Reusing the seam (not hand-rolling a second
//!   SHACL call) is itself the "explicit" part: nothing here relies on the
//!   Emporium generic ingest spine's automatic gate (which never runs, since
//!   that spine is never called — §A.7's whole point).
//! - The **`materialize_memory_record` per-subject upsert IDEA** (DELETE the
//!   subject's current triples, INSERT its new ones —
//!   `ba1a97eccc83:src-tauri/src/emporium/memory_applier.rs:245-268`) is
//!   "revived" here as [`partition_rollups_by_class`]'s per-class
//!   `ClassScope.subjects: Some(batch_subjects)` restriction: `class_diff`'s
//!   survey is `VALUES ?s { batch_subjects } … ?s a <class> ; ?p ?o`, so the
//!   DELETE/INSERT it renders touches exactly the batch's own subjects — the
//!   SAME "per-subject full replace" outcome, but diff-based (an unchanged
//!   subject emits zero ops, unlike `materialize_memory_record`'s
//!   unconditional DELETE+INSERT) so idempotency is free. The function
//!   itself is not called directly: it is `pub(super)` (visible only inside
//!   `emporium`), and re-deriving the same DELETE-then-INSERT shape through
//!   the reconcile seam keeps this module on the ONE apply primitive
//!   (`reconcile_class*_validated`) instead of mixing two.
//!
//! **Two lanes, two `Placement`s, never a third:**
//! - **raw** ([`apply_raw`]) — `ClassScope.subjects: None` (the WHOLE
//!   `obs:CaptureEvent` class span in `:projection:obs:raw`) diffed against
//!   the caller's `raw_snapshot` (already the sidecar's current-window
//!   content — this module does not window anything itself, §A.3's "full-
//!   window idempotent recompute"). A survey-then-diff `DELETE DATA`/
//!   `INSERT DATA` — never a blind `CLEAR NAMED`/`DROP NAMED` (`reconcile.rs`
//!   never emits either), so a converged re-run is 0 ops and disposing the
//!   raw graph out-of-band then re-running is a pure re-INSERT (byte-
//!   identical), not a semantically different code path.
//! - **rollups** ([`apply_rollups`]) — `ClassScope.subjects: Some(batch)` PER
//!   CLASS (9 classes, §A.2's table minus `CaptureEvent`), via
//!   [`partition_rollups_by_class`] + `reconcile_classes_validated` (ONE
//!   apply, ONE SHACL check over the full desired union). The freshness
//!   triple `obs:cursorHighWaterMark` — deliberately never emitted by A2's
//!   pure `mapping` module (it is caller-supplied, I/O-derived cursor state
//!   the pure mapper never touches) — is appended to the SAME
//!   `obs:ProjectionRun` subject `map_lifecycle` already minted, so it lands
//!   in the exact same `reconcile_classes_validated` call as every other
//!   rollup triple: "the SAME gated apply as the rollups" (§A.3), not a
//!   second write.
//!
//! **The bundle/raw_snapshot wire shape.** Production feeds this module from
//! the file contract §A.1 describes: the sidecar writes `/work/obs-bundle.json`
//! + `/work/raw-snapshot.ndjson` on a shared volume; gardend's boot hook (A4)
//! reads those bytes and calls [`catch_up`]. So `catch_up` takes `&str` JSON/
//! NDJSON text, not pre-parsed Rust structs — the real production interface,
//! and the one an external `tests/` integration crate can drive with the
//! REAL fixture bytes already checked in under `fixtures/` (A2's own
//! `LifecycleEvalJson`/`BillingEvalJson`/`CaptureEventJson` are `pub(crate)`,
//! invisible outside this crate; JSON text has no such restriction and is
//! what the wire contract actually carries). No fixture ships a literal
//! `obs-bundle.json` (the real sidecar's exact wrapper shape lives in
//! `platform-next`, outside every checkout available here), so
//! [`ObsBundleJson`]'s `{cursor_high_water_mark, lifecycle[], billing[]}`
//! envelope is this module's own documented design choice — analogous to
//! A2's documented `MetricEvaluation` subject-mint formula — wrapping A2's
//! own per-eval JSON shapes verbatim (`lifecycle`/`billing` array elements
//! deserialize with `mapping::LifecycleEvalJson`/`BillingEvalJson`, field-for-
//! field identical to the standalone fixtures A2 already tests against).

use std::collections::{BTreeMap, BTreeSet};

use oxigraph::model::Literal;
use oxigraph::store::Store;
use serde::Deserialize;

use crate::emporium::contract::{get_vocabulary, VocabularyContract};
use crate::emporium::reconcile::{
    reconcile_class_validated, reconcile_classes_validated, ClassScope, Placement, SpanKey,
};
use crate::emporium::shacl_validator::validate_desired;
use crate::emporium::terms::{Term, Triple, TripleDiff};
use crate::runtime_config::RDF_TYPE;

use super::graph_identity::{raw_graph_iri, rollups_graph_iri, GRAPH_ID};
use super::mapping::{
    map_billing, map_capture, map_lifecycle, map_metric_definition, BillingEvalJson,
    CaptureEventJson, LifecycleEvalJson, MetricDefinitionJson, MACH_NS, OBS_NS,
};

/// The registered `emporium-observatory` pack name (A1,
/// `src-tauri/src/emporium/vocabs.rs:374`) — the ONE contract both lanes
/// validate `desired` against, always `Some(..)`, never `None` (§A.7).
const VOCAB_NAME: &str = "emporium-observatory";

/// `/work/obs-bundle.json`'s parsed shape — see the module doc for why this
/// exact envelope is this module's own design choice, not a spec-given byte
/// format. `lifecycle`/`billing` elements are A2's own per-eval JSON shapes,
/// unmodified.
#[derive(Debug, Deserialize)]
struct ObsBundleJson {
    /// The sidecar's EFS-backed ledger-object-cache high-water mark (§A.3) —
    /// an OPAQUE string this module never interprets or computes, only
    /// carries through verbatim to `obs:cursorHighWaterMark`. The real value
    /// is "the max ledger object key processed"; that computation lives in
    /// the sidecar (outside every checkout available to this crate — §A.1:
    /// gardend stays free of AWS SDK / S3-key knowledge).
    cursor_high_water_mark: String,
    #[serde(default)]
    lifecycle: Vec<LifecycleEvalJson>,
    #[serde(default)]
    billing: Vec<BillingEvalJson>,
    /// Optional for rolling compatibility with a pre-F1 projector. Presence
    /// means a complete definitions-only catalogue and activates full-class
    /// reconciliation; absence leaves any already-published catalogue alone.
    #[serde(default)]
    metric_catalog: Option<MetricCatalogProjectionJson>,
}

#[derive(Debug, Deserialize)]
struct MetricCatalogProjectionJson {
    object_type: String,
    schema_version: u8,
    graph_id: String,
    definitions: Vec<MetricDefinitionJson>,
}

/// A `pub` face onto one lane's applied [`TripleDiff`] — `TripleDiff` itself
/// is `pub(crate)` (crate-internal), so an external `tests/` integration
/// crate cannot read `.adds`/`.removes` directly; this struct is the plain
/// counts [`catch_up`] hands back instead.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LaneDiff {
    pub added: usize,
    pub removed: usize,
}

impl LaneDiff {
    /// A converged re-run applies zero ops in this lane (§A.3's idempotency
    /// oracle: "re-run → empty TripleDiff, 0 added / 0 removed").
    pub fn is_empty(&self) -> bool {
        self.added == 0 && self.removed == 0
    }

    fn from_triple_diff(diff: &TripleDiff) -> Self {
        Self {
            added: diff.adds.len(),
            removed: diff.removes.len(),
        }
    }
}

/// [`catch_up`]'s result: the two lanes' applied diffs, `pub` so an external
/// `tests/` crate can assert on them directly.
#[derive(Debug, Clone, Copy, Default)]
pub struct CatchUpReport {
    pub raw: LaneDiff,
    pub rollups: LaneDiff,
}

/// Full-window idempotent catch-up (§A.3): map `bundle` (rollups) and
/// `raw_snapshot` (raw) through A2's pure `mapping` functions, then reconcile
/// each into its reserved `:projection:obs:*` graph via the shared
/// `reconcile_class*_validated` seam — SHACL-gated, diff-based, never a blind
/// `CLEAR`/`DROP`. `store` is a REAL gardend `Store` (direct-on-store,
/// bypassing the user authority gate the same legitimate way memory/salience/
/// song do — never `rdf_authority.rs`, never the Emporium generic ingest
/// spine). Returns `Err("SHACL: …")` with NO partial write on the first
/// conformance failure (the seam's own guarantee); any other `Err` is a
/// JSON-parse or SPARQL-execution failure, equally loud.
///
/// **Both lanes are fully mapped AND SHACL-validated BEFORE either lane is
/// written** (A4 review WRONG #2: the previous shape mapped+wrote `raw`
/// first, so a bad rollup could leave `raw` committed while `rollups` failed
/// to even map — a partial apply). `mapping::map_capture`/`map_lifecycle`/
/// `map_billing` are all fallible now (a malformed upstream IRI or an
/// out-of-range timestamp is a loud `Err`, never a `panic!`), and
/// `shacl_validator::validate_desired` is pure (no Store, no I/O), so both
/// checks can run to completion for BOTH lanes before `store` is touched at
/// all. `apply_raw`/`apply_rollups` still run their OWN internal SHACL check
/// too (via `reconcile_class*_validated`) — a harmless, cheap second pass,
/// kept so those two functions stay independently safe to call (as the A3
/// test suite already does) without relying on a caller having pre-validated.
pub fn catch_up(store: &Store, bundle: &str, raw_snapshot: &str) -> Result<CatchUpReport, String> {
    let bundle: ObsBundleJson = serde_json::from_str(bundle)
        .map_err(|error| format!("observatory bundle JSON: {error}"))?;
    let raw_events = parse_raw_snapshot(raw_snapshot)?;

    let contract = get_vocabulary(VOCAB_NAME)
        .ok_or_else(|| format!("{VOCAB_NAME} vocab pack is not registered (A1 must land first)"))?;

    // Pre-map both lanes (fallible — propagates a malformed upstream value as
    // a clean `Err`, before either lane is written).
    let raw_desired = map_raw_desired(&raw_events)?;
    let rollups_desired = rollups_desired(&bundle)?;

    // Pre-validate both lanes' FULL desired triple sets against the SAME
    // SHACL contract both `apply_*` calls use — a violation in EITHER lane
    // halts here, before `store` is touched by either.
    validate_desired(&raw_desired, contract)?;
    validate_desired(&rollups_desired, contract)?;

    let raw_diff = apply_raw(store, raw_desired, contract)?;
    let rollups_diff = apply_rollups(store, rollups_desired, contract)?;

    let report = CatchUpReport {
        raw: LaneDiff::from_triple_diff(&raw_diff),
        rollups: LaneDiff::from_triple_diff(&rollups_diff),
    };

    // MERGE-BOUNDARY resolution (feat/observatory-materializer × the
    // dirty-driven flush that landed in 7d4f34f): this module's writes go
    // through `emporium::reconcile::run_update` — raw `SparqlEvaluator`
    // straight against `&Store` — which bumps NEITHER `mark_rdf_store_written`
    // nor any `GraphPersistenceLease`, and `catch_up` does not run under
    // `emporium::write_gate` (the ingest-route choke point the durability
    // audit instrumented). Without this hook, `flush`'s per-store dirty-skip
    // (`cell_durability::store_epoch_to_backup`) judges the store clean after
    // its first backup, so the hot-cell periodic re-check's forced flush
    // silently no-ops and a freshly applied generation never reaches the
    // durable plane (caught by `observatory_gardend_process_hot_cell_
    // periodic_recheck_applies_a_newer_generation_without_restart`). Same
    // narrow-hook discipline as the other wholesale direct-on-store
    // materializer, `rdf_record_materializer.rs`: mark AFTER the writes have
    // landed, and only when something actually changed (a converged 0-delta
    // re-run wrote nothing, so there is nothing to publish).
    if !(report.raw.is_empty() && report.rollups.is_empty()) {
        crate::cell_durability::mark_rdf_store_written(store);
    }

    Ok(report)
}

/// `/work/raw-snapshot.ndjson` — one `CaptureEventJson` per non-empty line,
/// mirroring `mapping.rs`'s own test helper (`valid.ndjson` fixture shape).
fn parse_raw_snapshot(ndjson: &str) -> Result<Vec<CaptureEventJson>, String> {
    ndjson
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(|line| {
            serde_json::from_str(line).map_err(|error| format!("raw CaptureEvent line: {error}"))
        })
        .collect()
}

/// **raw lane mapping** (§A.2/§A.7, pure — no `Store`): every `events` line
/// mapped through [`map_capture`] (already filtered to the bounded raw-kind
/// register: lifecycle/governance plus Hoja's two low-volume evidence-index
/// events; every other interaction line maps to nothing). Fallible: a single
/// semantically malformed event (e.g. an
/// out-of-range timestamp) fails the WHOLE lane, before anything is written.
fn map_raw_desired(events: &[CaptureEventJson]) -> Result<Vec<Triple>, String> {
    let mut desired = Vec::new();
    for event in events {
        desired.extend(map_capture(event)?);
    }
    Ok(desired)
}

/// **raw lane — windowed-replace** (§A.2/§A.3/§A.7). `subjects: None` spans
/// the WHOLE `obs:CaptureEvent` class in `:projection:obs:raw`: the survey
/// reads every `CaptureEvent` currently in the graph, diffs it against the
/// already-mapped `desired` set, and applies only the delta. An event that
/// aged out of `desired` (vs. the previous boot's window) is a `removes`
/// entry; a converged re-run on the SAME window is 0 ops.
fn apply_raw(
    store: &Store,
    desired: Vec<Triple>,
    contract: &VocabularyContract,
) -> Result<TripleDiff, String> {
    let scope = ClassScope {
        placement: Placement::Named(raw_graph_iri()),
        key: SpanKey::Fixed {
            rdf_type: format!("{OBS_NS}CaptureEvent"),
        },
        graph_id_conjunct: None,
        subjects: None,
    };
    reconcile_class_validated(store, &scope, &desired, Some(contract))
}

/// **rollups lane mapping** (§A.2/§A.3/§A.7, pure — no `Store`): maps every
/// lifecycle/billing evaluation in `bundle`, appending the durable-cursor
/// freshness triple to each lifecycle eval's own `obs:ProjectionRun` subject
/// (the SAME apply as every other rollup triple — §A.3/§A.4). Fallible: a
/// single semantically malformed evaluation (e.g. an invalid upstream IRI)
/// fails the WHOLE lane, before anything is written.
fn rollups_desired(bundle: &ObsBundleJson) -> Result<Vec<Triple>, String> {
    let mut desired: Vec<Triple> = Vec::new();

    for eval in &bundle.lifecycle {
        let mapped = map_lifecycle(eval)?;
        for (_target, triples) in &mapped {
            desired.extend(triples.iter().cloned());
        }
        // §A.3's durable cursor: `mapping::push_projection_run` deliberately
        // never emits `obs:cursorHighWaterMark` (it is I/O-derived state the
        // pure mapper never sees) — attach it here, to the SAME minted
        // `obs:ProjectionRun` subject, so it rides the SAME apply.
        let projection_run_subject = find_projection_run_subject(&mapped)?;
        desired.push((
            projection_run_subject,
            format!("{OBS_NS}cursorHighWaterMark"),
            text_term(bundle.cursor_high_water_mark.clone()),
        ));
    }

    for eval in &bundle.billing {
        for (_target, triples) in map_billing(eval)? {
            desired.extend(triples);
        }
    }

    if let Some(catalog) = &bundle.metric_catalog {
        if catalog.object_type != "MetricCatalogProjection"
            || catalog.schema_version != 1
            || catalog.graph_id != GRAPH_ID
        {
            return Err(format!(
                "metric_catalog identity must be MetricCatalogProjection/v1 for graph {GRAPH_ID}"
            ));
        }
        if catalog.definitions.is_empty() {
            return Err("metric_catalog.definitions must not be empty".to_owned());
        }
        let mut subjects = BTreeSet::new();
        for definition in &catalog.definitions {
            if !subjects.insert(definition.subject.clone()) {
                return Err(format!(
                    "metric_catalog repeats subject {:?}",
                    definition.subject
                ));
            }
            desired.extend(map_metric_definition(definition)?);
        }
    }

    Ok(desired)
}

/// **rollups lane — subject-scoped upsert** (§A.2/§A.3/§A.7). Partitions the
/// already-mapped `desired` union by class ([`partition_rollups_by_class`])
/// and reconciles all classes in ONE `reconcile_classes_validated` call (one
/// SHACL check over the whole desired union, one apply).
fn apply_rollups(
    store: &Store,
    desired: Vec<Triple>,
    contract: &VocabularyContract,
) -> Result<TripleDiff, String> {
    let scopes = partition_rollups_by_class(&desired);
    reconcile_classes_validated(store, &scopes, Some(contract))
}

fn text_term(value: impl Into<String>) -> Term {
    Term::Lit(Literal::new_simple_literal(value.into()))
}

/// Find the one subject `map_lifecycle` typed `obs:ProjectionRun` in `mapped`
/// (exactly one per `LifecycleEvalJson` — [`push_projection_run`'s single
/// call in `mapping::map_lifecycle`]). Scanning the ALREADY-EMITTED triples
/// (rather than re-deriving the mint formula independently) means this
/// module can never drift from `mapping.rs`'s own subject, by construction —
/// no second formula to keep in sync.
fn find_projection_run_subject(
    mapped: &[(super::mapping::GraphTarget, Vec<Triple>)],
) -> Result<String, String> {
    let projection_run_type = format!("{OBS_NS}ProjectionRun");
    for (_target, triples) in mapped {
        for (subject, predicate, object) in triples {
            if predicate == RDF_TYPE {
                if let Term::Uri(node) = object {
                    if node.as_str() == projection_run_type {
                        return Ok(subject.clone());
                    }
                }
            }
        }
    }
    Err("map_lifecycle output carries no obs:ProjectionRun subject".to_string())
}

/// The 10 rollup classes' domain-specific `rdf:type` IRIs (§A.2's table, minus
/// `CaptureEvent` which is raw-only). Each MO type mints subjects under
/// exactly one of these — disjoint by construction (distinct URN prefixes
/// per §A.2) — so partitioning `desired` by "which of these types does this
/// subject carry" is a safe, total partition of the rollups desired set.
fn rollup_class_iris() -> [String; 10] {
    [
        format!("{MACH_NS}MachineRun"),
        format!("{OBS_NS}SourceSnapshot"),
        format!("{OBS_NS}SpawnAttempt"),
        format!("{OBS_NS}FleetObservation"),
        format!("{OBS_NS}CapacityEstimate"),
        format!("{OBS_NS}SequenceGap"),
        format!("{OBS_NS}ProjectionRun"),
        format!("{OBS_NS}MetricDefinition"),
        format!("{OBS_NS}MetricObservation"),
        format!("{OBS_NS}MetricEvaluation"),
    ]
}

/// Partition `desired` into one `(ClassScope, desired_subset)` pair per
/// rollup class present in this batch — the "materialize_memory_record-style
/// upsert" the module doc describes: each scope's `subjects` is exactly the
/// batch's own subjects for that class, so `reconcile_classes_validated`'s
/// per-class survey (`VALUES ?s {batch_subjects} … ?s a <class> ; ?p ?o`)
/// only ever touches subjects THIS batch restates — a sibling subject from a
/// PRIOR batch (not repeated here) never enters the survey and is left
/// untouched (§A.6 Direction 4: partial re-run leaves other subjects intact,
/// `MachineRun@T1→T2` supersedes only that one subject). MetricDefinition is
/// the deliberate exception: a present catalogue is complete and definitions-
/// only, so its scope is `subjects: None` and a removed definition disappears
/// from the discoverable current-state catalogue. An absent catalogue creates
/// no scope at all, preserving rolling compatibility with an older projector.
fn partition_rollups_by_class(desired: &[Triple]) -> Vec<(ClassScope, Vec<Triple>)> {
    let classes = rollup_class_iris();

    let mut subjects_by_class: BTreeMap<&str, BTreeSet<String>> = BTreeMap::new();
    for (subject, predicate, object) in desired {
        if predicate != RDF_TYPE {
            continue;
        }
        let Term::Uri(node) = object else { continue };
        if let Some(class_iri) = classes.iter().find(|c| c.as_str() == node.as_str()) {
            subjects_by_class
                .entry(class_iri.as_str())
                .or_default()
                .insert(subject.clone());
        }
    }

    let mut scopes = Vec::new();
    for class_iri in &classes {
        let Some(subjects) = subjects_by_class.get(class_iri.as_str()) else {
            continue;
        };
        if subjects.is_empty() {
            continue;
        }
        let desired_subset: Vec<Triple> = desired
            .iter()
            .filter(|(subject, _, _)| subjects.contains(subject))
            .cloned()
            .collect();
        scopes.push((
            ClassScope {
                placement: Placement::Named(rollups_graph_iri()),
                key: SpanKey::Fixed {
                    rdf_type: class_iri.clone(),
                },
                graph_id_conjunct: None,
                subjects: if class_iri == &format!("{OBS_NS}MetricDefinition") {
                    None
                } else {
                    Some(subjects.clone())
                },
            },
            desired_subset,
        ));
    }
    scopes
}
