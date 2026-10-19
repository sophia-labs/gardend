//! A workflow plan that is RDF: `workflow_plan_create`, `workflow_plan_read`
//! and `workflow_plan_compose` (Vera, 2026-10-18: "keep the richness but make
//! it RDF native").
//!
//! A workflow is a document whose workspace `documentKind` is `"workflow"`.
//! Its plan, the program a run executes, is the named graph
//! `urn:sophia:wf:<documentId>:draft` in Choreograph's graph-plan-v1 grammar
//! (normative in `choreograph/src/workflow/graph-plan.ts`, `compileGraphPlan`):
//!
//! ```text
//! <urn:sophia:wf:D> a wf:Workflow ; wf:name "D" ; dcterms:title "…" .
//! <…#p1> a wf:Phase ; wf:partOfWorkflow <urn:sophia:wf:D> ; wf:order 1 ; dcterms:title "…" .
//! <…#reader> a wf:AgentNode ; wf:inPhase <…#p1> ; wf:label "…" ; wf:agentType "…" ;
//!     wf:instructions "…" ; wf:mode "once"|"perItem" ; wf:over <…#c> ; wf:consumes <…#c> ;
//!     wf:produces <…#c> ; wf:itemSchema "{json schema}" ; wf:checks <…#c> .
//! ```
//!
//! The ideas come from the workflow book (`workflow_book_mcp.rs`: compose moves,
//! live validation, a `wf:CompositionEvent` per change), not its script
//! anatomy; the book is unchanged. A compose applies its moves in one SPARQL
//! update and records one `wf:CompositionEvent` in
//! `urn:sophia:wf:<documentId>:authoring`. Validation never blocks a draft
//! (FlagAndAccept): SHACL shapes (`workflow_plan_shapes.ttl`, run by rudof as
//! Emporium's EA-1 face does) judge each node, and the structural checks here
//! judge what SHACL Core cannot say, worded as the TS compiler words them.
//!
//! The fence: every tool refuses a document whose kind is not `"workflow"`,
//! and writes only that document's `:draft` and `:authoring` graphs.
use crate::{
    app_error::{AppError, AppResult},
    app_runtime::AppHandle,
    emporium::shacl_validator::{validate_against_shapes, ViolationRecord},
    emporium::terms::{Term as ShaclTerm, Triple as ShaclTriple},
    mcp_arg_utils::{mcp_arg_string, mcp_required_graph_id},
    rdf_service::SparqlUpdateInput,
};
use oxigraph::model::{GraphNameRef, Literal, NamedNode, NamedOrBlankNode, Term, Triple};
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::sync::OnceLock;

const WF: &str = "http://mnemosyne.dev/workflow#";
const RDF_TYPE: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";
const DCT_TITLE: &str = "http://purl.org/dc/terms/title";
const PROV: &str = "http://www.w3.org/ns/prov#";
const XSD_INTEGER: &str = "http://www.w3.org/2001/XMLSchema#integer";
const XSD_DATE_TIME: &str = "http://www.w3.org/2001/XMLSchema#dateTime";
const SH_MESSAGE: &str = "http://www.w3.org/ns/shacl#message";
const WORKFLOW_KIND: &str = "workflow";
/// The graph-plan-v1 shapes (SHACL Core), one constraint per named property
/// shape so each `sh:message` says exactly what is wrong.
const SHAPES_TTL: &str = include_str!("workflow_plan_shapes.ttl");
/// `GRAPH_PLAN_LIMITS` in graph-plan.ts; the length limits are in the shapes.
const MAX_PHASES: usize = 6;
const MAX_STEPS: usize = 12;
const RECENT_EVENTS: usize = 10;
const ID_MAX: usize = 64;

/// The names one workflow document owns.
#[derive(Debug, Clone)]
struct PlanNames {
    document_id: String,
    workflow: String,
    draft: String,
    authoring: String,
}

impl PlanNames {
    fn new(document_id: &str) -> Self {
        let workflow = format!("urn:sophia:wf:{document_id}");
        Self {
            document_id: document_id.to_string(),
            draft: format!("{workflow}:draft"),
            authoring: format!("{workflow}:authoring"),
            workflow,
        }
    }

    fn item(&self, id: &str) -> String {
        format!("{}#{id}", self.workflow)
    }
}

// ── the three tools ─────────────────────────────────────────────────────────

pub(super) async fn mcp_local_workflow_plan_create(
    app: AppHandle,
    arguments: &Value,
) -> AppResult<Value> {
    let graph_id = mcp_required_graph_id(arguments).map_err(AppError::validation)?;
    let title = mcp_arg_string(arguments, &["title"])
        .map(|title| title.trim().to_string())
        .filter(|title| !title.is_empty())
        .ok_or_else(|| AppError::validation("title is required"))?;
    let mut create = json!({
        "graphId": graph_id,
        "title": title,
        "documentKind": WORKFLOW_KIND,
    });
    if let Some(parent_id) = mcp_arg_string(arguments, &["parentId", "parent_id"]) {
        create["parentId"] = json!(parent_id);
    }
    let created =
        crate::mcp_workspace_crdt_service::mcp_local_create_document(app.clone(), &create)
            .await
            .map_err(AppError::internal)?;
    let document_id = created
        .get("documentId")
        .and_then(Value::as_str)
        .ok_or_else(|| AppError::internal("create_document did not say which document it made"))?
        .to_string();
    let names = PlanNames::new(&document_id);
    crate::document_mutation_service::mcp_local_write_document(
        app.clone(),
        &json!({
            "graph_id": graph_id,
            "document_id": document_id,
            "format": "markdown",
            "content": format!(
                "This document is a workflow. Its plan is RDF in the named graph `{}`. \
                 Read it with `workflow_plan_read`, change it with `workflow_plan_compose` \
                 (or raw RDF in that graph); the person approves and runs it from the \
                 workflow pane.",
                names.draft
            ),
        }),
    )
    .await
    .map_err(AppError::internal)?;
    let workflow = NamedNode::new_unchecked(names.workflow.clone());
    let seed = vec![
        Triple::new(
            workflow.clone(),
            iri(RDF_TYPE),
            iri(&format!("{WF}Workflow")),
        ),
        Triple::new(
            workflow.clone(),
            iri(&format!("{WF}name")),
            text(&document_id),
        ),
        Triple::new(workflow, iri(DCT_TITLE), text(&title)),
    ];
    let event = composition_event(&names, "created by workflow_plan_create", &json!([]), 3, 0);
    let update = format!(
        "{} ;\n{}",
        insert_data(&names.draft, &seed),
        insert_data(&names.authoring, &event)
    );
    run_update(&app, &graph_id, update).await?;
    read_result(&app, &graph_id, &names)
}

pub(super) async fn mcp_local_workflow_plan_read(
    app: AppHandle,
    arguments: &Value,
) -> AppResult<Value> {
    let (graph_id, names) = workflow_document(&app, arguments).await?;
    read_result(&app, &graph_id, &names)
}

