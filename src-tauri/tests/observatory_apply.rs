//! Direct-on-store apply acceptance gates (A3, §A.3/§A.6/§A.7 of
//! `plans/observatory-analysis-cell-spec-20260715.md` in the `sophia` hub
//! repo).
//!
//! REAL on-disk gardend `Store` (`garden_lib::observatory::authority_harness::open_real_store`
//! — the exact function `rdf_service.rs` opens for every RDF request), REAL
//! `catch_up` (`garden_lib::observatory::apply::catch_up`), REAL fixture
//! bytes (`../src/observatory/fixtures/*`, the same files A2's own unit
//! tests already validate against). No mocks anywhere in this file.
//!
//! Continues A0's "Direction" numbering onto the materializer itself (§A.6):
//! Direction 2 (projector cannot clobber user/ux work), Direction 3 (raw
//! window cannot touch rollups), Direction 4 (rollup upsert is subject-
//! scoped) now run against the REAL `catch_up`, not just the authority gate.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use garden_lib::observatory::apply::catch_up;
use garden_lib::observatory::authority_harness::{execute_update_unchecked, open_real_store};
use garden_lib::observatory::graph_identity::{graph_subject, raw_graph_iri, rollups_graph_iri};

use oxigraph::model::Term as OxTerm;
use oxigraph::sparql::{QueryResults, SparqlEvaluator};
use oxigraph::store::Store;
use serde_json::Value;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Fixture bytes (REAL — the same files `mapping.rs`'s own unit tests use)
// ---------------------------------------------------------------------------

const LIFECYCLE_JSON: &str =
    include_str!("../src/observatory/fixtures/lifecycle_evaluation.input.json");
const BILLING_JSON: &str = include_str!("../src/observatory/fixtures/billing_llm_dau_v1.ok.json");
const RAW_NDJSON: &str = include_str!("../src/observatory/fixtures/valid.ndjson");
const APPLY_SOURCE: &str = include_str!("../src/observatory/apply.rs");

const MACH_NS: &str = "http://mnemosyne.dev/machine#";
const OBS_NS: &str = "http://mnemosyne.dev/observatory#";
const PROV_NS: &str = "http://www.w3.org/ns/prov#";

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn temp_graph_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "sophia-observatory-apply-{label}-{}",
        Uuid::new_v4()
    ));
    std::fs::create_dir_all(&dir).expect("create temp graph dir");
    dir
}

fn cleanup(dir: &Path) {
    let _ = std::fs::remove_dir_all(dir);
}

/// `/work/obs-bundle.json`'s wire shape (§A.3's own `ObsBundleJson` — see
/// `apply.rs`'s module doc): wraps A2's own per-eval JSON verbatim.
fn wrap_bundle(cursor: &str, lifecycle_jsons: &[&str], billing_jsons: &[&str]) -> String {
    format!(
        "{{\"cursor_high_water_mark\":{cursor:?},\"lifecycle\":[{}],\"billing\":[{}]}}",
        lifecycle_jsons.join(","),
        billing_jsons.join(","),
    )
}

fn full_bundle(cursor: &str) -> String {
    wrap_bundle(cursor, &[LIFECYCLE_JSON], &[BILLING_JSON])
}

fn empty_bundle(cursor: &str) -> String {
    wrap_bundle(cursor, &[], &[])
}

fn metric_definition(metric_id: &str) -> Value {
    serde_json::json!({
        "subject": format!("urn:sophia:observatory:metric:{metric_id}"),
        "metricId": metric_id,
        "label": format!("Metric {metric_id}"),
        "description": "A fixture measurement definition.",
        "pack": "fixture_v1",
        "unit": "events",
        "lifecycleStatus": "provisional",
        "version": 1,
        "computationStatus": "computable-from-ledger",
        "catalogBinding": "definition-bound",
        "querySha256": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        "querySha256Provenance": "fixture definition verified against fixture SQL",
        "owner": "urn:sophia:principal:fixture-steward"
    })
}

fn bundle_with_metric_catalog(cursor: &str, definitions: Vec<Value>) -> String {
    let mut bundle: Value = serde_json::from_str(&empty_bundle(cursor)).expect("bundle fixture");
    bundle["metric_catalog"] = serde_json::json!({
        "object_type": "MetricCatalogProjection",
        "schema_version": 1,
        "graph_id": "observatory",
        "definitions": definitions
    });
    bundle.to_string()
}

