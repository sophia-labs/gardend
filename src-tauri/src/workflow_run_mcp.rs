use crate::{
    app_error::{AppError, AppResult},
    app_runtime::AppHandle,
    document_mcp_blocks::mcp_local_get_block,
    emporium::{survey::parse_term, terms::Term},
    local_service_host::{LocalServiceState, CHOREOGRAPH_SERVICE_ID},
    loopback_state::LoopbackState,
    mcp_arg_utils::{mcp_arg_bool, mcp_arg_string, mcp_arg_u64, mcp_required_graph_id},
    rdf::sparql_string_literal,
    rdf_authority::user_rdf_graph_iri,
    rdf_service::{
        run_sparql_query_service, run_sparql_update_service, SparqlInput, SparqlUpdateInput,
    },
    runtime_config::PROFILE_ID,
    workflow_book_mcp::{mcp_local_workflow_book_open, mcp_local_workflow_book_validate},
};
use chrono::{SecondsFormat, Utc};
use reqwest::Method;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{collections::BTreeSet, fmt, sync::Arc};
#[cfg(feature = "desktop")]
use tauri::Manager;
use tokio::time::{sleep, Duration, Instant};

const WF_NS: &str = "http://mnemosyne.dev/workflow#";
const PROV_NS: &str = "http://www.w3.org/ns/prov#";
const CHOREOGRAPH_RUN_PATH: &str = "/api/workflows/run";
const CHOREOGRAPH_HEALTH_PATH: &str = "/health";
const CHOREOGRAPH_READY_TIMEOUT_MS: u64 = 15_000;
const CHOREOGRAPH_READY_POLL_MS: u64 = 200;

#[derive(Debug, Clone)]
pub(crate) struct ExecutableWorkflowDefinition {
    pub(crate) workflow_name: String,
    pub(crate) workflow_uri: String,
    pub(crate) definition_digest: String,
    pub(crate) source: String,
}

#[derive(Debug, Clone)]
pub(crate) struct WorkflowExecutionResolutionError {
    pub(crate) code: &'static str,
    pub(crate) message: String,
}

impl WorkflowExecutionResolutionError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl fmt::Display for WorkflowExecutionResolutionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

/// Resolve and pin the executable source behind a published WorkflowBinding.
/// The durable workflow subject, its declared digest, and the source block bytes
/// must all agree. Multiple graph answers fail closed as contested testimony.
pub(crate) async fn resolve_executable_workflow(
    app: AppHandle,
    graph_id: &str,
    workflow_uri: &str,
    workflow_name: &str,
    expected_digest: &str,
) -> Result<ExecutableWorkflowDefinition, WorkflowExecutionResolutionError> {
    validate_embedded_iri(workflow_uri).map_err(|message| {
        WorkflowExecutionResolutionError::new("invalid_workflow_binding", message)
    })?;
    let read_graph = user_rdf_graph_iri(graph_id);
    let query = format!(
        "PREFIX wf: <{WF_NS}>\nSELECT ?name ?scriptBlock ?sha WHERE {{\n  GRAPH <{read_graph}> {{\n    <{workflow_uri}> a wf:Workflow ;\n      wf:name ?name ;\n      wf:scriptBlock ?scriptBlock ;\n      wf:scriptSha256 ?sha .\n  }}\n}}"
    );
    let result = run_sparql_query_service(
        app.clone(),
        SparqlInput {
            graph_id: graph_id.to_string(),
            query,
        },
    )
    .map_err(|error| {
        WorkflowExecutionResolutionError::new(
            "workflow_resolution_failed",
            format!("read workflow definition: {error}"),
        )
    })?;
    let candidates = result
        .rows
        .iter()
        .filter_map(|row| {
            Some((
                rdf_term_value(row.get("name")?),
                rdf_term_value(row.get("scriptBlock")?),
                rdf_term_value(row.get("sha")?),
            ))
        })
        .collect::<BTreeSet<_>>();
    if candidates.is_empty() {
        return Err(WorkflowExecutionResolutionError::new(
            "workflow_definition_incomplete",
            format!(
                "workflow '{workflow_uri}' must have exactly one wf:name, wf:scriptBlock, and wf:scriptSha256"
            ),
        ));
    }
    if candidates.len() != 1 {
        return Err(WorkflowExecutionResolutionError::new(
            "contested_executable_binding",
            format!(
                "workflow '{workflow_uri}' resolves to {} executable source candidates",
                candidates.len()
            ),
        ));
    }
    let (resolved_name, script_block, resolved_digest) = candidates
        .into_iter()
        .next()
        .expect("checked one candidate");
    if resolved_name != workflow_name {
        return Err(WorkflowExecutionResolutionError::new(
            "workflow_binding_mismatch",
            format!(
                "binding names workflow '{workflow_name}' but subject '{workflow_uri}' is named '{resolved_name}'"
            ),
        ));
    }
    if resolved_digest != expected_digest {
        return Err(WorkflowExecutionResolutionError::new(
            "definition_digest_mismatch",
            format!(
                "binding digest '{expected_digest}' does not match workflow digest '{resolved_digest}'"
            ),
        ));
    }
    let (document_id, block_id) =
        local_doc_block_ref(graph_id, &script_block).ok_or_else(|| {
            WorkflowExecutionResolutionError::new(
                "workflow_definition_incomplete",
                format!("wf:scriptBlock '{script_block}' is not a block in graph '{graph_id}'"),
            )
        })?;
    let block = mcp_local_get_block(
        app,
        &json!({
            "graphId": graph_id,
            "documentId": document_id,
            "blockId": block_id,
            "format": "text",
        }),
    )
    .await
    .map_err(|error| {
        WorkflowExecutionResolutionError::new(
            "workflow_source_unavailable",
            format!("read wf:scriptBlock: {error}"),
        )
    })?;
    let source = block
        .pointer("/block/text")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            WorkflowExecutionResolutionError::new(
                "workflow_source_unavailable",
                "wf:scriptBlock did not return text",
            )
        })?
        .to_string();
    let actual_digest = format!("{:x}", Sha256::digest(source.as_bytes()));
    if actual_digest != expected_digest {
        return Err(WorkflowExecutionResolutionError::new(
            "definition_digest_mismatch",
            format!(
                "source block digest '{actual_digest}' does not match pinned digest '{expected_digest}'"
            ),
        ));
    }
    Ok(ExecutableWorkflowDefinition {
        workflow_name: workflow_name.to_string(),
        workflow_uri: workflow_uri.to_string(),
        definition_digest: expected_digest.to_string(),
        source,
    })
}

