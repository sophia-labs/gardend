//! The RDF-native workflow plan tools (`workflow_plan_create`,
//! `workflow_plan_read`, `workflow_plan_compose`), driven through the real MCP
//! dispatch table against a disposable graph, the way a `tools/call` reaches
//! them. Written before the tools existed: every call resolves its tool with
//! `mcp_dispatch_registry::lookup`, so this file compiled and failed first.
//!
//! The grammar is Choreograph's graph-plan-v1 (`src/workflow/graph-plan.ts`,
//! `compileGraphPlan`); the fixture is the real Freud definition its tests pin
//! (`tests/fixtures/workflow_plan/graph-plan-freud-ch1.sparql.json`).
use crate::local_jobs::LocalJobRegistry;
use crate::mcp_dispatch_registry::{lookup, McpCallCtx};
use serde_json::{json, Value};
use std::sync::Arc;

const GRAPH: &str = "workflow-plan";
const WF: &str = "http://mnemosyne.dev/workflow#";
const FREUD_FIXTURE: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/workflow_plan/graph-plan-freud-ch1.sparql.json"
));

fn with_graph(test: impl FnOnce(&McpCallCtx<'_>)) {
    let _serial = crate::tauri_runtime::profile_env_serial()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let previous = std::env::var_os("GARDEN_PROFILE_DIR");
    let profile =
        std::env::temp_dir().join(format!("garden-workflow-plan-{}", uuid::Uuid::new_v4()));
    std::env::set_var("GARDEN_PROFILE_DIR", &profile);
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let app = crate::tauri_runtime::build_mock_app_for_tests(true);
        crate::graph_service::create_graph_service(
            &app,
            crate::graph_service::CreateGraphInput {
                graph_id: Some(GRAPH.into()),
                title: "Workflow plan specimen".into(),
                description: None,
                operation_id: None,
            },
        )
        .unwrap();
        let jobs = Arc::new(LocalJobRegistry::new(profile.join("jobs")).unwrap());
        let ctx = McpCallCtx {
            app: app.clone(),
            jobs: &jobs,
        };
        test(&ctx);
    }));
    match previous {
        Some(value) => std::env::set_var("GARDEN_PROFILE_DIR", value),
        None => std::env::remove_var("GARDEN_PROFILE_DIR"),
    }
    let _ = std::fs::remove_dir_all(&profile);
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}

/// One `tools/call` through the registry entry, as `mcp_tool_dispatch` runs it.
fn call(ctx: &McpCallCtx<'_>, name: &str, mut args: Value) -> Result<Value, String> {
    args["graphId"] = json!(GRAPH);
    let entry = lookup(name).unwrap_or_else(|| panic!("no dispatch entry for {name}"));
    crate::app_runtime::async_runtime::block_on((entry.handler)(ctx, &args))
        .map_err(|error| error.to_string())
}

fn ok(ctx: &McpCallCtx<'_>, name: &str, args: Value) -> Value {
    call(ctx, name, args.clone()).unwrap_or_else(|error| panic!("{name} {args}: {error}"))
}

fn create(ctx: &McpCallCtx<'_>, title: &str) -> (String, Value) {
    let created = ok(ctx, "workflow_plan_create", json!({ "title": title }));
    let id = created["documentId"]
        .as_str()
        .unwrap_or_else(|| panic!("no documentId: {created}"))
        .to_string();
    (id, created)
}

fn messages(read: &Value) -> Vec<String> {
    read["report"]["problems"]
        .as_array()
        .unwrap_or_else(|| panic!("no report.problems: {read}"))
        .iter()
        .map(|problem| problem["message"].as_str().unwrap_or_default().to_string())
        .collect()
}

fn problem<'a>(read: &'a Value, message: &str) -> &'a Value {
    read["report"]["problems"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|problem| problem["message"] == message)
        .unwrap_or_else(|| panic!("no problem {message:?} in {}", read["report"]))
}

/// `?s ?p ?o` of one named graph, as N-Triples terms.
fn graph_rows(ctx: &McpCallCtx<'_>, graph: &str) -> Vec<Value> {
    let result = ok(
        ctx,
        "sparql_query",
        json!({ "query": format!("SELECT ?s ?p ?o WHERE {{ GRAPH <{graph}> {{ ?s ?p ?o }} }}") }),
    );
    result["rows"].as_array().cloned().unwrap_or_default()
}

/// The Freud plan of Choreograph's fixture, as compose moves.
fn freud_moves() -> Value {
    let fixture: Value = serde_json::from_str(FREUD_FIXTURE).unwrap();
    let literal = |subject: &str, predicate: &str| -> String {
        let row = fixture["rows"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| {
                row["s"] == format!("<urn:sophia:wf:freud-ch1#{subject}>")
                    && row["p"] == format!("<{WF}{predicate}>")
            })
            .unwrap_or_else(|| panic!("fixture has no {subject} {predicate}"));
        serde_json::from_str::<String>(row["o"].as_str().unwrap()).unwrap()
    };
    json!([
        { "op": "set_title", "title": "Things done to sleepers, chapter I" },
        { "op": "upsert_phase", "id": "p1", "title": "Day residue", "order": 1 },
        { "op": "upsert_phase", "id": "p2", "title": "Dream-work", "order": 2 },
        { "op": "upsert_phase", "id": "p3", "title": "The Censor", "order": 3 },
        {
            "op": "upsert_step", "id": "reader", "phase": "p1", "label": "the Reader",
            "agentType": "segmenter", "mode": "once", "produces": "passages",
            "instructions": literal("reader", "instructions"),
            "itemSchema": serde_json::from_str::<Value>(&literal("reader", "itemSchema")).unwrap(),
        },
        {
            "op": "upsert_step", "id": "clerk", "phase": "p2", "label": "the Experimenter's Clerk",
            "agentType": "extractor", "mode": "perItem", "over": "passages", "produces": "candidates",
            "instructions": literal("clerk", "instructions"),
            "itemSchema": literal("clerk", "itemSchema"),
        },
        {
            "op": "upsert_step", "id": "censor", "phase": "p3", "label": "the Censor",
            "agentType": "quote checker", "checks": "candidates", "produces": "verified",
            "instructions": literal("censor", "instructions"),
        },
    ])
}

#[test]
fn create_makes_a_workflow_document_with_a_seeded_draft() {
    with_graph(|ctx| {
        let (id, created) = create(ctx, "Things done to sleepers");
        let workflow = format!("urn:sophia:wf:{id}");
        let draft = format!("urn:sophia:wf:{id}:draft");
        assert_eq!(created["workflow"], workflow.as_str(), "{created}");
        assert_eq!(created["draftGraph"], draft.as_str(), "{created}");
        assert_eq!(created["title"], "Things done to sleepers", "{created}");

        // The document is a workflow, and its body says so to whoever opens it.
        let kinds = ok(
            ctx,
            "sparql_query",
            json!({ "query": format!(
                "PREFIX doc: <http://mnemosyne.dev/doc#> SELECT ?kind WHERE {{ GRAPH ?g {{ \
                 <urn:mnemosyne:local:document:{id}> doc:documentKind ?kind }} }}"
            ) }),
        );
        assert_eq!(kinds["rows"][0]["kind"], "\"workflow\"", "{kinds}");
        let body = ok(ctx, "read_document", json!({ "document_id": id })).to_string();
        assert!(body.contains("This document is a workflow"), "{body}");
        assert!(body.contains(&draft), "{body}");
        assert!(body.contains("workflow_plan_compose"), "{body}");

        // The draft holds exactly the seed.
        let rows = graph_rows(ctx, &draft);
        let has = |p: &str, o: &str| {
            rows.iter()
                .any(|row| row["s"] == format!("<{workflow}>") && row["p"] == p && row["o"] == o)
        };
        assert_eq!(rows.len(), 3, "{rows:?}");
        assert!(has(
            "<http://www.w3.org/1999/02/22-rdf-syntax-ns#type>",
            &format!("<{WF}Workflow>")
        ));
        assert!(has(&format!("<{WF}name>"), &format!("\"{id}\"")));
        assert!(has(
            "<http://purl.org/dc/terms/title>",
            "\"Things done to sleepers\""
        ));

        // A fresh plan is incomplete, and says why.
        assert_eq!(created["report"]["conforms"], false, "{created}");
        assert_eq!(
            problem(&created, "the workflow has no phases")["source"],
            "structure"
        );
        assert!(created["phases"].as_array().unwrap().is_empty());
    });
}