/// Every `(?s,?p,?o)` in `graph_iri`, rendered as a canonical N-Triples-style
/// line set — the byte-identical comparator for idempotency/disposability/
/// two-writer assertions.
fn graph_triples(store: &Store, graph_iri: &str) -> BTreeSet<String> {
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

fn count_distinct_typed(store: &Store, graph_iri: &str, type_iri: &str) -> usize {
    let query = format!(
        "SELECT (COUNT(DISTINCT ?s) AS ?n) WHERE {{ GRAPH <{graph_iri}> {{ ?s a <{type_iri}> }} }}"
    );
    let results = SparqlEvaluator::new()
        .parse_query(&query)
        .expect("parse count query")
        .on_store(store)
        .execute()
        .expect("execute count query");
    let QueryResults::Solutions(mut solutions) = results else {
        panic!("expected SELECT solutions")
    };
    let row = solutions
        .next()
        .expect("count query returns one row")
        .expect("row ok");
    match row.get("n") {
        Some(OxTerm::Literal(lit)) => lit.value().parse::<usize>().expect("count literal parses"),
        other => panic!("count query missing ?n, got {other:?}"),
    }
}

/// The single bound `?v` of `?s a <type_iri> ; <predicate_iri> ?v` in
/// `graph_iri` (panics if the row count is not exactly one — every caller
/// here targets a predicate declared `multi:false` on a class this test
/// applies exactly once).
fn single_value(store: &Store, graph_iri: &str, type_iri: &str, predicate_iri: &str) -> OxTerm {
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
    row.get("v").expect("?v bound").clone()
}

/// [`single_value`], asserting the bound term is a literal and returning its
/// plain value (no quotes/datatype wrapper).
fn single_literal(store: &Store, graph_iri: &str, type_iri: &str, predicate_iri: &str) -> String {
    match single_value(store, graph_iri, type_iri, predicate_iri) {
        OxTerm::Literal(lit) => lit.value().to_string(),
        other => panic!("{predicate_iri} is not a literal: {other:?}"),
    }
}

/// [`single_value`], asserting the bound term is a `NamedNode` and returning
/// its plain IRI (no `<…>` wrapper).
fn single_uri(store: &Store, graph_iri: &str, type_iri: &str, predicate_iri: &str) -> String {
    match single_value(store, graph_iri, type_iri, predicate_iri) {
        OxTerm::NamedNode(node) => node.as_str().to_string(),
        other => panic!("{predicate_iri} is not a NamedNode: {other:?}"),
    }
}

fn seed_probe(store: &Store, graph_iri: &str, label: &str) {
    let update = format!(
        "INSERT DATA {{ GRAPH <{graph_iri}> {{ <urn:sophia:observatory:probe:{label}> <http://mnemosyne.dev/observatory#objectType> \"Probe\" . }} }}"
    );
    execute_update_unchecked(store, &update).expect("seed probe writes");
}

// ---------------------------------------------------------------------------
// Gate 1 — apply the fixture bundle: per-class triple counts in both graphs
// ---------------------------------------------------------------------------

#[test]
fn observatory_apply_catch_up_applies_fixture_bundle_with_expected_per_class_counts() {
    let dir = temp_graph_dir("gate1-counts");
    let store = open_real_store(&dir).expect("open real store");

    let report =
        catch_up(&store, &full_bundle("cursor-gate1"), RAW_NDJSON).expect("catch_up applies");

    // First apply on a fresh store: pure INSERT, no removes in either lane.
    assert_eq!(report.raw.removed, 0, "fresh raw apply has no removes");
    assert_eq!(
        report.rollups.removed, 0,
        "fresh rollups apply has no removes"
    );
    assert!(report.raw.added > 0, "fresh raw apply adds triples");
    assert!(report.rollups.added > 0, "fresh rollups apply adds triples");

    let raw = raw_graph_iri();
    let rollups = rollups_graph_iri();

    // 26 of valid.ndjson's 36 lines carry one of the 15 whitelisted
    // lifecycle-90d/governance-395d kinds (mapping.rs's own
    // `map_capture_materializes_only_lifecycle_and_governance_kinds` proves
    // this count against the same fixture) — every distinct event_id mints a
    // distinct `urn:sophia:observatory:capture:{event_id}` subject.
    assert_eq!(
        count_distinct_typed(&store, &raw, &format!("{OBS_NS}CaptureEvent")),
        26
    );

    // Rollups: one subject per class, EXCEPT SourceSnapshot (lifecycle's
    // snapshot and billing's snapshot are two distinct subjects).
    assert_eq!(
        count_distinct_typed(&store, &rollups, &format!("{MACH_NS}MachineRun")),
        1
    );
    assert_eq!(
        count_distinct_typed(&store, &rollups, &format!("{OBS_NS}SpawnAttempt")),
        1
    );
    assert_eq!(
        count_distinct_typed(&store, &rollups, &format!("{OBS_NS}FleetObservation")),
        1
    );
    assert_eq!(
        count_distinct_typed(&store, &rollups, &format!("{OBS_NS}SequenceGap")),
        1
    );
    assert_eq!(
        count_distinct_typed(&store, &rollups, &format!("{OBS_NS}CapacityEstimate")),
        1
    );
    assert_eq!(
        count_distinct_typed(&store, &rollups, &format!("{OBS_NS}ProjectionRun")),
        1
    );
    assert_eq!(
        count_distinct_typed(&store, &rollups, &format!("{OBS_NS}MetricObservation")),
        1
    );
    assert_eq!(
        count_distinct_typed(&store, &rollups, &format!("{OBS_NS}MetricEvaluation")),
        1
    );
    assert_eq!(
        count_distinct_typed(&store, &rollups, &format!("{OBS_NS}SourceSnapshot")),
        2
    );

    cleanup(&dir);
}

// ---------------------------------------------------------------------------
// Gate 2 — idempotency: re-run on the SAME window converges to an empty diff
// ---------------------------------------------------------------------------

#[test]
fn observatory_apply_catch_up_reconverge_is_zero_ops_not_a_blind_clear() {
    let dir = temp_graph_dir("gate2-idempotent");
    let store = open_real_store(&dir).expect("open real store");

    let bundle = full_bundle("cursor-gate2");
    catch_up(&store, &bundle, RAW_NDJSON).expect("first catch_up applies");
    let raw_before = graph_triples(&store, &raw_graph_iri());
    let rollups_before = graph_triples(&store, &rollups_graph_iri());

    let report = catch_up(&store, &bundle, RAW_NDJSON).expect("second catch_up (reconverge)");

    assert!(
        report.raw.is_empty(),
        "reconverged raw apply must be 0 added / 0 removed, got {:?}",
        report.raw
    );
    assert!(
        report.rollups.is_empty(),
        "reconverged rollups apply must be 0 added / 0 removed, got {:?}",
        report.rollups
    );

    // Not-a-blind-CLEAR: the graph content survived the "converged" apply
    // byte-identical (a `CLEAR NAMED` followed by re-INSERT would ALSO
    // reconverge to the same content but would not be a zero-op diff, which
    // the assertion above already rules out; this is the direct content
    // check on top).
    assert_eq!(graph_triples(&store, &raw_graph_iri()), raw_before);
    assert_eq!(graph_triples(&store, &rollups_graph_iri()), rollups_before);

    cleanup(&dir);
}

#[test]
fn observatory_apply_metric_catalog_is_discoverable_full_class_and_idempotent() {
    let dir = temp_graph_dir("metric-catalog");
    let store = open_real_store(&dir).expect("open real store");
    let first = bundle_with_metric_catalog(
        "catalog-1",
        vec![
            metric_definition("metric_alpha_v1"),
            metric_definition("metric_beta_v1"),
        ],
    );
    let report = catch_up(&store, &first, "").expect("catalog applies");
    assert!(report.rollups.added > 0);
    let rollups = rollups_graph_iri();
    assert_eq!(
        count_distinct_typed(&store, &rollups, &format!("{OBS_NS}MetricDefinition")),
        2
    );

    let replay = catch_up(&store, &first, "").expect("catalog replay converges");
    assert!(
        replay.rollups.is_empty(),
        "catalog replay must be zero-delta"
    );

    // Presence means a complete definitions-only catalogue, so removing beta
    // from the next bundle removes its discoverable current-state projection.
    let second =
        bundle_with_metric_catalog("catalog-2", vec![metric_definition("metric_alpha_v1")]);
    let report = catch_up(&store, &second, "").expect("catalog replacement applies");
    assert!(report.rollups.removed > 0);
    assert_eq!(
        count_distinct_typed(&store, &rollups, &format!("{OBS_NS}MetricDefinition")),
        1
    );

    // Absence is rolling-compatible with an older projector and leaves the
    // already-published catalogue intact rather than interpreting absence as
    // authority to erase it.
    catch_up(&store, &empty_bundle("catalog-old-projector"), "")
        .expect("pre-F1 bundle remains compatible");
    assert_eq!(
        count_distinct_typed(&store, &rollups, &format!("{OBS_NS}MetricDefinition")),
        1
    );
    cleanup(&dir);
}

#[test]
fn observatory_apply_metric_catalog_rejects_pending_before_any_write() {
    let dir = temp_graph_dir("metric-catalog-pending");
    let store = open_real_store(&dir).expect("open real store");
    let mut pending = metric_definition("metric_pending_v1");
    pending["catalogBinding"] = Value::String("spec-intent-unbound".to_owned());
    pending["querySha256"] = Value::String("PENDING:metric_pending_v1".to_owned());
    let error = catch_up(
        &store,
        &bundle_with_metric_catalog("catalog-pending", vec![pending]),
        "",
    )
    .expect_err("PENDING catalogue entity must fail closed");
    assert!(
        error.contains("non-authoritative catalogBinding"),
        "{error}"
    );
    assert!(graph_triples(&store, &rollups_graph_iri()).is_empty());
    cleanup(&dir);
}

/// Static-grep companion: `apply.rs` never renders a `CLEAR`/`DROP` verb —
/// every write is a `reconcile_class*_validated` call, which only ever emits
/// `DELETE DATA`/`INSERT DATA` (`reconcile.rs::apply_diff`/`render_updates`).
/// Scans CODE lines only (skips `//`/`///` doc-comment lines, which legally
/// discuss "never a blind CLEAR NAMED" in prose).
#[test]
fn observatory_apply_rs_never_names_clear_or_drop() {
    let code_only: String = APPLY_SOURCE
        .lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        !code_only.contains("CLEAR ") && !code_only.contains("DROP "),
        "apply.rs code (excluding comments) must never render a blind CLEAR/DROP verb"
    );
}