pub(super) async fn mcp_local_workflow_plan_compose(
    app: AppHandle,
    arguments: &Value,
) -> AppResult<Value> {
    let (graph_id, names) = workflow_document(&app, arguments).await?;
    let rationale = mcp_arg_string(arguments, &["rationale"])
        .map(|rationale| rationale.trim().to_string())
        .filter(|rationale| !rationale.is_empty())
        .ok_or_else(|| AppError::validation("rationale is required: say in a sentence why"))?;
    let moves_value = arguments
        .get("moves")
        .filter(|moves| moves.as_array().is_some_and(|moves| !moves.is_empty()))
        .ok_or_else(|| AppError::validation("moves is required: a non-empty list of moves"))?;
    let moves = moves_value
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
        .map(|(index, value)| parse_move(index + 1, value))
        .collect::<Result<Vec<_>, _>>()
        .map_err(AppError::validation)?;
    let before = load_graph(&app, &graph_id, &names.draft)?;
    let (after, operations) = apply_moves(&names, &before, &moves).map_err(AppError::validation)?;
    let before_set: HashSet<&Triple> = before.iter().collect();
    let after_set: HashSet<&Triple> = after.iter().collect();
    let inserted = after_set.difference(&before_set).count();
    let deleted = before_set.difference(&after_set).count();
    let event = composition_event(&names, &rationale, moves_value, inserted, deleted);
    let mut update = operations.join(" ;\n");
    update.push_str(" ;\n");
    update.push_str(&insert_data(&names.authoring, &event));
    run_update(&app, &graph_id, update).await?;
    read_result(&app, &graph_id, &names)
}

/// One finding a person may publish (Shrubbery's `publishableFindings`): an
/// item of the plan's last collection that names the `documentId` it is
/// about, the exact `quote` it rests on, the `note` to write beside it and,
/// optionally, the `blockId` that holds the quote.
#[derive(Debug, Clone)]
struct Finding {
    /// 1-based: `wf:position` + 1, the `n` of its comment id `wf-<runId>-<n>`.
    index: i64,
    document_id: String,
    quote: String,
    note: String,
    block_id: Option<String>,
}