#[test]
fn composing_the_freud_plan_conforms_and_records_one_event() {
    with_graph(|ctx| {
        let (id, _) = create(ctx, "Freud");
        let read = ok(
            ctx,
            "workflow_plan_compose",
            json!({
                "documentId": id,
                "rationale": "Catalogue the things done to sleepers in chapter I.",
                "moves": freud_moves(),
            }),
        );
        assert_eq!(read["report"]["conforms"], true, "{}", read["report"]);
        assert_eq!(read["title"], "Things done to sleepers, chapter I");
        let phases: Vec<(String, i64, Vec<String>)> = read["phases"]
            .as_array()
            .unwrap()
            .iter()
            .map(|phase| {
                (
                    phase["title"].as_str().unwrap().to_string(),
                    phase["order"].as_i64().unwrap(),
                    phase["steps"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|step| step["label"].as_str().unwrap().to_string())
                        .collect(),
                )
            })
            .collect();
        assert_eq!(
            phases,
            vec![
                ("Day residue".into(), 1, vec!["the Reader".into()]),
                (
                    "Dream-work".into(),
                    2,
                    vec!["the Experimenter's Clerk".into()]
                ),
                ("The Censor".into(), 3, vec!["the Censor".into()]),
            ]
        );
        let censor = &read["phases"][2]["steps"][0];
        assert_eq!(censor["id"], "censor");
        assert_eq!(
            censor["mode"], "perItem",
            "a check runs once per item: {censor}"
        );
        assert_eq!(censor["checks"], "candidates");
        assert_eq!(censor["produces"], "verified");
        let clerk = &read["phases"][1]["steps"][0];
        assert_eq!(clerk["over"], "passages");
        assert_eq!(clerk["itemSchema"]["required"][0], "who", "{clerk}");

        // The plan is RDF in the draft graph, as the compiler reads it.
        let draft = format!("urn:sophia:wf:{id}:draft");
        let rows = graph_rows(ctx, &draft);
        assert!(rows
            .iter()
            .any(|row| row["s"] == format!("<urn:sophia:wf:{id}#p2>")
                && row["p"] == format!("<{WF}order>")
                && row["o"] == "\"2\"^^<http://www.w3.org/2001/XMLSchema#integer>"));
        assert!(rows
            .iter()
            .any(|row| row["s"] == format!("<urn:sophia:wf:{id}#censor>")
                && row["p"] == format!("<{WF}checks>")
                && row["o"] == format!("<urn:sophia:wf:{id}#candidates>")));

        // One composition event in the authoring graph: the rationale, the
        // moves, and what changed.
        let recent = read["authoring"]["recent"].as_array().unwrap();
        let event = &recent[0];
        assert_eq!(
            event["rationale"],
            "Catalogue the things done to sleepers in chapter I."
        );
        assert_eq!(event["moves"].as_array().unwrap().len(), 7, "{event}");
        assert!(
            event["at"].as_str().is_some_and(|at| at.starts_with("20")),
            "{event}"
        );
        let authoring = format!("urn:sophia:wf:{id}:authoring");
        let events = ok(
            ctx,
            "sparql_query",
            json!({ "query": format!(
                "PREFIX wf: <{WF}> SELECT ?e ?ins ?del WHERE {{ GRAPH <{authoring}> {{ \
                 ?e a wf:CompositionEvent ; wf:rationale ?r ; wf:insertedTripleCount ?ins ; \
                 wf:deletedTripleCount ?del . FILTER(CONTAINS(STR(?r), \"sleepers\")) }} }}"
            ) }),
        );
        let events = events["rows"].as_array().unwrap();
        assert_eq!(events.len(), 1, "{events:?}");
        // 1 title replaced (1 out, 1 in) + 3 phases x 4 + 3 steps (9 + 8 + 7).
        assert_eq!(
            events[0]["ins"], "\"37\"^^<http://www.w3.org/2001/XMLSchema#integer>",
            "{events:?}"
        );
        assert_eq!(
            events[0]["del"],
            "\"1\"^^<http://www.w3.org/2001/XMLSchema#integer>"
        );
    });
}

#[test]
fn a_gap_is_reported_like_the_compiler_and_never_blocks_the_write() {
    with_graph(|ctx| {
        let (id, _) = create(ctx, "Freud");
        ok(
            ctx,
            "workflow_plan_compose",
            json!({ "documentId": id, "rationale": "the plan", "moves": freud_moves() }),
        );
        let read = ok(
            ctx,
            "workflow_plan_compose",
            json!({
                "documentId": id,
                "rationale": "a second reader of dreams nobody collected",
                "moves": [{
                    "op": "upsert_step", "id": "dreamer", "phase": "p2", "label": "the Dreamer",
                    "agentType": "reader", "instructions": "Read every dream.",
                    "consumes": ["dreams"], "produces": "readings",
                    "itemSchema": { "type": "object" },
                }],
            }),
        );
        assert_eq!(read["report"]["conforms"], false);
        let gap = problem(&read, "dreamer reads dreams, which no step produces");
        assert_eq!(gap["source"], "structure");
        assert_eq!(gap["focus"], format!("urn:sophia:wf:{id}#dreamer"));
        // Accepted all the same: the step is in the plan.
        let dream_work = &read["phases"][1]["steps"];
        assert!(
            dream_work
                .as_array()
                .unwrap()
                .iter()
                .any(|step| step["id"] == "dreamer"),
            "{dream_work}"
        );
        // And the other structural rules, worded as the compiler words them.
        let read = ok(
            ctx,
            "workflow_plan_compose",
            json!({
                "documentId": id,
                "rationale": "break the rest",
                "moves": [
                    { "op": "upsert_phase", "id": "p4", "title": "Again", "order": 3 },
                    {
                        "op": "upsert_step", "id": "echo", "phase": "p1", "label": "the Echo",
                        "agentType": "reader", "instructions": "Echo the readings.",
                        "mode": "perItem", "consumes": ["candidates"], "produces": "passages",
                        "itemSchema": "[1, 2]",
                    },
                    {
                        "op": "upsert_step", "id": "sentry", "phase": "p3", "label": "the Sentry",
                        "agentType": "checker", "instructions": "Check again.",
                        "mode": "once", "checks": "candidates", "over": "passages",
                    },
                ],
            }),
        );
        let found = messages(&read);
        for expected in [
            "two phases share a wf:order",
            "echo runs per item but names no collection (wf:over)",
            "echo's wf:itemSchema is not a JSON Schema object",
            "passages is produced by both echo and reader",
            "echo reads candidates before an earlier phase has produced it",
            "sentry checks a collection, so it runs once per item",
            "sentry checks candidates but runs over passages",
        ] {
            assert!(
                found.iter().any(|m| m == expected),
                "no {expected:?} in {found:?}"
            );
        }
    });
}

#[test]
fn shacl_reports_per_node_problems_like_the_compiler() {
    with_graph(|ctx| {
        let (id, _) = create(ctx, "Freud");
        ok(
            ctx,
            "workflow_plan_compose",
            json!({ "documentId": id, "rationale": "the plan", "moves": freud_moves() }),
        );
        // Raw RDF into the draft is a legitimate way to change it; the report
        // judges the graph, whoever wrote it.
        let w = format!("urn:sophia:wf:{id}");
        let long = "x".repeat(4001);
        ok(
            ctx,
            "sparql_update",
            json!({ "update": format!(
                "PREFIX wf: <{WF}> INSERT DATA {{ GRAPH <{w}:draft> {{ \
                 <{w}#odd> a wf:AgentNode ; wf:inPhase <{w}#p9> ; wf:mode \"sometimes\" ; \
                 wf:instructions \"{long}\" ; wf:produces <{w}#oddities> ; \
                 wf:itemSchema \"{{}}\" . \
                 <{w}#p3> wf:order \"third\" . }} }}"
            ) }),
        );
        let read = ok(ctx, "workflow_plan_read", json!({ "documentId": id }));
        assert_eq!(read["report"]["conforms"], false);
        for expected in [
            "odd has no wf:label",
            "odd's wf:mode is \"sometimes\"; use \"once\" or \"perItem\"",
            "odd's instructions are longer than 4000 characters",
            "odd is in p9, which is not a phase of this workflow",
            "p3 has more than one wf:order; it needs exactly one",
            "p3's wf:order must be a whole number",
        ] {
            let found = problem(&read, expected);
            assert_eq!(found["source"], "shacl", "{found}");
        }
        assert_eq!(
            problem(&read, "odd has no wf:label")["focus"],
            format!("{w}#odd")
        );
    });
}

