//! N1 landing-brief step 6 — the `lex-scotus-core` ROUND-TRIP test over a REAL
//! store, through the REAL spine (no mocks): ingest via
//! [`crate::emporium::objects::create_objects`] (the same generic-ingest lane
//! `emporium-bookmark`/`kg-ultra-intuition` prove elsewhere), then read back
//! through the T2 query face — [`crate::emporium::query_engine::run_named_query`]
//! (`currentHeads`/`asOf`/`lineageOf`) and
//! [`crate::emporium::object_query::run_object_query`] (`emporium_query`
//! criteria) — over the Roberts→Crawford Confrontation Clause doctrine
//! lineage from `crawford/ontology/doctrine-seeds.json`.
//!
//! One loud-rejection case closes the loop: a `DoctrineHead` missing the
//! required `lex:statement` must halt with a structured violation NAMING the
//! predicate (`mint.rs`'s frozen-vocab guard), never a silent partial write.

use serde_json::{json, Value};

use crate::app_runtime::AppHandle;
use crate::emporium::contract::get_vocabulary;
use crate::emporium::object_query::{run_object_query, ObjectQueryOptions};
use crate::emporium::objects::{create_objects, resolve_subject};
use crate::emporium::query_engine::run_named_query;
use crate::graph_service::{create_graph_service, CreateGraphInput};

const VOCAB: &str = "lex-scotus-core";

fn env_serial() -> &'static std::sync::Mutex<()> {
    crate::tauri_runtime::profile_env_serial()
}

fn temp_profile(name: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    std::env::temp_dir().join(format!("garden-lex-scotus-core-{name}-{nanos}"))
}

fn run_isolated(name: &str, body: impl FnOnce() + std::panic::UnwindSafe) {
    let _serial = env_serial().lock().unwrap_or_else(|p| p.into_inner());
    let profile = temp_profile(name);
    std::env::set_var("GARDEN_PROFILE_DIR", &profile);
    let result = std::panic::catch_unwind(body);
    std::env::remove_var("GARDEN_PROFILE_DIR");
    let _ = std::fs::remove_dir_all(&profile);
    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}

fn mock_app() -> AppHandle {
    crate::tauri_runtime::build_mock_app_for_tests(true)
}

fn seed_graph(app: &AppHandle, graph_id: &str) {
    create_graph_service(
        app,
        CreateGraphInput {
            graph_id: Some(graph_id.to_string()),
            title: "Crawford round-trip".to_string(),
            description: None,
            operation_id: None,
        },
    )
    .expect("create graph");
}

/// Ingest one batch of generic records via the REAL spine (subject-scoped
/// upsert, SHACL-gated, journaled) — the same lane the bookmark/kg-ultra
/// headless tests exercise, never a bespoke write path for `lex:`.
fn ingest(app: &AppHandle, graph_id: &str, records: Value) -> Value {
    crate::app_runtime::async_runtime::block_on(create_objects(app, graph_id, VOCAB, records))
        .expect("lex-scotus-core ingest must succeed")
}

/// Predict a class's minted subject BEFORE ingest — the same
/// `resolve_subject` the write lane itself uses, never a hand-rolled URI
/// format string. The embedded tier resolves without touching the store.
fn subject_of(graph_id: &str, class: &str, local_id: &str) -> String {
    let contract = get_vocabulary(VOCAB).expect("lex-scotus-core registered");
    resolve_subject(contract, graph_id, class, local_id).expect("subject predicts")
}