fn validate_embedded_iri(value: &str) -> Result<(), String> {
    const FORBIDDEN: &[char] = &['<', '>', '"', '{', '}', '|', '^', '\\', '`', ' '];
    if value.is_empty()
        || value
            .chars()
            .any(|character| character.is_control() || FORBIDDEN.contains(&character))
    {
        return Err("workflow URI is not safe to embed in a SPARQL IRI reference".to_string());
    }
    Ok(())
}

fn rdf_term_value(value: &str) -> String {
    match parse_term(value) {
        Term::Uri(node) => node.as_str().to_string(),
        Term::Lit(literal) => literal.value().to_string(),
        Term::Placeholder(name) => format!("urn:wf-emit:placeholder:{name}"),
    }
}

fn local_doc_block_ref(graph_id: &str, uri: &str) -> Option<(String, String)> {
    let prefix = format!("urn:mnemosyne:local:graph:{graph_id}:doc:");
    let tail = uri.strip_prefix(&prefix)?;
    let (document_id, block_id) = tail.split_once('#')?;
    if document_id.is_empty() || block_id.is_empty() || block_id.contains('#') {
        return None;
    }
    Some((document_id.to_string(), block_id.to_string()))
}

pub(super) async fn mcp_local_workflow_run_start(
    app: AppHandle,
    arguments: &Value,
) -> AppResult<Value> {
    let graph_id = mcp_required_graph_id(arguments).map_err(AppError::validation)?;
    let workflow_name = mcp_arg_string(arguments, &["workflowName", "workflow_name"])
        .ok_or_else(|| AppError::validation("workflowName is required"))?;
    let allow_invalid = mcp_arg_bool(arguments, &["allowInvalid", "allow_invalid"], false);
    let auto_start = mcp_arg_bool(arguments, &["autoStart", "auto_start"], true);

    let validation_args = json!({
        "graphId": graph_id,
        "workflowName": workflow_name,
    });
    let validation = mcp_local_workflow_book_validate(app.clone(), &validation_args)?;
    let validation_passed = validation
        .get("passed")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if !validation_passed && !allow_invalid {
        return Ok(json!({
            "kind": "workflowRunStart",
            "status": "validation_failed",
            "graphId": graph_id,
            "workflowName": workflow_name,
            "validationReport": validation,
            "message": "Workflow validation failed; pass allowInvalid=true to submit anyway.",
        }));
    }

    let state = local_loopback_state(&app)?;
    let mut service_status = state.services.status(CHOREOGRAPH_SERVICE_ID, &state.app);
    if !matches!(service_status.state, LocalServiceState::Running) && auto_start {
        match state.services.start(
            CHOREOGRAPH_SERVICE_ID,
            &state.app,
            &state.manifest,
            &state.token,
        ) {
            Ok(status) => service_status = status,
            Err(error) => {
                return Ok(json!({
                    "kind": "workflowRunStart",
                    "status": "service_unavailable",
                    "graphId": graph_id,
                    "workflowName": workflow_name,
                    "serviceStatus": service_status,
                    "validationReport": validation,
                    "error": error,
                }));
            }
        }
    }
    if !matches!(service_status.state, LocalServiceState::Running) {
        return Ok(json!({
            "kind": "workflowRunStart",
            "status": "service_not_running",
            "graphId": graph_id,
            "workflowName": workflow_name,
            "serviceStatus": service_status,
            "validationReport": validation,
            "message": "Choreograph is not running; call again with autoStart=true or start the service first.",
        }));
    }
    if let Err(error) = wait_for_choreograph_ready(&state).await {
        return Ok(json!({
            "kind": "workflowRunStart",
            "status": "service_unavailable",
            "graphId": graph_id,
            "workflowName": workflow_name,
            "serviceStatus": state.services.status(CHOREOGRAPH_SERVICE_ID, &state.app),
            "validationReport": validation,
            "error": String::from(error),
        }));
    }

    let body = build_run_body(&app, arguments, &graph_id, &workflow_name).await?;
    let upstream = choreograph_json_request(
        &state,
        Method::POST,
        CHOREOGRAPH_RUN_PATH,
        None,
        Some(body.clone()),
    )
    .await?;
    let run_id = upstream.body.get("runId").and_then(Value::as_str);
    let status = if upstream.status.is_success() {
        "started"
    } else {
        "upstream_error"
    };
    let run_record = if upstream.status.is_success() {
        run_id.map(|run_id| {
            let start_status = start_record_status(&upstream.body);
            record_workflow_run_start(
                app.clone(),
                &graph_id,
                &workflow_name,
                extract_workflow_uri(&validation, &workflow_name).as_deref(),
                run_id,
                &start_status,
            )
            .unwrap_or_else(|error| {
                json!({
                    "kind": "workflowRunRecord",
                    "status": "record_failed",
                    "runId": run_id,
                    "error": error.to_string(),
                })
            })
        })
    } else {
        None
    };

    Ok(json!({
        "kind": "workflowRunStart",
        "status": status,
        "httpStatus": upstream.status.as_u16(),
        "graphId": graph_id,
        "workflowName": workflow_name,
        "runId": run_id,
        "serviceStatus": service_status,
        "validationReport": validation,
        "request": {
            "facade": {"method": "POST", "path": "/workflows/runs"},
            "upstream": {"serviceId": "choreograph", "method": "POST", "path": CHOREOGRAPH_RUN_PATH},
            "body": body,
        },
        "response": upstream.body,
        "runRecord": run_record,
        "monitor": run_id.map(|run_id| monitor_descriptor_with_context(run_id, &graph_id, &workflow_name)),
    }))
}