/// Choreograph's own fixture, written as raw RDF into a workflow's draft:
/// the report agrees with `compileGraphPlan` (it compiles, so it conforms;
/// without the reader's `wf:produces` the compiler refuses with "reads
/// passages, which no step produces", and so does the report).
#[test]
fn the_compilers_fixture_conforms_and_its_gap_is_named_the_same_way() {
    with_graph(|ctx| {
        let (id, _) = create(ctx, "Freud");
        let w = format!("urn:sophia:wf:{id}");
        let fixture: Value = serde_json::from_str(FREUD_FIXTURE).unwrap();
        let rename = |term: &str| term.replace("urn:sophia:wf:freud-ch1", &w);
        let body = fixture["rows"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| {
                let o = if row["p"] == format!("<{WF}name>") {
                    format!("\"{id}\"")
                } else {
                    rename(row["o"].as_str().unwrap())
                };
                format!(
                    "{} {} {o} .",
                    rename(row["s"].as_str().unwrap()),
                    row["p"].as_str().unwrap()
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        ok(
            ctx,
            "sparql_update",
            json!({ "update": format!(
                "DELETE WHERE {{ GRAPH <{w}:draft> {{ ?s ?p ?o }} }} ; \
                 INSERT DATA {{ GRAPH <{w}:draft> {{ {body} }} }}"
            ) }),
        );
        let read = ok(ctx, "workflow_plan_read", json!({ "documentId": id }));
        assert_eq!(read["report"]["conforms"], true, "{}", read["report"]);
        assert_eq!(read["phases"].as_array().unwrap().len(), 3);

        ok(
            ctx,
            "sparql_update",
            json!({ "update": format!(
                "DELETE WHERE {{ GRAPH <{w}:draft> {{ <{w}#reader> <{WF}produces> ?c }} }}"
            ) }),
        );
        let read = ok(ctx, "workflow_plan_read", json!({ "documentId": id }));
        let found = messages(&read);
        assert!(
            found
                .iter()
                .any(|m| m.contains("reads passages, which no step produces")),
            "{found:?}"
        );
        assert!(
            found.iter().any(|m| m == "reader has no wf:produces"),
            "{found:?}"
        );
    });
}

#[test]
fn malformed_moves_refuse_the_whole_call() {
    with_graph(|ctx| {
        let (id, _) = create(ctx, "Freud");
        let draft = format!("urn:sophia:wf:{id}:draft");
        let before = graph_rows(ctx, &draft).len();
        let phase = json!({ "op": "upsert_phase", "id": "p1", "title": "Day residue", "order": 1 });
        for (moves, says) in [
            (json!([phase, { "op": "frobnicate" }]), "frobnicate"),
            (
                json!([{ "op": "upsert_phase", "id": "1st", "title": "x", "order": 1 }]),
                "1st",
            ),
            (
                json!([{ "op": "upsert_step", "id": "reader", "phase": "p1",
                         "label": "the Reader", "agentType": "segmenter" }]),
                "instructions",
            ),
            (
                json!([{ "op": "upsert_phase", "id": "p1", "title": "x" }]),
                "order",
            ),
            (json!([{ "op": "remove", "id": "nothing" }]), "nothing"),
        ] {
            let error = call(
                ctx,
                "workflow_plan_compose",
                json!({ "documentId": id, "rationale": "try", "moves": moves }),
            )
            .expect_err("a malformed move refuses the call");
            assert!(error.contains(says), "{says:?} not in {error}");
        }
        assert_eq!(graph_rows(ctx, &draft).len(), before, "nothing was written");

        // A phase is removed only once it is empty.
        ok(
            ctx,
            "workflow_plan_compose",
            json!({ "documentId": id, "rationale": "the plan", "moves": freud_moves() }),
        );
        let error = call(
            ctx,
            "workflow_plan_compose",
            json!({ "documentId": id, "rationale": "drop", "moves": [{ "op": "remove", "id": "p1" }] }),
        )
        .expect_err("a phase with steps is not removed");
        assert!(error.contains("p1") && error.contains("reader"), "{error}");
        let read = ok(
            ctx,
            "workflow_plan_compose",
            json!({ "documentId": id, "rationale": "drop", "moves": [
                { "op": "remove", "id": "reader" }, { "op": "remove", "id": "p1" },
            ] }),
        );
        assert_eq!(
            read["phases"].as_array().unwrap().len(),
            2,
            "{}",
            read["phases"]
        );
    });
}

#[test]
fn the_tools_refuse_a_document_that_is_not_a_workflow() {
    with_graph(|ctx| {
        ok(
            ctx,
            "create_document",
            json!({ "documentId": "plain-notes", "title": "Plain notes" }),
        );
        for (tool, args) in [
            ("workflow_plan_read", json!({ "documentId": "plain-notes" })),
            (
                "workflow_plan_compose",
                json!({ "documentId": "plain-notes", "rationale": "x",
                        "moves": [{ "op": "set_title", "title": "Hijacked" }] }),
            ),
        ] {
            let error = call(ctx, tool, args).expect_err("not a workflow document");
            assert!(error.contains("not a workflow"), "{tool}: {error}");
        }
        assert!(graph_rows(ctx, "urn:sophia:wf:plain-notes:draft").is_empty());
    });
}

#[test]
fn the_tools_are_registered_with_scopes_in_every_source_of_truth() {
    for name in [
        "workflow_plan_create",
        "workflow_plan_read",
        "workflow_plan_compose",
    ] {
        assert!(lookup(name).is_some(), "dispatch row for {name}");
        assert!(
            crate::mcp_tool_registry::is_known_mcp_tool_name(name),
            "catalog entry for {name}"
        );
    }
    assert_eq!(
        crate::loopback_scopes::mcp_tool_scopes("workflow_plan_read"),
        vec!["rdf.query"]
    );
    // Editing a workflow document, not general RDF write: no rdf.update.
    assert_eq!(
        crate::loopback_scopes::mcp_tool_scopes("workflow_plan_compose"),
        vec!["rdf.query", "documents.write.crdt"]
    );
    assert_eq!(
        crate::loopback_scopes::mcp_tool_scopes("workflow_plan_create"),
        vec!["workspace.write.crdt", "documents.write.crdt"]
    );
}

// ── who may call them: the tool's own scope gate, through the real router ──
//
// Changing a workflow's plan is editing that workflow document, not general
// RDF write: the tools fence every write to the document's own `:draft` and
// `:authoring` graphs, behind the kind check. So compose needs `rdf.query` +
// `documents.write.crdt` and create `workspace.write.crdt` +
// `documents.write.crdt`; neither needs `rdf.update`, which a member (and so a
// supervised service such as the self-host Vehicle) does not hold. The RDF
// writes inside the tools take no second scope check.

const MASTER: &str = "workflow-plan-test-master-key";

fn loopback_state(
    app: crate::app_runtime::AppHandle,
    dir: &std::path::Path,
) -> Arc<crate::loopback_state::LoopbackState> {
    let grant = crate::loopback_token_grants::session_all_token_grant();
    Arc::new(crate::loopback_state::LoopbackState {
        app,
        token: MASTER.into(),
        manifest: crate::loopback_state::LoopbackManifest {
            runtime_profile: "test",
            bind_host: "127.0.0.1",
            port: 0,
            api_url: "http://127.0.0.1:0".into(),
            mcp_url: "http://127.0.0.1:0/mcp".into(),
            openapi_url: "http://127.0.0.1:0/openapi.json".into(),
            token: MASTER.into(),
            pid: 0,
            started_at: String::new(),
            manifest_path: String::new(),
            auth_header: "Authorization: Bearer <token>",
            token_audience: "test",
            token_storage: "memory",
            security_warning: "synthetic test manifest",
            cell_graph_id: None,
            cell_owner: None,
            cell_generation: None,
            cell_registry_revision: None,
            capabilities: grant.scopes.clone(),
            token_scope_mode: grant.scope_mode,
            token_scopes: grant.scopes,
            scope_details: grant.scope_details,
            grant_profiles: grant.grant_profiles,
        },
        jobs: Arc::new(LocalJobRegistry::new(dir.join("jobs")).unwrap()),
        services: Arc::new(crate::local_service_host::LocalServiceHost::default()),
        lifecycle: Arc::new(crate::cell_lifecycle::CellLifecycle::new()),
        cell_graph: Arc::new(crate::cell_graph_boundary::CellGraphBoundary::for_test(
            None,
        )),
        self_host: Arc::new(Default::default()),
    })
}

fn client_token(
    app: &crate::app_runtime::AppHandle,
    grant: Option<&str>,
    scopes: Option<&[&str]>,
) -> String {
    crate::loopback_client_token_service::create_client_token(
        app,
        crate::loopback_client_token_types::CreateLoopbackClientTokenInput {
            label: Some(format!("{grant:?} {scopes:?}")),
            grant_profile_id: grant.map(str::to_string),
            scopes: scopes.map(|scopes| scopes.iter().map(|s| s.to_string()).collect()),
            expires_in_days: None,
        },
    )
    .unwrap()
    .token
}

/// One JSON-RPC call to `/mcp` with `token`: the result, or the error message.
async fn rpc(
    state: &Arc<crate::loopback_state::LoopbackState>,
    token: &str,
    method: &str,
    params: Value,
) -> Result<Value, String> {
    use tower::ServiceExt;
    let body = json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params });
    let request = axum::http::Request::builder()
        .method("POST")
        .uri("/mcp")
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/json")
        .body(axum::body::Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let response = crate::loopback_router::loopback_router(state.clone())
        .oneshot(request)
        .await
        .unwrap();
    let bytes = axum::body::to_bytes(response.into_body(), 10_000_000)
        .await
        .unwrap();
    let reply: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    match reply["error"]["message"].as_str() {
        Some(message) => Err(message.to_string()),
        None => Ok(reply["result"].clone()),
    }
}

async fn tool_call(
    state: &Arc<crate::loopback_state::LoopbackState>,
    token: &str,
    name: &str,
    mut arguments: Value,
) -> Result<Value, String> {
    arguments["graphId"] = json!(GRAPH);
    rpc(
        state,
        token,
        "tools/call",
        json!({ "name": name, "arguments": arguments }),
    )
    .await
    .map(|result| result["structuredContent"].clone())
}

#[test]
fn a_document_writer_without_rdf_update_composes_and_a_reader_cannot() {
    with_graph(|ctx| {
        let dir = std::env::temp_dir().join(format!(
            "garden-workflow-plan-state-{}",
            uuid::Uuid::new_v4()
        ));
        let state = loopback_state(ctx.app.clone(), &dir);
        // Document scopes and RDF read, no rdf.update.
        let writer = client_token(
            &ctx.app,
            None,
            Some(&["mcp.tools.call", "rdf.query", "documents.write.crdt"]),
        );
        // A reader that may call tools.
        let reader = client_token(&ctx.app, None, Some(&["mcp.tools.call", "rdf.query"]));
        // What gardend's supervised services hold (the self-host Vehicle).
        let member = client_token(&ctx.app, Some("member"), None);
        crate::app_runtime::async_runtime::block_on(async {
            let created = tool_call(
                &state,
                MASTER,
                "workflow_plan_create",
                json!({ "title": "Freud" }),
            )
            .await
            .expect("the owner creates a workflow");
            let id = created["documentId"]
                .as_str()
                .expect("documentId")
                .to_string();

            let composed = tool_call(
                &state,
                &writer,
                "workflow_plan_compose",
                json!({ "documentId": id, "rationale": "the plan", "moves": freud_moves() }),
            )
            .await
            .expect("documents.write.crdt + rdf.query compose, without rdf.update");
            assert_eq!(
                composed["report"]["conforms"], true,
                "{}",
                composed["report"]
            );

            let refused = tool_call(
                &state,
                &reader,
                "workflow_plan_compose",
                json!({ "documentId": id, "rationale": "x",
                        "moves": [{ "op": "set_title", "title": "Hijacked" }] }),
            )
            .await
            .expect_err("a reader cannot compose");
            assert!(refused.contains("documents.write.crdt"), "{refused}");
            let read = tool_call(
                &state,
                &reader,
                "workflow_plan_read",
                json!({ "documentId": id }),
            )
            .await
            .expect("a reader reads");
            assert_eq!(read["title"], "Things done to sleepers, chapter I");

            // A member (what gardend's supervised services hold) creates one too.
            tool_call(
                &state,
                &member,
                "workflow_plan_create",
                json!({ "title": "Another" }),
            )
            .await
            .expect("workspace.write.crdt + documents.write.crdt create, without rdf.update");

            // And a member is offered all three (a client hides what it cannot call).
            let listed = rpc(&state, &member, "tools/list", json!({})).await.unwrap();
            let names: Vec<&str> = listed["tools"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|tool| tool["name"].as_str())
                .collect();
            for name in [
                "workflow_plan_create",
                "workflow_plan_read",
                "workflow_plan_compose",
            ] {
                assert!(names.contains(&name), "{name} not offered to a member");
            }
        });
        let _ = std::fs::remove_dir_all(&dir);
    });
}

// ── workflow_plan_publish: a run's findings into the margin ─────────────────

/// A two-phase plan whose last collection is `findings`.
fn finding_moves() -> Value {
    json!([
        { "op": "upsert_phase", "id": "p1", "title": "Read", "order": 1 },
        { "op": "upsert_phase", "id": "p2", "title": "Find", "order": 2 },
        {
            "op": "upsert_step", "id": "reader", "phase": "p1", "label": "the Reader",
            "agentType": "segmenter", "instructions": "Cut it into passages.", "mode": "once",
            "produces": "passages", "itemSchema": { "type": "object" },
        },
        {
            "op": "upsert_step", "id": "finder", "phase": "p2", "label": "the Finder",
            "agentType": "extractor", "instructions": "Find what to say.", "mode": "perItem",
            "over": "passages", "produces": "findings", "itemSchema": { "type": "object" },
        },
    ])
}

/// The run graph Choreograph's `serializeGraphPlanRun` writes, for `items`
/// (collection id, item JSON), in its exact shape.
fn write_run(ctx: &McpCallCtx<'_>, workflow: &str, run_id: &str, items: &[(&str, Value)]) {
    let run_graph = format!(
        "{workflow}:run:{}",
        run_id.replace(
            |c: char| !c.is_ascii_alphanumeric() && c != '_' && c != '-',
            "_"
        )
    );
    let run = format!("<{run_graph}#run>");
    let lit = |value: &str| serde_json::to_string(value).unwrap();
    let mut triples = vec![format!(
        "{run} a wf:GraphPlanRun ; wf:runId {} ; wf:status {} ; wf:definition <{workflow}:v1> ; \
         wf:definitionDigest {} ; wf:workflow <{workflow}> .",
        lit(run_id),
        lit("completed"),
        lit("sha256:test")
    )];
    let mut positions: std::collections::BTreeMap<&str, usize> = Default::default();
    for (n, (collection, item)) in items.iter().enumerate() {
        let position = positions.entry(collection).or_default();
        triples.push(format!(
            "<{run_graph}#item-{}> a wf:Item ; wf:partOfRun {run} ; wf:inCollection <{workflow}#{collection}> ; \
             wf:position {position} ; wf:json {} .",
            n + 1,
            lit(&item.to_string())
        ));
        *position += 1;
    }
    ok(
        ctx,
        "sparql_update",
        json!({ "update": format!(
            "PREFIX wf: <{WF}>\nINSERT DATA {{ GRAPH <{run_graph}> {{\n{}\n}} }}",
            triples.join("\n")
        ) }),
    );
}

fn stored_comments(ctx: &McpCallCtx<'_>, document_id: &str) -> serde_json::Map<String, Value> {
    use yrs::types::ToJson;
    use yrs::{ReadTxn, Transact};
    let graph_dir = crate::graph_paths::existing_graph_dir(&ctx.app, GRAPH).unwrap();
    let path =
        crate::ydoc_paths::checked_document_ydoc_state_path(&graph_dir, document_id).unwrap();
    crate::app_runtime::async_runtime::block_on(async {
        let room = ctx
            .app
            .state::<crate::crdt_engine::rooms::RoomRegistry>()
            .get_or_create(&format!("doc:{GRAPH}:{document_id}"), path)
            .await
            .unwrap();
        room.with_doc(|doc| {
            let txn = doc.transact();
            txn.get_map("comments")
                .map(|comments| serde_json::to_value(comments.to_json(&txn)).unwrap())
                .unwrap_or(Value::Null)
        })
        .await
    })
    .as_object()
    .cloned()
    .unwrap_or_default()
}

fn revision(ctx: &McpCallCtx<'_>, document_id: &str) -> u64 {
    ok(ctx, "read_document", json!({ "document_id": document_id }))["revision"]
        .as_u64()
        .unwrap_or_else(|| panic!("no revision for {document_id}"))
}

#[test]
fn publish_places_a_runs_findings_in_one_write_per_document_and_twice_is_once() {
    with_graph(|ctx| {
        let (id, _) = create(ctx, "Margins");
        let workflow = format!("urn:sophia:wf:{id}");
        ok(
            ctx,
            "workflow_plan_compose",
            json!({ "documentId": id, "rationale": "the plan", "moves": finding_moves() }),
        );
        for (doc, content) in [
            (
                "alpha",
                "The quick brown fox jumps.\n\nOver the lazy dog it goes.",
            ),
            ("beta", "Nothing much is said here."),
        ] {
            ok(
                ctx,
                "write_document",
                json!({ "document_id": doc, "title": doc, "content": content, "format": "markdown" }),
            );
        }
        let blocks = ok(
            ctx,
            "read_document",
            json!({ "document_id": "alpha", "format": "ids_only" }),
        );
        let first_block = blocks["block_ids"][0].as_str().unwrap().to_string();
        write_run(
            ctx,
            &workflow,
            "run/1",
            &[
                ("passages", json!({ "text": "a passage, not a finding" })),
                (
                    "findings",
                    json!({ "documentId": "alpha", "blockId": first_block,
                                     "quote": "quick brown fox", "note": "An animal." }),
                ),
                (
                    "findings",
                    json!({ "documentId": "alpha", "quote": "lazy dog", "note": "Another." }),
                ),
                (
                    "findings",
                    json!({ "documentId": "beta", "quote": "not in this text", "note": "Lost." }),
                ),
                (
                    "findings",
                    json!({ "documentId": "nowhere", "quote": "x", "note": "No such document." }),
                ),
                ("findings", json!({ "note": "no document, no quote" })),
            ],
        );
        let alpha_before = revision(ctx, "alpha");

        let published = ok(
            ctx,
            "workflow_plan_publish",
            json!({ "documentId": id, "runId": "run/1" }),
        );
        assert_eq!(published["placed"], 2, "{published}");
        let not_found: Vec<(i64, String)> = published["notFound"]
            .as_array()
            .unwrap()
            .iter()
            .map(|miss| {
                (
                    miss["index"].as_i64().unwrap(),
                    miss["quote"].as_str().unwrap().to_string(),
                )
            })
            .collect();
        assert_eq!(
            not_found,
            vec![(3, "not in this text".to_string()), (4, "x".to_string())],
            "{published}"
        );
        assert!(
            published["notFound"][1]["reason"]
                .as_str()
                .is_some_and(|reason| reason.contains("nowhere")),
            "{published}"
        );
        assert_eq!(published["skipped"], 1, "{published}");

        let comments = stored_comments(ctx, "alpha");
        let mut ids: Vec<&String> = comments.keys().collect();
        ids.sort();
        assert_eq!(ids, vec!["wf-run_1-1", "wf-run_1-2"], "{comments:?}");
        assert_eq!(comments["wf-run_1-1"]["text"], "An animal.");
        assert_eq!(comments["wf-run_1-2"]["quotedText"], "lazy dog");
        assert_eq!(
            comments["wf-run_1-2"]["blockId"], blocks["block_ids"][1],
            "anchored by search"
        );
        assert!(stored_comments(ctx, "beta").is_empty());
        assert_eq!(
            revision(ctx, "alpha"),
            alpha_before + 1,
            "both comments land in one write of alpha"
        );
        let xml = ok(ctx, "read_document", json!({ "document_id": "alpha" }))["content"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(
            xml.contains("wf-run_1-1") && xml.contains("wf-run_1-2"),
            "{xml}"
        );

        // Twice is once: the same ids are updated, nothing doubles.
        let again = ok(
            ctx,
            "workflow_plan_publish",
            json!({ "documentId": id, "runId": "run/1" }),
        );
        assert_eq!(again["placed"], 2, "{again}");
        assert_eq!(stored_comments(ctx, "alpha").len(), 2);
        let xml_again = ok(ctx, "read_document", json!({ "document_id": "alpha" }))["content"]
            .as_str()
            .unwrap()
            .to_string();
        assert_eq!(
            xml_again.matches("wf-run_1-1").count(),
            xml.matches("wf-run_1-1").count(),
            "{xml_again}"
        );

        // Each publish is one wf:Publication in the authoring graph.
        let publications = ok(
            ctx,
            "sparql_query",
            json!({ "query": format!(
                "PREFIX wf: <{WF}> SELECT ?p ?placed WHERE {{ GRAPH <{workflow}:authoring> {{ \
                 ?p a wf:Publication ; wf:placedCount ?placed ; wf:runId \"run/1\" ; \
                 wf:targetDocument <urn:mnemosyne:local:document:alpha> ; \
                 wf:notFoundQuote \"not in this text\" }} }}"
            ) }),
        );
        assert_eq!(
            publications["rows"].as_array().unwrap().len(),
            2,
            "{publications}"
        );
    });
}

#[test]
fn publish_is_fenced_and_needs_a_run() {
    with_graph(|ctx| {
        ok(
            ctx,
            "create_document",
            json!({ "documentId": "plain", "title": "Plain" }),
        );
        let error = call(
            ctx,
            "workflow_plan_publish",
            json!({ "documentId": "plain", "runId": "r1" }),
        )
        .expect_err("not a workflow document");
        assert!(error.contains("not a workflow"), "{error}");
        let (id, _) = create(ctx, "Margins");
        ok(
            ctx,
            "workflow_plan_compose",
            json!({ "documentId": id, "rationale": "the plan", "moves": finding_moves() }),
        );
        let error = call(
            ctx,
            "workflow_plan_publish",
            json!({ "documentId": id, "runId": "never" }),
        )
        .expect_err("no such run");
        assert!(error.contains("never"), "{error}");
        assert_eq!(
            crate::loopback_scopes::mcp_tool_scopes("workflow_plan_publish"),
            vec!["rdf.query", "documents.write.crdt"]
        );
        assert!(crate::mcp_tool_registry::is_known_mcp_tool_name(
            "workflow_plan_publish"
        ));
    });
}

/// The live run's misses, on the book's own chapter I text: a wrapped and
/// elided quote, straight for curly quotes, a quote whose `blockId` names
/// the passage's first block while the text sits in the next, and a quote
/// the book does not hold. Three land, one is reported.
#[test]
fn publish_anchors_quotes_as_the_clerks_copied_them() {
    with_graph(|ctx| {
        let fixture: Value = serde_json::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/comment_anchoring/freud-ch1-blocks.json"
        )))
        .unwrap();
        ok(
            ctx,
            "write_document",
            json!({ "document_id": "freud-ch1", "title": "Chapter I", "tiptapJson": fixture["tiptapJson"] }),
        );
        let ids = ok(
            ctx,
            "read_document",
            json!({ "document_id": "freud-ch1", "format": "ids_only" }),
        )["block_ids"]
            .clone();
        let block = |n: usize| ids[n].as_str().unwrap().to_string();
        let (id, _) = create(ctx, "Margins");
        ok(
            ctx,
            "workflow_plan_compose",
            json!({ "documentId": id, "rationale": "the plan", "moves": finding_moves() }),
        );
        let finding = |block_id: String, quote: &str| json!({ "documentId": "freud-ch1", "blockId": block_id, "quote": quote, "note": "Seen." });
        write_run(
            ctx,
            &format!("urn:sophia:wf:{id}"),
            "run-ch1",
            &[
                ("findings", finding(block(1), "The younger Strümpell,\nson of the philosopher whose writings on the subject have\nalready more than once served us…")),
                ("findings", finding(block(3), "\"Every indistinctly perceived noise gives rise to corresponding dream pictures")),
                ("findings", finding(block(1), "There are a great many stimuli of such nature, ranging from those that are unavoidable")),
                ("findings", finding(block(1), "The dreamer counts the sheep twice and wakes in Lisbon.")),
            ],
        );
        let published = ok(
            ctx,
            "workflow_plan_publish",
            json!({ "documentId": id, "runId": "run-ch1" }),
        );
        assert_eq!(published["placed"], 3, "{published}");
        let missed: Vec<i64> = published["notFound"]
            .as_array()
            .unwrap()
            .iter()
            .map(|miss| miss["index"].as_i64().unwrap())
            .collect();
        assert_eq!(missed, vec![4], "{published}");
        let comments = stored_comments(ctx, "freud-ch1");
        assert_eq!(comments.len(), 3, "{comments:?}");
        assert_eq!(comments["wf-run-ch1-1"]["blockId"], block(1).as_str());
        assert_eq!(
            comments["wf-run-ch1-3"]["blockId"],
            block(2).as_str(),
            "the next block holds it"
        );
        assert!(!comments.contains_key("wf-run-ch1-4"));
    });
}