/// STEP 6 — the full round-trip: ingest Roberts→Crawford through the real
/// spine, then read every named query the brief names back through the T2
/// face. One flow (not split across many `#[test]`s) because the later reads
/// depend on the earlier heads' minted subjects — splitting would mean
/// re-deriving (not sharing) those subjects per test.
#[test]
fn crawford_doctrine_lineage_round_trips_through_the_real_spine() {
    run_isolated("round-trip", || {
        let app = mock_app();
        let graph_id = "n1-lex-round-trip";
        seed_graph(&app, graph_id);

        // ── predict every subject the ingest below will mint ──
        let roberts_case = subject_of(graph_id, "Case", "448-56");
        let crawford_case = subject_of(graph_id, "Case", "541-36");
        let question = subject_of(graph_id, "LegalQuestion", "confrontation-admissibility");
        let roberts_head = subject_of(graph_id, "DoctrineHead", "roberts-reliability");
        let crawford_head = subject_of(graph_id, "DoctrineHead", "crawford-testimonial");

        // ── ingest: the two Cases (doctrine-seeds.json's establishedBy targets) ──
        ingest(
            &app,
            graph_id,
            json!([{
                "kind": "Case",
                "localId": "448-56",
                "caseName": "Ohio v. Roberts",
                "usCite": "448 U.S. 56",
                "dateDecision": "1980-06-25T00:00:00Z",
            }]),
        );
        ingest(
            &app,
            graph_id,
            json!([{
                "kind": "Case",
                "localId": "541-36",
                "caseName": "Crawford v. Washington",
                "usCite": "541 U.S. 36",
                "dateDecision": "2004-03-08T00:00:00Z",
                "precedentAlteration": true,
                "overrules": [roberts_case],
            }]),
        );

        // ── ingest: the LegalQuestion the two heads answer ──
        ingest(
            &app,
            graph_id,
            json!([{
                "kind": "LegalQuestion",
                "localId": "confrontation-admissibility",
                "questionText": "Under what conditions does the admission of an out-of-court \
                    statement against a criminal defendant violate the Confrontation Clause?",
                "clause": "Confrontation Clause, U.S. Const. amend. VI",
            }]),
        );

        // ── ingest: the Roberts→Crawford DoctrineHead lineage (doctrine-seeds.json) ──
        // `lex:lineage` follows the estate's own convention (mem:lineage): the
        // FIRST version's own subject is the stable lineage id every later
        // version inherits — here, the Roberts head's own minted subject.
        ingest(
            &app,
            graph_id,
            json!([{
                "kind": "DoctrineHead",
                "localId": "roberts-reliability",
                "question": question,
                "statement": "An unavailable declarant's out-of-court statement may be admitted \
                    if it bears adequate indicia of reliability — satisfied where the evidence \
                    falls within a firmly rooted hearsay exception or bears particularized \
                    guarantees of trustworthiness. Ohio v. Roberts, 448 U.S. 56, 66 (1980).",
                "establishedBy": roberts_case,
                "lineage": roberts_head,
                "isCurrent": false,
                "createdAt": "1980-06-25T00:00:00Z",
            }]),
        );
        ingest(
            &app,
            graph_id,
            json!([{
                "kind": "DoctrineHead",
                "localId": "crawford-testimonial",
                "question": question,
                "statement": "Testimonial statements of a witness absent from trial are \
                    admissible only where the declarant is unavailable and the defendant has had \
                    a prior opportunity for cross-examination; reliability is assessed by the \
                    Constitution's prescribed method — confrontation — not judicial estimates of \
                    trustworthiness. Crawford v. Washington, 541 U.S. 36, 53-54, 61-62, 68-69 \
                    (2004), overruling Ohio v. Roberts.",
                "establishedBy": crawford_case,
                "lineage": roberts_head,
                "isCurrent": true,
                "createdAt": "2004-03-08T00:00:00Z",
                "supersedes": [roberts_head],
            }]),
        );

        // ── currentHeads (live): Crawford is the sole current head ──
        let live = run_named_query(
            &app,
            graph_id,
            VOCAB,
            "DoctrineHead",
            "currentHeads",
            None,
            &json!({}),
        )
        .expect("currentHeads runs");
        let live_row = live
            .rows
            .iter()
            .find(|row| row["lineage"] == json!(roberts_head))
            .expect("the doctrine lineage is present live");
        assert_eq!(
            live_row["heads"],
            json!([crawford_head]),
            "Crawford is the sole live head: {live_row:?}"
        );
        assert_eq!(live_row["contested"], json!(false), "{live_row:?}");

        // ── currentHeads asOf 1990: Roberts governs (pre-Crawford) ──
        let historical = run_named_query(
            &app,
            graph_id,
            VOCAB,
            "DoctrineHead",
            "currentHeads",
            None,
            &json!({ "asOf": "1990-01-01T00:00:00Z" }),
        )
        .expect("asOf currentHeads runs");
        let historical_row = historical
            .rows
            .iter()
            .find(|row| row["lineage"] == json!(roberts_head))
            .expect("the doctrine lineage is present as of 1990");
        assert_eq!(
            historical_row["heads"],
            json!([roberts_head]),
            "Roberts governs as of 1990 (Crawford postdates it): {historical_row:?}"
        );

        // ── lineageOf(Crawford head): walks the supersession chain to Roberts ──
        let chain = run_named_query(
            &app,
            graph_id,
            VOCAB,
            "DoctrineHead",
            "lineageOf",
            None,
            &json!({ "subject": crawford_head }),
        )
        .expect("lineageOf runs");
        let node_values: Vec<String> = chain
            .rows
            .iter()
            .filter_map(|row| row.get("node").and_then(Value::as_str))
            .map(str::to_string)
            .collect();
        assert_eq!(
            node_values.len(),
            2,
            "Crawford head + the Roberts head it supersedes: {node_values:?}"
        );
        assert!(
            node_values.iter().any(|v| v.contains(&roberts_head)),
            "chain reaches Roberts: {node_values:?}"
        );
        assert!(
            node_values.iter().any(|v| v.contains(&crawford_head)),
            "chain includes Crawford itself (zero-length path): {node_values:?}"
        );

        // ── emporium_query: criteria on lex:usCite hydrates the Crawford Case ──
        let by_cite = run_object_query(
            &app,
            graph_id,
            VOCAB,
            "Case",
            &json!({ "lex:usCite": { "eq": "541 U.S. 36" } }),
            &ObjectQueryOptions::default(),
        )
        .expect("emporium_query by usCite runs");
        assert_eq!(
            by_cite.objects.len(),
            1,
            "exactly one Case carries this cite: {by_cite:?}"
        );
        let hydrated = &by_cite.objects[0];
        assert_eq!(hydrated.subject, crawford_case);
        let case_name = hydrated
            .predicates
            .get("http://mnemosyne.dev/lex#caseName")
            .and_then(|values| values.first());
        assert_eq!(
            case_name.map(String::as_str),
            Some("\"Crawford v. Washington\""),
            "{hydrated:?}"
        );
    });
}