// ---------------------------------------------------------------------------
// Gate 3 — disposability: drop the raw graph out-of-band, re-run, byte-identical
// ---------------------------------------------------------------------------

#[test]
fn observatory_apply_catch_up_raw_graph_is_disposable_and_replays_byte_identical() {
    let dir = temp_graph_dir("gate3-disposable");
    let store = open_real_store(&dir).expect("open real store");

    let bundle = full_bundle("cursor-gate3");
    catch_up(&store, &bundle, RAW_NDJSON).expect("first catch_up applies");
    let raw_before = graph_triples(&store, &raw_graph_iri());
    assert!(!raw_before.is_empty());

    // Drop the raw graph OUT OF BAND (direct store execute, not through
    // `catch_up` — simulates an operator/EFS-loss wipe, never something
    // `catch_up` itself does).
    execute_update_unchecked(&store, &format!("CLEAR GRAPH <{}>", raw_graph_iri()))
        .expect("out-of-band drop of the raw graph");
    assert!(graph_triples(&store, &raw_graph_iri()).is_empty());

    catch_up(&store, &bundle, RAW_NDJSON).expect("replay catch_up rebuilds the raw graph");
    let raw_after = graph_triples(&store, &raw_graph_iri());

    assert_eq!(
        raw_after, raw_before,
        "replay after an out-of-band drop must be byte-identical"
    );

    cleanup(&dir);
}