pub(super) async fn mcp_local_workflow_run_monitor(
    app: AppHandle,
    arguments: &Value,
) -> AppResult<Value> {
    let graph_id = mcp_arg_string(arguments, &["graphId", "graph_id"]);
    let workflow_name = mcp_arg_string(arguments, &["workflowName", "workflow_name"]);
    let run_id = match mcp_arg_string(arguments, &["runId", "run_id"]) {
        Some(run_id) => run_id,
        None => {
            let graph_id = graph_id.as_deref().ok_or_else(|| {
                AppError::validation(
                    "runId is required unless graphId and workflowName identify a workflow with a recorded run",
                )
            })?;
            let workflow_name = workflow_name.as_deref().ok_or_else(|| {
                AppError::validation(
                    "runId is required unless graphId and workflowName identify a workflow with a recorded run",
                )
            })?;
            latest_recorded_run_id(&app, graph_id, workflow_name)?
        }
    };
    let since = mcp_arg_u64(arguments, &["since"]).unwrap_or(0);
    let include_events = mcp_arg_bool(arguments, &["includeEvents", "include_events"], true);
    let state = local_loopback_state(&app)?;
    let service_status = state.services.status(CHOREOGRAPH_SERVICE_ID, &state.app);
    if !matches!(service_status.state, LocalServiceState::Running) {
        return Ok(json!({
            "kind": "workflowRunMonitor",
            "status": "service_not_running",
            "runId": run_id,
            "serviceStatus": service_status,
        }));
    }

    let encoded_run_id = encode_path_segment(&run_id);
    let detail_path = format!("/api/workflows/runs/{encoded_run_id}");
    let detail = choreograph_json_request(&state, Method::GET, &detail_path, None, None).await?;
    let (events, next_since) = if include_events {
        let event_path = format!("/api/workflows/runs/{encoded_run_id}/events");
        let query = format!("since={since}");
        let events =
            choreograph_json_request(&state, Method::GET, &event_path, Some(&query), None).await?;
        let next_since = events.next_since.clone();
        (Some(events), next_since)
    } else {
        (None, None)
    };
    let status = if detail.status.is_success()
        && events
            .as_ref()
            .map(|events| events.status.is_success())
            .unwrap_or(true)
    {
        "ok"
    } else {
        "upstream_error"
    };
    let run_record = if status == "ok" {
        graph_id.as_deref().and_then(|graph_id| {
            workflow_name.as_deref().map(|workflow_name| {
                record_workflow_run_monitor(
                    app.clone(),
                    graph_id,
                    workflow_name,
                    &run_id,
                    &detail.body,
                )
                .unwrap_or_else(|error| {
                    json!({
                        "kind": "workflowRunRecord",
                        "status": "record_failed",
                        "runId": &run_id,
                        "error": error.to_string(),
                    })
                })
            })
        })
    } else {
        None
    };
    let monitor = match (graph_id.as_deref(), workflow_name.as_deref()) {
        (Some(graph_id), Some(workflow_name)) => {
            monitor_descriptor_with_context(&run_id, graph_id, workflow_name)
        }
        _ => monitor_descriptor(&run_id),
    };

    Ok(json!({
        "kind": "workflowRunMonitor",
        "status": status,
        "runId": run_id,
        "graphId": graph_id,
        "workflowName": workflow_name,
        "detail": {
            "httpStatus": detail.status.as_u16(),
            "body": detail.body,
        },
        "events": events.map(|events| json!({
            "httpStatus": events.status.as_u16(),
            "body": events.body,
        })),
        "nextSince": next_since,
        "runRecord": run_record,
        "monitor": monitor,
    }))
}