/// `runId` as a graph and comment-id segment (`graphPlanRunGraphIri`).
fn run_segment(run_id: &str) -> String {
    run_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn finding_of(index: i64, json: &str) -> Option<Finding> {
    let value: Value = serde_json::from_str(json).ok()?;
    let text = |key: &str| {
        value
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(str::to_string)
    };
    let document_id = value.get("documentId").and_then(Value::as_str)?;
    let id_shaped = (1..=128).contains(&document_id.len())
        && document_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if !id_shaped {
        return None;
    }
    Some(Finding {
        index,
        document_id: document_id.to_string(),
        quote: text("quote")?,
        note: text("note")?,
        block_id: value
            .get("blockId")
            .and_then(Value::as_str)
            .filter(|block| !block.is_empty())
            .map(str::to_string),
    })
}

/// The plan's last collection: what the first producing step of its last
/// phase produces (as the workflow pane reads it).
fn last_collection(draft: &[Triple], names: &PlanNames) -> Option<String> {
    let plan = Plan::new(draft);
    let workflows = plan.typed("Workflow");
    let workflow = if workflows.len() == 1 {
        workflows[0]
    } else {
        names.workflow.as_str()
    };
    let mut phases: Vec<(i64, &str)> = plan
        .typed("Phase")
        .into_iter()
        .filter(|phase| {
            plan.iris(phase, &format!("{WF}partOfWorkflow"))
                .contains(&workflow)
        })
        .map(|phase| (phase_order(&plan, phase).unwrap_or(i64::MAX), phase))
        .collect();
    phases.sort();
    let (_, last) = phases.last()?;
    plan.typed("AgentNode")
        .into_iter()
        .filter(|step| plan.iri_of(step, &format!("{WF}inPhase")) == Some(*last))
        .find_map(|step| plan.iri_of(step, &format!("{WF}produces")))
        .map(str::to_string)
}

pub(super) async fn mcp_local_workflow_plan_publish(
    app: AppHandle,
    arguments: &Value,
) -> AppResult<Value> {
    let started = std::time::Instant::now();
    let (graph_id, names) = workflow_document(&app, arguments).await?;
    let run_id = mcp_arg_string(arguments, &["runId", "run_id"])
        .map(|run_id| run_id.trim().to_string())
        .filter(|run_id| !run_id.is_empty())
        .ok_or_else(|| AppError::validation("runId is required"))?;
    let segment = run_segment(&run_id);
    let run_graph = format!("{}:run:{segment}", names.workflow);
    let draft = load_graph(&app, &graph_id, &names.draft)?;
    let collection = last_collection(&draft, &names).ok_or_else(|| {
        AppError::validation(
            "the plan has no last collection: its last phase has no step that produces one",
        )
    })?;
    let run_triples = load_graph(&app, &graph_id, &run_graph)?;
    let run = Plan::new(&run_triples);
    let Some(run_node) = run.typed("GraphPlanRun").first().copied() else {
        return Err(AppError::validation(format!(
            "there is no run {run_id} of this workflow (no wf:GraphPlanRun in {run_graph})"
        )));
    };
    let mut items: Vec<(i64, &str)> = run
        .typed("Item")
        .into_iter()
        .filter(|item| run.iri_of(item, &format!("{WF}inCollection")) == Some(collection.as_str()))
        .filter_map(|item| {
            let position = run
                .literal(item, &format!("{WF}position"))?
                .trim()
                .parse::<i64>()
                .ok()?;
            Some((position, item))
        })
        .collect();
    items.sort();
    let mut skipped = 0usize;
    let mut by_document: Vec<(String, Vec<Finding>)> = Vec::new();
    for (position, item) in &items {
        let finding = run
            .literal(item, &format!("{WF}json"))
            .and_then(|json| finding_of(position + 1, json));
        let Some(finding) = finding else {
            skipped += 1;
            continue;
        };
        match by_document
            .iter_mut()
            .find(|(document, _)| *document == finding.document_id)
        {
            Some((_, findings)) => findings.push(finding),
            None => by_document.push((finding.document_id.clone(), vec![finding])),
        }
    }
    let author = plan_view(&draft, &names)["title"]
        .as_str()
        .unwrap_or("Workflow")
        .to_string();

    // One document.editComments per target document: one Y.Doc transaction
    // and one re-projection for all of its comments.
    let mut placed_total = 0usize;
    let mut not_found: Vec<Value> = Vec::new();
    let mut documents: Vec<Value> = Vec::new();
    let mut targets: Vec<String> = Vec::new();
    for (document_id, findings) in &by_document {
        let comment_id = |finding: &Finding| format!("wf-{segment}-{}", finding.index);
        let comments: Vec<Value> = findings
            .iter()
            .map(|finding| {
                let mut comment = json!({
                    "commentId": comment_id(finding),
                    "text": finding.note,
                    "find": finding.quote,
                    "quotedText": finding.quote,
                });
                if let Some(block_id) = &finding.block_id {
                    comment["blockId"] = json!(block_id);
                }
                comment
            })
            .collect();
        let result = crate::crdt_queue::enqueue_crdt_operation(
            app.clone(),
            crate::crdt_queue::EnqueueCrdtOperationInput {
                kind: "document.editComments".to_string(),
                graph_id: graph_id.clone(),
                document_id: Some(document_id.clone()),
                payload: json!({
                    "documentId": document_id,
                    "author": author,
                    "comments": comments,
                }),
            },
        )
        .await
        .map_err(AppError::internal)?;
        let placed = result["placed"].as_array().map_or(0, Vec::len);
        placed_total += placed;
        let missed = result["notFound"].as_array().cloned().unwrap_or_default();
        for miss in &missed {
            let Some(finding) = findings
                .iter()
                .find(|finding| miss["commentId"].as_str() == Some(comment_id(finding).as_str()))
            else {
                continue;
            };
            not_found.push(json!({
                "index": finding.index,
                "documentId": document_id,
                "quote": finding.quote,
                "reason": miss["reason"],
            }));
        }
        let in_graph = !missed.iter().any(|miss| {
            miss["reason"].as_str().is_some_and(|reason| {
                reason.starts_with(&format!("document {document_id} is not in graph"))
            })
        });
        if in_graph {
            targets.push(document_id.clone());
        }
        documents.push(json!({
            "documentId": document_id,
            "inGraph": in_graph,
            "findings": findings.len(),
            "placed": placed,
            "notFound": missed.len(),
        }));
    }
    not_found.sort_by_key(|miss| miss["index"].as_i64().unwrap_or_default());

    // One wf:Publication in the authoring trail.
    let publication = NamedNode::new_unchecked(format!(
        "{}:publication:{}",
        names.authoring,
        uuid::Uuid::new_v4().simple()
    ));
    let p = |name: &str| iri(&format!("{WF}{name}"));
    let at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let mut record = vec![
        Triple::new(publication.clone(), iri(RDF_TYPE), p("Publication")),
        Triple::new(
            publication.clone(),
            iri(RDF_TYPE),
            iri(&format!("{PROV}Activity")),
        ),
        Triple::new(
            publication.clone(),
            p("definitionSubject"),
            NamedNode::new_unchecked(names.workflow.clone()),
        ),
        Triple::new(
            publication.clone(),
            p("run"),
            NamedNode::new_unchecked(run_node.to_string()),
        ),
        Triple::new(publication.clone(), p("runId"), text(&run_id)),
        Triple::new(
            publication.clone(),
            p("collection"),
            NamedNode::new_unchecked(collection.clone()),
        ),
        Triple::new(
            publication.clone(),
            p("placedCount"),
            integer(placed_total as i64),
        ),
        Triple::new(
            publication.clone(),
            p("notFoundCount"),
            integer(not_found.len() as i64),
        ),
        Triple::new(
            publication.clone(),
            p("notFoundJson"),
            text(&Value::Array(not_found.clone()).to_string()),
        ),
        Triple::new(
            publication.clone(),
            iri(&format!("{PROV}generatedAtTime")),
            Literal::new_typed_literal(at, NamedNode::new_unchecked(XSD_DATE_TIME)),
        ),
    ];
    for target in &targets {
        record.push(Triple::new(
            publication.clone(),
            p("targetDocument"),
            NamedNode::new_unchecked(crate::rdf::document_subject(target)),
        ));
    }
    for miss in &not_found {
        if let Some(quote) = miss["quote"].as_str() {
            record.push(Triple::new(
                publication.clone(),
                p("notFoundQuote"),
                text(quote),
            ));
        }
    }
    if let Some(actor) = crate::request_actor::current_request_actor() {
        record.push(Triple::new(
            publication.clone(),
            p("actor"),
            text(&actor.user_id),
        ));
        if let Some(via) = actor.via {
            record.push(Triple::new(publication.clone(), p("actorVia"), text(&via)));
        }
    }
    run_update(&app, &graph_id, insert_data(&names.authoring, &record)).await?;

    Ok(json!({
        "graphId": graph_id,
        "documentId": names.document_id,
        "runId": run_id,
        "runGraph": run_graph,
        "collection": local_id(&collection),
        "placed": placed_total,
        "notFound": not_found,
        "skipped": skipped,
        "documents": documents,
        "publication": publication.as_str(),
        "elapsedMs": started.elapsed().as_millis() as u64,
    }))
}

/// The fence: `documentId` must name a document of kind `"workflow"`.
async fn workflow_document(app: &AppHandle, arguments: &Value) -> AppResult<(String, PlanNames)> {
    let graph_id = mcp_required_graph_id(arguments).map_err(AppError::validation)?;
    let document_id = mcp_arg_string(arguments, &["documentId", "document_id"])
        .ok_or_else(|| AppError::validation("documentId is required"))?;
    crate::ids::validate_local_id(&document_id, "documentId").map_err(AppError::validation)?;
    let kind =
        crate::crdt_engine::document_ops::resolve_document_kind(app, &graph_id, &document_id)
            .await
            .map_err(AppError::internal)?;
    if kind.as_deref() != Some(WORKFLOW_KIND) {
        let has = kind.map_or_else(|| "no kind".to_string(), |kind| format!("kind {kind:?}"));
        return Err(AppError::validation(format!(
            "{document_id} is not a workflow document (it has {has}); make one with workflow_plan_create"
        )));
    }
    Ok((graph_id, PlanNames::new(&document_id)))
}

async fn run_update(app: &AppHandle, graph_id: &str, update: String) -> AppResult<()> {
    crate::sparql_admission::run_external_sparql_update(
        app.clone(),
        SparqlUpdateInput {
            graph_id: graph_id.to_string(),
            update,
        },
    )
    .await
    .map(|_| ())
}

fn load_graph(app: &AppHandle, graph_id: &str, graph_iri: &str) -> AppResult<Vec<Triple>> {
    let graph_dir =
        crate::graph_paths::existing_graph_dir(app, graph_id).map_err(AppError::storage)?;
    let store = crate::rdf_service::open_graph_store(&graph_dir).map_err(AppError::rdf)?;
    let graph =
        NamedNode::new(graph_iri).map_err(|error| AppError::validation(error.to_string()))?;
    store
        .quads_for_pattern(
            None,
            None,
            None,
            Some(GraphNameRef::NamedNode(graph.as_ref())),
        )
        .map(|quad| quad.map(|quad| Triple::new(quad.subject, quad.predicate, quad.object)))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| AppError::rdf(format!("read {graph_iri}: {error}")))
}