// ---------------------------------------------------------------------------
// Gate 4 — freshness: obs:projectedThrough == the fixture's own value
// ---------------------------------------------------------------------------

#[test]
fn observatory_apply_catch_up_freshness_and_cursor_triples_land_in_the_same_apply() {
    let dir = temp_graph_dir("gate4-freshness");
    let store = open_real_store(&dir).expect("open real store");

    catch_up(&store, &full_bundle("cursor-gate4-abc123"), RAW_NDJSON).expect("catch_up applies");

    let rollups = rollups_graph_iri();
    // `lifecycle_evaluation.input.json`'s own `projected_through` field —
    // `map_lifecycle` passes it through verbatim (mapping.rs's
    // `push_projection_run`); `catch_up` writes it in the SAME apply as
    // every other rollup triple (no separate freshness write).
    let projected_through = single_literal(
        &store,
        &rollups,
        &format!("{OBS_NS}ProjectionRun"),
        &format!("{OBS_NS}projectedThrough"),
    );
    assert_eq!(projected_through, "2026-07-15T09:00:00.018Z");

    // The durable cursor (§A.3) round-trips verbatim into the SAME
    // `obs:ProjectionRun` subject, in the SAME apply.
    let cursor = single_literal(
        &store,
        &rollups,
        &format!("{OBS_NS}ProjectionRun"),
        &format!("{OBS_NS}cursorHighWaterMark"),
    );
    assert_eq!(cursor, "cursor-gate4-abc123");

    cleanup(&dir);
}

// ---------------------------------------------------------------------------
// Gate 5 — DAU parity: obs:MetricObservation obs:value == 2
// ---------------------------------------------------------------------------

#[test]
fn observatory_apply_catch_up_dau_metric_observation_value_matches_billing_fixture() {
    let dir = temp_graph_dir("gate5-dau");
    let store = open_real_store(&dir).expect("open real store");

    catch_up(&store, &full_bundle("cursor-gate5"), RAW_NDJSON).expect("catch_up applies");

    let rollups = rollups_graph_iri();
    let value = single_literal(
        &store,
        &rollups,
        &format!("{OBS_NS}MetricObservation"),
        &format!("{OBS_NS}value"),
    );
    // `billing_llm_dau_v1.ok.json`'s own `observation.value` field (§A.2:
    // xsd:integer counts/ms).
    assert_eq!(value, "2");

    cleanup(&dir);
}