fn latest_recorded_run_id(
    app: &AppHandle,
    graph_id: &str,
    workflow_name: &str,
) -> AppResult<String> {
    let book = mcp_local_workflow_book_open(
        app.clone(),
        &json!({
            "graphId": graph_id,
            "workflowName": workflow_name,
            "pageId": "execute",
        }),
    )?;
    latest_run_id_from_book(&book).ok_or_else(|| {
        AppError::not_found(format!(
            "workflow '{workflow_name}' has no recorded run to monitor"
        ))
    })
}

fn latest_run_id_from_book(book: &Value) -> Option<String> {
    book.pointer("/page/objects")
        .and_then(Value::as_array)?
        .iter()
        .find(|object| object.get("kind").and_then(Value::as_str) == Some("workflowRun"))
        .and_then(|object| object.get("runId").and_then(Value::as_str))
        .map(str::to_string)
}

async fn wait_for_choreograph_ready(state: &Arc<LoopbackState>) -> AppResult<()> {
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|error| AppError::internal(format!("build Choreograph health client: {error}")))?;
    let deadline = Instant::now() + Duration::from_millis(CHOREOGRAPH_READY_TIMEOUT_MS);
    loop {
        let target = state
            .services
            .target(CHOREOGRAPH_SERVICE_ID)
            .map_err(AppError::validation)?;
        let url = build_upstream_url(&target.base_url, CHOREOGRAPH_HEALTH_PATH, None);
        let attempt_error = match client.get(&url).send().await {
            Ok(response) if response.status().is_success() => return Ok(()),
            Ok(response) => format!("health returned HTTP {}", response.status().as_u16()),
            Err(error) => error.to_string(),
        };
        if Instant::now() >= deadline {
            return Err(AppError::internal(format!(
                "Choreograph service did not become ready: {attempt_error}"
            )));
        }
        sleep(Duration::from_millis(CHOREOGRAPH_READY_POLL_MS)).await;
    }
}

async fn build_run_body(
    app: &AppHandle,
    arguments: &Value,
    graph_id: &str,
    workflow_name: &str,
) -> AppResult<Value> {
    let explicit_workflow_args = pick_value(arguments, &["workflowArgs", "workflow_args"]).cloned();
    let workflow_args_from_block = workflow_args_from_block(app.clone(), arguments).await?;
    let workflow_args = match (explicit_workflow_args, workflow_args_from_block) {
        (Some(explicit), Some(_)) if !is_empty_object(&explicit) => {
            return Err(AppError::validation(
                "provide either workflowArgs or workflowArgsBlock, not both",
            ));
        }
        (_, Some(from_block)) => from_block,
        (Some(explicit), None) => explicit,
        (None, None) => json!({}),
    };
    let mut body = json!({
        "graph_id": graph_id,
        "workflow_name": workflow_name,
        "workflow_args": workflow_args,
    });
    if let Some(source) = mcp_arg_string(arguments, &["workflowSource", "workflow_source"]) {
        body["workflow_source"] = json!(source);
    }
    if let Some(binding) = pick_value(arguments, &["workflowBinding", "workflow_binding"]).cloned()
    {
        body["workflow_binding"] = binding;
    }
    if body.get("workflow_source").is_none() && body.get("workflow_binding").is_none() {
        if let Some(source_block) = workflow_source_from_block(app.clone(), arguments).await? {
            let mut binding = json!({
                "name": workflow_name,
                "source": source_block.source,
            });
            if let Some(script_sha256) =
                mcp_arg_string(arguments, &["scriptSha256", "script_sha256"])
            {
                binding["scriptSha256"] = json!(script_sha256);
            }
            body["workflow_binding"] = binding;
        }
    }
    Ok(body)
}