fn read_result(app: &AppHandle, graph_id: &str, names: &PlanNames) -> AppResult<Value> {
    let draft = load_graph(app, graph_id, &names.draft)?;
    let authoring = load_graph(app, graph_id, &names.authoring)?;
    let problems = report(&draft, &names.draft);
    let mut result = plan_view(&draft, names);
    result["graphId"] = json!(graph_id);
    result["documentId"] = json!(names.document_id);
    result["workflow"] = json!(names.workflow);
    result["draftGraph"] = json!(names.draft);
    result["authoringGraph"] = json!(names.authoring);
    result["report"] = json!({
        "conforms": problems.is_empty(),
        "problems": problems.iter().map(Problem::to_json).collect::<Vec<_>>(),
    });
    result["authoring"] = json!({ "recent": recent_events(&authoring) });
    Ok(result)
}

// ── moves ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
enum Move {
    SetTitle {
        title: String,
    },
    UpsertPhase {
        id: String,
        title: String,
        order: i64,
    },
    UpsertStep(StepMove),
    Remove {
        id: String,
    },
}

#[derive(Debug, Clone)]
struct StepMove {
    id: String,
    phase: String,
    label: String,
    agent_type: String,
    instructions: String,
    mode: Option<String>,
    over: Option<String>,
    consumes: Vec<String>,
    checks: Option<String>,
    produces: Option<String>,
    item_schema: Option<String>,
}

const MOVE_OPS: &str = "set_title, upsert_phase, upsert_step or remove";

fn parse_move(index: usize, value: &Value) -> Result<Move, String> {
    let object = value
        .as_object()
        .ok_or_else(|| format!("move {index} is not an object"))?;
    let op = object
        .get("op")
        .and_then(Value::as_str)
        .ok_or_else(|| format!("move {index} has no op; use {MOVE_OPS}"))?;
    let at = format!("move {index} ({op})");
    let allowed: &[&str] = match op {
        "set_title" => &["op", "title"],
        "upsert_phase" => &["op", "id", "title", "order"],
        "upsert_step" => &[
            "op",
            "id",
            "phase",
            "label",
            "agentType",
            "instructions",
            "mode",
            "over",
            "consumes",
            "checks",
            "produces",
            "itemSchema",
        ],
        "remove" => &["op", "id"],
        other => {
            return Err(format!(
                "move {index} has an unknown op {other:?}; use {MOVE_OPS}"
            ))
        }
    };
    if let Some(unknown) = object.keys().find(|key| !allowed.contains(&key.as_str())) {
        return Err(format!(
            "{at} has an unknown field {unknown:?}; it takes {}",
            allowed[1..].join(", ")
        ));
    }
    let text = |field: &str| -> Result<String, String> {
        object
            .get(field)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .ok_or_else(|| format!("{at} needs {field} (text)"))
    };
    let id_field = |field: &str| -> Result<String, String> {
        let id = object
            .get(field)
            .and_then(Value::as_str)
            .ok_or_else(|| format!("{at} needs {field} (an id)"))?;
        valid_id(&at, field, id)
    };
    let optional_id = |field: &str| -> Result<Option<String>, String> {
        match object.get(field) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(id)) => valid_id(&at, field, id).map(Some),
            Some(_) => Err(format!("{at}: {field} must be an id")),
        }
    };
    Ok(match op {
        "set_title" => Move::SetTitle {
            title: text("title")?,
        },
        "upsert_phase" => {
            let order = object
                .get("order")
                .and_then(|order| {
                    order.as_i64().or_else(|| {
                        order
                            .as_f64()
                            .filter(|f| f.fract() == 0.0)
                            .map(|f| f as i64)
                    })
                })
                .ok_or_else(|| format!("{at} needs order (a whole number)"))?;
            Move::UpsertPhase {
                id: id_field("id")?,
                title: text("title")?,
                order,
            }
        }
        "upsert_step" => {
            let mode = match object.get("mode") {
                None | Some(Value::Null) => None,
                Some(Value::String(mode)) if mode == "once" || mode == "perItem" => {
                    Some(mode.clone())
                }
                Some(other) => {
                    return Err(format!(
                        "{at}: mode is {other}; use \"once\" or \"perItem\""
                    ))
                }
            };
            let consumes = match object.get("consumes") {
                None | Some(Value::Null) => Vec::new(),
                Some(Value::String(id)) => vec![valid_id(&at, "consumes", id)?],
                Some(Value::Array(ids)) => ids
                    .iter()
                    .map(|id| {
                        id.as_str()
                            .ok_or_else(|| format!("{at}: consumes must be a list of ids"))
                            .and_then(|id| valid_id(&at, "consumes", id))
                    })
                    .collect::<Result<Vec<_>, _>>()?,
                Some(_) => return Err(format!("{at}: consumes must be a list of ids")),
            };
            let item_schema = match object.get("itemSchema") {
                None | Some(Value::Null) => None,
                Some(Value::String(schema)) => Some(schema.clone()),
                Some(schema @ Value::Object(_)) => Some(schema.to_string()),
                Some(_) => {
                    return Err(format!(
                        "{at}: itemSchema must be a JSON Schema object (or its JSON text)"
                    ))
                }
            };
            Move::UpsertStep(StepMove {
                id: id_field("id")?,
                phase: id_field("phase")?,
                label: text("label")?,
                agent_type: text("agentType")?,
                instructions: text("instructions")?,
                mode,
                over: optional_id("over")?,
                consumes,
                checks: optional_id("checks")?,
                produces: optional_id("produces")?,
                item_schema,
            })
        }
        _ => Move::Remove {
            id: id_field("id")?,
        },
    })
}

/// `^[A-Za-z][A-Za-z0-9_-]{0,63}$`: the fragment of `urn:sophia:wf:<documentId>#<id>`.
fn valid_id(at: &str, field: &str, id: &str) -> Result<String, String> {
    let mut chars = id.chars();
    let ok = chars
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        && id.len() <= ID_MAX;
    if ok {
        Ok(id.to_string())
    } else {
        Err(format!(
            "{at}: {field} {id:?} is not an id (a letter, then up to 63 letters, digits, _ or -)"
        ))
    }
}

