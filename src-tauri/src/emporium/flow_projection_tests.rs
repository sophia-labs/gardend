//! Contract and durable-write proofs for the registered `flow` pack (unit G1,
//! Mithras Flow playground build, lane G).
//!
//! Modeled on `site_projection_tests.rs`: plain `#[test]`s exercise the real
//! planner + SHACL gate against the real engine (no headless cell needed);
//! the `headless` submodule additionally proves the durable round trip
//! through `emporium_write` into a real per-graph oxigraph store, using
//! `build/contracts/golden-board.json` (copied byte-for-byte into
//! `fixtures/flow-golden-board.json` — `include_str!` is CARGO_MANIFEST_DIR-
//! relative, so this crate stays portable; see `fixtures.rs`'s own doc
//! comment for the same discipline).

use crate::emporium::{
    contract::get_vocabulary,
    planner::plan_generic_compute,
    schemas::GenericRecordIn,
    shacl_validator::{compile_check_contract, validate_desired, validate_desired_structured},
    subject_rule::{parse_subject_rule, SubjectRule},
};
use crate::rdf::graph_subject;
use serde_json::{json, Map as JsonMap, Value as Json};
use std::collections::BTreeMap;

/// The 13 classes this pack registers — one per Flow DDL table (schema.ts
/// `TABLES`), `flow:Snapshot` (the JSON file envelope, not a table) excluded.
const CLASS_ROSTER: [&str; 13] = [
    "System",
    "Requirement",
    "Task",
    "Workflow",
    "Outcome",
    "WorkflowLink",
    "Trade",
    "TradeLink",
    "Constraint",
    "ConstraintLink",
    "Source",
    "Edge",
    "Dependency",
];

#[test]
fn flow_pack_registers_thirteen_classes_and_is_not_a_public_pillar() {
    let contract = get_vocabulary("flow").expect("flow pack is registered");
    assert_eq!(contract.primary_namespace(), "urn:sophia:flow:vocab:");
    assert_eq!(contract.write_target.as_deref(), Some("projection:flow"));
    assert_eq!(contract.classes.len(), 13, "13 Flow tables, not 12 or 14");

    let mut names: Vec<&str> = contract.classes.keys().map(String::as_str).collect();
    names.sort();
    let mut roster = CLASS_ROSTER;
    roster.sort();
    assert_eq!(names, roster);
}

/// The brief cites `vocabs::golden_subject_rules_are_parseable` as an
/// existing exhaustive test to keep green; it does not exist under that name
/// anywhere in this tree — only a stale doc-comment reference at
/// `subject_rule.rs:29` claims it (see this unit's reported deviation). This
/// is the flow-scoped equivalent: every one of the 13 classes' `subject_rule`
/// strings parses through the REAL grammar (`parse_subject_rule`), as a
/// Template binding `{localId}` (never a Descriptive/code-minted marker —
/// the generic planner would reject that for a fresh pack with no bespoke
/// fork).
#[test]
fn flow_subject_rules_are_parseable() {
    let contract = get_vocabulary("flow").expect("flow pack is registered");
    assert_eq!(
        contract.classes.len(),
        CLASS_ROSTER.len(),
        "roster covers every class this test iterates"
    );
    for (name, spec) in &contract.classes {
        let rule_str = spec
            .subject_rule
            .as_deref()
            .unwrap_or_else(|| panic!("{name} has a subject_rule"));
        let rule = parse_subject_rule(rule_str)
            .unwrap_or_else(|e| panic!("{name} subject_rule '{rule_str}' parses: {e}"));
        match rule {
            SubjectRule::Template {
                required_tokens, ..
            } => {
                assert!(
                    required_tokens.iter().any(|t| t == "localId"),
                    "{name} subject_rule binds {{localId}}: {required_tokens:?}"
                );
            }
            SubjectRule::Descriptive { marker } => {
                panic!("{name} subject_rule is Descriptive ({marker}), not a mintable Template")
            }
        }
    }
}