async fn workflow_args_from_block(app: AppHandle, arguments: &Value) -> AppResult<Option<Value>> {
    let block_ref = pick_value(arguments, &["workflowArgsBlock", "workflow_args_block"]);
    let document_id = block_ref
        .and_then(|value| mcp_arg_string(value, &["documentId", "document_id"]))
        .or_else(|| mcp_arg_string(arguments, &["inputDocumentId", "input_document_id"]));
    let block_id = block_ref
        .and_then(|value| mcp_arg_string(value, &["blockId", "block_id"]))
        .or_else(|| mcp_arg_string(arguments, &["inputBlockId", "input_block_id"]));
    let (Some(document_id), Some(block_id)) = (document_id, block_id) else {
        return Ok(None);
    };
    let graph_id = mcp_required_graph_id(arguments).map_err(AppError::validation)?;
    let read_args = json!({
        "graphId": graph_id,
        "documentId": document_id,
        "blockId": block_id,
        "format": "text",
    });
    let block = mcp_local_get_block(app, &read_args)
        .await
        .map_err(AppError::internal)?;
    let text = block
        .pointer("/block/text")
        .and_then(Value::as_str)
        .ok_or_else(|| AppError::internal("workflow args block did not return text"))?;
    workflow_args_from_text(text)
        .map(Some)
        .map_err(AppError::validation)
}

fn workflow_args_from_text(text: &str) -> Result<Value, String> {
    let candidate = json_text_candidate(text);
    let value = serde_json::from_str::<Value>(&candidate)
        .map_err(|error| format!("workflow args block must contain JSON: {error}"))?;
    if !value.is_object() {
        return Err("workflow args block must contain a JSON object".to_string());
    }
    Ok(value)
}

fn json_text_candidate(text: &str) -> String {
    let trimmed = text.trim();
    if !trimmed.starts_with("```") {
        return trimmed.to_string();
    }
    let mut lines = trimmed.lines();
    let Some(first) = lines.next() else {
        return trimmed.to_string();
    };
    let fence_lang = first.trim_start_matches("```").trim();
    if !fence_lang.is_empty() && !fence_lang.eq_ignore_ascii_case("json") {
        return trimmed.to_string();
    }
    let mut body = Vec::new();
    for line in lines {
        if line.trim_start().starts_with("```") {
            return body.join("\n").trim().to_string();
        }
        body.push(line);
    }
    trimmed.to_string()
}

fn is_empty_object(value: &Value) -> bool {
    value
        .as_object()
        .map(|object| object.is_empty())
        .unwrap_or(false)
}

struct WorkflowSourceBlock {
    source: String,
}

async fn workflow_source_from_block(
    app: AppHandle,
    arguments: &Value,
) -> AppResult<Option<WorkflowSourceBlock>> {
    let block_ref = pick_value(arguments, &["workflowSourceBlock", "workflow_source_block"]);
    let document_id = block_ref
        .and_then(|value| mcp_arg_string(value, &["documentId", "document_id"]))
        .or_else(|| mcp_arg_string(arguments, &["sourceDocumentId", "source_document_id"]));
    let block_id = block_ref
        .and_then(|value| mcp_arg_string(value, &["blockId", "block_id"]))
        .or_else(|| mcp_arg_string(arguments, &["sourceBlockId", "source_block_id"]));
    let (Some(document_id), Some(block_id)) = (document_id, block_id) else {
        return Ok(None);
    };
    let graph_id = mcp_required_graph_id(arguments).map_err(AppError::validation)?;
    let read_args = json!({
        "graphId": graph_id,
        "documentId": document_id,
        "blockId": block_id,
        "format": "text",
    });
    let block = mcp_local_get_block(app, &read_args)
        .await
        .map_err(AppError::internal)?;
    let source = block
        .pointer("/block/text")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| AppError::internal("workflow source block did not return text"))?;
    Ok(Some(WorkflowSourceBlock { source }))
}

fn local_loopback_state(app: &AppHandle) -> AppResult<Arc<LoopbackState>> {
    app.try_state::<Arc<LoopbackState>>()
        .map(|state| state.inner().clone())
        .ok_or_else(|| {
            AppError::validation(
                "local loopback state is unavailable; workflow execution requires local mode",
            )
        })
}

struct UpstreamJson {
    status: reqwest::StatusCode,
    body: Value,
    next_since: Option<String>,
}

async fn choreograph_json_request(
    state: &Arc<LoopbackState>,
    method: Method,
    path: &str,
    query: Option<&str>,
    body: Option<Value>,
) -> AppResult<UpstreamJson> {
    let target = state
        .services
        .target(CHOREOGRAPH_SERVICE_ID)
        .map_err(AppError::validation)?;
    let url = build_upstream_url(&target.base_url, path, query);
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|error| AppError::internal(format!("build Choreograph client: {error}")))?;
    let mut request = client
        .request(method, &url)
        .header("Accept", "application/json")
        .header("X-Internal-Service", target.internal_secret)
        .header("X-User-ID", PROFILE_ID);
    if let Some(body) = body {
        request = request.json(&body);
    }
    let response = request
        .send()
        .await
        .map_err(|error| AppError::internal(format!("call Choreograph service: {error}")))?;
    let status = response.status();
    let next_since = response
        .headers()
        .get("x-next-since")
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let bytes = response
        .bytes()
        .await
        .map_err(|error| AppError::internal(format!("read Choreograph response: {error}")))?;
    let body = serde_json::from_slice(&bytes).unwrap_or_else(|_| {
        json!({
            "text": String::from_utf8_lossy(&bytes).to_string(),
        })
    });
    Ok(UpstreamJson {
        status,
        body,
        next_since,
    })
}