/// Apply `moves` to the draft in memory (to check each against the state the
/// moves before it leave) and render the SPARQL operations that do the same
/// in the store. A move that cannot apply refuses the whole call.
fn apply_moves(
    names: &PlanNames,
    before: &[Triple],
    moves: &[Move],
) -> Result<(Vec<Triple>, Vec<String>), String> {
    let mut state: Vec<Triple> = before.to_vec();
    let mut operations = Vec::new();
    let workflow = NamedNode::new_unchecked(names.workflow.clone());
    let typed = |state: &[Triple], subject: &str, class: &str| {
        state.iter().any(|t| {
            subject_iri(t) == Some(subject)
                && t.predicate.as_str() == RDF_TYPE
                && matches!(&t.object, Term::NamedNode(o) if o.as_str() == format!("{WF}{class}"))
        })
    };
    let collection = |state: &[Triple], item: &str| {
        state.iter().any(|t| {
            ["over", "consumes", "produces", "checks"]
                .iter()
                .any(|p| t.predicate.as_str() == format!("{WF}{p}"))
                && matches!(&t.object, Term::NamedNode(o) if o.as_str() == item)
        })
    };
    for (index, change) in moves.iter().enumerate() {
        let at = |op: &str| format!("move {} ({op})", index + 1);
        match change {
            Move::SetTitle { title } => {
                state.retain(|t| {
                    !(subject_iri(t) == Some(names.workflow.as_str())
                        && t.predicate.as_str() == DCT_TITLE)
                });
                let added = vec![Triple::new(workflow.clone(), iri(DCT_TITLE), text(title))];
                operations.push(format!(
                    "DELETE WHERE {{ GRAPH <{}> {{ <{}> <{DCT_TITLE}> ?o }} }}",
                    names.draft, names.workflow
                ));
                operations.push(insert_data(&names.draft, &added));
                state.extend(added);
            }
            Move::UpsertPhase { id, title, order } => {
                let item = names.item(id);
                if typed(&state, &item, "AgentNode") {
                    return Err(format!(
                        "{}: {id} is a step; give the phase its own id",
                        at("upsert_phase")
                    ));
                }
                if collection(&state, &item) {
                    return Err(format!(
                        "{}: {id} names a collection; give the phase its own id",
                        at("upsert_phase")
                    ));
                }
                let phase = NamedNode::new_unchecked(item.clone());
                let added = vec![
                    Triple::new(phase.clone(), iri(RDF_TYPE), iri(&format!("{WF}Phase"))),
                    Triple::new(
                        phase.clone(),
                        iri(&format!("{WF}partOfWorkflow")),
                        workflow.clone(),
                    ),
                    Triple::new(phase.clone(), iri(&format!("{WF}order")), integer(*order)),
                    Triple::new(phase, iri(DCT_TITLE), text(title)),
                ];
                replace_subject(names, &mut state, &mut operations, &item, added);
            }
            Move::UpsertStep(step) => {
                let item = names.item(&step.id);
                let here = at("upsert_step");
                if typed(&state, &item, "Phase") {
                    return Err(format!(
                        "{here}: {} is a phase; give the step its own id",
                        step.id
                    ));
                }
                if collection(&state, &item) {
                    return Err(format!(
                        "{here}: {} names a collection; give the step its own id",
                        step.id
                    ));
                }
                let phase_item = names.item(&step.phase);
                if typed(&state, &phase_item, "AgentNode") || phase_item == item {
                    return Err(format!(
                        "{here}: phase {} is a step, not a phase",
                        step.phase
                    ));
                }
                let reads = step
                    .over
                    .iter()
                    .chain(step.consumes.iter())
                    .chain(step.checks.iter())
                    .chain(step.produces.iter());
                for read in reads {
                    let read_item = names.item(read);
                    if read_item == item
                        || typed(&state, &read_item, "AgentNode")
                        || typed(&state, &read_item, "Phase")
                    {
                        return Err(format!(
                            "{here}: {read} is a phase or a step, not a collection"
                        ));
                    }
                }
                let subject = NamedNode::new_unchecked(item.clone());
                let p = |name: &str| iri(&format!("{WF}{name}"));
                let collection_term =
                    |id: &str| Term::NamedNode(NamedNode::new_unchecked(names.item(id)));
                let mut added = vec![
                    Triple::new(
                        subject.clone(),
                        iri(RDF_TYPE),
                        iri(&format!("{WF}AgentNode")),
                    ),
                    Triple::new(
                        subject.clone(),
                        p("inPhase"),
                        NamedNode::new_unchecked(phase_item),
                    ),
                    Triple::new(subject.clone(), p("label"), text(&step.label)),
                    Triple::new(subject.clone(), p("agentType"), text(&step.agent_type)),
                    Triple::new(subject.clone(), p("instructions"), text(&step.instructions)),
                ];
                if let Some(mode) = &step.mode {
                    added.push(Triple::new(subject.clone(), p("mode"), text(mode)));
                }
                if let Some(over) = &step.over {
                    added.push(Triple::new(
                        subject.clone(),
                        p("over"),
                        collection_term(over),
                    ));
                }
                for consumed in &step.consumes {
                    added.push(Triple::new(
                        subject.clone(),
                        p("consumes"),
                        collection_term(consumed),
                    ));
                }
                if let Some(checks) = &step.checks {
                    added.push(Triple::new(
                        subject.clone(),
                        p("checks"),
                        collection_term(checks),
                    ));
                }
                if let Some(produces) = &step.produces {
                    added.push(Triple::new(
                        subject.clone(),
                        p("produces"),
                        collection_term(produces),
                    ));
                }
                if let Some(schema) = &step.item_schema {
                    added.push(Triple::new(subject.clone(), p("itemSchema"), text(schema)));
                }
                replace_subject(names, &mut state, &mut operations, &item, added);
            }
            Move::Remove { id } => {
                let item = names.item(id);
                let here = at("remove");
                if typed(&state, &item, "Phase") {
                    let held: BTreeSet<String> = state
                        .iter()
                        .filter(|t| {
                            t.predicate.as_str() == format!("{WF}inPhase")
                                && matches!(&t.object, Term::NamedNode(o) if o.as_str() == item)
                        })
                        .filter_map(|t| subject_iri(t).map(local_id))
                        .collect();
                    if !held.is_empty() {
                        return Err(format!(
                            "{here}: phase {id} still holds {}; remove or move those steps first",
                            held.into_iter().collect::<Vec<_>>().join(", ")
                        ));
                    }
                } else if !typed(&state, &item, "AgentNode") {
                    return Err(format!(
                        "{here}: there is no phase or step {id} in this plan"
                    ));
                }
                replace_subject(names, &mut state, &mut operations, &item, Vec::new());
            }
        }
    }
    Ok((state, operations))
}

/// Replace every triple of `item` with `added`, in memory and as SPARQL.
fn replace_subject(
    names: &PlanNames,
    state: &mut Vec<Triple>,
    operations: &mut Vec<String>,
    item: &str,
    added: Vec<Triple>,
) {
    state.retain(|t| subject_iri(t) != Some(item));
    operations.push(format!(
        "DELETE WHERE {{ GRAPH <{}> {{ <{item}> ?p ?o }} }}",
        names.draft
    ));
    if !added.is_empty() {
        operations.push(insert_data(&names.draft, &added));
        state.extend(added);
    }
}

