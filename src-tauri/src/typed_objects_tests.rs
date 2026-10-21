//! Typed objects end to end (plans/2026-10-20-typed-objects, slices T1 and T2
//! on gardend's side), through the real MCP dispatch table against a
//! disposable graph, as `workflow_plan_tests` drives the workflow tools:
//! define a dream report with `propose_domain_ontology`, read it back with
//! `emporium_vocab`, bind a workflow step to it with `workflow_plan_compose`,
//! record a run whose items become dream reports (or unfit ones) with
//! `workflow_run_record`, find them with SPARQL, publish them by their
//! class's roles, and describe them with `emporium_describe`. No mocks.
use crate::mcp_dispatch_registry::McpCallCtx;
use crate::workflow_plan_tests::{
    call, client_token, create, graph_rows, loopback_state, messages, ok, rpc, step,
    stored_comments, tool_call, with_graph, GRAPH, MASTER, WF,
};
use serde_json::{json, Value};

const NS: &str = "urn:sophia:type:workflow-plan:workspace#";
const CLERK: &str = "urn:sophia:agent:agent-62517a586738e5e9";
const CENSOR: &str = "urn:sophia:agent:agent-c3n50r";
const ENDED: &str = "2026-10-20T19:58:03Z";

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

/// Reader → clerk (a dream report per passage) → censor (checks them) →
/// the book's margin.
fn dream_moves(item_class: Value) -> Value {
    let mut clerk = json!({
        "op": "upsert_step", "id": "clerk", "phase": "p2", "label": "Experiment clerk",
        "agentType": "extractor", "mode": "perItem", "over": "passages", "produces": "candidates",
        "instructions": "Report each experiment on a sleeper as a dream report.",
    });
    if !item_class.is_null() {
        clerk["itemClass"] = item_class;
    }
    json!([
        { "op": "set_title", "title": "Things done to sleepers" },
        { "op": "upsert_phase", "id": "p1", "title": "Read", "order": 1 },
        { "op": "upsert_phase", "id": "p2", "title": "Report", "order": 2 },
        { "op": "upsert_phase", "id": "p3", "title": "Check", "order": 3 },
        {
            "op": "upsert_step", "id": "reader", "phase": "p1", "label": "the Reader",
            "agentType": "segmenter", "instructions": "Cut the book into passages.", "mode": "once",
            "produces": "passages", "itemSchema": { "type": "object", "properties": { "text": { "type": "string" } } },
        },
        clerk,
        {
            "op": "upsert_step", "id": "censor", "phase": "p3", "label": "the Censor",
            "agentType": "quote checker", "checks": "candidates", "produces": "verified",
            "instructions": "Pass a report only if its quote is in the book.",
        },
        {
            "op": "upsert_destination", "id": "margin", "label": "The book's margin",
            "receives": "verified", "documentId": "freud-ch1",
        },
    ])
}

