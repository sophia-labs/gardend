//! Typed objects end to end (plans/2026-10-20-typed-objects, slice T1 on
//! gardend's side), through the real MCP dispatch table against a
//! disposable graph, as `workflow_plan_tests` drives the workflow tools:
//! define a dream report with `propose_domain_ontology`, read it back with
//! `emporium_vocab` (and its HTTP twin), and reach `emporium_describe`
//! through the router. No mocks.
use crate::mcp_dispatch_registry::McpCallCtx;
use crate::workflow_plan_tests::{
    call, client_token, loopback_state, ok, rpc, tool_call, with_graph, GRAPH, MASTER,
};
use serde_json::{json, Value};

const NS: &str = "urn:sophia:type:workflow-plan:workspace#";

/// The dream report (and the stimulus it nests, once and as a list), compact
/// form: the shape of Shrubbery's fixture `freud-dreams-workspace.vocab.json`.
fn dream_report() -> Vec<Value> {
    vec![
        json!({
            "name": "DreamReport",
            "label": "Dream report",
            "plural": "Dream reports",
            "description": "A dream an experimenter induced in a sleeper, as the book reports it.",
            "fields": [
                { "key": "who", "label": "Who", "datatype": "string", "required": true,
                  "description": "The sleeper, as the book names them" },
                { "key": "procedure", "label": "What was done", "datatype": "string", "required": true },
                { "key": "dream", "label": "The dream", "datatype": "string", "required": true },
                { "key": "quote", "label": "Quote", "datatype": "string", "required": true,
                  "description": "The book's words, copied exactly" },
                { "key": "documentId", "label": "Document", "datatype": "string", "required": true },
                { "key": "blockId", "label": "Block", "datatype": "string" },
                { "key": "page", "label": "Page", "datatype": "integer" },
                { "key": "stimulus", "label": "Stimulus", "datatype": "object", "class": "Stimulus",
                  "description": "What reached the sleeper's senses" },
                { "key": "witnesses", "label": "Witnesses", "datatype": "object", "class": "Stimulus",
                  "multi": true }
            ],
            "presentation": {
                "title": "who",
                "subtitle": "procedure",
                "roles": { "quote": "quote", "note": "dream", "document": "documentId", "block": "blockId" }
            }
        }),
        json!({
            "name": "Stimulus", "label": "Stimulus", "plural": "Stimuli",
            "fields": [
                { "key": "sense", "label": "Sense", "datatype": "string", "required": true },
                { "key": "what", "label": "What", "datatype": "string" }
            ],
            "presentation": { "title": "what" }
        }),
    ]
}

fn define(ctx: &McpCallCtx<'_>, classes: Vec<Value>) -> Value {
    ok(
        ctx,
        "propose_domain_ontology",
        json!({ "classes": classes, "rationale": "what the clerks hand back" }),
    )
}

fn catalog_class<'a>(read: &'a Value, name: &str) -> &'a Value {
    read["classCatalog"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|entry| entry["name"] == name)
        .unwrap_or_else(|| panic!("no {name} in {}", read["classCatalog"]))
}

