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
//!     wf:produces <…#c> ; wf:itemSchema "{json schema}" ; wf:checks <…#c> ;
//!     wf:reads <…#book> .
//! <…#book> a wf:Source ; wf:label "…" ; wf:documentId "…" ; wf:fromBlock "…" ; wf:toBlock "…" .
//! <…#margin> a wf:Destination ; wf:label "…" ; wf:kind "margin" ; wf:receives <…#c> ;
//!     wf:documentId "…" .
//! ```
//!
//! A source is what a step reads (a document, or the blocks from one block
//! through another), so its instructions can say "the book" instead of a
//! document id; a destination is where the results go, and
//! `workflow_plan_publish` publishes what a margin destination receives
//! (`plans/2026-10-19-workflow-canvas.md`, "The grammar addition").
//!
//! The ideas come from the workflow book (`workflow_book_mcp.rs`: compose moves,
//! live validation, a `wf:CompositionEvent` per change), not its script
//! anatomy; the book is unchanged. A compose applies its moves in one SPARQL
//! update and records one `wf:CompositionEvent` in
//! `urn:sophia:wf:<documentId>:authoring`. Validation never blocks a draft
//! (FlagAndAccept): SHACL shapes (`workflow_plan_shapes.ttl`, run by rudof as
//! Emporium's EA-1 face does) judge each node, and the structural checks here
//! judge what SHACL Core cannot say, worded as the TS compiler words them. One
//! check is this cell's alone, because the compiler cannot see the workspace:
//! whether the documents the sources and destinations name are in this graph.
//!
//! `workflow_run_record` is the runner's side: when a run ends, Choreograph
//! records its results (the run node, its items, refusals and step tallies)
//! in `urn:sophia:wf:<documentId>:run:<runId>`, the graph the workflow pane
//! and `workflow_plan_publish` read. It needs only `documents.write.crdt`, so
//! a runner holding a member's token records its runs without `rdf.update`.
//!
//! The fence: every tool refuses a document whose kind is not `"workflow"`,
//! and writes only that document's own graphs: `:draft` and `:authoring`, and
//! (`workflow_run_record`) one `:run:<runId>` graph, only input triples whose
//! subject is in that run's namespace, plus host-derived cast registrations in
//! the document's `:agents` graph.
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
#[cfg(feature = "desktop")]
use tauri::Manager;

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
const MAX_SOURCES: usize = 6;
const MAX_DESTINATIONS: usize = 4;
/// The one kind of destination there is: comments in a document's margin.
const MARGIN: &str = "margin";
const RECENT_EVENTS: usize = 10;
const ID_MAX: usize = 64;
/// The longest note composed for a finding that has none, in characters.
const COMPOSED_NOTE_MAX: usize = 600;
/// The longest `runId` a run record takes, in characters.
const RUN_ID_MAX: usize = 128;
/// The most Turtle a run record takes: 16 MiB.
const RUN_TURTLE_MAX_BYTES: usize = 16 * 1024 * 1024;

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

    /// The graph a run's results live in: `urn:sophia:wf:<documentId>:run:<runId>`
    /// with `runId` made a segment (`run_segment`).
    fn run_graph(&self, run_id: &str) -> String {
        format!("{}:run:{}", self.workflow, run_segment(run_id))
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
    read_result(&app, &graph_id, &names).await
}

pub(super) async fn mcp_local_workflow_plan_read(
    app: AppHandle,
    arguments: &Value,
) -> AppResult<Value> {
    let (graph_id, names) = workflow_document(&app, arguments).await?;
    read_result(&app, &graph_id, &names).await
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
    let mut moves = moves_value
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
        .map(|(index, value)| parse_move(index + 1, value))
        .collect::<Result<Vec<_>, _>>()
        .map_err(AppError::validation)?;
    bind_item_classes(&app, &graph_id, &mut moves)?;
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
    read_result(&app, &graph_id, &names).await
}

/// Resolve each step move's `itemClass` against this graph's types (D4): a
/// known class becomes its IRI, its `<vocab>@<version>` pin and its derived
/// JSON Schema; an unknown one keeps the IRI it names, so the move is still
/// accepted and the report says the class is not a type of this graph.
fn bind_item_classes(app: &AppHandle, graph_id: &str, moves: &mut [Move]) -> AppResult<()> {
    if !moves
        .iter()
        .any(|m| matches!(m, Move::UpsertStep(step) if step.item_class.is_some()))
    {
        return Ok(());
    }
    let store = graph_store(app, graph_id)?;
    for (index, change) in moves.iter_mut().enumerate() {
        let Move::UpsertStep(step) = change else {
            continue;
        };
        let Some(reference) = &step.item_class else {
            continue;
        };
        let resolved =
            crate::emporium::type_catalog::resolve_class_reference(&store, graph_id, reference)
                .map_err(|error| {
                    AppError::validation(format!("move {} (upsert_step): {error}", index + 1))
                })?;
        step.item_binding = Some(match resolved {
            crate::emporium::type_catalog::ClassReference::Known(class) => ItemClassBinding {
                iri: class.iri.clone(),
                pin: Some(class.pin()),
                schema: Some(class.json_schema().to_string()),
            },
            crate::emporium::type_catalog::ClassReference::Unknown { iri } => ItemClassBinding {
                iri,
                pin: None,
                schema: None,
            },
        });
    }
    Ok(())
}

fn graph_store(
    app: &AppHandle,
    graph_id: &str,
) -> AppResult<std::sync::Arc<oxigraph::store::Store>> {
    let graph_dir =
        crate::graph_paths::existing_graph_dir(app, graph_id).map_err(AppError::storage)?;
    crate::rdf_service::open_graph_store(&graph_dir).map_err(AppError::rdf)
}

/// One finding a person may publish (Shrubbery's `publishableFindings`): an
/// item of the collection being published that names the `documentId` it is
/// about (or leaves it to the destination's), the exact `quote` it rests on,
/// the `note` to write beside it (or, when it has none, its other fields)
/// and, optionally, the `blockId` that holds the quote.
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

/// `^[A-Za-z0-9_-]{1,128}$`: a document id (graph-plan.ts `wf:documentId`).
fn document_id_shaped(id: &str) -> bool {
    (1..=128).contains(&id.len())
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// `^[A-Za-z0-9_.:-]{1,128}$`: a block id (`wf:fromBlock`, `wf:toBlock`).
fn block_id_shaped(id: &str) -> bool {
    (1..=128).contains(&id.len())
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | ':' | '-'))
}

/// Which keys of an item Publish reads (D2 `presentation.roles`): by default
/// the convention `quote`, `note`, `documentId`, `blockId`; a class that
/// declares roles names its own keys for each.
#[derive(Debug, Clone)]
struct FindingKeys {
    quote: String,
    note: String,
    document: String,
    block: String,
}

impl Default for FindingKeys {
    fn default() -> Self {
        Self {
            quote: "quote".into(),
            note: "note".into(),
            document: "documentId".into(),
            block: "blockId".into(),
        }
    }
}