/// The `vocab_to_shacl` snapshot: the derived closed shape lands for all 13
/// classes, AND the raw shapes this unit hand-wrote (sh:order, the three
/// enums, the sh:or class-typing, the eleven sh:sparql invariants) actually
/// concatenate into the compiled, rudof-parseable output — proving
/// `raw_shacl_shapes` is not inert text (shacl_emit.rs:179 appends it; a
/// Turtle syntax error there would fail `compile_check_contract`, not just
/// silently vanish).
#[test]
fn flow_shapes_compile_and_carry_derived_plus_raw_shapes() {
    let contract = get_vocabulary("flow").expect("flow pack is registered");
    let shapes = compile_check_contract(contract).expect("flow SHACL compiles in the real engine");

    for class in CLASS_ROSTER {
        assert!(
            shapes.contains(&format!("sh:targetClass <urn:sophia:flow:vocab:{class}>")),
            "missing derived NodeShape target for {class}"
        );
    }
    assert!(shapes.contains("sh:closed true"), "derived shapes are closed");

    // raw shapes landed: sh:order (RTRIP-5), the enums, the sh:or typing, and
    // the sh:sparql invariants ported from flow-shapes-v1.ttl.
    assert!(shapes.contains("fsh:SystemOrderShape"), "sh:order shapes present");
    assert!(
        shapes.contains("sh:in ( \"open\" \"met\" \"waived\" )"),
        "requirement status enum present"
    );
    assert!(
        shapes.contains("fsh:RequirementOwnedByTypeShape"),
        "ownedBy sh:or class-typing present"
    );
    assert!(
        shapes.contains("fsh:TradeLinkDuplicateShape"),
        "the duplicate-trade-link sh:sparql constraint is present"
    );
    assert!(
        shapes.contains("fsh:DependencyCycleShape"),
        "the dependency-cycle sh:sparql constraint is present"
    );

    // 13 derived NodeShapes plus the raw ones (order/enum/pattern/type/sparql
    // shapes) — comfortably more than 13, proving the raw block is not a
    // no-op append.
    let node_shapes = shapes.matches("a sh:NodeShape").count();
    assert!(
        node_shapes > 13,
        "raw shapes must add MORE NodeShapes on top of the 13 derived ones, got {node_shapes}"
    );
}

/// Interfaces.md §C: geometry (x, y, width, height, waypoints — 14 fields) is
/// scene, never resource. No class in this pack may materialize any of them.
#[test]
fn geometry_columns_are_absent_from_every_class() {
    let contract = get_vocabulary("flow").expect("flow pack is registered");
    for (name, spec) in &contract.classes {
        for banned in ["flow:x", "flow:y", "flow:width", "flow:height", "flow:waypoints"] {
            assert!(
                !spec.predicates.contains_key(banned),
                "{name} must not materialize geometry predicate {banned} (interfaces.md §C)"
            );
        }
    }
}

/// A requirement whose `flow:status` is not in {open, met, waived} — the
/// exact mutation `courtship/mut-bad-status.ttl` seeds (`... status "done"`)
/// — is rejected by the real SHACL gate. Pure `validate_desired` proof (no
/// headless cell needed); the `headless` module below additionally proves
/// the SAME mutant Halts through the real `emporium_write` door.
#[test]
fn illegal_requirement_status_is_rejected_by_validate_desired() {
    let contract = get_vocabulary("flow").expect("flow pack is registered");
    let graph_id = "flow-mutant-lab";
    let mut rows = flow_records(graph_id);
    mutate_first(&mut rows, "Requirement", |r| {
        r.insert("status".to_string(), json!("done"));
    });
    let plan = plan_generic_compute(contract, graph_id, &generic_records(rows))
        .expect("wire shape plans before semantic SHACL");
    let error = validate_desired(&plan.desired_inserts, contract)
        .expect_err("an illegal flow:status value must Halt");
    assert!(error.starts_with("SHACL:"), "failure is loud: {error}");
}