#[test]
fn defining_a_dream_report_reads_back_and_a_second_definition_is_version_two() {
    with_graph(|ctx| {
        let defined = define(ctx, dream_report());
        assert_eq!(defined["status"], "ok", "{defined}");
        assert_eq!(defined["ontologyName"], "workspace");
        assert_eq!(defined["ontologyVersion"], "1");
        assert_eq!(defined["namespace"], NS);
        let report = catalog_class(&defined, "DreamReport");
        assert_eq!(report["iri"], format!("{NS}DreamReport"));
        assert_eq!(report["jsonSchema"]["title"], "Dream report");
        assert_eq!(
            report["jsonSchema"]["properties"]["stimulus"]["title"],
            "Stimulus"
        );
        assert_eq!(
            report["jsonSchema"]["properties"]["witnesses"]["type"],
            "array"
        );
        assert_eq!(
            report["jsonSchema"]["properties"]["witnesses"]["items"]["title"],
            "Stimulus"
        );

        // The catalogue lists it beside the built-ins.
        let listed = ok(ctx, "emporium_vocab", json!({}));
        let rows = listed["vocabularies"].as_array().unwrap();
        assert_eq!(rows.iter().filter(|r| r["source"] == "builtin").count(), 23);
        let domain: Vec<&Value> = rows.iter().filter(|r| r["source"] == "domain").collect();
        assert_eq!(domain.len(), 1, "{listed}");
        assert_eq!(domain[0]["name"], "workspace");
        assert_eq!(domain[0]["status"], "active");
        assert_eq!(domain[0]["classes"], json!(["DreamReport", "Stimulus"]));
        // The list answer carries the graph's classes too (both call shapes).
        assert_eq!(domain[0]["classCatalog"], defined["classCatalog"]);
        assert_eq!(listed["classCatalog"], defined["classCatalog"]);

        // Reading it back.
        let read = ok(ctx, "emporium_vocab", json!({ "name": "workspace" }));
        println!(
            "TYPES_READ_DREAM_REPORT {}",
            serde_json::to_string_pretty(&read).unwrap()
        );
        assert_eq!(read["source"], "domain");
        assert_eq!(read["version"], "1");
        assert_eq!(read["status"], "active");
        assert_eq!(read["classCatalog"], defined["classCatalog"]);
        let report = catalog_class(&read, "DreamReport");
        assert_eq!(report["label"], "Dream report");
        assert_eq!(report["plural"], "Dream reports");
        assert_eq!(report["presentation"]["title"], "who");
        assert_eq!(report["presentation"]["roles"]["note"], "dream");
        let keys: Vec<&str> = report["fields"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["key"].as_str().unwrap())
            .collect();
        assert_eq!(
            keys,
            [
                "who",
                "procedure",
                "dream",
                "quote",
                "documentId",
                "blockId",
                "page",
                "stimulus",
                "witnesses"
            ]
        );
        let witnesses = &report["fields"][8];
        assert_eq!(witnesses["datatype"], "object");
        assert_eq!(witnesses["multi"], true);
        assert_eq!(witnesses["class"], format!("{NS}Stimulus"));
        assert_eq!(
            catalog_class(&read, "Stimulus")["iri"],
            format!("{NS}Stimulus")
        );

        // Defining exactly the same again is not a new version.
        let again = define(ctx, dream_report());
        assert_eq!(again["ontologyVersion"], "1", "{again}");

        // A second definition is version 2; version 1 stays readable.
        let mut changed = dream_report();
        changed[0]["fields"]
            .as_array_mut()
            .unwrap()
            .push(json!({ "key": "mood", "label": "Mood", "datatype": "string" }));
        let second = define(ctx, changed);
        assert_eq!(second["ontologyVersion"], "2", "{second}");
        let latest = ok(ctx, "emporium_vocab", json!({ "name": "workspace" }));
        assert_eq!(latest["version"], "2");
        assert!(catalog_class(&latest, "DreamReport")["fields"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f["key"] == "mood"));
        let first = ok(
            ctx,
            "emporium_vocab",
            json!({ "name": "workspace", "version": "1" }),
        );
        assert_eq!(first["version"], "1");
        assert_eq!(first["status"], "superseded");
        assert_eq!(first["supersededBy"], latest["ontologySubject"]);
        assert!(!catalog_class(&first, "DreamReport")["fields"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f["key"] == "mood"));
        assert_eq!(latest["versions"].as_array().unwrap().len(), 2);

        // A cycle is refused, loudly, and nothing is stored.
        let cycle = call(
            ctx,
            "propose_domain_ontology",
            json!({ "vocab": "loops", "classes": [
                { "name": "A", "fields": [{ "key": "b", "datatype": "object", "class": "B" }] },
                { "name": "B", "fields": [{ "key": "a", "datatype": "object", "class": "A" }] }
            ] }),
        )
        .expect_err("a cycle is refused");
        assert!(cycle.contains("cycle (A → B → A"), "{cycle}");
        let listed = ok(ctx, "emporium_vocab", json!({}));
        assert!(!listed["vocabularies"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["name"] == "loops"));

        // The full form still works, and its classes are read the same way.
        let full = ok(
            ctx,
            "propose_domain_ontology",
            json!({ "contract": {
                "name": "notes", "version": "1.0.0", "title": "Notes", "description": "Notes",
                "namespaces": { "n": "urn:sophia:type:workflow-plan:notes#" },
                "primary_prefix": "n", "write_target": "projection:types",
                "classes": { "Note": {
                    "label": "Note", "rdf_types": ["n:Note"],
                    "subject_rule": "{graph_subject}:projection:types:note:{localId}",
                    "source_kind": "current-state", "identity_kind": "logical-id",
                    "store_mode": "materialize", "store_target": "projection:types",
                    "enforcement": "halt", "reconciliation_strategy": "codeBacked",
                    "dispatch_mode": "current-state-materialize",
                    "predicates": { "n:text": { "datatype": "string", "required": true, "label": "Text" } }
                } }
            } }),
        );
        assert_eq!(catalog_class(&full, "Note")["fields"][0]["label"], "Text");

        // A built-in answers too: its label falls back to the class name.
        let bookmark = ok(
            ctx,
            "emporium_vocab",
            json!({ "name": "emporium-bookmark" }),
        );
        assert_eq!(bookmark["source"], "builtin");
        assert_eq!(catalog_class(&bookmark, "Bookmark")["label"], "Bookmark");
        assert_eq!(
            catalog_class(&bookmark, "Bookmark")["presentation"],
            json!({})
        );
    });
}