fn record_workflow_run_start(
    app: AppHandle,
    graph_id: &str,
    workflow_name: &str,
    workflow_uri: Option<&str>,
    run_id: &str,
    status: &str,
) -> AppResult<Value> {
    let run_uri = workflow_run_subject(graph_id, run_id);
    let started_at = now_iso();
    let update = workflow_run_start_update(
        graph_id,
        workflow_name,
        workflow_uri,
        run_id,
        &run_uri,
        status,
        &started_at,
    );
    let mutation = run_sparql_update_service(
        app,
        SparqlUpdateInput {
            graph_id: graph_id.to_string(),
            update,
        },
    )?;
    Ok(json!({
        "kind": "workflowRunRecord",
        "status": "recorded",
        "recordKind": "start",
        "graphId": graph_id,
        "workflowName": workflow_name,
        "runId": run_id,
        "runUri": run_uri,
        "startedAt": started_at,
        "runStatus": status,
        "mutation": mutation,
    }))
}

fn record_workflow_run_monitor(
    app: AppHandle,
    graph_id: &str,
    workflow_name: &str,
    run_id: &str,
    detail_body: &Value,
) -> AppResult<Value> {
    let Some(status) = run_status_from_detail(detail_body) else {
        return Ok(json!({
            "kind": "workflowRunRecord",
            "status": "not_recorded",
            "recordKind": "monitor",
            "graphId": graph_id,
            "workflowName": workflow_name,
            "runId": run_id,
            "message": "Choreograph run detail did not include a status.",
        }));
    };
    let run_uri = workflow_run_subject(graph_id, run_id);
    let ended_at = is_terminal_run_status(&status).then(now_iso);
    let update = workflow_run_status_update(
        graph_id,
        workflow_name,
        run_id,
        &run_uri,
        &status,
        ended_at.as_deref(),
    );
    let mutation = run_sparql_update_service(
        app,
        SparqlUpdateInput {
            graph_id: graph_id.to_string(),
            update,
        },
    )?;
    Ok(json!({
        "kind": "workflowRunRecord",
        "status": "recorded",
        "recordKind": "monitor",
        "graphId": graph_id,
        "workflowName": workflow_name,
        "runId": run_id,
        "runUri": run_uri,
        "runStatus": status,
        "endedAt": ended_at,
        "mutation": mutation,
    }))
}

fn workflow_run_start_update(
    graph_id: &str,
    workflow_name: &str,
    workflow_uri: Option<&str>,
    run_id: &str,
    run_uri: &str,
    status: &str,
    started_at: &str,
) -> String {
    let graph = user_rdf_graph_iri(graph_id);
    let used_insert = workflow_uri
        .map(|workflow_uri| format!("    <{run_uri}> <{PROV_NS}used> <{workflow_uri}> .\n"))
        .unwrap_or_default();
    format!(
        "DELETE {{\n  GRAPH <{graph}> {{\n    <{run_uri}> <{WF_NS}status> ?oldStatus .\n    <{run_uri}> <{PROV_NS}startedAtTime> ?oldStarted .\n    <{run_uri}> <{PROV_NS}endedAtTime> ?oldEnded .\n    <{run_uri}> <{WF_NS}durationMs> ?oldDurationMs .\n    <{run_uri}> <{WF_NS}totalTokens> ?oldTotalTokens .\n    <{run_uri}> <{WF_NS}agentCount> ?oldAgentCount .\n  }}\n}}\nINSERT {{\n  GRAPH <{graph}> {{\n    <{run_uri}> a <{WF_NS}Run> ;\n      <{WF_NS}workflowName> {workflow_name_lit} ;\n      <{WF_NS}runId> {run_id_lit} ;\n      <{WF_NS}status> {status_lit} ;\n      <{PROV_NS}startedAtTime> {started_lit} .\n{used_insert}  }}\n}}\nWHERE {{\n  GRAPH <{graph}> {{\n    OPTIONAL {{ <{run_uri}> <{WF_NS}status> ?oldStatus }}\n    OPTIONAL {{ <{run_uri}> <{PROV_NS}startedAtTime> ?oldStarted }}\n    OPTIONAL {{ <{run_uri}> <{PROV_NS}endedAtTime> ?oldEnded }}\n    OPTIONAL {{ <{run_uri}> <{WF_NS}durationMs> ?oldDurationMs }}\n    OPTIONAL {{ <{run_uri}> <{WF_NS}totalTokens> ?oldTotalTokens }}\n    OPTIONAL {{ <{run_uri}> <{WF_NS}agentCount> ?oldAgentCount }}\n  }}\n}}",
        workflow_name_lit = sparql_string_literal(workflow_name),
        run_id_lit = sparql_string_literal(run_id),
        status_lit = sparql_string_literal(status),
        started_lit = sparql_string_literal(started_at),
    )
}