fn composition_event(
    names: &PlanNames,
    rationale: &str,
    moves: &Value,
    inserted: usize,
    deleted: usize,
) -> Vec<Triple> {
    let event = NamedNode::new_unchecked(format!(
        "{}:event:{}",
        names.authoring,
        uuid::Uuid::new_v4().simple()
    ));
    let at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let p = |name: &str| iri(&format!("{WF}{name}"));
    let mut triples = vec![
        Triple::new(
            event.clone(),
            iri(RDF_TYPE),
            iri(&format!("{WF}CompositionEvent")),
        ),
        Triple::new(
            event.clone(),
            iri(RDF_TYPE),
            iri(&format!("{PROV}Activity")),
        ),
        Triple::new(
            event.clone(),
            p("definitionSubject"),
            NamedNode::new_unchecked(names.workflow.clone()),
        ),
        Triple::new(
            event.clone(),
            iri(&format!("{PROV}generatedAtTime")),
            Literal::new_typed_literal(at, NamedNode::new_unchecked(XSD_DATE_TIME)),
        ),
        Triple::new(event.clone(), p("rationale"), text(rationale)),
        Triple::new(event.clone(), p("movesJson"), text(&moves.to_string())),
        Triple::new(
            event.clone(),
            p("insertedTripleCount"),
            integer(inserted as i64),
        ),
        Triple::new(
            event.clone(),
            p("deletedTripleCount"),
            integer(deleted as i64),
        ),
    ];
    if let Some(actor) = crate::request_actor::current_request_actor() {
        triples.push(Triple::new(event.clone(), p("actor"), text(&actor.user_id)));
        if let Some(via) = actor.via {
            triples.push(Triple::new(event, p("actorVia"), text(&via)));
        }
    }
    triples
}

fn insert_data(graph: &str, triples: &[Triple]) -> String {
    let body = triples
        .iter()
        .map(|t| format!("{t} ."))
        .collect::<Vec<_>>()
        .join("\n  ");
    format!("INSERT DATA {{ GRAPH <{graph}> {{\n  {body}\n}} }}")
}

fn iri(value: &str) -> NamedNode {
    NamedNode::new_unchecked(value)
}

fn text(value: &str) -> Literal {
    Literal::new_simple_literal(value)
}

fn integer(value: i64) -> Literal {
    Literal::new_typed_literal(value.to_string(), NamedNode::new_unchecked(XSD_INTEGER))
}

// ── reading the graph ───────────────────────────────────────────────────────

fn subject_iri(triple: &Triple) -> Option<&str> {
    match &triple.subject {
        NamedOrBlankNode::NamedNode(node) => Some(node.as_str()),
        _ => None,
    }
}

/// graph-plan.ts `localId`: the last `#`, `/` or `:` segment, made id-safe.
fn local_id(iri: &str) -> String {
    let tail = iri
        .split(|c| c == '#' || c == '/' || c == ':')
        .filter(|part| !part.is_empty())
        .last()
        .unwrap_or(iri);
    tail.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// The draft indexed by named subject.
struct Plan<'a> {
    by_subject: BTreeMap<&'a str, Vec<&'a Triple>>,
}

impl<'a> Plan<'a> {
    fn new(triples: &'a [Triple]) -> Self {
        let mut by_subject: BTreeMap<&str, Vec<&Triple>> = BTreeMap::new();
        for triple in triples {
            if let Some(subject) = subject_iri(triple) {
                by_subject.entry(subject).or_default().push(triple);
            }
        }
        Self { by_subject }
    }

    /// Subjects typed `wf:<class>`, sorted.
    fn typed(&self, class: &str) -> Vec<&'a str> {
        let class = format!("{WF}{class}");
        self.by_subject
            .iter()
            .filter(|(_, triples)| {
                triples.iter().any(|t| {
                    t.predicate.as_str() == RDF_TYPE
                        && matches!(&t.object, Term::NamedNode(o) if o.as_str() == class)
                })
            })
            .map(|(subject, _)| *subject)
            .collect()
    }

    fn values(&self, subject: &str, predicate: &str) -> Vec<&'a Term> {
        self.by_subject
            .get(subject)
            .into_iter()
            .flatten()
            .filter(|t| t.predicate.as_str() == predicate)
            .map(|t| &t.object)
            .collect()
    }

    /// The first value, when it is text (graph-plan.ts `literal`).
    fn literal(&self, subject: &str, predicate: &str) -> Option<&'a str> {
        match self.values(subject, predicate).first() {
            Some(Term::Literal(literal)) => Some(literal.value()),
            _ => None,
        }
    }

    /// The first value, when it is an IRI (graph-plan.ts `iriOf`).
    fn iri_of(&self, subject: &str, predicate: &str) -> Option<&'a str> {
        match self.values(subject, predicate).first() {
            Some(Term::NamedNode(node)) => Some(node.as_str()),
            _ => None,
        }
    }

    /// Every IRI value, sorted (graph-plan.ts `iris`).
    fn iris(&self, subject: &str, predicate: &str) -> Vec<&'a str> {
        let mut iris: Vec<&str> = self
            .values(subject, predicate)
            .into_iter()
            .filter_map(|term| match term {
                Term::NamedNode(node) => Some(node.as_str()),
                _ => None,
            })
            .collect();
        iris.sort_unstable();
        iris
    }
}

/// The plan as `workflow_plan_read` shows it: title, phases in order with
/// their steps, and any step that sits in no phase of this workflow.
fn plan_view(triples: &[Triple], names: &PlanNames) -> Value {
    let plan = Plan::new(triples);
    let workflows = plan.typed("Workflow");
    let workflow = if workflows.len() == 1 {
        workflows[0]
    } else {
        names.workflow.as_str()
    };
    let title = plan
        .literal(workflow, DCT_TITLE)
        .or_else(|| plan.literal(workflow, &format!("{WF}name")));
    let mut phases: Vec<(Option<i64>, &str)> = plan
        .typed("Phase")
        .into_iter()
        .filter(|phase| {
            plan.iris(phase, &format!("{WF}partOfWorkflow"))
                .contains(&workflow)
        })
        .map(|phase| (phase_order(&plan, phase), phase))
        .collect();
    phases.sort_by_key(|(order, phase)| (order.unwrap_or(i64::MAX), *phase));
    let mut placed: BTreeSet<&str> = BTreeSet::new();
    let phase_values: Vec<Value> = phases
        .iter()
        .map(|(order, phase)| {
            let steps: Vec<Value> = plan
                .typed("AgentNode")
                .into_iter()
                .filter(|step| plan.iri_of(step, &format!("{WF}inPhase")) == Some(*phase))
                .map(|step| {
                    placed.insert(step);
                    step_view(&plan, step)
                })
                .collect();
            json!({
                "id": local_id(phase),
                "title": plan.literal(phase, DCT_TITLE),
                "order": order,
                "steps": steps,
            })
        })
        .collect();
    let unplaced: Vec<Value> = plan
        .typed("AgentNode")
        .into_iter()
        .filter(|step| !placed.contains(step))
        .map(|step| step_view(&plan, step))
        .collect();
    let mut view = json!({ "title": title, "phases": phase_values });
    if !unplaced.is_empty() {
        view["unplacedSteps"] = json!(unplaced);
    }
    view
}

fn phase_order(plan: &Plan<'_>, phase: &str) -> Option<i64> {
    plan.literal(phase, &format!("{WF}order"))
        .and_then(|order| order.trim().parse::<i64>().ok())
}