// ── sources and destinations (graph-plan-v1, additive) ──────────────────────

/// A two-phase plan whose Reader reads the book (a source) and whose findings
/// go into the book's margin (a destination). The Reader names the source
/// before the source exists: a later move may bring it.
fn sourced_moves(document_id: &str) -> Value {
    json!([
        { "op": "upsert_phase", "id": "p1", "title": "Read", "order": 1 },
        { "op": "upsert_phase", "id": "p2", "title": "Find", "order": 2 },
        {
            "op": "upsert_step", "id": "reader", "phase": "p1", "label": "the Reader",
            "agentType": "segmenter", "instructions": "Cut the book into passages.",
            "mode": "once", "reads": "book", "produces": "passages",
            "itemSchema": { "type": "object" },
        },
        {
            "op": "upsert_step", "id": "finder", "phase": "p2", "label": "the Finder",
            "agentType": "extractor", "instructions": "Find what to say.", "mode": "perItem",
            "over": "passages", "produces": "findings", "itemSchema": { "type": "object" },
        },
        {
            "op": "upsert_source", "id": "book", "label": "The Interpretation of Dreams, I.C",
            "documentId": document_id, "fromBlock": "block-be1fe535", "toBlock": "block-c0.ffee:2",
        },
        {
            "op": "upsert_destination", "id": "margin", "label": "The book's margin",
            "receives": "findings", "documentId": document_id,
        },
    ])
}