fn workflow_run_status_update(
    graph_id: &str,
    workflow_name: &str,
    run_id: &str,
    run_uri: &str,
    status: &str,
    ended_at: Option<&str>,
) -> String {
    let graph = user_rdf_graph_iri(graph_id);
    let ended_insert = ended_at
        .map(|ended_at| {
            format!(
                "    <{run_uri}> <{PROV_NS}endedAtTime> {} .\n",
                sparql_string_literal(ended_at)
            )
        })
        .unwrap_or_default();
    format!(
        "DELETE {{\n  GRAPH <{graph}> {{\n    <{run_uri}> <{WF_NS}status> ?oldStatus .\n    <{run_uri}> <{PROV_NS}endedAtTime> ?oldEnded .\n  }}\n}}\nINSERT {{\n  GRAPH <{graph}> {{\n    <{run_uri}> a <{WF_NS}Run> ;\n      <{WF_NS}workflowName> {workflow_name_lit} ;\n      <{WF_NS}runId> {run_id_lit} ;\n      <{WF_NS}status> {status_lit} .\n{ended_insert}  }}\n}}\nWHERE {{\n  GRAPH <{graph}> {{\n    OPTIONAL {{ <{run_uri}> <{WF_NS}status> ?oldStatus }}\n    OPTIONAL {{ <{run_uri}> <{PROV_NS}endedAtTime> ?oldEnded }}\n  }}\n}}",
        workflow_name_lit = sparql_string_literal(workflow_name),
        run_id_lit = sparql_string_literal(run_id),
        status_lit = sparql_string_literal(status),
    )
}

fn workflow_run_subject(graph_id: &str, run_id: &str) -> String {
    format!(
        "urn:mnemosyne:local:workflow-run:{}:{}",
        iri_component(graph_id),
        iri_component(run_id)
    )
}

fn iri_component(value: &str) -> String {
    let mut out = String::new();
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("_{byte:02X}")),
        }
    }
    if out.is_empty() {
        "item".to_string()
    } else {
        out
    }
}

fn extract_workflow_uri(validation: &Value, workflow_name: &str) -> Option<String> {
    validation
        .get("reports")
        .and_then(Value::as_array)?
        .iter()
        .find(|report| report.get("workflowName").and_then(Value::as_str) == Some(workflow_name))
        .or_else(|| {
            validation
                .get("reports")
                .and_then(Value::as_array)
                .and_then(|reports| reports.first())
        })
        .and_then(|report| report.get("workflowUri").and_then(Value::as_str))
        .map(str::to_string)
}

fn start_record_status(body: &Value) -> String {
    run_status_from_detail(body)
        .filter(|status| !status.trim().is_empty())
        .unwrap_or_else(|| "running".to_string())
}

fn run_status_from_detail(body: &Value) -> Option<String> {
    body.pointer("/header/status")
        .and_then(Value::as_str)
        .or_else(|| body.get("status").and_then(Value::as_str))
        .or_else(|| body.pointer("/run/status").and_then(Value::as_str))
        .map(str::to_string)
}

fn is_terminal_run_status(status: &str) -> bool {
    !matches!(
        status.trim().to_ascii_lowercase().as_str(),
        "" | "queued" | "pending" | "running" | "started" | "starting"
    )
}

fn now_iso() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn monitor_descriptor(run_id: &str) -> Value {
    json!({
        "detail": {
            "tool": "workflow_run_monitor",
            "arguments": {
                "runId": run_id,
                "includeEvents": true,
                "since": 0,
            },
        },
        "facade": {
            "detailPath": format!("/workflows/runs/{run_id}"),
            "eventsPath": format!("/workflows/runs/{run_id}/events"),
        },
        "upstream": {
            "detailPath": format!("/api/workflows/runs/{run_id}"),
            "eventsPath": format!("/api/workflows/runs/{run_id}/events"),
        },
    })
}

fn monitor_descriptor_with_context(run_id: &str, graph_id: &str, workflow_name: &str) -> Value {
    let mut descriptor = monitor_descriptor(run_id);
    if let Some(arguments) = descriptor.pointer_mut("/detail/arguments") {
        arguments["graphId"] = json!(graph_id);
        arguments["workflowName"] = json!(workflow_name);
    }
    descriptor
}

fn pick_value<'a>(arguments: &'a Value, keys: &[&str]) -> Option<&'a Value> {
    keys.iter().find_map(|key| arguments.get(*key))
}

fn build_upstream_url(base_url: &str, path: &str, query: Option<&str>) -> String {
    let base = base_url.trim_end_matches('/');
    let path = if path.starts_with('/') {
        path.to_string()
    } else {
        format!("/{path}")
    };
    match query {
        Some(query) if !query.is_empty() => format!("{base}{path}?{query}"),
        _ => format!("{base}{path}"),
    }
}