fn step_view(plan: &Plan<'_>, step: &str) -> Value {
    let p = |name: &str| format!("{WF}{name}");
    let short = |iri: Option<&str>| iri.map(local_id);
    let checks = plan.iri_of(step, &p("checks"));
    let mode = plan
        .literal(step, &p("mode"))
        .map(str::to_string)
        .unwrap_or_else(|| if checks.is_some() { "perItem" } else { "once" }.to_string());
    let item_schema = plan.literal(step, &p("itemSchema")).map(|schema| {
        serde_json::from_str::<Value>(schema)
            .ok()
            .filter(Value::is_object)
            .unwrap_or_else(|| json!(schema))
    });
    json!({
        "id": local_id(step),
        "label": plan.literal(step, &p("label")),
        "agentType": plan.literal(step, &p("agentType")),
        "mode": mode,
        "over": short(plan.iri_of(step, &p("over"))),
        "consumes": plan.iris(step, &p("consumes")).into_iter().map(local_id).collect::<Vec<_>>(),
        "checks": short(checks),
        "produces": short(plan.iri_of(step, &p("produces"))),
        "instructions": plan.literal(step, &p("instructions")),
        "itemSchema": item_schema,
    })
}

fn recent_events(triples: &[Triple]) -> Vec<Value> {
    let plan = Plan::new(triples);
    let p = |name: &str| format!("{WF}{name}");
    let mut events: Vec<(String, &str)> = plan
        .typed("CompositionEvent")
        .into_iter()
        .map(|event| {
            let at = plan
                .literal(event, &format!("{PROV}generatedAtTime"))
                .unwrap_or_default()
                .to_string();
            (at, event)
        })
        .collect();
    events.sort_by(|a, b| b.cmp(a));
    events
        .into_iter()
        .take(RECENT_EVENTS)
        .map(|(at, event)| {
            let moves = plan
                .literal(event, &p("movesJson"))
                .and_then(|moves| serde_json::from_str::<Value>(moves).ok())
                .unwrap_or_else(|| json!([]));
            let mut row = Map::new();
            row.insert("at".into(), json!(at));
            row.insert("actor".into(), json!(plan.literal(event, &p("actor"))));
            if let Some(via) = plan.literal(event, &p("actorVia")) {
                row.insert("via".into(), json!(via));
            }
            row.insert(
                "rationale".into(),
                json!(plan.literal(event, &p("rationale"))),
            );
            row.insert("moves".into(), moves);
            Value::Object(row)
        })
        .collect()
}

// ── the report ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
struct Problem {
    focus: String,
    message: String,
    source: &'static str,
}

impl Problem {
    fn structure(focus: &str, message: String) -> Self {
        Self {
            focus: focus.to_string(),
            message,
            source: "structure",
        }
    }

    fn to_json(&self) -> Value {
        json!({ "focus": self.focus, "message": self.message, "source": self.source })
    }
}

/// Every problem with the draft: SHACL first, then the structural checks,
/// one entry per message (as graph-plan.ts dedupes its problems).
fn report(triples: &[Triple], draft_iri: &str) -> Vec<Problem> {
    let mut seen = BTreeSet::new();
    shacl_problems(triples, draft_iri)
        .into_iter()
        .chain(structure_problems(triples, draft_iri))
        .filter(|problem| seen.insert(problem.message.clone()))
        .collect()
}

fn shacl_problems(triples: &[Triple], draft_iri: &str) -> Vec<Problem> {
    let data: Vec<ShaclTriple> = triples
        .iter()
        .filter_map(|t| {
            let subject = subject_iri(t)?;
            let object = match &t.object {
                Term::NamedNode(node) => ShaclTerm::Uri(node.clone()),
                Term::Literal(literal) => ShaclTerm::Lit(literal.clone()),
                _ => return None,
            };
            Some((
                subject.to_string(),
                t.predicate.as_str().to_string(),
                object,
            ))
        })
        .collect();
    let Err(violations) = validate_against_shapes(SHAPES_TTL, &data, draft_iri, "graph-plan-v1")
    else {
        return Vec::new();
    };
    let mut problems: Vec<Problem> = violations.iter().map(shacl_problem).collect();
    problems.sort_by(|a, b| (&a.focus, &a.message).cmp(&(&b.focus, &b.message)));
    problems
}

/// A violation in the compiler's words: the shape's `sh:message` with
/// `{$this}`, `{?value}` and `{?short}` filled in.
fn shacl_problem(violation: &ViolationRecord) -> Problem {
    let focus = bare_iri(&violation.focus_node).to_string();
    let template = violation
        .shape
        .as_deref()
        .and_then(|shape| shape_messages().get(bare_iri(shape)))
        .cloned()
        .unwrap_or_else(|| format!("{{$this}}: {}", violation.message));
    let (value, short) = match violation.offending_value.as_deref().map(str::trim) {
        Some(raw) if raw.starts_with('"') => {
            let inner = literal_lexical(raw);
            (format!("\"{inner}\""), inner)
        }
        Some(raw) => {
            let iri = bare_iri(raw);
            (iri.to_string(), local_id(iri))
        }
        None => (String::new(), String::new()),
    };
    Problem {
        message: template
            .replace("{$this}", &local_id(&focus))
            .replace("{?value}", &value)
            .replace("{?short}", &short),
        focus,
        source: "shacl",
    }
}

fn bare_iri(value: &str) -> &str {
    value
        .trim()
        .strip_prefix('<')
        .and_then(|rest| rest.strip_suffix('>'))
        .unwrap_or(value.trim())
}

/// The lexical form of an N-Triples literal (`"a \"b\""^^<dt>` → `a "b"`).
fn literal_lexical(raw: &str) -> String {
    let mut out = String::new();
    let mut chars = raw.chars().skip(1);
    while let Some(c) = chars.next() {
        match c {
            '"' => break,
            '\\' => match chars.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some('r') => out.push('\r'),
                Some(other) => out.push(other),
                None => break,
            },
            other => out.push(other),
        }
    }
    out
}

/// Shape IRI → `sh:message`, read from the shapes graph itself.
fn shape_messages() -> &'static BTreeMap<String, String> {
    static MESSAGES: OnceLock<BTreeMap<String, String>> = OnceLock::new();
    MESSAGES.get_or_init(|| {
        let mut messages = BTreeMap::new();
        let parser = oxigraph::io::RdfParser::from_format(oxigraph::io::RdfFormat::Turtle);
        for quad in parser.for_reader(SHAPES_TTL.as_bytes()).flatten() {
            if quad.predicate.as_str() != SH_MESSAGE {
                continue;
            }
            if let (NamedOrBlankNode::NamedNode(shape), Term::Literal(message)) =
                (&quad.subject, &quad.object)
            {
                messages.insert(shape.as_str().to_string(), message.value().to_string());
            }
        }
        messages
    })
}