// ---------------------------------------------------------------------------
// Malformed bundle/raw-snapshot (A4 review WRONG #2 / MISSING) — a clean
// bounded `Err`, never a panic, and NO PARTIAL STORE WRITE in either graph —
// not merely at the mapping level (`mapping.rs`'s own unit tests already
// prove that), but end-to-end through the REAL `catch_up` against a REAL
// Store, proving the pre-map-both-lanes-before-either-write ordering holds.
// ---------------------------------------------------------------------------

#[test]
fn observatory_apply_catch_up_malformed_json_bundle_is_a_clean_err_not_a_panic() {
    let dir = temp_graph_dir("malformed-json-bundle");
    let store = open_real_store(&dir).expect("open real store");

    let result = catch_up(&store, "{not valid json", RAW_NDJSON);
    assert!(
        result.is_err(),
        "malformed bundle JSON must be a clean Err, never a panic"
    );

    assert!(
        graph_triples(&store, &raw_graph_iri()).is_empty(),
        "raw graph must stay empty"
    );
    assert!(
        graph_triples(&store, &rollups_graph_iri()).is_empty(),
        "rollups graph must stay empty"
    );

    cleanup(&dir);
}

#[test]
fn observatory_apply_catch_up_malformed_json_raw_snapshot_is_a_clean_err_not_a_panic() {
    let dir = temp_graph_dir("malformed-json-raw");
    let store = open_real_store(&dir).expect("open real store");

    let result = catch_up(
        &store,
        &full_bundle("cursor-malformed-raw"),
        "{not valid json",
    );
    assert!(
        result.is_err(),
        "malformed raw-snapshot JSON must be a clean Err, never a panic"
    );

    assert!(
        graph_triples(&store, &raw_graph_iri()).is_empty(),
        "raw graph must stay empty"
    );
    assert!(
        graph_triples(&store, &rollups_graph_iri()).is_empty(),
        "rollups graph must stay empty — mapping the VALID rollups lane must not have written \
         anything before the malformed raw lane failed to even parse (pre-map-both-lanes-before-\
         either-write)"
    );

    cleanup(&dir);
}

/// A syntactically valid but semantically malformed rollups lane (an invalid
/// upstream `machine` IRI) must fail loudly with NO partial write — not even
/// to the (independently valid) raw lane. Proves the A4 review's demanded
/// ordering: BOTH lanes are mapped (and SHACL-validated) before EITHER lane
/// is written, not "map+write raw, then map+write rollups".
#[test]
fn observatory_apply_catch_up_semantically_malformed_machine_iri_is_a_clean_err_and_writes_nothing()
{
    let dir = temp_graph_dir("malformed-machine-iri");
    let store = open_real_store(&dir).expect("open real store");

    let mut lifecycle: Value =
        serde_json::from_str(LIFECYCLE_JSON).expect("parse lifecycle fixture");
    lifecycle["machine_runs"][0]["machine"] =
        Value::String("not a valid iri (has spaces)".to_string());
    let bundle = wrap_bundle(
        "cursor-malformed-machine",
        &[&lifecycle.to_string()],
        &[BILLING_JSON],
    );

    let result = catch_up(&store, &bundle, RAW_NDJSON);
    assert!(
        result.is_err(),
        "a semantically malformed machine IRI must be a clean Err, never a panic"
    );

    assert!(
        graph_triples(&store, &raw_graph_iri()).is_empty(),
        "the raw lane (independently valid) must NOT have been written — both lanes are \
         pre-mapped/validated before either apply runs"
    );
    assert!(
        graph_triples(&store, &rollups_graph_iri()).is_empty(),
        "the rollups graph must stay empty on a mapping failure"
    );

    cleanup(&dir);
}