/// The duplicate-trade-link mutant `courtship/mut-duplicate-trade-link.ttl`
/// seeds (a second `TradeLink` naming the same `(trade, system)` pair as an
/// existing one) fires `fsh:TradeLinkDuplicateShape`'s `sh:sparql`
/// constraint. Pure `validate_desired` proof.
#[test]
fn duplicate_trade_link_fires_the_sh_sparql_constraint() {
    let contract = get_vocabulary("flow").expect("flow pack is registered");
    let graph_id = "flow-mutant-lab";
    let mut rows = flow_records(graph_id);
    let dup = rows
        .iter()
        .find(|r| r["kind"] == "TradeLink")
        .expect("golden-board.json has a TradeLink row")
        .clone();
    let mut dup = dup.as_object().unwrap().clone();
    dup.insert("localId".to_string(), json!("tl-1-duplicate"));
    rows.push(Json::Object(dup));

    let plan = plan_generic_compute(contract, graph_id, &generic_records(rows))
        .expect("wire shape plans before semantic SHACL");
    let error = validate_desired(&plan.desired_inserts, contract)
        .expect_err("a duplicate (trade, system) TradeLink pair must Halt");
    assert!(error.starts_with("SHACL:"), "failure is loud: {error}");
}

/// RESUMED WORK (this unit): the severity-placement fix. rudof does NOT
/// cascade a NodeShape's `sh:severity` down into its nested `sh:property
/// [...]` blank-node shapes — a property shape is its own shape, and its
/// severity independently defaults to `sh:Violation` when unset, matching
/// the SHACL spec's own rule that a result's severity is read off the shape
/// that was actually validated (the property shape for a property
/// constraint), never an enclosing shape that merely references it. Measured
/// directly while resuming this unit: with `sh:severity sh:Warning` declared
/// only on the outer `fsh:SystemColorShape` NodeShape (this unit's first
/// landing, commit b11d887), a malformed `flow:color` value still surfaced
/// `severity: "Violation"`. `raw_shacl_shapes` now places `sh:severity` on
/// the `sh:property [...]` block itself (and, for the `sh:sparql` shapes, on
/// the `sh:SPARQLConstraint` blank node — `shacl_sparql.rs` reads `?c
/// sh:severity ?sev` off that node, the same discipline). This proves the
/// fix at the layer this unit owns: the SHACL evaluation itself.
///
/// This does NOT claim the real `emporium_write` door tolerates a
/// Warning-severity finding end-to-end — it currently does not.
/// `write_generic_lane`'s dry-run preview (`validate_desired_structured`)
/// and the real apply path (`reconcile_classes_validated` ->
/// `validate_desired`, `spine.rs`/`reconcile.rs`) both halt on ANY
/// violation, regardless of severity; `write.rs::is_blocking` (the
/// severity-aware gate) is wired into the MEMORY lane's
/// `preview_memory_validation`/`validate_memory_write` only, never into the
/// generic lane `flow` uses. That is a pre-existing, cross-pack Emporium
/// gap (every generic-lane pack, not something this vocab-registration unit
/// introduced or is scoped to fix) — see `contracts/vocabulary-map.md` for
/// the full note.
#[test]
fn malformed_system_color_is_warning_severity_not_violation() {
    let contract = get_vocabulary("flow").expect("flow pack is registered");
    let graph_id = "flow-mutant-lab";
    let mut rows = flow_records(graph_id);
    mutate_first(&mut rows, "System", |r| {
        r.insert("color".to_string(), json!("not-a-hex-color"));
    });
    let plan = plan_generic_compute(contract, graph_id, &generic_records(rows))
        .expect("wire shape plans before semantic SHACL");
    let violations = validate_desired_structured(&plan.desired_inserts, contract)
        .expect_err("a malformed flow:color value is a SHACL finding");
    let color_violation = violations
        .iter()
        .find(|v| v.property_path.as_deref() == Some("urn:sophia:flow:vocab:color"))
        .unwrap_or_else(|| panic!("the color pattern constraint fires: {violations:?}"));
    assert_eq!(
        color_violation.severity, "Warning",
        "flow:color's pattern shape is declared Warning-severity in raw_shacl_shapes \
         (mirrors flow-shapes-v1.ttl: 'the UI emits #rrggbb, the app never validates') — \
         a Violation here means sh:severity did not attach to the shape that actually \
         produced the result: {violations:?}"
    );
}

