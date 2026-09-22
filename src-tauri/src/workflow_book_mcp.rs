use crate::{
    app_error::{AppError, AppResult},
    app_runtime::AppHandle,
    emporium::schemas::{MemoryRecordIn, SourceRefIn},
    emporium::vocabs::WORKFLOW_UI_NS as WFUI_NS,
    geist_memory_service::ingest_memory_record,
    mcp_arg_utils::{mcp_arg_string, mcp_arg_string_vec, mcp_arg_usize, mcp_required_graph_id},
    rdf_authority::user_rdf_graph_iri,
    rdf_service::{
        load_rdf_service, run_sparql_query_service, run_sparql_update_service, RdfLoadInput,
        SparqlInput, SparqlUpdateInput,
    },
};
use chrono::{SecondsFormat, Utc};
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use uuid::Uuid;

const WF_NS: &str = "http://mnemosyne.dev/workflow#";
const AGT_NS: &str = "http://mnemosyne.dev/agent#";
const DCTERMS_NS: &str = "http://purl.org/dc/terms/";
const PROV_NS: &str = "http://www.w3.org/ns/prov#";
const DRAFT_DERIVED_FROM_QUERY: &str =
    "workflow_authoring_session.draft(definition subject, composition events, workflow contract)";
const COMPLETENESS_GAP_DERIVED_FROM_QUERY: &str =
    "workflow_authoring_session.draft.completenessGaps(definition subject, composition events, workflow contract)";
const DRAFT_WARNING_DERIVED_FROM_QUERY: &str =
    "workflow_authoring_session.draft.warnings(definition subject, composition events, workflow contract)";
const RUN_STATISTICS_DERIVED_FROM_QUERY: &str =
    "workflow_book.runStatistics(wf:Run, wf:AgentRun, wf:PageTurnDecision history for workflow)";
const COMPOSITION_FOLD_PROJECTION_SOURCE: &str = "wf:CompositionEvent.insertTriple/deleteTriple";

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct WorkflowBookSnapshot {
    graph_id: String,
    read_graph: String,
    workflows: Vec<WorkflowSummary>,
    adventures: Vec<WorkflowAdventurePacket>,
    decisions: Vec<WorkflowPageTurnDecision>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct WorkflowSummary {
    uri: String,
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    when_to_use: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    script_sha256: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    script_block: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    input_block: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    doc_id: Option<String>,
    seeded_from: Vec<String>,
    phases: Vec<WorkflowPhase>,
    nodes: Vec<WorkflowAgentNode>,
    runs: Vec<WorkflowRun>,
    composition_events: Vec<WorkflowCompositionEvent>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct WorkflowPhase {
    uri: String,
    order: usize,
    title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    seeded_from: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct WorkflowAgentNode {
    uri: String,
    label: String,
    phase_index: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    agent_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    doc_id: Option<String>,
    seeded_from: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct WorkflowRun {
    uri: String,
    run_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    started_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ended_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    duration_ms: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    total_tokens: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    agent_count: Option<usize>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct WorkflowCompositionEvent {
    uri: String,
    authoring_session_uri: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    generated_at: Option<String>,
    event_order: i64,
    gesture_kind: String,
    definition_subject: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    target_subject: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    rationale: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    driver_agent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    driver_lease: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    agent_turn_uri: Option<String>,
    insert_triples: Vec<String>,
    delete_triples: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct WorkflowPageTurnDecision {
    subject: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    generated_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    graph_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    workflow_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    from_page: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    intent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    readiness: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    native_suggested_choice: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    recommended_route: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    recommended_route_label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    recommended_choice: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    followed_route: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    followed_route_label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    followed_choice: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    followed_choice_label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    execution_authorized: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    authorization_flag_count: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    rationale: Option<String>,
    evidence: Vec<WorkflowPageTurnEvidence>,
    navigation_routes: Vec<WorkflowPageTurnNavigationRoute>,
    authorization_flags: Vec<WorkflowAuthorizationFlag>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct WorkflowPageTurnEvidence {
    subject: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    role: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    path: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct WorkflowPageTurnNavigationRoute {
    subject: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    route_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    route_label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    route_kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    intent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    choice_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    choice_label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    rationale: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    action_json: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    route_action: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    action_tool: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    action_kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    command_template: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    argument_hash: Option<String>,
    action_arguments: Vec<WorkflowRouteActionArgument>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct WorkflowRouteActionArgument {
    subject: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    value: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct WorkflowRawSparqlQuery {
    subject: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    query: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    order: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    graph_id: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct WorkflowAuthorizationFlag {
    subject: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    page_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    choice_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct WorkflowAdventurePacket {
    subject: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    superseded_page_view: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    generated_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    graph_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    workflow_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    page_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    page_title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    page_scene: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    from_page: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    followed_route: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    intent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    recommended_route: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    recommended_route_label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    recommended_choice: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    visible_object_count: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    warning_count: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    raw_sparql_json: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    authorization_flag_count: Option<usize>,
    raw_sparql_queries: Vec<WorkflowRawSparqlQuery>,
    navigation_routes: Vec<WorkflowPageTurnNavigationRoute>,
    authorization_flags: Vec<WorkflowAuthorizationFlag>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct WorkflowBook {
    kind: String,
    graph_id: String,
    read_graph: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    workflow_name: Option<String>,
    start_page_id: String,
    pages: Vec<WorkflowBookPage>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct WorkflowBookPage {
    id: String,
    kind: String,
    title: String,
    scene: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    summary: Option<String>,
    objects: Vec<Value>,
    warnings: Vec<String>,
    facts: Vec<Value>,
    sections: Vec<Value>,
    choices: Vec<WorkflowBookChoice>,
    #[serde(skip_serializing_if = "Option::is_none")]
    suggested_choice_id: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct WorkflowBookIntentRecommendation {
    intent: String,
    choice_id: String,
    choice_label: String,
    source: String,
    rationale: String,
    semantic_class: String,
    store_mode: String,
    identity_kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    derived_from_query: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct WorkflowBookNavigationRoute {
    id: String,
    label: String,
    kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    intent: Option<String>,
    choice_id: String,
    choice_label: String,
    source: String,
    rationale: String,
    action: Value,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct WorkflowBookChoice {
    id: String,
    kind: String,
    label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    target_page_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    action: Option<Value>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct WorkflowValidationReport {
    workflow_name: String,
    workflow_uri: String,
    passed: bool,
    errors: usize,
    warnings: usize,
    infos: usize,
    issues: Vec<WorkflowValidationIssue>,
    summary: WorkflowValidationSummary,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct WorkflowValidationSummary {
    phase_count: usize,
    agent_count: usize,
    run_count: usize,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct WorkflowValidationIssue {
    severity: String,
    code: String,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    subject: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum WorkflowBookApplyRequest {
    SparqlUpdate {
        update: String,
    },
    RdfLoad {
        data: String,
        format: String,
        base_iri: Option<String>,
        target_graph_iri: Option<String>,
    },
}

impl WorkflowBookApplyRequest {
    fn kind(&self) -> &'static str {
        match self {
            Self::SparqlUpdate { .. } => "sparql_update",
            Self::RdfLoad { .. } => "rdf_load",
        }
    }
}

#[derive(Debug, Clone)]
struct WorkflowBookComposePlan {
    operation: String,
    workflow_name: String,
    workflow_uri: String,
    target_page_id: String,
    update: String,
    definition_projection_source: String,
    definition_insert_count: usize,
    definition_delete_count: usize,
    composition_event_uri: String,
    authoring_session_uri: String,
    composition_event_generated_at: String,
    composition_event_order: i64,
    retained_memory_record: Option<MemoryRecordIn>,
}

#[derive(Debug, Clone)]
struct WorkflowBookCompositionEventPlan {
    event_uri: String,
    authoring_session_uri: String,
    generated_at: String,
    event_order: i64,
    workflow_name: String,
    definition_subject: String,
    target_subject: String,
    gesture_kind: String,
    rationale: Option<String>,
    driver_agent: Option<String>,
    triples: Vec<String>,
    insert_triples: Vec<String>,
    delete_triples: Vec<String>,
}

pub(super) fn mcp_local_workflow_book_open(app: AppHandle, arguments: &Value) -> AppResult<Value> {
    let graph_id = mcp_required_graph_id(arguments).map_err(AppError::validation)?;
    let workflow_name = mcp_arg_string(arguments, &["workflowName", "workflow_name"]);
    let page_id = mcp_arg_string(arguments, &["pageId", "page_id"]);
    let intent = workflow_book_intent(arguments);
    let snapshot = query_workflow_book_snapshot(&app, &graph_id)?;
    let book = render_workflow_book(&snapshot, workflow_name.as_deref())?;
    let reports = validation_reports(&snapshot, workflow_name.as_deref())?;
    let current_page_id = page_id.unwrap_or_else(|| book.start_page_id.clone());
    book_response(
        &snapshot,
        book,
        &current_page_id,
        None,
        &reports,
        intent.as_deref(),
    )
}

pub(super) fn mcp_local_workflow_book_choose(
    app: AppHandle,
    arguments: &Value,
) -> AppResult<Value> {
    let graph_id = mcp_required_graph_id(arguments).map_err(AppError::validation)?;
    let workflow_name = mcp_arg_string(arguments, &["workflowName", "workflow_name"]);
    let page_id = mcp_arg_string(arguments, &["pageId", "page_id"]);
    let intent = workflow_book_intent(arguments);
    let choice_id = mcp_arg_string(arguments, &["choiceId", "choice_id"])
        .ok_or_else(|| AppError::validation("choiceId is required"))?;
    let snapshot = query_workflow_book_snapshot(&app, &graph_id)?;
    let book = render_workflow_book(&snapshot, workflow_name.as_deref())?;
    let current_page_id = page_id.unwrap_or_else(|| book.start_page_id.clone());
    choose_from_book(
        &snapshot,
        book,
        &current_page_id,
        &choice_id,
        intent.as_deref(),
    )
}

pub(super) async fn mcp_local_workflow_book_apply(
    app: AppHandle,
    arguments: &Value,
) -> AppResult<Value> {
    let graph_id = mcp_required_graph_id(arguments).map_err(AppError::validation)?;
    let workflow_name = mcp_arg_string(arguments, &["workflowName", "workflow_name"]);
    let page_id = mcp_arg_string(arguments, &["pageId", "page_id"]);
    let intent = workflow_book_intent(arguments);
    let request = workflow_book_apply_request(arguments).map_err(AppError::validation)?;
    let pre_snapshot = query_workflow_book_snapshot(&app, &graph_id)?;
    let apply_event = workflow_book_apply_event_plan(&pre_snapshot, arguments, request.kind())
        .map_err(AppError::validation)?;
    let retained_memory_record = apply_event
        .as_ref()
        .and_then(composition_event_rationale_memory_record);
    apply_workflow_book_request(
        app,
        &graph_id,
        workflow_name,
        page_id,
        request,
        None,
        apply_event,
        retained_memory_record,
        intent.as_deref(),
    )
    .await
}

pub(super) async fn mcp_local_workflow_book_compose(
    app: AppHandle,
    arguments: &Value,
) -> AppResult<Value> {
    let graph_id = mcp_required_graph_id(arguments).map_err(AppError::validation)?;
    let snapshot = query_workflow_book_snapshot(&app, &graph_id)?;
    let plan = workflow_book_compose_plan(&snapshot, arguments).map_err(AppError::validation)?;
    let page_id = mcp_arg_string(arguments, &["pageId", "page_id"])
        .or_else(|| Some(plan.target_page_id.clone()));
    let intent = workflow_book_intent(arguments);
    let compose = json!({
        "kind": "workflowBookCompose",
        "operation": plan.operation.clone(),
        "workflowName": plan.workflow_name.clone(),
        "workflowUri": plan.workflow_uri.clone(),
        "targetPageId": plan.target_page_id.clone(),
        "update": plan.update.clone(),
        "compositionEvent": {
            "uri": plan.composition_event_uri,
            "authoringSessionUri": plan.authoring_session_uri,
            "generatedAt": plan.composition_event_generated_at,
            "eventOrder": plan.composition_event_order,
        },
        "definitionProjection": {
            "source": plan.definition_projection_source,
            "storeRole": "fold-cache",
            "insertTripleCount": plan.definition_insert_count,
            "deleteTripleCount": plan.definition_delete_count,
        },
        "retainedKnowledge": plan.retained_memory_record.as_ref().map(|record| json!({
            "source": "mem:MemoryRecord",
            "storeRole": "projection:memory",
            "content": record.content.clone(),
        })),
    });
    let retained_memory_record = plan.retained_memory_record;
    apply_workflow_book_request(
        app,
        &graph_id,
        Some(
            compose
                .get("workflowName")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        ),
        page_id,
        WorkflowBookApplyRequest::SparqlUpdate {
            update: compose
                .get("update")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        },
        Some(compose),
        None,
        retained_memory_record,
        intent.as_deref(),
    )
    .await
}

async fn apply_workflow_book_request(
    app: AppHandle,
    graph_id: &str,
    workflow_name: Option<String>,
    page_id: Option<String>,
    request: WorkflowBookApplyRequest,
    compose: Option<Value>,
    apply_event: Option<WorkflowBookCompositionEventPlan>,
    retained_memory_record: Option<MemoryRecordIn>,
    intent: Option<&str>,
) -> AppResult<Value> {
    let operation_kind = request.kind();
    let mutation = match request {
        WorkflowBookApplyRequest::SparqlUpdate { update } => {
            serde_json::to_value(run_sparql_update_service(
                app.clone(),
                SparqlUpdateInput {
                    graph_id: graph_id.to_string(),
                    update,
                },
            )?)
            .map_err(|error| {
                AppError::serialization(format!("serialize sparql update result: {error}"))
            })?
        }
        WorkflowBookApplyRequest::RdfLoad {
            data,
            format,
            base_iri,
            target_graph_iri,
        } => serde_json::to_value(load_rdf_service(
            app.clone(),
            RdfLoadInput {
                graph_id: graph_id.to_string(),
                data,
                format,
                base_iri,
                target_graph_iri,
            },
        )?)
        .map_err(|error| AppError::serialization(format!("serialize rdf load result: {error}")))?,
    };
    let composition_event = if let Some(event) = apply_event {
        let event_update = insert_data_update(&user_rdf_graph_iri(graph_id), event.triples.clone());
        let event_mutation = serde_json::to_value(run_sparql_update_service(
            app.clone(),
            SparqlUpdateInput {
                graph_id: graph_id.to_string(),
                update: event_update,
            },
        )?)
        .map_err(|error| {
            AppError::serialization(format!("serialize composition event result: {error}"))
        })?;
        Some(json!({
            "uri": event.event_uri,
            "authoringSessionUri": event.authoring_session_uri,
            "generatedAt": event.generated_at,
            "eventOrder": event.event_order,
            "mutation": event_mutation,
        }))
    } else {
        None
    };
    let retained_memory = if let Some(record) = retained_memory_record {
        Some(
            ingest_memory_record(&app, graph_id, record)
                .await
                .map_err(AppError::internal)?,
        )
    } else {
        None
    };
    let snapshot = query_workflow_book_snapshot(&app, graph_id)?;
    let reports = validation_reports(&snapshot, workflow_name.as_deref())?;
    let book = render_workflow_book(&snapshot, workflow_name.as_deref())?;
    let current_page_id = page_id.unwrap_or_else(|| book.start_page_id.clone());
    let refreshed = book_response(&snapshot, book, &current_page_id, None, &reports, intent)?;
    let raw_sparql = raw_queries(&snapshot, None);
    let mut authoring = refreshed.get("authoring").cloned().unwrap_or(Value::Null);
    if let Some(authoring) = authoring.as_object_mut() {
        authoring.insert("mutationApplied".to_string(), json!(true));
    }
    let mut response = json!({
        "kind": "workflowBookApplyResult",
        "graphId": snapshot.graph_id.clone(),
        "readGraph": snapshot.read_graph.clone(),
        "workflowName": workflow_name,
        "operation": operation_kind,
        "mutation": mutation,
        "validation": validation_result_value(&reports),
        "authoring": authoring,
        "refreshed": refreshed,
        "raw": {
            "sparql": raw_sparql,
        },
    });
    if let Some(compose) = compose {
        response["compose"] = compose;
    }
    if let Some(composition_event) = composition_event {
        response["compositionEvent"] = composition_event;
    }
    if let Some(retained_memory) = retained_memory {
        response["retainedMemory"] = retained_memory;
    }
    Ok(response)
}

pub(super) fn mcp_local_workflow_book_validate(
    app: AppHandle,
    arguments: &Value,
) -> AppResult<Value> {
    let graph_id = mcp_required_graph_id(arguments).map_err(AppError::validation)?;
    let workflow_name = mcp_arg_string(arguments, &["workflowName", "workflow_name"]);
    let snapshot = query_workflow_book_snapshot(&app, &graph_id)?;
    let reports = validation_reports(&snapshot, workflow_name.as_deref())?;
    let error_count = reports.iter().map(|report| report.errors).sum::<usize>();
    let warning_count = reports.iter().map(|report| report.warnings).sum::<usize>();
    let validation = validation_result_value(&reports);
    let raw_sparql = raw_queries(&snapshot, None);
    Ok(json!({
        "kind": "workflowValidationReport",
        "graphId": snapshot.graph_id.clone(),
        "readGraph": snapshot.read_graph.clone(),
        "workflowName": workflow_name,
        "passed": error_count == 0,
        "summary": {
            "workflowCount": reports.len(),
            "errors": error_count,
            "warnings": warning_count,
        },
        "reports": reports,
        "validation": validation,
        "raw": {
            "sparql": raw_sparql,
        },
    }))
}

fn query_workflow_book_snapshot(
    app: &AppHandle,
    graph_id: &str,
) -> AppResult<WorkflowBookSnapshot> {
    let read_graph = user_rdf_graph_iri(graph_id);
    let graph_named = format!("<{read_graph}>");
    let mut workflows = Vec::new();
    let mut by_uri = BTreeMap::new();
    let mut by_name = BTreeMap::new();
    let mut adventures = Vec::new();
    let mut by_adventure_subject = BTreeMap::new();
    let mut decisions = Vec::new();
    let mut by_decision_subject = BTreeMap::new();

    let workflow_rows = run_query(
        app,
        graph_id,
        format!(
            "SELECT ?workflow ?name ?description ?whenToUse ?sha ?scriptBlock ?inputBlock WHERE {{\n  GRAPH {graph_named} {{\n    ?workflow a <{WF_NS}Workflow> ; <{WF_NS}name> ?name .\n    OPTIONAL {{ ?workflow <{WF_NS}description> ?description }}\n    OPTIONAL {{ ?workflow <{WF_NS}whenToUse> ?whenToUse }}\n    OPTIONAL {{ ?workflow <{WF_NS}scriptSha256> ?sha }}\n    OPTIONAL {{ ?workflow <{WF_NS}scriptBlock> ?scriptBlock }}\n    OPTIONAL {{ ?workflow <{WF_NS}inputBlock> ?inputBlock }}\n  }}\n}}"
        ),
    )?;
    for row in workflow_rows {
        merge_workflow_summary_row(&mut workflows, &mut by_uri, &mut by_name, &row);
    }

    let phase_rows = run_query(
        app,
        graph_id,
        format!(
            "SELECT ?workflow ?phase ?order ?title ?description WHERE {{\n  GRAPH {graph_named} {{\n    ?workflow a <{WF_NS}Workflow> ; <{WF_NS}phase> ?phase .\n    ?phase a <{WF_NS}Phase> ; <{WF_NS}order> ?order ; <{DCTERMS_NS}title> ?title .\n    OPTIONAL {{ ?phase <{DCTERMS_NS}description> ?description }}\n  }}\n}}"
        ),
    )?;
    for row in phase_rows {
        let workflow_uri = row
            .get("workflow")
            .map(|value| strip_uri(value))
            .unwrap_or_default();
        let Some(index) = by_uri.get(&workflow_uri).copied() else {
            continue;
        };
        workflows[index].phases.push(WorkflowPhase {
            uri: row
                .get("phase")
                .map(|value| strip_uri(value))
                .unwrap_or_default(),
            order: row
                .get("order")
                .and_then(|value| literal_usize(value))
                .unwrap_or(0),
            title: row
                .get("title")
                .map(|value| literal_value(value))
                .unwrap_or_else(|| "Untitled phase".to_string()),
            description: row.get("description").map(|value| literal_value(value)),
            seeded_from: Vec::new(),
        });
    }

    let node_rows = run_query(
        app,
        graph_id,
        format!(
            "SELECT ?workflow ?node ?label ?phaseIndex ?agentType WHERE {{\n  GRAPH {graph_named} {{\n    ?node a <{WF_NS}AgentNode> ;\n      <{WF_NS}partOfWorkflow> ?workflow ;\n      <{WF_NS}label> ?label ;\n      <{WF_NS}phaseIndex> ?phaseIndex .\n    OPTIONAL {{ ?node <{WF_NS}agentType> ?agentType }}\n  }}\n}}"
        ),
    )?;
    for row in node_rows {
        let workflow_uri = row
            .get("workflow")
            .map(|value| strip_uri(value))
            .unwrap_or_default();
        let Some(index) = by_uri.get(&workflow_uri).copied() else {
            continue;
        };
        let uri = row
            .get("node")
            .map(|value| strip_uri(value))
            .unwrap_or_default();
        workflows[index].nodes.push(WorkflowAgentNode {
            doc_id: doc_id_from_uri(&uri),
            uri,
            label: row
                .get("label")
                .map(|value| literal_value(value))
                .unwrap_or_else(|| "agent".to_string()),
            phase_index: row
                .get("phaseIndex")
                .and_then(|value| literal_usize(value))
                .unwrap_or(0),
            agent_type: row.get("agentType").map(|value| literal_value(value)),
            seeded_from: Vec::new(),
        });
    }

    let seeded_from_rows = run_query(
        app,
        graph_id,
        format!(
            "SELECT ?subject ?seededFrom WHERE {{\n  GRAPH {graph_named} {{\n    ?subject <{WF_NS}seededFrom> ?seededFrom .\n  }}\n}}"
        ),
    )?;
    for row in seeded_from_rows {
        merge_seeded_from_row(&mut workflows, &row);
    }

    let run_rows = run_query(
        app,
        graph_id,
        format!(
            "SELECT ?run ?workflow ?workflowName ?runId ?status ?started ?ended ?durationMs ?totalTokens ?agentCount WHERE {{\n  GRAPH {graph_named} {{\n    ?run a <{WF_NS}Run> ;\n      <{WF_NS}workflowName> ?workflowName ;\n      <{WF_NS}runId> ?runId .\n    OPTIONAL {{ ?run <{PROV_NS}used> ?workflow }}\n    OPTIONAL {{ ?run <{WF_NS}status> ?status }}\n    OPTIONAL {{ ?run <{PROV_NS}startedAtTime> ?started }}\n    OPTIONAL {{ ?run <{PROV_NS}endedAtTime> ?ended }}\n    OPTIONAL {{ ?run <{WF_NS}durationMs> ?durationMs }}\n    OPTIONAL {{ ?run <{WF_NS}totalTokens> ?totalTokens }}\n    OPTIONAL {{ ?run <{WF_NS}agentCount> ?agentCount }}\n  }}\n}}"
        ),
    )?;
    for row in run_rows {
        let workflow_uri = row.get("workflow").map(|value| strip_uri(value));
        let workflow_name = row.get("workflowName").map(|value| literal_value(value));
        let index = workflow_uri
            .as_deref()
            .and_then(|uri| by_uri.get(uri).copied())
            .or_else(|| {
                workflow_name
                    .as_deref()
                    .and_then(|name| by_name.get(name).copied())
            });
        let Some(index) = index else {
            continue;
        };
        workflows[index].runs.push(WorkflowRun {
            uri: row
                .get("run")
                .map(|value| strip_uri(value))
                .unwrap_or_default(),
            run_id: row
                .get("runId")
                .map(|value| literal_value(value))
                .unwrap_or_default(),
            status: row.get("status").map(|value| literal_value(value)),
            started_at: row.get("started").map(|value| literal_value(value)),
            ended_at: row.get("ended").map(|value| literal_value(value)),
            duration_ms: row.get("durationMs").and_then(|value| literal_usize(value)),
            total_tokens: row
                .get("totalTokens")
                .and_then(|value| literal_usize(value)),
            agent_count: row.get("agentCount").and_then(|value| literal_usize(value)),
        });
    }

    let composition_event_rows =
        run_query(app, graph_id, composition_events_query(&read_graph, None))?;
    for row in composition_event_rows {
        merge_composition_event_row(&mut workflows, &by_uri, &row);
    }

    let adventure_rows = run_query(app, graph_id, workflow_adventures_query(&read_graph, None))?;
    for row in adventure_rows {
        merge_workflow_adventure_row(&mut adventures, &mut by_adventure_subject, &row);
    }
    let superseded_page_views = adventures
        .iter()
        .filter_map(|adventure| adventure.superseded_page_view.clone())
        .collect::<BTreeSet<_>>();
    adventures.retain(|adventure| !superseded_page_views.contains(&adventure.subject));

    let decision_rows = run_query(app, graph_id, page_turn_decisions_query(&read_graph, None))?;
    for row in decision_rows {
        merge_page_turn_decision_row(&mut decisions, &mut by_decision_subject, &row);
    }

    for workflow in &mut workflows {
        workflow.phases.sort_by(|left, right| {
            left.order
                .cmp(&right.order)
                .then_with(|| left.title.cmp(&right.title))
        });
        workflow.nodes.sort_by(|left, right| {
            left.phase_index
                .cmp(&right.phase_index)
                .then_with(|| left.label.cmp(&right.label))
        });
        workflow.runs.sort_by(|left, right| {
            right
                .started_at
                .cmp(&left.started_at)
                .then_with(|| right.run_id.cmp(&left.run_id))
        });
        workflow.composition_events.sort_by(|left, right| {
            left.event_order
                .cmp(&right.event_order)
                .then_with(|| left.uri.cmp(&right.uri))
        });
    }
    workflows.sort_by(|left, right| left.name.cmp(&right.name));
    decisions.sort_by(|left, right| {
        right
            .generated_at
            .cmp(&left.generated_at)
            .then_with(|| left.workflow_name.cmp(&right.workflow_name))
            .then_with(|| left.subject.cmp(&right.subject))
    });
    adventures.sort_by(|left, right| {
        right
            .generated_at
            .cmp(&left.generated_at)
            .then_with(|| left.workflow_name.cmp(&right.workflow_name))
            .then_with(|| left.subject.cmp(&right.subject))
    });

    Ok(WorkflowBookSnapshot {
        graph_id: graph_id.to_string(),
        read_graph,
        workflows,
        adventures,
        decisions,
    })
}

fn merge_workflow_summary_row(
    workflows: &mut Vec<WorkflowSummary>,
    by_uri: &mut BTreeMap<String, usize>,
    by_name: &mut BTreeMap<String, usize>,
    row: &BTreeMap<String, String>,
) {
    let Some(uri) = row.get("workflow").map(|value| strip_uri(value)) else {
        return;
    };
    let name = row
        .get("name")
        .map(|value| literal_value(value))
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| uri.clone());
    let index = if let Some(index) = by_uri.get(&uri).copied() {
        index
    } else {
        let index = workflows.len();
        by_uri.insert(uri.clone(), index);
        workflows.push(WorkflowSummary {
            doc_id: doc_id_from_uri(&uri),
            uri: uri.clone(),
            name: name.clone(),
            description: None,
            when_to_use: None,
            script_sha256: None,
            script_block: None,
            input_block: None,
            seeded_from: Vec::new(),
            phases: Vec::new(),
            nodes: Vec::new(),
            runs: Vec::new(),
            composition_events: Vec::new(),
        });
        index
    };
    by_name.entry(name.clone()).or_insert(index);
    if name.as_str() < workflows[index].name.as_str() {
        workflows[index].name = name;
    }
    merge_optional_summary_value(
        &mut workflows[index].description,
        row.get("description").map(|value| literal_value(value)),
    );
    merge_optional_summary_value(
        &mut workflows[index].when_to_use,
        row.get("whenToUse").map(|value| literal_value(value)),
    );
    merge_optional_summary_value(
        &mut workflows[index].script_sha256,
        row.get("sha").map(|value| literal_value(value)),
    );
    merge_optional_summary_value(
        &mut workflows[index].script_block,
        row.get("scriptBlock").map(|value| strip_uri(value)),
    );
    merge_optional_summary_value(
        &mut workflows[index].input_block,
        row.get("inputBlock").map(|value| strip_uri(value)),
    );
}

fn merge_composition_event_row(
    workflows: &mut [WorkflowSummary],
    by_uri: &BTreeMap<String, usize>,
    row: &BTreeMap<String, String>,
) {
    let Some(definition_subject) = row.get("definitionSubject").map(|value| strip_uri(value))
    else {
        return;
    };
    let Some(index) = by_uri.get(&definition_subject).copied() else {
        return;
    };
    let uri = row
        .get("event")
        .map(|value| strip_uri(value))
        .unwrap_or_default();
    if uri.trim().is_empty() {
        return;
    }
    if let Some(existing) = workflows[index]
        .composition_events
        .iter_mut()
        .find(|event| event.uri == uri)
    {
        merge_composition_event_delta(existing, row);
        return;
    }
    workflows[index].composition_events.push({
        let mut event = WorkflowCompositionEvent {
            uri,
            authoring_session_uri: row
                .get("session")
                .map(|value| strip_uri(value))
                .unwrap_or_default(),
            generated_at: row.get("generatedAt").map(|value| literal_value(value)),
            event_order: row
                .get("eventOrder")
                .and_then(|value| literal_i64(value))
                .unwrap_or(0),
            gesture_kind: row
                .get("gestureKind")
                .map(|value| literal_value(value))
                .unwrap_or_else(|| "unknown".to_string()),
            definition_subject,
            target_subject: row.get("targetSubject").map(|value| strip_uri(value)),
            rationale: row.get("rationale").map(|value| literal_value(value)),
            driver_agent: row.get("driverAgent").map(|value| literal_value(value)),
            driver_lease: row
                .get("sessionDriverLease")
                .map(|value| literal_value(value)),
            agent_turn_uri: row.get("agentTurn").map(|value| strip_uri(value)),
            insert_triples: Vec::new(),
            delete_triples: Vec::new(),
        };
        merge_composition_event_delta(&mut event, row);
        event
    });
}

fn merge_composition_event_delta(
    event: &mut WorkflowCompositionEvent,
    row: &BTreeMap<String, String>,
) {
    merge_optional_summary_value(
        &mut event.driver_lease,
        row.get("sessionDriverLease")
            .map(|value| literal_value(value)),
    );
    if let Some(insert_triple) = row.get("insertTriple").map(|value| literal_value(value)) {
        push_unique_string(&mut event.insert_triples, insert_triple);
    }
    if let Some(delete_triple) = row.get("deleteTriple").map(|value| literal_value(value)) {
        push_unique_string(&mut event.delete_triples, delete_triple);
    }
}

fn push_unique_string(values: &mut Vec<String>, value: String) {
    if !value.trim().is_empty() && !values.iter().any(|existing| existing == &value) {
        values.push(value);
    }
}

fn merge_seeded_from_row(workflows: &mut [WorkflowSummary], row: &BTreeMap<String, String>) {
    let Some(subject) = row.get("subject").map(|value| strip_uri(value)) else {
        return;
    };
    let Some(seed) = row.get("seededFrom").map(|value| strip_uri(value)) else {
        return;
    };
    for workflow in workflows {
        if workflow.uri == subject {
            push_unique_string(&mut workflow.seeded_from, seed.clone());
            return;
        }
        if let Some(phase) = workflow
            .phases
            .iter_mut()
            .find(|phase| phase.uri == subject)
        {
            push_unique_string(&mut phase.seeded_from, seed.clone());
            return;
        }
        if let Some(node) = workflow.nodes.iter_mut().find(|node| node.uri == subject) {
            push_unique_string(&mut node.seeded_from, seed.clone());
            return;
        }
    }
}

fn merge_optional_summary_value(target: &mut Option<String>, value: Option<String>) {
    let Some(value) = value.filter(|value| !value.trim().is_empty()) else {
        return;
    };
    if let Some(existing) = target.as_mut() {
        if value.as_str() < existing.as_str() {
            *existing = value;
        }
    } else {
        *target = Some(value);
    }
}

fn navigation_route_from_row(
    subject: String,
    row: &BTreeMap<String, String>,
) -> WorkflowPageTurnNavigationRoute {
    let mut route = WorkflowPageTurnNavigationRoute {
        subject,
        route_id: None,
        route_label: None,
        route_kind: None,
        intent: None,
        choice_id: None,
        choice_label: None,
        source: None,
        rationale: None,
        action_json: None,
        route_action: None,
        action_tool: None,
        action_kind: None,
        command_template: None,
        argument_hash: None,
        action_arguments: Vec::new(),
    };
    merge_navigation_route_row(&mut route, row);
    route
}

fn merge_navigation_route_row(
    route: &mut WorkflowPageTurnNavigationRoute,
    row: &BTreeMap<String, String>,
) {
    merge_optional_summary_value(
        &mut route.route_id,
        row.get("routeId").map(|value| literal_value(value)),
    );
    merge_optional_summary_value(
        &mut route.route_label,
        row.get("routeLabel").map(|value| literal_value(value)),
    );
    merge_optional_summary_value(
        &mut route.route_kind,
        row.get("routeKind").map(|value| literal_value(value)),
    );
    merge_optional_summary_value(
        &mut route.intent,
        row.get("routeIntent").map(|value| literal_value(value)),
    );
    merge_optional_summary_value(
        &mut route.choice_id,
        row.get("routeChoiceId").map(|value| literal_value(value)),
    );
    merge_optional_summary_value(
        &mut route.choice_label,
        row.get("routeChoiceLabel")
            .map(|value| literal_value(value)),
    );
    merge_optional_summary_value(
        &mut route.source,
        row.get("routeSource").map(|value| literal_value(value)),
    );
    merge_optional_summary_value(
        &mut route.rationale,
        row.get("routeRationale").map(|value| literal_value(value)),
    );
    merge_optional_summary_value(
        &mut route.action_json,
        row.get("routeActionJson").map(|value| literal_value(value)),
    );
    merge_optional_summary_value(
        &mut route.route_action,
        row.get("routeAction").map(|value| strip_uri(value)),
    );
    merge_optional_summary_value(
        &mut route.action_tool,
        row.get("routeActionTool").map(|value| literal_value(value)),
    );
    merge_optional_summary_value(
        &mut route.action_kind,
        row.get("routeActionKind").map(|value| literal_value(value)),
    );
    merge_optional_summary_value(
        &mut route.command_template,
        row.get("routeCommandTemplate")
            .map(|value| literal_value(value)),
    );
    merge_optional_summary_value(
        &mut route.argument_hash,
        row.get("routeArgumentHash")
            .map(|value| literal_value(value)),
    );
    if let Some(argument) = route_action_argument_from_row(row) {
        if !route
            .action_arguments
            .iter()
            .any(|existing| existing.subject == argument.subject)
        {
            route.action_arguments.push(argument);
        }
    }
}

fn merge_page_turn_decision_row(
    decisions: &mut Vec<WorkflowPageTurnDecision>,
    by_subject: &mut BTreeMap<String, usize>,
    row: &BTreeMap<String, String>,
) {
    let Some(subject) = row.get("decision").map(|value| strip_uri(value)) else {
        return;
    };
    let index = if let Some(index) = by_subject.get(&subject).copied() {
        index
    } else {
        let index = decisions.len();
        by_subject.insert(subject.clone(), index);
        decisions.push(WorkflowPageTurnDecision {
            subject: subject.clone(),
            generated_at: None,
            graph_id: None,
            workflow_name: None,
            from_page: None,
            intent: None,
            readiness: None,
            native_suggested_choice: None,
            recommended_route: None,
            recommended_route_label: None,
            recommended_choice: None,
            followed_route: None,
            followed_route_label: None,
            followed_choice: None,
            followed_choice_label: None,
            execution_authorized: None,
            authorization_flag_count: None,
            rationale: None,
            evidence: Vec::new(),
            navigation_routes: Vec::new(),
            authorization_flags: Vec::new(),
        });
        index
    };

    let decision = &mut decisions[index];
    merge_optional_summary_value(
        &mut decision.generated_at,
        row.get("generatedAt").map(|value| literal_value(value)),
    );
    merge_optional_summary_value(
        &mut decision.graph_id,
        row.get("decisionGraphId").map(|value| literal_value(value)),
    );
    merge_optional_summary_value(
        &mut decision.workflow_name,
        row.get("workflowName").map(|value| literal_value(value)),
    );
    merge_optional_summary_value(
        &mut decision.from_page,
        row.get("fromPage").map(|value| literal_value(value)),
    );
    merge_optional_summary_value(
        &mut decision.intent,
        row.get("intent").map(|value| literal_value(value)),
    );
    merge_optional_summary_value(
        &mut decision.readiness,
        row.get("readiness").map(|value| literal_value(value)),
    );
    merge_optional_summary_value(
        &mut decision.native_suggested_choice,
        row.get("nativeSuggestedChoice")
            .map(|value| literal_value(value)),
    );
    merge_optional_summary_value(
        &mut decision.recommended_route,
        row.get("recommendedRoute")
            .map(|value| literal_value(value)),
    );
    merge_optional_summary_value(
        &mut decision.recommended_route_label,
        row.get("recommendedRouteLabel")
            .map(|value| literal_value(value)),
    );
    merge_optional_summary_value(
        &mut decision.recommended_choice,
        row.get("recommendedChoice")
            .map(|value| literal_value(value)),
    );
    merge_optional_summary_value(
        &mut decision.followed_route,
        row.get("followedRoute").map(|value| literal_value(value)),
    );
    merge_optional_summary_value(
        &mut decision.followed_route_label,
        row.get("followedRouteLabel")
            .map(|value| literal_value(value)),
    );
    merge_optional_summary_value(
        &mut decision.followed_choice,
        row.get("followedChoice").map(|value| literal_value(value)),
    );
    merge_optional_summary_value(
        &mut decision.followed_choice_label,
        row.get("followedChoiceLabel")
            .map(|value| literal_value(value)),
    );
    merge_optional_summary_value(
        &mut decision.rationale,
        row.get("rationale").map(|value| literal_value(value)),
    );
    if decision.execution_authorized.is_none() {
        decision.execution_authorized = row
            .get("executionAuthorized")
            .and_then(|value| literal_bool(value));
    }
    if decision.authorization_flag_count.is_none() {
        decision.authorization_flag_count = row
            .get("authorizationFlagCount")
            .and_then(|value| literal_usize(value));
    }

    if let Some(evidence_subject) = row.get("evidence").map(|value| strip_uri(value)) {
        if !evidence_subject.trim().is_empty()
            && !decision
                .evidence
                .iter()
                .any(|evidence| evidence.subject == evidence_subject)
        {
            decision.evidence.push(WorkflowPageTurnEvidence {
                subject: evidence_subject,
                role: row.get("evidenceRole").map(|value| literal_value(value)),
                path: row.get("evidencePath").map(|value| literal_value(value)),
            });
        }
    }

    if let Some(route_subject) = row.get("navigationRoute").map(|value| strip_uri(value)) {
        if !route_subject.trim().is_empty() {
            if let Some(route) = decision
                .navigation_routes
                .iter_mut()
                .find(|route| route.subject == route_subject)
            {
                merge_navigation_route_row(route, row);
            } else {
                decision
                    .navigation_routes
                    .push(navigation_route_from_row(route_subject, row));
            }
        }
    }

    if let Some(flag_subject) = row.get("flag").map(|value| strip_uri(value)) {
        if !flag_subject.trim().is_empty()
            && !decision
                .authorization_flags
                .iter()
                .any(|flag| flag.subject == flag_subject)
        {
            decision
                .authorization_flags
                .push(WorkflowAuthorizationFlag {
                    subject: flag_subject,
                    page_id: row.get("flagPageId").map(|value| literal_value(value)),
                    choice_id: row.get("flagChoiceId").map(|value| literal_value(value)),
                    reason: row.get("flagReason").map(|value| literal_value(value)),
                });
        }
    }
}

fn merge_workflow_adventure_row(
    adventures: &mut Vec<WorkflowAdventurePacket>,
    by_subject: &mut BTreeMap<String, usize>,
    row: &BTreeMap<String, String>,
) {
    let Some(subject) = row.get("adventure").map(|value| strip_uri(value)) else {
        return;
    };
    let superseded_page_view = row
        .get("supersededPageView")
        .map(|value| strip_uri(value))
        .filter(|value| !value.trim().is_empty());
    let index = if let Some(index) = by_subject.get(&subject).copied() {
        index
    } else if let Some(index) = superseded_page_view
        .as_ref()
        .and_then(|superseded| by_subject.get(superseded))
        .copied()
    {
        index
    } else {
        let index = adventures.len();
        by_subject.insert(subject.clone(), index);
        adventures.push(WorkflowAdventurePacket {
            subject: subject.clone(),
            superseded_page_view: None,
            generated_at: None,
            graph_id: None,
            workflow_name: None,
            page_id: None,
            page_title: None,
            page_scene: None,
            from_page: None,
            followed_route: None,
            intent: None,
            recommended_route: None,
            recommended_route_label: None,
            recommended_choice: None,
            visible_object_count: None,
            warning_count: None,
            raw_sparql_json: None,
            authorization_flag_count: None,
            raw_sparql_queries: Vec::new(),
            navigation_routes: Vec::new(),
            authorization_flags: Vec::new(),
        });
        index
    };

    by_subject.insert(subject.clone(), index);
    if let Some(superseded) = superseded_page_view.as_ref() {
        by_subject.insert(superseded.clone(), index);
        if let Some(adventure) = adventures.get_mut(index) {
            if adventure.subject == *superseded || is_canonical_page_view_subject(&subject) {
                adventure.subject = subject.clone();
            }
        }
    }

    let adventure = &mut adventures[index];
    merge_optional_summary_value(
        &mut adventure.generated_at,
        row.get("generatedAt").map(|value| literal_value(value)),
    );
    merge_optional_summary_value(
        &mut adventure.superseded_page_view,
        row.get("supersededPageView").map(|value| strip_uri(value)),
    );
    merge_optional_summary_value(
        &mut adventure.graph_id,
        row.get("adventureGraphId")
            .map(|value| literal_value(value)),
    );
    merge_optional_summary_value(
        &mut adventure.workflow_name,
        row.get("workflowName").map(|value| literal_value(value)),
    );
    merge_optional_summary_value(
        &mut adventure.page_id,
        row.get("pageId").map(|value| literal_value(value)),
    );
    merge_optional_summary_value(
        &mut adventure.page_title,
        row.get("pageTitle").map(|value| literal_value(value)),
    );
    merge_optional_summary_value(
        &mut adventure.page_scene,
        row.get("pageScene").map(|value| literal_value(value)),
    );
    merge_optional_summary_value(
        &mut adventure.from_page,
        row.get("fromPage").map(|value| literal_value(value)),
    );
    merge_optional_summary_value(
        &mut adventure.followed_route,
        row.get("followedRoute").map(|value| literal_value(value)),
    );
    merge_optional_summary_value(
        &mut adventure.intent,
        row.get("intent").map(|value| literal_value(value)),
    );
    merge_optional_summary_value(
        &mut adventure.recommended_route,
        row.get("recommendedRoute")
            .map(|value| literal_value(value)),
    );
    merge_optional_summary_value(
        &mut adventure.recommended_route_label,
        row.get("recommendedRouteLabel")
            .map(|value| literal_value(value)),
    );
    merge_optional_summary_value(
        &mut adventure.recommended_choice,
        row.get("recommendedChoice")
            .map(|value| literal_value(value)),
    );
    merge_optional_summary_value(
        &mut adventure.raw_sparql_json,
        row.get("rawSparqlJson").map(|value| literal_value(value)),
    );
    if let Some(raw_query) = raw_sparql_query_from_row(row) {
        if !adventure
            .raw_sparql_queries
            .iter()
            .any(|query| query.subject == raw_query.subject)
        {
            adventure.raw_sparql_queries.push(raw_query);
        }
    }
    if adventure.visible_object_count.is_none() {
        adventure.visible_object_count = row
            .get("visibleObjectCount")
            .and_then(|value| literal_usize(value));
    }
    if adventure.warning_count.is_none() {
        adventure.warning_count = row
            .get("warningCount")
            .and_then(|value| literal_usize(value));
    }
    if adventure.authorization_flag_count.is_none() {
        adventure.authorization_flag_count = row
            .get("authorizationFlagCount")
            .and_then(|value| literal_usize(value));
    }

    if let Some(route_subject) = row.get("navigationRoute").map(|value| strip_uri(value)) {
        if !route_subject.trim().is_empty() {
            if let Some(route) = adventure
                .navigation_routes
                .iter_mut()
                .find(|route| route.subject == route_subject)
            {
                merge_navigation_route_row(route, row);
            } else if let Some(route_id) = row.get("routeId").map(|value| literal_value(value)) {
                if let Some(route) = adventure
                    .navigation_routes
                    .iter_mut()
                    .find(|route| route.route_id.as_deref() == Some(route_id.as_str()))
                {
                    if is_canonical_page_view_subject(&route_subject) {
                        route.subject = route_subject;
                        if let Some(source) =
                            row.get("routeSource").map(|value| literal_value(value))
                        {
                            route.source = Some(source);
                        }
                    }
                    merge_navigation_route_row(route, row);
                } else {
                    adventure
                        .navigation_routes
                        .push(navigation_route_from_row(route_subject, row));
                }
            } else {
                adventure
                    .navigation_routes
                    .push(navigation_route_from_row(route_subject, row));
            }
        }
    }

    if let Some(flag_subject) = row.get("flag").map(|value| strip_uri(value)) {
        if !flag_subject.trim().is_empty() {
            let page_id = row.get("flagPageId").map(|value| literal_value(value));
            let choice_id = row.get("flagChoiceId").map(|value| literal_value(value));
            if let Some(flag) = adventure.authorization_flags.iter_mut().find(|flag| {
                flag.subject == flag_subject
                    || (flag.page_id == page_id && flag.choice_id == choice_id)
            }) {
                if is_canonical_page_view_subject(&flag_subject) {
                    flag.subject = flag_subject;
                }
                merge_optional_summary_value(&mut flag.page_id, page_id);
                merge_optional_summary_value(&mut flag.choice_id, choice_id);
                merge_optional_summary_value(
                    &mut flag.reason,
                    row.get("flagReason").map(|value| literal_value(value)),
                );
            } else {
                adventure
                    .authorization_flags
                    .push(WorkflowAuthorizationFlag {
                        subject: flag_subject,
                        page_id,
                        choice_id,
                        reason: row.get("flagReason").map(|value| literal_value(value)),
                    });
            }
        }
    }
}

fn is_canonical_page_view_subject(subject: &str) -> bool {
    subject.starts_with("urn:sophia:wf:page-view:")
}

fn run_query(
    app: &AppHandle,
    graph_id: &str,
    query: String,
) -> AppResult<Vec<BTreeMap<String, String>>> {
    let result = run_sparql_query_service(
        app.clone(),
        SparqlInput {
            graph_id: graph_id.to_string(),
            query,
        },
    )?;
    Ok(result.rows)
}

fn render_workflow_book(
    snapshot: &WorkflowBookSnapshot,
    workflow_name: Option<&str>,
) -> AppResult<WorkflowBook> {
    let Some(workflow_name) = workflow_name else {
        return Ok(render_catalog_book(snapshot));
    };
    let workflow = snapshot
        .workflows
        .iter()
        .find(|candidate| candidate.name == workflow_name)
        .ok_or_else(|| AppError::not_found(format!("workflow not found: {workflow_name}")))?;
    Ok(render_single_workflow_book(snapshot, workflow))
}

fn render_catalog_book(snapshot: &WorkflowBookSnapshot) -> WorkflowBook {
    let mut pages = vec![
        catalog_page(snapshot),
        adventure_trail_page(snapshot, None),
        decision_trail_page(snapshot, None),
        raw_page(snapshot, None),
    ];
    for adventure in adventures_for_workflow(snapshot, None) {
        pages.push(adventure_detail_page(snapshot, None, adventure));
    }
    for decision in decisions_for_workflow(snapshot, None) {
        pages.push(decision_launch_page(snapshot, None, decision));
        pages.push(decision_detail_page(snapshot, None, decision));
    }
    WorkflowBook {
        kind: "workflowBook".to_string(),
        graph_id: snapshot.graph_id.clone(),
        read_graph: snapshot.read_graph.clone(),
        workflow_name: None,
        start_page_id: "catalog".to_string(),
        pages,
    }
}

fn catalog_page(snapshot: &WorkflowBookSnapshot) -> WorkflowBookPage {
    let mut choices = snapshot
        .workflows
        .iter()
        .map(|workflow| WorkflowBookChoice {
            id: format!("open-{}", choice_suffix(&workflow.name)),
            kind: "open-workflow".to_string(),
            label: format!("Open {}", workflow.name),
            description: Some(format!(
                "Enter the navigable workflow book for '{}'.",
                workflow.name
            )),
            target_page_id: Some("overview".to_string()),
            action: Some(json!({
                "workflowName": workflow.name,
            })),
        })
        .collect::<Vec<_>>();
    choices.push(compose_template_choice(
        "compose-create-workflow",
        "Compose new workflow",
        "Use workflow_book_compose to mint workflow RDF, then return to the refreshed validated book.",
        json!({
            "graphId": snapshot.graph_id,
            "operation": "create_workflow",
            "workflowName": "new-workflow",
            "description": "Describe the workflow.",
            "whenToUse": "State when an agent should use this workflow.",
            "sourceDocumentId": "document-id",
            "sourceBlockId": "block-id",
            "scriptSha256": "sha256-if-known",
            "inputDocumentId": "input-document-id",
            "inputBlockId": "input-block-id"
        }),
    ));
    choices.push(navigate_choice_with_description(
        "adventure-trail",
        "Adventure trail",
        "adventure-trail",
        "Inspect retained Workflow PageView objects in this graph.",
    ));
    choices.push(navigate_choice_with_description(
        "decision-trail",
        "Decision trail",
        "decision-trail",
        "Inspect retained workflow-book PageTurnDecision objects in this graph.",
    ));
    choices.push(navigate_choice_with_description(
        "raw-sparql",
        "Raw SPARQL",
        "raw",
        "Inspect the exact SPARQL queries behind the catalog.",
    ));
    let suggested_choice_id = choices.first().map(|choice| choice.id.clone());
    let warnings = if snapshot.workflows.is_empty() {
        vec!["No workflow definitions were found in the RDF graph.".to_string()]
    } else {
        Vec::new()
    };

    WorkflowBookPage {
        id: "catalog".to_string(),
        kind: "catalog".to_string(),
        title: "Workflow Books".to_string(),
        scene: format!(
            "You are looking at {} workflow book(s) in graph '{}'. Raw SPARQL remains available as a first-class path.",
            snapshot.workflows.len(),
            snapshot.graph_id
        ),
        summary: Some("Choose a workflow, or inspect the raw RDF/SPARQL surface.".to_string()),
        objects: snapshot.workflows.iter().map(workflow_object).collect(),
        warnings,
        facts: vec![
            json!({"label": "graphId", "value": snapshot.graph_id}),
            json!({"label": "readGraph", "value": snapshot.read_graph}),
            json!({"label": "workflowCount", "value": snapshot.workflows.len()}),
            json!({"label": "adventureCount", "value": adventures_for_workflow(snapshot, None).len()}),
            json!({"label": "decisionCount", "value": decisions_for_workflow(snapshot, None).len()}),
        ],
        sections: vec![json!({
            "id": "workflows",
            "title": "Workflows",
            "items": snapshot.workflows.iter().map(workflow_catalog_item).collect::<Vec<_>>(),
        })],
        choices,
        suggested_choice_id,
    }
}

fn render_single_workflow_book(
    snapshot: &WorkflowBookSnapshot,
    workflow: &WorkflowSummary,
) -> WorkflowBook {
    let mut pages = vec![overview_page(snapshot, workflow)];
    pages.push(draft_page(snapshot, workflow));
    pages.push(composition_trail_page(snapshot, workflow));
    pages.push(perception_page(snapshot, workflow));
    for phase in &workflow.phases {
        pages.push(phase_page(snapshot, workflow, phase));
    }
    for node in &workflow.nodes {
        pages.push(agent_page(snapshot, workflow, node));
    }
    pages.push(validation_page(snapshot, workflow));
    pages.push(execute_page(snapshot, workflow));
    pages.push(adventure_trail_page(snapshot, Some(workflow)));
    for adventure in adventures_for_workflow(snapshot, Some(workflow)) {
        pages.push(adventure_detail_page(snapshot, Some(workflow), adventure));
    }
    pages.push(decision_trail_page(snapshot, Some(workflow)));
    for decision in decisions_for_workflow(snapshot, Some(workflow)) {
        pages.push(decision_launch_page(snapshot, Some(workflow), decision));
        pages.push(decision_detail_page(snapshot, Some(workflow), decision));
    }
    pages.push(catalog_page(snapshot));
    pages.push(raw_page(snapshot, Some(workflow)));

    WorkflowBook {
        kind: "workflowBook".to_string(),
        graph_id: snapshot.graph_id.clone(),
        read_graph: snapshot.read_graph.clone(),
        workflow_name: Some(workflow.name.clone()),
        start_page_id: "overview".to_string(),
        pages,
    }
}

fn overview_page(snapshot: &WorkflowBookSnapshot, workflow: &WorkflowSummary) -> WorkflowBookPage {
    let mut choices = Vec::new();
    if let Some(first_phase) = workflow.phases.first() {
        choices.push(navigate_choice_with_description(
            "enter-phases",
            "Enter phases",
            &phase_page_id(first_phase.order),
            "Walk the workflow phase by phase.",
        ));
    }
    choices.push(navigate_choice_with_description(
        "perception-map",
        "Perception map",
        "perception",
        "Choose the next graph percept or conceptual route through this workflow.",
    ));
    choices.push(navigate_choice_with_description(
        "authoring-draft",
        "Authoring draft",
        "authoring-draft",
        "Inspect the virtual wf:Draft, blocking gaps, and non-blocking warnings for this workflow.",
    ));
    choices.push(navigate_choice_with_description(
        "validate-graph",
        "Validate graph anatomy",
        "validation",
        "Check required RDF anatomy before execution.",
    ));
    choices.push(navigate_choice_with_description(
        "execute-async",
        "Prepare async run",
        "execute",
        "Open explicit Choreograph run actions for this workflow.",
    ));
    choices.push(navigate_choice_with_description(
        "composition-trail",
        "Composition trail",
        "composition-trail",
        "Inspect the authoring events that formed this workflow.",
    ));
    let next_phase_order = workflow
        .phases
        .iter()
        .map(|phase| phase.order)
        .max()
        .unwrap_or(0)
        + 1;
    choices.push(compose_template_choice(
        "compose-add-phase",
        "Compose phase",
        "Use workflow_book_compose to add one ordered phase, validate, and reopen this workflow book.",
        json!({
            "graphId": snapshot.graph_id,
            "operation": "add_phase",
            "workflowName": workflow.name,
            "phaseOrder": next_phase_order,
            "phaseTitle": format!("Phase {next_phase_order}"),
            "phaseDescription": "Describe the phase objective."
        }),
    ));
    if let Some(first_phase) = workflow.phases.first() {
        choices.push(compose_template_choice(
            "compose-attach-agent-node",
            "Compose agent node",
            "Use workflow_book_compose to attach a named agent node to an existing phase.",
            json!({
                "graphId": snapshot.graph_id,
                "operation": "attach_agent_node",
                "workflowName": workflow.name,
                "label": "agent-label",
                "phaseIndex": first_phase.order,
                "agentType": "scout",
                "documentId": "agent-node-document-id"
            }),
        ));
    }
    choices.push(compose_template_choice(
        "compose-bind-source-block",
        "Bind source block",
        "Use workflow_book_compose to bind or replace the Garden block that contains workflow source.",
        json!({
            "graphId": snapshot.graph_id,
            "operation": "bind_source_block",
            "workflowName": workflow.name,
            "sourceDocumentId": "source-document-id",
            "sourceBlockId": "source-block-id",
            "scriptSha256": "sha256-if-known"
        }),
    ));
    choices.push(compose_template_choice(
        "compose-bind-input-block",
        "Bind input block",
        "Use workflow_book_compose to bind a Garden block containing JSON workflow arguments.",
        json!({
            "graphId": snapshot.graph_id,
            "operation": "bind_input_block",
            "workflowName": workflow.name,
            "inputDocumentId": "input-document-id",
            "inputBlockId": "input-block-id"
        }),
    ));
    choices.push(navigate_choice_with_description(
        "adventure-trail",
        "Adventure trail",
        "adventure-trail",
        "Inspect retained source-side adventure packets for this workflow.",
    ));
    choices.push(navigate_choice_with_description(
        "decision-trail",
        "Decision trail",
        "decision-trail",
        "Inspect retained page-turn decisions for this workflow.",
    ));
    choices.push(navigate_choice_with_description(
        "raw-sparql",
        "Raw SPARQL",
        "raw",
        "Inspect or run the exact RDF queries behind this book.",
    ));
    choices.push(navigate_choice_with_description(
        "catalog",
        "Workflow catalog",
        "catalog",
        "Return to the graph's workflow catalog.",
    ));
    let report = validate_workflow(workflow);
    let suggested_choice_id = Some(if report.passed {
        "execute-async".to_string()
    } else {
        "authoring-draft".to_string()
    });

    WorkflowBookPage {
        id: "overview".to_string(),
        kind: "overview".to_string(),
        title: workflow.name.clone(),
        scene: workflow_scene(workflow),
        summary: workflow.description.clone().or_else(|| {
            Some("A graph-native workflow definition surfaced as a navigable book.".to_string())
        }),
        objects: overview_objects(snapshot, workflow),
        warnings: report_warning_messages(&report),
        facts: workflow_facts(snapshot, workflow),
        sections: vec![
            json!({
                "id": "phases",
                "title": "Phases",
                "items": workflow.phases.iter().map(|phase| json!({
                    "order": phase.order,
                    "title": phase.title,
                    "description": phase.description,
                    "pageId": phase_page_id(phase.order),
                    "agentCount": workflow.nodes.iter().filter(|node| node.phase_index == phase.order).count(),
                })).collect::<Vec<_>>(),
            }),
            json!({
                "id": "recentRuns",
                "title": "Recent runs",
                "items": workflow.runs.iter().take(5).map(run_item).collect::<Vec<_>>(),
            }),
        ],
        choices,
        suggested_choice_id,
    }
}

fn draft_page(snapshot: &WorkflowBookSnapshot, workflow: &WorkflowSummary) -> WorkflowBookPage {
    let report = validate_workflow(workflow);
    let gaps = draft_completeness_gap_objects(&report);
    let draft_warnings = draft_warning_objects(&report);
    let info_items = draft_info_objects(&report);
    let mut objects = vec![draft_object(workflow, &report)];
    objects.extend(gaps.iter().cloned());
    objects.extend(draft_warnings.iter().cloned());

    let mut choices = draft_repair_choices(snapshot, workflow, &report);
    choices.push(WorkflowBookChoice {
        id: "validate-now".to_string(),
        kind: "mcp-tool".to_string(),
        label: "Validate now".to_string(),
        description: Some(
            "Run the workflow_book_validate MCP tool for a fresh report.".to_string(),
        ),
        target_page_id: None,
        action: Some(json!({
            "tool": "workflow_book_validate",
            "arguments": {
                "graphId": snapshot.graph_id,
                "workflowName": workflow.name,
            },
        })),
    });
    choices.push(navigate_choice_with_description(
        "composition-trail",
        "Composition trail",
        "composition-trail",
        "Inspect the authoring events that produced the current definition.",
    ));
    choices.push(navigate_choice_with_description(
        "validate-graph",
        "Validation",
        "validation",
        "Open the detailed validation report and raw validation queries.",
    ));
    choices.push(navigate_choice_with_description(
        "overview",
        "Overview",
        "overview",
        "Return to the workflow overview.",
    ));

    let suggested_choice_id = if report.passed {
        Some("validate-now".to_string())
    } else {
        choices
            .iter()
            .find(|choice| choice.kind == "mcp-tool-template")
            .map(|choice| choice.id.clone())
            .or_else(|| Some("validate-now".to_string()))
    };

    WorkflowBookPage {
        id: "authoring-draft".to_string(),
        kind: "authoringDraft".to_string(),
        title: "Authoring Draft".to_string(),
        scene: if report.passed {
            format!(
                "The virtual wf:Draft for '{}' has no blocking completeness gaps.",
                workflow.name
            )
        } else {
            format!(
                "The virtual wf:Draft for '{}' has {} blocking gap(s).",
                workflow.name, report.errors
            )
        },
        summary: Some(
            "This page is derived at read time from the workflow definition, composition events, and local validation; it stores no authoritative RDF."
                .to_string(),
        ),
        objects,
        warnings: report_warning_messages(&report),
        facts: vec![
            json!({"label": "workflowUri", "value": workflow.uri}),
            json!({"label": "semanticClass", "value": "wf:Draft"}),
            json!({"label": "sourceKind", "value": "derived"}),
            json!({"label": "storeMode", "value": "virtual"}),
            json!({"label": "identityKind", "value": "resolve-by-query"}),
            json!({"label": "runnable", "value": report.passed}),
            json!({"label": "blockingGapCount", "value": report.errors}),
            json!({"label": "draftWarningCount", "value": report.warnings}),
            json!({"label": "derivedFromQuery", "value": DRAFT_DERIVED_FROM_QUERY}),
        ],
        sections: vec![
            json!({
                "id": "completenessGaps",
                "title": "Completeness gaps",
                "items": gaps,
            }),
            json!({
                "id": "draftWarnings",
                "title": "Draft warnings",
                "items": draft_warnings,
            }),
            json!({
                "id": "draftInfo",
                "title": "Draft info",
                "items": info_items,
            }),
        ],
        choices,
        suggested_choice_id,
    }
}

fn composition_trail_page(
    snapshot: &WorkflowBookSnapshot,
    workflow: &WorkflowSummary,
) -> WorkflowBookPage {
    let fold_check = workflow_fold_check(workflow);
    let fold_status = fold_check
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_string();
    let mut choices = vec![
        perceptual_sparql_choice(
            "perceive-composition-events",
            "Perceive composition events",
            "Run the exact SPARQL query that backs this composition trail.",
            snapshot,
            composition_events_query(&snapshot.read_graph, Some(workflow.uri.as_str())),
        ),
        navigate_choice_with_description(
            "overview",
            "Overview",
            "overview",
            "Return to the workflow overview.",
        ),
        navigate_choice_with_description(
            "raw-sparql",
            "Raw SPARQL",
            "raw",
            "Inspect or run the exact RDF queries behind this book.",
        ),
    ];
    choices.push(navigate_choice_with_description(
        "catalog",
        "Workflow catalog",
        "catalog",
        "Return to the graph's workflow catalog.",
    ));
    let warnings = if workflow.composition_events.is_empty() {
        vec!["No wf:CompositionEvent records are visible for this workflow yet.".to_string()]
    } else if fold_status == "partial" {
        vec![
            "The composition event log is only partially replayable; at least one event lacks fold delta triples or the log predates create_workflow."
                .to_string(),
        ]
    } else if fold_status == "diverged" {
        vec![
            "The composition event fold does not match the current workflow definition cache."
                .to_string(),
        ]
    } else {
        Vec::new()
    };
    let mut objects = vec![fold_check.clone()];
    objects.extend(
        workflow
            .composition_events
            .iter()
            .map(composition_event_object),
    );

    WorkflowBookPage {
        id: "composition-trail".to_string(),
        kind: "compositionTrail".to_string(),
        title: "Composition Trail".to_string(),
        scene: format!(
            "This workflow has {} recorded composition event(s).",
            workflow.composition_events.len()
        ),
        summary: Some(
            "The workflow birth narrative: authoring sessions, compose gestures, and Agent turn projections."
                .to_string(),
        ),
        objects,
        warnings,
        facts: vec![
            json!({"label": "graphId", "value": snapshot.graph_id}),
            json!({"label": "workflowUri", "value": workflow.uri}),
            json!({"label": "compositionEventCount", "value": workflow.composition_events.len()}),
            json!({"label": "definitionFoldStatus", "value": fold_status}),
            json!({"label": "replayableEventCount", "value": fold_check.pointer("/replayableEventCount").cloned().unwrap_or(Value::Null)}),
        ],
        sections: vec![
            json!({
                "id": "definitionFold",
                "title": "Definition fold",
                "items": [fold_check],
            }),
            json!({
                "id": "compositionEvents",
                "title": "Composition events",
                "items": workflow
                    .composition_events
                    .iter()
                    .map(composition_event_item)
                    .collect::<Vec<_>>(),
            }),
        ],
        choices,
        suggested_choice_id: Some("perceive-composition-events".to_string()),
    }
}

fn perception_page(
    snapshot: &WorkflowBookSnapshot,
    workflow: &WorkflowSummary,
) -> WorkflowBookPage {
    let mut choices = Vec::new();
    choices.push(perceptual_sparql_choice(
        "perceive-workflow-anatomy",
        "Perceive workflow anatomy",
        "Return the agent nodes and phase indexes that define this workflow.",
        snapshot,
        workflow_nodes_query(&snapshot.read_graph, &workflow.uri),
    ));
    choices.push(perceptual_sparql_choice(
        "perceive-bound-blocks",
        "Perceive bound blocks",
        "Return the Garden blocks bound as workflow source and workflow input.",
        snapshot,
        workflow_blocks_query(&snapshot.read_graph, &workflow.uri),
    ));
    choices.push(perceptual_sparql_choice(
        "perceive-run-history",
        "Perceive run history",
        "Return recorded runs that are linked to this workflow.",
        snapshot,
        workflow_runs_for_workflow_query(&snapshot.read_graph, workflow),
    ));
    choices.extend(workflow.phases.iter().map(|phase| {
        conceptual_route_choice(
            &format!("concept-phase-{}", phase.order),
            &format!("Follow phase {}", phase.order),
            &phase_page_id(phase.order),
            format!(
                "Treat '{}' as the next conceptual node in the workflow.",
                phase.title
            ),
        )
    }));
    choices.extend(workflow.nodes.iter().map(|node| {
        conceptual_route_choice(
            &format!("concept-agent-{}", choice_suffix(&node.label)),
            &format!("Follow {}", node.label),
            &agent_page_id(&node.label),
            format!(
                "Treat agent node '{}' as the next conceptual object.",
                node.label
            ),
        )
    }));
    if let Some(input_block) = workflow.input_block.as_deref().and_then(doc_block_from_uri) {
        choices.push(WorkflowBookChoice {
            id: "perceive-input-block".to_string(),
            kind: "perceptual-tool".to_string(),
            label: "Perceive input block".to_string(),
            description: Some(
                "Read the Garden block that supplies workflow arguments.".to_string(),
            ),
            target_page_id: None,
            action: Some(json!({
                "tool": "get_block",
                "arguments": {
                    "graphId": snapshot.graph_id,
                    "documentId": input_block.document_id,
                    "blockId": input_block.block_id,
                    "format": "text",
                },
            })),
        });
    }
    if let Some(source_block) = workflow
        .script_block
        .as_deref()
        .and_then(doc_block_from_uri)
    {
        choices.push(WorkflowBookChoice {
            id: "perceive-source-block".to_string(),
            kind: "perceptual-tool".to_string(),
            label: "Perceive source block".to_string(),
            description: Some("Read the Garden block that supplies workflow source.".to_string()),
            target_page_id: None,
            action: Some(json!({
                "tool": "get_block",
                "arguments": {
                    "graphId": snapshot.graph_id,
                    "documentId": source_block.document_id,
                    "blockId": source_block.block_id,
                    "format": "text",
                },
            })),
        });
    }
    choices.push(navigate_choice_with_description(
        "validate-graph",
        "Validate graph anatomy",
        "validation",
        "Check whether the current perception has enough workflow anatomy to run.",
    ));
    choices.push(navigate_choice_with_description(
        "execute-async",
        "Prepare async run",
        "execute",
        "Convert this graph perception into an explicit Choreograph run action.",
    ));
    choices.push(navigate_choice_with_description(
        "adventure-trail",
        "Adventure trail",
        "adventure-trail",
        "Inspect retained source-side adventure packets before choosing an execution path.",
    ));
    choices.push(navigate_choice_with_description(
        "decision-trail",
        "Decision trail",
        "decision-trail",
        "Inspect retained page-turn decisions before choosing an execution path.",
    ));
    choices.push(navigate_choice_with_description(
        "raw-sparql",
        "Raw SPARQL",
        "raw",
        "Inspect or run the exact RDF queries behind this perception page.",
    ));
    choices.push(navigate_choice_with_description(
        "overview",
        "Overview",
        "overview",
        "Return to the workflow overview.",
    ));
    let suggested_choice_id = workflow
        .phases
        .first()
        .map(|phase| format!("concept-phase-{}", phase.order))
        .or_else(|| Some("perceive-workflow-anatomy".to_string()));

    WorkflowBookPage {
        id: "perception".to_string(),
        kind: "perception".to_string(),
        title: "Perception Map".to_string(),
        scene: format!(
            "You are choosing how to perceive '{}' next: ask a graph question, inspect a bound block, or follow a concept page.",
            workflow.name
        ),
        summary: Some(
            "This is a choose-your-own-adventure layer over the same RDF surface; SPARQL remains available as explicit actions."
                .to_string(),
        ),
        objects: perception_objects(workflow),
        warnings: Vec::new(),
        facts: vec![
            json!({"label": "graphId", "value": snapshot.graph_id}),
            json!({"label": "workflowUri", "value": workflow.uri}),
            json!({"label": "perceptCount", "value": 3}),
            json!({"label": "conceptCount", "value": workflow.phases.len() + workflow.nodes.len()}),
        ],
        sections: vec![
            json!({
                "id": "percepts",
                "title": "Percepts",
                "items": [
                    {
                        "id": "workflow-anatomy",
                        "kind": "sparqlPercept",
                        "choiceId": "perceive-workflow-anatomy",
                        "label": "Workflow anatomy"
                    },
                    {
                        "id": "bound-blocks",
                        "kind": "sparqlPercept",
                        "choiceId": "perceive-bound-blocks",
                        "label": "Bound source and input blocks"
                    },
                    {
                        "id": "run-history",
                        "kind": "sparqlPercept",
                        "choiceId": "perceive-run-history",
                        "label": "Run history"
                    }
                ],
            }),
            json!({
                "id": "concepts",
                "title": "Concepts",
                "items": workflow.phases.iter().map(|phase| json!({
                    "kind": "phaseConcept",
                    "label": phase.title,
                    "phaseOrder": phase.order,
                    "pageId": phase_page_id(phase.order),
                    "choiceId": format!("concept-phase-{}", phase.order),
                })).chain(workflow.nodes.iter().map(|node| json!({
                    "kind": "agentConcept",
                    "label": node.label,
                    "phaseIndex": node.phase_index,
                    "agentType": node.agent_type,
                    "pageId": agent_page_id(&node.label),
                    "choiceId": format!("concept-agent-{}", choice_suffix(&node.label)),
                }))).collect::<Vec<_>>(),
            }),
        ],
        choices,
        suggested_choice_id,
    }
}

fn adventure_trail_page(
    snapshot: &WorkflowBookSnapshot,
    workflow: Option<&WorkflowSummary>,
) -> WorkflowBookPage {
    let adventures = adventures_for_workflow(snapshot, workflow);
    let mut choices = Vec::new();
    for (index, adventure) in adventures.iter().enumerate() {
        choices.push(navigate_choice_with_description(
            &format!("open-adventure-{}", index + 1),
            &format!("Inspect {}", adventure_title(adventure)),
            &adventure_page_id(&adventure.subject),
            "Open the retained Workflow PageView as a book page.",
        ));
    }
    choices.push(perceptual_sparql_choice(
        "perceive-workflow-adventures",
        "Perceive PageView RDF",
        "Run the exact SPARQL query that backs this adventure trail.",
        snapshot,
        workflow_adventures_query(
            &snapshot.read_graph,
            workflow.map(|item| item.name.as_str()),
        ),
    ));
    if workflow.is_some() {
        choices.push(navigate_choice_with_description(
            "overview",
            "Overview",
            "overview",
            "Return to the workflow overview.",
        ));
    }
    choices.push(navigate_choice_with_description(
        "decision-trail",
        "Decision trail",
        "decision-trail",
        "Inspect retained page-turn decisions for this scope.",
    ));
    choices.push(navigate_choice_with_description(
        "raw-sparql",
        "Raw SPARQL",
        "raw",
        "Inspect or run the exact RDF queries behind this book.",
    ));
    choices.push(navigate_choice_with_description(
        "catalog",
        "Workflow catalog",
        "catalog",
        "Return to the graph's workflow catalog.",
    ));
    let warnings = if adventures.is_empty() {
        vec![
            "No wf:PageView or legacy wfui:WorkflowAdventurePacket objects are visible for this scope. Load a retained PageView Turtle artifact first."
                .to_string(),
        ]
    } else {
        Vec::new()
    };
    let scope = workflow
        .map(|workflow| format!("workflow '{}'", workflow.name))
        .unwrap_or_else(|| format!("graph '{}'", snapshot.graph_id));

    WorkflowBookPage {
        id: "adventure-trail".to_string(),
        kind: "adventureTrail".to_string(),
        title: "Adventure Trail".to_string(),
        scene: format!(
            "You are reading {} retained Workflow PageView object(s) for {scope}.",
            adventures.len()
        ),
        summary: Some(
            "This page is the book-native shelf for Workflow PageViews: page perception, conceptual routes, retained recommended action, and raw SPARQL."
                .to_string(),
        ),
        objects: adventures.iter().map(|adventure| adventure_object(adventure)).collect(),
        warnings,
        facts: vec![
            json!({"label": "graphId", "value": snapshot.graph_id}),
            json!({"label": "readGraph", "value": snapshot.read_graph}),
            json!({"label": "workflowName", "value": workflow.map(|item| item.name.as_str())}),
            json!({"label": "pageViewCount", "value": adventures.len()}),
        ],
        sections: vec![json!({
            "id": "workflowPageViews",
            "title": "Workflow PageViews",
            "items": adventures.iter().enumerate().map(|(index, adventure)| {
                json!({
                    "index": index + 1,
                    "subject": adventure.subject,
                    "generatedAt": adventure.generated_at,
                    "workflowName": adventure.workflow_name,
                    "pageId": adventure.page_id,
                    "pageTitle": adventure.page_title,
                    "intent": adventure.intent,
                    "recommendedRoute": adventure.recommended_route,
                    "recommendedRouteLabel": adventure.recommended_route_label,
                    "recommendedChoice": adventure.recommended_choice,
                    "navigationRouteCount": adventure.navigation_routes.len(),
                    "rawSparqlQueryCount": adventure_raw_sparql_count(adventure),
                    "pageIdForBook": adventure_page_id(&adventure.subject),
                    "choiceId": format!("open-adventure-{}", index + 1),
                })
            }).collect::<Vec<_>>(),
        })],
        choices,
        suggested_choice_id: if adventures.is_empty() {
            Some("perceive-workflow-adventures".to_string())
        } else {
            Some("open-adventure-1".to_string())
        },
    }
}

fn adventure_detail_page(
    snapshot: &WorkflowBookSnapshot,
    workflow: Option<&WorkflowSummary>,
    adventure: &WorkflowAdventurePacket,
) -> WorkflowBookPage {
    let primary_route = adventure_primary_route(adventure);
    let mut choices = Vec::new();
    for route in &adventure.navigation_routes {
        choices.push(retained_adventure_route_choice(adventure, route));
    }
    choices.push(perceptual_sparql_choice(
        "perceive-this-adventure",
        "Perceive this PageView RDF",
        "Run the exact SPARQL query for this PageView and its retained route map.",
        snapshot,
        workflow_adventure_detail_query(&snapshot.read_graph, &adventure.subject),
    ));
    choices.push(navigate_choice_with_description(
        "adventure-trail",
        "Adventure trail",
        "adventure-trail",
        "Return to the retained adventure trail.",
    ));
    choices.push(navigate_choice_with_description(
        "decision-trail",
        "Decision trail",
        "decision-trail",
        "Inspect retained page-turn decisions for this scope.",
    ));
    if workflow.is_some() {
        choices.push(navigate_choice_with_description(
            "overview",
            "Overview",
            "overview",
            "Return to the workflow overview.",
        ));
    }
    choices.push(navigate_choice_with_description(
        "raw-sparql",
        "Raw SPARQL",
        "raw",
        "Inspect or run the exact RDF queries behind this book.",
    ));

    let suggested_choice_id = primary_route
        .and_then(|route| route.route_id.as_deref())
        .map(|route_id| format!("route-{}", choice_suffix(route_id)))
        .filter(|choice_id| choices.iter().any(|choice| choice.id == *choice_id))
        .or_else(|| Some("adventure-trail".to_string()));

    WorkflowBookPage {
        id: adventure_page_id(&adventure.subject),
        kind: "adventureDetail".to_string(),
        title: adventure_title(adventure),
        scene: adventure.page_scene.clone().unwrap_or_else(|| {
            format!(
                "You are inspecting one retained Workflow PageView for page '{}'.",
                adventure.page_id.as_deref().unwrap_or("unknown")
            )
        }),
        summary: Some(
            "This is a retained source-side PageView. Its routes preserve the original source action, and raw SPARQL remains available as exact graph perception."
                .to_string(),
        ),
        objects: adventure_detail_objects(adventure),
        warnings: adventure_warnings(adventure),
        facts: vec![
            json!({"label": "subject", "value": adventure.subject}),
            json!({"label": "generatedAt", "value": adventure.generated_at}),
            json!({"label": "graphId", "value": adventure.graph_id}),
            json!({"label": "workflowName", "value": adventure.workflow_name}),
            json!({"label": "pageId", "value": adventure.page_id}),
            json!({"label": "pageTitle", "value": adventure.page_title}),
            json!({"label": "intent", "value": adventure.intent}),
            json!({"label": "recommendedRoute", "value": adventure.recommended_route}),
            json!({"label": "recommendedRouteLabel", "value": adventure.recommended_route_label}),
            json!({"label": "recommendedChoice", "value": adventure.recommended_choice}),
            json!({"label": "visibleObjectCount", "value": adventure.visible_object_count}),
            json!({"label": "warningCount", "value": adventure.warning_count}),
            json!({"label": "authorizationFlagCount", "value": adventure.authorization_flag_count}),
            json!({"label": "rawSparqlQueryCount", "value": adventure_raw_sparql_count(adventure)}),
        ],
        sections: vec![
            json!({
                "id": "routeMap",
                "title": "Route Map",
                "items": adventure.navigation_routes.iter().map(|route| adventure_route_item(adventure, route)).collect::<Vec<_>>(),
            }),
            json!({
                "id": "authorizationBoundary",
                "title": "Authorization Boundary",
                "items": adventure.authorization_flags.iter().map(authorization_flag_item).collect::<Vec<_>>(),
            }),
            json!({
                "id": "rawSparql",
                "title": "Raw SPARQL",
                "items": adventure_raw_sparql_items(snapshot, adventure),
            }),
        ],
        choices,
        suggested_choice_id,
    }
}

fn decision_trail_page(
    snapshot: &WorkflowBookSnapshot,
    workflow: Option<&WorkflowSummary>,
) -> WorkflowBookPage {
    let decisions = decisions_for_workflow(snapshot, workflow);
    let mut choices = Vec::new();
    for (index, decision) in decisions.iter().enumerate() {
        choices.push(navigate_choice_with_description(
            &format!("launch-decision-{}", index + 1),
            &format!("Launch {}", decision_title(decision)),
            &decision_launch_page_id(&decision.subject),
            "Open the compact scout launch page for this retained route turn.",
        ));
        choices.push(navigate_choice_with_description(
            &format!("open-decision-{}", index + 1),
            &format!("Inspect {}", decision_title(decision)),
            &decision_page_id(&decision.subject),
            "Open the full audit view for this recorded workflow-book page-turn decision.",
        ));
    }
    choices.push(perceptual_sparql_choice(
        "perceive-page-turn-decisions",
        "Perceive decision RDF",
        "Run the exact SPARQL query that backs this decision trail.",
        snapshot,
        page_turn_decisions_query(
            &snapshot.read_graph,
            workflow.map(|item| item.name.as_str()),
        ),
    ));
    if workflow.is_some() {
        choices.push(navigate_choice_with_description(
            "overview",
            "Overview",
            "overview",
            "Return to the workflow overview.",
        ));
    }
    choices.push(navigate_choice_with_description(
        "raw-sparql",
        "Raw SPARQL",
        "raw",
        "Inspect or run the exact RDF queries behind this book.",
    ));
    choices.push(navigate_choice_with_description(
        "catalog",
        "Workflow catalog",
        "catalog",
        "Return to the graph's workflow catalog.",
    ));
    let warnings = if decisions.is_empty() {
        vec![
            "No wfui:PageTurnDecision objects are visible for this scope. Load a retained decision Turtle artifact first."
                .to_string(),
        ]
    } else {
        Vec::new()
    };
    let scope = workflow
        .map(|workflow| format!("workflow '{}'", workflow.name))
        .unwrap_or_else(|| format!("graph '{}'", snapshot.graph_id));

    WorkflowBookPage {
        id: "decision-trail".to_string(),
        kind: "decisionTrail".to_string(),
        title: "Decision Trail".to_string(),
        scene: format!(
            "You are reading {} retained workflow-book page-turn decision(s) for {scope}.",
            decisions.len()
        ),
        summary: Some(
            "This page is the book-native shelf for PageTurnDecision objects: why a page turn was recommended, what evidence supported it, and which choices required explicit authorization."
                .to_string(),
        ),
        objects: decisions
            .iter()
            .map(|decision| decision_object(decision))
            .collect(),
        warnings,
        facts: vec![
            json!({"label": "graphId", "value": snapshot.graph_id}),
            json!({"label": "readGraph", "value": snapshot.read_graph}),
            json!({"label": "workflowName", "value": workflow.map(|item| item.name.as_str())}),
            json!({"label": "decisionCount", "value": decisions.len()}),
        ],
        sections: vec![json!({
            "id": "pageTurnDecisions",
            "title": "PageTurnDecisions",
            "items": decisions.iter().enumerate().map(|(index, decision)| {
                json!({
                    "index": index + 1,
                    "subject": decision.subject,
                    "generatedAt": decision.generated_at,
                    "workflowName": decision.workflow_name,
                    "fromPage": decision.from_page,
                    "intent": decision.intent,
                    "readiness": decision.readiness,
                    "nativeSuggestedChoice": decision.native_suggested_choice,
                    "recommendedRoute": decision.recommended_route,
                    "recommendedRouteLabel": decision.recommended_route_label,
                    "recommendedChoice": decision.recommended_choice,
                    "followedRoute": decision.followed_route,
                    "followedRouteLabel": decision.followed_route_label,
                    "followedChoice": decision.followed_choice,
                    "followedChoiceLabel": decision.followed_choice_label,
                    "executionAuthorized": decision.execution_authorized,
                    "authorizationFlagCount": decision.authorization_flag_count,
                    "pageId": decision_page_id(&decision.subject),
                    "launchPageId": decision_launch_page_id(&decision.subject),
                    "choiceId": format!("open-decision-{}", index + 1),
                    "launchChoiceId": format!("launch-decision-{}", index + 1),
                })
            }).collect::<Vec<_>>(),
        })],
        choices,
        suggested_choice_id: if decisions.is_empty() {
            Some("perceive-page-turn-decisions".to_string())
        } else {
            Some("launch-decision-1".to_string())
        },
    }
}

fn decision_launch_page(
    snapshot: &WorkflowBookSnapshot,
    workflow: Option<&WorkflowSummary>,
    decision: &WorkflowPageTurnDecision,
) -> WorkflowBookPage {
    let primary_route = decision_primary_route(decision);
    let mut choices = Vec::new();
    for route in &decision.navigation_routes {
        choices.push(retained_route_choice(snapshot, workflow, decision, route));
    }
    choices.push(navigate_choice_with_description(
        "decision-detail",
        "Decision detail",
        &decision_page_id(&decision.subject),
        "Inspect the full retained PageTurnDecision evidence, facts, and raw SPARQL.",
    ));
    if let Some(from_page) = decision.from_page.as_deref() {
        choices.push(navigate_choice_with_description(
            "origin-page",
            "Origin page",
            from_page,
            "Return to the workflow-book page where this decision was made.",
        ));
    }
    choices.push(perceptual_sparql_choice(
        "perceive-this-decision",
        "Perceive this decision RDF",
        "Run the exact SPARQL query for this PageTurnDecision.",
        snapshot,
        page_turn_decision_detail_query(&snapshot.read_graph, &decision.subject),
    ));
    choices.push(navigate_choice_with_description(
        "decision-trail",
        "Decision trail",
        "decision-trail",
        "Return to the retained decision trail.",
    ));
    if workflow.is_some() {
        choices.push(navigate_choice_with_description(
            "overview",
            "Overview",
            "overview",
            "Return to the workflow overview.",
        ));
    }
    choices.push(navigate_choice_with_description(
        "raw-sparql",
        "Raw SPARQL",
        "raw",
        "Inspect or run the exact RDF queries behind this book.",
    ));

    let suggested_choice_id = primary_route
        .and_then(|route| route.route_id.as_deref())
        .map(|route_id| format!("route-{}", choice_suffix(route_id)))
        .filter(|choice_id| choices.iter().any(|choice| choice.id == *choice_id))
        .or_else(|| Some("decision-detail".to_string()));
    let branch_label = primary_route
        .and_then(|route| route.route_id.as_deref().or(route.route_label.as_deref()))
        .unwrap_or("no retained route");
    let branch_choice = primary_route
        .and_then(|route| route.choice_id.as_deref())
        .unwrap_or("no retained choice");

    WorkflowBookPage {
        id: decision_launch_page_id(&decision.subject),
        kind: "decisionLaunch".to_string(),
        title: format!("Launch {}", decision_title(decision)),
        scene: format!(
            "You are at the compact launch page for a retained PageTurnDecision. Current branch: '{}' -> '{}'.",
            branch_label, branch_choice
        ),
        summary: Some(
            "Use the route map to continue through the workflow book, or open the audit detail/raw SPARQL surface before turning."
                .to_string(),
        ),
        objects: decision_launch_objects(decision, primary_route),
        warnings: decision_warnings(decision),
        facts: vec![
            json!({"label": "subject", "value": decision.subject}),
            json!({"label": "graphId", "value": decision.graph_id}),
            json!({"label": "workflowName", "value": decision.workflow_name}),
            json!({"label": "fromPage", "value": decision.from_page}),
            json!({"label": "intent", "value": decision.intent}),
            json!({"label": "readiness", "value": decision.readiness}),
            json!({"label": "primaryRoute", "value": primary_route.and_then(|route| route.route_id.as_deref())}),
            json!({"label": "primaryChoice", "value": primary_route.and_then(|route| route.choice_id.as_deref())}),
            json!({"label": "executionAuthorized", "value": decision.execution_authorized}),
            json!({"label": "authorizationFlagCount", "value": decision.authorization_flag_count}),
        ],
        sections: vec![
            json!({
                "id": "currentBranch",
                "title": "Current Branch",
                "items": primary_route
                    .map(|route| vec![decision_launch_route_item(decision, route)])
                    .unwrap_or_default(),
            }),
            json!({
                "id": "routeMap",
                "title": "Route Map",
                "items": decision.navigation_routes.iter().map(|route| decision_launch_route_item(decision, route)).collect::<Vec<_>>(),
            }),
            json!({
                "id": "evidence",
                "title": "Evidence",
                "items": decision.evidence.iter().map(evidence_item).collect::<Vec<_>>(),
            }),
            json!({
                "id": "authorizationBoundary",
                "title": "Authorization Boundary",
                "items": decision.authorization_flags.iter().map(authorization_flag_item).collect::<Vec<_>>(),
            }),
            json!({
                "id": "rawSparql",
                "title": "Raw SPARQL",
                "items": [{
                    "title": "This PageTurnDecision",
                    "tool": "sparql_query",
                    "arguments": {
                        "graphId": snapshot.graph_id,
                        "query": page_turn_decision_detail_query(&snapshot.read_graph, &decision.subject),
                    },
                }],
            }),
        ],
        choices,
        suggested_choice_id,
    }
}

fn decision_detail_page(
    snapshot: &WorkflowBookSnapshot,
    workflow: Option<&WorkflowSummary>,
    decision: &WorkflowPageTurnDecision,
) -> WorkflowBookPage {
    let mut choices = Vec::new();
    for route in &decision.navigation_routes {
        choices.push(retained_route_choice(snapshot, workflow, decision, route));
    }
    if let Some(choice_id) = decision.recommended_choice.as_deref() {
        choices.push(recorded_choice_action(
            "replay-recommended-choice",
            "Replay recommended choice",
            "Call workflow_book_choose with the recorded recommended choice. This replays the page turn; it does not execute a run by itself.",
            snapshot,
            workflow,
            decision,
            choice_id,
        ));
    }
    if let Some(choice_id) = decision.native_suggested_choice.as_deref() {
        choices.push(recorded_choice_action(
            "replay-native-suggestion",
            "Replay native suggestion",
            "Call workflow_book_choose with the native Garden suggestion captured at decision time.",
            snapshot,
            workflow,
            decision,
            choice_id,
        ));
    }
    if let Some(from_page) = decision.from_page.as_deref() {
        choices.push(navigate_choice_with_description(
            "origin-page",
            "Origin page",
            from_page,
            "Return to the workflow-book page where this decision was made.",
        ));
    }
    choices.push(navigate_choice_with_description(
        "launch-page",
        "Launch page",
        &decision_launch_page_id(&decision.subject),
        "Open the compact scout launch page for this retained route turn.",
    ));
    choices.push(perceptual_sparql_choice(
        "perceive-this-decision",
        "Perceive this decision RDF",
        "Run the exact SPARQL query for this PageTurnDecision, including evidence and authorization flags.",
        snapshot,
        page_turn_decision_detail_query(&snapshot.read_graph, &decision.subject),
    ));
    choices.push(navigate_choice_with_description(
        "decision-trail",
        "Decision trail",
        "decision-trail",
        "Return to the retained decision trail.",
    ));
    if workflow.is_some() {
        choices.push(navigate_choice_with_description(
            "overview",
            "Overview",
            "overview",
            "Return to the workflow overview.",
        ));
    }
    choices.push(navigate_choice_with_description(
        "raw-sparql",
        "Raw SPARQL",
        "raw",
        "Inspect or run the exact RDF queries behind this book.",
    ));

    WorkflowBookPage {
        id: decision_page_id(&decision.subject),
        kind: "decisionDetail".to_string(),
        title: decision_title(decision),
        scene: format!(
            "You are inspecting one PageTurnDecision from '{}' to '{}'.",
            decision.from_page.as_deref().unwrap_or("unknown page"),
            decision
                .recommended_route
                .as_deref()
                .or(decision.recommended_choice.as_deref())
                .unwrap_or("no recorded recommendation")
        ),
        summary: decision.rationale.clone().or_else(|| {
            Some(
                "A retained decision object with linked evidence, authorization flags, and raw SPARQL."
                    .to_string(),
            )
        }),
        objects: decision_detail_objects(decision),
        warnings: decision_warnings(decision),
        facts: vec![
            json!({"label": "subject", "value": decision.subject}),
            json!({"label": "generatedAt", "value": decision.generated_at}),
            json!({"label": "graphId", "value": decision.graph_id}),
            json!({"label": "workflowName", "value": decision.workflow_name}),
            json!({"label": "fromPage", "value": decision.from_page}),
            json!({"label": "intent", "value": decision.intent}),
            json!({"label": "readiness", "value": decision.readiness}),
            json!({"label": "nativeSuggestedChoice", "value": decision.native_suggested_choice}),
            json!({"label": "recommendedRoute", "value": decision.recommended_route}),
            json!({"label": "recommendedRouteLabel", "value": decision.recommended_route_label}),
            json!({"label": "recommendedChoice", "value": decision.recommended_choice}),
            json!({"label": "followedRoute", "value": decision.followed_route}),
            json!({"label": "followedRouteLabel", "value": decision.followed_route_label}),
            json!({"label": "followedChoice", "value": decision.followed_choice}),
            json!({"label": "followedChoiceLabel", "value": decision.followed_choice_label}),
            json!({"label": "executionAuthorized", "value": decision.execution_authorized}),
            json!({"label": "authorizationFlagCount", "value": decision.authorization_flag_count}),
        ],
        sections: vec![
            json!({
                "id": "evidence",
                "title": "Evidence",
                "items": decision.evidence.iter().map(evidence_item).collect::<Vec<_>>(),
            }),
            json!({
                "id": "navigationRoutes",
                "title": "Navigation Routes",
                "items": decision.navigation_routes.iter().map(navigation_route_item).collect::<Vec<_>>(),
            }),
            json!({
                "id": "authorizationFlags",
                "title": "Authorization Flags",
                "items": decision.authorization_flags.iter().map(authorization_flag_item).collect::<Vec<_>>(),
            }),
            json!({
                "id": "rawSparql",
                "title": "Raw SPARQL",
                "items": [{
                    "title": "This PageTurnDecision",
                    "tool": "sparql_query",
                    "arguments": {
                        "graphId": snapshot.graph_id,
                        "query": page_turn_decision_detail_query(&snapshot.read_graph, &decision.subject),
                    },
                }],
            }),
        ],
        choices,
        suggested_choice_id: Some("decision-trail".to_string()),
    }
}

fn phase_page(
    _snapshot: &WorkflowBookSnapshot,
    workflow: &WorkflowSummary,
    phase: &WorkflowPhase,
) -> WorkflowBookPage {
    let phase_nodes = workflow
        .nodes
        .iter()
        .filter(|node| node.phase_index == phase.order)
        .collect::<Vec<_>>();
    let mut choices = phase_nodes
        .iter()
        .map(|node| {
            navigate_choice(
                &format!("inspect-agent-{}", choice_suffix(&node.label)),
                &format!("Inspect {}", node.label),
                &agent_page_id(&node.label),
            )
        })
        .collect::<Vec<_>>();
    if let Some(next) = workflow
        .phases
        .iter()
        .find(|candidate| candidate.order > phase.order)
    {
        choices.push(navigate_choice_with_description(
            "next-phase",
            "Next phase",
            &phase_page_id(next.order),
            "Continue to the next phase in order.",
        ));
    }
    if let Some(prev) = workflow
        .phases
        .iter()
        .rev()
        .find(|candidate| candidate.order < phase.order)
    {
        choices.push(navigate_choice_with_description(
            "previous-phase",
            "Previous phase",
            &phase_page_id(prev.order),
            "Return to the previous phase in order.",
        ));
    }
    choices.push(navigate_choice_with_description(
        "overview",
        "Overview",
        "overview",
        "Return to the workflow overview.",
    ));
    choices.push(navigate_choice_with_description(
        "raw-sparql",
        "Raw SPARQL",
        "raw",
        "Inspect or run the exact RDF queries behind this book.",
    ));
    let suggested_choice_id = choices.first().map(|choice| choice.id.clone());
    let warnings = if phase_nodes.is_empty() {
        vec!["No agent nodes are attached to this phase.".to_string()]
    } else {
        Vec::new()
    };

    WorkflowBookPage {
        id: phase_page_id(phase.order),
        kind: "phase".to_string(),
        title: format!("Phase {}: {}", phase.order, phase.title),
        scene: format!(
            "You are inside phase {} of '{}'; {} agent node(s) are visible here.",
            phase.order,
            workflow.name,
            phase_nodes.len()
        ),
        summary: phase.description.clone(),
        objects: phase_objects(phase, &phase_nodes),
        warnings,
        facts: vec![
            json!({"label": "phaseUri", "value": phase.uri}),
            json!({"label": "order", "value": phase.order}),
            json!({"label": "agentCount", "value": phase_nodes.len()}),
        ],
        sections: vec![json!({
            "id": "agents",
            "title": "Agents",
            "items": phase_nodes.iter().map(|node| agent_item(node)).collect::<Vec<_>>(),
        })],
        choices,
        suggested_choice_id,
    }
}

fn agent_page(
    snapshot: &WorkflowBookSnapshot,
    workflow: &WorkflowSummary,
    node: &WorkflowAgentNode,
) -> WorkflowBookPage {
    let mut choices = Vec::new();
    if let Some(doc_id) = &node.doc_id {
        choices.push(WorkflowBookChoice {
            id: "read-node-document".to_string(),
            kind: "mcp-tool".to_string(),
            label: "Read node document".to_string(),
            description: Some(
                "Open the Garden document block/source for this agent node.".to_string(),
            ),
            target_page_id: None,
            action: Some(json!({
                "tool": "read_document",
                "arguments": {
                    "graphId": snapshot.graph_id,
                    "documentId": doc_id,
                },
            })),
        });
    }
    choices.push(navigate_choice_with_description(
        "back-to-phase",
        "Back to phase",
        &phase_page_id(node.phase_index),
        "Return to this agent's phase page.",
    ));
    choices.push(navigate_choice_with_description(
        "overview",
        "Overview",
        "overview",
        "Return to the workflow overview.",
    ));
    choices.push(navigate_choice_with_description(
        "raw-sparql",
        "Raw SPARQL",
        "raw",
        "Inspect or run the exact RDF queries behind this book.",
    ));
    let mut warnings = Vec::new();
    if node.doc_id.is_none() {
        warnings.push("This agent node does not resolve to a Garden document id.".to_string());
    }
    if node.agent_type.is_none() {
        warnings.push("This agent node does not declare wf:agentType.".to_string());
    }
    let suggested_choice_id = choices.first().map(|choice| choice.id.clone());

    WorkflowBookPage {
        id: agent_page_id(&node.label),
        kind: "agent".to_string(),
        title: format!("{} / {}", workflow.name, node.label),
        scene: format!(
            "You are inspecting agent node '{}' in phase {} of '{}'.",
            node.label, node.phase_index, workflow.name
        ),
        summary: Some("Agent prompt/source text lives in the node document; RDF carries identity and workflow anatomy.".to_string()),
        objects: agent_objects(node),
        warnings,
        facts: vec![
            json!({"label": "agentNodeUri", "value": node.uri}),
            json!({"label": "label", "value": node.label}),
            json!({"label": "phaseIndex", "value": node.phase_index}),
            json!({"label": "agentType", "value": node.agent_type}),
            json!({"label": "documentId", "value": node.doc_id}),
        ],
        sections: Vec::new(),
        choices,
        suggested_choice_id,
    }
}

fn validation_page(
    snapshot: &WorkflowBookSnapshot,
    workflow: &WorkflowSummary,
) -> WorkflowBookPage {
    let report = validate_workflow(workflow);
    let suggested_choice_id = Some(if report.passed {
        "execute-async".to_string()
    } else {
        "run-missing-required-query".to_string()
    });
    WorkflowBookPage {
        id: "validation".to_string(),
        kind: "validation".to_string(),
        title: "Validation".to_string(),
        scene: if report.passed {
            format!(
                "Local validation passes for '{}'; execution can be prepared next.",
                workflow.name
            )
        } else {
            format!(
                "Local validation found {} error(s) and {} warning(s) in '{}'.",
                report.errors, report.warnings, workflow.name
            )
        },
        summary: Some(validation_summary_sentence(&report)),
        objects: validation_objects(&report),
        warnings: report_warning_messages(&report),
        facts: vec![
            json!({"label": "workflowUri", "value": workflow.uri}),
            json!({"label": "readGraph", "value": snapshot.read_graph}),
            json!({"label": "passed", "value": report.passed}),
            json!({"label": "errors", "value": report.errors}),
            json!({"label": "warnings", "value": report.warnings}),
        ],
        sections: vec![
            json!({
                "id": "validationReport",
                "title": "Validation report",
                "items": report.issues,
            }),
            json!({
                "id": "validationQueries",
                "title": "Validation queries",
                "items": validation_queries(snapshot, workflow),
            }),
        ],
        choices: vec![
            WorkflowBookChoice {
                id: "validate-now".to_string(),
                kind: "mcp-tool".to_string(),
                label: "Validate now".to_string(),
                description: Some(
                    "Run the workflow_book_validate MCP tool for a fresh report.".to_string(),
                ),
                target_page_id: None,
                action: Some(json!({
                    "tool": "workflow_book_validate",
                    "arguments": {
                        "graphId": snapshot.graph_id,
                        "workflowName": workflow.name,
                    },
                })),
            },
            WorkflowBookChoice {
                id: "run-missing-required-query".to_string(),
                kind: "raw-sparql".to_string(),
                label: "Run missing-required query".to_string(),
                description: Some(
                    "Use raw SPARQL to inspect missing required workflow triples.".to_string(),
                ),
                target_page_id: None,
                action: Some(json!({
                    "tool": "sparql_query",
                    "arguments": {
                        "graphId": snapshot.graph_id,
                        "query": missing_required_query(&snapshot.read_graph, &workflow.uri),
                    },
                })),
            },
            navigate_choice_with_description(
                "execute-async",
                "Prepare async run",
                "execute",
                "Open explicit Choreograph run actions for this workflow.",
            ),
            navigate_choice_with_description(
                "overview",
                "Overview",
                "overview",
                "Return to the workflow overview.",
            ),
            navigate_choice_with_description(
                "raw-sparql",
                "Raw SPARQL",
                "raw",
                "Inspect or run the exact RDF queries behind this book.",
            ),
        ],
        suggested_choice_id,
    }
}

fn execute_page(snapshot: &WorkflowBookSnapshot, workflow: &WorkflowSummary) -> WorkflowBookPage {
    let run_body = json!({
        "graph_id": snapshot.graph_id,
        "workflow_name": workflow.name,
        "workflow_args": {},
    });
    let source_block = workflow
        .script_block
        .as_deref()
        .and_then(doc_block_from_uri);
    let input_block = workflow.input_block.as_deref().and_then(doc_block_from_uri);
    let mut start_args = json!({
        "graphId": snapshot.graph_id,
        "workflowName": workflow.name,
        "workflowArgs": {},
        "autoStart": true,
    });
    if let Some(input_block) = &input_block {
        start_args["workflowArgsBlock"] = json!({
            "documentId": input_block.document_id,
            "blockId": input_block.block_id,
        });
    }
    let mut choices = vec![
        WorkflowBookChoice {
            id: "start-run-mcp".to_string(),
            kind: "mcp-tool".to_string(),
            label: "Start async run".to_string(),
            description: Some(
                "Call workflow_run_start through the Garden MCP surface; this may auto-start the Choreograph sidecar."
                    .to_string(),
            ),
            target_page_id: None,
            action: Some(json!({
                "tool": "workflow_run_start",
                "arguments": start_args.clone(),
            })),
        },
        WorkflowBookChoice {
            id: "submit-run".to_string(),
            kind: "http-request".to_string(),
            label: "Submit async run via facade".to_string(),
            description: Some(
                "Submit the equivalent HTTP request through Garden's proxied service facade.".to_string(),
            ),
            target_page_id: None,
            action: Some(json!({
                "facade": {"method": "POST", "path": "/workflows/runs"},
                "upstream": {"serviceId": "choreograph", "method": "POST", "path": "/api/workflows/run"},
                "requiredScope": "services.proxy",
                "body": run_body,
                "monitor": {
                    "detailPath": "/workflows/runs/{runId}",
                    "eventsPath": "/workflows/runs/{runId}/events"
                }
            })),
        },
    ];
    if let Some(input_block) = &input_block {
        choices.push(WorkflowBookChoice {
            id: "read-workflow-input".to_string(),
            kind: "mcp-tool".to_string(),
            label: "Read workflow input block".to_string(),
            description: Some(
                "Read the Garden block that contains JSON workflow arguments.".to_string(),
            ),
            target_page_id: None,
            action: Some(json!({
                "tool": "get_block",
                "arguments": {
                    "graphId": snapshot.graph_id,
                    "documentId": input_block.document_id,
                    "blockId": input_block.block_id,
                    "format": "text",
                },
            })),
        });
    }
    if let Some(source_block) = &source_block {
        choices.push(WorkflowBookChoice {
            id: "read-workflow-source".to_string(),
            kind: "mcp-tool".to_string(),
            label: "Read workflow source block".to_string(),
            description: Some(
                "Read the Garden block that contains the workflow source bytes.".to_string(),
            ),
            target_page_id: None,
            action: Some(json!({
                "tool": "get_block",
                "arguments": {
                    "graphId": snapshot.graph_id,
                    "documentId": source_block.document_id,
                    "blockId": source_block.block_id,
                    "format": "text",
                },
            })),
        });
        choices.push(WorkflowBookChoice {
            id: "start-run-with-source-block".to_string(),
            kind: "mcp-tool".to_string(),
            label: "Start using source block".to_string(),
            description: Some(
                "Dereference the workflow source block, verify the optional sha, and start the Choreograph run."
                    .to_string(),
            ),
            target_page_id: None,
            action: {
                let mut args = start_args.clone();
                args["workflowSourceBlock"] = json!({
                    "documentId": source_block.document_id,
                    "blockId": source_block.block_id,
                });
                args["scriptSha256"] = json!(workflow.script_sha256);
                Some(json!({
                    "tool": "workflow_run_start",
                    "arguments": args,
                }))
            },
        });
    }
    if let Some(run) = workflow.runs.first() {
        choices.push(WorkflowBookChoice {
            id: "monitor-latest-run".to_string(),
            kind: "mcp-tool".to_string(),
            label: "Monitor latest run".to_string(),
            description: Some(
                "Poll the latest known run detail and journal events through Garden MCP."
                    .to_string(),
            ),
            target_page_id: None,
            action: Some(json!({
                "tool": "workflow_run_monitor",
                "arguments": {
                    "graphId": snapshot.graph_id,
                    "workflowName": workflow.name,
                    "runId": run.run_id,
                    "since": 0,
                    "includeEvents": true,
                },
            })),
        });
    }
    choices.push(navigate_choice_with_description(
        "validate-graph",
        "Validate graph anatomy",
        "validation",
        "Return to validation before executing.",
    ));
    choices.push(navigate_choice_with_description(
        "overview",
        "Overview",
        "overview",
        "Return to the workflow overview.",
    ));
    choices.push(navigate_choice_with_description(
        "raw-sparql",
        "Raw SPARQL",
        "raw",
        "Inspect or run the exact RDF queries behind this book.",
    ));
    let report = validate_workflow(workflow);
    let mut warnings = report_warning_messages(&report);
    if workflow.script_block.is_none() {
        warnings.push(
            "No wf:scriptBlock is available; dynamic source execution will need a registered workflow or explicit binding."
                .to_string(),
        );
    } else {
        warnings.push(
            "Source-block execution depends on a trusted Choreograph sidecar with dynamic source enabled."
                .to_string(),
        );
    }
    if workflow.input_block.is_none() {
        warnings.push(
            "No wf:inputBlock is bound; large workflow inputs still need explicit workflowArgs or a later input binding."
                .to_string(),
        );
    }
    let suggested_choice_id = if source_block.is_some() && report.passed {
        Some("start-run-with-source-block".to_string())
    } else if report.passed {
        Some("start-run-mcp".to_string())
    } else {
        Some("validate-graph".to_string())
    };

    WorkflowBookPage {
        id: "execute".to_string(),
        kind: "execute".to_string(),
        title: "Execute".to_string(),
        scene: "You are at the explicit execution doorway: choose an MCP or HTTP action, then monitor the run journal. This page does not execute by itself.".to_string(),
        summary: Some(
            "This page prepares explicit Choreograph run actions; choosing an MCP action still requires the agent to call that tool."
                .to_string(),
        ),
        objects: execute_objects(
            workflow,
            source_block.as_ref(),
            input_block.as_ref(),
            &run_body,
        ),
        warnings,
        facts: vec![
            json!({"label": "workflowName", "value": workflow.name}),
            json!({"label": "graphId", "value": snapshot.graph_id}),
            json!({"label": "requiresLoopbackScope", "value": "services.proxy"}),
            json!({"label": "sourceBlock", "value": workflow.script_block}),
            json!({"label": "inputBlock", "value": workflow.input_block}),
        ],
        sections: vec![json!({
            "id": "request",
            "title": "Run request",
            "items": [{
                "facade": {"method": "POST", "path": "/workflows/runs"},
                "upstream": {"serviceId": "choreograph", "method": "POST", "path": "/api/workflows/run"},
                "body": run_body,
                "monitor": {
                    "detailPath": "/workflows/runs/{runId}",
                    "eventsPath": "/workflows/runs/{runId}/events"
                }
            }],
        })],
        choices,
        suggested_choice_id,
    }
}

fn raw_page(
    snapshot: &WorkflowBookSnapshot,
    workflow: Option<&WorkflowSummary>,
) -> WorkflowBookPage {
    let mut choices = raw_query_choices(snapshot, workflow);
    choices.push(navigate_choice_with_description(
        "catalog",
        "Workflow catalog",
        "catalog",
        "Return to the graph's workflow catalog.",
    ));
    if workflow.is_some() {
        choices.insert(
            0,
            navigate_choice_with_description(
                "overview",
                "Overview",
                "overview",
                "Return to the workflow overview.",
            ),
        );
        choices.insert(
            1,
            navigate_choice_with_description(
                "adventure-trail",
                "Adventure trail",
                "adventure-trail",
                "Inspect retained source-side adventure packets for this workflow.",
            ),
        );
        choices.insert(
            2,
            navigate_choice_with_description(
                "decision-trail",
                "Decision trail",
                "decision-trail",
                "Inspect retained page-turn decisions for this workflow.",
            ),
        );
    } else {
        choices.insert(
            0,
            navigate_choice_with_description(
                "adventure-trail",
                "Adventure trail",
                "adventure-trail",
                "Inspect retained Workflow PageView objects in this graph.",
            ),
        );
        choices.insert(
            1,
            navigate_choice_with_description(
                "decision-trail",
                "Decision trail",
                "decision-trail",
                "Inspect retained workflow-book PageTurnDecision objects in this graph.",
            ),
        );
    }
    let suggested_choice_id = choices.first().map(|choice| choice.id.clone());
    WorkflowBookPage {
        id: "raw".to_string(),
        kind: "raw".to_string(),
        title: "Raw SPARQL".to_string(),
        scene: "You are at the graph layer. These are exact SPARQL queries that back or inspect the workflow book.".to_string(),
        summary: Some(
            "The book is a view over these RDF queries, not a replacement for them.".to_string(),
        ),
        objects: raw_objects(snapshot, workflow),
        warnings: Vec::new(),
        facts: vec![
            json!({"label": "graphId", "value": snapshot.graph_id}),
            json!({"label": "readGraph", "value": snapshot.read_graph}),
        ],
        sections: vec![json!({
            "id": "queries",
            "title": "Queries",
            "items": raw_queries(snapshot, workflow),
        })],
        choices,
        suggested_choice_id,
    }
}

fn choose_from_book(
    snapshot: &WorkflowBookSnapshot,
    book: WorkflowBook,
    page_id: &str,
    choice_id: &str,
    intent: Option<&str>,
) -> AppResult<Value> {
    let page = book
        .pages
        .iter()
        .find(|candidate| candidate.id == page_id)
        .ok_or_else(|| AppError::validation(format!("unknown workflow-book page: {page_id}")))?;
    let choice = page
        .choices
        .iter()
        .find(|candidate| candidate.id == choice_id)
        .cloned()
        .ok_or_else(|| {
            AppError::validation(format!(
                "unknown workflow-book choice '{choice_id}' on page '{page_id}'"
            ))
        })?;

    if choice.kind == "open-workflow" {
        let workflow_name = choice
            .action
            .as_ref()
            .and_then(|action| action.get("workflowName"))
            .and_then(Value::as_str)
            .ok_or_else(|| AppError::validation("open-workflow choice missing workflowName"))?;
        let next_book = render_workflow_book(snapshot, Some(workflow_name))?;
        let reports = validation_reports(snapshot, Some(workflow_name))?;
        return book_response(
            snapshot,
            next_book,
            "overview",
            Some(choice),
            &reports,
            intent,
        );
    }

    let next_page_id = choice
        .target_page_id
        .clone()
        .as_deref()
        .filter(|target| !target.trim().is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| page_id.to_string());
    let reports = validation_reports(snapshot, book.workflow_name.as_deref())?;
    book_response(
        snapshot,
        book,
        &next_page_id,
        Some(choice),
        &reports,
        intent,
    )
}

fn book_response(
    snapshot: &WorkflowBookSnapshot,
    book: WorkflowBook,
    current_page_id: &str,
    selected_choice: Option<WorkflowBookChoice>,
    reports: &[WorkflowValidationReport],
    intent: Option<&str>,
) -> AppResult<Value> {
    let page = book
        .pages
        .iter()
        .find(|candidate| candidate.id == current_page_id)
        .cloned()
        .ok_or_else(|| {
            AppError::validation(format!("unknown workflow-book page: {current_page_id}"))
        })?;
    let mut page_value = serde_json::to_value(&page).map_err(|error| {
        AppError::serialization(format!("serialize workflow-book page: {error}"))
    })?;
    let routes = navigation_routes(snapshot, &book, current_page_id, &page);
    if !routes.is_empty() {
        page_value["navigationRoutes"] = serde_json::to_value(routes).map_err(|error| {
            AppError::serialization(format!(
                "serialize workflow-book navigation routes: {error}"
            ))
        })?;
    }
    if let Some(recommendation) = intent_recommendation(intent, &page) {
        page_value["intentRecommendation"] =
            serde_json::to_value(recommendation).map_err(|error| {
                AppError::serialization(format!(
                    "serialize workflow-book intent recommendation: {error}"
                ))
            })?;
    }
    let mut response = json!({
        "book": book,
        "currentPageId": current_page_id,
        "page": page_value,
    });
    response["authoring"] = authoring_state(snapshot, &response["page"], reports);
    if let Some(choice) = selected_choice {
        response["selectedChoice"] = serde_json::to_value(choice).map_err(|error| {
            AppError::serialization(format!("serialize workflow-book choice: {error}"))
        })?;
    }
    Ok(response)
}

fn workflow_book_intent(arguments: &Value) -> Option<String> {
    mcp_arg_string(
        arguments,
        &[
            "handoffIntent",
            "handoff_intent",
            "workflowIntent",
            "workflow_intent",
            "intent",
        ],
    )
    .and_then(|value| normalize_workflow_book_intent(&value))
}

fn normalize_workflow_book_intent(value: &str) -> Option<String> {
    let normalized = value.trim().to_ascii_lowercase().replace('_', "-");
    match normalized.as_str() {
        "" | "suggested" | "default" | "none" => None,
        "analysis" | "analysis-scout" | "scout" | "perception" | "perceptual" | "conceptual" => {
            Some("analysis-scout".to_string())
        }
        "run" | "run-prep" | "execute" | "execution" | "prepare" | "prepare-execute"
        | "prepare-execution" => Some("run-prep".to_string()),
        _ => Some(normalized),
    }
}

fn intent_recommendation(
    intent: Option<&str>,
    page: &WorkflowBookPage,
) -> Option<WorkflowBookIntentRecommendation> {
    let intent = normalize_workflow_book_intent(intent?)?;
    if let Some(recommendation) = retained_page_view_intent_recommendation(&intent, page) {
        return Some(recommendation);
    }
    let (choice, rationale) = match intent.as_str() {
        "analysis-scout" => (
            first_matching_choice(
                &page.choices,
                &[
                    "perception-map",
                    "concept-phase-1",
                    "enter-phases",
                    "adventure-trail",
                    "decision-trail",
                    "validate-graph",
                    "raw-sparql",
                    "raw",
                ],
                &["conceptual-route", "perceptual-sparql", "raw-sparql"],
            )?,
            "Intent is analysis-scout, so inspect perception, conceptual routes, validation, or raw SPARQL before any run-preparation path.",
        ),
        "run-prep" => (
            first_matching_choice(
                &page.choices,
                &[
                    "execute-async",
                    "start-run-with-source-block",
                    "start-run-mcp",
                    "submit-run",
                    "validate-graph",
                    "prepare-execute",
                    "validate-now",
                ],
                &["execute", "mcp-tool", "http-request"],
            )?,
            "Intent is run-prep, so move toward explicit run preparation while preserving validation and authorization boundaries.",
        ),
        _ => return None,
    };
    Some(WorkflowBookIntentRecommendation {
        intent,
        choice_id: choice.id.clone(),
        choice_label: choice.label.clone(),
        source: "garden-intent-rule".to_string(),
        rationale: rationale.to_string(),
        semantic_class: "wf:LiveRecommendedAction".to_string(),
        store_mode: "virtual".to_string(),
        identity_kind: "resolve-by-query".to_string(),
        derived_from_query: Some(
            "workflow_book_open.intentRecommendation(current page choices, navigation routes, retained PageView recommendation rows)"
                .to_string(),
        ),
    })
}

fn retained_page_view_intent_recommendation(
    intent: &str,
    page: &WorkflowBookPage,
) -> Option<WorkflowBookIntentRecommendation> {
    if page.kind != "adventureDetail" {
        return None;
    }
    let page_view = page.objects.iter().find(|object| {
        object.get("kind").and_then(Value::as_str) == Some("workflowPageView")
            || object.get("compatKind").and_then(Value::as_str) == Some("workflowAdventurePacket")
    })?;
    let page_view_intent = page_view
        .get("intent")
        .and_then(Value::as_str)
        .and_then(normalize_workflow_book_intent)?;
    if page_view_intent != intent {
        return None;
    }
    let recommended_route = page_view.get("recommendedRoute").and_then(Value::as_str);
    let recommended_choice = page_view.get("recommendedChoice").and_then(Value::as_str);
    let choice = page.choices.iter().find(|choice| {
        if choice.kind != "retained-adventure-route" {
            return false;
        }
        let action = choice.action.as_ref();
        action
            .and_then(|action| action.get("routeId"))
            .and_then(Value::as_str)
            .is_some_and(|route_id| Some(route_id) == recommended_route)
            || action
                .and_then(|action| action.get("choiceId"))
                .and_then(Value::as_str)
                .is_some_and(|choice_id| Some(choice_id) == recommended_choice)
    })?;
    let route_label = page_view
        .get("recommendedRouteLabel")
        .and_then(Value::as_str)
        .unwrap_or("retained PageView route");
    Some(WorkflowBookIntentRecommendation {
        intent: intent.to_string(),
        choice_id: choice.id.clone(),
        choice_label: choice.label.clone(),
        source: "retained-page-view".to_string(),
        rationale: format!("Retained PageView recommends {route_label}."),
        semantic_class: "wf:RecommendedAction".to_string(),
        store_mode: "materialize".to_string(),
        identity_kind: "urn-template".to_string(),
        derived_from_query: None,
    })
}

fn navigation_routes(
    snapshot: &WorkflowBookSnapshot,
    book: &WorkflowBook,
    current_page_id: &str,
    page: &WorkflowBookPage,
) -> Vec<WorkflowBookNavigationRoute> {
    let mut routes = Vec::new();
    routes.extend(retained_decision_navigation_routes(page));
    routes.extend(retained_adventure_navigation_routes(page));
    if !routes.is_empty() {
        return routes;
    }
    if let Some(route) = intent_route(
        snapshot,
        book,
        current_page_id,
        page,
        "analysis-scout",
        "Analysis scout",
        "analysis-scout",
        "Start with perception, conceptual structure, validation, or raw SPARQL before any execution-preparation route.",
    ) {
        routes.push(route);
    }
    if let Some(route) = intent_route(
        snapshot,
        book,
        current_page_id,
        page,
        "run-prep",
        "Run preparation",
        "run-prep",
        "Move toward validation and explicit run preparation; execution still requires explicit authorization.",
    ) {
        routes.push(route);
    }
    if let Some(choice) = page
        .choices
        .iter()
        .find(|choice| choice.id == "adventure-trail")
    {
        routes.push(navigation_route_from_choice(
            snapshot,
            book,
            current_page_id,
            "adventure-trail",
            "Adventure trail",
            "history",
            None,
            choice,
            "Inspect retained source-side workflow adventure packets before choosing the next move.",
        ));
    }
    if let Some(choice) = page
        .choices
        .iter()
        .find(|choice| choice.id == "decision-trail")
    {
        routes.push(navigation_route_from_choice(
            snapshot,
            book,
            current_page_id,
            "decision-trail",
            "Decision trail",
            "history",
            None,
            choice,
            "Inspect retained workflow-book page-turn decisions before choosing the next move.",
        ));
    }
    if let Some(choice) =
        first_matching_choice(&page.choices, &["raw-sparql", "raw"], &["raw-sparql"])
    {
        routes.push(navigation_route_from_choice(
            snapshot,
            book,
            current_page_id,
            "raw-sparql",
            "Raw SPARQL",
            "raw-sparql",
            None,
            choice,
            "Drop to exact RDF/SPARQL inspection for this workflow-book page.",
        ));
    }
    routes
}

fn retained_decision_navigation_routes(
    page: &WorkflowBookPage,
) -> Vec<WorkflowBookNavigationRoute> {
    if page.kind != "decisionDetail" && page.kind != "decisionLaunch" {
        return Vec::new();
    }
    page.choices
        .iter()
        .filter(|choice| choice.kind == "retained-route")
        .filter_map(|choice| {
            let action = choice.action.clone()?;
            let arguments = action.get("arguments").and_then(Value::as_object);
            let route_id = arguments
                .and_then(|args| args.get("routeId"))
                .and_then(Value::as_str)
                .or_else(|| choice.id.strip_prefix("route-"))
                .unwrap_or(choice.id.as_str())
                .to_string();
            let label = choice
                .label
                .strip_prefix("Route: ")
                .unwrap_or(choice.label.as_str())
                .to_string();
            let intent = arguments
                .and_then(|args| args.get("handoffIntent"))
                .and_then(Value::as_str)
                .map(str::to_string);
            let rationale = choice
                .description
                .clone()
                .unwrap_or_else(|| format!("Replay retained workflow-book route '{route_id}'."));
            Some(WorkflowBookNavigationRoute {
                id: route_id,
                label,
                kind: "retained-route".to_string(),
                intent,
                choice_id: choice.id.clone(),
                choice_label: choice.label.clone(),
                source: "garden-retained-decision-route".to_string(),
                rationale,
                action,
            })
        })
        .collect()
}

fn retained_adventure_navigation_routes(
    page: &WorkflowBookPage,
) -> Vec<WorkflowBookNavigationRoute> {
    if page.kind != "adventureDetail" {
        return Vec::new();
    }
    page.choices
        .iter()
        .filter(|choice| choice.kind == "retained-adventure-route")
        .filter_map(|choice| {
            let action = choice.action.clone()?;
            let route_id = action
                .get("routeId")
                .and_then(Value::as_str)
                .or_else(|| choice.id.strip_prefix("route-"))
                .unwrap_or(choice.id.as_str())
                .to_string();
            let label = choice
                .label
                .strip_prefix("Route: ")
                .unwrap_or(choice.label.as_str())
                .to_string();
            let intent = action
                .get("intent")
                .and_then(Value::as_str)
                .map(str::to_string);
            let rationale = choice
                .description
                .clone()
                .unwrap_or_else(|| format!("Inspect retained adventure route '{route_id}'."));
            Some(WorkflowBookNavigationRoute {
                id: route_id,
                label,
                kind: "retained-adventure-route".to_string(),
                intent,
                choice_id: choice.id.clone(),
                choice_label: choice.label.clone(),
                source: "garden-retained-adventure-route".to_string(),
                rationale,
                action,
            })
        })
        .collect()
}

fn intent_route(
    snapshot: &WorkflowBookSnapshot,
    book: &WorkflowBook,
    current_page_id: &str,
    page: &WorkflowBookPage,
    id: &str,
    label: &str,
    intent: &str,
    rationale: &str,
) -> Option<WorkflowBookNavigationRoute> {
    let recommendation = intent_recommendation(Some(intent), page)?;
    let choice = page
        .choices
        .iter()
        .find(|choice| choice.id == recommendation.choice_id)?;
    Some(navigation_route_from_choice(
        snapshot,
        book,
        current_page_id,
        id,
        label,
        "intent",
        Some(recommendation.intent.as_str()),
        choice,
        rationale,
    ))
}

fn navigation_route_from_choice(
    snapshot: &WorkflowBookSnapshot,
    book: &WorkflowBook,
    current_page_id: &str,
    id: &str,
    label: &str,
    kind: &str,
    intent: Option<&str>,
    choice: &WorkflowBookChoice,
    rationale: &str,
) -> WorkflowBookNavigationRoute {
    WorkflowBookNavigationRoute {
        id: id.to_string(),
        label: label.to_string(),
        kind: kind.to_string(),
        intent: intent.map(str::to_string),
        choice_id: choice.id.clone(),
        choice_label: choice.label.clone(),
        source: "garden-route-rule".to_string(),
        rationale: rationale.to_string(),
        action: navigation_route_action(snapshot, book, current_page_id, choice, intent),
    }
}

fn navigation_route_action(
    snapshot: &WorkflowBookSnapshot,
    book: &WorkflowBook,
    current_page_id: &str,
    choice: &WorkflowBookChoice,
    intent: Option<&str>,
) -> Value {
    let mut arguments = json!({
        "graphId": snapshot.graph_id,
        "pageId": current_page_id,
        "choiceId": choice.id,
    });
    if let Some(workflow_name) = book.workflow_name.as_deref() {
        arguments["workflowName"] = json!(workflow_name);
    }
    if let Some(intent) = intent {
        arguments["handoffIntent"] = json!(intent);
    }
    json!({
        "tool": "workflow_book_choose",
        "arguments": arguments,
    })
}

fn first_matching_choice<'a>(
    choices: &'a [WorkflowBookChoice],
    ids: &[&str],
    kinds: &[&str],
) -> Option<&'a WorkflowBookChoice> {
    ids.iter()
        .find_map(|id| choices.iter().find(|choice| choice.id == *id))
        .or_else(|| {
            kinds
                .iter()
                .find_map(|kind| choices.iter().find(|choice| choice.kind == *kind))
        })
}

fn authoring_state(
    snapshot: &WorkflowBookSnapshot,
    page: &Value,
    reports: &[WorkflowValidationReport],
) -> Value {
    let error_count = reports.iter().map(|report| report.errors).sum::<usize>();
    let warning_count = reports.iter().map(|report| report.warnings).sum::<usize>();
    let info_count = reports.iter().map(|report| report.infos).sum::<usize>();
    let workflow_name = if reports.len() == 1 {
        Some(reports[0].workflow_name.as_str())
    } else {
        None
    };
    let current_page_id = page.get("id").and_then(Value::as_str).unwrap_or("unknown");
    let current_page_kind = page
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let suggested_choice_id = page.get("suggestedChoiceId").and_then(Value::as_str);
    let suggested_choice = suggested_choice_id
        .and_then(|choice_id| {
            page.get("choices")
                .and_then(Value::as_array)
                .and_then(|choices| {
                    choices
                        .iter()
                        .find(|choice| choice.get("id").and_then(Value::as_str) == Some(choice_id))
                })
        })
        .cloned()
        .unwrap_or(Value::Null);
    let intent_recommendation = page.get("intentRecommendation").cloned();
    let navigation_routes = page.get("navigationRoutes").cloned();
    let next_choices = page
        .get("choices")
        .and_then(Value::as_array)
        .map(|choices| {
            choices
                .iter()
                .take(8)
                .map(choice_summary)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let mut validation_args = json!({
        "graphId": snapshot.graph_id,
    });
    if let Some(workflow_name) = workflow_name {
        validation_args["workflowName"] = json!(workflow_name);
    }
    let perception_choice = workflow_name.map(|workflow_name| {
        json!({
            "tool": "workflow_book_choose",
            "arguments": {
                "graphId": snapshot.graph_id,
                "workflowName": workflow_name,
                "pageId": "overview",
                "choiceId": "perception-map",
            },
        })
    });

    let mut state = json!({
        "kind": "workflowAuthoringState",
        "graphId": snapshot.graph_id,
        "workflowName": workflow_name,
        "currentPageId": current_page_id,
        "currentPageKind": current_page_kind,
        "status": authoring_status(current_page_kind, reports.len(), error_count, warning_count),
        "message": authoring_message(current_page_kind, reports.len(), error_count, warning_count),
        "validation": {
            "state": "complete",
            "scope": "current-snapshot",
            "passed": error_count == 0,
            "workflowCount": reports.len(),
            "errors": error_count,
            "warnings": warning_count,
            "infos": info_count,
            "refreshAction": {
                "tool": "workflow_book_validate",
                "arguments": validation_args,
            },
        },
        "suggestedChoiceId": suggested_choice_id,
        "suggestedChoice": suggested_choice,
        "nextChoices": next_choices,
        "perceptionAction": perception_choice,
    });
    if let Some(intent_recommendation) = intent_recommendation {
        state["intentRecommendation"] = intent_recommendation;
    }
    if let Some(navigation_routes) = navigation_routes {
        state["navigationRoutes"] = navigation_routes;
    }
    state
}

fn choice_summary(choice: &Value) -> Value {
    json!({
        "id": choice.get("id").cloned().unwrap_or(Value::Null),
        "kind": choice.get("kind").cloned().unwrap_or(Value::Null),
        "label": choice.get("label").cloned().unwrap_or(Value::Null),
        "description": choice.get("description").cloned().unwrap_or(Value::Null),
        "targetPageId": choice.get("targetPageId").cloned().unwrap_or(Value::Null),
        "hasAction": choice.get("action").is_some_and(|action| !action.is_null()),
    })
}

fn authoring_status(
    current_page_kind: &str,
    workflow_count: usize,
    error_count: usize,
    warning_count: usize,
) -> &'static str {
    if current_page_kind == "catalog" {
        return if workflow_count == 0 {
            "catalog_empty"
        } else {
            "choose_workflow"
        };
    }
    if error_count > 0 {
        return "needs_repair";
    }
    if current_page_kind == "execute" {
        return "ready_to_execute";
    }
    if warning_count > 0 {
        return "ready_with_warnings";
    }
    "ready"
}

fn authoring_message(
    current_page_kind: &str,
    workflow_count: usize,
    error_count: usize,
    warning_count: usize,
) -> String {
    match authoring_status(
        current_page_kind,
        workflow_count,
        error_count,
        warning_count,
    ) {
        "catalog_empty" => {
            "No workflow definitions are visible yet; compose or load workflow RDF next."
                .to_string()
        }
        "choose_workflow" => "Choose a workflow or compose a new one.".to_string(),
        "needs_repair" => {
            format!(
                "Validation found {error_count} error(s); repair graph anatomy before execution."
            )
        }
        "ready_to_execute" => {
            "Validation passes and this page exposes explicit execution actions.".to_string()
        }
        "ready_with_warnings" => format!(
            "Validation passes with {warning_count} warning(s); inspect warnings or continue."
        ),
        _ => "Validation passes; follow the suggested choice or inspect perception/raw SPARQL."
            .to_string(),
    }
}

fn workflow_catalog_item(workflow: &WorkflowSummary) -> Value {
    json!({
        "name": workflow.name,
        "description": workflow.description,
        "workflowUri": workflow.uri,
        "documentId": workflow.doc_id,
        "phaseCount": workflow.phases.len(),
        "agentCount": workflow.nodes.len(),
        "runCount": workflow.runs.len(),
        "compositionEventCount": workflow.composition_events.len(),
        "pageChoiceId": format!("open-{}", choice_suffix(&workflow.name)),
    })
}

fn workflow_facts(snapshot: &WorkflowBookSnapshot, workflow: &WorkflowSummary) -> Vec<Value> {
    vec![
        json!({"label": "graphId", "value": snapshot.graph_id}),
        json!({"label": "workflowUri", "value": workflow.uri}),
        json!({"label": "documentId", "value": workflow.doc_id}),
        json!({"label": "scriptSha256", "value": workflow.script_sha256}),
        json!({"label": "scriptBlock", "value": workflow.script_block}),
        json!({"label": "inputBlock", "value": workflow.input_block}),
        json!({"label": "whenToUse", "value": workflow.when_to_use}),
        json!({"label": "phaseCount", "value": workflow.phases.len()}),
        json!({"label": "agentCount", "value": workflow.nodes.len()}),
        json!({"label": "runCount", "value": workflow.runs.len()}),
        json!({"label": "compositionEventCount", "value": workflow.composition_events.len()}),
    ]
}

fn workflow_book_apply_request(arguments: &Value) -> Result<WorkflowBookApplyRequest, String> {
    let operation = mcp_arg_string(arguments, &["operation", "op"]);
    let update = mcp_arg_string(arguments, &["update", "sparqlUpdate", "sparql_update"]);
    let data = mcp_arg_string(arguments, &["data"]);

    if update.is_some() && data.is_some() {
        return Err("provide either update or data/format, not both".to_string());
    }

    match operation.as_deref() {
        Some("sparql_update") | Some("sparql-update") | Some("sparql") | Some("update") => {
            let update = update.ok_or_else(|| {
                "update is required for workflow_book_apply sparql_update".to_string()
            })?;
            Ok(WorkflowBookApplyRequest::SparqlUpdate { update })
        }
        Some("rdf_load") | Some("rdf-load") | Some("rdf") | Some("load") => {
            let data = data
                .ok_or_else(|| "data is required for workflow_book_apply rdf_load".to_string())?;
            Ok(WorkflowBookApplyRequest::RdfLoad {
                data,
                format: mcp_arg_string(arguments, &["format"])
                    .unwrap_or_else(|| "turtle".to_string()),
                base_iri: mcp_arg_string(arguments, &["baseIri", "base_iri"]),
                target_graph_iri: mcp_arg_string(
                    arguments,
                    &["targetGraphIri", "target_graph_iri"],
                ),
            })
        }
        Some(other) => Err(format!(
            "unsupported workflow_book_apply operation '{other}'; use sparql_update or rdf_load"
        )),
        None => {
            if let Some(update) = update {
                return Ok(WorkflowBookApplyRequest::SparqlUpdate { update });
            }
            if let Some(data) = data {
                return Ok(WorkflowBookApplyRequest::RdfLoad {
                    data,
                    format: mcp_arg_string(arguments, &["format"])
                        .unwrap_or_else(|| "turtle".to_string()),
                    base_iri: mcp_arg_string(arguments, &["baseIri", "base_iri"]),
                    target_graph_iri: mcp_arg_string(
                        arguments,
                        &["targetGraphIri", "target_graph_iri"],
                    ),
                });
            }
            Err("workflow_book_apply requires either update or data".to_string())
        }
    }
}

fn workflow_book_apply_event_plan(
    snapshot: &WorkflowBookSnapshot,
    arguments: &Value,
    request_kind: &str,
) -> Result<Option<WorkflowBookCompositionEventPlan>, String> {
    let Some(workflow_name) = mcp_arg_string(arguments, &["workflowName", "workflow_name"]) else {
        return Ok(None);
    };
    let workflow = existing_workflow(snapshot, arguments, &workflow_name)?;
    let definition_subject = optional_uri_arg(
        arguments,
        &[
            "definitionSubject",
            "definition_subject",
            "workflowDefinitionSubject",
        ],
    )?
    .unwrap_or_else(|| workflow.uri.clone());
    let target_subject =
        optional_uri_arg(arguments, &["targetSubject", "target_subject", "targetUri"])?
            .unwrap_or_else(|| definition_subject.clone());
    let gesture_kind = mcp_arg_string(
        arguments,
        &["gestureKind", "gesture_kind", "compositionGesture"],
    )
    .unwrap_or_else(|| format!("apply_{request_kind}"));
    let mut event = composition_event_plan(
        snapshot,
        arguments,
        &workflow.name,
        &definition_subject,
        &target_subject,
        &gesture_kind,
    )?;
    let insert_triples = composition_event_delta_arg(
        arguments,
        &["insertTriple", "insertTriples", "insert_triples"],
    );
    let delete_triples = composition_event_delta_arg(
        arguments,
        &["deleteTriple", "deleteTriples", "delete_triples"],
    );
    append_composition_fold_delta(&mut event, &insert_triples, &delete_triples);
    Ok(Some(event))
}

fn workflow_book_compose_plan(
    snapshot: &WorkflowBookSnapshot,
    arguments: &Value,
) -> Result<WorkflowBookComposePlan, String> {
    let operation = mcp_arg_string(arguments, &["operation", "op"])
        .ok_or_else(|| "operation is required for workflow_book_compose".to_string())?;
    match normalize_compose_operation(&operation)?.as_str() {
        "create_workflow" => compose_create_workflow_plan(snapshot, arguments),
        "add_phase" => compose_add_phase_plan(snapshot, arguments),
        "attach_agent_node" => compose_attach_agent_node_plan(snapshot, arguments),
        "bind_source_block" => compose_bind_source_block_plan(snapshot, arguments),
        "bind_input_block" => compose_bind_input_block_plan(snapshot, arguments),
        other => Err(format!(
            "unsupported workflow_book_compose operation '{other}'; use create_workflow, add_phase, attach_agent_node, bind_source_block, or bind_input_block"
        )),
    }
}

fn normalize_compose_operation(operation: &str) -> Result<String, String> {
    let normalized = operation.trim().replace('-', "_");
    match normalized.as_str() {
        "create_workflow" | "create" | "workflow" => Ok("create_workflow".to_string()),
        "add_phase" | "phase" => Ok("add_phase".to_string()),
        "attach_agent_node" | "add_agent_node" | "agent_node" | "attach_agent" | "agent" => {
            Ok("attach_agent_node".to_string())
        }
        "bind_source_block" | "bind_source" | "source_block" | "script_block" => {
            Ok("bind_source_block".to_string())
        }
        "bind_input_block"
        | "bind_input"
        | "input_block"
        | "workflow_args_block"
        | "args_block" => Ok("bind_input_block".to_string()),
        _ => Ok(normalized),
    }
}

fn compose_create_workflow_plan(
    snapshot: &WorkflowBookSnapshot,
    arguments: &Value,
) -> Result<WorkflowBookComposePlan, String> {
    let workflow_name = required_workflow_name(arguments)?;
    if snapshot
        .workflows
        .iter()
        .any(|workflow| workflow.name == workflow_name)
    {
        return Err(format!(
            "workflow '{workflow_name}' already exists; open it or compose a more specific edit"
        ));
    }
    let workflow_uri = optional_uri_arg(arguments, &["workflowUri", "workflow_uri"])?
        .unwrap_or_else(|| minted_workflow_uri(&snapshot.graph_id, &workflow_name));
    if snapshot
        .workflows
        .iter()
        .any(|workflow| workflow.uri == workflow_uri)
    {
        return Err(format!("workflow URI already exists: {workflow_uri}"));
    }

    let mut triples = vec![
        format!("<{workflow_uri}> a <{WF_NS}Workflow> ."),
        format!(
            "<{workflow_uri}> <{WF_NS}name> {} .",
            sparql_string_literal(&workflow_name)
        ),
    ];
    if let Some(description) = mcp_arg_string(arguments, &["description"]) {
        triples.push(format!(
            "<{workflow_uri}> <{WF_NS}description> {} .",
            sparql_string_literal(&description)
        ));
    }
    if let Some(when_to_use) = mcp_arg_string(arguments, &["whenToUse", "when_to_use"]) {
        triples.push(format!(
            "<{workflow_uri}> <{WF_NS}whenToUse> {} .",
            sparql_string_literal(&when_to_use)
        ));
    }
    if let Some(script_sha256) = mcp_arg_string(arguments, &["scriptSha256", "script_sha256"]) {
        triples.push(format!(
            "<{workflow_uri}> <{WF_NS}scriptSha256> {} .",
            sparql_string_literal(&script_sha256)
        ));
    }
    if let Some(script_block) = script_block_from_args(&snapshot.graph_id, arguments, false)? {
        triples.push(format!(
            "<{workflow_uri}> <{WF_NS}scriptBlock> <{script_block}> ."
        ));
    }
    if let Some(input_block) = input_block_from_args(&snapshot.graph_id, arguments, false)? {
        triples.push(format!(
            "<{workflow_uri}> <{WF_NS}inputBlock> <{input_block}> ."
        ));
    }
    push_seeded_from_triples(&mut triples, &workflow_uri, arguments)?;
    let definition_insert_triples = triples.clone();
    let mut event = composition_event_plan(
        snapshot,
        arguments,
        &workflow_name,
        &workflow_uri,
        &workflow_uri,
        "create_workflow",
    )?;
    append_composition_fold_delta(&mut event, &definition_insert_triples, &[]);

    Ok(compose_plan_from_event(
        &snapshot.read_graph,
        "create_workflow",
        workflow_name,
        workflow_uri,
        "overview".to_string(),
        event,
    ))
}

fn compose_add_phase_plan(
    snapshot: &WorkflowBookSnapshot,
    arguments: &Value,
) -> Result<WorkflowBookComposePlan, String> {
    let workflow_name = required_workflow_name(arguments)?;
    let workflow = existing_workflow(snapshot, arguments, &workflow_name)?;
    let order = mcp_arg_usize(arguments, &["phaseOrder", "phase_order", "order"], 0);
    if order == 0 {
        return Err("phaseOrder/order must be a positive integer".to_string());
    }
    if workflow.phases.iter().any(|phase| phase.order == order) {
        return Err(format!(
            "workflow '{}' already has a phase with order {order}",
            workflow.name
        ));
    }
    let title = mcp_arg_string(arguments, &["phaseTitle", "phase_title", "title"])
        .unwrap_or_else(|| format!("Phase {order}"));
    let phase_uri = optional_uri_arg(arguments, &["phaseUri", "phase_uri"])?
        .unwrap_or_else(|| minted_phase_uri(snapshot, workflow, order));
    if workflow.phases.iter().any(|phase| phase.uri == phase_uri) {
        return Err(format!("phase URI already exists: {phase_uri}"));
    }

    let mut triples = vec![
        format!("<{}> <{WF_NS}phase> <{phase_uri}> .", workflow.uri),
        format!("<{phase_uri}> a <{WF_NS}Phase> ."),
        format!(
            "<{phase_uri}> <{WF_NS}order> {} .",
            sparql_usize_literal(order)
        ),
        format!(
            "<{phase_uri}> <{DCTERMS_NS}title> {} .",
            sparql_string_literal(&title)
        ),
    ];
    if let Some(description) = mcp_arg_string(
        arguments,
        &["phaseDescription", "phase_description", "description"],
    ) {
        triples.push(format!(
            "<{phase_uri}> <{DCTERMS_NS}description> {} .",
            sparql_string_literal(&description)
        ));
    }
    push_seeded_from_triples(&mut triples, &phase_uri, arguments)?;
    let definition_insert_triples = triples.clone();
    let mut event = composition_event_plan(
        snapshot,
        arguments,
        &workflow.name,
        &workflow.uri,
        &phase_uri,
        "add_phase",
    )?;
    append_composition_fold_delta(&mut event, &definition_insert_triples, &[]);

    Ok(compose_plan_from_event(
        &snapshot.read_graph,
        "add_phase",
        workflow_name,
        workflow.uri.clone(),
        phase_page_id(order),
        event,
    ))
}

fn compose_attach_agent_node_plan(
    snapshot: &WorkflowBookSnapshot,
    arguments: &Value,
) -> Result<WorkflowBookComposePlan, String> {
    let workflow_name = required_workflow_name(arguments)?;
    let workflow = existing_workflow(snapshot, arguments, &workflow_name)?;
    let label = mcp_arg_string(arguments, &["label", "nodeLabel", "node_label"])
        .ok_or_else(|| "label/nodeLabel is required for attach_agent_node".to_string())?;
    if workflow.nodes.iter().any(|node| node.label == label) {
        return Err(format!(
            "workflow '{}' already has an agent node labeled '{}'",
            workflow.name, label
        ));
    }
    let phase_index = mcp_arg_usize(arguments, &["phaseIndex", "phase_index"], 0);
    let phase_index = if phase_index == 0 {
        workflow
            .phases
            .first()
            .map(|phase| phase.order)
            .ok_or_else(|| {
                format!(
                    "workflow '{}' has no phases; add a phase before attaching agent nodes",
                    workflow.name
                )
            })?
    } else {
        phase_index
    };
    if !workflow
        .phases
        .iter()
        .any(|phase| phase.order == phase_index)
    {
        return Err(format!(
            "phaseIndex {phase_index} does not resolve to a phase in workflow '{}'",
            workflow.name
        ));
    }
    let node_uri = if let Some(node_uri) =
        optional_uri_arg(arguments, &["nodeUri", "node_uri", "agentNodeUri"])?
    {
        node_uri
    } else if let Some(document_id) = mcp_arg_string(arguments, &["documentId", "document_id"]) {
        local_document_uri(&snapshot.graph_id, &document_id)?
    } else {
        minted_agent_node_uri(snapshot, workflow, &label)
    };

    let mut triples = vec![
        format!("<{node_uri}> a <{WF_NS}AgentNode> ."),
        format!("<{node_uri}> <{WF_NS}partOfWorkflow> <{}> .", workflow.uri),
        format!(
            "<{node_uri}> <{WF_NS}label> {} .",
            sparql_string_literal(&label)
        ),
        format!(
            "<{node_uri}> <{WF_NS}phaseIndex> {} .",
            sparql_usize_literal(phase_index)
        ),
    ];
    if let Some(agent_type) = mcp_arg_string(arguments, &["agentType", "agent_type"]) {
        triples.push(format!(
            "<{node_uri}> <{WF_NS}agentType> {} .",
            sparql_string_literal(&agent_type)
        ));
    }
    push_seeded_from_triples(&mut triples, &node_uri, arguments)?;
    let definition_insert_triples = triples.clone();
    let mut event = composition_event_plan(
        snapshot,
        arguments,
        &workflow.name,
        &workflow.uri,
        &node_uri,
        "attach_agent_node",
    )?;
    append_composition_fold_delta(&mut event, &definition_insert_triples, &[]);

    Ok(compose_plan_from_event(
        &snapshot.read_graph,
        "attach_agent_node",
        workflow_name,
        workflow.uri.clone(),
        agent_page_id(&label),
        event,
    ))
}

fn compose_bind_source_block_plan(
    snapshot: &WorkflowBookSnapshot,
    arguments: &Value,
) -> Result<WorkflowBookComposePlan, String> {
    let workflow_name = required_workflow_name(arguments)?;
    let workflow = existing_workflow(snapshot, arguments, &workflow_name)?;
    let script_block = script_block_from_args(&snapshot.graph_id, arguments, true)?
        .expect("required script block returns Some");
    let script_sha256 = mcp_arg_string(arguments, &["scriptSha256", "script_sha256"]);
    let definition_insert_triples =
        source_block_insert_triples(&workflow.uri, &script_block, script_sha256.as_deref());
    let definition_delete_triples = source_block_delete_triples(workflow);
    let mut event = composition_event_plan(
        snapshot,
        arguments,
        &workflow.name,
        &workflow.uri,
        &workflow.uri,
        "bind_source_block",
    )?;
    append_composition_fold_delta(
        &mut event,
        &definition_insert_triples,
        &definition_delete_triples,
    );

    Ok(compose_plan_from_event(
        &snapshot.read_graph,
        "bind_source_block",
        workflow_name,
        workflow.uri.clone(),
        "execute".to_string(),
        event,
    ))
}

fn compose_bind_input_block_plan(
    snapshot: &WorkflowBookSnapshot,
    arguments: &Value,
) -> Result<WorkflowBookComposePlan, String> {
    let workflow_name = required_workflow_name(arguments)?;
    let workflow = existing_workflow(snapshot, arguments, &workflow_name)?;
    let input_block = input_block_from_args(&snapshot.graph_id, arguments, true)?
        .expect("required input block returns Some");
    let definition_insert_triples = vec![input_block_triple(&workflow.uri, &input_block)];
    let definition_delete_triples = workflow
        .input_block
        .as_deref()
        .map(|old_input_block| input_block_triple(&workflow.uri, old_input_block))
        .into_iter()
        .collect::<Vec<_>>();
    let mut event = composition_event_plan(
        snapshot,
        arguments,
        &workflow.name,
        &workflow.uri,
        &workflow.uri,
        "bind_input_block",
    )?;
    append_composition_fold_delta(
        &mut event,
        &definition_insert_triples,
        &definition_delete_triples,
    );

    Ok(compose_plan_from_event(
        &snapshot.read_graph,
        "bind_input_block",
        workflow_name,
        workflow.uri.clone(),
        "execute".to_string(),
        event,
    ))
}

fn compose_plan_from_event(
    read_graph: &str,
    operation: &str,
    workflow_name: String,
    workflow_uri: String,
    target_page_id: String,
    event: WorkflowBookCompositionEventPlan,
) -> WorkflowBookComposePlan {
    let definition_insert_count = event.insert_triples.len();
    let definition_delete_count = event.delete_triples.len();
    let update = composition_event_fold_projection_update(read_graph, &event);
    let retained_memory_record = composition_event_rationale_memory_record(&event);
    WorkflowBookComposePlan {
        operation: operation.to_string(),
        workflow_name,
        workflow_uri,
        target_page_id,
        update,
        definition_projection_source: COMPOSITION_FOLD_PROJECTION_SOURCE.to_string(),
        definition_insert_count,
        definition_delete_count,
        composition_event_uri: event.event_uri,
        authoring_session_uri: event.authoring_session_uri,
        composition_event_generated_at: event.generated_at,
        composition_event_order: event.event_order,
        retained_memory_record,
    }
}

fn composition_event_plan(
    snapshot: &WorkflowBookSnapshot,
    arguments: &Value,
    workflow_name: &str,
    definition_subject: &str,
    target_subject: &str,
    gesture_kind: &str,
) -> Result<WorkflowBookCompositionEventPlan, String> {
    let session_id = Uuid::new_v4().to_string();
    let now = Utc::now();
    let generated_at = now.to_rfc3339_opts(SecondsFormat::Millis, true);
    let event_order = now.timestamp_micros();
    let supplied_session_uri = optional_uri_arg(
        arguments,
        &[
            "authoringSessionUri",
            "authoring_session_uri",
            "sessionUri",
            "session_uri",
        ],
    )?;
    let implicit_session = supplied_session_uri.is_none();
    let authoring_session_uri = supplied_session_uri.unwrap_or_else(|| {
        minted_authoring_session_uri(&snapshot.graph_id, workflow_name, &session_id)
    });
    let event_session_id = if implicit_session {
        session_id.clone()
    } else {
        uri_tail_suffix(&authoring_session_uri)
    };
    let supplied_driver_lease = mcp_arg_string(arguments, &["driverLease", "driver_lease"]);
    let existing_driver_lease =
        existing_authoring_session_driver_lease(snapshot, &authoring_session_uri);
    if let Some(existing_driver_lease) = existing_driver_lease.as_deref() {
        let Some(supplied_driver_lease) = supplied_driver_lease.as_deref() else {
            return Err(format!(
                "authoringSessionUri '{authoring_session_uri}' is driver-leased; provide matching driverLease"
            ));
        };
        if supplied_driver_lease != existing_driver_lease {
            return Err(format!(
                "driverLease does not match authoringSessionUri '{authoring_session_uri}'"
            ));
        }
    }
    let should_write_driver_lease =
        supplied_driver_lease.is_some() && (implicit_session || existing_driver_lease.is_none());
    let event_uri = optional_uri_arg(
        arguments,
        &[
            "compositionEventUri",
            "composition_event_uri",
            "eventUri",
            "event_uri",
        ],
    )?
    .unwrap_or_else(|| {
        minted_composition_event_uri(
            &snapshot.graph_id,
            workflow_name,
            &event_session_id,
            event_order,
        )
    });
    let delta_json = serde_json::to_string(&json!({
        "operation": gesture_kind,
        "workflowName": workflow_name,
        "definitionSubject": definition_subject,
        "targetSubject": target_subject,
        "arguments": arguments,
    }))
    .map_err(|error| format!("serialize composition event delta: {error}"))?;

    let mut triples = Vec::new();
    if implicit_session {
        triples.extend([
            format!("<{authoring_session_uri}> a <{WF_NS}AuthoringSession>, <{PROV_NS}Activity> ."),
            format!(
                "<{authoring_session_uri}> <{WF_NS}definitionSubject> <{definition_subject}> ."
            ),
            format!(
                "<{authoring_session_uri}> <{WF_NS}openedAt> {} .",
                sparql_datetime_literal(&generated_at)
            ),
            format!(
                "<{authoring_session_uri}> <{WF_NS}sessionStatus> {} .",
                sparql_string_literal("open")
            ),
            format!(
                "<{authoring_session_uri}> <{WF_NS}workflowName> {} .",
                sparql_string_literal(workflow_name)
            ),
        ]);
    }
    triples.extend([
        format!("<{authoring_session_uri}> <{WF_NS}hasCompositionEvent> <{event_uri}> ."),
        format!("<{event_uri}> a <{WF_NS}CompositionEvent>, <{PROV_NS}Activity> ."),
        format!(
            "<{event_uri}> <{PROV_NS}generatedAtTime> {} .",
            sparql_datetime_literal(&generated_at)
        ),
        format!("<{event_uri}> <{WF_NS}definitionSubject> <{definition_subject}> ."),
        format!(
            "<{event_uri}> <{WF_NS}deltaJson> {} .",
            sparql_string_literal(&delta_json)
        ),
        format!(
            "<{event_uri}> <{WF_NS}eventOrder> {} .",
            sparql_i64_literal(event_order)
        ),
        format!(
            "<{event_uri}> <{WF_NS}gestureKind> {} .",
            sparql_string_literal(gesture_kind)
        ),
        format!("<{event_uri}> <{WF_NS}partOfAuthoringSession> <{authoring_session_uri}> ."),
        format!("<{event_uri}> <{WF_NS}targetSubject> <{target_subject}> ."),
    ]);
    push_authoring_agent_projection_triples(
        &mut triples,
        &snapshot.graph_id,
        &authoring_session_uri,
        &event_uri,
        &event_session_id,
        &generated_at,
        event_order,
    );
    let driver_agent = mcp_arg_string(
        arguments,
        &["driverAgent", "driver_agent", "agentId", "agent_id"],
    );
    if let Some(driver_agent) = driver_agent.as_deref() {
        if implicit_session {
            triples.push(format!(
                "<{authoring_session_uri}> <{WF_NS}driverAgent> {} .",
                sparql_string_literal(driver_agent)
            ));
        }
        triples.push(format!(
            "<{event_uri}> <{WF_NS}driverAgent> {} .",
            sparql_string_literal(driver_agent)
        ));
    }
    if implicit_session {
        if let Some(driver_lease) = supplied_driver_lease
            .as_deref()
            .filter(|_| should_write_driver_lease)
        {
            triples.push(format!(
                "<{authoring_session_uri}> <{WF_NS}driverLease> {} .",
                sparql_string_literal(driver_lease)
            ));
        }
    } else if let Some(driver_lease) = supplied_driver_lease
        .as_deref()
        .filter(|_| should_write_driver_lease)
    {
        triples.push(format!(
            "<{authoring_session_uri}> <{WF_NS}driverLease> {} .",
            sparql_string_literal(driver_lease)
        ));
    }
    let rationale = mcp_arg_string(arguments, &["rationale", "why"]);
    if let Some(rationale) = rationale.as_deref() {
        triples.push(format!(
            "<{event_uri}> <{WF_NS}rationale> {} .",
            sparql_string_literal(rationale)
        ));
    }
    push_seeded_from_triples(&mut triples, &event_uri, arguments)?;

    Ok(WorkflowBookCompositionEventPlan {
        event_uri,
        authoring_session_uri,
        generated_at,
        event_order,
        workflow_name: workflow_name.to_string(),
        definition_subject: definition_subject.to_string(),
        target_subject: target_subject.to_string(),
        gesture_kind: gesture_kind.to_string(),
        rationale,
        driver_agent,
        triples,
        insert_triples: Vec::new(),
        delete_triples: Vec::new(),
    })
}

fn composition_event_rationale_memory_record(
    event: &WorkflowBookCompositionEventPlan,
) -> Option<MemoryRecordIn> {
    let rationale = event.rationale.as_deref()?.trim();
    if rationale.is_empty() {
        return None;
    }
    let observed_at = event.event_order / 1000;
    let content = format!(
        "Workflow authoring rationale for '{}' ({}): {}",
        event.workflow_name, event.gesture_kind, rationale
    );
    Some(MemoryRecordIn {
        client_ref: Some(format!(
            "workflow-composition-rationale:{}",
            choice_suffix(&event.event_uri)
        )),
        scope: "graph".to_string(),
        kind: "SummaryMemory".to_string(),
        content_orientation: "knowledge".to_string(),
        visibility: "shared".to_string(),
        status: "active".to_string(),
        content,
        source_refs: vec![SourceRefIn {
            source_kind: "PlatformEvent".to_string(),
            source_label: Some(format!(
                "Workflow CompositionEvent {} for {}",
                event.gesture_kind, event.workflow_name
            )),
            block_id: None,
            document_id: None,
            external_id: Some(event.event_uri.clone()),
            external_uri: Some(event.event_uri.clone()),
            observed_at: Some(observed_at),
            trust_tier: Some("system".to_string()),
        }],
        evidence: Vec::new(),
        observed_at: Some(observed_at),
        valid_from: None,
        is_current: None,
        confidence: None,
        valence: None,
        agent_id: event.driver_agent.clone(),
        observer_agent_id: None,
        tags: vec![
            "workflow".to_string(),
            "workflow-authoring".to_string(),
            event.gesture_kind.clone(),
            choice_suffix(&event.definition_subject),
            choice_suffix(&event.target_subject),
        ],
        supersedes_ref: None,
        contradicts_ref: None,
    })
}

fn existing_authoring_session_driver_lease(
    snapshot: &WorkflowBookSnapshot,
    authoring_session_uri: &str,
) -> Option<String> {
    snapshot
        .workflows
        .iter()
        .flat_map(|workflow| workflow.composition_events.iter())
        .filter(|event| event.authoring_session_uri == authoring_session_uri)
        .filter_map(|event| event.driver_lease.as_deref())
        .find(|lease| !lease.trim().is_empty())
        .map(str::to_string)
}

fn push_authoring_agent_projection_triples(
    triples: &mut Vec<String>,
    graph_id: &str,
    authoring_session_uri: &str,
    event_uri: &str,
    event_session_id: &str,
    generated_at: &str,
    event_order: i64,
) {
    let agent_session_uri = minted_authoring_agent_session_uri(authoring_session_uri);
    let agent_turn_uri = minted_authoring_agent_turn_uri(&agent_session_uri, event_order);
    let session_id = format!(
        "workflow-authoring-{}-{}",
        choice_suffix(graph_id),
        choice_suffix(event_session_id)
    );
    triples.extend([
        format!("<{authoring_session_uri}> <{WF_NS}boundToAgent> <{agent_session_uri}> ."),
        format!("<{agent_session_uri}> a <{AGT_NS}Session> ."),
        format!(
            "<{agent_session_uri}> <{AGT_NS}sessionId> {} .",
            sparql_string_literal(&session_id)
        ),
        format!(
            "<{agent_session_uri}> <{AGT_NS}graphId> {} .",
            sparql_string_literal(graph_id)
        ),
        format!("<{agent_session_uri}> <{AGT_NS}realizedBy> <{authoring_session_uri}> ."),
        format!("<{event_uri}> <{WF_NS}boundToAgent> <{agent_turn_uri}> ."),
        format!("<{agent_turn_uri}> a <{AGT_NS}Turn> ."),
        format!("<{agent_turn_uri}> <{AGT_NS}inSession> <{agent_session_uri}> ."),
        format!(
            "<{agent_turn_uri}> <{AGT_NS}ordinal> {} .",
            sparql_i64_literal(event_order)
        ),
        format!(
            "<{agent_turn_uri}> <{AGT_NS}role> {} .",
            sparql_string_literal("authoring")
        ),
        format!(
            "<{agent_turn_uri}> <{AGT_NS}turnTime> {} .",
            sparql_datetime_literal(generated_at)
        ),
        format!("<{agent_turn_uri}> <{AGT_NS}realizedBy> <{event_uri}> ."),
    ]);
}

fn required_workflow_name(arguments: &Value) -> Result<String, String> {
    mcp_arg_string(arguments, &["workflowName", "workflow_name"])
        .ok_or_else(|| "workflowName is required".to_string())
}

fn existing_workflow<'a>(
    snapshot: &'a WorkflowBookSnapshot,
    arguments: &Value,
    workflow_name: &str,
) -> Result<&'a WorkflowSummary, String> {
    if let Some(workflow_uri) = optional_uri_arg(arguments, &["workflowUri", "workflow_uri"])? {
        return snapshot
            .workflows
            .iter()
            .find(|workflow| workflow.uri == workflow_uri)
            .ok_or_else(|| format!("workflow URI not found: {workflow_uri}"));
    }
    snapshot
        .workflows
        .iter()
        .find(|workflow| workflow.name == workflow_name)
        .ok_or_else(|| format!("workflow not found: {workflow_name}"))
}

fn optional_uri_arg(arguments: &Value, keys: &[&str]) -> Result<Option<String>, String> {
    mcp_arg_string(arguments, keys)
        .map(|value| validate_uri_for_angle(&value, keys[0]))
        .transpose()
}

fn push_seeded_from_triples(
    triples: &mut Vec<String>,
    subject_uri: &str,
    arguments: &Value,
) -> Result<(), String> {
    for seed_uri in seeded_from_uris(arguments)? {
        triples.push(seeded_from_triple(subject_uri, &seed_uri));
    }
    Ok(())
}

fn append_composition_fold_delta(
    event: &mut WorkflowBookCompositionEventPlan,
    insert_triples: &[String],
    delete_triples: &[String],
) {
    let insert_triples = normalized_delta_triples(insert_triples);
    let delete_triples = normalized_delta_triples(delete_triples);
    event.insert_triples.extend(insert_triples.iter().cloned());
    event.delete_triples.extend(delete_triples.iter().cloned());
    for triple in insert_triples {
        event.triples.push(format!(
            "<{}> <{WF_NS}insertTriple> {} .",
            event.event_uri,
            sparql_string_literal(&triple)
        ));
    }
    for triple in delete_triples {
        event.triples.push(format!(
            "<{}> <{WF_NS}deleteTriple> {} .",
            event.event_uri,
            sparql_string_literal(&triple)
        ));
    }
}

fn composition_event_delta_arg(arguments: &Value, keys: &[&str]) -> Vec<String> {
    normalized_delta_triples(&mcp_arg_string_vec(arguments, keys))
}

fn normalized_delta_triples(triples: &[String]) -> Vec<String> {
    triples
        .iter()
        .map(|triple| triple.trim())
        .filter(|triple| !triple.is_empty())
        .map(str::to_string)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn seeded_from_uris(arguments: &Value) -> Result<Vec<String>, String> {
    mcp_arg_string_vec(
        arguments,
        &["seededFrom", "seeded_from", "seedUri", "seed_uri"],
    )
    .into_iter()
    .map(|value| validate_uri_for_angle(&value, "seededFrom"))
    .collect()
}

fn script_block_from_args(
    graph_id: &str,
    arguments: &Value,
    required: bool,
) -> Result<Option<String>, String> {
    if let Some(script_block) = optional_uri_arg(
        arguments,
        &["scriptBlock", "script_block", "workflowSourceBlockUri"],
    )? {
        return Ok(Some(script_block));
    }
    let document_id = mcp_arg_string(
        arguments,
        &[
            "sourceDocumentId",
            "source_document_id",
            "workflowSourceDocumentId",
        ],
    );
    let block_id = mcp_arg_string(
        arguments,
        &["sourceBlockId", "source_block_id", "workflowSourceBlockId"],
    );
    match (document_id, block_id) {
        (Some(document_id), Some(block_id)) => {
            Ok(Some(local_block_uri(graph_id, &document_id, &block_id)?))
        }
        (None, None) if required => Err(
            "scriptBlock or sourceDocumentId/sourceBlockId is required for bind_source_block"
                .to_string(),
        ),
        (None, None) => Ok(None),
        _ => Err("sourceDocumentId and sourceBlockId must be provided together".to_string()),
    }
}

fn input_block_from_args(
    graph_id: &str,
    arguments: &Value,
    required: bool,
) -> Result<Option<String>, String> {
    if let Some(input_block) = optional_uri_arg(
        arguments,
        &["inputBlock", "input_block", "workflowArgsBlockUri"],
    )? {
        return Ok(Some(input_block));
    }
    let document_id = mcp_arg_string(
        arguments,
        &[
            "inputDocumentId",
            "input_document_id",
            "workflowArgsDocumentId",
        ],
    );
    let block_id = mcp_arg_string(
        arguments,
        &["inputBlockId", "input_block_id", "workflowArgsBlockId"],
    );
    match (document_id, block_id) {
        (Some(document_id), Some(block_id)) => {
            Ok(Some(local_block_uri(graph_id, &document_id, &block_id)?))
        }
        (None, None) if required => Err(
            "inputBlock or inputDocumentId/inputBlockId is required for bind_input_block"
                .to_string(),
        ),
        (None, None) => Ok(None),
        _ => Err("inputDocumentId and inputBlockId must be provided together".to_string()),
    }
}

fn source_block_insert_triples(
    workflow_uri: &str,
    script_block: &str,
    script_sha256: Option<&str>,
) -> Vec<String> {
    let mut triples = vec![source_block_triple(workflow_uri, script_block)];
    if let Some(script_sha256) = script_sha256 {
        triples.push(script_sha256_triple(workflow_uri, script_sha256));
    }
    triples
}

fn source_block_delete_triples(workflow: &WorkflowSummary) -> Vec<String> {
    let mut triples = Vec::new();
    if let Some(script_block) = workflow.script_block.as_deref() {
        triples.push(source_block_triple(&workflow.uri, script_block));
    }
    if let Some(script_sha256) = workflow.script_sha256.as_deref() {
        triples.push(script_sha256_triple(&workflow.uri, script_sha256));
    }
    triples
}

fn source_block_triple(workflow_uri: &str, script_block: &str) -> String {
    format!("<{workflow_uri}> <{WF_NS}scriptBlock> <{script_block}> .")
}

fn script_sha256_triple(workflow_uri: &str, script_sha256: &str) -> String {
    format!(
        "<{workflow_uri}> <{WF_NS}scriptSha256> {} .",
        sparql_string_literal(script_sha256)
    )
}

fn input_block_triple(workflow_uri: &str, input_block: &str) -> String {
    format!("<{workflow_uri}> <{WF_NS}inputBlock> <{input_block}> .")
}

fn seeded_from_triple(subject_uri: &str, seed_uri: &str) -> String {
    format!("<{subject_uri}> <{WF_NS}seededFrom> <{seed_uri}> .")
}

fn validate_uri_for_angle(value: &str, field: &str) -> Result<String, String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(format!("{field} must not be empty"));
    }
    if trimmed
        .chars()
        .any(|ch| ch.is_whitespace() || ch == '<' || ch == '>')
    {
        return Err(format!(
            "{field} must be an absolute RDF IRI without whitespace or angle brackets"
        ));
    }
    Ok(trimmed.to_string())
}

fn local_document_uri(graph_id: &str, document_id: &str) -> Result<String, String> {
    validate_uri_component(graph_id, "graphId", false)?;
    validate_uri_component(document_id, "documentId", true)?;
    Ok(format!(
        "urn:mnemosyne:local:graph:{graph_id}:doc:{document_id}"
    ))
}

fn local_block_uri(graph_id: &str, document_id: &str, block_id: &str) -> Result<String, String> {
    validate_uri_component(block_id, "blockId", true)?;
    Ok(format!(
        "{}#{block_id}",
        local_document_uri(graph_id, document_id)?
    ))
}

fn validate_uri_component(value: &str, field: &str, reject_hash: bool) -> Result<(), String> {
    if value.trim().is_empty() {
        return Err(format!("{field} must not be empty"));
    }
    if value
        .chars()
        .any(|ch| ch.is_whitespace() || ch == '<' || ch == '>' || (reject_hash && ch == '#'))
    {
        return Err(format!(
            "{field} contains a character that cannot be used in a local workflow URI"
        ));
    }
    Ok(())
}

fn minted_workflow_uri(graph_id: &str, workflow_name: &str) -> String {
    format!(
        "urn:sophia:workflow:{}:{}",
        choice_suffix(graph_id),
        choice_suffix(workflow_name)
    )
}

fn minted_authoring_session_uri(graph_id: &str, workflow_name: &str, session_id: &str) -> String {
    format!(
        "urn:sophia:wf:authoring-session:{}:{}:{}",
        choice_suffix(graph_id),
        choice_suffix(workflow_name),
        choice_suffix(session_id)
    )
}

fn minted_composition_event_uri(
    graph_id: &str,
    workflow_name: &str,
    session_id: &str,
    event_order: i64,
) -> String {
    format!(
        "urn:sophia:wf:composition-event:{}:{}:{}:{event_order}",
        choice_suffix(graph_id),
        choice_suffix(workflow_name),
        choice_suffix(session_id)
    )
}

fn minted_authoring_agent_session_uri(authoring_session_uri: &str) -> String {
    format!(
        "urn:sophia:agent:session:workflow-authoring:{}",
        choice_suffix(authoring_session_uri)
    )
}

fn minted_authoring_agent_turn_uri(agent_session_uri: &str, event_order: i64) -> String {
    format!("{agent_session_uri}:turn:{event_order}")
}

fn minted_phase_uri(
    snapshot: &WorkflowBookSnapshot,
    workflow: &WorkflowSummary,
    order: usize,
) -> String {
    format!(
        "urn:sophia:workflow:{}:{}:phase:{order}",
        choice_suffix(&snapshot.graph_id),
        choice_suffix(&workflow.name)
    )
}

fn minted_agent_node_uri(
    snapshot: &WorkflowBookSnapshot,
    workflow: &WorkflowSummary,
    label: &str,
) -> String {
    format!(
        "urn:sophia:workflow:{}:{}:agent:{}",
        choice_suffix(&snapshot.graph_id),
        choice_suffix(&workflow.name),
        choice_suffix(label)
    )
}

fn insert_data_update(read_graph: &str, triples: Vec<String>) -> String {
    format!(
        "INSERT DATA {{\n  GRAPH <{read_graph}> {{\n    {}\n  }}\n}}",
        triples.join("\n    ")
    )
}

fn delete_data_update(read_graph: &str, triples: Vec<String>) -> Option<String> {
    if triples.is_empty() {
        None
    } else {
        Some(format!(
            "DELETE DATA {{\n  GRAPH <{read_graph}> {{\n    {}\n  }}\n}}",
            triples.join("\n    ")
        ))
    }
}

fn composition_event_fold_projection_update(
    read_graph: &str,
    event: &WorkflowBookCompositionEventPlan,
) -> String {
    let mut updates = vec![insert_data_update(read_graph, event.triples.clone())];
    if let Some(delete_update) = delete_data_update(read_graph, event.delete_triples.clone()) {
        updates.push(delete_update);
    }
    if !event.insert_triples.is_empty() {
        updates.push(insert_data_update(read_graph, event.insert_triples.clone()));
    }
    updates.join(";\n")
}

fn sparql_usize_literal(value: usize) -> String {
    format!("\"{value}\"^^<http://www.w3.org/2001/XMLSchema#integer>")
}

fn sparql_i64_literal(value: i64) -> String {
    format!("\"{value}\"^^<http://www.w3.org/2001/XMLSchema#integer>")
}

fn sparql_datetime_literal(value: &str) -> String {
    format!(
        "{}^^<http://www.w3.org/2001/XMLSchema#dateTime>",
        sparql_string_literal(value)
    )
}

fn validation_result_value(reports: &[WorkflowValidationReport]) -> Value {
    let error_count = reports.iter().map(|report| report.errors).sum::<usize>();
    let warning_count = reports.iter().map(|report| report.warnings).sum::<usize>();
    json!({
        "passed": error_count == 0,
        "summary": {
            "workflowCount": reports.len(),
            "errors": error_count,
            "warnings": warning_count,
        },
        "reports": reports,
    })
}

fn draft_repair_choices(
    snapshot: &WorkflowBookSnapshot,
    workflow: &WorkflowSummary,
    report: &WorkflowValidationReport,
) -> Vec<WorkflowBookChoice> {
    let codes = report
        .issues
        .iter()
        .map(|issue| issue.code.as_str())
        .collect::<BTreeSet<_>>();
    let mut choices = Vec::new();
    if codes.contains("wf:phase.missing") {
        choices.push(compose_template_choice(
            "compose-add-phase",
            "Compose phase",
            "Use workflow_book_compose to add the first ordered phase.",
            json!({
                "graphId": snapshot.graph_id,
                "operation": "add_phase",
                "workflowName": workflow.name,
                "phaseOrder": 1,
                "phaseTitle": "Phase 1",
                "phaseDescription": "Describe the phase objective."
            }),
        ));
    }
    if codes.contains("wf:scriptBlock") || codes.contains("wf:scriptSha256") {
        choices.push(compose_template_choice(
            "compose-bind-source-block",
            "Bind source block",
            "Use workflow_book_compose to bind the Garden block and optional sha that define workflow source identity.",
            json!({
                "graphId": snapshot.graph_id,
                "operation": "bind_source_block",
                "workflowName": workflow.name,
                "sourceDocumentId": "source-document-id",
                "sourceBlockId": "source-block-id",
                "scriptSha256": "sha256-if-known"
            }),
        ));
    }
    if codes.contains("wf:description") {
        choices.push(WorkflowBookChoice {
            id: "apply-description".to_string(),
            kind: "mcp-tool-template".to_string(),
            label: "Apply description".to_string(),
            description: Some(
                "Use workflow_book_apply to add a workflow description while recording a CompositionEvent."
                    .to_string(),
            ),
            target_page_id: None,
            action: Some(json!({
                "tool": "workflow_book_apply",
                "argumentsTemplate": {
                    "graphId": snapshot.graph_id,
                    "workflowName": workflow.name,
                    "operation": "sparql_update",
                    "gestureKind": "edit_description",
                    "targetSubject": workflow.uri,
                    "update": format!(
                        "INSERT DATA {{ GRAPH <{}> {{ <{}> <{}description> \"Describe this workflow.\" }} }}",
                        snapshot.read_graph, workflow.uri, WF_NS
                    )
                },
                "requiresEdits": true,
            })),
        });
    }
    choices
}

fn draft_object(workflow: &WorkflowSummary, report: &WorkflowValidationReport) -> Value {
    json!({
        "kind": "workflowDraft",
        "semanticClass": "wf:Draft",
        "uri": virtual_draft_uri(workflow),
        "definitionSubject": workflow.uri,
        "workflowName": workflow.name,
        "runnable": report.passed,
        "blockingGapCount": report.errors,
        "draftWarningCount": report.warnings,
        "infoCount": report.infos,
        "compositionEventCount": workflow.composition_events.len(),
        "sourceKind": "derived",
        "storeMode": "virtual",
        "identityKind": "resolve-by-query",
        "derivedFromQuery": DRAFT_DERIVED_FROM_QUERY,
    })
}

fn draft_completeness_gap_objects(report: &WorkflowValidationReport) -> Vec<Value> {
    report
        .issues
        .iter()
        .filter(|issue| issue.severity == "error")
        .map(|issue| {
            let target = issue
                .subject
                .as_deref()
                .unwrap_or(report.workflow_uri.as_str());
            json!({
                "kind": "completenessGap",
                "semanticClass": "wf:CompletenessGap",
                "uri": virtual_draft_issue_uri(report, "gap", issue),
                "gapBlocking": true,
                "gapKind": completeness_gap_kind(&issue.code),
                "gapTarget": target,
                "validationCode": issue.code,
                "rationale": issue.message,
                "sourceKind": "derived",
                "storeMode": "virtual",
                "identityKind": "resolve-by-query",
                "derivedFromQuery": COMPLETENESS_GAP_DERIVED_FROM_QUERY,
            })
        })
        .collect()
}

fn draft_warning_objects(report: &WorkflowValidationReport) -> Vec<Value> {
    report
        .issues
        .iter()
        .filter(|issue| issue.severity == "warning")
        .map(|issue| {
            let target = issue
                .subject
                .as_deref()
                .unwrap_or(report.workflow_uri.as_str());
            json!({
                "kind": "draftWarning",
                "semanticClass": "wf:DraftWarning",
                "uri": virtual_draft_issue_uri(report, "warning", issue),
                "warningKind": normalized_issue_kind(&issue.code),
                "warningTarget": target,
                "validationCode": issue.code,
                "rationale": issue.message,
                "sourceKind": "derived",
                "storeMode": "virtual",
                "identityKind": "resolve-by-query",
                "derivedFromQuery": DRAFT_WARNING_DERIVED_FROM_QUERY,
            })
        })
        .collect()
}

fn draft_info_objects(report: &WorkflowValidationReport) -> Vec<Value> {
    report
        .issues
        .iter()
        .filter(|issue| issue.severity == "info")
        .map(|issue| {
            json!({
                "kind": "draftInfo",
                "code": issue.code,
                "message": issue.message,
                "subject": issue.subject,
            })
        })
        .collect()
}

fn completeness_gap_kind(code: &str) -> String {
    match code {
        "wf:phase.missing" => "missing-phase".to_string(),
        "wf:scriptBlock" | "wf:scriptSha256" => "unbound-source".to_string(),
        "wf:phaseIndex.unresolved" => "unreachable-node".to_string(),
        "wf:name" => "missing-name".to_string(),
        "wf:description" => "missing-description".to_string(),
        "dcterms:title.missing" => "phase-title-missing".to_string(),
        _ => normalized_issue_kind(code),
    }
}

fn normalized_issue_kind(code: &str) -> String {
    let local = code
        .rsplit_once(':')
        .map(|(_, local)| local)
        .unwrap_or(code);
    let mut normalized = String::new();
    let mut previous_dash = false;
    for ch in local.chars() {
        if ch.is_ascii_alphanumeric() {
            normalized.push(ch.to_ascii_lowercase());
            previous_dash = false;
        } else if !previous_dash {
            normalized.push('-');
            previous_dash = true;
        }
    }
    let normalized = normalized.trim_matches('-').to_string();
    if normalized.is_empty() {
        "validation-issue".to_string()
    } else {
        normalized
    }
}

fn virtual_draft_uri(workflow: &WorkflowSummary) -> String {
    format!(
        "urn:sophia:wf:virtual:draft:{}",
        choice_suffix(&workflow.uri)
    )
}

fn virtual_draft_issue_uri(
    report: &WorkflowValidationReport,
    issue_kind: &str,
    issue: &WorkflowValidationIssue,
) -> String {
    let target = issue
        .subject
        .as_deref()
        .unwrap_or(report.workflow_uri.as_str());
    format!(
        "urn:sophia:wf:virtual:draft:{}:{}:{}",
        choice_suffix(&report.workflow_uri),
        issue_kind,
        choice_suffix(&format!("{}:{target}", issue.code))
    )
}

fn workflow_scene(workflow: &WorkflowSummary) -> String {
    let latest = workflow
        .runs
        .first()
        .and_then(|run| run.status.as_deref())
        .map(|status| format!(" Latest run status is '{status}'."))
        .unwrap_or_else(|| " No runs are recorded yet.".to_string());
    format!(
        "This workflow has {} phase(s), {} agent node(s), and {} recorded run(s).{}",
        workflow.phases.len(),
        workflow.nodes.len(),
        workflow.runs.len(),
        latest
    )
}

fn overview_objects(snapshot: &WorkflowBookSnapshot, workflow: &WorkflowSummary) -> Vec<Value> {
    let mut objects = vec![
        workflow_object(workflow),
        run_statistics_object(snapshot, workflow),
    ];
    objects.extend(workflow.phases.iter().map(|phase| {
        phase_object(
            phase,
            workflow
                .nodes
                .iter()
                .filter(|node| node.phase_index == phase.order)
                .count(),
        )
    }));
    objects.extend(workflow.nodes.iter().map(agent_object));
    if let Some(script_block) = &workflow.script_block {
        objects.push(source_block_object(script_block));
    }
    if let Some(input_block) = &workflow.input_block {
        objects.push(input_block_object(input_block));
    }
    if let Some(run) = workflow.runs.first() {
        objects.push(run_object(run));
    }
    objects
}

fn phase_objects(phase: &WorkflowPhase, nodes: &[&WorkflowAgentNode]) -> Vec<Value> {
    let mut objects = vec![phase_object(phase, nodes.len())];
    objects.extend(nodes.iter().map(|node| agent_object(node)));
    objects
}

fn agent_objects(node: &WorkflowAgentNode) -> Vec<Value> {
    let mut objects = vec![agent_object(node)];
    if let Some(doc_id) = &node.doc_id {
        objects.push(json!({
            "kind": "document",
            "documentId": doc_id,
            "role": "agentNodeDocument",
        }));
    }
    objects
}

fn validation_objects(report: &WorkflowValidationReport) -> Vec<Value> {
    let mut objects = vec![json!({
        "kind": "validationReport",
        "workflowName": report.workflow_name,
        "passed": report.passed,
        "errors": report.errors,
        "warnings": report.warnings,
        "infos": report.infos,
        "summary": report.summary,
    })];
    objects.extend(report.issues.iter().map(|issue| {
        json!({
            "kind": "validationIssue",
            "severity": issue.severity,
            "code": issue.code,
            "message": issue.message,
            "subject": issue.subject,
        })
    }));
    objects
}

fn run_statistics_object(snapshot: &WorkflowBookSnapshot, workflow: &WorkflowSummary) -> Value {
    let durations = workflow
        .runs
        .iter()
        .filter_map(|run| run.duration_ms)
        .collect::<Vec<_>>();
    let tokens = workflow
        .runs
        .iter()
        .filter_map(|run| run.total_tokens)
        .collect::<Vec<_>>();
    let branch_frequency_rows = branch_frequency_rows(snapshot, workflow);
    let branch_frequency = branch_frequency_rows
        .iter()
        .filter_map(branch_frequency_literal)
        .collect::<Vec<_>>();
    json!({
        "kind": "runStatistics",
        "semanticClass": "wf:RunStatistics",
        "uri": virtual_run_statistics_uri(workflow),
        "definitionSubject": workflow.uri,
        "workflowName": workflow.name,
        "runCount": workflow.runs.len(),
        "lastRunAt": workflow.runs.first().and_then(|run| run.started_at.as_deref()),
        "latestRunStatus": workflow.runs.first().and_then(|run| run.status.as_deref()),
        "medianDurationMs": median_usize(durations),
        "medianRunTokens": median_usize(tokens),
        "branchFrequency": branch_frequency,
        "branchFrequencyRows": branch_frequency_rows,
        "nodeReliability": Vec::<Value>::new(),
        "nodeReliabilityStatus": "unavailable",
        "nodeReliabilityRationale": "wf:AgentRun acceptance/rejection verdicts are not present in the workflow-book snapshot yet.",
        "sourceKind": "derived",
        "storeMode": "virtual",
        "identityKind": "resolve-by-query",
        "derivedFromQuery": RUN_STATISTICS_DERIVED_FROM_QUERY,
    })
}

fn branch_frequency_rows(
    snapshot: &WorkflowBookSnapshot,
    workflow: &WorkflowSummary,
) -> Vec<Value> {
    let mut counts = BTreeMap::<String, (usize, Option<String>)>::new();
    for decision in snapshot.decisions.iter().filter(|decision| {
        decision
            .workflow_name
            .as_deref()
            .is_some_and(|name| name == workflow.name.as_str())
    }) {
        let Some(route_id) = decision
            .followed_route
            .as_deref()
            .or(decision.followed_choice.as_deref())
        else {
            continue;
        };
        let entry = counts.entry(route_id.to_string()).or_insert((0, None));
        entry.0 += 1;
        if entry.1.is_none() {
            entry.1 = decision
                .followed_route_label
                .clone()
                .or_else(|| decision.followed_choice_label.clone());
        }
    }
    let mut items = counts
        .into_iter()
        .map(|(route_id, (count, route_label))| {
            json!({
                "routeId": route_id,
                "routeLabel": route_label,
                "count": count,
            })
        })
        .collect::<Vec<_>>();
    items.sort_by(|left, right| {
        let left_count = left.get("count").and_then(Value::as_u64).unwrap_or(0);
        let right_count = right.get("count").and_then(Value::as_u64).unwrap_or(0);
        right_count.cmp(&left_count).then_with(|| {
            left.get("routeId")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .cmp(
                    right
                        .get("routeId")
                        .and_then(Value::as_str)
                        .unwrap_or_default(),
                )
        })
    });
    items
}

fn branch_frequency_literal(row: &Value) -> Option<String> {
    let route_id = row.get("routeId").and_then(Value::as_str)?;
    let count = row.get("count").and_then(Value::as_u64)?;
    Some(format!("{route_id}={count}"))
}

fn median_usize(mut values: Vec<usize>) -> Option<usize> {
    if values.is_empty() {
        return None;
    }
    values.sort_unstable();
    let mid = values.len() / 2;
    if values.len() % 2 == 1 {
        Some(values[mid])
    } else {
        Some((values[mid - 1] + values[mid]) / 2)
    }
}

fn virtual_run_statistics_uri(workflow: &WorkflowSummary) -> String {
    format!(
        "urn:sophia:wf:virtual:run-statistics:{}",
        choice_suffix(&workflow.uri)
    )
}

fn execute_objects(
    workflow: &WorkflowSummary,
    source_block: Option<&DocBlockRef>,
    input_block: Option<&DocBlockRef>,
    run_body: &Value,
) -> Vec<Value> {
    let mut objects = vec![
        workflow_object(workflow),
        json!({
            "kind": "endpoint",
            "id": "facadeSubmitRun",
            "method": "POST",
            "path": "/workflows/runs",
            "requiredScope": "services.proxy",
        }),
        json!({
            "kind": "endpoint",
            "id": "upstreamSubmitRun",
            "serviceId": "choreograph",
            "method": "POST",
            "path": "/api/workflows/run",
        }),
        json!({
            "kind": "endpoint",
            "id": "runEvents",
            "method": "GET",
            "path": "/workflows/runs/{runId}/events",
        }),
        json!({
            "kind": "runRequest",
            "body": run_body,
        }),
    ];
    if let Some(script_block) = &workflow.script_block {
        objects.push(source_block_object(script_block));
    }
    if let Some(input_block_uri) = &workflow.input_block {
        objects.push(input_block_object(input_block_uri));
    }
    if let Some(source_block) = source_block {
        objects.push(json!({
            "kind": "sourceBlock",
            "documentId": source_block.document_id,
            "blockId": source_block.block_id,
            "parsed": true,
        }));
    }
    if let Some(input_block) = input_block {
        objects.push(json!({
            "kind": "inputBlock",
            "documentId": input_block.document_id,
            "blockId": input_block.block_id,
            "parsed": true,
        }));
    }
    if let Some(run) = workflow.runs.first() {
        objects.push(run_object(run));
    }
    objects
}

fn perception_objects(workflow: &WorkflowSummary) -> Vec<Value> {
    let mut objects = vec![json!({
        "kind": "perceptionFrame",
        "focus": "workflow",
        "workflowName": workflow.name,
        "workflowUri": workflow.uri,
        "percepts": [
            "workflow-anatomy",
            "bound-blocks",
            "run-history"
        ],
        "conceptCount": workflow.phases.len() + workflow.nodes.len(),
    })];
    objects.push(workflow_object(workflow));
    objects.extend(workflow.phases.iter().map(|phase| {
        json!({
            "kind": "concept",
            "conceptKind": "phase",
            "label": phase.title,
            "phaseOrder": phase.order,
            "pageId": phase_page_id(phase.order),
        })
    }));
    objects.extend(workflow.nodes.iter().map(|node| {
        json!({
            "kind": "concept",
            "conceptKind": "agentNode",
            "label": node.label,
            "phaseIndex": node.phase_index,
            "agentType": node.agent_type,
            "pageId": agent_page_id(&node.label),
        })
    }));
    if let Some(script_block) = &workflow.script_block {
        objects.push(source_block_object(script_block));
    }
    if let Some(input_block) = &workflow.input_block {
        objects.push(input_block_object(input_block));
    }
    objects
}

fn raw_objects(snapshot: &WorkflowBookSnapshot, workflow: Option<&WorkflowSummary>) -> Vec<Value> {
    let mut objects = vec![
        json!({"kind": "rdfClass", "curie": "wf:Workflow", "iri": format!("{WF_NS}Workflow")}),
        json!({"kind": "rdfClass", "curie": "wf:Phase", "iri": format!("{WF_NS}Phase")}),
        json!({"kind": "rdfClass", "curie": "wf:AgentNode", "iri": format!("{WF_NS}AgentNode")}),
        json!({"kind": "rdfClass", "curie": "wf:Run", "iri": format!("{WF_NS}Run")}),
        json!({"kind": "rdfClass", "curie": "wf:PageView", "iri": format!("{WF_NS}PageView")}),
        json!({"kind": "rdfClass", "curie": "wf:RawSparqlQuery", "iri": format!("{WF_NS}RawSparqlQuery")}),
        json!({"kind": "rdfClass", "curie": "wf:RouteAction", "iri": format!("{WF_NS}RouteAction")}),
        json!({"kind": "rdfClass", "curie": "wf:RecommendedAction", "iri": format!("{WF_NS}RecommendedAction")}),
        json!({"kind": "rdfClass", "curie": "wf:AuthorizationFlag", "iri": format!("{WF_NS}AuthorizationFlag")}),
        json!({"kind": "rdfClass", "curie": "wf:PageTurnDecision", "iri": format!("{WF_NS}PageTurnDecision")}),
        json!({"kind": "rdfClass", "curie": "wf:EvidenceArtifact", "iri": format!("{WF_NS}EvidenceArtifact")}),
        json!({"kind": "rdfClass", "curie": "wfui:WorkflowAdventurePacket", "iri": format!("{WFUI_NS}WorkflowAdventurePacket")}),
        json!({"kind": "rdfClass", "curie": "wfui:PageTurnDecision", "iri": format!("{WFUI_NS}PageTurnDecision")}),
        json!({"kind": "rdfClass", "curie": "wfui:NavigationRoute", "iri": format!("{WFUI_NS}NavigationRoute")}),
        json!({"kind": "rdfClass", "curie": "wfui:EvidenceArtifact", "iri": format!("{WFUI_NS}EvidenceArtifact")}),
        json!({"kind": "rdfClass", "curie": "wfui:AuthorizationFlag", "iri": format!("{WFUI_NS}AuthorizationFlag")}),
    ];
    if let Some(workflow) = workflow {
        objects.push(workflow_object(workflow));
    }
    objects.extend(
        adventures_for_workflow(snapshot, workflow)
            .into_iter()
            .map(adventure_object),
    );
    objects.extend(
        decisions_for_workflow(snapshot, workflow)
            .into_iter()
            .map(decision_object),
    );
    objects.extend(raw_queries(snapshot, workflow).into_iter().map(|query| {
        json!({
            "kind": "sparqlQuery",
            "title": query.get("title").cloned().unwrap_or(Value::Null),
            "tool": query.get("tool").cloned().unwrap_or(Value::Null),
            "arguments": query.get("arguments").cloned().unwrap_or(Value::Null),
        })
    }));
    objects
}

fn adventures_for_workflow<'a>(
    snapshot: &'a WorkflowBookSnapshot,
    workflow: Option<&WorkflowSummary>,
) -> Vec<&'a WorkflowAdventurePacket> {
    let Some(workflow) = workflow else {
        return snapshot.adventures.iter().collect();
    };
    snapshot
        .adventures
        .iter()
        .filter(|adventure| adventure.workflow_name.as_deref() == Some(workflow.name.as_str()))
        .collect()
}

fn decisions_for_workflow<'a>(
    snapshot: &'a WorkflowBookSnapshot,
    workflow: Option<&WorkflowSummary>,
) -> Vec<&'a WorkflowPageTurnDecision> {
    let Some(workflow) = workflow else {
        return snapshot.decisions.iter().collect();
    };
    snapshot
        .decisions
        .iter()
        .filter(|decision| decision.workflow_name.as_deref() == Some(workflow.name.as_str()))
        .collect()
}

fn adventure_title(adventure: &WorkflowAdventurePacket) -> String {
    let scope = [
        adventure.workflow_name.as_deref(),
        adventure.page_id.as_deref(),
        adventure.intent.as_deref(),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join(" / ");
    if scope.is_empty() {
        "Workflow PageView".to_string()
    } else {
        scope
    }
}

fn decision_title(decision: &WorkflowPageTurnDecision) -> String {
    let scope = [
        decision.workflow_name.as_deref(),
        decision.from_page.as_deref(),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join(" / ");
    let turn = decision
        .recommended_route
        .as_deref()
        .or(decision.recommended_choice.as_deref())
        .or(decision.native_suggested_choice.as_deref())
        .unwrap_or("decision");
    if scope.is_empty() {
        turn.to_string()
    } else {
        format!("{scope} -> {turn}")
    }
}

fn adventure_object(adventure: &WorkflowAdventurePacket) -> Value {
    json!({
        "kind": "workflowPageView",
        "compatKind": "workflowAdventurePacket",
        "retainedClass": adventure_retained_class(adventure),
        "subject": adventure.subject,
        "supersededPageView": adventure.superseded_page_view,
        "pageIdForBook": adventure_page_id(&adventure.subject),
        "generatedAt": adventure.generated_at,
        "graphId": adventure.graph_id,
        "workflowName": adventure.workflow_name,
        "pageId": adventure.page_id,
        "pageTitle": adventure.page_title,
        "intent": adventure.intent,
        "recommendedRoute": adventure.recommended_route,
        "recommendedRouteLabel": adventure.recommended_route_label,
        "recommendedChoice": adventure.recommended_choice,
        "visibleObjectCount": adventure.visible_object_count,
        "warningCount": adventure.warning_count,
        "navigationRouteCount": adventure.navigation_routes.len(),
        "authorizationFlagCount": adventure.authorization_flag_count,
        "authorizationFlagsVisible": adventure.authorization_flags.len(),
        "rawSparqlQueryCount": adventure_raw_sparql_count(adventure),
    })
}

fn adventure_retained_class(adventure: &WorkflowAdventurePacket) -> &'static str {
    if adventure.superseded_page_view.is_some()
        || adventure.subject.starts_with("urn:sophia:wf:page-view:")
    {
        "wf:PageView"
    } else {
        "wfui:WorkflowAdventurePacket"
    }
}

fn adventure_detail_objects(adventure: &WorkflowAdventurePacket) -> Vec<Value> {
    let mut objects = vec![adventure_object(adventure)];
    objects.extend(adventure.navigation_routes.iter().map(|route| {
        json!({
            "kind": "navigationRoute",
            "subject": route.subject,
            "routeId": route.route_id,
            "routeLabel": route.route_label,
            "routeKind": route.route_kind,
            "intent": route.intent,
            "choiceId": route.choice_id,
            "choiceLabel": route.choice_label,
            "source": route.source,
            "rationale": route.rationale,
            "routeAction": route_action_value(route),
            "actionJson": route.action_json,
        })
    }));
    objects.extend(adventure.authorization_flags.iter().map(|flag| {
        json!({
            "kind": "authorizationFlag",
            "subject": flag.subject,
            "pageId": flag.page_id,
            "choiceId": flag.choice_id,
            "reason": flag.reason,
        })
    }));
    objects
}

fn decision_object(decision: &WorkflowPageTurnDecision) -> Value {
    json!({
        "kind": "pageTurnDecision",
        "subject": decision.subject,
        "pageId": decision_page_id(&decision.subject),
        "generatedAt": decision.generated_at,
        "graphId": decision.graph_id,
        "workflowName": decision.workflow_name,
        "fromPage": decision.from_page,
        "intent": decision.intent,
        "readiness": decision.readiness,
        "nativeSuggestedChoice": decision.native_suggested_choice,
        "recommendedRoute": decision.recommended_route,
        "recommendedRouteLabel": decision.recommended_route_label,
        "recommendedChoice": decision.recommended_choice,
        "followedRoute": decision.followed_route,
        "followedRouteLabel": decision.followed_route_label,
        "followedChoice": decision.followed_choice,
        "followedChoiceLabel": decision.followed_choice_label,
        "executionAuthorized": decision.execution_authorized,
        "authorizationFlagCount": decision.authorization_flag_count,
        "evidenceCount": decision.evidence.len(),
        "navigationRouteCount": decision.navigation_routes.len(),
        "authorizationFlagsVisible": decision.authorization_flags.len(),
    })
}

fn decision_detail_objects(decision: &WorkflowPageTurnDecision) -> Vec<Value> {
    let mut objects = vec![decision_object(decision)];
    objects.extend(decision.evidence.iter().map(|evidence| {
        json!({
            "kind": "evidenceArtifact",
            "subject": evidence.subject,
            "role": evidence.role,
            "path": evidence.path,
        })
    }));
    objects.extend(decision.navigation_routes.iter().map(|route| {
        json!({
            "kind": "navigationRoute",
            "subject": route.subject,
            "routeId": route.route_id,
            "routeLabel": route.route_label,
            "routeKind": route.route_kind,
            "intent": route.intent,
            "choiceId": route.choice_id,
            "choiceLabel": route.choice_label,
            "source": route.source,
            "rationale": route.rationale,
            "actionJson": route.action_json,
        })
    }));
    objects.extend(decision.authorization_flags.iter().map(|flag| {
        json!({
            "kind": "authorizationFlag",
            "subject": flag.subject,
            "pageId": flag.page_id,
            "choiceId": flag.choice_id,
            "reason": flag.reason,
        })
    }));
    objects
}

fn decision_launch_objects(
    decision: &WorkflowPageTurnDecision,
    primary_route: Option<&WorkflowPageTurnNavigationRoute>,
) -> Vec<Value> {
    let mut objects = vec![json!({
        "kind": "pageTurnDecisionLaunch",
        "subject": decision.subject,
        "pageId": decision_launch_page_id(&decision.subject),
        "detailPageId": decision_page_id(&decision.subject),
        "workflowName": decision.workflow_name,
        "fromPage": decision.from_page,
        "intent": decision.intent,
        "readiness": decision.readiness,
        "primaryRoute": primary_route.and_then(|route| route.route_id.as_deref()),
        "primaryRouteLabel": primary_route.and_then(|route| route.route_label.as_deref()),
        "primaryChoice": primary_route.and_then(|route| route.choice_id.as_deref()),
        "primaryChoiceLabel": primary_route.and_then(|route| route.choice_label.as_deref()),
        "executionAuthorized": decision.execution_authorized,
        "authorizationFlagCount": decision.authorization_flag_count,
        "routeCount": decision.navigation_routes.len(),
        "evidenceCount": decision.evidence.len(),
    })];
    if let Some(route) = primary_route {
        objects.push(json!({
            "kind": "currentBranch",
            "subject": route.subject,
            "routeId": route.route_id,
            "routeLabel": route.route_label,
            "routeStatus": decision_route_status(decision, route),
            "choiceId": route.choice_id,
            "choiceLabel": route.choice_label,
            "intent": route.intent,
            "rationale": route.rationale,
        }));
    }
    objects
}

fn decision_primary_route(
    decision: &WorkflowPageTurnDecision,
) -> Option<&WorkflowPageTurnNavigationRoute> {
    if let Some(followed_route) = decision.followed_route.as_deref() {
        if let Some(route) = decision.navigation_routes.iter().find(|route| {
            route.route_id.as_deref() == Some(followed_route)
                || route.choice_id.as_deref() == decision.followed_choice.as_deref()
        }) {
            return Some(route);
        }
    }
    if let Some(recommended_route) = decision.recommended_route.as_deref() {
        if let Some(route) = decision.navigation_routes.iter().find(|route| {
            route.route_id.as_deref() == Some(recommended_route)
                || route.choice_id.as_deref() == decision.recommended_choice.as_deref()
        }) {
            return Some(route);
        }
    }
    if let Some(followed_choice) = decision.followed_choice.as_deref() {
        if let Some(route) = decision
            .navigation_routes
            .iter()
            .find(|route| route.choice_id.as_deref() == Some(followed_choice))
        {
            return Some(route);
        }
    }
    if let Some(recommended_choice) = decision.recommended_choice.as_deref() {
        if let Some(route) = decision
            .navigation_routes
            .iter()
            .find(|route| route.choice_id.as_deref() == Some(recommended_choice))
        {
            return Some(route);
        }
    }
    decision.navigation_routes.first()
}

fn decision_route_status(
    decision: &WorkflowPageTurnDecision,
    route: &WorkflowPageTurnNavigationRoute,
) -> &'static str {
    if route.route_id.as_deref() == decision.followed_route.as_deref()
        || route.choice_id.as_deref() == decision.followed_choice.as_deref()
    {
        return "followed";
    }
    if route.route_id.as_deref() == decision.recommended_route.as_deref()
        || route.choice_id.as_deref() == decision.recommended_choice.as_deref()
    {
        return "recommended";
    }
    "available"
}

fn decision_launch_route_item(
    decision: &WorkflowPageTurnDecision,
    route: &WorkflowPageTurnNavigationRoute,
) -> Value {
    json!({
        "subject": route.subject,
        "routeId": route.route_id,
        "routeLabel": route.route_label,
        "routeKind": route.route_kind,
        "routeStatus": decision_route_status(decision, route),
        "intent": route.intent,
        "choiceId": route.choice_id,
        "choiceLabel": route.choice_label,
        "source": route.source,
        "rationale": route.rationale,
        "routeAction": route_action_value(route),
        "actionJson": route.action_json,
    })
}

fn evidence_item(evidence: &WorkflowPageTurnEvidence) -> Value {
    json!({
        "subject": evidence.subject,
        "role": evidence.role,
        "path": evidence.path,
    })
}

fn navigation_route_item(route: &WorkflowPageTurnNavigationRoute) -> Value {
    json!({
        "subject": route.subject,
        "routeId": route.route_id,
        "routeLabel": route.route_label,
        "routeKind": route.route_kind,
        "intent": route.intent,
        "choiceId": route.choice_id,
        "choiceLabel": route.choice_label,
        "source": route.source,
        "rationale": route.rationale,
        "actionJson": route.action_json,
    })
}

fn adventure_primary_route(
    adventure: &WorkflowAdventurePacket,
) -> Option<&WorkflowPageTurnNavigationRoute> {
    if let Some(followed_route) = adventure.followed_route.as_deref() {
        if let Some(route) = adventure
            .navigation_routes
            .iter()
            .find(|route| route.route_id.as_deref() == Some(followed_route))
        {
            return Some(route);
        }
    }
    if let Some(recommended_route) = adventure.recommended_route.as_deref() {
        if let Some(route) = adventure.navigation_routes.iter().find(|route| {
            route.route_id.as_deref() == Some(recommended_route)
                || route.choice_id.as_deref() == adventure.recommended_choice.as_deref()
        }) {
            return Some(route);
        }
    }
    if let Some(recommended_choice) = adventure.recommended_choice.as_deref() {
        if let Some(route) = adventure
            .navigation_routes
            .iter()
            .find(|route| route.choice_id.as_deref() == Some(recommended_choice))
        {
            return Some(route);
        }
    }
    adventure.navigation_routes.first()
}

fn adventure_route_status(
    adventure: &WorkflowAdventurePacket,
    route: &WorkflowPageTurnNavigationRoute,
) -> &'static str {
    if route.route_id.as_deref() == adventure.followed_route.as_deref() {
        return "followed";
    }
    if route.route_id.as_deref() == adventure.recommended_route.as_deref()
        || route.choice_id.as_deref() == adventure.recommended_choice.as_deref()
    {
        return "recommended";
    }
    "available"
}

fn adventure_route_item(
    adventure: &WorkflowAdventurePacket,
    route: &WorkflowPageTurnNavigationRoute,
) -> Value {
    json!({
        "subject": route.subject,
        "routeId": route.route_id,
        "routeLabel": route.route_label,
        "routeKind": route.route_kind,
        "routeStatus": adventure_route_status(adventure, route),
        "intent": route.intent,
        "choiceId": route.choice_id,
        "choiceLabel": route.choice_label,
        "source": route.source,
        "rationale": route.rationale,
        "action": route_action_value(route).unwrap_or_else(|| action_json_value(route.action_json.as_deref())),
        "actionJson": route.action_json,
    })
}

fn route_action_value(route: &WorkflowPageTurnNavigationRoute) -> Option<Value> {
    let has_typed_action = route.route_action.is_some()
        || route.action_tool.is_some()
        || route.action_kind.is_some()
        || route.command_template.is_some()
        || route.argument_hash.is_some()
        || !route.action_arguments.is_empty();
    if !has_typed_action {
        return None;
    }
    Some(json!({
        "subject": route.route_action,
        "tool": route.action_tool,
        "kind": route.action_kind,
        "commandTemplate": route.command_template,
        "argumentHash": route.argument_hash,
        "arguments": route.action_arguments.iter().map(|argument| {
            json!({
                "subject": argument.subject,
                "name": argument.name,
                "value": argument.value,
            })
        }).collect::<Vec<_>>(),
    }))
}

fn authorization_flag_item(flag: &WorkflowAuthorizationFlag) -> Value {
    json!({
        "subject": flag.subject,
        "pageId": flag.page_id,
        "choiceId": flag.choice_id,
        "reason": flag.reason,
    })
}

fn adventure_warnings(adventure: &WorkflowAdventurePacket) -> Vec<String> {
    let mut warnings = Vec::new();
    if !adventure.authorization_flags.is_empty() {
        warnings.push(format!(
            "{} adventure choice(s) require explicit authorization.",
            adventure.authorization_flags.len()
        ));
    }
    if adventure.raw_sparql_json.is_none() && adventure.raw_sparql_queries.is_empty() {
        warnings.push("This retained PageView did not retain raw SPARQL queries.".to_string());
    }
    warnings
}

fn adventure_raw_sparql_count(adventure: &WorkflowAdventurePacket) -> usize {
    if !adventure.raw_sparql_queries.is_empty() {
        return adventure.raw_sparql_queries.len();
    }
    adventure_legacy_raw_sparql_array(adventure).len()
}

fn adventure_raw_sparql_items(
    snapshot: &WorkflowBookSnapshot,
    adventure: &WorkflowAdventurePacket,
) -> Vec<Value> {
    let mut items = if adventure.raw_sparql_queries.is_empty() {
        adventure_legacy_raw_sparql_array(adventure)
            .into_iter()
            .enumerate()
            .map(|(index, item)| {
                let title = item
                    .get("title")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("Adventure raw SPARQL {}", index + 1));
                let query = item
                    .get("query")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                json!({
                    "title": title,
                    "tool": "sparql_query",
                    "arguments": {
                        "graphId": snapshot.graph_id,
                        "query": query,
                    },
                })
            })
            .collect::<Vec<_>>()
    } else {
        let mut queries = adventure
            .raw_sparql_queries
            .iter()
            .collect::<Vec<&WorkflowRawSparqlQuery>>();
        queries.sort_by(|left, right| {
            left.order
                .cmp(&right.order)
                .then_with(|| left.title.cmp(&right.title))
                .then_with(|| left.subject.cmp(&right.subject))
        });
        queries
            .into_iter()
            .enumerate()
            .map(|(index, query)| {
                json!({
                    "title": query.title.clone().unwrap_or_else(|| format!("PageView raw SPARQL {}", index + 1)),
                    "subject": query.subject,
                    "queryOrder": query.order,
                    "tool": "sparql_query",
                    "arguments": {
                        "graphId": query.graph_id.as_deref().unwrap_or(snapshot.graph_id.as_str()),
                        "query": query.query.clone().unwrap_or_default(),
                    },
                })
            })
            .collect::<Vec<_>>()
    };
    items.push(json!({
        "title": "This PageView",
        "tool": "sparql_query",
        "arguments": {
            "graphId": snapshot.graph_id,
            "query": workflow_adventure_detail_query(&snapshot.read_graph, &adventure.subject),
        },
    }));
    items
}

fn adventure_legacy_raw_sparql_array(adventure: &WorkflowAdventurePacket) -> Vec<Value> {
    adventure
        .raw_sparql_json
        .as_deref()
        .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
        .and_then(|value| value.as_array().cloned())
        .unwrap_or_default()
}

fn action_json_value(action_json: Option<&str>) -> Value {
    action_json
        .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
        .unwrap_or(Value::Null)
}

fn decision_warnings(decision: &WorkflowPageTurnDecision) -> Vec<String> {
    let mut warnings = Vec::new();
    if decision.execution_authorized == Some(false) {
        warnings.push(
            "This decision explicitly records that execution was not authorized.".to_string(),
        );
    }
    if !decision.authorization_flags.is_empty() {
        warnings.push(format!(
            "{} execution-capable choice(s) require explicit authorization.",
            decision.authorization_flags.len()
        ));
    }
    warnings
}

fn workflow_object(workflow: &WorkflowSummary) -> Value {
    json!({
        "kind": "workflow",
        "name": workflow.name,
        "description": workflow.description,
        "workflowUri": workflow.uri,
        "documentId": workflow.doc_id,
        "scriptSha256": workflow.script_sha256,
        "scriptBlock": workflow.script_block,
        "inputBlock": workflow.input_block,
        "phaseCount": workflow.phases.len(),
        "agentCount": workflow.nodes.len(),
        "runCount": workflow.runs.len(),
        "compositionEventCount": workflow.composition_events.len(),
        "latestRunStatus": workflow.runs.first().and_then(|run| run.status.as_deref()),
    })
}

fn phase_object(phase: &WorkflowPhase, agent_count: usize) -> Value {
    json!({
        "kind": "phase",
        "order": phase.order,
        "title": phase.title,
        "description": phase.description,
        "phaseUri": phase.uri,
        "agentCount": agent_count,
        "pageId": phase_page_id(phase.order),
    })
}

fn agent_object(node: &WorkflowAgentNode) -> Value {
    json!({
        "kind": "agentNode",
        "label": node.label,
        "phaseIndex": node.phase_index,
        "agentType": node.agent_type,
        "agentNodeUri": node.uri,
        "documentId": node.doc_id,
        "pageId": agent_page_id(&node.label),
    })
}

fn run_object(run: &WorkflowRun) -> Value {
    json!({
        "kind": "workflowRun",
        "runId": run.run_id,
        "status": run.status,
        "startedAt": run.started_at,
        "endedAt": run.ended_at,
        "durationMs": run.duration_ms,
        "totalTokens": run.total_tokens,
        "agentCount": run.agent_count,
        "runUri": run.uri,
    })
}

fn source_block_object(script_block: &str) -> Value {
    let parsed = doc_block_from_uri(script_block);
    json!({
        "kind": "sourceBlock",
        "uri": script_block,
        "documentId": parsed.as_ref().map(|block| block.document_id.as_str()),
        "blockId": parsed.as_ref().map(|block| block.block_id.as_str()),
        "parsed": parsed.is_some(),
    })
}

fn input_block_object(input_block: &str) -> Value {
    let parsed = doc_block_from_uri(input_block);
    json!({
        "kind": "inputBlock",
        "uri": input_block,
        "documentId": parsed.as_ref().map(|block| block.document_id.as_str()),
        "blockId": parsed.as_ref().map(|block| block.block_id.as_str()),
        "parsed": parsed.is_some(),
    })
}

fn report_warning_messages(report: &WorkflowValidationReport) -> Vec<String> {
    report
        .issues
        .iter()
        .filter(|issue| issue.severity == "error" || issue.severity == "warning")
        .map(|issue| format!("{} [{}]: {}", issue.severity, issue.code, issue.message))
        .collect()
}

fn validation_reports(
    snapshot: &WorkflowBookSnapshot,
    workflow_name: Option<&str>,
) -> AppResult<Vec<WorkflowValidationReport>> {
    if let Some(workflow_name) = workflow_name {
        let workflow = snapshot
            .workflows
            .iter()
            .find(|candidate| candidate.name == workflow_name)
            .ok_or_else(|| AppError::not_found(format!("workflow not found: {workflow_name}")))?;
        return Ok(vec![validate_workflow(workflow)]);
    }
    Ok(snapshot.workflows.iter().map(validate_workflow).collect())
}

fn validate_workflow(workflow: &WorkflowSummary) -> WorkflowValidationReport {
    let mut issues = Vec::new();
    push_required(
        &mut issues,
        "wf:name",
        !workflow.name.trim().is_empty(),
        "Workflow must have a non-empty wf:name.",
        Some(workflow.uri.as_str()),
    );
    push_required(
        &mut issues,
        "wf:description",
        workflow
            .description
            .as_deref()
            .is_some_and(|value| !value.trim().is_empty()),
        "Workflow should carry wf:description for agent-facing orientation.",
        Some(workflow.uri.as_str()),
    );
    push_required(
        &mut issues,
        "wf:scriptSha256",
        workflow
            .script_sha256
            .as_deref()
            .is_some_and(|value| !value.trim().is_empty()),
        "Workflow must carry wf:scriptSha256 so Choreograph can verify source identity.",
        Some(workflow.uri.as_str()),
    );
    push_required(
        &mut issues,
        "wf:scriptBlock",
        workflow
            .script_block
            .as_deref()
            .is_some_and(|value| !value.trim().is_empty()),
        "Workflow must point at the script block that produced its source bytes.",
        Some(workflow.uri.as_str()),
    );
    if workflow.phases.is_empty() {
        issues.push(validation_issue(
            "error",
            "wf:phase.missing",
            "Workflow must have at least one phase.",
            Some(workflow.uri.as_str()),
        ));
    }
    if workflow.nodes.is_empty() {
        issues.push(validation_issue(
            "warning",
            "wf:AgentNode.empty",
            "Workflow has no agent nodes; execution may be a no-op or code-only workflow.",
            Some(workflow.uri.as_str()),
        ));
    }

    let mut phase_orders = BTreeSet::new();
    let mut duplicate_phase_orders = BTreeSet::new();
    for phase in &workflow.phases {
        if phase.order == 0 {
            issues.push(validation_issue(
                "error",
                "wf:order.invalid",
                "Phase order must be a positive 1-based integer.",
                Some(phase.uri.as_str()),
            ));
        }
        if !phase_orders.insert(phase.order) {
            duplicate_phase_orders.insert(phase.order);
        }
        if phase.title.trim().is_empty() {
            issues.push(validation_issue(
                "error",
                "dcterms:title.missing",
                "Phase must have a non-empty dcterms:title.",
                Some(phase.uri.as_str()),
            ));
        }
    }
    for order in duplicate_phase_orders {
        issues.push(validation_issue(
            "error",
            "wf:order.duplicate",
            format!("Multiple phases use order {order}."),
            Some(workflow.uri.as_str()),
        ));
    }

    let mut labels = BTreeSet::new();
    let mut duplicate_labels = BTreeSet::new();
    for node in &workflow.nodes {
        if node.label.trim().is_empty() {
            issues.push(validation_issue(
                "error",
                "wf:label.missing",
                "Agent node must have a non-empty wf:label.",
                Some(node.uri.as_str()),
            ));
        }
        if !labels.insert(node.label.clone()) {
            duplicate_labels.insert(node.label.clone());
        }
        if node.phase_index == 0 {
            issues.push(validation_issue(
                "error",
                "wf:phaseIndex.invalid",
                "Agent node phaseIndex must be a positive 1-based integer.",
                Some(node.uri.as_str()),
            ));
        } else if !phase_orders.contains(&node.phase_index) {
            issues.push(validation_issue(
                "error",
                "wf:phaseIndex.unresolved",
                format!(
                    "Agent node '{}' points at missing phase index {}.",
                    node.label, node.phase_index
                ),
                Some(node.uri.as_str()),
            ));
        }
    }
    for label in duplicate_labels {
        issues.push(validation_issue(
            "error",
            "wf:label.duplicate",
            format!("Multiple agent nodes use label '{label}'."),
            Some(workflow.uri.as_str()),
        ));
    }

    if workflow.runs.is_empty() {
        issues.push(validation_issue(
            "info",
            "wf:Run.none",
            "No runs are recorded for this workflow yet.",
            Some(workflow.uri.as_str()),
        ));
    }

    let errors = issues
        .iter()
        .filter(|issue| issue.severity == "error")
        .count();
    let warnings = issues
        .iter()
        .filter(|issue| issue.severity == "warning")
        .count();
    let infos = issues
        .iter()
        .filter(|issue| issue.severity == "info")
        .count();
    WorkflowValidationReport {
        workflow_name: workflow.name.clone(),
        workflow_uri: workflow.uri.clone(),
        passed: errors == 0,
        errors,
        warnings,
        infos,
        issues,
        summary: WorkflowValidationSummary {
            phase_count: workflow.phases.len(),
            agent_count: workflow.nodes.len(),
            run_count: workflow.runs.len(),
        },
    }
}

fn push_required(
    issues: &mut Vec<WorkflowValidationIssue>,
    code: &str,
    ok: bool,
    message: &str,
    subject: Option<&str>,
) {
    if !ok {
        issues.push(validation_issue("error", code, message, subject));
    }
}

fn validation_issue(
    severity: &str,
    code: &str,
    message: impl Into<String>,
    subject: Option<&str>,
) -> WorkflowValidationIssue {
    WorkflowValidationIssue {
        severity: severity.to_string(),
        code: code.to_string(),
        message: message.into(),
        subject: subject.map(str::to_string),
    }
}

fn validation_summary_sentence(report: &WorkflowValidationReport) -> String {
    if report.passed {
        format!(
            "Workflow anatomy passes local validation with {} warning(s). Emporium remains the authoritative write-time validator.",
            report.warnings
        )
    } else {
        format!(
            "Workflow anatomy has {} error(s) and {} warning(s); fix errors before execution.",
            report.errors, report.warnings
        )
    }
}

fn agent_item(node: &WorkflowAgentNode) -> Value {
    json!({
        "label": node.label,
        "phaseIndex": node.phase_index,
        "agentType": node.agent_type,
        "agentNodeUri": node.uri,
        "documentId": node.doc_id,
        "pageId": agent_page_id(&node.label),
    })
}

fn run_item(run: &WorkflowRun) -> Value {
    json!({
        "runId": run.run_id,
        "status": run.status,
        "startedAt": run.started_at,
        "endedAt": run.ended_at,
        "durationMs": run.duration_ms,
        "totalTokens": run.total_tokens,
        "agentCount": run.agent_count,
        "runUri": run.uri,
    })
}

fn workflow_fold_check(workflow: &WorkflowSummary) -> Value {
    let current = current_definition_triples(workflow);
    let folded = folded_definition_triples(&workflow.composition_events);
    let replayable_event_count = workflow
        .composition_events
        .iter()
        .filter(|event| !event.insert_triples.is_empty() || !event.delete_triples.is_empty())
        .count();
    let opaque_event_count = workflow
        .composition_events
        .len()
        .saturating_sub(replayable_event_count);
    let has_create_event = workflow
        .composition_events
        .iter()
        .any(|event| event.gesture_kind == "create_workflow" && !event.insert_triples.is_empty());
    let missing_from_fold = current.difference(&folded).cloned().collect::<Vec<_>>();
    let extra_from_fold = folded.difference(&current).cloned().collect::<Vec<_>>();
    let status = if workflow.composition_events.is_empty() {
        "absent"
    } else if opaque_event_count > 0 || !has_create_event {
        "partial"
    } else if missing_from_fold.is_empty() && extra_from_fold.is_empty() {
        "converged"
    } else {
        "diverged"
    };
    json!({
        "kind": "definitionFoldCheck",
        "workflowName": workflow.name,
        "definitionSubject": workflow.uri,
        "sourceKind": "derived",
        "storeMode": "virtual",
        "derivedFromQuery": "workflow_book.compositionTrail.fold(wf:CompositionEvent.insertTriple/deleteTriple)",
        "status": status,
        "hasCreateEvent": has_create_event,
        "compositionEventCount": workflow.composition_events.len(),
        "replayableEventCount": replayable_event_count,
        "opaqueEventCount": opaque_event_count,
        "currentDefinitionTripleCount": current.len(),
        "foldedDefinitionTripleCount": folded.len(),
        "missingFromFold": missing_from_fold,
        "extraFromFold": extra_from_fold,
    })
}

fn current_definition_triples(workflow: &WorkflowSummary) -> BTreeSet<String> {
    let mut triples = BTreeSet::new();
    triples.insert(format!("<{}> a <{WF_NS}Workflow> .", workflow.uri));
    triples.insert(format!(
        "<{}> <{WF_NS}name> {} .",
        workflow.uri,
        sparql_string_literal(&workflow.name)
    ));
    if let Some(description) = workflow.description.as_deref() {
        triples.insert(format!(
            "<{}> <{WF_NS}description> {} .",
            workflow.uri,
            sparql_string_literal(description)
        ));
    }
    if let Some(when_to_use) = workflow.when_to_use.as_deref() {
        triples.insert(format!(
            "<{}> <{WF_NS}whenToUse> {} .",
            workflow.uri,
            sparql_string_literal(when_to_use)
        ));
    }
    if let Some(script_sha256) = workflow.script_sha256.as_deref() {
        triples.insert(script_sha256_triple(&workflow.uri, script_sha256));
    }
    if let Some(script_block) = workflow.script_block.as_deref() {
        triples.insert(source_block_triple(&workflow.uri, script_block));
    }
    if let Some(input_block) = workflow.input_block.as_deref() {
        triples.insert(input_block_triple(&workflow.uri, input_block));
    }
    for seed in &workflow.seeded_from {
        triples.insert(seeded_from_triple(&workflow.uri, seed));
    }
    for phase in &workflow.phases {
        triples.insert(format!(
            "<{}> <{WF_NS}phase> <{}> .",
            workflow.uri, phase.uri
        ));
        triples.insert(format!("<{}> a <{WF_NS}Phase> .", phase.uri));
        triples.insert(format!(
            "<{}> <{WF_NS}order> {} .",
            phase.uri,
            sparql_usize_literal(phase.order)
        ));
        triples.insert(format!(
            "<{}> <{DCTERMS_NS}title> {} .",
            phase.uri,
            sparql_string_literal(&phase.title)
        ));
        if let Some(description) = phase.description.as_deref() {
            triples.insert(format!(
                "<{}> <{DCTERMS_NS}description> {} .",
                phase.uri,
                sparql_string_literal(description)
            ));
        }
        for seed in &phase.seeded_from {
            triples.insert(seeded_from_triple(&phase.uri, seed));
        }
    }
    for node in &workflow.nodes {
        triples.insert(format!("<{}> a <{WF_NS}AgentNode> .", node.uri));
        triples.insert(format!(
            "<{}> <{WF_NS}partOfWorkflow> <{}> .",
            node.uri, workflow.uri
        ));
        triples.insert(format!(
            "<{}> <{WF_NS}label> {} .",
            node.uri,
            sparql_string_literal(&node.label)
        ));
        triples.insert(format!(
            "<{}> <{WF_NS}phaseIndex> {} .",
            node.uri,
            sparql_usize_literal(node.phase_index)
        ));
        if let Some(agent_type) = node.agent_type.as_deref() {
            triples.insert(format!(
                "<{}> <{WF_NS}agentType> {} .",
                node.uri,
                sparql_string_literal(agent_type)
            ));
        }
        for seed in &node.seeded_from {
            triples.insert(seeded_from_triple(&node.uri, seed));
        }
    }
    triples
}

fn folded_definition_triples(events: &[WorkflowCompositionEvent]) -> BTreeSet<String> {
    let mut triples = BTreeSet::new();
    let mut ordered = events.iter().collect::<Vec<_>>();
    ordered.sort_by(|left, right| {
        left.event_order
            .cmp(&right.event_order)
            .then_with(|| left.uri.cmp(&right.uri))
    });
    for event in ordered {
        for delete_triple in normalized_delta_triples(&event.delete_triples) {
            triples.remove(&delete_triple);
        }
        for insert_triple in normalized_delta_triples(&event.insert_triples) {
            triples.insert(insert_triple);
        }
    }
    triples
}

fn composition_event_item(event: &WorkflowCompositionEvent) -> Value {
    json!({
        "gestureKind": event.gesture_kind,
        "eventOrder": event.event_order,
        "generatedAt": event.generated_at,
        "targetSubject": event.target_subject,
        "eventUri": event.uri,
        "authoringSessionUri": event.authoring_session_uri,
        "agentTurnUri": event.agent_turn_uri,
        "driverLease": event.driver_lease,
        "insertTripleCount": event.insert_triples.len(),
        "deleteTripleCount": event.delete_triples.len(),
    })
}

fn composition_event_object(event: &WorkflowCompositionEvent) -> Value {
    json!({
        "kind": "compositionEvent",
        "eventUri": event.uri,
        "authoringSessionUri": event.authoring_session_uri,
        "generatedAt": event.generated_at,
        "eventOrder": event.event_order,
        "gestureKind": event.gesture_kind,
        "definitionSubject": event.definition_subject,
        "targetSubject": event.target_subject,
        "rationale": event.rationale,
        "driverAgent": event.driver_agent,
        "driverLease": event.driver_lease,
        "agentTurnUri": event.agent_turn_uri,
        "insertTriples": event.insert_triples,
        "deleteTriples": event.delete_triples,
    })
}

fn raw_queries(snapshot: &WorkflowBookSnapshot, workflow: Option<&WorkflowSummary>) -> Vec<Value> {
    let mut queries = vec![
        json!({
            "title": "List workflows",
            "tool": "sparql_query",
            "arguments": {
                "graphId": snapshot.graph_id,
                "query": list_workflows_query(&snapshot.read_graph),
            },
        }),
        json!({
            "title": "List workflow nodes",
            "tool": "sparql_query",
            "arguments": {
                "graphId": snapshot.graph_id,
                "query": list_nodes_query(&snapshot.read_graph),
            },
        }),
        json!({
            "title": "List workflow runs",
            "tool": "sparql_query",
            "arguments": {
                "graphId": snapshot.graph_id,
                "query": list_runs_query(&snapshot.read_graph),
            },
        }),
        json!({
            "title": "List workflow adventures",
            "tool": "sparql_query",
            "arguments": {
                "graphId": snapshot.graph_id,
                "query": workflow_adventures_query(
                    &snapshot.read_graph,
                    workflow.map(|workflow| workflow.name.as_str()),
                ),
            },
        }),
        json!({
            "title": "List page-turn decisions",
            "tool": "sparql_query",
            "arguments": {
                "graphId": snapshot.graph_id,
                "query": page_turn_decisions_query(
                    &snapshot.read_graph,
                    workflow.map(|workflow| workflow.name.as_str()),
                ),
            },
        }),
        json!({
            "title": "List composition events",
            "tool": "sparql_query",
            "arguments": {
                "graphId": snapshot.graph_id,
                "query": composition_events_query(
                    &snapshot.read_graph,
                    workflow.map(|workflow| workflow.uri.as_str()),
                ),
            },
        }),
    ];
    if let Some(workflow) = workflow {
        queries.insert(
            0,
            json!({
                "title": "Find this workflow",
                "tool": "sparql_query",
                "arguments": {
                    "graphId": snapshot.graph_id,
                    "query": find_workflow_query(&snapshot.read_graph, &workflow.name),
                },
            }),
        );
    }
    queries
}

fn validation_queries(snapshot: &WorkflowBookSnapshot, workflow: &WorkflowSummary) -> Vec<Value> {
    vec![
        json!({
            "title": "Missing required workflow anatomy",
            "tool": "sparql_query",
            "arguments": {
                "graphId": snapshot.graph_id,
                "query": missing_required_query(&snapshot.read_graph, &workflow.uri),
            },
        }),
        json!({
            "title": "Agent nodes for this workflow",
            "tool": "sparql_query",
            "arguments": {
                "graphId": snapshot.graph_id,
                "query": workflow_nodes_query(&snapshot.read_graph, &workflow.uri),
            },
        }),
    ]
}

fn compose_template_choice(
    id: &str,
    label: &str,
    description: &str,
    arguments_template: Value,
) -> WorkflowBookChoice {
    WorkflowBookChoice {
        id: id.to_string(),
        kind: "mcp-tool-template".to_string(),
        label: label.to_string(),
        description: Some(description.to_string()),
        target_page_id: None,
        action: Some(json!({
            "tool": "workflow_book_compose",
            "argumentsTemplate": arguments_template,
            "requiresEdits": true,
        })),
    }
}

fn perceptual_sparql_choice(
    id: &str,
    label: &str,
    description: &str,
    snapshot: &WorkflowBookSnapshot,
    query: String,
) -> WorkflowBookChoice {
    WorkflowBookChoice {
        id: id.to_string(),
        kind: "perceptual-sparql".to_string(),
        label: label.to_string(),
        description: Some(description.to_string()),
        target_page_id: None,
        action: Some(json!({
            "tool": "sparql_query",
            "arguments": {
                "graphId": snapshot.graph_id,
                "query": query,
            },
        })),
    }
}

fn retained_route_choice(
    snapshot: &WorkflowBookSnapshot,
    workflow: Option<&WorkflowSummary>,
    decision: &WorkflowPageTurnDecision,
    route: &WorkflowPageTurnNavigationRoute,
) -> WorkflowBookChoice {
    let route_id = route.route_id.as_deref().unwrap_or("unknown-route");
    let route_label = route
        .route_label
        .as_deref()
        .or(route.route_id.as_deref())
        .unwrap_or("Retained route");
    let choice_id = route.choice_id.as_deref().unwrap_or("unknown-choice");
    let mut arguments = json!({
        "graphId": snapshot.graph_id,
        "pageId": decision.from_page.as_deref().unwrap_or("overview"),
        "choiceId": choice_id,
        "routeId": route_id,
    });
    if let Some(workflow_name) = workflow
        .map(|workflow| workflow.name.as_str())
        .or(decision.workflow_name.as_deref())
    {
        arguments["workflowName"] = json!(workflow_name);
    }
    if let Some(intent) = route.intent.as_deref().or(decision.intent.as_deref()) {
        arguments["handoffIntent"] = json!(intent);
    }
    WorkflowBookChoice {
        id: format!("route-{}", choice_suffix(route_id)),
        kind: "retained-route".to_string(),
        label: format!("Route: {}", route_label),
        description: Some(route.rationale.clone().unwrap_or_else(|| {
            format!(
                "Replay retained route '{}' through workflow_book_choose.",
                route_id
            )
        })),
        target_page_id: None,
        action: Some(json!({
            "tool": "workflow_book_choose",
            "arguments": arguments,
        })),
    }
}

fn retained_adventure_route_choice(
    adventure: &WorkflowAdventurePacket,
    route: &WorkflowPageTurnNavigationRoute,
) -> WorkflowBookChoice {
    let route_id = route.route_id.as_deref().unwrap_or("unknown-route");
    let route_label = route
        .route_label
        .as_deref()
        .or(route.route_id.as_deref())
        .unwrap_or("Retained adventure route");
    let original_action = route_action_value(route)
        .unwrap_or_else(|| action_json_value(route.action_json.as_deref()));
    let tool = original_action
        .get("tool")
        .and_then(Value::as_str)
        .unwrap_or("retained-action");
    WorkflowBookChoice {
        id: format!("route-{}", choice_suffix(route_id)),
        kind: "retained-adventure-route".to_string(),
        label: format!("Route: {}", route_label),
        description: Some(
            route
                .rationale
                .clone()
                .unwrap_or_else(|| format!("Inspect retained adventure route '{}'.", route_id)),
        ),
        target_page_id: None,
        action: Some(json!({
            "kind": "retainedAdventureRoute",
            "source": adventure_retained_class(adventure),
            "adventureSubject": adventure.subject,
            "routeId": route_id,
            "routeKind": route.route_kind,
            "intent": route.intent,
            "choiceId": route.choice_id,
            "choiceLabel": route.choice_label,
            "tool": tool,
            "requiresExternalExecutor": tool == "workflow_compose_adventure",
            "originalAction": original_action,
            "originalActionJson": route.action_json,
        })),
    }
}

fn recorded_choice_action(
    id: &str,
    label: &str,
    description: &str,
    snapshot: &WorkflowBookSnapshot,
    workflow: Option<&WorkflowSummary>,
    decision: &WorkflowPageTurnDecision,
    choice_id: &str,
) -> WorkflowBookChoice {
    let mut arguments = json!({
        "graphId": snapshot.graph_id,
        "pageId": decision.from_page.as_deref().unwrap_or("overview"),
        "choiceId": choice_id,
    });
    if let Some(workflow_name) = workflow
        .map(|workflow| workflow.name.as_str())
        .or(decision.workflow_name.as_deref())
    {
        arguments["workflowName"] = json!(workflow_name);
    }
    WorkflowBookChoice {
        id: id.to_string(),
        kind: "mcp-tool".to_string(),
        label: label.to_string(),
        description: Some(description.to_string()),
        target_page_id: None,
        action: Some(json!({
            "tool": "workflow_book_choose",
            "arguments": arguments,
        })),
    }
}

fn conceptual_route_choice(
    id: &str,
    label: &str,
    target_page_id: &str,
    description: impl Into<String>,
) -> WorkflowBookChoice {
    WorkflowBookChoice {
        id: id.to_string(),
        kind: "conceptual-route".to_string(),
        label: label.to_string(),
        description: Some(description.into()),
        target_page_id: Some(target_page_id.to_string()),
        action: None,
    }
}

fn raw_query_choices(
    snapshot: &WorkflowBookSnapshot,
    workflow: Option<&WorkflowSummary>,
) -> Vec<WorkflowBookChoice> {
    raw_queries(snapshot, workflow)
        .into_iter()
        .map(|query| {
            let title = query
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or("Run SPARQL")
                .to_string();
            let tool = query
                .get("tool")
                .and_then(Value::as_str)
                .unwrap_or("sparql_query")
                .to_string();
            let arguments = query.get("arguments").cloned().unwrap_or_else(|| {
                json!({
                    "graphId": snapshot.graph_id,
                })
            });
            WorkflowBookChoice {
                id: format!("sparql-{}", choice_suffix(&title)),
                kind: "raw-sparql".to_string(),
                label: title,
                description: Some(
                    "Run or adapt this SPARQL query against the Garden graph.".to_string(),
                ),
                target_page_id: None,
                action: Some(json!({
                    "tool": tool,
                    "arguments": arguments,
                })),
            }
        })
        .collect()
}

fn navigate_choice(id: &str, label: &str, target_page_id: &str) -> WorkflowBookChoice {
    navigate_choice_with_description(
        id,
        label,
        target_page_id,
        format!("Open the '{label}' page."),
    )
}

fn navigate_choice_with_description(
    id: &str,
    label: &str,
    target_page_id: &str,
    description: impl Into<String>,
) -> WorkflowBookChoice {
    WorkflowBookChoice {
        id: id.to_string(),
        kind: "navigate".to_string(),
        label: label.to_string(),
        description: Some(description.into()),
        target_page_id: Some(target_page_id.to_string()),
        action: None,
    }
}

fn phase_page_id(order: usize) -> String {
    format!("phase-{order}")
}

fn agent_page_id(label: &str) -> String {
    format!("agent-{}", choice_suffix(label))
}

fn decision_page_id(subject: &str) -> String {
    format!("decision-{}", choice_suffix(subject))
}

fn adventure_page_id(subject: &str) -> String {
    format!("adventure-{}", choice_suffix(subject))
}

fn decision_launch_page_id(subject: &str) -> String {
    format!("decision-launch-{}", choice_suffix(subject))
}

fn choice_suffix(value: &str) -> String {
    let mut out = String::new();
    let mut last_dash = false;
    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            last_dash = false;
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
    }
    let trimmed = out.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "item".to_string()
    } else {
        trimmed
    }
}

fn uri_tail_suffix(uri: &str) -> String {
    let tail = uri
        .rsplit(['/', '#', ':'])
        .find(|part| !part.trim().is_empty())
        .unwrap_or(uri);
    choice_suffix(tail)
}

fn list_workflows_query(read_graph: &str) -> String {
    format!(
        "PREFIX wf: <{WF_NS}>\nSELECT ?workflow ?name ?sha WHERE {{\n  GRAPH <{read_graph}> {{\n    ?workflow a wf:Workflow ; wf:name ?name .\n    OPTIONAL {{ ?workflow wf:scriptSha256 ?sha }}\n  }}\n}}"
    )
}

fn list_nodes_query(read_graph: &str) -> String {
    format!(
        "PREFIX wf: <{WF_NS}>\nSELECT ?workflow ?node ?label ?phaseIndex WHERE {{\n  GRAPH <{read_graph}> {{\n    ?node a wf:AgentNode ; wf:partOfWorkflow ?workflow ; wf:label ?label ; wf:phaseIndex ?phaseIndex .\n  }}\n}}"
    )
}

fn list_runs_query(read_graph: &str) -> String {
    format!(
        "PREFIX wf: <{WF_NS}>\nPREFIX prov: <{PROV_NS}>\nSELECT ?run ?workflow ?workflowName ?runId ?status WHERE {{\n  GRAPH <{read_graph}> {{\n    ?run a wf:Run ; wf:workflowName ?workflowName ; wf:runId ?runId .\n    OPTIONAL {{ ?run prov:used ?workflow }}\n    OPTIONAL {{ ?run wf:status ?status }}\n  }}\n}}"
    )
}

fn composition_events_query(read_graph: &str, workflow_uri: Option<&str>) -> String {
    let workflow_filter = workflow_uri
        .map(|uri| format!("\n    FILTER(?definitionSubject = <{uri}>)"))
        .unwrap_or_default();
    format!(
        "PREFIX wf: <{WF_NS}>\nPREFIX prov: <{PROV_NS}>\nSELECT ?session ?sessionDriverLease ?event ?definitionSubject ?targetSubject ?generatedAt ?eventOrder ?gestureKind ?rationale ?driverAgent ?agentTurn ?insertTriple ?deleteTriple WHERE {{\n  GRAPH <{read_graph}> {{\n    ?event a wf:CompositionEvent ;\n      wf:partOfAuthoringSession ?session ;\n      wf:definitionSubject ?definitionSubject ;\n      wf:eventOrder ?eventOrder ;\n      wf:gestureKind ?gestureKind .\n    OPTIONAL {{ ?session wf:driverLease ?sessionDriverLease }}\n    OPTIONAL {{ ?event prov:generatedAtTime ?generatedAt }}\n    OPTIONAL {{ ?event wf:targetSubject ?targetSubject }}\n    OPTIONAL {{ ?event wf:rationale ?rationale }}\n    OPTIONAL {{ ?event wf:driverAgent ?driverAgent }}\n    OPTIONAL {{ ?event wf:boundToAgent ?agentTurn }}\n    OPTIONAL {{ ?event wf:insertTriple ?insertTriple }}\n    OPTIONAL {{ ?event wf:deleteTriple ?deleteTriple }}{workflow_filter}\n  }}\n}}\nORDER BY ?eventOrder ?event ?insertTriple ?deleteTriple"
    )
}

fn find_workflow_query(read_graph: &str, workflow_name: &str) -> String {
    format!(
        "PREFIX wf: <{WF_NS}>\nSELECT ?workflow ?description ?sha WHERE {{\n  GRAPH <{read_graph}> {{\n    ?workflow a wf:Workflow ; wf:name {} .\n    OPTIONAL {{ ?workflow wf:description ?description }}\n    OPTIONAL {{ ?workflow wf:scriptSha256 ?sha }}\n  }}\n}}",
        sparql_string_literal(workflow_name)
    )
}

fn workflow_nodes_query(read_graph: &str, workflow_uri: &str) -> String {
    format!(
        "PREFIX wf: <{WF_NS}>\nSELECT ?node ?label ?phaseIndex WHERE {{\n  GRAPH <{read_graph}> {{\n    ?node a wf:AgentNode ; wf:partOfWorkflow <{workflow_uri}> ; wf:label ?label ; wf:phaseIndex ?phaseIndex .\n  }}\n}}"
    )
}

fn workflow_blocks_query(read_graph: &str, workflow_uri: &str) -> String {
    format!(
        "PREFIX wf: <{WF_NS}>\nSELECT ?scriptBlock ?inputBlock ?sha WHERE {{\n  GRAPH <{read_graph}> {{\n    <{workflow_uri}> a wf:Workflow .\n    OPTIONAL {{ <{workflow_uri}> wf:scriptBlock ?scriptBlock }}\n    OPTIONAL {{ <{workflow_uri}> wf:inputBlock ?inputBlock }}\n    OPTIONAL {{ <{workflow_uri}> wf:scriptSha256 ?sha }}\n  }}\n}}"
    )
}

fn workflow_runs_for_workflow_query(read_graph: &str, workflow: &WorkflowSummary) -> String {
    format!(
        "PREFIX wf: <{WF_NS}>\nPREFIX prov: <{PROV_NS}>\nSELECT ?run ?runId ?status ?startedAt ?endedAt ?totalTokens WHERE {{\n  GRAPH <{read_graph}> {{\n    ?run a wf:Run ; wf:runId ?runId .\n    {{ ?run prov:used <{}> . }} UNION {{ ?run wf:workflowName {} . }}\n    OPTIONAL {{ ?run wf:status ?status }}\n    OPTIONAL {{ ?run prov:startedAtTime ?startedAt }}\n    OPTIONAL {{ ?run prov:endedAtTime ?endedAt }}\n    OPTIONAL {{ ?run wf:totalTokens ?totalTokens }}\n  }}\n}}\nORDER BY DESC(?startedAt)",
        workflow.uri,
        sparql_string_literal(&workflow.name)
    )
}

fn workflow_adventures_query(read_graph: &str, workflow_name: Option<&str>) -> String {
    let workflow_filter = workflow_name
        .map(|name| {
            format!(
                "\n    FILTER(!BOUND(?workflowName) || ?workflowName = {})",
                sparql_string_literal(name)
            )
        })
        .unwrap_or_default();
    format!(
        "PREFIX wf: <{WF_NS}>\nPREFIX wfui: <{WFUI_NS}>\nPREFIX prov: <{PROV_NS}>\nSELECT ?adventure ?supersededPageView ?generatedAt ?adventureGraphId ?workflowName ?pageId ?pageTitle ?pageScene ?fromPage ?followedRoute ?intent ?recommendedRoute ?recommendedRouteLabel ?recommendedChoice ?visibleObjectCount ?warningCount ?rawSparqlJson ?rawSparqlQuery ?rawSparqlTitle ?rawSparqlText ?rawSparqlOrder ?rawSparqlGraph ?authorizationFlagCount ?navigationRoute ?routeId ?routeLabel ?routeKind ?routeIntent ?routeChoiceId ?routeChoiceLabel ?routeSource ?routeRationale ?routeActionJson ?routeAction ?routeActionTool ?routeActionKind ?routeCommandTemplate ?routeArgumentHash ?routeArgument ?routeArgumentName ?routeArgumentValue ?flag ?flagPageId ?flagChoiceId ?flagReason WHERE {{\n  GRAPH <{read_graph}> {{\n    ?adventure a ?adventureType .\n    VALUES ?adventureType {{ wf:PageView wfui:WorkflowAdventurePacket }}\n    OPTIONAL {{ ?adventure wf:supersedesPageView ?supersededPageView }}\n    OPTIONAL {{ ?adventure prov:generatedAtTime ?generatedAt }}\n    OPTIONAL {{ ?adventure wf:graphId ?wfGraphId }}\n    OPTIONAL {{ ?adventure wfui:graphId ?wfuiGraphId }}\n    BIND(COALESCE(?wfGraphId, ?wfuiGraphId) AS ?adventureGraphId)\n    OPTIONAL {{ ?adventure wf:workflowName ?wfWorkflowName }}\n    OPTIONAL {{ ?adventure wfui:workflowName ?wfuiWorkflowName }}\n    BIND(COALESCE(?wfWorkflowName, ?wfuiWorkflowName) AS ?workflowName)\n    OPTIONAL {{ ?adventure wf:pageId ?wfPageId }}\n    OPTIONAL {{ ?adventure wfui:pageId ?wfuiPageId }}\n    BIND(COALESCE(?wfPageId, ?wfuiPageId) AS ?pageId)\n    OPTIONAL {{ ?adventure wf:pageTitle ?wfPageTitle }}\n    OPTIONAL {{ ?adventure wfui:pageTitle ?wfuiPageTitle }}\n    BIND(COALESCE(?wfPageTitle, ?wfuiPageTitle) AS ?pageTitle)\n    OPTIONAL {{ ?adventure wf:pageScene ?wfPageScene }}\n    OPTIONAL {{ ?adventure wfui:pageScene ?wfuiPageScene }}\n    BIND(COALESCE(?wfPageScene, ?wfuiPageScene) AS ?pageScene)\n    OPTIONAL {{ ?adventure wf:fromPage ?wfFromPage }}\n    OPTIONAL {{ ?adventure wfui:fromPage ?wfuiFromPage }}\n    BIND(COALESCE(?wfFromPage, ?wfuiFromPage) AS ?fromPage)\n    OPTIONAL {{ ?adventure wf:followedRouteId ?wfFollowedRoute }}\n    OPTIONAL {{ ?adventure wfui:followedRouteId ?wfuiFollowedRoute }}\n    BIND(COALESCE(?wfFollowedRoute, ?wfuiFollowedRoute) AS ?followedRoute)\n    OPTIONAL {{ ?adventure wf:intent ?wfIntent }}\n    OPTIONAL {{ ?adventure wfui:intent ?wfuiIntent }}\n    BIND(COALESCE(?wfIntent, ?wfuiIntent) AS ?intent)\n    OPTIONAL {{ ?adventure wf:recommendedAction ?recommendedAction . OPTIONAL {{ ?recommendedAction wf:recommendedRoute ?wfRecommendedRoute }} OPTIONAL {{ ?recommendedAction wf:routeLabel ?wfRecommendedRouteLabel }} OPTIONAL {{ ?recommendedAction wf:recommendedChoice ?wfRecommendedChoice }} }}\n    OPTIONAL {{ ?adventure wfui:recommendedRouteId ?wfuiRecommendedRoute }}\n    OPTIONAL {{ ?adventure wfui:recommendedRouteLabel ?wfuiRecommendedRouteLabel }}\n    OPTIONAL {{ ?adventure wfui:recommendedChoiceId ?wfuiRecommendedChoice }}\n    BIND(COALESCE(?wfRecommendedRoute, ?wfuiRecommendedRoute) AS ?recommendedRoute)\n    BIND(COALESCE(?wfRecommendedRouteLabel, ?wfuiRecommendedRouteLabel) AS ?recommendedRouteLabel)\n    BIND(COALESCE(?wfRecommendedChoice, ?wfuiRecommendedChoice) AS ?recommendedChoice)\n    OPTIONAL {{ ?adventure wf:visibleObjectCount ?wfVisibleObjectCount }}\n    OPTIONAL {{ ?adventure wfui:visibleObjectCount ?wfuiVisibleObjectCount }}\n    BIND(COALESCE(?wfVisibleObjectCount, ?wfuiVisibleObjectCount) AS ?visibleObjectCount)\n    OPTIONAL {{ ?adventure wf:warningCount ?wfWarningCount }}\n    OPTIONAL {{ ?adventure wfui:warningCount ?wfuiWarningCount }}\n    BIND(COALESCE(?wfWarningCount, ?wfuiWarningCount) AS ?warningCount)\n    OPTIONAL {{ ?adventure wfui:rawSparqlJson ?rawSparqlJson }}\n    OPTIONAL {{\n      ?adventure wf:hasRawSparqlQuery ?rawSparqlQuery .\n      OPTIONAL {{ ?rawSparqlQuery wf:queryTitle ?rawSparqlTitle }}\n      OPTIONAL {{ ?rawSparqlQuery wf:queryText ?rawSparqlText }}\n      OPTIONAL {{ ?rawSparqlQuery wf:queryOrder ?rawSparqlOrder }}\n      OPTIONAL {{ ?rawSparqlQuery wf:queryGraph ?rawSparqlGraph }}\n    }}\n    OPTIONAL {{ ?adventure wf:authorizationFlagCount ?wfAuthorizationFlagCount }}\n    OPTIONAL {{ ?adventure wfui:authorizationFlagCount ?wfuiAuthorizationFlagCount }}\n    BIND(COALESCE(?wfAuthorizationFlagCount, ?wfuiAuthorizationFlagCount) AS ?authorizationFlagCount){workflow_filter}\n    OPTIONAL {{\n      {{ ?adventure wf:hasNavigationRoute ?navigationRoute . }} UNION {{ ?adventure wfui:hasNavigationRoute ?navigationRoute . }}\n      OPTIONAL {{ ?navigationRoute wf:routeId ?wfRouteId }}\n      OPTIONAL {{ ?navigationRoute wfui:routeId ?wfuiRouteId }}\n      BIND(COALESCE(?wfRouteId, ?wfuiRouteId) AS ?routeId)\n      OPTIONAL {{ ?navigationRoute wf:routeLabel ?wfRouteLabel }}\n      OPTIONAL {{ ?navigationRoute wfui:routeLabel ?wfuiRouteLabel }}\n      BIND(COALESCE(?wfRouteLabel, ?wfuiRouteLabel) AS ?routeLabel)\n      OPTIONAL {{ ?navigationRoute wf:routeKind ?wfRouteKind }}\n      OPTIONAL {{ ?navigationRoute wfui:routeKind ?wfuiRouteKind }}\n      BIND(COALESCE(?wfRouteKind, ?wfuiRouteKind) AS ?routeKind)\n      OPTIONAL {{ ?navigationRoute wf:intent ?wfRouteIntent }}\n      OPTIONAL {{ ?navigationRoute wfui:intent ?wfuiRouteIntent }}\n      BIND(COALESCE(?wfRouteIntent, ?wfuiRouteIntent) AS ?routeIntent)\n      OPTIONAL {{ ?navigationRoute wf:choiceId ?wfRouteChoiceId }}\n      OPTIONAL {{ ?navigationRoute wfui:choiceId ?wfuiRouteChoiceId }}\n      BIND(COALESCE(?wfRouteChoiceId, ?wfuiRouteChoiceId) AS ?routeChoiceId)\n      OPTIONAL {{ ?navigationRoute wf:choiceLabel ?wfRouteChoiceLabel }}\n      OPTIONAL {{ ?navigationRoute wfui:choiceLabel ?wfuiRouteChoiceLabel }}\n      BIND(COALESCE(?wfRouteChoiceLabel, ?wfuiRouteChoiceLabel) AS ?routeChoiceLabel)\n      OPTIONAL {{ ?navigationRoute wf:source ?wfRouteSource }}\n      OPTIONAL {{ ?navigationRoute wfui:source ?wfuiRouteSource }}\n      BIND(COALESCE(?wfRouteSource, ?wfuiRouteSource) AS ?routeSource)\n      OPTIONAL {{ ?navigationRoute wf:rationale ?wfRouteRationale }}\n      OPTIONAL {{ ?navigationRoute wfui:rationale ?wfuiRouteRationale }}\n      BIND(COALESCE(?wfRouteRationale, ?wfuiRouteRationale) AS ?routeRationale)\n      OPTIONAL {{ ?navigationRoute wfui:actionJson ?routeActionJson }}\n      OPTIONAL {{\n        ?navigationRoute wf:routeAction ?routeAction .\n        OPTIONAL {{ ?routeAction wf:actionTool ?routeActionTool }}\n        OPTIONAL {{ ?routeAction wf:actionKind ?routeActionKind }}\n        OPTIONAL {{ ?routeAction wf:commandTemplate ?routeCommandTemplate }}\n        OPTIONAL {{ ?routeAction wf:argumentHash ?routeArgumentHash }}\n        OPTIONAL {{\n          ?routeAction wf:argument ?routeArgument .\n          OPTIONAL {{ ?routeArgument wf:argumentName ?routeArgumentName }}\n          OPTIONAL {{ ?routeArgument wf:argumentValue ?routeArgumentValue }}\n        }}\n      }}\n    }}\n    OPTIONAL {{\n      {{ ?adventure wf:hasAuthorizationFlag ?flag . }} UNION {{ ?adventure wfui:hasAuthorizationFlag ?flag . }}\n      OPTIONAL {{ ?flag wf:pageId ?wfFlagPageId }}\n      OPTIONAL {{ ?flag wfui:pageId ?wfuiFlagPageId }}\n      BIND(COALESCE(?wfFlagPageId, ?wfuiFlagPageId) AS ?flagPageId)\n      OPTIONAL {{ ?flag wf:choiceId ?wfFlagChoiceId }}\n      OPTIONAL {{ ?flag wfui:choiceId ?wfuiFlagChoiceId }}\n      BIND(COALESCE(?wfFlagChoiceId, ?wfuiFlagChoiceId) AS ?flagChoiceId)\n      OPTIONAL {{ ?flag wf:authorizationReason ?wfFlagReason }}\n      OPTIONAL {{ ?flag wfui:authorizationReason ?wfuiFlagReason }}\n      BIND(COALESCE(?wfFlagReason, ?wfuiFlagReason) AS ?flagReason)\n    }}\n  }}\n}}\nORDER BY DESC(?generatedAt) ?adventure ?rawSparqlOrder ?routeId ?routeArgumentName ?flagPageId ?flagChoiceId"
    )
}

fn workflow_adventure_detail_query(read_graph: &str, subject: &str) -> String {
    format!(
        "PREFIX wf: <{WF_NS}>\nPREFIX wfui: <{WFUI_NS}>\nPREFIX prov: <{PROV_NS}>\nSELECT ?adventure ?supersededPageView ?generatedAt ?adventureGraphId ?workflowName ?pageId ?pageTitle ?pageScene ?fromPage ?followedRoute ?intent ?recommendedRoute ?recommendedRouteLabel ?recommendedChoice ?visibleObjectCount ?warningCount ?rawSparqlJson ?rawSparqlQuery ?rawSparqlTitle ?rawSparqlText ?rawSparqlOrder ?rawSparqlGraph ?authorizationFlagCount ?navigationRoute ?routeId ?routeLabel ?routeKind ?routeIntent ?routeChoiceId ?routeChoiceLabel ?routeSource ?routeRationale ?routeActionJson ?routeAction ?routeActionTool ?routeActionKind ?routeCommandTemplate ?routeArgumentHash ?routeArgument ?routeArgumentName ?routeArgumentValue ?flag ?flagPageId ?flagChoiceId ?flagReason WHERE {{\n  GRAPH <{read_graph}> {{\n    BIND(<{subject}> AS ?adventure)\n    ?adventure a ?adventureType .\n    VALUES ?adventureType {{ wf:PageView wfui:WorkflowAdventurePacket }}\n    OPTIONAL {{ ?adventure wf:supersedesPageView ?supersededPageView }}\n    OPTIONAL {{ ?adventure prov:generatedAtTime ?generatedAt }}\n    OPTIONAL {{ ?adventure wf:graphId ?wfGraphId }}\n    OPTIONAL {{ ?adventure wfui:graphId ?wfuiGraphId }}\n    BIND(COALESCE(?wfGraphId, ?wfuiGraphId) AS ?adventureGraphId)\n    OPTIONAL {{ ?adventure wf:workflowName ?wfWorkflowName }}\n    OPTIONAL {{ ?adventure wfui:workflowName ?wfuiWorkflowName }}\n    BIND(COALESCE(?wfWorkflowName, ?wfuiWorkflowName) AS ?workflowName)\n    OPTIONAL {{ ?adventure wf:pageId ?wfPageId }}\n    OPTIONAL {{ ?adventure wfui:pageId ?wfuiPageId }}\n    BIND(COALESCE(?wfPageId, ?wfuiPageId) AS ?pageId)\n    OPTIONAL {{ ?adventure wf:pageTitle ?wfPageTitle }}\n    OPTIONAL {{ ?adventure wfui:pageTitle ?wfuiPageTitle }}\n    BIND(COALESCE(?wfPageTitle, ?wfuiPageTitle) AS ?pageTitle)\n    OPTIONAL {{ ?adventure wf:pageScene ?wfPageScene }}\n    OPTIONAL {{ ?adventure wfui:pageScene ?wfuiPageScene }}\n    BIND(COALESCE(?wfPageScene, ?wfuiPageScene) AS ?pageScene)\n    OPTIONAL {{ ?adventure wf:fromPage ?wfFromPage }}\n    OPTIONAL {{ ?adventure wfui:fromPage ?wfuiFromPage }}\n    BIND(COALESCE(?wfFromPage, ?wfuiFromPage) AS ?fromPage)\n    OPTIONAL {{ ?adventure wf:followedRouteId ?wfFollowedRoute }}\n    OPTIONAL {{ ?adventure wfui:followedRouteId ?wfuiFollowedRoute }}\n    BIND(COALESCE(?wfFollowedRoute, ?wfuiFollowedRoute) AS ?followedRoute)\n    OPTIONAL {{ ?adventure wf:intent ?wfIntent }}\n    OPTIONAL {{ ?adventure wfui:intent ?wfuiIntent }}\n    BIND(COALESCE(?wfIntent, ?wfuiIntent) AS ?intent)\n    OPTIONAL {{ ?adventure wf:recommendedAction ?recommendedAction . OPTIONAL {{ ?recommendedAction wf:recommendedRoute ?wfRecommendedRoute }} OPTIONAL {{ ?recommendedAction wf:routeLabel ?wfRecommendedRouteLabel }} OPTIONAL {{ ?recommendedAction wf:recommendedChoice ?wfRecommendedChoice }} }}\n    OPTIONAL {{ ?adventure wfui:recommendedRouteId ?wfuiRecommendedRoute }}\n    OPTIONAL {{ ?adventure wfui:recommendedRouteLabel ?wfuiRecommendedRouteLabel }}\n    OPTIONAL {{ ?adventure wfui:recommendedChoiceId ?wfuiRecommendedChoice }}\n    BIND(COALESCE(?wfRecommendedRoute, ?wfuiRecommendedRoute) AS ?recommendedRoute)\n    BIND(COALESCE(?wfRecommendedRouteLabel, ?wfuiRecommendedRouteLabel) AS ?recommendedRouteLabel)\n    BIND(COALESCE(?wfRecommendedChoice, ?wfuiRecommendedChoice) AS ?recommendedChoice)\n    OPTIONAL {{ ?adventure wf:visibleObjectCount ?wfVisibleObjectCount }}\n    OPTIONAL {{ ?adventure wfui:visibleObjectCount ?wfuiVisibleObjectCount }}\n    BIND(COALESCE(?wfVisibleObjectCount, ?wfuiVisibleObjectCount) AS ?visibleObjectCount)\n    OPTIONAL {{ ?adventure wf:warningCount ?wfWarningCount }}\n    OPTIONAL {{ ?adventure wfui:warningCount ?wfuiWarningCount }}\n    BIND(COALESCE(?wfWarningCount, ?wfuiWarningCount) AS ?warningCount)\n    OPTIONAL {{ ?adventure wfui:rawSparqlJson ?rawSparqlJson }}\n    OPTIONAL {{\n      ?adventure wf:hasRawSparqlQuery ?rawSparqlQuery .\n      OPTIONAL {{ ?rawSparqlQuery wf:queryTitle ?rawSparqlTitle }}\n      OPTIONAL {{ ?rawSparqlQuery wf:queryText ?rawSparqlText }}\n      OPTIONAL {{ ?rawSparqlQuery wf:queryOrder ?rawSparqlOrder }}\n      OPTIONAL {{ ?rawSparqlQuery wf:queryGraph ?rawSparqlGraph }}\n    }}\n    OPTIONAL {{ ?adventure wf:authorizationFlagCount ?wfAuthorizationFlagCount }}\n    OPTIONAL {{ ?adventure wfui:authorizationFlagCount ?wfuiAuthorizationFlagCount }}\n    BIND(COALESCE(?wfAuthorizationFlagCount, ?wfuiAuthorizationFlagCount) AS ?authorizationFlagCount)\n    OPTIONAL {{\n      {{ ?adventure wf:hasNavigationRoute ?navigationRoute . }} UNION {{ ?adventure wfui:hasNavigationRoute ?navigationRoute . }}\n      OPTIONAL {{ ?navigationRoute wf:routeId ?wfRouteId }}\n      OPTIONAL {{ ?navigationRoute wfui:routeId ?wfuiRouteId }}\n      BIND(COALESCE(?wfRouteId, ?wfuiRouteId) AS ?routeId)\n      OPTIONAL {{ ?navigationRoute wf:routeLabel ?wfRouteLabel }}\n      OPTIONAL {{ ?navigationRoute wfui:routeLabel ?wfuiRouteLabel }}\n      BIND(COALESCE(?wfRouteLabel, ?wfuiRouteLabel) AS ?routeLabel)\n      OPTIONAL {{ ?navigationRoute wf:routeKind ?wfRouteKind }}\n      OPTIONAL {{ ?navigationRoute wfui:routeKind ?wfuiRouteKind }}\n      BIND(COALESCE(?wfRouteKind, ?wfuiRouteKind) AS ?routeKind)\n      OPTIONAL {{ ?navigationRoute wf:intent ?wfRouteIntent }}\n      OPTIONAL {{ ?navigationRoute wfui:intent ?wfuiRouteIntent }}\n      BIND(COALESCE(?wfRouteIntent, ?wfuiRouteIntent) AS ?routeIntent)\n      OPTIONAL {{ ?navigationRoute wf:choiceId ?wfRouteChoiceId }}\n      OPTIONAL {{ ?navigationRoute wfui:choiceId ?wfuiRouteChoiceId }}\n      BIND(COALESCE(?wfRouteChoiceId, ?wfuiRouteChoiceId) AS ?routeChoiceId)\n      OPTIONAL {{ ?navigationRoute wf:choiceLabel ?wfRouteChoiceLabel }}\n      OPTIONAL {{ ?navigationRoute wfui:choiceLabel ?wfuiRouteChoiceLabel }}\n      BIND(COALESCE(?wfRouteChoiceLabel, ?wfuiRouteChoiceLabel) AS ?routeChoiceLabel)\n      OPTIONAL {{ ?navigationRoute wf:source ?wfRouteSource }}\n      OPTIONAL {{ ?navigationRoute wfui:source ?wfuiRouteSource }}\n      BIND(COALESCE(?wfRouteSource, ?wfuiRouteSource) AS ?routeSource)\n      OPTIONAL {{ ?navigationRoute wf:rationale ?wfRouteRationale }}\n      OPTIONAL {{ ?navigationRoute wfui:rationale ?wfuiRouteRationale }}\n      BIND(COALESCE(?wfRouteRationale, ?wfuiRouteRationale) AS ?routeRationale)\n      OPTIONAL {{ ?navigationRoute wfui:actionJson ?routeActionJson }}\n      OPTIONAL {{\n        ?navigationRoute wf:routeAction ?routeAction .\n        OPTIONAL {{ ?routeAction wf:actionTool ?routeActionTool }}\n        OPTIONAL {{ ?routeAction wf:actionKind ?routeActionKind }}\n        OPTIONAL {{ ?routeAction wf:commandTemplate ?routeCommandTemplate }}\n        OPTIONAL {{ ?routeAction wf:argumentHash ?routeArgumentHash }}\n        OPTIONAL {{\n          ?routeAction wf:argument ?routeArgument .\n          OPTIONAL {{ ?routeArgument wf:argumentName ?routeArgumentName }}\n          OPTIONAL {{ ?routeArgument wf:argumentValue ?routeArgumentValue }}\n        }}\n      }}\n    }}\n    OPTIONAL {{\n      {{ ?adventure wf:hasAuthorizationFlag ?flag . }} UNION {{ ?adventure wfui:hasAuthorizationFlag ?flag . }}\n      OPTIONAL {{ ?flag wf:pageId ?wfFlagPageId }}\n      OPTIONAL {{ ?flag wfui:pageId ?wfuiFlagPageId }}\n      BIND(COALESCE(?wfFlagPageId, ?wfuiFlagPageId) AS ?flagPageId)\n      OPTIONAL {{ ?flag wf:choiceId ?wfFlagChoiceId }}\n      OPTIONAL {{ ?flag wfui:choiceId ?wfuiFlagChoiceId }}\n      BIND(COALESCE(?wfFlagChoiceId, ?wfuiFlagChoiceId) AS ?flagChoiceId)\n      OPTIONAL {{ ?flag wf:authorizationReason ?wfFlagReason }}\n      OPTIONAL {{ ?flag wfui:authorizationReason ?wfuiFlagReason }}\n      BIND(COALESCE(?wfFlagReason, ?wfuiFlagReason) AS ?flagReason)\n    }}\n  }}\n}}\nORDER BY ?rawSparqlOrder ?routeId ?routeArgumentName ?flagPageId ?flagChoiceId"
    )
}

fn page_turn_decisions_query(read_graph: &str, workflow_name: Option<&str>) -> String {
    let workflow_filter = workflow_name
        .map(|name| {
            format!(
                "\n    FILTER(!BOUND(?workflowName) || ?workflowName = {})",
                sparql_string_literal(name)
            )
        })
        .unwrap_or_default();
    format!(
        "PREFIX wf: <{WF_NS}>\nPREFIX wfui: <{WFUI_NS}>\nPREFIX prov: <{PROV_NS}>\nPREFIX dcterms: <{DCTERMS_NS}>\nSELECT ?decision ?generatedAt ?decisionGraphId ?workflowName ?fromPage ?intent ?readiness ?nativeSuggestedChoice ?recommendedRoute ?recommendedRouteLabel ?recommendedChoice ?followedRoute ?followedRouteLabel ?followedChoice ?followedChoiceLabel ?executionAuthorized ?authorizationFlagCount ?rationale ?evidence ?evidenceRole ?evidencePath ?navigationRoute ?routeId ?routeLabel ?routeKind ?routeIntent ?routeChoiceId ?routeChoiceLabel ?routeSource ?routeRationale ?routeActionJson ?routeAction ?routeActionTool ?routeActionKind ?routeCommandTemplate ?routeArgumentHash ?routeArgument ?routeArgumentName ?routeArgumentValue ?flag ?flagPageId ?flagChoiceId ?flagReason WHERE {{\n  GRAPH <{read_graph}> {{\n    ?decision a ?decisionType .\n    VALUES ?decisionType {{ wf:PageTurnDecision wfui:PageTurnDecision }}\n    OPTIONAL {{ ?decision prov:generatedAtTime ?generatedAt }}\n    OPTIONAL {{ ?decision wf:graphId ?wfDecisionGraphId }}\n    OPTIONAL {{ ?decision wfui:graphId ?wfuiDecisionGraphId }}\n    BIND(COALESCE(?wfDecisionGraphId, ?wfuiDecisionGraphId) AS ?decisionGraphId)\n    OPTIONAL {{ ?decision wf:workflowName ?wfWorkflowName }}\n    OPTIONAL {{ ?decision wfui:workflowName ?wfuiWorkflowName }}\n    BIND(COALESCE(?wfWorkflowName, ?wfuiWorkflowName) AS ?workflowName)\n    OPTIONAL {{ ?decision wf:fromPage ?wfFromPage }}\n    OPTIONAL {{ ?decision wfui:fromPage ?wfuiFromPage }}\n    BIND(COALESCE(?wfFromPage, ?wfuiFromPage) AS ?fromPage)\n    OPTIONAL {{ ?decision wf:intent ?wfIntent }}\n    OPTIONAL {{ ?decision wfui:intent ?wfuiIntent }}\n    BIND(COALESCE(?wfIntent, ?wfuiIntent) AS ?intent)\n    OPTIONAL {{ ?decision wf:readiness ?wfReadiness }}\n    OPTIONAL {{ ?decision wfui:readiness ?wfuiReadiness }}\n    BIND(COALESCE(?wfReadiness, ?wfuiReadiness) AS ?readiness)\n    OPTIONAL {{ ?decision wf:nativeSuggestedChoiceId ?wfNativeSuggestedChoice }}\n    OPTIONAL {{ ?decision wfui:nativeSuggestedChoiceId ?wfuiNativeSuggestedChoice }}\n    BIND(COALESCE(?wfNativeSuggestedChoice, ?wfuiNativeSuggestedChoice) AS ?nativeSuggestedChoice)\n    OPTIONAL {{ ?decision wf:recommendedRouteId ?wfRecommendedRoute }}\n    OPTIONAL {{ ?decision wfui:recommendedRouteId ?wfuiRecommendedRoute }}\n    BIND(COALESCE(?wfRecommendedRoute, ?wfuiRecommendedRoute) AS ?recommendedRoute)\n    OPTIONAL {{ ?decision wf:recommendedRouteLabel ?wfRecommendedRouteLabel }}\n    OPTIONAL {{ ?decision wfui:recommendedRouteLabel ?wfuiRecommendedRouteLabel }}\n    BIND(COALESCE(?wfRecommendedRouteLabel, ?wfuiRecommendedRouteLabel) AS ?recommendedRouteLabel)\n    OPTIONAL {{ ?decision wf:recommendedChoiceId ?wfRecommendedChoice }}\n    OPTIONAL {{ ?decision wfui:recommendedChoiceId ?wfuiRecommendedChoice }}\n    BIND(COALESCE(?wfRecommendedChoice, ?wfuiRecommendedChoice) AS ?recommendedChoice)\n    OPTIONAL {{ ?decision wf:followedRouteId ?wfFollowedRoute }}\n    OPTIONAL {{ ?decision wfui:followedRouteId ?wfuiFollowedRoute }}\n    BIND(COALESCE(?wfFollowedRoute, ?wfuiFollowedRoute) AS ?followedRoute)\n    OPTIONAL {{ ?decision wf:followedRouteLabel ?wfFollowedRouteLabel }}\n    OPTIONAL {{ ?decision wfui:followedRouteLabel ?wfuiFollowedRouteLabel }}\n    BIND(COALESCE(?wfFollowedRouteLabel, ?wfuiFollowedRouteLabel) AS ?followedRouteLabel)\n    OPTIONAL {{ ?decision wf:followedChoiceId ?wfFollowedChoice }}\n    OPTIONAL {{ ?decision wfui:followedChoiceId ?wfuiFollowedChoice }}\n    BIND(COALESCE(?wfFollowedChoice, ?wfuiFollowedChoice) AS ?followedChoice)\n    OPTIONAL {{ ?decision wf:followedChoiceLabel ?wfFollowedChoiceLabel }}\n    OPTIONAL {{ ?decision wfui:followedChoiceLabel ?wfuiFollowedChoiceLabel }}\n    BIND(COALESCE(?wfFollowedChoiceLabel, ?wfuiFollowedChoiceLabel) AS ?followedChoiceLabel)\n    OPTIONAL {{ ?decision wf:executionAuthorized ?wfExecutionAuthorized }}\n    OPTIONAL {{ ?decision wfui:executionAuthorized ?wfuiExecutionAuthorized }}\n    BIND(COALESCE(?wfExecutionAuthorized, ?wfuiExecutionAuthorized) AS ?executionAuthorized)\n    OPTIONAL {{ ?decision wf:authorizationFlagCount ?wfAuthorizationFlagCount }}\n    OPTIONAL {{ ?decision wfui:authorizationFlagCount ?wfuiAuthorizationFlagCount }}\n    BIND(COALESCE(?wfAuthorizationFlagCount, ?wfuiAuthorizationFlagCount) AS ?authorizationFlagCount)\n    OPTIONAL {{ ?decision prov:value ?rationale }}{workflow_filter}\n    OPTIONAL {{\n      ?decision prov:wasDerivedFrom ?evidence .\n      OPTIONAL {{ ?evidence dcterms:identifier ?evidenceRole }}\n      OPTIONAL {{ ?evidence wf:path ?wfEvidencePath }}\n      OPTIONAL {{ ?evidence wfui:path ?wfuiEvidencePath }}\n      BIND(COALESCE(?wfEvidencePath, ?wfuiEvidencePath) AS ?evidencePath)\n    }}\n    OPTIONAL {{\n      {{ ?decision wf:hasNavigationRoute ?navigationRoute . }} UNION {{ ?decision wfui:hasNavigationRoute ?navigationRoute . }}\n      OPTIONAL {{ ?navigationRoute wf:routeId ?wfRouteId }}\n      OPTIONAL {{ ?navigationRoute wfui:routeId ?wfuiRouteId }}\n      BIND(COALESCE(?wfRouteId, ?wfuiRouteId) AS ?routeId)\n      OPTIONAL {{ ?navigationRoute wf:routeLabel ?wfRouteLabel }}\n      OPTIONAL {{ ?navigationRoute wfui:routeLabel ?wfuiRouteLabel }}\n      BIND(COALESCE(?wfRouteLabel, ?wfuiRouteLabel) AS ?routeLabel)\n      OPTIONAL {{ ?navigationRoute wf:routeKind ?wfRouteKind }}\n      OPTIONAL {{ ?navigationRoute wfui:routeKind ?wfuiRouteKind }}\n      BIND(COALESCE(?wfRouteKind, ?wfuiRouteKind) AS ?routeKind)\n      OPTIONAL {{ ?navigationRoute wf:intent ?wfRouteIntent }}\n      OPTIONAL {{ ?navigationRoute wfui:intent ?wfuiRouteIntent }}\n      BIND(COALESCE(?wfRouteIntent, ?wfuiRouteIntent) AS ?routeIntent)\n      OPTIONAL {{ ?navigationRoute wf:choiceId ?wfRouteChoiceId }}\n      OPTIONAL {{ ?navigationRoute wfui:choiceId ?wfuiRouteChoiceId }}\n      BIND(COALESCE(?wfRouteChoiceId, ?wfuiRouteChoiceId) AS ?routeChoiceId)\n      OPTIONAL {{ ?navigationRoute wf:choiceLabel ?wfRouteChoiceLabel }}\n      OPTIONAL {{ ?navigationRoute wfui:choiceLabel ?wfuiRouteChoiceLabel }}\n      BIND(COALESCE(?wfRouteChoiceLabel, ?wfuiRouteChoiceLabel) AS ?routeChoiceLabel)\n      OPTIONAL {{ ?navigationRoute wf:source ?wfRouteSource }}\n      OPTIONAL {{ ?navigationRoute wfui:source ?wfuiRouteSource }}\n      BIND(COALESCE(?wfRouteSource, ?wfuiRouteSource) AS ?routeSource)\n      OPTIONAL {{ ?navigationRoute wf:rationale ?wfRouteRationale }}\n      OPTIONAL {{ ?navigationRoute wfui:rationale ?wfuiRouteRationale }}\n      BIND(COALESCE(?wfRouteRationale, ?wfuiRouteRationale) AS ?routeRationale)\n      OPTIONAL {{ ?navigationRoute wfui:actionJson ?routeActionJson }}\n      OPTIONAL {{\n        ?navigationRoute wf:routeAction ?routeAction .\n        OPTIONAL {{ ?routeAction wf:actionTool ?routeActionTool }}\n        OPTIONAL {{ ?routeAction wf:actionKind ?routeActionKind }}\n        OPTIONAL {{ ?routeAction wf:commandTemplate ?routeCommandTemplate }}\n        OPTIONAL {{ ?routeAction wf:argumentHash ?routeArgumentHash }}\n        OPTIONAL {{\n          ?routeAction wf:argument ?routeArgument .\n          OPTIONAL {{ ?routeArgument wf:argumentName ?routeArgumentName }}\n          OPTIONAL {{ ?routeArgument wf:argumentValue ?routeArgumentValue }}\n        }}\n      }}\n    }}\n    OPTIONAL {{\n      {{ ?decision wf:hasAuthorizationFlag ?flag . }} UNION {{ ?decision wfui:hasAuthorizationFlag ?flag . }}\n      OPTIONAL {{ ?flag wf:pageId ?wfFlagPageId }}\n      OPTIONAL {{ ?flag wfui:pageId ?wfuiFlagPageId }}\n      BIND(COALESCE(?wfFlagPageId, ?wfuiFlagPageId) AS ?flagPageId)\n      OPTIONAL {{ ?flag wf:choiceId ?wfFlagChoiceId }}\n      OPTIONAL {{ ?flag wfui:choiceId ?wfuiFlagChoiceId }}\n      BIND(COALESCE(?wfFlagChoiceId, ?wfuiFlagChoiceId) AS ?flagChoiceId)\n      OPTIONAL {{ ?flag wf:authorizationReason ?wfFlagReason }}\n      OPTIONAL {{ ?flag wfui:authorizationReason ?wfuiFlagReason }}\n      BIND(COALESCE(?wfFlagReason, ?wfuiFlagReason) AS ?flagReason)\n    }}\n  }}\n}}\nORDER BY DESC(?generatedAt) ?decision ?evidenceRole ?routeId ?routeArgumentName ?flagPageId ?flagChoiceId"
    )
}

fn page_turn_decision_detail_query(read_graph: &str, subject: &str) -> String {
    format!(
        "PREFIX wf: <{WF_NS}>\nPREFIX wfui: <{WFUI_NS}>\nPREFIX prov: <{PROV_NS}>\nPREFIX dcterms: <{DCTERMS_NS}>\nSELECT ?decision ?generatedAt ?decisionGraphId ?workflowName ?fromPage ?intent ?readiness ?nativeSuggestedChoice ?recommendedRoute ?recommendedRouteLabel ?recommendedChoice ?followedRoute ?followedRouteLabel ?followedChoice ?followedChoiceLabel ?executionAuthorized ?authorizationFlagCount ?rationale ?evidence ?evidenceRole ?evidencePath ?navigationRoute ?routeId ?routeLabel ?routeKind ?routeIntent ?routeChoiceId ?routeChoiceLabel ?routeSource ?routeRationale ?routeActionJson ?routeAction ?routeActionTool ?routeActionKind ?routeCommandTemplate ?routeArgumentHash ?routeArgument ?routeArgumentName ?routeArgumentValue ?flag ?flagPageId ?flagChoiceId ?flagReason WHERE {{\n  GRAPH <{read_graph}> {{\n    BIND(<{subject}> AS ?decision)\n    ?decision a ?decisionType .\n    VALUES ?decisionType {{ wf:PageTurnDecision wfui:PageTurnDecision }}\n    OPTIONAL {{ ?decision prov:generatedAtTime ?generatedAt }}\n    OPTIONAL {{ ?decision wf:graphId ?wfDecisionGraphId }}\n    OPTIONAL {{ ?decision wfui:graphId ?wfuiDecisionGraphId }}\n    BIND(COALESCE(?wfDecisionGraphId, ?wfuiDecisionGraphId) AS ?decisionGraphId)\n    OPTIONAL {{ ?decision wf:workflowName ?wfWorkflowName }}\n    OPTIONAL {{ ?decision wfui:workflowName ?wfuiWorkflowName }}\n    BIND(COALESCE(?wfWorkflowName, ?wfuiWorkflowName) AS ?workflowName)\n    OPTIONAL {{ ?decision wf:fromPage ?wfFromPage }}\n    OPTIONAL {{ ?decision wfui:fromPage ?wfuiFromPage }}\n    BIND(COALESCE(?wfFromPage, ?wfuiFromPage) AS ?fromPage)\n    OPTIONAL {{ ?decision wf:intent ?wfIntent }}\n    OPTIONAL {{ ?decision wfui:intent ?wfuiIntent }}\n    BIND(COALESCE(?wfIntent, ?wfuiIntent) AS ?intent)\n    OPTIONAL {{ ?decision wf:readiness ?wfReadiness }}\n    OPTIONAL {{ ?decision wfui:readiness ?wfuiReadiness }}\n    BIND(COALESCE(?wfReadiness, ?wfuiReadiness) AS ?readiness)\n    OPTIONAL {{ ?decision wf:nativeSuggestedChoiceId ?wfNativeSuggestedChoice }}\n    OPTIONAL {{ ?decision wfui:nativeSuggestedChoiceId ?wfuiNativeSuggestedChoice }}\n    BIND(COALESCE(?wfNativeSuggestedChoice, ?wfuiNativeSuggestedChoice) AS ?nativeSuggestedChoice)\n    OPTIONAL {{ ?decision wf:recommendedRouteId ?wfRecommendedRoute }}\n    OPTIONAL {{ ?decision wfui:recommendedRouteId ?wfuiRecommendedRoute }}\n    BIND(COALESCE(?wfRecommendedRoute, ?wfuiRecommendedRoute) AS ?recommendedRoute)\n    OPTIONAL {{ ?decision wf:recommendedRouteLabel ?wfRecommendedRouteLabel }}\n    OPTIONAL {{ ?decision wfui:recommendedRouteLabel ?wfuiRecommendedRouteLabel }}\n    BIND(COALESCE(?wfRecommendedRouteLabel, ?wfuiRecommendedRouteLabel) AS ?recommendedRouteLabel)\n    OPTIONAL {{ ?decision wf:recommendedChoiceId ?wfRecommendedChoice }}\n    OPTIONAL {{ ?decision wfui:recommendedChoiceId ?wfuiRecommendedChoice }}\n    BIND(COALESCE(?wfRecommendedChoice, ?wfuiRecommendedChoice) AS ?recommendedChoice)\n    OPTIONAL {{ ?decision wf:followedRouteId ?wfFollowedRoute }}\n    OPTIONAL {{ ?decision wfui:followedRouteId ?wfuiFollowedRoute }}\n    BIND(COALESCE(?wfFollowedRoute, ?wfuiFollowedRoute) AS ?followedRoute)\n    OPTIONAL {{ ?decision wf:followedRouteLabel ?wfFollowedRouteLabel }}\n    OPTIONAL {{ ?decision wfui:followedRouteLabel ?wfuiFollowedRouteLabel }}\n    BIND(COALESCE(?wfFollowedRouteLabel, ?wfuiFollowedRouteLabel) AS ?followedRouteLabel)\n    OPTIONAL {{ ?decision wf:followedChoiceId ?wfFollowedChoice }}\n    OPTIONAL {{ ?decision wfui:followedChoiceId ?wfuiFollowedChoice }}\n    BIND(COALESCE(?wfFollowedChoice, ?wfuiFollowedChoice) AS ?followedChoice)\n    OPTIONAL {{ ?decision wf:followedChoiceLabel ?wfFollowedChoiceLabel }}\n    OPTIONAL {{ ?decision wfui:followedChoiceLabel ?wfuiFollowedChoiceLabel }}\n    BIND(COALESCE(?wfFollowedChoiceLabel, ?wfuiFollowedChoiceLabel) AS ?followedChoiceLabel)\n    OPTIONAL {{ ?decision wf:executionAuthorized ?wfExecutionAuthorized }}\n    OPTIONAL {{ ?decision wfui:executionAuthorized ?wfuiExecutionAuthorized }}\n    BIND(COALESCE(?wfExecutionAuthorized, ?wfuiExecutionAuthorized) AS ?executionAuthorized)\n    OPTIONAL {{ ?decision wf:authorizationFlagCount ?wfAuthorizationFlagCount }}\n    OPTIONAL {{ ?decision wfui:authorizationFlagCount ?wfuiAuthorizationFlagCount }}\n    BIND(COALESCE(?wfAuthorizationFlagCount, ?wfuiAuthorizationFlagCount) AS ?authorizationFlagCount)\n    OPTIONAL {{ ?decision prov:value ?rationale }}\n    OPTIONAL {{\n      ?decision prov:wasDerivedFrom ?evidence .\n      OPTIONAL {{ ?evidence dcterms:identifier ?evidenceRole }}\n      OPTIONAL {{ ?evidence wf:path ?wfEvidencePath }}\n      OPTIONAL {{ ?evidence wfui:path ?wfuiEvidencePath }}\n      BIND(COALESCE(?wfEvidencePath, ?wfuiEvidencePath) AS ?evidencePath)\n    }}\n    OPTIONAL {{\n      {{ ?decision wf:hasNavigationRoute ?navigationRoute . }} UNION {{ ?decision wfui:hasNavigationRoute ?navigationRoute . }}\n      OPTIONAL {{ ?navigationRoute wf:routeId ?wfRouteId }}\n      OPTIONAL {{ ?navigationRoute wfui:routeId ?wfuiRouteId }}\n      BIND(COALESCE(?wfRouteId, ?wfuiRouteId) AS ?routeId)\n      OPTIONAL {{ ?navigationRoute wf:routeLabel ?wfRouteLabel }}\n      OPTIONAL {{ ?navigationRoute wfui:routeLabel ?wfuiRouteLabel }}\n      BIND(COALESCE(?wfRouteLabel, ?wfuiRouteLabel) AS ?routeLabel)\n      OPTIONAL {{ ?navigationRoute wf:routeKind ?wfRouteKind }}\n      OPTIONAL {{ ?navigationRoute wfui:routeKind ?wfuiRouteKind }}\n      BIND(COALESCE(?wfRouteKind, ?wfuiRouteKind) AS ?routeKind)\n      OPTIONAL {{ ?navigationRoute wf:intent ?wfRouteIntent }}\n      OPTIONAL {{ ?navigationRoute wfui:intent ?wfuiRouteIntent }}\n      BIND(COALESCE(?wfRouteIntent, ?wfuiRouteIntent) AS ?routeIntent)\n      OPTIONAL {{ ?navigationRoute wf:choiceId ?wfRouteChoiceId }}\n      OPTIONAL {{ ?navigationRoute wfui:choiceId ?wfuiRouteChoiceId }}\n      BIND(COALESCE(?wfRouteChoiceId, ?wfuiRouteChoiceId) AS ?routeChoiceId)\n      OPTIONAL {{ ?navigationRoute wf:choiceLabel ?wfRouteChoiceLabel }}\n      OPTIONAL {{ ?navigationRoute wfui:choiceLabel ?wfuiRouteChoiceLabel }}\n      BIND(COALESCE(?wfRouteChoiceLabel, ?wfuiRouteChoiceLabel) AS ?routeChoiceLabel)\n      OPTIONAL {{ ?navigationRoute wf:source ?wfRouteSource }}\n      OPTIONAL {{ ?navigationRoute wfui:source ?wfuiRouteSource }}\n      BIND(COALESCE(?wfRouteSource, ?wfuiRouteSource) AS ?routeSource)\n      OPTIONAL {{ ?navigationRoute wf:rationale ?wfRouteRationale }}\n      OPTIONAL {{ ?navigationRoute wfui:rationale ?wfuiRouteRationale }}\n      BIND(COALESCE(?wfRouteRationale, ?wfuiRouteRationale) AS ?routeRationale)\n      OPTIONAL {{ ?navigationRoute wfui:actionJson ?routeActionJson }}\n      OPTIONAL {{\n        ?navigationRoute wf:routeAction ?routeAction .\n        OPTIONAL {{ ?routeAction wf:actionTool ?routeActionTool }}\n        OPTIONAL {{ ?routeAction wf:actionKind ?routeActionKind }}\n        OPTIONAL {{ ?routeAction wf:commandTemplate ?routeCommandTemplate }}\n        OPTIONAL {{ ?routeAction wf:argumentHash ?routeArgumentHash }}\n        OPTIONAL {{\n          ?routeAction wf:argument ?routeArgument .\n          OPTIONAL {{ ?routeArgument wf:argumentName ?routeArgumentName }}\n          OPTIONAL {{ ?routeArgument wf:argumentValue ?routeArgumentValue }}\n        }}\n      }}\n    }}\n    OPTIONAL {{\n      {{ ?decision wf:hasAuthorizationFlag ?flag . }} UNION {{ ?decision wfui:hasAuthorizationFlag ?flag . }}\n      OPTIONAL {{ ?flag wf:pageId ?wfFlagPageId }}\n      OPTIONAL {{ ?flag wfui:pageId ?wfuiFlagPageId }}\n      BIND(COALESCE(?wfFlagPageId, ?wfuiFlagPageId) AS ?flagPageId)\n      OPTIONAL {{ ?flag wf:choiceId ?wfFlagChoiceId }}\n      OPTIONAL {{ ?flag wfui:choiceId ?wfuiFlagChoiceId }}\n      BIND(COALESCE(?wfFlagChoiceId, ?wfuiFlagChoiceId) AS ?flagChoiceId)\n      OPTIONAL {{ ?flag wf:authorizationReason ?wfFlagReason }}\n      OPTIONAL {{ ?flag wfui:authorizationReason ?wfuiFlagReason }}\n      BIND(COALESCE(?wfFlagReason, ?wfuiFlagReason) AS ?flagReason)\n    }}\n  }}\n}}\nORDER BY ?evidenceRole ?routeId ?routeArgumentName ?flagPageId ?flagChoiceId"
    )
}

fn missing_required_query(read_graph: &str, workflow_uri: &str) -> String {
    format!(
        "PREFIX wf: <{WF_NS}>\nSELECT ?missing WHERE {{\n  VALUES ?missing {{ \"wf:name\" \"wf:description\" \"wf:scriptSha256\" \"wf:phase\" }}\n  FILTER(\n    (?missing = \"wf:name\" && NOT EXISTS {{ GRAPH <{read_graph}> {{ <{workflow_uri}> wf:name ?v }} }}) ||\n    (?missing = \"wf:description\" && NOT EXISTS {{ GRAPH <{read_graph}> {{ <{workflow_uri}> wf:description ?v }} }}) ||\n    (?missing = \"wf:scriptSha256\" && NOT EXISTS {{ GRAPH <{read_graph}> {{ <{workflow_uri}> wf:scriptSha256 ?v }} }}) ||\n    (?missing = \"wf:phase\" && NOT EXISTS {{ GRAPH <{read_graph}> {{ <{workflow_uri}> wf:phase ?v }} }})\n  )\n}}"
    )
}

fn sparql_string_literal(value: &str) -> String {
    let escaped = value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
        .replace('\t', "\\t");
    format!("\"{escaped}\"")
}

fn strip_uri(value: &str) -> String {
    value
        .strip_prefix('<')
        .and_then(|inner| inner.strip_suffix('>'))
        .map(str::to_string)
        .unwrap_or_else(|| value.to_string())
}

fn literal_usize(value: &str) -> Option<usize> {
    literal_value(value).parse::<usize>().ok()
}

fn literal_i64(value: &str) -> Option<i64> {
    literal_value(value).parse::<i64>().ok()
}

fn literal_bool(value: &str) -> Option<bool> {
    match literal_value(value).as_str() {
        "true" | "1" => Some(true),
        "false" | "0" => Some(false),
        _ => None,
    }
}

fn route_action_argument_from_row(
    row: &BTreeMap<String, String>,
) -> Option<WorkflowRouteActionArgument> {
    let subject = row.get("routeArgument").map(|value| strip_uri(value))?;
    if subject.trim().is_empty() {
        return None;
    }
    Some(WorkflowRouteActionArgument {
        subject,
        name: row
            .get("routeArgumentName")
            .map(|value| literal_value(value)),
        value: row
            .get("routeArgumentValue")
            .map(|value| literal_value(value)),
    })
}

fn raw_sparql_query_from_row(row: &BTreeMap<String, String>) -> Option<WorkflowRawSparqlQuery> {
    let subject = row.get("rawSparqlQuery").map(|value| strip_uri(value))?;
    if subject.trim().is_empty() {
        return None;
    }
    Some(WorkflowRawSparqlQuery {
        subject,
        title: row.get("rawSparqlTitle").map(|value| literal_value(value)),
        query: row.get("rawSparqlText").map(|value| literal_value(value)),
        order: row
            .get("rawSparqlOrder")
            .and_then(|value| literal_usize(value)),
        graph_id: row.get("rawSparqlGraph").map(|value| literal_value(value)),
    })
}

fn literal_value(value: &str) -> String {
    let trimmed = value.trim();
    if let Some(inner) = trimmed
        .strip_prefix('<')
        .and_then(|inner| inner.strip_suffix('>'))
    {
        return inner.to_string();
    }
    if !trimmed.starts_with('"') {
        return trimmed.to_string();
    }
    let Some(close) = find_closing_quote(trimmed) else {
        return trimmed.to_string();
    };
    unescape_nt(&trimmed[1..close])
}

fn find_closing_quote(value: &str) -> Option<usize> {
    let bytes = value.as_bytes();
    let mut index = 1;
    while index < bytes.len() {
        match bytes[index] {
            b'\\' => index += 2,
            b'"' => return Some(index),
            _ => index += 1,
        }
    }
    None
}

fn unescape_nt(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('t') => out.push('\t'),
            Some('"') => out.push('"'),
            Some('\\') => out.push('\\'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

fn doc_id_from_uri(uri: &str) -> Option<String> {
    let index = uri.rfind(":doc:")?;
    let tail = &uri[index + ":doc:".len()..];
    if tail.is_empty() || tail.contains('#') {
        return None;
    }
    Some(tail.to_string())
}

#[derive(Debug, Clone)]
struct DocBlockRef {
    document_id: String,
    block_id: String,
}

fn doc_block_from_uri(uri: &str) -> Option<DocBlockRef> {
    let index = uri.rfind(":doc:")?;
    let tail = &uri[index + ":doc:".len()..];
    let (document_id, block_id) = tail.split_once('#')?;
    if document_id.is_empty() || block_id.is_empty() {
        return None;
    }
    Some(DocBlockRef {
        document_id: document_id.to_string(),
        block_id: block_id.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect()
    }

    fn sample_definition_triples() -> Vec<String> {
        vec![
            format!("<urn:mnemosyne:local:graph:lab:doc:wf-demo> a <{WF_NS}Workflow> ."),
            format!("<urn:mnemosyne:local:graph:lab:doc:wf-demo> <{WF_NS}name> \"demo\" ."),
            format!(
                "<urn:mnemosyne:local:graph:lab:doc:wf-demo> <{WF_NS}description> \"Demo workflow\" ."
            ),
            format!(
                "<urn:mnemosyne:local:graph:lab:doc:wf-demo> <{WF_NS}whenToUse> \"When testing\" ."
            ),
            format!(
                "<urn:mnemosyne:local:graph:lab:doc:wf-demo> <{WF_NS}scriptSha256> \"abc123\" ."
            ),
            format!(
                "<urn:mnemosyne:local:graph:lab:doc:wf-demo> <{WF_NS}scriptBlock> <urn:mnemosyne:local:graph:lab:doc:wf-demo#block-1> ."
            ),
            format!(
                "<urn:mnemosyne:local:graph:lab:doc:wf-demo> <{WF_NS}inputBlock> <urn:mnemosyne:local:graph:lab:doc:wf-input#block-2> ."
            ),
            format!(
                "<urn:mnemosyne:local:graph:lab:doc:wf-demo> <{WF_NS}phase> <urn:sophia:wf:demo:phase:1> ."
            ),
            format!("<urn:sophia:wf:demo:phase:1> a <{WF_NS}Phase> ."),
            format!(
                "<urn:sophia:wf:demo:phase:1> <{WF_NS}order> \"1\"^^<http://www.w3.org/2001/XMLSchema#integer> ."
            ),
            format!("<urn:sophia:wf:demo:phase:1> <{DCTERMS_NS}title> \"Go\" ."),
            format!(
                "<urn:mnemosyne:local:graph:lab:doc:wf-demo-n-a> a <{WF_NS}AgentNode> ."
            ),
            format!(
                "<urn:mnemosyne:local:graph:lab:doc:wf-demo-n-a> <{WF_NS}partOfWorkflow> <urn:mnemosyne:local:graph:lab:doc:wf-demo> ."
            ),
            format!(
                "<urn:mnemosyne:local:graph:lab:doc:wf-demo-n-a> <{WF_NS}label> \"extractor-a\" ."
            ),
            format!(
                "<urn:mnemosyne:local:graph:lab:doc:wf-demo-n-a> <{WF_NS}phaseIndex> \"1\"^^<http://www.w3.org/2001/XMLSchema#integer> ."
            ),
            format!(
                "<urn:mnemosyne:local:graph:lab:doc:wf-demo-n-a> <{WF_NS}agentType> \"scout\" ."
            ),
        ]
    }

    fn sample_snapshot() -> WorkflowBookSnapshot {
        WorkflowBookSnapshot {
            graph_id: "lab".to_string(),
            read_graph: "urn:mnemosyne:local:graph:lab:user:rdf".to_string(),
            workflows: vec![WorkflowSummary {
                uri: "urn:mnemosyne:local:graph:lab:doc:wf-demo".to_string(),
                name: "demo".to_string(),
                description: Some("Demo workflow".to_string()),
                when_to_use: Some("When testing".to_string()),
                script_sha256: Some("abc123".to_string()),
                script_block: Some("urn:mnemosyne:local:graph:lab:doc:wf-demo#block-1".to_string()),
                input_block: Some("urn:mnemosyne:local:graph:lab:doc:wf-input#block-2".to_string()),
                doc_id: Some("wf-demo".to_string()),
                seeded_from: Vec::new(),
                phases: vec![WorkflowPhase {
                    uri: "urn:sophia:wf:demo:phase:1".to_string(),
                    order: 1,
                    title: "Go".to_string(),
                    description: None,
                    seeded_from: Vec::new(),
                }],
                nodes: vec![WorkflowAgentNode {
                    uri: "urn:mnemosyne:local:graph:lab:doc:wf-demo-n-a".to_string(),
                    label: "extractor-a".to_string(),
                    phase_index: 1,
                    agent_type: Some("scout".to_string()),
                    doc_id: Some("wf-demo-n-a".to_string()),
                    seeded_from: Vec::new(),
                }],
                runs: vec![WorkflowRun {
                    uri: "urn:sophia:wf-run:run-1".to_string(),
                    run_id: "run-1".to_string(),
                    status: Some("completed".to_string()),
                    started_at: Some("2026-06-24T10:00:00Z".to_string()),
                    ended_at: None,
                    duration_ms: Some(1000),
                    total_tokens: Some(20),
                    agent_count: Some(1),
                }],
                composition_events: vec![WorkflowCompositionEvent {
                    uri: "urn:sophia:wf:composition-event:lab:demo:session-1:1".to_string(),
                    authoring_session_uri:
                        "urn:sophia:wf:authoring-session:lab:demo:session-1".to_string(),
                    generated_at: Some("2026-06-25T04:00:00Z".to_string()),
                    event_order: 1,
                    gesture_kind: "create_workflow".to_string(),
                    definition_subject: "urn:mnemosyne:local:graph:lab:doc:wf-demo".to_string(),
                    target_subject: Some("urn:mnemosyne:local:graph:lab:doc:wf-demo".to_string()),
                    rationale: Some("Start the workflow draft.".to_string()),
                    driver_agent: Some("codex".to_string()),
                    driver_lease: Some("lease-demo".to_string()),
                    agent_turn_uri: Some(
                        "urn:sophia:agent:session:workflow-authoring:lab-demo-session-1:turn:1"
                            .to_string(),
                    ),
                    insert_triples: sample_definition_triples(),
                    delete_triples: Vec::new(),
                }],
            }],
            adventures: vec![WorkflowAdventurePacket {
                subject:
                    "urn:sophia:wf:page-view:lab:demo:overview:analysis-scout:0123456789abcdef0123456789abcdef"
                        .to_string(),
                superseded_page_view: Some(
                    "urn:sophia:workflow-adventure:lab:demo:overview:analysis-scout".to_string(),
                ),
                generated_at: Some("2026-06-25T03:48:00Z".to_string()),
                graph_id: Some("lab".to_string()),
                workflow_name: Some("demo".to_string()),
                page_id: Some("overview".to_string()),
                page_title: Some("demo".to_string()),
                page_scene: Some("A retained source-side adventure packet.".to_string()),
                from_page: Some("overview".to_string()),
                followed_route: None,
                intent: Some("analysis-scout".to_string()),
                recommended_route: Some("analysis-scout".to_string()),
                recommended_route_label: Some("Analysis scout".to_string()),
                recommended_choice: Some("perception-map".to_string()),
                visible_object_count: Some(8),
                warning_count: Some(0),
                raw_sparql_json: None,
                authorization_flag_count: Some(1),
                raw_sparql_queries: vec![WorkflowRawSparqlQuery {
                    subject:
                        "urn:sophia:wf:page-view:lab:demo:overview:analysis-scout:0123456789abcdef0123456789abcdef:raw-sparql:1"
                            .to_string(),
                    title: Some("Find workflow".to_string()),
                    query: Some("SELECT ?workflow WHERE { ?workflow ?p ?o }".to_string()),
                    order: Some(1),
                    graph_id: Some("lab".to_string()),
                }],
                navigation_routes: vec![
                    WorkflowPageTurnNavigationRoute {
                        subject: "urn:sophia:wf:page-view:lab:demo:overview:analysis-scout:0123456789abcdef0123456789abcdef:route:analysis-scout".to_string(),
                        route_id: Some("analysis-scout".to_string()),
                        route_label: Some("Analysis scout".to_string()),
                        route_kind: Some("intent".to_string()),
                        intent: Some("analysis-scout".to_string()),
                        choice_id: Some("perception-map".to_string()),
                        choice_label: Some("Perception map".to_string()),
                        source: Some("workflow_compose_adventure".to_string()),
                        rationale: Some("Inspect the workflow perception before run prep.".to_string()),
                        action_json: None,
                        route_action: Some("urn:sophia:wf:page-view:lab:demo:overview:analysis-scout:0123456789abcdef0123456789abcdef:route:analysis-scout:action".to_string()),
                        action_tool: Some("workflow_compose_adventure".to_string()),
                        action_kind: Some("workflow_compose_adventure".to_string()),
                        command_template: Some("workflow_compose_adventure --page perception --intent analysis-scout".to_string()),
                        argument_hash: Some("hash-analysis-scout".to_string()),
                        action_arguments: vec![
                            WorkflowRouteActionArgument {
                                subject: "urn:sophia:wf:page-view:lab:demo:overview:analysis-scout:0123456789abcdef0123456789abcdef:route:analysis-scout:action:arg:graph-id".to_string(),
                                name: Some("graphId".to_string()),
                                value: Some("lab".to_string()),
                            },
                            WorkflowRouteActionArgument {
                                subject: "urn:sophia:wf:page-view:lab:demo:overview:analysis-scout:0123456789abcdef0123456789abcdef:route:analysis-scout:action:arg:page-id".to_string(),
                                name: Some("pageId".to_string()),
                                value: Some("perception".to_string()),
                            },
                        ],
                    },
                    WorkflowPageTurnNavigationRoute {
                        subject: "urn:sophia:wf:page-view:lab:demo:overview:analysis-scout:0123456789abcdef0123456789abcdef:route:raw-sparql".to_string(),
                        route_id: Some("raw-sparql".to_string()),
                        route_label: Some("Raw SPARQL".to_string()),
                        route_kind: Some("raw-sparql".to_string()),
                        intent: Some("raw-sparql".to_string()),
                        choice_id: Some("raw-sparql".to_string()),
                        choice_label: Some("Raw SPARQL".to_string()),
                        source: Some("workflow_compose_adventure".to_string()),
                        rationale: Some("Drop to exact RDF queries.".to_string()),
                        action_json: None,
                        route_action: Some("urn:sophia:wf:page-view:lab:demo:overview:analysis-scout:0123456789abcdef0123456789abcdef:route:raw-sparql:action".to_string()),
                        action_tool: Some("workflow_compose_adventure".to_string()),
                        action_kind: Some("workflow_compose_adventure".to_string()),
                        command_template: Some("workflow_compose_adventure --page raw --intent raw-sparql".to_string()),
                        argument_hash: Some("hash-raw-sparql".to_string()),
                        action_arguments: vec![WorkflowRouteActionArgument {
                            subject: "urn:sophia:wf:page-view:lab:demo:overview:analysis-scout:0123456789abcdef0123456789abcdef:route:raw-sparql:action:arg:page-id".to_string(),
                            name: Some("pageId".to_string()),
                            value: Some("raw".to_string()),
                        }],
                    },
                ],
                authorization_flags: vec![WorkflowAuthorizationFlag {
                    subject: "urn:sophia:workflow-adventure:lab:demo:overview:analysis-scout:authorization:run-prep".to_string(),
                    page_id: Some("execute".to_string()),
                    choice_id: Some("run-prep".to_string()),
                    reason: Some("run-prep remains authorization-gated".to_string()),
                }],
            }],
            decisions: vec![WorkflowPageTurnDecision {
                subject: "urn:sophia:workflow-page-turn-decision:lab:demo:overview:perception-map:analysis-scout:shard-01".to_string(),
                generated_at: Some("2026-06-25T03:49:50Z".to_string()),
                graph_id: Some("lab".to_string()),
                workflow_name: Some("demo".to_string()),
                from_page: Some("overview".to_string()),
                intent: Some("analysis-scout".to_string()),
                readiness: Some("inspect_before_execute".to_string()),
                native_suggested_choice: Some("execute-async".to_string()),
                recommended_route: Some("analysis-scout".to_string()),
                recommended_route_label: Some("Analysis scout".to_string()),
                recommended_choice: Some("perception-map".to_string()),
                followed_route: Some("analysis-scout".to_string()),
                followed_route_label: Some("Analysis scout".to_string()),
                followed_choice: Some("perception-map".to_string()),
                followed_choice_label: Some("Perception map".to_string()),
                execution_authorized: Some(false),
                authorization_flag_count: Some(1),
                rationale: Some("Prefer perceptual orientation before execute.".to_string()),
                evidence: vec![WorkflowPageTurnEvidence {
                    subject: "urn:sophia:workflow-page-turn-decision:lab:demo:overview:perception-map:analysis-scout:shard-01:evidence:handoff".to_string(),
                    role: Some("handoff".to_string()),
                    path: Some("scripts/lme-labeling/batches/50case-v0/workflow-args/shard-01.handoff.md".to_string()),
                }],
                navigation_routes: vec![WorkflowPageTurnNavigationRoute {
                    subject: "urn:sophia:workflow-page-turn-decision:lab:demo:overview:perception-map:analysis-scout:shard-01:route:analysis-scout".to_string(),
                    route_id: Some("analysis-scout".to_string()),
                    route_label: Some("Analysis scout".to_string()),
                    route_kind: Some("intent".to_string()),
                    intent: Some("analysis-scout".to_string()),
                    choice_id: Some("perception-map".to_string()),
                    choice_label: Some("Perception map".to_string()),
                    source: Some("garden-navigation-routes".to_string()),
                    rationale: Some("Start with perception before run prep.".to_string()),
                    action_json: Some("{\"tool\":\"workflow_book_choose\"}".to_string()),
                    route_action: None,
                    action_tool: None,
                    action_kind: None,
                    command_template: None,
                    argument_hash: None,
                    action_arguments: Vec::new(),
                }],
                authorization_flags: vec![WorkflowAuthorizationFlag {
                    subject: "urn:sophia:workflow-page-turn-decision:lab:demo:overview:perception-map:analysis-scout:shard-01:authorization:1".to_string(),
                    page_id: Some("execute".to_string()),
                    choice_id: Some("start-run-mcp".to_string()),
                    reason: Some("choice calls workflow_run_start and may start Choreograph sidecar work".to_string()),
                }],
            }],
        }
    }

    fn empty_snapshot() -> WorkflowBookSnapshot {
        WorkflowBookSnapshot {
            graph_id: "lab".to_string(),
            read_graph: "urn:mnemosyne:local:graph:lab:user:rdf".to_string(),
            workflows: Vec::new(),
            adventures: Vec::new(),
            decisions: Vec::new(),
        }
    }

    #[test]
    fn workflow_rows_merge_multi_valued_optional_facts_by_uri() {
        let mut workflows = Vec::new();
        let mut by_uri = BTreeMap::new();
        let mut by_name = BTreeMap::new();
        let mut first = BTreeMap::new();
        first.insert("workflow".to_string(), "<urn:sophia:wf:demo>".to_string());
        first.insert("name".to_string(), "\"demo\"".to_string());
        first.insert("description".to_string(), "\"Demo workflow\"".to_string());
        first.insert("whenToUse".to_string(), "\"Use when testing\"".to_string());
        first.insert("sha".to_string(), "\"abc123\"".to_string());
        let mut second = first.clone();
        second.insert(
            "whenToUse".to_string(),
            "\"Raw SPARQL enrichment\"".to_string(),
        );

        merge_workflow_summary_row(&mut workflows, &mut by_uri, &mut by_name, &first);
        merge_workflow_summary_row(&mut workflows, &mut by_uri, &mut by_name, &second);

        assert_eq!(workflows.len(), 1);
        assert_eq!(by_uri.get("urn:sophia:wf:demo").copied(), Some(0));
        assert_eq!(by_name.get("demo").copied(), Some(0));
        assert_eq!(workflows[0].name, "demo");
        assert_eq!(
            workflows[0].when_to_use.as_deref(),
            Some("Raw SPARQL enrichment")
        );
        assert_eq!(workflows[0].script_sha256.as_deref(), Some("abc123"));
    }

    #[test]
    fn catalog_opens_workflows_and_preserves_raw_sparql() {
        let snapshot = sample_snapshot();
        let book = render_workflow_book(&snapshot, None).expect("catalog renders");
        assert_eq!(book.start_page_id, "catalog");
        let catalog = book.pages.iter().find(|page| page.id == "catalog").unwrap();
        assert!(catalog
            .choices
            .iter()
            .any(|choice| choice.kind == "open-workflow"));
        assert!(catalog.scene.contains("workflow book"));
        assert_eq!(catalog.suggested_choice_id.as_deref(), Some("open-demo"));
        assert!(catalog
            .objects
            .iter()
            .any(|object| object.pointer("/kind").and_then(Value::as_str) == Some("workflow")));
        let raw = book.pages.iter().find(|page| page.id == "raw").unwrap();
        assert!(raw.choices.iter().any(|choice| choice.kind == "raw-sparql"));
        let raw_json = serde_json::to_value(raw).unwrap();
        assert!(raw_json.to_string().contains("SELECT ?workflow ?name ?sha"));
    }

    #[test]
    fn workflow_book_surfaces_phase_agent_and_execute_pages() {
        let snapshot = sample_snapshot();
        let book = render_workflow_book(&snapshot, Some("demo")).expect("workflow renders");
        assert_eq!(book.start_page_id, "overview");
        assert!(book.pages.iter().any(|page| page.id == "phase-1"));
        assert!(book.pages.iter().any(|page| page.id == "agent-extractor-a"));
        assert!(book.pages.iter().any(|page| page.id == "perception"));
        assert!(book.pages.iter().any(|page| page.id == "catalog"));
        let overview = book
            .pages
            .iter()
            .find(|page| page.id == "overview")
            .unwrap();
        assert!(overview.scene.contains("1 phase"));
        assert_eq!(
            overview.suggested_choice_id.as_deref(),
            Some("execute-async")
        );
        assert!(overview
            .objects
            .iter()
            .any(|object| object.pointer("/kind").and_then(Value::as_str) == Some("phase")));
        let run_statistics = overview
            .objects
            .iter()
            .find(|object| object.pointer("/kind").and_then(Value::as_str) == Some("runStatistics"))
            .expect("overview includes virtual run statistics");
        assert_eq!(
            run_statistics
                .pointer("/semanticClass")
                .and_then(Value::as_str),
            Some("wf:RunStatistics")
        );
        assert_eq!(
            run_statistics
                .pointer("/sourceKind")
                .and_then(Value::as_str),
            Some("derived")
        );
        assert_eq!(
            run_statistics.pointer("/storeMode").and_then(Value::as_str),
            Some("virtual")
        );
        assert_eq!(
            run_statistics
                .pointer("/identityKind")
                .and_then(Value::as_str),
            Some("resolve-by-query")
        );
        assert_eq!(
            run_statistics
                .pointer("/derivedFromQuery")
                .and_then(Value::as_str),
            Some(RUN_STATISTICS_DERIVED_FROM_QUERY)
        );
        assert_eq!(
            run_statistics.pointer("/runCount").and_then(Value::as_u64),
            Some(1)
        );
        assert_eq!(
            run_statistics.pointer("/lastRunAt").and_then(Value::as_str),
            Some("2026-06-24T10:00:00Z")
        );
        assert_eq!(
            run_statistics
                .pointer("/latestRunStatus")
                .and_then(Value::as_str),
            Some("completed")
        );
        assert_eq!(
            run_statistics
                .pointer("/medianDurationMs")
                .and_then(Value::as_u64),
            Some(1000)
        );
        assert_eq!(
            run_statistics
                .pointer("/medianRunTokens")
                .and_then(Value::as_u64),
            Some(20)
        );
        assert_eq!(
            run_statistics
                .pointer("/branchFrequency/0")
                .and_then(Value::as_str),
            Some("analysis-scout=1")
        );
        assert_eq!(
            run_statistics
                .pointer("/branchFrequencyRows/0/routeId")
                .and_then(Value::as_str),
            Some("analysis-scout")
        );
        assert_eq!(
            run_statistics
                .pointer("/branchFrequencyRows/0/count")
                .and_then(Value::as_u64),
            Some(1)
        );
        assert_eq!(
            run_statistics
                .pointer("/nodeReliabilityStatus")
                .and_then(Value::as_str),
            Some("unavailable")
        );
        assert!(overview
            .choices
            .iter()
            .any(|choice| choice.id == "perception-map"));
        let perception = book
            .pages
            .iter()
            .find(|page| page.id == "perception")
            .unwrap();
        assert_eq!(perception.kind, "perception");
        assert_eq!(
            perception.suggested_choice_id.as_deref(),
            Some("concept-phase-1")
        );
        assert!(perception
            .choices
            .iter()
            .any(|choice| choice.kind == "perceptual-sparql"));
        assert!(perception
            .choices
            .iter()
            .any(|choice| choice.kind == "conceptual-route"));
        let execute = book.pages.iter().find(|page| page.id == "execute").unwrap();
        assert_eq!(
            execute.suggested_choice_id.as_deref(),
            Some("start-run-with-source-block")
        );
        assert!(execute
            .warnings
            .iter()
            .any(|warning| warning.contains("dynamic source enabled")));
        assert!(execute.objects.iter().any(|object| {
            object.pointer("/kind").and_then(Value::as_str) == Some("sourceBlock")
                && object.pointer("/parsed").and_then(Value::as_bool) == Some(true)
        }));
        assert!(execute.objects.iter().any(|object| {
            object.pointer("/kind").and_then(Value::as_str) == Some("inputBlock")
                && object.pointer("/parsed").and_then(Value::as_bool) == Some(true)
        }));
        let start_with_source = execute
            .choices
            .iter()
            .find(|choice| choice.id == "start-run-with-source-block")
            .unwrap();
        assert_eq!(
            start_with_source
                .action
                .as_ref()
                .and_then(|action| action.pointer("/arguments/workflowArgsBlock/documentId"))
                .and_then(Value::as_str),
            Some("wf-input")
        );
        let submit = execute
            .choices
            .iter()
            .find(|choice| choice.id == "submit-run")
            .unwrap();
        assert_eq!(submit.kind, "http-request");
        assert!(submit
            .description
            .as_deref()
            .is_some_and(|value| { value.contains("proxied service facade") }));
        assert_eq!(
            submit
                .action
                .as_ref()
                .and_then(|action| action.pointer("/facade/path"))
                .and_then(Value::as_str),
            Some("/workflows/runs")
        );
        assert_eq!(
            submit
                .action
                .as_ref()
                .and_then(|action| action.pointer("/body/workflow_name"))
                .and_then(Value::as_str),
            Some("demo")
        );
        let validation = book
            .pages
            .iter()
            .find(|page| page.id == "validation")
            .unwrap();
        let validate_now = validation
            .choices
            .iter()
            .find(|choice| choice.id == "validate-now")
            .unwrap();
        assert_eq!(
            validate_now
                .action
                .as_ref()
                .and_then(|action| action.get("tool"))
                .and_then(Value::as_str),
            Some("workflow_book_validate")
        );
    }

    #[test]
    fn workflow_book_surfaces_contextual_intent_recommendation() {
        let snapshot = sample_snapshot();
        let book = render_workflow_book(&snapshot, Some("demo")).expect("workflow renders");
        let reports = validation_reports(&snapshot, Some("demo")).unwrap();
        let response = book_response(
            &snapshot,
            book,
            "overview",
            None,
            &reports,
            Some("analysis-scout"),
        )
        .unwrap();

        assert_eq!(
            response
                .pointer("/page/suggestedChoiceId")
                .and_then(Value::as_str),
            Some("execute-async")
        );
        assert_eq!(
            response
                .pointer("/page/intentRecommendation/choiceId")
                .and_then(Value::as_str),
            Some("perception-map")
        );
        assert_eq!(
            response
                .pointer("/page/intentRecommendation/source")
                .and_then(Value::as_str),
            Some("garden-intent-rule")
        );
        assert_eq!(
            response
                .pointer("/page/intentRecommendation/semanticClass")
                .and_then(Value::as_str),
            Some("wf:LiveRecommendedAction")
        );
        assert_eq!(
            response
                .pointer("/page/intentRecommendation/storeMode")
                .and_then(Value::as_str),
            Some("virtual")
        );
        assert_eq!(
            response
                .pointer("/page/intentRecommendation/identityKind")
                .and_then(Value::as_str),
            Some("resolve-by-query")
        );
        assert!(response
            .pointer("/page/intentRecommendation/derivedFromQuery")
            .and_then(Value::as_str)
            .is_some_and(|query| query.contains("retained PageView recommendation rows")));
        assert_eq!(
            response
                .pointer("/authoring/intentRecommendation/choiceId")
                .and_then(Value::as_str),
            Some("perception-map")
        );
        let routes = response
            .pointer("/page/navigationRoutes")
            .and_then(Value::as_array)
            .expect("page should expose navigation routes");
        assert!(routes.iter().any(|route| {
            route.get("id").and_then(Value::as_str) == Some("analysis-scout")
                && route.get("choiceId").and_then(Value::as_str) == Some("perception-map")
        }));
        let analysis_route = routes
            .iter()
            .find(|route| route.get("id").and_then(Value::as_str) == Some("analysis-scout"))
            .expect("analysis route");
        assert_eq!(
            analysis_route
                .pointer("/action/tool")
                .and_then(Value::as_str),
            Some("workflow_book_choose")
        );
        assert_eq!(
            analysis_route
                .pointer("/action/arguments/handoffIntent")
                .and_then(Value::as_str),
            Some("analysis-scout")
        );
        assert_eq!(
            analysis_route
                .pointer("/action/arguments/choiceId")
                .and_then(Value::as_str),
            Some("perception-map")
        );
        assert!(routes.iter().any(|route| {
            route.get("id").and_then(Value::as_str) == Some("run-prep")
                && route.get("choiceId").and_then(Value::as_str) == Some("execute-async")
        }));
        assert!(routes.iter().any(|route| {
            route.get("id").and_then(Value::as_str) == Some("decision-trail")
                && route.get("choiceId").and_then(Value::as_str) == Some("decision-trail")
        }));
        assert!(routes.iter().any(|route| {
            route.get("id").and_then(Value::as_str) == Some("raw-sparql")
                && route.get("choiceId").and_then(Value::as_str) == Some("raw-sparql")
        }));
        assert_eq!(
            response
                .pointer("/authoring/navigationRoutes/0/id")
                .and_then(Value::as_str),
            Some("analysis-scout")
        );
    }

    #[test]
    fn workflow_book_surfaces_virtual_authoring_draft() {
        let mut snapshot = sample_snapshot();
        let workflow = snapshot.workflows.first_mut().unwrap();
        workflow.script_sha256 = None;
        workflow.script_block = None;
        workflow.nodes.clear();

        let book = render_workflow_book(&snapshot, Some("demo")).expect("workflow renders");
        let overview = book
            .pages
            .iter()
            .find(|page| page.id == "overview")
            .expect("overview page");
        assert_eq!(
            overview.suggested_choice_id.as_deref(),
            Some("authoring-draft")
        );
        assert!(overview
            .choices
            .iter()
            .any(|choice| choice.id == "authoring-draft"));

        let draft = book
            .pages
            .iter()
            .find(|page| page.id == "authoring-draft")
            .expect("draft page");
        assert_eq!(draft.kind, "authoringDraft");
        assert_eq!(
            draft.suggested_choice_id.as_deref(),
            Some("compose-bind-source-block")
        );
        assert!(draft.objects.iter().any(|object| {
            object.pointer("/kind").and_then(Value::as_str) == Some("workflowDraft")
                && object.pointer("/semanticClass").and_then(Value::as_str) == Some("wf:Draft")
                && object.pointer("/storeMode").and_then(Value::as_str) == Some("virtual")
                && object.pointer("/identityKind").and_then(Value::as_str)
                    == Some("resolve-by-query")
                && object.pointer("/runnable").and_then(Value::as_bool) == Some(false)
        }));
        assert!(draft.objects.iter().any(|object| {
            object.pointer("/kind").and_then(Value::as_str) == Some("completenessGap")
                && object.pointer("/semanticClass").and_then(Value::as_str)
                    == Some("wf:CompletenessGap")
                && object.pointer("/gapBlocking").and_then(Value::as_bool) == Some(true)
                && object.pointer("/gapKind").and_then(Value::as_str) == Some("unbound-source")
                && object.pointer("/validationCode").and_then(Value::as_str)
                    == Some("wf:scriptSha256")
                && object
                    .pointer("/derivedFromQuery")
                    .and_then(Value::as_str)
                    .is_some_and(|query| query.contains("completenessGaps"))
        }));
        assert!(draft.objects.iter().any(|object| {
            object.pointer("/kind").and_then(Value::as_str) == Some("draftWarning")
                && object.pointer("/semanticClass").and_then(Value::as_str)
                    == Some("wf:DraftWarning")
                && object.pointer("/warningKind").and_then(Value::as_str) == Some("agentnode-empty")
                && object.pointer("/validationCode").and_then(Value::as_str)
                    == Some("wf:AgentNode.empty")
                && object
                    .pointer("/derivedFromQuery")
                    .and_then(Value::as_str)
                    .is_some_and(|query| query.contains("draft.warnings"))
        }));
        assert!(draft.sections.iter().any(|section| {
            section.pointer("/id").and_then(Value::as_str) == Some("completenessGaps")
                && section
                    .pointer("/items")
                    .and_then(Value::as_array)
                    .is_some_and(|items| !items.is_empty())
        }));

        let repair = draft
            .choices
            .iter()
            .find(|choice| choice.id == "compose-bind-source-block")
            .expect("source repair choice");
        assert_eq!(
            repair
                .action
                .as_ref()
                .and_then(|action| action.get("tool"))
                .and_then(Value::as_str),
            Some("workflow_book_compose")
        );

        let response =
            choose_from_book(&snapshot, book, "overview", "authoring-draft", None).unwrap();
        assert_eq!(
            response.pointer("/currentPageId").and_then(Value::as_str),
            Some("authoring-draft")
        );
        assert_eq!(
            response.pointer("/page/kind").and_then(Value::as_str),
            Some("authoringDraft")
        );
        assert_eq!(
            response
                .pointer("/authoring/validation/passed")
                .and_then(Value::as_bool),
            Some(false)
        );
    }

    #[test]
    fn workflow_page_view_rows_dedupe_legacy_correspondence() {
        let canonical =
            "urn:sophia:wf:page-view:lab:demo:overview:analysis-scout:0123456789abcdef0123456789abcdef";
        let legacy = "urn:sophia:workflow-adventure:lab:demo:overview:analysis-scout";
        let mut adventures = Vec::new();
        let mut by_subject = BTreeMap::new();

        let legacy_route = format!("{legacy}:route:analysis-scout");
        let legacy_flag = format!("{legacy}:authorization:run-prep");
        merge_workflow_adventure_row(
            &mut adventures,
            &mut by_subject,
            &row(&[
                ("adventure", legacy),
                ("generatedAt", "2026-06-25T03:48:00Z"),
                ("workflowName", "demo"),
                ("pageId", "overview"),
                ("intent", "analysis-scout"),
                ("recommendedRoute", "analysis-scout"),
                ("navigationRoute", legacy_route.as_str()),
                ("routeId", "analysis-scout"),
                ("routeLabel", "Analysis scout"),
                ("routeSource", "workflow-adventure-packet"),
                (
                    "routeActionJson",
                    "{\"tool\":\"workflow_compose_adventure\"}",
                ),
                ("flag", legacy_flag.as_str()),
                ("flagPageId", "overview"),
                ("flagChoiceId", "run-prep"),
                ("flagReason", "authorization-gated"),
            ]),
        );

        let canonical_route = format!("{canonical}:route:analysis-scout");
        let canonical_action = format!("{canonical_route}:action");
        let canonical_arg = format!("{canonical_action}:arg:graph-id");
        let canonical_flag = format!("{canonical}:authorization:run-prep");
        merge_workflow_adventure_row(
            &mut adventures,
            &mut by_subject,
            &row(&[
                ("adventure", canonical),
                ("supersededPageView", legacy),
                ("generatedAt", "2026-06-25T03:48:00Z"),
                ("workflowName", "demo"),
                ("pageId", "overview"),
                ("intent", "analysis-scout"),
                ("recommendedRoute", "analysis-scout"),
                ("navigationRoute", canonical_route.as_str()),
                ("routeId", "analysis-scout"),
                ("routeLabel", "Analysis scout"),
                ("routeSource", "workflow-page-view"),
                ("routeAction", canonical_action.as_str()),
                ("routeActionTool", "workflow_compose_adventure"),
                ("routeArgument", canonical_arg.as_str()),
                ("routeArgumentName", "graph-id"),
                ("routeArgumentValue", "lab"),
                ("flag", canonical_flag.as_str()),
                ("flagPageId", "overview"),
                ("flagChoiceId", "run-prep"),
                ("flagReason", "authorization-gated"),
            ]),
        );

        assert_eq!(adventures.len(), 1);
        assert_eq!(by_subject.get(canonical), Some(&0));
        assert_eq!(by_subject.get(legacy), Some(&0));

        let adventure = &adventures[0];
        assert_eq!(adventure.subject, canonical);
        assert_eq!(adventure.superseded_page_view.as_deref(), Some(legacy));
        assert_eq!(adventure.navigation_routes.len(), 1);
        let route = &adventure.navigation_routes[0];
        assert_eq!(route.subject, canonical_route);
        assert_eq!(route.route_id.as_deref(), Some("analysis-scout"));
        assert_eq!(route.source.as_deref(), Some("workflow-page-view"));
        assert_eq!(
            route.route_action.as_deref(),
            Some(canonical_action.as_str())
        );
        assert_eq!(
            route.action_json.as_deref(),
            Some("{\"tool\":\"workflow_compose_adventure\"}")
        );
        assert_eq!(route.action_arguments.len(), 1);
        assert_eq!(
            route.action_arguments[0].subject.as_str(),
            canonical_arg.as_str()
        );
        assert_eq!(adventure.authorization_flags.len(), 1);
        assert_eq!(adventure.authorization_flags[0].subject, canonical_flag);
    }

    #[test]
    fn workflow_book_surfaces_composition_trail() {
        let snapshot = sample_snapshot();
        let book = render_workflow_book(&snapshot, Some("demo")).expect("workflow renders");

        let overview = book
            .pages
            .iter()
            .find(|page| page.id == "overview")
            .unwrap();
        assert!(overview
            .choices
            .iter()
            .any(|choice| choice.id == "composition-trail"));

        let trail = book
            .pages
            .iter()
            .find(|page| page.id == "composition-trail")
            .expect("composition trail page");
        assert_eq!(trail.kind, "compositionTrail");
        assert_eq!(
            trail.suggested_choice_id.as_deref(),
            Some("perceive-composition-events")
        );
        assert!(trail.objects.iter().any(|object| {
            object.pointer("/kind").and_then(Value::as_str) == Some("compositionEvent")
                && object.pointer("/gestureKind").and_then(Value::as_str) == Some("create_workflow")
                && object
                    .pointer("/insertTriples")
                    .and_then(Value::as_array)
                    .is_some_and(|triples| !triples.is_empty())
                && object
                    .pointer("/agentTurnUri")
                    .and_then(Value::as_str)
                    .is_some()
                && object.pointer("/driverLease").and_then(Value::as_str) == Some("lease-demo")
        }));
        assert!(trail.objects.iter().any(|object| {
            object.pointer("/kind").and_then(Value::as_str) == Some("definitionFoldCheck")
                && object.pointer("/status").and_then(Value::as_str) == Some("converged")
                && object
                    .pointer("/missingFromFold")
                    .and_then(Value::as_array)
                    .is_some_and(Vec::is_empty)
                && object
                    .pointer("/extraFromFold")
                    .and_then(Value::as_array)
                    .is_some_and(Vec::is_empty)
        }));
        assert!(trail.facts.iter().any(|fact| {
            fact.pointer("/label").and_then(Value::as_str) == Some("definitionFoldStatus")
                && fact.pointer("/value").and_then(Value::as_str) == Some("converged")
        }));
        assert!(trail.sections.iter().any(|section| {
            section.pointer("/id").and_then(Value::as_str) == Some("definitionFold")
                && section.pointer("/items/0/status").and_then(Value::as_str) == Some("converged")
        }));
        assert!(trail.sections.iter().any(|section| {
            section.pointer("/id").and_then(Value::as_str) == Some("compositionEvents")
                && section
                    .pointer("/items/0/gestureKind")
                    .and_then(Value::as_str)
                    == Some("create_workflow")
        }));
        assert!(trail
            .choices
            .iter()
            .any(|choice| choice.id == "perceive-composition-events"));

        let response =
            choose_from_book(&snapshot, book, "overview", "composition-trail", None).unwrap();
        assert_eq!(
            response.pointer("/currentPageId").and_then(Value::as_str),
            Some("composition-trail")
        );
        assert_eq!(
            response.pointer("/page/kind").and_then(Value::as_str),
            Some("compositionTrail")
        );
    }

    #[test]
    fn workflow_book_fold_check_distinguishes_partial_and_diverged_logs() {
        let snapshot = sample_snapshot();
        let workflow = snapshot.workflows.first().unwrap();
        let converged = workflow_fold_check(workflow);
        assert_eq!(
            converged.pointer("/status").and_then(Value::as_str),
            Some("converged")
        );

        let mut partial = sample_snapshot();
        partial.workflows[0].composition_events[0]
            .insert_triples
            .clear();
        let partial_check = workflow_fold_check(partial.workflows.first().unwrap());
        assert_eq!(
            partial_check.pointer("/status").and_then(Value::as_str),
            Some("partial")
        );
        assert_eq!(
            partial_check
                .pointer("/opaqueEventCount")
                .and_then(Value::as_u64),
            Some(1)
        );

        let mut diverged = sample_snapshot();
        diverged.workflows[0].composition_events[0]
            .insert_triples
            .retain(|triple| !triple.contains("agentType"));
        let diverged_check = workflow_fold_check(diverged.workflows.first().unwrap());
        assert_eq!(
            diverged_check.pointer("/status").and_then(Value::as_str),
            Some("diverged")
        );
        assert!(diverged_check
            .pointer("/missingFromFold")
            .and_then(Value::as_array)
            .is_some_and(|missing| missing.iter().any(|triple| triple
                .as_str()
                .is_some_and(|value| value.contains("agentType")))));
    }

    #[test]
    fn workflow_book_surfaces_workflow_adventure_trail() {
        let snapshot = sample_snapshot();
        let book = render_workflow_book(&snapshot, Some("demo")).expect("workflow renders");
        assert!(book.pages.iter().any(|page| page.id == "adventure-trail"));

        let catalog = book.pages.iter().find(|page| page.id == "catalog").unwrap();
        assert_eq!(
            catalog
                .choices
                .iter()
                .filter(|choice| choice.id == "adventure-trail")
                .count(),
            1
        );
        let overview = book
            .pages
            .iter()
            .find(|page| page.id == "overview")
            .unwrap();
        assert!(overview
            .choices
            .iter()
            .any(|choice| choice.id == "adventure-trail"));
        let perception = book
            .pages
            .iter()
            .find(|page| page.id == "perception")
            .unwrap();
        assert!(perception
            .choices
            .iter()
            .any(|choice| choice.id == "adventure-trail"));

        let trail = book
            .pages
            .iter()
            .find(|page| page.id == "adventure-trail")
            .unwrap();
        assert_eq!(trail.kind, "adventureTrail");
        assert_eq!(
            trail.suggested_choice_id.as_deref(),
            Some("open-adventure-1")
        );
        assert!(trail.objects.iter().any(|object| {
            object.pointer("/kind").and_then(Value::as_str) == Some("workflowPageView")
                && object.pointer("/compatKind").and_then(Value::as_str)
                    == Some("workflowAdventurePacket")
                && object.pointer("/retainedClass").and_then(Value::as_str) == Some("wf:PageView")
                && object.pointer("/recommendedRoute").and_then(Value::as_str)
                    == Some("analysis-scout")
                && object
                    .pointer("/rawSparqlQueryCount")
                    .and_then(Value::as_u64)
                    == Some(1)
        }));
        assert!(trail.choices.iter().any(|choice| {
            choice.id == "perceive-workflow-adventures"
                && choice.kind == "perceptual-sparql"
                && choice
                    .action
                    .as_ref()
                    .and_then(|action| action.pointer("/arguments/query"))
                    .and_then(Value::as_str)
                    .is_some_and(|query| {
                        query.contains("wf:PageView")
                            && query.contains("wfui:WorkflowAdventurePacket")
                    })
        }));

        let detail_page_id = adventure_page_id(&snapshot.adventures[0].subject);
        let detail = book
            .pages
            .iter()
            .find(|page| page.id == detail_page_id)
            .unwrap();
        assert_eq!(detail.kind, "adventureDetail");
        assert!(detail.sections.iter().any(|section| {
            section.pointer("/id").and_then(Value::as_str) == Some("routeMap")
                && section
                    .pointer("/items/0/action/tool")
                    .and_then(Value::as_str)
                    == Some("workflow_compose_adventure")
                && section
                    .pointer("/items/0/action/arguments/0/name")
                    .and_then(Value::as_str)
                    == Some("graphId")
        }));
        assert!(detail.sections.iter().any(|section| {
            section.pointer("/id").and_then(Value::as_str) == Some("authorizationBoundary")
                && section
                    .pointer("/items/0/reason")
                    .and_then(Value::as_str)
                    .is_some_and(|reason| reason.contains("authorization-gated"))
        }));
        assert!(detail.sections.iter().any(|section| {
            section.pointer("/id").and_then(Value::as_str) == Some("rawSparql")
                && section
                    .pointer("/items/0/arguments/query")
                    .and_then(Value::as_str)
                    .is_some_and(|query| query.contains("SELECT ?workflow"))
        }));
        let retained_route = detail
            .choices
            .iter()
            .find(|choice| choice.id == "route-analysis-scout")
            .unwrap();
        assert_eq!(retained_route.kind, "retained-adventure-route");
        assert_eq!(
            retained_route
                .action
                .as_ref()
                .and_then(|action| action.pointer("/tool"))
                .and_then(Value::as_str),
            Some("workflow_compose_adventure")
        );
        assert_eq!(
            retained_route
                .action
                .as_ref()
                .and_then(|action| action.pointer("/source"))
                .and_then(Value::as_str),
            Some("wf:PageView")
        );
        assert_eq!(
            retained_route
                .action
                .as_ref()
                .and_then(|action| action.pointer("/requiresExternalExecutor"))
                .and_then(Value::as_bool),
            Some(true)
        );
        assert_eq!(
            retained_route
                .action
                .as_ref()
                .and_then(|action| action.pointer("/originalAction/tool"))
                .and_then(Value::as_str),
            Some("workflow_compose_adventure")
        );
        assert_eq!(
            retained_route
                .action
                .as_ref()
                .and_then(|action| action.pointer("/arguments/tool"))
                .and_then(Value::as_str),
            None
        );

        let detail_response = book_response(
            &snapshot,
            book.clone(),
            detail_page_id.as_str(),
            None,
            &[],
            Some("analysis-scout"),
        )
        .unwrap();
        let detail_routes = detail_response
            .pointer("/page/navigationRoutes")
            .and_then(Value::as_array)
            .unwrap();
        assert_eq!(
            detail_response
                .pointer("/page/intentRecommendation/source")
                .and_then(Value::as_str),
            Some("retained-page-view")
        );
        assert_eq!(
            detail_response
                .pointer("/page/intentRecommendation/semanticClass")
                .and_then(Value::as_str),
            Some("wf:RecommendedAction")
        );
        assert_eq!(
            detail_response
                .pointer("/page/intentRecommendation/storeMode")
                .and_then(Value::as_str),
            Some("materialize")
        );
        assert!(detail_response
            .pointer("/page/intentRecommendation/derivedFromQuery")
            .is_none());
        assert_eq!(
            detail_response
                .pointer("/page/intentRecommendation/choiceId")
                .and_then(Value::as_str),
            Some("route-analysis-scout")
        );
        assert!(detail_routes.iter().any(|route| {
            route.pointer("/id").and_then(Value::as_str) == Some("analysis-scout")
                && route.pointer("/kind").and_then(Value::as_str)
                    == Some("retained-adventure-route")
                && route
                    .pointer("/action/originalAction/tool")
                    .and_then(Value::as_str)
                    == Some("workflow_compose_adventure")
        }));

        let raw = book.pages.iter().find(|page| page.id == "raw").unwrap();
        assert!(raw
            .objects
            .iter()
            .any(|object| object.pointer("/curie").and_then(Value::as_str) == Some("wf:PageView")));
        assert!(raw
            .choices
            .iter()
            .any(|choice| choice.id == "adventure-trail"));
    }

    #[test]
    fn workflow_book_surfaces_page_turn_decision_trail() {
        let snapshot = sample_snapshot();
        let book = render_workflow_book(&snapshot, Some("demo")).expect("workflow renders");
        assert!(book.pages.iter().any(|page| page.id == "decision-trail"));

        let overview = book
            .pages
            .iter()
            .find(|page| page.id == "overview")
            .unwrap();
        assert!(overview
            .choices
            .iter()
            .any(|choice| choice.id == "decision-trail"));

        let trail = book
            .pages
            .iter()
            .find(|page| page.id == "decision-trail")
            .unwrap();
        assert_eq!(trail.kind, "decisionTrail");
        assert_eq!(
            trail.suggested_choice_id.as_deref(),
            Some("launch-decision-1")
        );
        let launch_page_id = decision_launch_page_id(&snapshot.decisions[0].subject);
        assert!(trail.choices.iter().any(|choice| {
            choice.id == "launch-decision-1"
                && choice.target_page_id.as_deref() == Some(launch_page_id.as_str())
        }));
        assert!(trail.sections.iter().any(|section| {
            section.pointer("/id").and_then(Value::as_str) == Some("pageTurnDecisions")
                && section
                    .pointer("/items/0/launchChoiceId")
                    .and_then(Value::as_str)
                    == Some("launch-decision-1")
                && section
                    .pointer("/items/0/launchPageId")
                    .and_then(Value::as_str)
                    == Some(launch_page_id.as_str())
        }));
        assert!(trail
            .objects
            .iter()
            .any(|object| object.pointer("/kind").and_then(Value::as_str)
                == Some("pageTurnDecision")));
        assert!(trail.objects.iter().any(|object| {
            object.pointer("/recommendedRoute").and_then(Value::as_str) == Some("analysis-scout")
                && object
                    .pointer("/recommendedRouteLabel")
                    .and_then(Value::as_str)
                    == Some("Analysis scout")
                && object.pointer("/followedRoute").and_then(Value::as_str)
                    == Some("analysis-scout")
        }));
        assert!(trail.choices.iter().any(|choice| {
            choice.id == "perceive-page-turn-decisions"
                && choice.kind == "perceptual-sparql"
                && choice
                    .action
                    .as_ref()
                    .and_then(|action| action.pointer("/arguments/query"))
                    .and_then(Value::as_str)
                    .is_some_and(|query| {
                        query.contains("wf:PageTurnDecision")
                            && query.contains("wfui:PageTurnDecision")
                    })
        }));

        let detail_page_id = decision_page_id(&snapshot.decisions[0].subject);
        let launch = book
            .pages
            .iter()
            .find(|page| page.id == launch_page_id)
            .unwrap();
        assert_eq!(launch.kind, "decisionLaunch");
        assert_eq!(
            launch.suggested_choice_id.as_deref(),
            Some("route-analysis-scout")
        );
        assert!(launch.objects.iter().any(|object| {
            object.pointer("/kind").and_then(Value::as_str) == Some("pageTurnDecisionLaunch")
                && object.pointer("/primaryRoute").and_then(Value::as_str) == Some("analysis-scout")
                && object.pointer("/primaryChoice").and_then(Value::as_str)
                    == Some("perception-map")
        }));
        assert!(launch.sections.iter().any(|section| {
            section.pointer("/id").and_then(Value::as_str) == Some("currentBranch")
                && section
                    .pointer("/items/0/routeStatus")
                    .and_then(Value::as_str)
                    == Some("followed")
        }));
        assert!(launch.sections.iter().any(|section| {
            section.pointer("/id").and_then(Value::as_str) == Some("authorizationBoundary")
        }));
        let launch_route = launch
            .choices
            .iter()
            .find(|choice| choice.id == "route-analysis-scout")
            .unwrap();
        assert_eq!(launch_route.kind, "retained-route");
        assert_eq!(
            launch_route
                .action
                .as_ref()
                .and_then(|action| action.pointer("/tool"))
                .and_then(Value::as_str),
            Some("workflow_book_choose")
        );
        let launch_response = book_response(
            &snapshot,
            book.clone(),
            launch_page_id.as_str(),
            None,
            &[],
            Some("analysis-scout"),
        )
        .unwrap();
        let launch_routes = launch_response
            .pointer("/page/navigationRoutes")
            .and_then(Value::as_array)
            .unwrap();
        assert!(launch_routes.iter().any(|route| {
            route.pointer("/id").and_then(Value::as_str) == Some("analysis-scout")
                && route.pointer("/choiceId").and_then(Value::as_str)
                    == Some("route-analysis-scout")
        }));

        let detail = book
            .pages
            .iter()
            .find(|page| page.id == detail_page_id)
            .unwrap();
        assert_eq!(detail.kind, "decisionDetail");
        assert!(detail.objects.iter().any(|object| {
            object.pointer("/kind").and_then(Value::as_str) == Some("navigationRoute")
                && object.pointer("/routeId").and_then(Value::as_str) == Some("analysis-scout")
                && object.pointer("/choiceId").and_then(Value::as_str) == Some("perception-map")
        }));
        assert!(detail.facts.iter().any(|fact| {
            fact.pointer("/label").and_then(Value::as_str) == Some("recommendedRoute")
                && fact.pointer("/value").and_then(Value::as_str) == Some("analysis-scout")
        }));
        assert!(detail.facts.iter().any(|fact| {
            fact.pointer("/label").and_then(Value::as_str) == Some("followedRoute")
                && fact.pointer("/value").and_then(Value::as_str) == Some("analysis-scout")
        }));
        assert!(detail
            .warnings
            .iter()
            .any(|warning| warning.contains("not authorized")));
        assert!(detail
            .sections
            .iter()
            .any(|section| section.pointer("/id").and_then(Value::as_str)
                == Some("navigationRoutes")));
        assert!(detail
            .sections
            .iter()
            .any(|section| section.pointer("/id").and_then(Value::as_str)
                == Some("authorizationFlags")));
        assert!(detail.choices.iter().any(|choice| {
            choice.id == "launch-page"
                && choice.target_page_id.as_deref() == Some(launch_page_id.as_str())
        }));
        let retained_route = detail
            .choices
            .iter()
            .find(|choice| choice.id == "route-analysis-scout")
            .unwrap();
        assert_eq!(retained_route.kind, "retained-route");
        assert_eq!(retained_route.label, "Route: Analysis scout");
        assert_eq!(
            retained_route
                .action
                .as_ref()
                .and_then(|action| action.pointer("/tool"))
                .and_then(Value::as_str),
            Some("workflow_book_choose")
        );
        assert_eq!(
            retained_route
                .action
                .as_ref()
                .and_then(|action| action.pointer("/arguments/choiceId"))
                .and_then(Value::as_str),
            Some("perception-map")
        );
        assert_eq!(
            retained_route
                .action
                .as_ref()
                .and_then(|action| action.pointer("/arguments/routeId"))
                .and_then(Value::as_str),
            Some("analysis-scout")
        );
        assert_eq!(
            retained_route
                .action
                .as_ref()
                .and_then(|action| action.pointer("/arguments/handoffIntent"))
                .and_then(Value::as_str),
            Some("analysis-scout")
        );
        let detail_response = book_response(
            &snapshot,
            book.clone(),
            detail_page_id.as_str(),
            None,
            &[],
            Some("analysis-scout"),
        )
        .unwrap();
        let detail_routes = detail_response
            .pointer("/page/navigationRoutes")
            .and_then(Value::as_array)
            .unwrap();
        let detail_analysis_route = detail_routes
            .iter()
            .find(|route| route.pointer("/id").and_then(Value::as_str) == Some("analysis-scout"))
            .unwrap();
        assert_eq!(
            detail_analysis_route
                .pointer("/choiceId")
                .and_then(Value::as_str),
            Some("route-analysis-scout")
        );
        assert_eq!(
            detail_analysis_route
                .pointer("/action/arguments/choiceId")
                .and_then(Value::as_str),
            Some("perception-map")
        );
        assert_eq!(
            detail_analysis_route
                .pointer("/action/arguments/routeId")
                .and_then(Value::as_str),
            Some("analysis-scout")
        );
        let replay = detail
            .choices
            .iter()
            .find(|choice| choice.id == "replay-recommended-choice")
            .unwrap();
        assert_eq!(
            replay
                .action
                .as_ref()
                .and_then(|action| action.pointer("/arguments/tool"))
                .and_then(Value::as_str),
            None
        );
        assert_eq!(
            replay
                .action
                .as_ref()
                .and_then(|action| action.pointer("/tool"))
                .and_then(Value::as_str),
            Some("workflow_book_choose")
        );
        assert_eq!(
            replay
                .action
                .as_ref()
                .and_then(|action| action.pointer("/arguments/choiceId"))
                .and_then(Value::as_str),
            Some("perception-map")
        );
        assert_eq!(
            replay
                .action
                .as_ref()
                .and_then(|action| action.pointer("/arguments/workflowName"))
                .and_then(Value::as_str),
            Some("demo")
        );

        let raw = book.pages.iter().find(|page| page.id == "raw").unwrap();
        assert!(raw
            .objects
            .iter()
            .any(|object| object.pointer("/curie").and_then(Value::as_str)
                == Some("wf:PageTurnDecision")));
        assert!(raw
            .objects
            .iter()
            .any(|object| object.pointer("/curie").and_then(Value::as_str)
                == Some("wfui:PageTurnDecision")));
        assert!(raw
            .choices
            .iter()
            .any(|choice| choice.id == "decision-trail"));
    }

    #[test]
    fn choices_navigate_or_return_actions_without_executing() {
        let snapshot = sample_snapshot();
        let catalog = render_workflow_book(&snapshot, None).expect("catalog renders");
        let opened = choose_from_book(&snapshot, catalog, "catalog", "open-demo", None).unwrap();
        assert_eq!(
            opened.pointer("/book/workflowName").and_then(Value::as_str),
            Some("demo")
        );
        assert_eq!(
            opened.pointer("/currentPageId").and_then(Value::as_str),
            Some("overview")
        );
        assert_eq!(
            opened.pointer("/authoring/status").and_then(Value::as_str),
            Some("ready")
        );
        assert_eq!(
            opened
                .pointer("/authoring/suggestedChoiceId")
                .and_then(Value::as_str),
            Some("execute-async")
        );
        assert_eq!(
            opened
                .pointer("/authoring/validation/refreshAction/tool")
                .and_then(Value::as_str),
            Some("workflow_book_validate")
        );

        let book = render_workflow_book(&snapshot, Some("demo")).expect("workflow renders");
        let response = choose_from_book(&snapshot, book, "execute", "submit-run", None).unwrap();
        assert_eq!(
            response.pointer("/currentPageId").and_then(Value::as_str),
            Some("execute")
        );
        assert_eq!(
            response
                .pointer("/selectedChoice/kind")
                .and_then(Value::as_str),
            Some("http-request")
        );
        assert_eq!(
            response
                .pointer("/authoring/status")
                .and_then(Value::as_str),
            Some("ready_to_execute")
        );
        assert!(response
            .pointer("/authoring/nextChoices")
            .and_then(Value::as_array)
            .is_some_and(|choices| choices
                .iter()
                .any(|choice| choice.pointer("/id").and_then(Value::as_str)
                    == Some("start-run-with-source-block"))));

        let book = render_workflow_book(&snapshot, Some("demo")).expect("workflow renders");
        let perception =
            choose_from_book(&snapshot, book, "overview", "perception-map", None).unwrap();
        assert_eq!(
            perception.pointer("/currentPageId").and_then(Value::as_str),
            Some("perception")
        );
        assert_eq!(
            perception
                .pointer("/authoring/perceptionAction/arguments/choiceId")
                .and_then(Value::as_str),
            Some("perception-map")
        );

        let book = render_workflow_book(&snapshot, Some("demo")).expect("workflow renders");
        let phase =
            choose_from_book(&snapshot, book, "perception", "concept-phase-1", None).unwrap();
        assert_eq!(
            phase.pointer("/currentPageId").and_then(Value::as_str),
            Some("phase-1")
        );

        let book = render_workflow_book(&snapshot, Some("demo")).expect("workflow renders");
        let percept =
            choose_from_book(&snapshot, book, "perception", "perceive-bound-blocks", None).unwrap();
        assert_eq!(
            percept
                .pointer("/selectedChoice/kind")
                .and_then(Value::as_str),
            Some("perceptual-sparql")
        );
        assert!(percept
            .pointer("/selectedChoice/action/arguments/query")
            .and_then(Value::as_str)
            .is_some_and(|query| query.contains("wf:inputBlock")));

        let book = render_workflow_book(&snapshot, Some("demo")).expect("workflow renders");
        let catalog = choose_from_book(&snapshot, book, "overview", "catalog", None).unwrap();
        assert_eq!(
            catalog.pointer("/currentPageId").and_then(Value::as_str),
            Some("catalog")
        );
    }

    #[test]
    fn workflow_book_apply_request_accepts_update_or_rdf_load() {
        assert_eq!(
            workflow_book_apply_request(&json!({
                "update": "INSERT DATA { <urn:s> <urn:p> <urn:o> }"
            }))
            .unwrap(),
            WorkflowBookApplyRequest::SparqlUpdate {
                update: "INSERT DATA { <urn:s> <urn:p> <urn:o> }".to_string()
            }
        );

        assert_eq!(
            workflow_book_apply_request(&json!({
                "operation": "rdf_load",
                "data": "<urn:s> <urn:p> <urn:o> .",
                "format": "n-triples",
                "base_iri": "urn:base:",
                "targetGraphIri": "urn:target"
            }))
            .unwrap(),
            WorkflowBookApplyRequest::RdfLoad {
                data: "<urn:s> <urn:p> <urn:o> .".to_string(),
                format: "n-triples".to_string(),
                base_iri: Some("urn:base:".to_string()),
                target_graph_iri: Some("urn:target".to_string()),
            }
        );

        assert!(workflow_book_apply_request(&json!({
            "update": "INSERT DATA { <urn:s> <urn:p> <urn:o> }",
            "data": "<urn:s> <urn:p> <urn:o> ."
        }))
        .is_err());
    }

    #[test]
    fn workflow_book_apply_event_plan_requires_workflow_scope() {
        let snapshot = sample_snapshot();
        assert!(workflow_book_apply_event_plan(
            &snapshot,
            &json!({
                "update": "INSERT DATA { <urn:s> <urn:p> <urn:o> }"
            }),
            "sparql_update"
        )
        .unwrap()
        .is_none());

        let event = workflow_book_apply_event_plan(
            &snapshot,
            &json!({
                "workflowName": "demo",
                "update": "INSERT DATA { <urn:s> <urn:p> <urn:o> }",
                "gestureKind": "manual_patch",
                "targetSubject": "urn:mnemosyne:local:graph:lab:doc:wf-demo#block-1",
                "driverAgent": "codex",
                "rationale": "Patch the definition cache after review.",
                "insertTriples": [
                    "<urn:mnemosyne:local:graph:lab:doc:wf-demo> <http://mnemosyne.dev/workflow#description> \"Patched\" ."
                ],
                "deleteTriples": [
                    "<urn:mnemosyne:local:graph:lab:doc:wf-demo> <http://mnemosyne.dev/workflow#description> \"Old\" ."
                ]
            }),
            "sparql_update",
        )
        .expect("apply event plan")
        .expect("workflow-scoped apply emits event");
        assert!(event
            .event_uri
            .starts_with("urn:sophia:wf:composition-event:lab:demo:"));
        assert!(event.triples.iter().any(|triple| triple.contains(&format!(
            "<{}> <{WF_NS}gestureKind> \"manual_patch\"",
            event.event_uri
        ))));
        assert!(event.triples.iter().any(|triple| triple.contains(&format!(
            "<{}> <{WF_NS}targetSubject> <urn:mnemosyne:local:graph:lab:doc:wf-demo#block-1>",
            event.event_uri
        ))));
        assert!(event.triples.iter().any(
            |triple| triple.contains(&format!("<{}> <{WF_NS}boundToAgent> <", event.event_uri))
        ));
        assert!(event
            .triples
            .iter()
            .any(|triple| triple.contains(&format!("a <{AGT_NS}Turn>"))));
        assert!(
            event
                .triples
                .iter()
                .any(|triple| triple
                    .contains(&format!("<{}> <{WF_NS}insertTriple>", event.event_uri)))
        );
        assert!(
            event
                .triples
                .iter()
                .any(|triple| triple
                    .contains(&format!("<{}> <{WF_NS}deleteTriple>", event.event_uri)))
        );
        let retained = composition_event_rationale_memory_record(&event)
            .expect("rationale yields retained memory");
        assert_eq!(retained.scope, "graph");
        assert_eq!(retained.kind, "SummaryMemory");
        assert_eq!(retained.content_orientation, "knowledge");
        assert_eq!(retained.visibility, "shared");
        assert_eq!(retained.status, "active");
        assert_eq!(retained.agent_id.as_deref(), Some("codex"));
        assert_eq!(retained.observer_agent_id, None);
        assert!(retained
            .content
            .contains("Patch the definition cache after review."));
        assert_eq!(retained.source_refs.len(), 1);
        assert_eq!(retained.source_refs[0].source_kind, "PlatformEvent");
        assert_eq!(
            retained.source_refs[0].external_uri.as_deref(),
            Some(event.event_uri.as_str())
        );
    }

    #[test]
    fn workflow_book_compose_plan_builds_named_updates() {
        let create = workflow_book_compose_plan(
            &empty_snapshot(),
            &json!({
                "operation": "create_workflow",
                "workflowName": "new demo",
                "description": "A composed workflow",
                "whenToUse": "When testing compose",
                "sourceDocumentId": "source-doc",
                "sourceBlockId": "block-1",
                "inputDocumentId": "input-doc",
                "inputBlockId": "input-1",
                "scriptSha256": "def456",
                "seededFrom": "urn:sophia:workflow-template:analysis"
            }),
        )
        .expect("create plan");
        assert_eq!(create.operation, "create_workflow");
        assert_eq!(create.workflow_name, "new demo");
        assert_eq!(create.target_page_id, "overview");
        assert_eq!(
            create.definition_projection_source,
            COMPOSITION_FOLD_PROJECTION_SOURCE
        );
        assert_eq!(create.definition_insert_count, 8);
        assert_eq!(create.definition_delete_count, 0);
        assert!(create
            .update
            .contains("GRAPH <urn:mnemosyne:local:graph:lab:user:rdf>"));
        assert!(create.update.contains(&format!("<{WF_NS}Workflow>")));
        assert!(create.update.contains(&format!(
            "<{}> <{WF_NS}name> \"new demo\"",
            create.workflow_uri
        )));
        assert!(create
            .update
            .contains("<urn:mnemosyne:local:graph:lab:doc:source-doc#block-1>"));
        assert!(create
            .update
            .contains("<urn:mnemosyne:local:graph:lab:doc:input-doc#input-1>"));
        assert!(create.update.contains(&format!(
            "<{}> <{WF_NS}seededFrom> <urn:sophia:workflow-template:analysis>",
            create.workflow_uri
        )));
        assert!(create
            .composition_event_uri
            .starts_with("urn:sophia:wf:composition-event:lab:new-demo:"));
        assert!(create
            .authoring_session_uri
            .starts_with("urn:sophia:wf:authoring-session:lab:new-demo:"));
        assert!(create.update.contains(&format!(
            "<{}> a <{WF_NS}CompositionEvent>",
            create.composition_event_uri
        )));
        assert!(create.update.contains(&format!(
            "<{}> <{WF_NS}hasCompositionEvent> <{}>",
            create.authoring_session_uri, create.composition_event_uri
        )));
        assert!(create.update.contains(&format!(
            "<{}> <{WF_NS}partOfAuthoringSession> <{}>",
            create.composition_event_uri, create.authoring_session_uri
        )));
        assert!(create.update.contains(&format!(
            "<{}> <{WF_NS}targetSubject> <{}>",
            create.composition_event_uri, create.workflow_uri
        )));
        assert!(create.update.contains(&format!(
            "<{}> <{WF_NS}insertTriple>",
            create.composition_event_uri
        )));
        let create_event_offset = create
            .update
            .find(&format!(
                "<{}> a <{WF_NS}CompositionEvent>",
                create.composition_event_uri
            ))
            .expect("event appears in create update");
        let create_definition_offset = create
            .update
            .find(&format!("<{}> a <{WF_NS}Workflow>", create.workflow_uri))
            .expect("definition cache appears in create update");
        assert!(create_event_offset < create_definition_offset);
        let create_agent_session_uri =
            minted_authoring_agent_session_uri(&create.authoring_session_uri);
        let create_agent_turn_uri = minted_authoring_agent_turn_uri(
            &create_agent_session_uri,
            create.composition_event_order,
        );
        assert!(create.update.contains(&format!(
            "<{}> <{WF_NS}boundToAgent> <{}>",
            create.authoring_session_uri, create_agent_session_uri
        )));
        assert!(create
            .update
            .contains(&format!("<{create_agent_session_uri}> a <{AGT_NS}Session>")));
        assert!(create.update.contains(&format!(
            "<{create_agent_session_uri}> <{AGT_NS}realizedBy> <{}>",
            create.authoring_session_uri
        )));
        assert!(create.update.contains(&format!(
            "<{}> <{WF_NS}boundToAgent> <{}>",
            create.composition_event_uri, create_agent_turn_uri
        )));
        assert!(create
            .update
            .contains(&format!("<{create_agent_turn_uri}> a <{AGT_NS}Turn>")));
        assert!(create.update.contains(&format!(
            "<{create_agent_turn_uri}> <{AGT_NS}inSession> <{create_agent_session_uri}>"
        )));
        assert!(create.update.contains(&format!(
            "<{create_agent_turn_uri}> <{AGT_NS}realizedBy> <{}>",
            create.composition_event_uri
        )));
        assert!(create.update.contains(&format!(
            "<{}> <{WF_NS}seededFrom> <urn:sophia:workflow-template:analysis>",
            create.composition_event_uri
        )));
        assert!(create
            .update
            .contains("<http://www.w3.org/2001/XMLSchema#dateTime>"));

        let explicit_session = workflow_book_compose_plan(
            &empty_snapshot(),
            &json!({
                "operation": "create_workflow",
                "workflowName": "session demo",
                "authoringSessionUri": "urn:sophia:wf:authoring-session:lab:session-demo:session-1",
                "driverAgent": "codex",
                "driverLease": "lease-explicit",
                "rationale": "Test caller-owned session linkage"
            }),
        )
        .expect("explicit session plan");
        assert_eq!(
            explicit_session.authoring_session_uri,
            "urn:sophia:wf:authoring-session:lab:session-demo:session-1"
        );
        assert!(explicit_session
            .composition_event_uri
            .contains(":session-1:"));
        assert!(explicit_session.update.contains(&format!(
            "<{}> <{WF_NS}driverAgent> \"codex\"",
            explicit_session.composition_event_uri
        )));
        assert!(explicit_session.update.contains(&format!(
            "<{}> <{WF_NS}rationale> \"Test caller-owned session linkage\"",
            explicit_session.composition_event_uri
        )));
        assert!(explicit_session.update.contains(&format!(
            "<{}> <{WF_NS}driverLease> \"lease-explicit\"",
            explicit_session.authoring_session_uri
        )));
        let explicit_agent_session_uri =
            minted_authoring_agent_session_uri(&explicit_session.authoring_session_uri);
        assert!(explicit_session.update.contains(&format!(
            "<{}> <{WF_NS}boundToAgent> <{}>",
            explicit_session.authoring_session_uri, explicit_agent_session_uri
        )));
        assert!(explicit_session.update.contains(&format!(
            "<{explicit_agent_session_uri}> a <{AGT_NS}Session>"
        )));
        assert!(!explicit_session
            .update
            .contains(&format!("a <{WF_NS}AuthoringSession>")));
        let retained = explicit_session
            .retained_memory_record
            .as_ref()
            .expect("rationale yields retained memory");
        assert_eq!(retained.kind, "SummaryMemory");
        assert_eq!(retained.source_refs[0].source_kind, "PlatformEvent");
        assert_eq!(
            retained.source_refs[0].external_uri.as_deref(),
            Some(explicit_session.composition_event_uri.as_str())
        );
        assert!(retained
            .content
            .contains("Test caller-owned session linkage"));
        assert_eq!(retained.agent_id.as_deref(), Some("codex"));
        assert_eq!(retained.observer_agent_id, None);

        let snapshot = sample_snapshot();
        let leased_phase = workflow_book_compose_plan(
            &snapshot,
            &json!({
                "operation": "add_phase",
                "workflowName": "demo",
                "phaseOrder": 2,
                "phaseTitle": "Review",
                "authoringSessionUri": "urn:sophia:wf:authoring-session:lab:demo:session-1",
                "driverLease": "lease-demo"
            }),
        )
        .expect("matching driver lease permits existing session");
        assert_eq!(
            leased_phase.authoring_session_uri,
            "urn:sophia:wf:authoring-session:lab:demo:session-1"
        );
        assert!(!leased_phase.update.contains(&format!(
            "<{}> <{WF_NS}driverLease> \"lease-demo\"",
            leased_phase.authoring_session_uri
        )));
        let missing_lease = workflow_book_compose_plan(
            &snapshot,
            &json!({
                "operation": "add_phase",
                "workflowName": "demo",
                "phaseOrder": 2,
                "phaseTitle": "Review",
                "authoringSessionUri": "urn:sophia:wf:authoring-session:lab:demo:session-1"
            }),
        )
        .expect_err("missing driver lease is rejected");
        assert!(missing_lease.contains("driver-leased"));
        let mismatched_lease = workflow_book_compose_plan(
            &snapshot,
            &json!({
                "operation": "add_phase",
                "workflowName": "demo",
                "phaseOrder": 2,
                "phaseTitle": "Review",
                "authoringSessionUri": "urn:sophia:wf:authoring-session:lab:demo:session-1",
                "driverLease": "wrong-lease"
            }),
        )
        .expect_err("mismatched driver lease is rejected");
        assert!(mismatched_lease.contains("does not match"));

        let phase = workflow_book_compose_plan(
            &snapshot,
            &json!({
                "operation": "add_phase",
                "workflowName": "demo",
                "phaseOrder": 2,
                "phaseTitle": "Review",
                "phaseDescription": "Check the output",
                "seededFrom": [
                    "urn:sophia:phase-template:review",
                    "urn:sophia:phase-template:critique"
                ]
            }),
        )
        .expect("phase plan");
        assert_eq!(phase.operation, "add_phase");
        assert_eq!(phase.target_page_id, "phase-2");
        assert_eq!(
            phase.definition_projection_source,
            COMPOSITION_FOLD_PROJECTION_SOURCE
        );
        assert_eq!(phase.definition_insert_count, 7);
        assert_eq!(phase.definition_delete_count, 0);
        assert!(phase
            .update
            .contains(&format!("<{}> <{WF_NS}phase>", snapshot.workflows[0].uri)));
        assert!(phase
            .update
            .contains(&format!("<{DCTERMS_NS}title> \"Review\"")));
        assert!(phase.update.contains(&format!(
            "<{WF_NS}seededFrom> <urn:sophia:phase-template:review>"
        )));
        assert!(phase.update.contains(&format!(
            "<{WF_NS}seededFrom> <urn:sophia:phase-template:critique>"
        )));
        assert!(phase.update.contains(&format!(
            "<{}> <{WF_NS}gestureKind> \"add_phase\"",
            phase.composition_event_uri
        )));
        assert!(phase.update.contains(&format!(
            "<{}> <{WF_NS}targetSubject> <urn:sophia:workflow:lab:demo:phase:2>",
            phase.composition_event_uri
        )));
        assert!(phase.update.contains(&format!(
            "<{}> <{WF_NS}seededFrom> <urn:sophia:phase-template:review>",
            phase.composition_event_uri
        )));
        assert!(phase.update.contains(&format!(
            "<{}> <{WF_NS}insertTriple>",
            phase.composition_event_uri
        )));

        let node = workflow_book_compose_plan(
            &snapshot,
            &json!({
                "operation": "attach_agent_node",
                "workflowName": "demo",
                "label": "synth",
                "phaseIndex": 1,
                "agentType": "scout",
                "documentId": "agent-synth",
                "seeded_from": "urn:sophia:agent-template:synth"
            }),
        )
        .expect("node plan");
        assert_eq!(node.operation, "attach_agent_node");
        assert_eq!(node.target_page_id, "agent-synth");
        assert_eq!(
            node.definition_projection_source,
            COMPOSITION_FOLD_PROJECTION_SOURCE
        );
        assert_eq!(node.definition_insert_count, 6);
        assert_eq!(node.definition_delete_count, 0);
        assert!(node
            .update
            .contains("<urn:mnemosyne:local:graph:lab:doc:agent-synth>"));
        assert!(node.update.contains(&format!(
            "<{WF_NS}partOfWorkflow> <{}>",
            snapshot.workflows[0].uri
        )));
        assert!(node.update.contains(&format!(
            "<{WF_NS}seededFrom> <urn:sophia:agent-template:synth>"
        )));
        assert!(node.update.contains(&format!(
            "<{}> <{WF_NS}gestureKind> \"attach_agent_node\"",
            node.composition_event_uri
        )));
        assert!(node.update.contains(&format!(
            "<{}> <{WF_NS}targetSubject> <urn:mnemosyne:local:graph:lab:doc:agent-synth>",
            node.composition_event_uri
        )));
        assert!(node.update.contains(&format!(
            "<{}> <{WF_NS}insertTriple>",
            node.composition_event_uri
        )));

        let bind = workflow_book_compose_plan(
            &snapshot,
            &json!({
                "operation": "bind_source_block",
                "workflowName": "demo",
                "sourceDocumentId": "source-doc",
                "sourceBlockId": "block-2",
                "scriptSha256": "new-sha"
            }),
        )
        .expect("bind plan");
        assert_eq!(bind.operation, "bind_source_block");
        assert_eq!(bind.target_page_id, "execute");
        assert_eq!(
            bind.definition_projection_source,
            COMPOSITION_FOLD_PROJECTION_SOURCE
        );
        assert_eq!(bind.definition_insert_count, 2);
        assert_eq!(bind.definition_delete_count, 2);
        assert!(bind.update.contains("DELETE DATA"));
        assert!(bind
            .update
            .contains("GRAPH <urn:mnemosyne:local:graph:lab:user:rdf>"));
        assert!(bind
            .update
            .contains("<urn:mnemosyne:local:graph:lab:doc:source-doc#block-2>"));
        assert!(bind.update.contains("\"new-sha\""));
        assert!(bind.update.contains(";\nINSERT DATA"));
        assert!(bind.update.contains(&format!(
            "<{}> <{WF_NS}gestureKind> \"bind_source_block\"",
            bind.composition_event_uri
        )));
        assert!(bind.update.contains(&format!(
            "<{}> <{WF_NS}insertTriple>",
            bind.composition_event_uri
        )));
        assert!(bind.update.contains(&format!(
            "<{}> <{WF_NS}deleteTriple>",
            bind.composition_event_uri
        )));
        assert!(bind.update.contains("abc123"));
        assert!(!bind.update.contains("\nWHERE"));

        let input = workflow_book_compose_plan(
            &snapshot,
            &json!({
                "operation": "bind_input_block",
                "workflowName": "demo",
                "inputDocumentId": "input-doc",
                "inputBlockId": "input-2"
            }),
        )
        .expect("input bind plan");
        assert_eq!(input.operation, "bind_input_block");
        assert_eq!(input.target_page_id, "execute");
        assert_eq!(
            input.definition_projection_source,
            COMPOSITION_FOLD_PROJECTION_SOURCE
        );
        assert_eq!(input.definition_insert_count, 1);
        assert_eq!(input.definition_delete_count, 1);
        assert!(input.update.contains(&format!("<{WF_NS}inputBlock>")));
        assert!(input
            .update
            .contains("<urn:mnemosyne:local:graph:lab:doc:input-doc#input-2>"));
        assert!(input.update.contains(";\nINSERT DATA"));
        assert!(input.update.contains(&format!(
            "<{}> <{WF_NS}gestureKind> \"bind_input_block\"",
            input.composition_event_uri
        )));
        assert!(input.update.contains(&format!(
            "<{}> <{WF_NS}insertTriple>",
            input.composition_event_uri
        )));
        assert!(input.update.contains(&format!(
            "<{}> <{WF_NS}deleteTriple>",
            input.composition_event_uri
        )));
    }

    #[test]
    fn workflow_book_compose_plan_guards_invalid_moves() {
        let snapshot = sample_snapshot();
        assert!(workflow_book_compose_plan(
            &snapshot,
            &json!({
                "operation": "create_workflow",
                "workflowName": "demo"
            }),
        )
        .is_err());
        assert!(workflow_book_compose_plan(
            &snapshot,
            &json!({
                "operation": "add_phase",
                "workflowName": "demo",
                "phaseOrder": 1
            }),
        )
        .is_err());
        assert!(workflow_book_compose_plan(
            &snapshot,
            &json!({
                "operation": "attach_agent_node",
                "workflowName": "demo",
                "label": "extractor-a",
                "phaseIndex": 1
            }),
        )
        .is_err());
        assert!(workflow_book_compose_plan(
            &snapshot,
            &json!({
                "operation": "bind_source_block",
                "workflowName": "demo"
            }),
        )
        .is_err());
        assert!(workflow_book_compose_plan(
            &snapshot,
            &json!({
                "operation": "bind_input_block",
                "workflowName": "demo"
            }),
        )
        .is_err());
        assert!(workflow_book_compose_plan(
            &empty_snapshot(),
            &json!({
                "operation": "create_workflow",
                "workflowName": "bad seed",
                "seededFrom": "urn:sophia:bad seed"
            }),
        )
        .is_err());
        assert!(workflow_book_compose_plan(
            &empty_snapshot(),
            &json!({
                "operation": "create_workflow",
                "workflowName": "bad event",
                "compositionEventUri": "urn:sophia:bad event"
            }),
        )
        .is_err());
    }

    #[test]
    fn validation_reports_pass_and_explain_broken_anatomy() {
        let snapshot = sample_snapshot();
        let reports = validation_reports(&snapshot, Some("demo")).expect("validate demo");
        assert_eq!(reports.len(), 1);
        assert!(reports[0].passed);
        assert_eq!(reports[0].errors, 0);

        let mut broken = sample_snapshot();
        let workflow = broken.workflows.first_mut().unwrap();
        workflow.script_sha256 = None;
        workflow.script_block = None;
        workflow.phases.push(WorkflowPhase {
            uri: "urn:sophia:wf:demo:phase:duplicate".to_string(),
            order: 1,
            title: "Again".to_string(),
            description: None,
            seeded_from: Vec::new(),
        });
        workflow.nodes.push(WorkflowAgentNode {
            uri: "urn:mnemosyne:local:graph:lab:doc:wf-demo-n-b".to_string(),
            label: "extractor-a".to_string(),
            phase_index: 99,
            agent_type: None,
            doc_id: Some("wf-demo-n-b".to_string()),
            seeded_from: Vec::new(),
        });

        let report = validate_workflow(workflow);
        assert!(!report.passed);
        assert!(report.errors >= 4, "{report:?}");
        let codes = report
            .issues
            .iter()
            .map(|issue| issue.code.as_str())
            .collect::<Vec<_>>();
        assert!(codes.contains(&"wf:scriptSha256"));
        assert!(codes.contains(&"wf:scriptBlock"));
        assert!(codes.contains(&"wf:order.duplicate"));
        assert!(codes.contains(&"wf:phaseIndex.unresolved"));
        assert!(codes.contains(&"wf:label.duplicate"));
    }
}