/// A semantically malformed raw `CaptureEvent.ts` (out of chrono's
/// representable range) must fail the WHOLE catch_up loudly, leaving the
/// (independently valid) rollups lane unwritten too.
#[test]
fn observatory_apply_catch_up_semantically_malformed_capture_timestamp_is_a_clean_err_and_writes_nothing(
) {
    let dir = temp_graph_dir("malformed-capture-ts");
    let store = open_real_store(&dir).expect("open real store");

    // Corrupt the `cell.spawn` line's `ts` to an out-of-range epoch-ms value
    // (a real materialized kind, so `map_capture` actually attempts the
    // epoch conversion rather than short-circuiting on kind first).
    let corrupted_raw = RAW_NDJSON.replacen(
        "\"event_id\":\"01J00000000000000000000009\",\"ts\":1783890000009",
        "\"event_id\":\"01J00000000000000000000009\",\"ts\":9223372036854775807",
        1,
    );
    assert_ne!(
        corrupted_raw, RAW_NDJSON,
        "the target line must actually be present in the fixture"
    );

    let result = catch_up(&store, &full_bundle("cursor-malformed-ts"), &corrupted_raw);
    assert!(
        result.is_err(),
        "an out-of-range CaptureEvent.ts must be a clean Err, never a panic"
    );

    assert!(
        graph_triples(&store, &raw_graph_iri()).is_empty(),
        "the raw graph must stay empty on a mapping failure"
    );
    assert!(
        graph_triples(&store, &rollups_graph_iri()).is_empty(),
        "the rollups lane (independently valid) must NOT have been written — both lanes are \
         pre-mapped/validated before either apply runs"
    );

    cleanup(&dir);
}

// ---------------------------------------------------------------------------
// Direction 2 (§A.6) — the projector cannot clobber :user:rdf / :ux:config
// ---------------------------------------------------------------------------

#[test]
fn observatory_apply_catch_up_direction_2_cannot_clobber_user_or_ux_work() {
    let dir = temp_graph_dir("direction2");
    let store = open_real_store(&dir).expect("open real store");

    let user_graph = format!("{}:user:rdf", graph_subject());
    let ux_graph = format!("{}:ux:config", graph_subject());
    seed_probe(&store, &user_graph, "d2-user");
    seed_probe(&store, &ux_graph, "d2-ux");
    let user_before = graph_triples(&store, &user_graph);
    let ux_before = graph_triples(&store, &ux_graph);
    assert!(!user_before.is_empty() && !ux_before.is_empty());

    catch_up(&store, &full_bundle("cursor-d2"), RAW_NDJSON).expect("catch_up applies");

    assert_eq!(
        graph_triples(&store, &user_graph),
        user_before,
        ":user:rdf must be untouched"
    );
    assert_eq!(
        graph_triples(&store, &ux_graph),
        ux_before,
        ":ux:config must be untouched"
    );

    // Static-grep companion (§A.6): every `Placement::Named(` call site in
    // `apply.rs` targets ONLY `raw_graph_iri()`/`rollups_graph_iri()` — never
    // a hand-rolled or user/ux graph IRI.
    let named_targets: Vec<&str> = APPLY_SOURCE
        .match_indices("Placement::Named(")
        .map(|(idx, _)| {
            let start = idx + "Placement::Named(".len();
            let rest = &APPLY_SOURCE[start..];
            // Paren-balance to the MATCHING close (the target itself is a
            // call like `raw_graph_iri()`, which owns its own inner pair).
            let mut depth = 1i32;
            let mut end = None;
            for (offset, ch) in rest.char_indices() {
                match ch {
                    '(' => depth += 1,
                    ')' => {
                        depth -= 1;
                        if depth == 0 {
                            end = Some(offset);
                            break;
                        }
                    }
                    _ => {}
                }
            }
            let close = end.expect("Placement::Named( has a balanced closing paren");
            rest[..close].trim()
        })
        .collect();
    assert!(
        !named_targets.is_empty(),
        "expected at least one Placement::Named( call site"
    );
    for target in named_targets {
        assert!(
            target == "raw_graph_iri()" || target == "rollups_graph_iri()",
            "apply.rs targets an unexpected named graph: {target}"
        );
    }

    cleanup(&dir);
}

// ---------------------------------------------------------------------------
// Direction 3 (§A.6) — a raw windowed-replace cannot touch rollups
// ---------------------------------------------------------------------------

#[test]
fn observatory_apply_catch_up_direction_3_raw_window_cannot_touch_rollups() {
    let dir = temp_graph_dir("direction3");
    let store = open_real_store(&dir).expect("open real store");

    catch_up(&store, &full_bundle("cursor-d3"), RAW_NDJSON)
        .expect("first catch_up materializes rollups + raw");
    let rollups_before = graph_triples(&store, &rollups_graph_iri());
    assert!(!rollups_before.is_empty());
    assert_eq!(
        count_distinct_typed(&store, &raw_graph_iri(), &format!("{OBS_NS}CaptureEvent")),
        26
    );

    // A raw-only windowed-replace: an EMPTY bundle (no lifecycle/billing —
    // `apply_rollups` short-circuits to a true no-op) with an EMPTY raw
    // snapshot (every previously-materialized CaptureEvent has aged out of
    // the current window).
    let report = catch_up(&store, &empty_bundle("cursor-d3-aged-out"), "")
        .expect("aging-out catch_up applies");

    assert!(
        report.rollups.is_empty(),
        "an empty bundle must be a true rollups no-op, got {:?}",
        report.rollups
    );
    assert_eq!(
        count_distinct_typed(&store, &raw_graph_iri(), &format!("{OBS_NS}CaptureEvent")),
        0,
        "the raw window must have aged every CaptureEvent out"
    );
    assert_eq!(
        graph_triples(&store, &rollups_graph_iri()),
        rollups_before,
        "the rollups graph must be byte-identical after a raw-only windowed-replace"
    );

    cleanup(&dir);
}