// ---------------------------------------------------------------------------
// golden-board.json -> wire records
// ---------------------------------------------------------------------------

const GOLDEN_BOARD_JSON: &str = include_str!("fixtures/flow-golden-board.json");

/// One row-field map per Flow table: `(json_top_level_key, ClassName,
/// kebab_subject_segment, [(ddl_column, wire_field_or_None_if_geometry,
/// is_fk)])`. Transcribed directly from `schema.ts`; the three RTRIP-11
/// renames (`source`->`requirementSource`, Source's `text`->
/// `sourceCitation`, Trade's `description`->`tradeRationale`) and the
/// geometry exclusions (x, y, width, height, waypoints -> `None`) are
/// exactly what `flow.golden.json`/`contracts/vocabulary-map.md` declare.
type FieldRow = (&'static str, Option<&'static str>, bool);

fn table_map() -> Vec<(&'static str, &'static str, &'static str, Vec<FieldRow>)> {
    vec![
        (
            "systems",
            "System",
            "system",
            vec![
                ("parent_id", Some("parent"), true),
                ("name", Some("name"), false),
                ("description", Some("description"), false),
                ("display_mode", Some("displayMode"), false),
                ("color", Some("color"), false),
                ("x", None, false),
                ("y", None, false),
                ("width", None, false),
                ("height", None, false),
            ],
        ),
        (
            "requirements",
            "Requirement",
            "requirement",
            vec![
                ("system_id", Some("ownedBy"), true),
                ("text", Some("text"), false),
                ("source", Some("requirementSource"), false),
                ("status", Some("status"), false),
                ("sort_order", Some("sortOrder"), false),
                ("start_date", Some("startDate"), false),
                ("due_date", Some("dueDate"), false),
                ("duration_days", Some("durationDays"), false),
            ],
        ),
        (
            "tasks",
            "Task",
            "task",
            vec![
                ("owner_type", Some("ownerType"), false),
                ("owner_id", Some("ownedBy"), true),
                ("text", Some("text"), false),
                ("done", Some("done"), false),
                ("sort_order", Some("sortOrder"), false),
                ("start_date", Some("startDate"), false),
                ("due_date", Some("dueDate"), false),
                ("duration_days", Some("durationDays"), false),
            ],
        ),
        (
            "workflows",
            "Workflow",
            "workflow",
            vec![
                ("name", Some("name"), false),
                ("description", Some("description"), false),
                ("x", None, false),
                ("y", None, false),
            ],
        ),
        (
            "workflowLinks",
            "WorkflowLink",
            "workflow-link",
            vec![
                ("workflow_id", Some("workflow"), true),
                ("system_id", Some("system"), true),
                ("role", Some("role"), false),
                ("waypoints", None, false),
            ],
        ),
        (
            "outcomes",
            "Outcome",
            "outcome",
            vec![
                ("workflow_id", Some("workflow"), true),
                ("text", Some("text"), false),
                ("achieved", Some("achieved"), false),
                ("sort_order", Some("sortOrder"), false),
            ],
        ),
        (
            "trades",
            "Trade",
            "trade",
            vec![
                ("name", Some("name"), false),
                ("description", Some("tradeRationale"), false),
                ("winner_id", Some("winner"), true),
                ("x", None, false),
                ("y", None, false),
            ],
        ),
        (
            "tradeLinks",
            "TradeLink",
            "trade-link",
            vec![
                ("trade_id", Some("trade"), true),
                ("system_id", Some("system"), true),
                ("waypoints", None, false),
            ],
        ),
        (
            "constraints",
            "Constraint",
            "constraint",
            vec![
                ("name", Some("name"), false),
                ("description", Some("description"), false),
                ("x", None, false),
                ("y", None, false),
            ],
        ),
        (
            "constraintLinks",
            "ConstraintLink",
            "constraint-link",
            vec![
                ("constraint_id", Some("constraint"), true),
                ("target_id", Some("target"), true),
                ("note", Some("note"), false),
                ("waypoints", None, false),
            ],
        ),
        (
            "sources",
            "Source",
            "source",
            vec![
                ("owner_id", Some("ownedBy"), true),
                ("text", Some("sourceCitation"), false),
                ("sort_order", Some("sortOrder"), false),
            ],
        ),
        (
            "edges",
            "Edge",
            "edge",
            vec![
                ("source_id", Some("sourceNode"), true),
                ("target_id", Some("targetNode"), true),
                ("label", Some("label"), false),
                ("waypoints", None, false),
            ],
        ),
        (
            "deps",
            "Dependency",
            "dependency",
            vec![
                ("predecessor_id", Some("predecessor"), true),
                ("successor_id", Some("successor"), true),
            ],
        ),
    ]
}