fn write_book(ctx: &McpCallCtx<'_>, document_id: &str, title: &str) {
    ok(
        ctx,
        "write_document",
        json!({ "document_id": document_id, "title": title, "format": "markdown",
                "content": "The quick brown fox jumps.\n\nOver the lazy dog it goes." }),
    );
}

fn step<'a>(read: &'a Value, id: &str) -> &'a Value {
    read["phases"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|phase| phase["steps"].as_array().into_iter().flatten())
        .find(|step| step["id"] == id)
        .unwrap_or_else(|| panic!("no step {id} in {}", read["phases"]))
}

#[test]
fn a_source_a_reading_step_and_a_destination_conform_and_read_back() {
    with_graph(|ctx| {
        write_book(ctx, "freud-ch1", "Chapter I");
        let (id, created) = create(ctx, "Freud");
        assert_eq!(created["sources"], json!([]), "{created}");
        assert_eq!(created["destinations"], json!([]), "{created}");
        let read = ok(
            ctx,
            "workflow_plan_compose",
            json!({ "documentId": id, "rationale": "read the book, write in its margin",
                    "moves": sourced_moves("freud-ch1") }),
        );
        assert_eq!(read["report"]["conforms"], true, "{}", read["report"]);
        assert_eq!(
            read["sources"],
            json!([{
                "id": "book",
                "label": "The Interpretation of Dreams, I.C",
                "documentId": "freud-ch1",
                "fromBlock": "block-be1fe535",
                "toBlock": "block-c0.ffee:2",
                "documentTitle": "Chapter I",
                "readBy": ["reader"],
            }])
        );
        assert_eq!(
            read["destinations"],
            json!([{
                "id": "margin",
                "label": "The book's margin",
                "kind": "margin",
                "receives": "findings",
                "documentId": "freud-ch1",
            }])
        );
        assert_eq!(step(&read, "reader")["reads"], json!(["book"]));
        assert_eq!(step(&read, "finder")["reads"], json!([]));

        // The RDF the compiler reads, with wf:kind written out.
        let w = format!("urn:sophia:wf:{id}");
        let rows = graph_rows(ctx, &format!("{w}:draft"));
        let has = |s: &str, p: &str, o: &str| {
            rows.iter()
                .any(|row| row["s"] == format!("<{w}#{s}>") && row["p"] == p && row["o"] == o)
        };
        let rdf_type = "<http://www.w3.org/1999/02/22-rdf-syntax-ns#type>";
        assert!(has("book", rdf_type, &format!("<{WF}Source>")), "{rows:?}");
        assert!(has("book", &format!("<{WF}documentId>"), "\"freud-ch1\""));
        assert!(has(
            "book",
            &format!("<{WF}fromBlock>"),
            "\"block-be1fe535\""
        ));
        assert!(has(
            "reader",
            &format!("<{WF}reads>"),
            &format!("<{w}#book>")
        ));
        assert!(has("margin", rdf_type, &format!("<{WF}Destination>")));
        assert!(has("margin", &format!("<{WF}kind>"), "\"margin\""));
        assert!(has(
            "margin",
            &format!("<{WF}receives>"),
            &format!("<{w}#findings>")
        ));
    });
}