// ---------------------------------------------------------------------------
// Direction 4 (§A.6) — rollup upsert is subject-scoped
// ---------------------------------------------------------------------------

fn lifecycle_t2_json() -> String {
    let mut value: Value =
        serde_json::from_str(LIFECYCLE_JSON).expect("parse T1 lifecycle fixture");
    // A second, distinct-content-hash FleetObservation subject (§A.6
    // Direction 4: "two FleetObservations with distinct content hashes both
    // survive a partial re-run").
    value["fleet"]["subject"] = Value::String(
        "urn:sophia:observatory:fleet-observation:sha256:t2t2t2t2t2t2t2t2t2t2t2t2t2t2t2t2t2t2t2t2t2t2t2t2t2t2t2t2t2t2t2t2"
            .to_string(),
    );
    // The SAME MachineRun subject (unchanged run_id/machine/graph_id), moved
    // to a later `as_of` with a different `run_state` — must SUPERSEDE, not
    // duplicate.
    value["machine_runs"][0]["as_of"] = Value::String("2026-07-15T13:00:00.000Z".to_string());
    value["machine_runs"][0]["run_state"] = Value::String(format!("{MACH_NS}Draining"));
    value.to_string()
}

#[test]
fn observatory_apply_catch_up_direction_4_rollup_upsert_is_subject_scoped() {
    let dir = temp_graph_dir("direction4");
    let store = open_real_store(&dir).expect("open real store");
    let rollups = rollups_graph_iri();

    // T1.
    let bundle_t1 = wrap_bundle("cursor-d4-t1", &[LIFECYCLE_JSON], &[]);
    catch_up(&store, &bundle_t1, "").expect("T1 catch_up applies");
    let machine_run_subject = "urn:sophia:machine-run:cloud-2-local:01J0000000000000000000000R";
    assert_eq!(
        single_uri(
            &store,
            &rollups,
            &format!("{MACH_NS}MachineRun"),
            &format!("{MACH_NS}runState")
        ),
        format!("{MACH_NS}Succeeded"),
    );

    // T2: a partial re-run — a NEW FleetObservation subject + the SAME
    // MachineRun subject at a later as_of/run_state.
    let lifecycle_t2 = lifecycle_t2_json();
    let bundle_t2 = wrap_bundle("cursor-d4-t2", &[lifecycle_t2.as_str()], &[]);
    catch_up(&store, &bundle_t2, "").expect("T2 catch_up applies");

    // Both FleetObservation subjects survive (distinct content hashes).
    assert_eq!(
        count_distinct_typed(&store, &rollups, &format!("{OBS_NS}FleetObservation")),
        2
    );

    // The MachineRun subject was SUPERSEDED, not duplicated: still exactly
    // one MachineRun subject overall, and its runState/asOf reflect ONLY T2.
    assert_eq!(
        count_distinct_typed(&store, &rollups, &format!("{MACH_NS}MachineRun")),
        1
    );
    assert_eq!(
        single_uri(
            &store,
            &rollups,
            &format!("{MACH_NS}MachineRun"),
            &format!("{MACH_NS}runState")
        ),
        format!("{MACH_NS}Draining"),
        "MachineRun's runState must reflect ONLY T2 (superseded, not duplicated)"
    );
    let as_of = single_literal(
        &store,
        &rollups,
        &format!("{MACH_NS}MachineRun"),
        &format!("{OBS_NS}asOf"),
    );
    // oxigraph canonicalizes the xsd:dateTime lexical form (drops the
    // all-zero fractional-seconds suffix) on round-trip through the store.
    assert_eq!(as_of, "2026-07-15T13:00:00Z");

    // The subject IDENTITY itself is unchanged across the supersede (proves
    // "supersedes THAT subject", not "mints a new one").
    let query = format!(
        "ASK {{ GRAPH <{rollups}> {{ <{machine_run_subject}> a <{MACH_NS}MachineRun> }} }}"
    );
    let ask = SparqlEvaluator::new()
        .parse_query(&query)
        .expect("parse ask")
        .on_store(&store)
        .execute()
        .expect("execute ask");
    assert!(
        matches!(ask, QueryResults::Boolean(true)),
        "the original MachineRun subject must still be typed"
    );

    // Untouched-sibling control: `prov:wasAttributedTo` is on EVERY rollup
    // subject (§A.2's testimony stamp) — still exactly 3 subjects carry it
    // after T2 (2 FleetObservations + 1 MachineRun), never more from a stray
    // duplicate mint.
    assert_eq!(
        count_distinct_typed(&store, &rollups, &format!("{PROV_NS}Entity")) >= 3,
        true,
        "at least the 2 FleetObservations + 1 MachineRun remain prov:Entity-typed"
    );

    cleanup(&dir);
}