/// Parse `fixtures/flow-golden-board.json` and build one wire record per row
/// (`{"kind": <Class>, "localId": <id>, ...predicate fields}` — the same
/// shape `emporium_write`/`plan_generic_compute` already accept), with every
/// FK column resolved to the REFERENCED row's actual minted subject IRI
/// (`{graph_subject}:projection:flow:{kebab-class}:{localId}` — `term_for`
/// treats a uri-typed field as a literal IRI string, so a bare local id like
/// `"sys-imu"` would silently degrade to the engine's invalid-uri fallback;
/// this is what a real `flow_seed_board`-style bridge must do too).
fn flow_records(graph_id: &str) -> Vec<Json> {
    let board: Json = serde_json::from_str(GOLDEN_BOARD_JSON).expect("golden-board.json parses");
    let gs = graph_subject(graph_id);
    let tables = table_map();

    // Pass 1: every row's bare id -> its minted subject IRI.
    let mut subject_of: BTreeMap<String, String> = BTreeMap::new();
    for (json_key, _class, kebab, _fields) in &tables {
        for row in board[json_key].as_array().expect("table is an array") {
            let id = row["id"].as_str().expect("row has an id").to_string();
            subject_of.insert(id, format!("{gs}:projection:flow:{kebab}:{}", row["id"].as_str().unwrap()));
            let _ = kebab;
        }
    }

    // Pass 2: build the wire records.
    let mut out = Vec::new();
    for (json_key, class, _kebab, fields) in &tables {
        for row in board[json_key].as_array().expect("table is an array") {
            let id = row["id"].as_str().expect("row has an id");
            let mut record = JsonMap::new();
            record.insert("kind".to_string(), json!(class));
            record.insert("localId".to_string(), json!(id));
            for (column, wire_field, is_fk) in fields {
                let Some(wire_field) = wire_field else {
                    continue; // geometry — excluded (interfaces.md §C)
                };
                let value = &row[*column];
                if value.is_null() {
                    continue; // nullable, optional in the pack — omit, never write null
                }
                if *is_fk {
                    let raw_id = value.as_str().unwrap_or_else(|| {
                        panic!("{class}.{column} on row {id} is a non-string FK value")
                    });
                    let resolved = subject_of.get(raw_id).unwrap_or_else(|| {
                        panic!("{class}.{column} on row {id} references unknown id {raw_id}")
                    });
                    record.insert(wire_field.to_string(), json!(resolved));
                } else {
                    record.insert(wire_field.to_string(), value.clone());
                }
            }
            out.push(Json::Object(record));
        }
    }
    out
}