fn draft_rows(ctx: &McpCallCtx<'_>, workflow: &str, step: &str) -> Vec<(String, String)> {
    graph_rows(ctx, &format!("{workflow}:draft"))
        .into_iter()
        .filter(|row| row["s"] == format!("<{workflow}#{step}>"))
        .map(|row| {
            (
                row["p"].as_str().unwrap().to_string(),
                row["o"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

#[test]
fn compose_binds_a_step_to_a_class_and_names_an_unknown_one() {
    with_graph(|ctx| {
        define(ctx, dream_report());
        write_freud(ctx);
        let (id, _) = create(ctx, "Dreams");
        let workflow = format!("urn:sophia:wf:{id}");

        // An unknown class: the move is accepted and the report names it.
        let unknown = ok(
            ctx,
            "workflow_plan_compose",
            json!({ "documentId": id, "rationale": "the plan",
                    "moves": dream_moves(json!("workspace:Nightmare")) }),
        );
        assert!(
            messages(&unknown).contains(
                &"clerk's wf:itemClass names Nightmare, which is not a type of this graph"
                    .to_string()
            ),
            "{}",
            unknown["report"]
        );
        assert!(
            !messages(&unknown)
                .iter()
                .any(|m| m.contains("has no wf:itemSchema")),
            "the class problem is the one said: {}",
            unknown["report"]
        );
        assert_eq!(unknown["report"]["conforms"], false);
        assert_eq!(
            step(&unknown, "clerk")["itemClass"],
            json!({ "iri": format!("{NS}Nightmare"), "known": false })
        );
        let rows = draft_rows(ctx, &workflow, "clerk");
        assert!(rows.contains(&(format!("<{WF}itemClass>"), format!("<{NS}Nightmare>"))));
        assert!(!rows.iter().any(|(p, _)| p == &format!("<{WF}itemVocab>")));

        // A known class: the three triples, and the derived schema read back.
        let known = ok(
            ctx,
            "workflow_plan_compose",
            json!({ "documentId": id, "rationale": "the class exists",
                    "moves": dream_moves(json!("workspace:DreamReport")) }),
        );
        assert_eq!(known["report"]["conforms"], true, "{}", known["report"]);
        let rows = draft_rows(ctx, &workflow, "clerk");
        assert!(rows.contains(&(format!("<{WF}itemClass>"), format!("<{NS}DreamReport>"))));
        assert!(rows.contains(&(format!("<{WF}itemVocab>"), "\"workspace@1\"".to_string())));
        let schema_literal = rows
            .iter()
            .find(|(p, _)| p == &format!("<{WF}itemSchema>"))
            .map(|(_, o)| o.clone())
            .expect("wf:itemSchema");
        let schema_text: String = serde_json::from_str(&schema_literal).unwrap();
        let types = ok(ctx, "emporium_vocab", json!({ "name": "workspace" }));
        let derived = &catalog_class(&types, "DreamReport")["jsonSchema"];
        assert_eq!(
            &serde_json::from_str::<Value>(&schema_text).unwrap(),
            derived
        );
        let clerk = step(&known, "clerk");
        assert_eq!(&clerk["itemSchema"], derived);
        assert_eq!(clerk["itemVocab"], "workspace@1");
        assert_eq!(clerk["itemClass"]["label"], "Dream report");
        assert_eq!(clerk["itemClass"]["plural"], "Dream reports");
        assert_eq!(clerk["itemClass"]["fields"][0]["label"], "Who");
        // The stacks: a check's items are the class of what it checks.
        let collections = known["collections"].as_array().unwrap();
        let stack = |id: &str| collections.iter().find(|c| c["id"] == id).unwrap();
        assert!(stack("passages").get("itemClass").is_none());
        assert_eq!(stack("candidates")["itemClass"]["plural"], "Dream reports");
        assert_eq!(stack("verified")["itemClass"]["label"], "Dream report");
        assert_eq!(stack("verified")["producedBy"], "censor");

        // The IRI form resolves the same way.
        let by_iri = ok(
            ctx,
            "workflow_plan_compose",
            json!({ "documentId": id, "rationale": "by IRI",
                    "moves": dream_moves(json!(format!("{NS}DreamReport"))) }),
        );
        assert_eq!(by_iri["report"]["conforms"], true, "{}", by_iri["report"]);
        assert_eq!(step(&by_iri, "clerk")["itemVocab"], "workspace@1");

        // Both at once is refused, and so is a reference that is neither form.
        let mut both = dream_moves(json!("workspace:DreamReport"));
        both[5]["itemSchema"] = json!({ "type": "object" });
        let error = call(
            ctx,
            "workflow_plan_compose",
            json!({ "documentId": id, "rationale": "x", "moves": both }),
        )
        .expect_err("itemClass and itemSchema together");
        assert!(
            error.contains("give itemClass or itemSchema, not both"),
            "{error}"
        );
        let error = call(
            ctx,
            "workflow_plan_compose",
            json!({ "documentId": id, "rationale": "x", "moves": dream_moves(json!("DreamReport")) }),
        )
        .expect_err("neither an IRI nor vocab:Class");
        assert!(
            error.contains("neither an IRI nor <vocab>:<Class>"),
            "{error}"
        );

        // The old itemSchema path is unchanged: no class anywhere.
        let (plain, _) = create(ctx, "Plain");
        let mut moves = dream_moves(Value::Null);
        moves[5]["itemSchema"] = json!({ "type": "object", "required": ["who"] });
        let read = ok(
            ctx,
            "workflow_plan_compose",
            json!({ "documentId": plain, "rationale": "a hand schema", "moves": moves }),
        );
        assert_eq!(read["report"]["conforms"], true, "{}", read["report"]);
        let clerk = step(&read, "clerk");
        assert!(clerk.get("itemClass").is_none(), "{clerk}");
        assert!(clerk.get("itemVocab").is_none(), "{clerk}");
        assert_eq!(
            clerk["itemSchema"],
            json!({ "type": "object", "required": ["who"] })
        );
        assert!(read["collections"]
            .as_array()
            .unwrap()
            .iter()
            .all(|c| c.get("itemClass").is_none()));
    });
}

const BOOK: &str = "Maury had a blade of straw drawn between his toes, and dreamt of pirates.\n\n\
                    A drop of water fell on his forehead; he dreamt of Italy.";

fn write_freud(ctx: &McpCallCtx<'_>) {
    ok(
        ctx,
        "write_document",
        json!({ "document_id": "freud-ch1", "title": "Chapter I", "format": "markdown", "content": BOOK }),
    );
}

/// A finished run in Choreograph's shape, with the being cast per step
/// (`wf:castAgent`), the sitting per item, and Choreograph's own unfit record.
fn dream_run(workflow: &str, run_id: &str, items: &[(&str, Value)]) -> String {
    let run_graph = format!("{workflow}:run:{run_id}");
    let run = format!("<{run_graph}#run>");
    let lit = |value: &str| serde_json::to_string(value).unwrap();
    let mut lines = vec![
        format!("@prefix wf: <{WF}> ."),
        "@prefix prov: <http://www.w3.org/ns/prov#> .".to_string(),
        "@prefix xsd: <http://www.w3.org/2001/XMLSchema#> .".to_string(),
        format!(
            "{run} a wf:GraphPlanRun ; wf:runId {} ; wf:status \"completed\" ; wf:definition <{workflow}:v1> ; \
             wf:definitionDigest \"sha256:test\" ; wf:workflow <{workflow}> ; \
             prov:endedAtTime \"{ENDED}\"^^xsd:dateTime .",
            lit(run_id)
        ),
        format!(
            "<{run_graph}#step-2> a wf:StepRun ; wf:partOfRun {run} ; wf:step <{workflow}#clerk> ; \
             wf:runs 2 ; wf:produced 2 ; wf:castAgent <{CLERK}> ."
        ),
        format!(
            "<{run_graph}#step-3> a wf:StepRun ; wf:partOfRun {run} ; wf:step <{workflow}#censor> ; \
             wf:runs 2 ; wf:produced 1 ; wf:refused 1 ; wf:castAgent <{CENSOR}> ."
        ),
        format!(
            "<{run_graph}#unfit-1> a wf:Unfit ; wf:partOfRun {run} ; wf:step <{workflow}#clerk> ; \
             wf:sitting \"Experiment clerk #2\" ; wf:violation \"the submit call did not fit after two tries\" ."
        ),
    ];
    let mut positions: std::collections::BTreeMap<&str, usize> = Default::default();
    for (n, (collection, item)) in items.iter().enumerate() {
        let position = positions.entry(collection).or_default();
        lines.push(format!(
            "<{run_graph}#item-{}> a wf:Item ; wf:partOfRun {run} ; wf:inCollection <{workflow}#{collection}> ; \
             wf:position {position} ; wf:json {} ; wf:sitting \"Experiment clerk #3\" .",
            n + 1,
            lit(&item.to_string())
        ));
        *position += 1;
    }
    lines.join("\n")
}

fn meier() -> Value {
    json!({
        "who": "Meier", "procedure": "a blade of straw drawn between his toes",
        "dream": "He was tortured by Arab pirates.", "quote": "a blade of straw drawn between his toes",
        "documentId": "freud-ch1", "page": 33,
        "stimulus": { "sense": "touch", "what": "a straw between the toes" },
        "witnesses": [
            { "sense": "report", "what": "Meier's own account (1758)" },
            { "sense": "citation", "what": "cited by Freud from Strümpell" }
        ]
    })
}

fn misfit() -> Value {
    json!({
        "who": "Maury", "procedure": "a drop of water on his forehead",
        "quote": "A drop of water fell on his forehead", "documentId": "freud-ch1",
        "page": "thirty-three", "stimulus": { "what": "water" },
        "witnesses": [{ "sense": "report" }, { "what": "nobody" }], "mood": "warm"
    })
}

fn select(ctx: &McpCallCtx<'_>, query: String) -> Vec<Value> {
    ok(ctx, "sparql_query", json!({ "query": query }))["rows"]
        .as_array()
        .cloned()
        .unwrap_or_default()
}

/// The typed-objects workflow, defined, composed and recorded.
fn recorded_dream_run(ctx: &McpCallCtx<'_>) -> (String, String, Value) {
    define(ctx, dream_report());
    write_freud(ctx);
    let (id, _) = create(ctx, "Dreams");
    ok(
        ctx,
        "workflow_plan_compose",
        json!({ "documentId": id, "rationale": "the plan",
                "moves": dream_moves(json!("workspace:DreamReport")) }),
    );
    let workflow = format!("urn:sophia:wf:{id}");
    let turtle = dream_run(
        &workflow,
        "r1",
        &[
            (
                "passages",
                json!({ "text": "Maury had a blade of straw..." }),
            ),
            ("candidates", meier()),
            ("candidates", misfit()),
            ("verified", meier()),
        ],
    );
    let recorded = ok(
        ctx,
        "workflow_run_record",
        json!({ "documentId": id, "runId": "r1", "turtle": turtle }),
    );
    (id, workflow, recorded)
}

#[test]
fn a_run_of_a_class_bound_step_records_dream_reports_and_unfit_ones() {
    with_graph(|ctx| {
        let (id, workflow, recorded) = recorded_dream_run(ctx);
        let run_graph = format!("{workflow}:run:r1");
        let mut sample: Vec<String> = graph_rows(ctx, &run_graph)
            .iter()
            .filter(|r| {
                let s = r["s"].as_str().unwrap_or_default();
                s.starts_with(&format!("<{run_graph}#item-4"))
                    || s == format!("<{run_graph}#item-3>")
            })
            .map(|r| {
                format!(
                    "{} {} {} .",
                    r["s"].as_str().unwrap(),
                    r["p"].as_str().unwrap(),
                    r["o"].as_str().unwrap()
                )
            })
            .collect();
        sample.sort();
        println!(
            "RUN_ITEM_TRIPLES\n{}\nEND_RUN_ITEM_TRIPLES",
            sample.join("\n")
        );
        assert_eq!(recorded["typed"], 2, "{recorded}");
        assert_eq!(recorded["unfit"], 1, "{recorded}");

        // SPARQL finds the dream reports by field (GRAPH ?g, as the pane does).
        let found = select(
            ctx,
            format!(
                "PREFIX t: <{NS}> SELECT ?x ?sense WHERE {{ GRAPH ?g {{ \
                 ?x a t:DreamReport ; t:who \"Meier\" ; t:page 33 ; t:stimulus ?s . \
                 ?s a t:Stimulus ; t:sense ?sense }} }} ORDER BY ?x"
            ),
        );
        assert_eq!(
            found
                .iter()
                .map(|r| r["x"].as_str().unwrap().to_string())
                .collect::<Vec<_>>(),
            vec![
                format!("<{run_graph}#item-2>"),
                format!("<{run_graph}#item-4>")
            ],
            "{found:?}"
        );
        assert_eq!(found[0]["sense"], "\"touch\"");
        // A nested list: one child per member, <item>/witnesses/<i> from 0.
        let witnesses = select(
            ctx,
            format!(
                "PREFIX t: <{NS}> SELECT ?w ?sense WHERE {{ GRAPH <{run_graph}> {{ \
                 <{run_graph}#item-2> t:witnesses ?w . ?w a t:Stimulus ; t:sense ?sense }} }} ORDER BY ?w"
            ),
        );
        assert_eq!(
            witnesses
                .iter()
                .map(|r| (
                    r["w"].as_str().unwrap().to_string(),
                    r["sense"].as_str().unwrap().to_string()
                ))
                .collect::<Vec<_>>(),
            vec![
                (
                    format!("<{run_graph}#item-2/witnesses/0>"),
                    "\"report\"".to_string()
                ),
                (
                    format!("<{run_graph}#item-2/witnesses/1>"),
                    "\"citation\"".to_string()
                ),
            ]
        );

        // The misfit is unfit, with its violations, and is no dream report.
        let unfit = select(
            ctx,
            format!(
                "PREFIX wf: <{WF}> SELECT ?v WHERE {{ GRAPH <{run_graph}> {{ \
                 <{run_graph}#item-3> a wf:Unfit ; wf:itemClass <{NS}DreamReport> ; wf:violation ?v }} }}"
            ),
        );
        let violations: Vec<String> = unfit
            .iter()
            .map(|r| serde_json::from_str::<String>(r["v"].as_str().unwrap()).unwrap())
            .collect();
        for expected in [
            "dream is missing; a Dream report needs it",
            "page: expected a whole number, got \"thirty-three\"",
            "stimulus.sense is missing; a Stimulus needs it",
            "witnesses[1].sense is missing; a Stimulus needs it",
        ] {
            assert!(violations.iter().any(|v| v == expected), "{violations:?}");
        }
        assert!(violations
            .iter()
            .any(|v| v.starts_with("mood is not a field of Dream report")));
        let types: Vec<String> = select(
            ctx,
            format!("SELECT ?t WHERE {{ GRAPH ?g {{ <{run_graph}#item-3> a ?t }} }}"),
        )
        .iter()
        .map(|r| r["t"].as_str().unwrap().to_string())
        .collect();
        assert!(types.contains(&format!("<{WF}Unfit>")), "{types:?}");
        assert!(!types.contains(&format!("<{NS}DreamReport>")), "{types:?}");

        // Testimony: the maker made it, the checker checked it, the run ended then.
        let testimony = |item: &str| -> Vec<(String, String)> {
            graph_rows(ctx, &run_graph)
                .into_iter()
                .filter(|r| r["s"] == format!("<{run_graph}#{item}>"))
                .map(|r| {
                    (
                        r["p"].as_str().unwrap().to_string(),
                        r["o"].as_str().unwrap().to_string(),
                    )
                })
                .collect()
        };
        let verified = testimony("item-4");
        let has = |rows: &[(String, String)], p: &str, o: &str| {
            rows.iter().any(|(rp, ro)| rp == p && ro == o)
        };
        assert!(
            has(
                &verified,
                &format!("<{WF}producedBy>"),
                &format!("<{CLERK}>")
            ),
            "{verified:?}"
        );
        assert!(
            has(
                &verified,
                &format!("<{WF}checkedBy>"),
                &format!("<{CENSOR}>")
            ),
            "{verified:?}"
        );
        assert!(!has(
            &verified,
            &format!("<{WF}producedBy>"),
            &format!("<{CENSOR}>")
        ));
        assert!(has(
            &verified,
            "<http://www.w3.org/ns/prov#generatedAtTime>",
            &format!("\"{ENDED}\"^^<http://www.w3.org/2001/XMLSchema#dateTime>")
        ));
        assert!(has(
            &verified,
            &format!("<{WF}itemVocab>"),
            "\"workspace@1\""
        ));
        assert!(has(
            &verified,
            &format!("<{WF}sitting>"),
            "\"Experiment clerk #3\""
        ));
        let candidate = testimony("item-2");
        assert!(has(
            &candidate,
            &format!("<{WF}producedBy>"),
            &format!("<{CLERK}>")
        ));
        assert!(!candidate
            .iter()
            .any(|(p, _)| p == &format!("<{WF}checkedBy>")));
        // An untyped collection is left as it was.
        let passage = testimony("item-1");
        assert!(!passage
            .iter()
            .any(|(p, _)| p == &format!("<{WF}producedBy>")));
        assert_eq!(passage.len(), 6, "{passage:?}");
        // Choreograph's own unfit record is taken as it came.
        assert_eq!(testimony("unfit-1").len(), 5);

        // Twice is once (the time is the run's, not the recording's).
        let before = graph_rows(ctx, &run_graph).len();
        let again = ok(
            ctx,
            "workflow_run_record",
            json!({ "documentId": id, "runId": "r1", "turtle": dream_run(&workflow, "r1", &[
                ("passages", json!({ "text": "Maury had a blade of straw..." })),
                ("candidates", meier()), ("candidates", misfit()), ("verified", meier()),
            ]) }),
        );
        assert_eq!(again, recorded);
        assert_eq!(graph_rows(ctx, &run_graph).len(), before);

        // Publish reads the class's roles: the note is the dream itself.
        let published = ok(
            ctx,
            "workflow_plan_publish",
            json!({ "documentId": id, "runId": "r1" }),
        );
        assert_eq!(published["placed"], 1, "{published}");
        let comments = stored_comments(ctx, "freud-ch1");
        assert_eq!(
            comments["wf-r1-1"]["text"], "He was tortured by Arab pirates.",
            "{comments:?}"
        );
    });
}

#[test]
fn describe_reads_a_dream_report_an_unfit_one_and_an_unknown_iri() {
    with_graph(|ctx| {
        let (_, workflow, _) = recorded_dream_run(ctx);
        let run_graph = format!("{workflow}:run:r1");
        let describe = |iri: String| ok(ctx, "emporium_describe", json!({ "iri": iri }));

        let report = describe(format!("{run_graph}#item-4"));
        println!(
            "DESCRIBE_DREAM_REPORT {}",
            serde_json::to_string_pretty(&report).unwrap()
        );
        assert_eq!(report["known"], true);
        assert_eq!(report["stance"], "read");
        assert_eq!(report["class"]["iri"], format!("{NS}DreamReport"));
        assert_eq!(report["class"]["label"], "Dream report");
        assert_eq!(report["class"]["version"], "1");
        assert_eq!(report["title"], "Meier");
        assert_eq!(
            report["subtitle"],
            "a blade of straw drawn between his toes"
        );
        let field = |key: &str| {
            report["fields"]
                .as_array()
                .unwrap()
                .iter()
                .find(|f| f["key"] == key)
                .unwrap()
                .clone()
        };
        assert_eq!(field("who")["label"], "Who");
        assert_eq!(field("who")["value"], "Meier");
        assert_eq!(field("blockId")["value"], Value::Null);
        assert_eq!(field("page")["value"], 33);
        let stimulus = field("stimulus")["value"].clone();
        assert_eq!(stimulus["iri"], format!("{run_graph}#item-4/stimulus"));
        assert_eq!(stimulus["class"]["label"], "Stimulus");
        assert_eq!(stimulus["title"], "a straw between the toes");
        assert_eq!(
            stimulus["fields"][0],
            json!({ "key": "sense", "label": "Sense", "datatype": "string", "value": "touch" })
        );
        let witnesses = field("witnesses")["value"].clone();
        assert_eq!(witnesses.as_array().map(Vec::len), Some(2), "{witnesses}");
        assert_eq!(
            witnesses[0]["iri"],
            format!("{run_graph}#item-4/witnesses/0")
        );
        assert_eq!(witnesses[1]["title"], "cited by Freud from Strümpell");
        assert_eq!(report["testimony"]["by"]["agentIri"], CLERK);
        assert_eq!(report["testimony"]["sitting"], "Experiment clerk #3");
        assert_eq!(report["testimony"]["run"]["id"], "r1");
        assert_eq!(
            report["testimony"]["run"]["iri"],
            format!("{run_graph}#run")
        );
        assert_eq!(report["testimony"]["at"], ENDED);

        let unfit = describe(format!("{run_graph}#item-3"));
        assert_eq!(unfit["stance"], "unfit");
        assert_eq!(unfit["known"], true);
        assert_eq!(unfit["class"]["label"], "Dream report");
        assert!(unfit["violations"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v == "dream is missing; a Dream report needs it"));
        assert_eq!(unfit["title"], "Maury");
        assert_eq!(unfit["extra"], json!([{ "key": "mood", "value": "warm" }]));

        // Choreograph's own unfit record: the class its step names.
        let theirs = describe(format!("{run_graph}#unfit-1"));
        assert_eq!(theirs["stance"], "unfit");
        assert_eq!(theirs["class"]["label"], "Dream report");
        assert_eq!(theirs["testimony"]["sitting"], "Experiment clerk #2");
        assert_eq!(
            theirs["violations"],
            json!(["the submit call did not fit after two tries"])
        );

        // An untyped item and an IRI nothing is said about: typed "unknown".
        let passage = describe(format!("{run_graph}#item-1"));
        assert_eq!(passage["known"], false);
        assert_eq!(passage["class"], Value::Null);
        assert_eq!(passage["stance"], "read");
        assert!(!passage["triples"].as_array().unwrap().is_empty());
        let nothing = describe("urn:example:nothing-here".to_string());
        assert_eq!(nothing["known"], false);
        assert_eq!(nothing["found"], false);
        assert_eq!(nothing["triples"], json!([]));
        let error = call(ctx, "emporium_describe", json!({ "iri": "not an iri" }))
            .expect_err("a malformed IRI is a request error");
        assert!(error.contains("is not an IRI"), "{error}");
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