impl FindingKeys {
    /// The class's roles over the convention, role by role.
    fn of(roles: &crate::emporium::contract::PresentationRoles) -> Self {
        let default = Self::default();
        Self {
            quote: roles.quote.clone().unwrap_or(default.quote),
            note: roles.note.clone().unwrap_or(default.note),
            document: roles.document.clone().unwrap_or(default.document),
            block: roles.block.clone().unwrap_or(default.block),
        }
    }

    fn is_role(&self, key: &str) -> bool {
        key == self.quote || key == self.note || key == self.document || key == self.block
    }
}

/// An item's finding. One whose JSON names no document (none, null or
/// empty) is about `fallback`, the destination's document, when there is one;
/// one with no note says its other fields instead (`composed_note`).
fn finding_of(
    index: i64,
    json: &str,
    fallback: Option<&str>,
    keys: &FindingKeys,
) -> Option<Finding> {
    let value: Value = serde_json::from_str(json).ok()?;
    let text = |key: &str| {
        value
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(str::to_string)
    };
    let document_id = match value.get(keys.document.as_str()) {
        None | Some(Value::Null) => fallback?,
        Some(Value::String(id)) if id.trim().is_empty() => fallback?,
        Some(Value::String(id)) => id.as_str(),
        Some(_) => return None,
    };
    if !document_id_shaped(document_id) {
        return None;
    }
    Some(Finding {
        index,
        document_id: document_id.to_string(),
        quote: text(&keys.quote)?,
        note: text(&keys.note).or_else(|| composed_note(&value, keys))?,
        block_id: value
            .get(keys.block.as_str())
            .and_then(Value::as_str)
            .filter(|block| !block.is_empty())
            .map(str::to_string),
    })
}

/// The note of an item that has none (Sophia's clerks answer `{who, whatDone,
/// dream, quote}`): its other non-empty text fields in the item's own key
/// order, each `<key as words>: <value>`, joined with `; ` and cut at 600
/// characters. `quote`, `documentId`, `blockId` and `note` are not said. An
/// item whose only text is its quote has none. Shrubbery's `composedNote`
/// (`cell/graph-plan.ts`) is the same rule, so what the workflow pane offers
/// to publish is what this writes.
fn composed_note(item: &Value, keys: &FindingKeys) -> Option<String> {
    let parts: Vec<String> = item
        .as_object()?
        .iter()
        .filter(|(key, _)| !keys.is_role(key))
        .filter_map(|(key, value)| {
            let value = value.as_str()?.trim();
            (!value.is_empty()).then(|| format!("{}: {value}", key_as_words(key)))
        })
        .collect();
    let note: String = parts.join("; ").chars().take(COMPOSED_NOTE_MAX).collect();
    (!note.is_empty()).then_some(note)
}