/// Mutate the FIRST record of the given `kind` in place (test-mutant helper).
fn mutate_first(rows: &mut [Json], kind: &str, f: impl FnOnce(&mut JsonMap<String, Json>)) {
    let row = rows
        .iter_mut()
        .find(|r| r["kind"] == kind)
        .unwrap_or_else(|| panic!("a {kind} row exists"));
    f(row.as_object_mut().expect("record is an object"));
}

fn generic_records(rows: Vec<Json>) -> Vec<GenericRecordIn> {
    rows.into_iter()
        .map(|value| serde_json::from_value(value).expect("flow record deserializes"))
        .collect()
}

#[test]
fn flow_records_from_golden_board_cover_all_thirteen_classes_with_no_geometry() {
    let rows = flow_records("flow-fixture-lab");
    assert_eq!(rows.len(), 17, "3 systems + 2 reqs + 2 tasks + 1 each of the other 10 tables");
    let mut by_kind: BTreeMap<String, usize> = BTreeMap::new();
    for row in &rows {
        *by_kind.entry(row["kind"].as_str().unwrap().to_string()).or_default() += 1;
        for banned in ["x", "y", "width", "height", "waypoints"] {
            assert!(
                row.get(banned).is_none(),
                "wire record must never carry geometry field {banned}: {row}"
            );
        }
    }
    assert_eq!(by_kind.len(), 13, "every one of the 13 classes appears at least once");
}

// ---------------------------------------------------------------------------
// headless: the durable round trip through the real emporium_write door
// ---------------------------------------------------------------------------

#[cfg(feature = "headless")]
mod headless {
    use super::*;
    use crate::{
        app_runtime::AppHandle,
        emporium::write::emporium_write,
        graph_service::{create_graph_service, CreateGraphInput},
        rdf_service::{run_sparql_query_service, SparqlInput},
    };
    use std::{
        path::PathBuf,
        sync::Mutex,
        time::{SystemTime, UNIX_EPOCH},
    };