/// What SHACL Core cannot say about a graph-plan-v1 definition, checked the
/// way `compileGraphPlan` checks it and worded as it words its problems.
fn structure_problems(triples: &[Triple], draft_iri: &str) -> Vec<Problem> {
    let mut problems = Vec::new();
    if triples.iter().any(|t| {
        !matches!(t.subject, NamedOrBlankNode::NamedNode(_))
            || matches!(t.object, Term::BlankNode(_))
    }) {
        problems.push(Problem::structure(
            draft_iri,
            "blank nodes are not allowed in a workflow definition; name every subject".into(),
        ));
    }
    let plan = Plan::new(triples);
    let p = |name: &str| format!("{WF}{name}");
    let workflows = plan.typed("Workflow");
    if workflows.len() != 1 {
        problems.push(Problem::structure(
            draft_iri,
            format!(
                "a definition holds exactly one wf:Workflow; this one holds {}",
                workflows.len()
            ),
        ));
        return problems;
    }
    let workflow = workflows[0];

    let all_phases = plan.typed("Phase");
    let phase_iris: Vec<&str> = all_phases
        .iter()
        .copied()
        .filter(|phase| plan.iris(phase, &p("partOfWorkflow")).contains(&workflow))
        .collect();
    for orphan in all_phases
        .iter()
        .filter(|phase| !phase_iris.contains(phase))
    {
        problems.push(Problem::structure(
            orphan,
            format!(
                "{} is a phase of no workflow here (wf:partOfWorkflow)",
                local_id(orphan)
            ),
        ));
    }
    if phase_iris.is_empty() {
        problems.push(Problem::structure(
            workflow,
            "the workflow has no phases".into(),
        ));
    }
    if phase_iris.len() > MAX_PHASES {
        problems.push(Problem::structure(
            workflow,
            format!("at most {MAX_PHASES} phases"),
        ));
    }
    let orders: Vec<i64> = phase_iris
        .iter()
        .filter_map(|phase| phase_order(&plan, phase))
        .collect();
    if orders.iter().collect::<BTreeSet<_>>().len() != orders.len() {
        problems.push(Problem::structure(
            workflow,
            "two phases share a wf:order".into(),
        ));
    }
    let mut ordered: Vec<(i64, &str)> = phase_iris
        .iter()
        .map(|phase| (phase_order(&plan, phase).unwrap_or(i64::MAX), *phase))
        .collect();
    ordered.sort();
    let phase_index: BTreeMap<&str, usize> = ordered
        .iter()
        .enumerate()
        .map(|(index, (_, phase))| (*phase, index))
        .collect();

    let steps = plan.typed("AgentNode");
    if steps.is_empty() {
        problems.push(Problem::structure(
            workflow,
            "the workflow has no steps (wf:AgentNode)".into(),
        ));
    }
    if steps.len() > MAX_STEPS {
        problems.push(Problem::structure(
            workflow,
            format!("at most {MAX_STEPS} steps"),
        ));
    }
    // collection → (step, phase index) of its producer.
    let mut produced_by: BTreeMap<&str, (&str, Option<usize>)> = BTreeMap::new();
    let mut reads: Vec<(&str, Option<usize>, Vec<&str>)> = Vec::new();
    for step in &steps {
        let short = local_id(step);
        let phase = plan.iri_of(step, &p("inPhase"));
        if let Some(phase) = phase {
            if !phase_index.contains_key(phase) {
                problems.push(Problem::structure(
                    step,
                    format!(
                        "{short} is in {}, which is not a phase of this workflow",
                        local_id(phase)
                    ),
                ));
            }
        }
        let at = phase.and_then(|phase| phase_index.get(phase).copied());
        let checks = plan.iri_of(step, &p("checks"));
        let mode = plan
            .literal(step, &p("mode"))
            .unwrap_or(if checks.is_some() { "perItem" } else { "once" });
        let per_item = mode == "perItem";
        let mut over = plan.iri_of(step, &p("over"));
        if let Some(checks) = checks {
            if !per_item {
                problems.push(Problem::structure(
                    step,
                    format!("{short} checks a collection, so it runs once per item"),
                ));
            }
            if let Some(over) = over.filter(|over| *over != checks) {
                problems.push(Problem::structure(
                    step,
                    format!(
                        "{short} checks {} but runs over {}",
                        local_id(checks),
                        local_id(over)
                    ),
                ));
            }
            over = Some(checks);
        }
        if per_item && over.is_none() {
            problems.push(Problem::structure(
                step,
                format!("{short} runs per item but names no collection (wf:over)"),
            ));
        }
        // "has no" only when there is no value at all; a value of the wrong
        // kind is the shapes' to report (graph-plan.ts `iriOf`/`literal`).
        let produces = plan.iri_of(step, &p("produces"));
        if plan.values(step, &p("produces")).is_empty() && checks.is_none() {
            problems.push(Problem::structure(
                step,
                format!("{short} has no wf:produces"),
            ));
        }
        let has_schema = !plan.values(step, &p("itemSchema")).is_empty();
        match plan.literal(step, &p("itemSchema")) {
            None if !has_schema && checks.is_none() => problems.push(Problem::structure(
                step,
                format!("{short} has no wf:itemSchema"),
            )),
            Some(schema)
                if !serde_json::from_str::<Value>(schema)
                    .is_ok_and(|parsed| parsed.is_object()) =>
            {
                problems.push(Problem::structure(
                    step,
                    format!("{short}'s wf:itemSchema is not a JSON Schema object"),
                ))
            }
            _ => {}
        }
        if let Some(produces) = produces {
            if let Some((prior, _)) = produced_by.get(produces) {
                problems.push(Problem::structure(
                    produces,
                    format!(
                        "{} is produced by both {} and {short}",
                        local_id(produces),
                        local_id(prior)
                    ),
                ));
            }
            produced_by.insert(produces, (*step, at));
        }
        let mut read = over.into_iter().collect::<Vec<_>>();
        read.extend(plan.iris(step, &p("consumes")));
        reads.push((*step, at, read));
    }
    for (step, at, read) in reads {
        for collection in read {
            match produced_by.get(collection) {
                None => problems.push(Problem::structure(
                    step,
                    format!(
                        "{} reads {}, which no step produces",
                        local_id(step),
                        local_id(collection)
                    ),
                )),
                // graph-plan.ts: a phase it cannot place counts as -1.
                Some((_, source))
                    if source.map_or(-1, |source| source as i64)
                        >= at.map_or(-1, |at| at as i64) =>
                {
                    problems.push(Problem::structure(
                        step,
                        format!(
                            "{} reads {} before an earlier phase has produced it",
                            local_id(step),
                            local_id(collection)
                        ),
                    ))
                }
                Some(_) => {}
            }
        }
    }
    let ids: Vec<String> = steps.iter().map(|step| local_id(step)).collect();
    if ids.iter().collect::<BTreeSet<_>>().len() != ids.len() {
        problems.push(Problem::structure(
            workflow,
            "two steps share a local name; give each step its own IRI fragment".into(),
        ));
    }
    problems
}