fn encode_path_segment(value: &str) -> String {
    let mut out = String::new();
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upstream_urls_and_path_segments_are_stable() {
        assert_eq!(
            build_upstream_url(
                "http://127.0.0.1:3456/",
                "/api/workflows/runs/run-1/events",
                Some("since=7")
            ),
            "http://127.0.0.1:3456/api/workflows/runs/run-1/events?since=7"
        );
        assert_eq!(encode_path_segment("run 1/2"), "run%201%2F2");
    }

    #[test]
    fn monitor_descriptor_points_back_to_mcp_and_facade() {
        let descriptor = monitor_descriptor("wfr-test");
        assert_eq!(
            descriptor.pointer("/detail/tool").and_then(Value::as_str),
            Some("workflow_run_monitor")
        );
        assert_eq!(
            descriptor
                .pointer("/facade/detailPath")
                .and_then(Value::as_str),
            Some("/workflows/runs/wfr-test")
        );

        let contextual = monitor_descriptor_with_context("wfr-test", "lab", "demo");
        assert_eq!(
            contextual
                .pointer("/detail/arguments/graphId")
                .and_then(Value::as_str),
            Some("lab")
        );
        assert_eq!(
            contextual
                .pointer("/detail/arguments/workflowName")
                .and_then(Value::as_str),
            Some("demo")
        );
    }

    #[test]
    fn latest_run_id_is_read_from_execute_page_objects() {
        let book = json!({
            "page": {
                "objects": [
                    {"kind": "workflow", "name": "demo"},
                    {"kind": "workflowRun", "runId": "wfr-new", "status": "running"}
                ]
            }
        });
        assert_eq!(latest_run_id_from_book(&book).as_deref(), Some("wfr-new"));
        assert!(latest_run_id_from_book(&json!({"page": {"objects": []}})).is_none());
    }

    #[test]
    fn run_record_updates_target_user_rdf_graph() {
        let run_uri = workflow_run_subject("graph/a", "run 1");
        assert_eq!(
            run_uri,
            "urn:mnemosyne:local:workflow-run:graph_2Fa:run_201"
        );
        let update = workflow_run_start_update(
            "graph-a",
            "demo",
            Some("urn:workflow:demo"),
            "wfr-test",
            "urn:mnemosyne:local:workflow-run:graph-a:wfr-test",
            "running",
            "2026-06-24T00:00:00.000Z",
        );
        assert!(update.contains("GRAPH <urn:mnemosyne:local:graph:graph-a:user:rdf>"));
        assert!(update.contains("<http://www.w3.org/ns/prov#used> <urn:workflow:demo>"));
        assert!(update.contains("<http://mnemosyne.dev/workflow#runId> \"wfr-test\""));
    }

    #[test]
    fn workflow_args_text_accepts_json_objects_and_code_fences() {
        assert_eq!(
            workflow_args_from_text(r#"{ "marker": "plain" }"#).unwrap(),
            json!({"marker": "plain"})
        );
        assert_eq!(
            workflow_args_from_text("```json\n{\"marker\":\"fenced\"}\n```").unwrap(),
            json!({"marker": "fenced"})
        );
        assert_eq!(
            workflow_args_from_text("```JSON\n{\"marker\":\"upper\"}\n```").unwrap(),
            json!({"marker": "upper"})
        );
        assert!(workflow_args_from_text("[1, 2]").is_err());
        assert!(workflow_args_from_text("{bad").is_err());
    }

    #[test]
    fn executable_source_refs_are_same_graph_and_exact() {
        assert_eq!(
            local_doc_block_ref(
                "phanes",
                "urn:mnemosyne:local:graph:phanes:doc:activity-loop#block-source"
            ),
            Some(("activity-loop".to_string(), "block-source".to_string()))
        );
        assert!(local_doc_block_ref(
            "phanes",
            "urn:mnemosyne:local:graph:other:doc:activity-loop#block-source"
        )
        .is_none());
        assert!(local_doc_block_ref(
            "phanes",
            "urn:mnemosyne:local:graph:phanes:doc:activity-loop#block#extra"
        )
        .is_none());
    }

    #[test]
    fn executable_iri_validation_rejects_sparql_breakout() {
        assert!(validate_embedded_iri("urn:sophia:wf:phanes-response").is_ok());
        assert!(validate_embedded_iri("urn:sophia:wf:bad> } UNION { ?s ?p ?o").is_err());
    }

    #[test]
    fn rdf_term_projection_handles_iris_and_escaped_literals() {
        assert_eq!(rdf_term_value("<urn:sophia:wf:one>"), "urn:sophia:wf:one");
        let escaped = oxigraph::model::Literal::new_simple_literal("line\nquoted\"").to_string();
        assert_eq!(rdf_term_value(&escaped), "line\nquoted\"");
        assert_eq!(
            rdf_term_value(r#""abc"^^<http://www.w3.org/2001/XMLSchema#string>"#),
            "abc"
        );
    }
}