/// STEP 6's honesty fixture: a `DoctrineHead` missing the required
/// `lex:statement` halts LOUDLY, naming the predicate — never a silent
/// partial write (the pin-cite discipline has no meaning if the engine will
/// mint a doctrine head with no statement of what it holds).
#[test]
fn doctrine_head_missing_statement_halts_loudly_naming_the_predicate() {
    run_isolated("missing-statement", || {
        let app = mock_app();
        let graph_id = "n1-lex-missing-statement";
        seed_graph(&app, graph_id);

        let question = subject_of(graph_id, "LegalQuestion", "confrontation-admissibility");
        let case = subject_of(graph_id, "Case", "541-36");
        let head = subject_of(graph_id, "DoctrineHead", "incomplete-head");

        let err = crate::app_runtime::async_runtime::block_on(create_objects(
            &app,
            graph_id,
            VOCAB,
            json!([{
                "kind": "DoctrineHead",
                "localId": "incomplete-head",
                "question": question,
                // "statement" deliberately omitted.
                "establishedBy": case,
                "lineage": head,
                "isCurrent": true,
                "createdAt": "2004-03-08T00:00:00Z",
            }]),
        ))
        .expect_err("a DoctrineHead with no lex:statement must halt, never succeed silently");
        assert_eq!(err.status(), 400, "{}", err.message());
        assert!(
            err.message().contains("lex:statement"),
            "the violation must NAME the missing predicate: {}",
            err.message()
        );
        assert!(
            err.message().contains("DoctrineHead"),
            "the violation must name the class too: {}",
            err.message()
        );
    });
}
