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
    let compose = crate::loopback_scopes::mcp_tool_scopes("workflow_plan_compose");
    assert!(compose.contains(&"rdf.update"), "{compose:?}");
    let create = crate::loopback_scopes::mcp_tool_scopes("workflow_plan_create");
    for scope in ["rdf.update", "workspace.write.crdt", "documents.write.crdt"] {
        assert!(create.contains(&scope), "{scope} not in {create:?}");
    }
}