// ---------------------------------------------------------------------------
// Gate 7 — multi-slice capture-metric evaluations (REAL canary-ledger shapes)
// share metric_id + evaluated_at and must each mint their OWN MetricEvaluation
// Activity. Regression for the 2026-07-21 real-ledger SHACL refusal
// (`MaxCount(1) not satisfied` on `obs:producedObservation` — the bare
// `{metric_id}:{evaluated_at}` mint collided across slices; see
// `mapping.rs::metric_evaluation_success_subject`). The fixture is the REAL
// 4-slice `usage_leverage_ratio_v1` evaluation array lifted verbatim from the
// first real canary-ledger `capture-pack` run (66,527-object fold,
// day 2026-07-20) — the exact wire bytes the projector CronJob publishes in
// the bundle's `billing[]`.
// ---------------------------------------------------------------------------

const CAPTURE_PACK_MULTI_SLICE_JSON: &str =
    include_str!("../src/observatory/fixtures/capture_pack_usage_leverage_ratio_v1.ok.json");

#[test]
fn observatory_apply_multi_slice_capture_evaluations_mint_distinct_activities() {
    let dir = temp_graph_dir("gate7-multi-slice");
    let store = open_real_store(&dir).expect("open real store");

    let evals: Vec<Value> =
        serde_json::from_str(CAPTURE_PACK_MULTI_SLICE_JSON).expect("parse real pack fixture");
    assert_eq!(evals.len(), 4, "fixture must carry the 4 real slices");
    let rendered: Vec<String> = evals.iter().map(|value| value.to_string()).collect();
    let billing: Vec<&str> = rendered.iter().map(String::as_str).collect();
    let bundle = wrap_bundle("cursor-gate7", &[LIFECYCLE_JSON], &billing);

    // Pre-fix this call failed the WHOLE rollups lane with
    // "SHACL: 20 violation(s) ... MaxCount(1) not satisfied".
    let report = catch_up(&store, &bundle, RAW_NDJSON).expect("multi-slice catch_up applies");
    assert!(report.rollups.added > 0);

    let rollups = rollups_graph_iri();
    // One Activity per wire evaluation object: 4 distinct MetricEvaluation
    // subjects, 4 distinct observations, and each Activity carries exactly
    // one producedObservation.
    assert_eq!(
        count_distinct_typed(&store, &rollups, &format!("{OBS_NS}MetricEvaluation")),
        4
    );
    assert_eq!(
        count_distinct_typed(&store, &rollups, &format!("{OBS_NS}MetricObservation")),
        4
    );
    let query = format!(
        "SELECT (COUNT(?obs) AS ?n) WHERE {{ GRAPH <{rollups}> {{ ?e a <{OBS_NS}MetricEvaluation> ; <{OBS_NS}metricDefinition> <urn:sophia:observatory:metric:usage_leverage_ratio_v1> ; <{OBS_NS}producedObservation> ?obs }} }}"
    );
    let results = SparqlEvaluator::new()
        .parse_query(&query)
        .expect("parse produced-observation count")
        .on_store(&store)
        .execute()
        .expect("execute produced-observation count");
    let QueryResults::Solutions(mut solutions) = results else {
        panic!("expected SELECT solutions")
    };
    let row = solutions.next().expect("one row").expect("row ok");
    let OxTerm::Literal(n) = row.get("n").expect("n bound").clone() else {
        panic!("count literal")
    };
    assert_eq!(
        n.value(),
        "4",
        "each of the 4 Activities carries exactly one producedObservation"
    );

    // The idempotency oracle still holds under the discriminated mint: an
    // unchanged re-apply of the SAME bundle is 0 ops in both lanes.
    let report = catch_up(&store, &bundle, RAW_NDJSON).expect("re-apply converges");
    assert!(
        report.raw.is_empty(),
        "re-applied raw must be 0 ops, got {:?}",
        report.raw
    );
    assert!(
        report.rollups.is_empty(),
        "re-applied rollups must be 0 ops, got {:?}",
        report.rollups
    );

    cleanup(&dir);
}