/// `whatDone` → `what done`, `seen_by` → `seen by`: a space where a lowercase
/// letter or digit meets a capital, one space for each run of `_` and `-`,
/// then lowercased and trimmed (`/([a-z0-9])([A-Z])/g`, then `/[_-]+/g`).
fn key_as_words(key: &str) -> String {
    let mut words = String::new();
    let mut previous: Option<char> = None;
    for c in key.chars() {
        if c == '_' || c == '-' {
            if !previous.is_some_and(|p| p == '_' || p == '-') {
                words.push(' ');
            }
        } else {
            if c.is_ascii_uppercase()
                && previous.is_some_and(|p| p.is_ascii_lowercase() || p.is_ascii_digit())
            {
                words.push(' ');
            }
            words.push(c);
        }
        previous = Some(c);
    }
    words.to_lowercase().trim().to_string()
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

/// A margin destination of the plan (`wf:Destination`, `wf:kind "margin"`):
/// the collection whose items go into a margin, and the document they go to
/// when an item names none.
#[derive(Debug, Clone)]
struct MarginDestination {
    iri: String,
    id: String,
    receives: Option<String>,
    document_id: Option<String>,
}

/// The plan's margin destinations, by id.
fn margin_destinations(draft: &[Triple]) -> Vec<MarginDestination> {
    let plan = Plan::new(draft);
    let p = |name: &str| format!("{WF}{name}");
    let mut destinations: Vec<MarginDestination> = plan
        .typed("Destination")
        .into_iter()
        .filter(|destination| plan.literal(destination, &p("kind")) == Some(MARGIN))
        .map(|destination| MarginDestination {
            iri: destination.to_string(),
            id: local_id(destination),
            receives: plan.iri_of(destination, &p("receives")).map(str::to_string),
            document_id: plan
                .literal(destination, &p("documentId"))
                .map(str::to_string),
        })
        .collect();
    destinations.sort_by(|a, b| (&a.id, &a.iri).cmp(&(&b.id, &b.iri)));
    destinations
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
    let run_graph = names.run_graph(&run_id);
    let draft = load_graph(&app, &graph_id, &names.draft)?;
    // What a margin destination receives: the one named, or else the first by
    // id. A plan with no destination publishes its last collection.
    let destinations = margin_destinations(&draft);
    let destination = match mcp_arg_string(arguments, &["destination"]) {
        Some(wanted) => Some(
            destinations
                .iter()
                .find(|destination| destination.id == wanted)
                .ok_or_else(|| {
                    let has = if destinations.is_empty() {
                        "none".to_string()
                    } else {
                        destinations
                            .iter()
                            .map(|destination| destination.id.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    };
                    AppError::validation(format!(
                        "the plan has no margin destination {wanted} (it has {has})"
                    ))
                })?,
        ),
        None => destinations.first(),
    };
    let collection = match destination {
        Some(destination) => destination.receives.clone().ok_or_else(|| {
            AppError::validation(format!("{} has no wf:receives", destination.id))
        })?,
        None => last_collection(&draft, &names).ok_or_else(|| {
            AppError::validation(
                "the plan has no last collection: its last phase has no step that produces one",
            )
        })?,
    };
    let fallback = destination.and_then(|destination| destination.document_id.as_deref());
    // A class-bound collection publishes by its class's roles (D2), falling
    // back to the key convention role by role.
    let keys = match collection_classes(&draft).get(&collection) {
        Some(binding) => {
            let store = graph_store(&app, &graph_id)?;
            crate::emporium::type_catalog::resolve_pinned_class(
                &store,
                &graph_id,
                &binding.class,
                binding.pin.as_deref(),
            )
            .map_err(AppError::internal)?
            .and_then(|class| class.class().presentation.clone())
            .map(|presentation| FindingKeys::of(&presentation.roles))
            .unwrap_or_default()
        }
        None => FindingKeys::default(),
    };
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
            .and_then(|json| finding_of(position + 1, json, fallback, &keys));
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
    if let Some(destination) = destination {
        record.push(Triple::new(
            publication.clone(),
            p("destination"),
            NamedNode::new_unchecked(destination.iri.clone()),
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

    let mut result = json!({
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
    });
    if let Some(destination) = destination {
        result["destination"] = json!(destination.id);
    }
    Ok(result)
}

/// `workflow_run_record`: a runner records a finished run's results in the
/// workflow document's own run graph, replacing what was there (clear, then
/// insert, in one SPARQL update and so one store transaction), so recording
/// the same run again is idempotent. Every input triple must be about that
/// run; one that is not, or Turtle that does not parse, refuses the whole
/// call before anything is written. Cast beings are registered in the
/// document's `:agents` graph in the same transaction, using the draft's
/// post labels.
pub(super) async fn mcp_local_workflow_run_record(
    app: AppHandle,
    arguments: &Value,
) -> AppResult<Value> {
    let (graph_id, names) = workflow_document(&app, arguments).await?;
    let run_id = mcp_arg_string(arguments, &["runId", "run_id"])
        .ok_or_else(|| AppError::validation("runId is required"))?;
    if run_id.chars().count() > RUN_ID_MAX {
        return Err(AppError::validation(format!(
            "runId is longer than {RUN_ID_MAX} characters"
        )));
    }
    let turtle = arguments
        .get("turtle")
        .and_then(Value::as_str)
        .ok_or_else(|| AppError::validation("turtle is required: the run's triples as Turtle"))?;
    if turtle.len() > RUN_TURTLE_MAX_BYTES {
        return Err(AppError::validation(format!(
            "turtle is {} bytes, more than the {RUN_TURTLE_MAX_BYTES} (16 MiB) a run record takes",
            turtle.len()
        )));
    }
    let run_graph = names.run_graph(&run_id);
    let mut triples = run_record_triples(&run_graph, turtle).map_err(AppError::validation)?;
    // Typed items (D5): the items of a class-bound collection gain their
    // class's triples, or are marked unfit, in this same transaction.
    let draft = load_graph(&app, &graph_id, &names.draft)?;
    let classes = collection_classes(&draft);
    let mut typing = None;
    if !classes.is_empty() {
        let store = graph_store(&app, &graph_id)?;
        let typed = typed_item_triples(&store, &graph_id, &run_graph, &triples, &classes)?;
        let mut seen: HashSet<Triple> = triples.iter().cloned().collect();
        for triple in typed.triples {
            if seen.insert(triple.clone()) {
                triples.push(triple);
            }
        }
        typing = Some((typed.typed, typed.unfit));
    }
    let mut update = format!("CLEAR SILENT GRAPH <{run_graph}>");
    if !triples.is_empty() {
        update.push_str(" ;\n");
        update.push_str(&insert_data(&run_graph, &triples));
    }
    let registrations = cast_registration_triples(&names, &draft, &triples);
    if !registrations.is_empty() {
        let agents_graph = format!("{}:agents", names.workflow);
        let beings: BTreeSet<_> = registrations.iter().filter_map(subject_iri).collect();
        for being in beings {
            update.push_str(&format!(
                " ;\nDELETE WHERE {{ GRAPH <{agents_graph}> {{ <{being}> ?p ?o }} }}"
            ));
        }
        update.push_str(" ;\n");
        update.push_str(&insert_data(&agents_graph, &registrations));
    }
    run_update(&app, &graph_id, update).await?;
    let mut result = json!({ "graph": run_graph, "triples": triples.len() });
    if let Some((typed, unfit)) = typing {
        result["typed"] = json!(typed);
        result["unfit"] = json!(unfit);
    }
    Ok(result)
}

/// Registration is a host-derived fact: only a cast for a post of this
/// document is registered. Existing workflow predicates name its workflow
/// and post; rdfs:label carries the post's role in the directory. The terms
/// already name workflow membership and the step of a sitting; no agent-
/// specific copies of those predicates are needed.
fn cast_registration_triples(names: &PlanNames, draft: &[Triple], run: &[Triple]) -> Vec<Triple> {
    const AGT: &str = "http://mnemosyne.dev/agent#";
    let plan = Plan::new(draft);
    let run = Plan::new(run);
    let posts: BTreeSet<_> = plan.typed("AgentNode").into_iter().collect();
    let mut triples = Vec::new();
    for sitting in run.typed("StepRun") {
        let (Some(step), Some(being)) = (
            run.iri_of(sitting, &format!("{WF}step")),
            run.iri_of(sitting, &format!("{WF}castAgent")),
        ) else {
            continue;
        };
        let Some(agent_id) = being.strip_prefix("urn:sophia:agent:agent-") else {
            continue;
        };
        if !posts.contains(step)
            || !step.starts_with(&format!("{}#", names.workflow))
            || agent_id.len() != 16
            || !agent_id
                .bytes()
                .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
        {
            continue;
        }
        let subject = NamedNode::new_unchecked(being);
        triples.extend([
            Triple::new(subject.clone(), iri(RDF_TYPE), iri(&format!("{AGT}Agent"))),
            Triple::new(
                subject.clone(),
                iri(&format!("{AGT}agentId")),
                text(&format!("agent-{agent_id}")),
            ),
            Triple::new(
                subject.clone(),
                iri(&format!("{WF}partOfWorkflow")),
                iri(&names.workflow),
            ),
            Triple::new(subject.clone(), iri(&format!("{WF}step")), iri(step)),
        ]);
        if let Some(label) = plan.literal(step, &format!("{WF}label")) {
            triples.push(Triple::new(
                subject,
                iri("http://www.w3.org/2000/01/rdf-schema#label"),
                text(label),
            ));
        }
    }
    triples
}

/// What typing a run's items adds (D5).
struct TypedItems {
    triples: Vec<Triple>,
    typed: usize,
    unfit: usize,
}

/// For each `wf:Item` of a class-bound collection: its `wf:json` mapped to
/// the class's triples through the class contract (strict coercion, nested
/// values as child nodes under the item, the derived shapes as the gate),
/// `wf:itemVocab` (the pin it was typed at), and testimony: `wf:producedBy`
/// (the being the run cast for the step whose class it is, from `<run#step-n>
/// wf:castAgent`: for a check's items the maker, never the checker), for a
/// checked collection `wf:checkedBy` (the checker's being), and
/// `prov:generatedAtTime` (the item's own, else the run's
/// `prov:endedAtTime`, else now). An item that does not fit is `a wf:Unfit`
/// with `wf:itemClass` (the class it was meant to be) and one `wf:violation`
/// per problem, never dropped. Every subject is the item or a node under it,
/// so all of it stays in the run's namespace.
fn typed_item_triples(
    store: &oxigraph::store::Store,
    graph_id: &str,
    run_graph: &str,
    run_triples: &[Triple],
    classes: &BTreeMap<String, CollectionClass>,
) -> AppResult<TypedItems> {
    use crate::emporium::type_catalog::{project_item, resolve_pinned_class, term_to_oxigraph};
    let run = Plan::new(run_triples);
    let p = |name: &str| format!("{WF}{name}");
    let generated = format!("{PROV}generatedAtTime");
    // step IRI → the being cast for it.
    let cast: BTreeMap<&str, &str> = run
        .typed("StepRun")
        .into_iter()
        .filter_map(|step_run| {
            Some((
                run.iri_of(step_run, &p("step"))?,
                run.iri_of(step_run, &p("castAgent"))?,
            ))
        })
        .collect();
    let ended = run
        .typed("GraphPlanRun")
        .first()
        .and_then(|node| {
            run.values(node, &format!("{PROV}endedAtTime"))
                .first()
                .copied()
        })
        .and_then(|term| match term {
            Term::Literal(literal) => Some(literal.clone()),
            _ => None,
        });
    let now = Literal::new_typed_literal(
        chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        NamedNode::new_unchecked(XSD_DATE_TIME),
    );
    let mut contracts: BTreeMap<&str, Option<crate::emporium::type_catalog::ResolvedClass>> =
        BTreeMap::new();
    for (collection, binding) in classes {
        let resolved =
            resolve_pinned_class(store, graph_id, &binding.class, binding.pin.as_deref())
                .map_err(AppError::internal)?;
        contracts.insert(collection.as_str(), resolved);
    }
    let mut out = TypedItems {
        triples: Vec::new(),
        typed: 0,
        unfit: 0,
    };
    let in_run = |iri: &str| {
        iri.strip_prefix(run_graph)
            .is_some_and(|rest| rest.starts_with('#'))
    };
    for item in run.typed("Item") {
        let Some(collection) = run.iri_of(item, &p("inCollection")) else {
            continue;
        };
        let Some(binding) = classes.get(collection) else {
            continue;
        };
        let subject = NamedNode::new_unchecked(item.to_string());
        let resolved = contracts.get(collection).cloned().flatten();
        let projected: Result<Vec<Triple>, Vec<String>> =
            match (&resolved, run.literal(item, &p("json"))) {
                (None, _) => Err(vec![format!(
                    "the class {} ({}) is not a type of this graph",
                    binding.class,
                    binding.pin.as_deref().unwrap_or("no version pinned")
                )]),
                (Some(_), None) => Err(vec!["the item has no wf:json".to_string()]),
                (Some(class), Some(json)) => match serde_json::from_str::<Value>(json) {
                    Err(error) => Err(vec![format!("wf:json is not JSON: {error}")]),
                    Ok(value) => {
                        project_item(&class.contract, &class.class_name, item, &value, run_graph)
                            .map(|projection| {
                                projection
                                    .into_iter()
                                    .map(|(s, p, o)| {
                                        Triple::new(
                                            NamedNode::new_unchecked(s),
                                            NamedNode::new_unchecked(p),
                                            term_to_oxigraph(&o),
                                        )
                                    })
                                    .collect()
                            })
                    }
                },
            };
        match projected {
            Ok(triples) => {
                if let Some(stray) = triples.iter().find_map(|t| {
                    subject_iri(t).filter(|s| *s != item && !s.starts_with(&format!("{item}/")))
                }) {
                    return Err(AppError::internal(format!(
                        "typing {item} produced a triple about {stray}, outside the item"
                    )));
                }
                out.triples.extend(triples);
                out.typed += 1;
            }
            Err(violations) => {
                out.triples.push(Triple::new(
                    subject.clone(),
                    iri(RDF_TYPE),
                    iri(&p("Unfit")),
                ));
                out.triples.push(Triple::new(
                    subject.clone(),
                    iri(&p("itemClass")),
                    NamedNode::new_unchecked(binding.class.clone()),
                ));
                for violation in violations {
                    out.triples.push(Triple::new(
                        subject.clone(),
                        iri(&p("violation")),
                        text(&violation),
                    ));
                }
                out.unfit += 1;
            }
        }
        if let Some(pin) = resolved
            .as_ref()
            .map(|class| class.pin())
            .or(binding.pin.clone())
        {
            out.triples.push(Triple::new(
                subject.clone(),
                iri(&p("itemVocab")),
                text(&pin),
            ));
        }
        if run.values(item, &p("producedBy")).is_empty() {
            if let Some(agent) = cast.get(binding.origin.as_str()) {
                out.triples.push(Triple::new(
                    subject.clone(),
                    iri(&p("producedBy")),
                    NamedNode::new_unchecked(agent.to_string()),
                ));
            }
        }
        // A check passed it on: the maker made it, the checker checked it.
        if binding.producer != binding.origin && run.values(item, &p("checkedBy")).is_empty() {
            if let Some(checker) = cast.get(binding.producer.as_str()) {
                out.triples.push(Triple::new(
                    subject.clone(),
                    iri(&p("checkedBy")),
                    NamedNode::new_unchecked(checker.to_string()),
                ));
            }
        }
        if run.values(item, &generated).is_empty() {
            out.triples.push(Triple::new(
                subject.clone(),
                iri(&generated),
                ended.clone().unwrap_or_else(|| now.clone()),
            ));
        }
        debug_assert!(in_run(item));
    }
    Ok(out)
}

/// The distinct triples of `turtle`, each about the run whose graph is
/// `run_graph`: its subject is the IRI `run_graph` itself or `run_graph#…`.
/// The first triple that does not parse or is about anything else (another
/// IRI, a blank node) is the error.
fn run_record_triples(run_graph: &str, turtle: &str) -> Result<Vec<Triple>, String> {
    let in_run = |subject: &NamedOrBlankNode| match subject {
        NamedOrBlankNode::NamedNode(node) => node
            .as_str()
            .strip_prefix(run_graph)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with('#')),
        _ => false,
    };
    let parser = oxigraph::io::RdfParser::from_format(oxigraph::io::RdfFormat::Turtle);
    let mut seen = HashSet::new();
    let mut triples = Vec::new();
    for quad in parser.for_slice(turtle.as_bytes()) {
        let quad = quad.map_err(|error| format!("turtle does not parse: {error}"))?;
        if !in_run(&quad.subject) {
            return Err(format!(
                "{} is not about this run: every subject must be <{run_graph}> or \
                 <{run_graph}#…>; nothing was written",
                quad.subject
            ));
        }
        let triple = Triple::new(quad.subject, quad.predicate, quad.object);
        if seen.insert(triple.clone()) {
            triples.push(triple);
        }
    }
    Ok(triples)
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
    let store = graph_store(app, graph_id)?;
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

async fn read_result(app: &AppHandle, graph_id: &str, names: &PlanNames) -> AppResult<Value> {
    let draft = load_graph(app, graph_id, &names.draft)?;
    let authoring = load_graph(app, graph_id, &names.authoring)?;
    let mut problems = report(&draft, &names.draft);
    let workspace = workspace_documents(app, graph_id, &draft).await?;
    for problem in workspace.problems(&draft) {
        if !problems
            .iter()
            .any(|known| known.message == problem.message)
        {
            problems.push(problem);
        }
    }
    let mut result = plan_view(&draft, names);
    for problem in item_class_views(app, graph_id, &draft, &mut result)? {
        if !problems
            .iter()
            .any(|known| known.message == problem.message)
        {
            problems.push(problem);
        }
    }
    for source in result["sources"].as_array_mut().into_iter().flatten() {
        let title = source["documentId"]
            .as_str()
            .and_then(|document_id| workspace.titles.get(document_id));
        if let Some(title) = title {
            source["documentTitle"] = json!(title);
        }
    }
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

/// What only this cell can see about the steps' classes (D4): each
/// class-bound step's `itemClass` becomes the class's catalogue view (label,
/// plural, fields) or `{iri, known: false}`; `collections` lists every
/// collection a step produces, with the class of its items (a check's items
/// are the class of what it checks); and a class this graph does not have, or
/// a pin it does not have, is a problem (source "types").
fn item_class_views(
    app: &AppHandle,
    graph_id: &str,
    draft: &[Triple],
    result: &mut Value,
) -> AppResult<Vec<Problem>> {
    let plan = Plan::new(draft);
    let p = |name: &str| format!("{WF}{name}");
    let steps = plan.typed("AgentNode");
    let classes = collection_classes(draft);
    let bound: Vec<(&str, &str, Option<&str>)> = steps
        .iter()
        .filter_map(|step| {
            let class = plan.iri_of(step, &p("itemClass"))?;
            Some((*step, class, plan.literal(step, &p("itemVocab"))))
        })
        .collect();
    let mut problems = Vec::new();
    let mut views: BTreeMap<String, Value> = BTreeMap::new();
    if !bound.is_empty() {
        let store = graph_store(app, graph_id)?;
        for (step, class, pin) in &bound {
            let found =
                crate::emporium::type_catalog::resolve_pinned_class(&store, graph_id, class, *pin)
                    .map_err(AppError::internal)?;
            let view = match found {
                Some(resolved) => resolved.summary(),
                None => {
                    let now = crate::emporium::type_catalog::resolve_class_iri(
                        &store, graph_id, class, false,
                    )
                    .map_err(AppError::internal)?;
                    let message = match (now, pin) {
                        (Some(_), Some(pin)) => format!(
                            "{}'s wf:itemVocab names {pin}, which this graph does not define",
                            local_id(step)
                        ),
                        _ => format!(
                            "{}'s wf:itemClass names {}, which is not a type of this graph",
                            local_id(step),
                            local_id(class)
                        ),
                    };
                    problems.push(Problem {
                        focus: step.to_string(),
                        message,
                        source: "types",
                    });
                    json!({ "iri": class, "known": false })
                }
            };
            views.insert(step.to_string(), view);
        }
        let fill = |list: &mut Value| {
            for step in list.as_array_mut().into_iter().flatten() {
                let Some(id) = step["id"].as_str().map(str::to_string) else {
                    continue;
                };
                if let Some((_, view)) = views.iter().find(|(iri, _)| local_id(iri) == id) {
                    step["itemClass"] = view.clone();
                }
            }
        };
        for phase in result["phases"].as_array_mut().into_iter().flatten() {
            fill(&mut phase["steps"]);
        }
        if let Some(unplaced) = result.get_mut("unplacedSteps") {
            fill(unplaced);
        }
    }
    let collections: Vec<Value> = steps
        .iter()
        .filter_map(|step| {
            let produces = plan.iri_of(step, &p("produces"))?;
            let mut entry = json!({ "id": local_id(produces), "producedBy": local_id(step) });
            if let Some(class) = classes.get(produces) {
                let view = views
                    .get(&class.origin)
                    .cloned()
                    .unwrap_or_else(|| json!({ "iri": class.class }));
                entry["itemClass"] = json!({
                    "iri": class.class,
                    "label": view.get("label").cloned().unwrap_or(Value::Null),
                    "plural": view.get("plural").cloned().unwrap_or(Value::Null),
                    "known": view.get("known").cloned().unwrap_or(json!(true)),
                });
            }
            Some(entry)
        })
        .collect();
    result["collections"] = json!(collections);
    Ok(problems)
}

/// What only this cell can see about the documents the plan's sources and
/// destinations name (the compiler cannot see the workspace): which are not
/// in this graph, and the title of each that is.
#[derive(Debug, Default)]
struct Workspace {
    missing: BTreeSet<String>,
    titles: BTreeMap<String, String>,
}

impl Workspace {
    /// `<id> names document <documentId>, which is not in this graph`, for
    /// each source, then each destination, whose document is missing.
    fn problems(&self, draft: &[Triple]) -> Vec<Problem> {
        let plan = Plan::new(draft);
        plan.typed("Source")
            .into_iter()
            .chain(plan.typed("Destination"))
            .filter_map(|subject| {
                let document_id = plan.literal(subject, &format!("{WF}documentId"))?;
                self.missing.contains(document_id).then(|| Problem {
                    focus: subject.to_string(),
                    message: format!(
                        "{} names document {document_id}, which is not in this graph",
                        local_id(subject)
                    ),
                    source: "workspace",
                })
            })
            .collect()
    }
}

async fn workspace_documents(
    app: &AppHandle,
    graph_id: &str,
    draft: &[Triple],
) -> AppResult<Workspace> {
    let plan = Plan::new(draft);
    // An id of the wrong shape is the structural checks' to report.
    let document_ids: BTreeSet<&str> = plan
        .typed("Source")
        .into_iter()
        .chain(plan.typed("Destination"))
        .filter_map(|subject| plan.literal(subject, &format!("{WF}documentId")))
        .filter(|document_id| document_id_shaped(document_id))
        .collect();
    let mut workspace = Workspace::default();
    if document_ids.is_empty() {
        return Ok(workspace);
    }
    let graph_dir =
        crate::graph_paths::existing_graph_dir(app, graph_id).map_err(AppError::storage)?;
    let registry = app.state::<crate::crdt_engine::rooms::RoomRegistry>();
    for document_id in document_ids {
        // The test document.editComments makes before it writes in a margin,
        // so what the report calls missing is what publish would not find.
        match crate::loopback_hocuspocus_routes::document_exists_for_room_connection(
            &registry,
            graph_id,
            &graph_dir,
            document_id,
        )
        .await
        {
            Ok(true) => {
                if let Some(title) =
                    document_title(&registry, graph_id, &graph_dir, document_id).await
                {
                    workspace.titles.insert(document_id.to_string(), title);
                }
            }
            Ok(false) => {
                workspace.missing.insert(document_id.to_string());
            }
            // A document the graph holds but cannot serve yet is not missing.
            Err(_) => {}
        }
    }
    Ok(workspace)
}

/// The title the workspace gives a document, else its stored record's.
async fn document_title(
    registry: &crate::crdt_engine::rooms::RoomRegistry,
    graph_id: &str,
    graph_dir: &std::path::Path,
    document_id: &str,
) -> Option<String> {
    let live = match registry.peek(&format!("workspace:{graph_id}")).await {
        Some(room) => {
            let document_id = document_id.to_string();
            room.with_doc(move |doc| {
                crate::crdt_engine::workspace_ops::document_title_in_workspace(doc, &document_id)
            })
            .await
        }
        None => None,
    };
    live.or_else(|| {
        crate::crdt_engine::workspace_ops::document_record_json(graph_dir, document_id)?
            .get("title")?
            .as_str()
            .filter(|title| !title.is_empty())
            .map(str::to_string)
    })
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
    UpsertSource(SourceMove),
    UpsertDestination(DestinationMove),
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
    /// `itemClass` as given: a class IRI or `<vocab>:<Class>` (D4).
    item_class: Option<String>,
    /// What `item_class` resolved to in this graph's types, filled in before
    /// the moves apply (compose has the store; `apply_moves` does not).
    item_binding: Option<ItemClassBinding>,
    /// The sources it reads (`wf:reads`), in the order given, each once.
    reads: Vec<String>,
}

/// A step's class, as compose writes it (D4): `wf:itemClass <iri>`, and for a
/// class this graph has, `wf:itemVocab "<vocab>@<version>"` (the pin) and
/// `wf:itemSchema` (its derived JSON Schema, replacing any hand schema). An
/// unknown class keeps only its IRI, which the report then names.
#[derive(Debug, Clone)]
struct ItemClassBinding {
    iri: String,
    pin: Option<String>,
    schema: Option<String>,
}

/// A `wf:Source`: a document, or the blocks of one from `from_block`
/// through `to_block`.
#[derive(Debug, Clone)]
struct SourceMove {
    id: String,
    label: String,
    document_id: String,
    from_block: Option<String>,
    to_block: Option<String>,
}

/// A `wf:Destination`: where the items of `receives` go.
#[derive(Debug, Clone)]
struct DestinationMove {
    id: String,
    label: String,
    kind: String,
    receives: String,
    document_id: Option<String>,
}

const MOVE_OPS: &str =
    "set_title, upsert_phase, upsert_step, upsert_source, upsert_destination or remove";

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
            "itemClass",
            "reads",
        ],
        "upsert_source" => &["op", "id", "label", "documentId", "fromBlock", "toBlock"],
        "upsert_destination" => &["op", "id", "label", "kind", "receives", "documentId"],
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
    // An id or a list of ids.
    let id_list = |field: &str| -> Result<Vec<String>, String> {
        match object.get(field) {
            None | Some(Value::Null) => Ok(Vec::new()),
            Some(Value::String(id)) => Ok(vec![valid_id(&at, field, id)?]),
            Some(Value::Array(ids)) => ids
                .iter()
                .map(|id| {
                    id.as_str()
                        .ok_or_else(|| format!("{at}: {field} must be a list of ids"))
                        .and_then(|id| valid_id(&at, field, id))
                })
                .collect(),
            Some(_) => Err(format!("{at}: {field} must be a list of ids")),
        }
    };
    // `^[A-Za-z0-9_-]{1,128}$`, refused here so a bad one never reaches the draft.
    let document_field = |field: &str| -> Result<Option<String>, String> {
        match object.get(field) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(id)) if document_id_shaped(id) => Ok(Some(id.clone())),
            Some(Value::String(id)) => Err(format!(
                "{at}: {field} {id:?} is not a document id (1 to 128 letters, digits, _ or -)"
            )),
            Some(_) => Err(format!("{at}: {field} must be a document id")),
        }
    };
    // `^[A-Za-z0-9_.:-]{1,128}$`.
    let block_field = |field: &str| -> Result<Option<String>, String> {
        match object.get(field) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(id)) if block_id_shaped(id) => Ok(Some(id.clone())),
            Some(Value::String(id)) => Err(format!(
                "{at}: {field} {id:?} is not a block id (1 to 128 letters, digits, _, ., : or -)"
            )),
            Some(_) => Err(format!("{at}: {field} must be a block id")),
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
            let consumes = id_list("consumes")?;
            let mut seen = HashSet::new();
            let reads: Vec<String> = id_list("reads")?
                .into_iter()
                .filter(|id| seen.insert(id.clone()))
                .collect();
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
            let item_class = match object.get("itemClass") {
                None | Some(Value::Null) => None,
                Some(Value::String(class)) if !class.trim().is_empty() => {
                    Some(class.trim().to_string())
                }
                Some(_) => {
                    return Err(format!(
                        "{at}: itemClass must be a class: its IRI or <vocab>:<Class> \
                         (e.g. \"workspace:DreamReport\")"
                    ))
                }
            };
            if item_class.is_some() && item_schema.is_some() {
                return Err(format!(
                    "{at}: give itemClass or itemSchema, not both; a class derives its item schema"
                ));
            }
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
                item_class,
                item_binding: None,
                reads,
            })
        }
        "upsert_source" => Move::UpsertSource(SourceMove {
            id: id_field("id")?,
            label: text("label")?,
            document_id: document_field("documentId")?
                .ok_or_else(|| format!("{at} needs documentId (a document id)"))?,
            from_block: block_field("fromBlock")?,
            to_block: block_field("toBlock")?,
        }),
        "upsert_destination" => {
            let kind = match object.get("kind") {
                None | Some(Value::Null) => MARGIN.to_string(),
                Some(Value::String(kind)) if kind == MARGIN => kind.clone(),
                Some(other) => return Err(format!("{at}: kind is {other}; use \"margin\"")),
            };
            Move::UpsertDestination(DestinationMove {
                id: id_field("id")?,
                label: text("label")?,
                kind,
                receives: id_field("receives")?,
                document_id: document_field("documentId")?,
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
    // What an id already names, when it names a node: a phase, a step, a
    // source or a destination.
    let named = |state: &[Triple], item: &str| -> Option<&'static str> {
        [
            ("Phase", "phase"),
            ("AgentNode", "step"),
            ("Source", "source"),
            ("Destination", "destination"),
        ]
        .into_iter()
        .find(|(class, _)| typed(state, item, class))
        .map(|(_, what)| what)
    };
    let collection = |state: &[Triple], item: &str| {
        state.iter().any(|t| {
            ["over", "consumes", "produces", "checks", "receives"]
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
                if let Some(what) = named(&state, &item).filter(|what| *what != "phase") {
                    return Err(format!(
                        "{}: {id} is a {what}; give the phase its own id",
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
                if let Some(what) = named(&state, &item).filter(|what| *what != "step") {
                    return Err(format!(
                        "{here}: {} is a {what}; give the step its own id",
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
                let phase_is = if phase_item == item {
                    Some("step")
                } else {
                    named(&state, &phase_item).filter(|what| *what != "phase")
                };
                if let Some(what) = phase_is {
                    return Err(format!(
                        "{here}: phase {} is a {what}, not a phase",
                        step.phase
                    ));
                }
                let collections: Vec<&String> = step
                    .over
                    .iter()
                    .chain(step.consumes.iter())
                    .chain(step.checks.iter())
                    .chain(step.produces.iter())
                    .collect();
                for used in &collections {
                    let used_item = names.item(used);
                    if used_item == item
                        || typed(&state, &used_item, "AgentNode")
                        || typed(&state, &used_item, "Phase")
                    {
                        return Err(format!(
                            "{here}: {used} is a phase or a step, not a collection"
                        ));
                    }
                    if let Some(what) = named(&state, &used_item) {
                        return Err(format!("{here}: {used} is a {what}, not a collection"));
                    }
                }
                // A source it reads may come in a later move (the report says
                // so until it does); anything else an id names is refused.
                for read in &step.reads {
                    let read_item = names.item(read);
                    let read_is = if read_item == item {
                        Some("step")
                    } else {
                        named(&state, &read_item).filter(|what| *what != "source")
                    };
                    if let Some(what) = read_is {
                        return Err(format!("{here}: {read} is a {what}, not a source"));
                    }
                    if collections.contains(&read) || collection(&state, &read_item) {
                        return Err(format!("{here}: {read} names a collection, not a source"));
                    }
                }
                let subject = NamedNode::new_unchecked(item.clone());
                let p = |name: &str| iri(&format!("{WF}{name}"));
                let node_term =
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
                    added.push(Triple::new(subject.clone(), p("over"), node_term(over)));
                }
                for consumed in &step.consumes {
                    added.push(Triple::new(
                        subject.clone(),
                        p("consumes"),
                        node_term(consumed),
                    ));
                }
                if let Some(checks) = &step.checks {
                    added.push(Triple::new(subject.clone(), p("checks"), node_term(checks)));
                }
                if let Some(produces) = &step.produces {
                    added.push(Triple::new(
                        subject.clone(),
                        p("produces"),
                        node_term(produces),
                    ));
                }
                match &step.item_binding {
                    Some(binding) => {
                        added.push(Triple::new(
                            subject.clone(),
                            p("itemClass"),
                            NamedNode::new_unchecked(binding.iri.clone()),
                        ));
                        if let Some(pin) = &binding.pin {
                            added.push(Triple::new(subject.clone(), p("itemVocab"), text(pin)));
                        }
                        if let Some(schema) = &binding.schema {
                            added.push(Triple::new(subject.clone(), p("itemSchema"), text(schema)));
                        }
                    }
                    None => {
                        if let Some(schema) = &step.item_schema {
                            added.push(Triple::new(subject.clone(), p("itemSchema"), text(schema)));
                        }
                    }
                }
                for read in &step.reads {
                    added.push(Triple::new(subject.clone(), p("reads"), node_term(read)));
                }
                replace_subject(names, &mut state, &mut operations, &item, added);
            }
            Move::UpsertSource(source) => {
                let item = names.item(&source.id);
                let here = at("upsert_source");
                if let Some(what) = named(&state, &item).filter(|what| *what != "source") {
                    return Err(format!(
                        "{here}: {} is a {what}; give the source its own id",
                        source.id
                    ));
                }
                if collection(&state, &item) {
                    return Err(format!(
                        "{here}: {} names a collection; give the source its own id",
                        source.id
                    ));
                }
                let subject = NamedNode::new_unchecked(item.clone());
                let p = |name: &str| iri(&format!("{WF}{name}"));
                let mut added = vec![
                    Triple::new(subject.clone(), iri(RDF_TYPE), p("Source")),
                    Triple::new(subject.clone(), p("label"), text(&source.label)),
                    Triple::new(subject.clone(), p("documentId"), text(&source.document_id)),
                ];
                if let Some(block) = &source.from_block {
                    added.push(Triple::new(subject.clone(), p("fromBlock"), text(block)));
                }
                if let Some(block) = &source.to_block {
                    added.push(Triple::new(subject.clone(), p("toBlock"), text(block)));
                }
                replace_subject(names, &mut state, &mut operations, &item, added);
            }
            Move::UpsertDestination(destination) => {
                let item = names.item(&destination.id);
                let here = at("upsert_destination");
                if let Some(what) = named(&state, &item).filter(|what| *what != "destination") {
                    return Err(format!(
                        "{here}: {} is a {what}; give the destination its own id",
                        destination.id
                    ));
                }
                if collection(&state, &item) {
                    return Err(format!(
                        "{here}: {} names a collection; give the destination its own id",
                        destination.id
                    ));
                }
                // What it receives may be produced in a later move (the report
                // says so until it is); a node is refused.
                let receives_item = names.item(&destination.receives);
                let receives_is = if receives_item == item {
                    Some("destination")
                } else {
                    named(&state, &receives_item)
                };
                if let Some(what) = receives_is {
                    return Err(format!(
                        "{here}: {} is a {what}, not a collection",
                        destination.receives
                    ));
                }
                let subject = NamedNode::new_unchecked(item.clone());
                let p = |name: &str| iri(&format!("{WF}{name}"));
                let mut added = vec![
                    Triple::new(subject.clone(), iri(RDF_TYPE), p("Destination")),
                    Triple::new(subject.clone(), p("label"), text(&destination.label)),
                    Triple::new(subject.clone(), p("kind"), text(&destination.kind)),
                    Triple::new(
                        subject.clone(),
                        p("receives"),
                        NamedNode::new_unchecked(receives_item),
                    ),
                ];
                if let Some(document_id) = &destination.document_id {
                    added.push(Triple::new(
                        subject.clone(),
                        p("documentId"),
                        text(document_id),
                    ));
                }
                replace_subject(names, &mut state, &mut operations, &item, added);
            }
            Move::Remove { id } => {
                let item = names.item(id);
                let here = at("remove");
                // The subjects of `predicate` that point at `item`, by id.
                let pointing = |predicate: &str| -> Vec<String> {
                    state
                        .iter()
                        .filter(|t| {
                            t.predicate.as_str() == format!("{WF}{predicate}")
                                && matches!(&t.object, Term::NamedNode(o) if o.as_str() == item)
                        })
                        .filter_map(|t| subject_iri(t).map(local_id))
                        .collect::<BTreeSet<_>>()
                        .into_iter()
                        .collect()
                };
                match named(&state, &item) {
                    Some("phase") => {
                        let held = pointing("inPhase");
                        if !held.is_empty() {
                            return Err(format!(
                                "{here}: phase {id} still holds {}; remove or move those steps first",
                                held.join(", ")
                            ));
                        }
                    }
                    Some("source") => {
                        let readers = pointing("reads");
                        if !readers.is_empty() {
                            return Err(format!(
                                "{here}: source {id} is still read by {}; change those steps first",
                                readers.join(", ")
                            ));
                        }
                    }
                    Some(_) => {}
                    None => {
                        let what = "phase, step, source or destination";
                        return Err(format!("{here}: there is no {what} {id} in this plan"));
                    }
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
/// their steps, the sources and destinations, and any step that sits in no
/// phase of this workflow.
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
    let sources: Vec<Value> = plan
        .typed("Source")
        .into_iter()
        .map(|source| source_view(&plan, source))
        .collect();
    let destinations: Vec<Value> = plan
        .typed("Destination")
        .into_iter()
        .map(|destination| destination_view(&plan, destination))
        .collect();
    let mut view = json!({
        "title": title,
        "phases": phase_values,
        "sources": sources,
        "destinations": destinations,
    });
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
    let mut view = json!({
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
        "reads": plan.iris(step, &p("reads")).into_iter().map(local_id).collect::<Vec<_>>(),
    });
    // A class-bound step (D4): the class IRI here, its catalogue view filled
    // in by `read_result`, which has the store.
    if let Some(class) = plan.iri_of(step, &p("itemClass")) {
        view["itemClass"] = json!({ "iri": class });
    }
    if let Some(pin) = plan.literal(step, &p("itemVocab")) {
        view["itemVocab"] = json!(pin);
    }
    view
}

/// Which class the items of each collection are (D4/D5), by collection IRI:
/// the producing step's `wf:itemClass` (and its pin), or, for a check, the
/// class of the collection it checks (a check passes its items through), with
/// the step whose class it is (`origin`, whose cast being made the items).
#[derive(Debug, Clone)]
struct CollectionClass {
    producer: String,
    origin: String,
    class: String,
    pin: Option<String>,
}

fn collection_classes(draft: &[Triple]) -> BTreeMap<String, CollectionClass> {
    let plan = Plan::new(draft);
    let p = |name: &str| format!("{WF}{name}");
    let steps = plan.typed("AgentNode");
    let mut classes: BTreeMap<String, CollectionClass> = BTreeMap::new();
    for step in &steps {
        let (Some(produces), Some(class)) = (
            plan.iri_of(step, &p("produces")),
            plan.iri_of(step, &p("itemClass")),
        ) else {
            continue;
        };
        classes.insert(
            produces.to_string(),
            CollectionClass {
                producer: step.to_string(),
                origin: step.to_string(),
                class: class.to_string(),
                pin: plan.literal(step, &p("itemVocab")).map(str::to_string),
            },
        );
    }
    // Checks inherit, through chains of checks (at most one per step).
    for _ in 0..steps.len() {
        let mut changed = false;
        for step in &steps {
            let (Some(produces), Some(checks)) = (
                plan.iri_of(step, &p("produces")),
                plan.iri_of(step, &p("checks")),
            ) else {
                continue;
            };
            if classes.contains_key(produces) || plan.iri_of(step, &p("itemClass")).is_some() {
                continue;
            }
            if let Some(checked) = classes.get(checks).cloned() {
                classes.insert(
                    produces.to_string(),
                    CollectionClass {
                        producer: step.to_string(),
                        ..checked
                    },
                );
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    classes
}

fn source_view(plan: &Plan<'_>, source: &str) -> Value {
    let p = |name: &str| format!("{WF}{name}");
    let read_by: Vec<String> = plan
        .typed("AgentNode")
        .into_iter()
        .filter(|step| plan.iris(step, &p("reads")).contains(&source))
        .map(local_id)
        .collect();
    json!({
        "id": local_id(source),
        "label": plan.literal(source, &p("label")),
        "documentId": plan.literal(source, &p("documentId")),
        "fromBlock": plan.literal(source, &p("fromBlock")),
        "toBlock": plan.literal(source, &p("toBlock")),
        "readBy": read_by,
    })
}

fn destination_view(plan: &Plan<'_>, destination: &str) -> Value {
    let p = |name: &str| format!("{WF}{name}");
    json!({
        "id": local_id(destination),
        "label": plan.literal(destination, &p("label")),
        "kind": plan.literal(destination, &p("kind")),
        "receives": plan.iri_of(destination, &p("receives")).map(local_id),
        "documentId": plan.literal(destination, &p("documentId")),
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
    let plan = Plan::new(triples);
    let mut problems: Vec<Problem> = violations
        .iter()
        .map(|violation| shacl_problem(&plan, violation))
        .collect();
    problems.sort_by(|a, b| (&a.focus, &a.message).cmp(&(&b.focus, &b.message)));
    problems
}

/// A violation in the compiler's words: the shape's `sh:message` with
/// `{$this}`, `{?value}`, `{?short}` and `{?count}` (how many values the
/// focus node holds on the shape's path) filled in.
fn shacl_problem(plan: &Plan<'_>, violation: &ViolationRecord) -> Problem {
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
    let count = violation
        .property_path
        .as_deref()
        .map_or(0, |path| plan.values(&focus, bare_iri(path)).len());
    Problem {
        message: template
            .replace("{$this}", &local_id(&focus))
            .replace("{?value}", &value)
            .replace("{?short}", &short)
            .replace("{?count}", &count.to_string()),
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
        // A class-bound step's schema is its class's; when the class is not a
        // type of this graph, the types check names that instead.
        let has_class = !plan.values(step, &p("itemClass")).is_empty();
        match plan.literal(step, &p("itemSchema")) {
            None if !has_schema && !has_class && checks.is_none() => problems.push(
                Problem::structure(step, format!("{short} has no wf:itemSchema")),
            ),
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

    // Sources and destinations (graph-plan-v1, additive): the counts, the
    // shapes of the ids they hold, and what each points at.
    let sources = plan.typed("Source");
    if sources.len() > MAX_SOURCES {
        problems.push(Problem::structure(
            workflow,
            format!("at most {MAX_SOURCES} sources"),
        ));
    }
    let destinations = plan.typed("Destination");
    if destinations.len() > MAX_DESTINATIONS {
        problems.push(Problem::structure(
            workflow,
            format!("at most {MAX_DESTINATIONS} destinations"),
        ));
    }
    for node in sources.iter().chain(destinations.iter()) {
        let document_id = plan.literal(node, &p("documentId"));
        if document_id.is_some_and(|document_id| !document_id_shaped(document_id)) {
            problems.push(Problem::structure(
                node,
                format!("{}'s wf:documentId is not a document id", local_id(node)),
            ));
        }
    }
    for source in &sources {
        for block in ["fromBlock", "toBlock"] {
            if plan
                .literal(source, &p(block))
                .is_some_and(|id| !block_id_shaped(id))
            {
                problems.push(Problem::structure(
                    source,
                    format!("{}'s wf:{block} is not a block id", local_id(source)),
                ));
            }
        }
    }
    for step in &steps {
        for read in plan.iris(step, &p("reads")) {
            if !sources.contains(&read) {
                problems.push(Problem::structure(
                    step,
                    format!(
                        "{} reads {}, which is not a source of this workflow (wf:Source)",
                        local_id(step),
                        local_id(read)
                    ),
                ));
            }
        }
    }
    for destination in &destinations {
        let receives = plan.iri_of(destination, &p("receives"));
        if let Some(receives) = receives.filter(|c| !produced_by.contains_key(c)) {
            problems.push(Problem::structure(
                destination,
                format!(
                    "{} receives {}, which no step produces",
                    local_id(destination),
                    local_id(receives)
                ),
            ));
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