#[test]
fn source_and_destination_problems_are_worded_like_the_compiler() {
    with_graph(|ctx| {
        let (id, _) = create(ctx, "Freud");
        ok(
            ctx,
            "workflow_plan_compose",
            json!({ "documentId": id, "rationale": "the plan", "moves": finding_moves() }),
        );
        // What moves can say wrong: a step reading what is not a source, a
        // destination receiving what nobody produces, too many of each.
        let mut moves = vec![
            json!({
                "op": "upsert_step", "id": "reader", "phase": "p1", "label": "the Reader",
                "agentType": "segmenter", "instructions": "Cut it into passages.", "mode": "once",
                "reads": ["ghost"], "produces": "passages", "itemSchema": { "type": "object" },
            }),
            json!({ "op": "upsert_destination", "id": "shelf", "label": "A shelf",
                    "receives": "nowhere" }),
        ];
        for n in 1..=7 {
            moves.push(json!({ "op": "upsert_source", "id": format!("s{n}"),
                               "label": format!("Source {n}"), "documentId": format!("doc-{n}") }));
        }
        for n in 1..=4 {
            moves.push(json!({ "op": "upsert_destination", "id": format!("d{n}"),
                               "label": format!("Margin {n}"), "receives": "findings" }));
        }
        ok(
            ctx,
            "workflow_plan_compose",
            json!({ "documentId": id, "rationale": "too much", "moves": moves }),
        );
        // What only raw RDF can say wrong.
        let w = format!("urn:sophia:wf:{id}");
        ok(
            ctx,
            "sparql_update",
            json!({ "update": format!(
                "PREFIX wf: <{WF}> INSERT DATA {{ GRAPH <{w}:draft> {{ \
                 <{w}#odd> a wf:Destination ; wf:label \"Odd\" ; wf:kind \"shelf\" ; \
                 wf:receives <{w}#findings> . \
                 <{w}#bare> a wf:Destination . \
                 <{w}#loose> a wf:Source ; wf:label \"a\" , \"b\" ; wf:documentId \"doc/1\" ; \
                 wf:fromBlock \"a b\" ; wf:toBlock \"c d\" . \
                 <{w}#nodoc> a wf:Source ; wf:label \"No document\" . \
                 <{w}#finder> wf:reads \"the book\" . }} }}"
            ) }),
        );
        let read = ok(ctx, "workflow_plan_read", json!({ "documentId": id }));
        assert_eq!(read["report"]["conforms"], false);
        for (expected, source) in [
            ("at most 6 sources", "structure"),
            ("at most 4 destinations", "structure"),
            (
                "reader reads ghost, which is not a source of this workflow (wf:Source)",
                "structure",
            ),
            (
                "shelf receives nowhere, which no step produces",
                "structure",
            ),
            ("odd's wf:kind is \"shelf\"; use \"margin\"", "shacl"),
            ("bare has no wf:label", "shacl"),
            ("bare has no wf:kind", "shacl"),
            ("bare has no wf:receives", "shacl"),
            ("loose has 2 wf:label; it needs exactly one", "shacl"),
            ("loose's wf:documentId is not a document id", "structure"),
            ("loose's wf:fromBlock is not a block id", "structure"),
            ("loose's wf:toBlock is not a block id", "structure"),
            ("nodoc has no wf:documentId", "shacl"),
            (
                "finder's wf:reads must be an IRI, not \"the book\"",
                "shacl",
            ),
        ] {
            let found = problem(&read, expected);
            assert_eq!(found["source"], source, "{found}");
        }
        assert_eq!(problem(&read, "at most 6 sources")["focus"], w.as_str());
        assert_eq!(
            problem(&read, "shelf receives nowhere, which no step produces")["focus"],
            format!("{w}#shelf")
        );
        // Each message once (the report dedupes as the compiler does).
        let found = messages(&read);
        let unique: std::collections::BTreeSet<&String> = found.iter().collect();
        assert_eq!(unique.len(), found.len(), "{found:?}");
    });
}