    fn env_serial() -> &'static Mutex<()> {
        crate::tauri_runtime::profile_env_serial()
    }

    fn temp_profile(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        std::env::temp_dir().join(format!("garden-flow-pack-{name}-{nanos}"))
    }

    fn seed_graph(app: &AppHandle, graph_id: &str) {
        create_graph_service(
            app,
            CreateGraphInput {
                graph_id: Some(graph_id.to_string()),
                title: "Flow Pack Lab".to_string(),
                description: None,
                operation_id: None,
            },
        )
        .expect("create graph");
    }

    fn select(app: &AppHandle, graph_id: &str, query: &str) -> Vec<BTreeMap<String, String>> {
        run_sparql_query_service(
            app.clone(),
            SparqlInput {
                graph_id: graph_id.to_string(),
                query: query.to_string(),
            },
        )
        .expect("sparql query")
        .rows
    }

    /// Distinct `flow:<Class>` subjects in the `:projection:flow` sink.
    fn class_subject_count(app: &AppHandle, graph_id: &str, class: &str) -> usize {
        let sink = format!("{}:projection:flow", graph_subject(graph_id));
        let flow_ns = "urn:sophia:flow:vocab:";
        select(
            app,
            graph_id,
            &format!(
                "SELECT DISTINCT ?s WHERE {{ GRAPH <{sink}> {{ ?s a <{flow_ns}{class}> }} }}"
            ),
        )
        .len()
    }

    /// G1's round trip (brief §4): `golden-board.json` (geometry stripped) ->
    /// `emporium_write` (the same primitive `reconcile_classes_validated`
    /// backs, per `write_generic_lane`) -> a temp per-graph store -> SPARQL
    /// subject count per class equals row count. Then two mutants, each
    /// through the SAME real write door: an illegal `flow:status` value
    /// Halts (SHACL structural teeth), and a duplicate `TradeLink` Halts
    /// (the `sh:sparql` teeth) — "the door bites in the failing direction."
    #[test]
    fn round_trip_golden_board_then_two_mutants_halt_at_the_real_write_door() {
        let _serial = env_serial()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let profile = temp_profile("round-trip");
        std::env::set_var("GARDEN_PROFILE_DIR", &profile);
        let result = std::panic::catch_unwind(|| {
            let app = crate::tauri_runtime::build_mock_app_for_tests(true);
            let graph_id = "flow-round-trip-lab";
            seed_graph(&app, graph_id);

            let rows = flow_records(graph_id);
            let by_kind = {
                let mut m: BTreeMap<String, usize> = BTreeMap::new();
                for r in &rows {
                    *m.entry(r["kind"].as_str().unwrap().to_string()).or_default() += 1;
                }
                m
            };
            assert_eq!(by_kind.len(), 13);

            let applied = crate::app_runtime::async_runtime::block_on(emporium_write(
                &app, graph_id, "flow", &rows, false, false, None,
            ))
            .expect("golden-board rows apply through the real write door");
            assert_eq!(applied["ok"], json!(true), "{applied}");
            assert!(applied["journalRef"].is_string(), "{applied}");

            for (class, expected) in &by_kind {
                let got = class_subject_count(&app, graph_id, class);
                assert_eq!(
                    got, *expected,
                    "{class}: expected {expected} subject(s) in :projection:flow, got {got}"
                );
            }

            // A converged replay of the SAME rows is zero new subjects (no
            // duplication) — reconcile_classes_validated's subject-scoped
            // upsert default.
            let replay = crate::app_runtime::async_runtime::block_on(emporium_write(
                &app, graph_id, "flow", &rows, false, false, None,
            ))
            .expect("replay converges");
            assert_eq!(replay["ok"], json!(true), "{replay}");
            for (class, expected) in &by_kind {
                let got = class_subject_count(&app, graph_id, class);
                assert_eq!(got, *expected, "{class}: replay must not duplicate subjects");
            }

            // Mutant 1 (courtship/mut-bad-status.ttl): illegal flow:status.
            let mut bad_status_rows = flow_records(graph_id);
            mutate_first(&mut bad_status_rows, "Requirement", |r| {
                r.insert("status".to_string(), json!("done"));
            });
            let refused = crate::app_runtime::async_runtime::block_on(emporium_write(
                &app,
                graph_id,
                "flow",
                &bad_status_rows,
                true, // dry_run — the door bites without writing anything
                false,
                None,
            ))
            .expect("emporium_write returns a typed refusal, not an Err");
            assert_eq!(refused["ok"], json!(false), "{refused}");
            assert_eq!(refused["results"][0]["outcome"], json!("halted"), "{refused}");

            // Mutant 2 (courtship/mut-duplicate-trade-link.ttl): a second
            // TradeLink naming the same (trade, system) pair.
            let mut dup_rows = flow_records(graph_id);
            let dup = dup_rows
                .iter()
                .find(|r| r["kind"] == "TradeLink")
                .expect("a TradeLink row exists")
                .clone();
            let mut dup = dup.as_object().unwrap().clone();
            dup.insert("localId".to_string(), json!("tl-1-duplicate"));
            dup_rows.push(Json::Object(dup));
            let refused_dup = crate::app_runtime::async_runtime::block_on(emporium_write(
                &app, graph_id, "flow", &dup_rows, true, false, None,
            ))
            .expect("emporium_write returns a typed refusal, not an Err");
            assert_eq!(refused_dup["ok"], json!(false), "{refused_dup}");

            // The original valid state is untouched by either refused dry-run.
            for (class, expected) in &by_kind {
                let got = class_subject_count(&app, graph_id, class);
                assert_eq!(got, *expected, "{class}: a refused dry-run must write nothing");
            }
        });
        std::env::remove_var("GARDEN_PROFILE_DIR");
        let _ = std::fs::remove_dir_all(&profile);
        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
    }
}