#[test]
fn the_type_tools_are_registered_and_a_reader_reads_types_through_the_router() {
    assert!(crate::mcp_dispatch_registry::lookup("emporium_describe").is_some());
    assert!(crate::mcp_tool_registry::is_known_mcp_tool_name(
        "emporium_describe"
    ));
    assert_eq!(
        crate::loopback_scopes::mcp_tool_scopes("emporium_describe"),
        vec!["rdf.query"]
    );
    assert_eq!(
        crate::loopback_scopes::mcp_tool_scopes("emporium_vocab"),
        vec!["rdf.query"]
    );
    assert_eq!(
        crate::loopback_scopes::mcp_tool_scopes("propose_domain_ontology"),
        vec!["rdf.update", "memory.write"]
    );
    with_graph(|ctx| {
        let dir =
            std::env::temp_dir().join(format!("garden-typed-objects-{}", uuid::Uuid::new_v4()));
        let state = loopback_state(ctx.app.clone(), &dir);
        let reader = client_token(
            &ctx.app,
            None,
            Some(&["mcp.tools.read", "mcp.tools.call", "rdf.query"]),
        );
        crate::app_runtime::async_runtime::block_on(async {
            let refused = tool_call(
                &state,
                &reader,
                "propose_domain_ontology",
                json!({ "classes": dream_report() }),
            )
            .await
            .expect_err("a reader cannot define types");
            assert!(refused.contains("rdf.update"), "{refused}");
            tool_call(
                &state,
                MASTER,
                "propose_domain_ontology",
                json!({ "classes": dream_report() }),
            )
            .await
            .expect("the owner defines them");
            let read = tool_call(
                &state,
                &reader,
                "emporium_vocab",
                json!({ "name": "workspace" }),
            )
            .await
            .expect("a reader reads the graph's types");
            assert_eq!(read["classCatalog"][0]["label"], "Dream report");
            let described = tool_call(
                &state,
                &reader,
                "emporium_describe",
                json!({ "iri": "urn:example:nothing" }),
            )
            .await
            .expect("a reader describes");
            assert_eq!(described["known"], false);
            let listed = rpc(&state, &reader, "tools/list", json!({})).await.unwrap();
            assert!(listed["tools"]
                .as_array()
                .unwrap()
                .iter()
                .any(|tool| tool["name"] == "emporium_describe"));
        });
        // The HTTP twin serves the same catalogue (graph-bound, rdf.query).
        crate::app_runtime::async_runtime::block_on(async {
            use tower::ServiceExt;
            let request = axum::http::Request::builder()
                .method("GET")
                .uri(format!("/emporium/vocabs/{GRAPH}/workspace/latest"))
                .header("authorization", format!("Bearer {reader}"))
                .body(axum::body::Body::empty())
                .unwrap();
            let response = crate::loopback_router::loopback_router(state.clone())
                .oneshot(request)
                .await
                .unwrap();
            assert_eq!(response.status(), 200);
            let bytes = axum::body::to_bytes(response.into_body(), 10_000_000)
                .await
                .unwrap();
            let body: Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(body["classCatalog"][0]["iri"], format!("{NS}DreamReport"));
            let request = axum::http::Request::builder()
                .method("GET")
                .uri(format!(
                    "/emporium/vocabs/{GRAPH}/workspace/1?format=turtle"
                ))
                .header("authorization", format!("Bearer {reader}"))
                .body(axum::body::Body::empty())
                .unwrap();
            let response = crate::loopback_router::loopback_router(state.clone())
                .oneshot(request)
                .await
                .unwrap();
            assert_eq!(response.status(), 200);
            let bytes = axum::body::to_bytes(response.into_body(), 10_000_000)
                .await
                .unwrap();
            let turtle = String::from_utf8(bytes.to_vec()).unwrap();
            assert!(
                turtle.contains(&format!("sh:class <{NS}Stimulus>")),
                "{turtle}"
            );
            assert!(
                turtle.contains("<urn:sophia:shacl:workspace#StimulusShape> a sh:NodeShape"),
                "{turtle}"
            );
        });
        let _ = std::fs::remove_dir_all(&dir);
    });
}