#[test]
fn the_report_names_a_document_this_graph_does_not_hold() {
    with_graph(|ctx| {
        write_book(ctx, "freud-ch1", "Chapter I");
        let (id, _) = create(ctx, "Freud");
        let mut moves = sourced_moves("freud-ch1").as_array().unwrap().clone();
        moves.push(
            json!({ "op": "upsert_source", "id": "lost", "label": "A lost book",
                           "documentId": "no-such-doc" }),
        );
        moves.push(
            json!({ "op": "upsert_destination", "id": "margin", "label": "Elsewhere",
                           "receives": "findings", "documentId": "elsewhere" }),
        );
        let read = ok(
            ctx,
            "workflow_plan_compose",
            json!({ "documentId": id, "rationale": "two books, one lost", "moves": moves }),
        );
        assert_eq!(read["report"]["conforms"], false, "{}", read["report"]);
        let lost = problem(
            &read,
            "lost names document no-such-doc, which is not in this graph",
        );
        assert_eq!(lost["source"], "workspace", "{lost}");
        assert_eq!(lost["focus"], format!("urn:sophia:wf:{id}#lost"));
        assert_eq!(
            problem(
                &read,
                "margin names document elsewhere, which is not in this graph"
            )["source"],
            "workspace"
        );
        // Silent for the book the graph holds, whose title it shows.
        let found = messages(&read);
        assert!(!found.iter().any(|m| m.contains("freud-ch1")), "{found:?}");
        assert_eq!(found.len(), 2, "{found:?}");
        let sources = read["sources"].as_array().unwrap();
        assert_eq!(sources[0]["id"], "book");
        assert_eq!(sources[0]["documentTitle"], "Chapter I");
        assert_eq!(sources[1]["id"], "lost");
        assert!(sources[1].get("documentTitle").is_none(), "{}", sources[1]);

        let read = ok(
            ctx,
            "workflow_plan_compose",
            json!({ "documentId": id, "rationale": "the margin is the book's", "moves": [
                { "op": "remove", "id": "lost" },
                { "op": "upsert_destination", "id": "margin", "label": "The book's margin",
                  "receives": "findings", "documentId": "freud-ch1" },
            ] }),
        );
        assert_eq!(read["report"]["conforms"], true, "{}", read["report"]);
    });
}

#[test]
fn a_source_is_removed_once_no_step_reads_it_and_a_destination_at_once() {
    with_graph(|ctx| {
        write_book(ctx, "freud-ch1", "Chapter I");
        let (id, _) = create(ctx, "Freud");
        ok(
            ctx,
            "workflow_plan_compose",
            json!({ "documentId": id, "rationale": "the plan", "moves": sourced_moves("freud-ch1") }),
        );
        let error = call(
            ctx,
            "workflow_plan_compose",
            json!({ "documentId": id, "rationale": "drop", "moves": [{ "op": "remove", "id": "book" }] }),
        )
        .expect_err("a source a step reads is not removed");
        assert!(
            error.contains("source book is still read by reader; change those steps first"),
            "{error}"
        );
        let read = ok(
            ctx,
            "workflow_plan_compose",
            json!({ "documentId": id, "rationale": "no margin", "moves": [{ "op": "remove", "id": "margin" }] }),
        );
        assert_eq!(read["destinations"], json!([]), "{read}");
        let read = ok(
            ctx,
            "workflow_plan_compose",
            json!({ "documentId": id, "rationale": "no book", "moves": [
                {
                    "op": "upsert_step", "id": "reader", "phase": "p1", "label": "the Reader",
                    "agentType": "segmenter", "instructions": "Cut the input into passages.",
                    "mode": "once", "produces": "passages", "itemSchema": { "type": "object" },
                },
                { "op": "remove", "id": "book" },
            ] }),
        );
        assert_eq!(read["sources"], json!([]), "{read}");
        assert_eq!(read["report"]["conforms"], true, "{}", read["report"]);
        let w = format!("urn:sophia:wf:{id}");
        assert!(!graph_rows(ctx, &format!("{w}:draft"))
            .iter()
            .any(|row| row["s"] == format!("<{w}#book>") || row["s"] == format!("<{w}#margin>")));
    });
}

#[test]
fn malformed_source_and_destination_moves_refuse_the_whole_call() {
    with_graph(|ctx| {
        let (id, _) = create(ctx, "Freud");
        let mut moves = finding_moves().as_array().unwrap().clone();
        moves.push(
            json!({ "op": "upsert_source", "id": "book", "label": "The book",
                           "documentId": "freud-ch1" }),
        );
        ok(
            ctx,
            "workflow_plan_compose",
            json!({ "documentId": id, "rationale": "the plan", "moves": moves }),
        );
        let draft = format!("urn:sophia:wf:{id}:draft");
        let before = graph_rows(ctx, &draft).len();
        let source = |extra: Value| {
            let mut source = json!({ "op": "upsert_source", "id": "other", "label": "Other",
                                     "documentId": "doc-other" });
            for (key, value) in extra.as_object().unwrap() {
                source[key] = value.clone();
            }
            source
        };
        let reader = |reads: Value| {
            json!({
                "op": "upsert_step", "id": "reader", "phase": "p1", "label": "the Reader",
                "agentType": "segmenter", "instructions": "Cut it.", "mode": "once",
                "produces": "passages", "itemSchema": { "type": "object" }, "reads": reads,
            })
        };
        let destination = |extra: Value| {
            let mut destination = json!({ "op": "upsert_destination", "id": "margin",
                                          "label": "Margin", "receives": "findings" });
            for (key, value) in extra.as_object().unwrap() {
                destination[key] = value.clone();
            }
            destination
        };
        for (bad, says) in [
            (
                source(json!({ "documentId": "doc/1" })),
                "documentId \"doc/1\" is not a document id",
            ),
            (
                source(json!({ "fromBlock": "a b" })),
                "fromBlock \"a b\" is not a block id",
            ),
            (
                source(json!({ "toBlock": "x".repeat(129) })),
                "is not a block id",
            ),
            (
                json!({ "op": "upsert_source", "id": "other", "label": "Other" }),
                "needs documentId",
            ),
            (source(json!({ "pages": "1-3" })), "unknown field \"pages\""),
            (
                destination(json!({ "kind": "shelf" })),
                "kind is \"shelf\"; use \"margin\"",
            ),
            (
                json!({ "op": "upsert_destination", "id": "margin", "label": "Margin" }),
                "needs receives",
            ),
            (reader(json!(5)), "reads must be a list of ids"),
            (reader(json!(["1st"])), "\"1st\" is not an id"),
            (
                source(json!({ "id": "p1" })),
                "p1 is a phase; give the source its own id",
            ),
            (
                source(json!({ "id": "passages" })),
                "passages names a collection; give the source its own id",
            ),
            (
                source(json!({ "id": "finder" })),
                "finder is a step; give the source its own id",
            ),
            (
                destination(json!({ "id": "book" })),
                "book is a source; give the destination its own id",
            ),
            (
                destination(json!({ "id": "findings" })),
                "findings names a collection; give the destination its own id",
            ),
            (
                destination(json!({ "receives": "book" })),
                "book is a source, not a collection",
            ),
            (
                reader(json!(["passages"])),
                "passages names a collection, not a source",
            ),
            (reader(json!("p2")), "p2 is a phase, not a source"),
            (
                json!({ "op": "upsert_phase", "id": "book", "title": "Books", "order": 9 }),
                "book is a source; give the phase its own id",
            ),
            (
                json!({
                    "op": "upsert_step", "id": "book", "phase": "p1", "label": "x",
                    "agentType": "x", "instructions": "x", "produces": "xs",
                }),
                "book is a source; give the step its own id",
            ),
            (
                json!({
                    "op": "upsert_step", "id": "lister", "phase": "p2", "label": "the Lister",
                    "agentType": "x", "instructions": "x", "mode": "perItem", "over": "book",
                    "produces": "lists",
                }),
                "book is a source, not a collection",
            ),
        ] {
            let moves = json!([{ "op": "set_title", "title": "Changed" }, bad]);
            let error = call(
                ctx,
                "workflow_plan_compose",
                json!({ "documentId": id, "rationale": "try", "moves": moves }),
            )
            .expect_err("a malformed move refuses the call");
            assert!(error.contains(says), "{says:?} not in {error}");
        }
        assert_eq!(graph_rows(ctx, &draft).len(), before, "nothing was written");
        let read = ok(ctx, "workflow_plan_read", json!({ "documentId": id }));
        assert_eq!(read["title"], "Freud");
    });
}

#[test]
fn publish_to_a_destination_publishes_what_it_receives_into_its_document() {
    with_graph(|ctx| {
        let (id, _) = create(ctx, "Margins");
        let workflow = format!("urn:sophia:wf:{id}");
        let mut moves = finding_moves().as_array().unwrap().clone();
        // b-margin comes first in the moves; a-margin is first by id.
        moves.push(
            json!({ "op": "upsert_destination", "id": "b-margin", "label": "The findings",
                           "receives": "findings" }),
        );
        moves.push(
            json!({ "op": "upsert_destination", "id": "a-margin", "label": "The passages",
                           "receives": "passages", "documentId": "alpha" }),
        );
        ok(
            ctx,
            "workflow_plan_compose",
            json!({ "documentId": id, "rationale": "the plan", "moves": moves }),
        );
        for (doc, content) in [
            (
                "alpha",
                "The quick brown fox jumps.\n\nOver the lazy dog it goes.",
            ),
            ("beta", "Nothing much is said here."),
        ] {
            ok(
                ctx,
                "write_document",
                json!({ "document_id": doc, "title": doc, "content": content, "format": "markdown" }),
            );
        }
        write_run(
            ctx,
            &workflow,
            "r1",
            &[
                (
                    "passages",
                    json!({ "quote": "quick brown fox", "note": "In the destination's document." }),
                ),
                (
                    "passages",
                    json!({ "documentId": "beta", "quote": "Nothing much", "note": "Its own." }),
                ),
                (
                    "findings",
                    json!({ "documentId": "beta", "quote": "said here", "note": "A finding." }),
                ),
                (
                    "findings",
                    json!({ "quote": "lazy dog", "note": "b-margin names no document." }),
                ),
            ],
        );

        // By default the first margin destination by id: the passages, not
        // the last collection.
        let published = ok(
            ctx,
            "workflow_plan_publish",
            json!({ "documentId": id, "runId": "r1" }),
        );
        assert_eq!(published["destination"], "a-margin", "{published}");
        assert_eq!(published["collection"], "passages", "{published}");
        assert_eq!(published["placed"], 2, "{published}");
        assert_eq!(published["skipped"], 0, "{published}");
        let alpha = stored_comments(ctx, "alpha");
        assert_eq!(alpha.len(), 1, "{alpha:?}");
        assert_eq!(alpha["wf-r1-1"]["text"], "In the destination's document.");
        assert_eq!(alpha["wf-r1-1"]["quotedText"], "quick brown fox");
        let beta = stored_comments(ctx, "beta");
        assert_eq!(beta["wf-r1-2"]["text"], "Its own.", "{beta:?}");
        let recorded = ok(
            ctx,
            "sparql_query",
            json!({ "query": format!(
                "PREFIX wf: <{WF}> SELECT ?p WHERE {{ GRAPH <{workflow}:authoring> {{ \
                 ?p a wf:Publication ; wf:destination <{workflow}#a-margin> ; \
                 wf:collection <{workflow}#passages> }} }}"
            ) }),
        );
        assert_eq!(recorded["rows"].as_array().unwrap().len(), 1, "{recorded}");

        // The one named, and an item whose document neither it nor the
        // destination names is skipped.
        let published = ok(
            ctx,
            "workflow_plan_publish",
            json!({ "documentId": id, "runId": "r1", "destination": "b-margin" }),
        );
        assert_eq!(published["destination"], "b-margin", "{published}");
        assert_eq!(published["collection"], "findings", "{published}");
        assert_eq!(published["placed"], 1, "{published}");
        assert_eq!(published["skipped"], 1, "{published}");
        assert_eq!(
            stored_comments(ctx, "beta")["wf-r1-1"]["text"],
            "A finding."
        );

        let error = call(
            ctx,
            "workflow_plan_publish",
            json!({ "documentId": id, "runId": "r1", "destination": "nowhere" }),
        )
        .expect_err("no such destination");
        assert!(
            error
                .contains("the plan has no margin destination nowhere (it has a-margin, b-margin)"),
            "{error}"
        );
    });
}

/// Sophia's clerks answer `{who, whatDone, dream, quote}`, with no note: the
/// comment says the item's other text fields in its own order, as the
/// workflow pane's `composedNote` does.
#[test]
fn publish_writes_an_items_other_fields_when_it_has_no_note() {
    with_graph(|ctx| {
        let (id, _) = create(ctx, "Margins");
        ok(
            ctx,
            "workflow_plan_compose",
            json!({ "documentId": id, "rationale": "the plan", "moves": finding_moves() }),
        );
        ok(
            ctx,
            "write_document",
            json!({ "document_id": "alpha", "title": "alpha", "format": "markdown",
                    "content": "The quick brown fox jumps.\n\nOver the lazy dog it goes." }),
        );
        write_run(
            ctx,
            &format!("urn:sophia:wf:{id}"),
            "r1",
            &[
                (
                    "findings",
                    json!({ "who": "Meier", "whatDone": "straw between toes", "dream": "a stake",
                            "quote": "quick brown fox", "documentId": "alpha" }),
                ),
                (
                    "findings",
                    json!({ "documentId": "alpha", "quote": "lazy dog", "seen_by": " the Censor ",
                            "page": 3, "aside": "   ", "dream--kind": "waking", "blockId": "",
                            "names": ["not", "text"] }),
                ),
                (
                    "findings",
                    json!({ "documentId": "alpha", "quote": "Over the" }),
                ),
                (
                    "findings",
                    json!({ "documentId": "alpha", "quote": "jumps", "note": "  ", "who": "Maury" }),
                ),
                (
                    "findings",
                    json!({ "documentId": "alpha", "quote": "goes", "who": "x".repeat(700) }),
                ),
            ],
        );
        let published = ok(
            ctx,
            "workflow_plan_publish",
            json!({ "documentId": id, "runId": "r1" }),
        );
        assert_eq!(published["placed"], 4, "{published}");
        assert_eq!(
            published["skipped"], 1,
            "the quote alone says nothing: {published}"
        );
        let comments = stored_comments(ctx, "alpha");
        assert_eq!(
            comments["wf-r1-1"]["text"],
            "who: Meier; what done: straw between toes; dream: a stake"
        );
        assert_eq!(
            comments["wf-r1-2"]["text"],
            "seen by: the Censor; dream kind: waking"
        );
        assert!(!comments.contains_key("wf-r1-3"), "{comments:?}");
        assert_eq!(comments["wf-r1-4"]["text"], "who: Maury");
        let long = comments["wf-r1-5"]["text"].as_str().unwrap();
        assert_eq!(long.chars().count(), 600, "{long}");
        assert!(long.starts_with("who: xxx"), "{long}");
        assert_eq!(comments["wf-r1-1"]["quotedText"], "quick brown fox");
    });
}
