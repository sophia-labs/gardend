use crate::{
    app_error::{AppError, AppResult},
    app_runtime::AppHandle,
    clock::timestamp,
    local_jobs::{local_job_submit_response, LocalJobProgress, LocalJobRegistry, LocalJobStatus},
    mcp_arg_utils::{mcp_arg_bool, mcp_arg_string, mcp_arg_u64, mcp_required_graph_id},
    workflow_book_mcp::{
        mcp_local_workflow_book_choose, mcp_local_workflow_book_compose,
        mcp_local_workflow_book_open, mcp_local_workflow_book_validate,
    },
    workflow_run_mcp::{mcp_local_workflow_run_monitor, mcp_local_workflow_run_start},
};
use serde_json::{json, Value};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::time::sleep;

const WORKFLOW_AUTHORING_AWAIT_JOB_TYPE: &str = "workflow_authoring_await";
const WORKFLOW_AUTHORING_VALIDATE_JOB_TYPE: &str = "workflow_authoring_validate";

pub(super) async fn mcp_local_workflow_authoring_session(
    app: AppHandle,
    jobs: Arc<LocalJobRegistry>,
    arguments: &Value,
) -> AppResult<Value> {
    let graph_id = mcp_required_graph_id(arguments).map_err(AppError::validation)?;
    let operation = normalize_session_operation(
        mcp_arg_string(arguments, &["operation", "op"])
            .as_deref()
            .unwrap_or("status"),
    );

    match operation.as_str() {
        "status" => {
            let book = mcp_local_workflow_book_open(app, arguments)?;
            Ok(session_response(&operation, &graph_id, json!({ "book": book })))
        }
        "choose" => {
            let book = mcp_local_workflow_book_choose(app, arguments)?;
            Ok(session_response(&operation, &graph_id, json!({ "book": book })))
        }
        "compose" => {
            let compose_args = compose_arguments(arguments)?;
            let result = mcp_local_workflow_book_compose(app, &compose_args).await?;
            Ok(session_response(&operation, &graph_id, json!({ "compose": result })))
        }
        "validate" => {
            let validation = mcp_local_workflow_book_validate(app.clone(), arguments)?;
            let book = mcp_local_workflow_book_open(app, arguments)?;
            Ok(session_response(
                &operation,
                &graph_id,
                json!({
                    "validation": validation,
                    "book": book,
                }),
            ))
        }
        "watch" => {
            let payload = authoring_watch(app, arguments, &graph_id).await?;
            Ok(session_response(&operation, &graph_id, payload))
        }
        "await" => {
            let payload = authoring_await(app, arguments, &graph_id).await?;
            Ok(session_response(&operation, &graph_id, payload))
        }
        "background" => {
            let payload = authoring_background(app, jobs, arguments, &graph_id)?;
            Ok(session_response(&operation, &graph_id, payload))
        }
        "draft" => {
            let payload = authoring_draft(app, arguments, &graph_id).await?;
            Ok(session_response(&operation, &graph_id, payload))
        }
        "prepare_execute" => {
            let book = execution_book(app, arguments)?;
            let (choice, action_args) = execution_choice(&book, arguments)?;
            Ok(session_response(
                &operation,
                &graph_id,
                json!({
                    "book": book,
                    "selectedExecutionChoice": choice,
                    "executionAction": {
                        "tool": "workflow_run_start",
                        "arguments": action_args,
                    },
                }),
            ))
        }
        "execute" => {
            let book = execution_book(app.clone(), arguments)?;
            let (choice, action_args) = execution_choice(&book, arguments)?;
            let run = mcp_local_workflow_run_start(app, &action_args).await?;
            Ok(session_response(
                &operation,
                &graph_id,
                json!({
                    "book": book,
                    "selectedExecutionChoice": choice,
                    "run": run,
                }),
            ))
        }
        "monitor" => {
            let monitor = mcp_local_workflow_run_monitor(app, arguments).await?;
            Ok(session_response(
                &operation,
                &graph_id,
                json!({ "monitor": monitor }),
            ))
        }
        other => Err(AppError::validation(format!(
            "unsupported workflow_authoring_session operation '{other}'; use status, choose, compose, validate, watch, await, background, draft, prepare_execute, execute, or monitor"
        ))),
    }
}

fn normalize_session_operation(operation: &str) -> String {
    let normalized = operation.trim().replace('-', "_");
    match normalized.as_str() {
        "" | "open" | "start" | "resume" | "inspect" => "status".to_string(),
        "choice" | "select" => "choose".to_string(),
        "refresh" | "observe" | "poll" | "tick" => "watch".to_string(),
        "wait" | "await_run" | "watch_until" | "stabilize" | "follow" => "await".to_string(),
        "draft_status" | "open_draft" | "resume_draft" | "draft_move" | "move_draft"
        | "compose_draft" => "draft".to_string(),
        "validate_background"
        | "background_validate"
        | "validation_background"
        | "background_validation"
        | "watch_validation"
        | "validation_watch"
        | "start_validation" => "background".to_string(),
        "await_background" | "background_await" | "watch_background" | "background_watch"
        | "start_background" | "start_watch" | "async_await" | "await_async" => {
            "background".to_string()
        }
        "run" | "start_run" => "execute".to_string(),
        "prepare" | "prepare_execution" | "execute_prepare" => "prepare_execute".to_string(),
        "create_workflow" | "add_phase" | "attach_agent_node" | "bind_source_block"
        | "bind_input_block" => "compose".to_string(),
        _ => normalized,
    }
}

async fn authoring_watch(app: AppHandle, arguments: &Value, graph_id: &str) -> AppResult<Value> {
    let workflow_name = mcp_arg_string(arguments, &["workflowName", "workflow_name"]);
    let validation = mcp_local_workflow_book_validate(app.clone(), arguments)?;
    let mut book = mcp_local_workflow_book_open(app.clone(), arguments)?;
    let mut latest_run = latest_workflow_run_from_book(&book).unwrap_or(Value::Null);
    let mut monitor = Value::Null;
    let mut monitor_attempted = false;
    let mut next_since = mcp_arg_u64(arguments, &["since"]).unwrap_or(0);

    if should_monitor_latest_run(arguments, workflow_name.as_deref(), &latest_run) {
        let workflow_name = workflow_name.as_deref().unwrap_or_default();
        let monitor_args = watch_monitor_arguments(arguments, graph_id, workflow_name);
        monitor_attempted = true;
        monitor = mcp_local_workflow_run_monitor(app.clone(), &monitor_args)
            .await
            .unwrap_or_else(|error| {
                json!({
                    "kind": "workflowRunMonitor",
                    "status": "monitor_failed",
                    "graphId": graph_id,
                    "workflowName": workflow_name,
                    "error": error.to_string(),
                })
            });
        if monitor.get("runRecord").is_some()
            || monitor.get("status").and_then(Value::as_str) == Some("ok")
        {
            next_since = monitor_next_since(&monitor).unwrap_or(next_since);
            book = mcp_local_workflow_book_open(app, arguments)?;
            latest_run = latest_workflow_run_from_book(&book).unwrap_or(Value::Null);
        }
    }

    Ok(json!({
        "validation": validation,
        "book": book,
        "latestRun": latest_run,
        "monitor": monitor,
        "watch": {
            "kind": "workflowAuthoringWatch",
            "monitorAttempted": monitor_attempted,
            "pollAfterMs": watch_poll_ms(arguments),
            "nextAction": watch_next_action(arguments, graph_id, workflow_name.as_deref(), next_since),
        },
    }))
}

async fn authoring_await(app: AppHandle, arguments: &Value, graph_id: &str) -> AppResult<Value> {
    authoring_await_loop(app, arguments, graph_id, None).await
}

async fn authoring_draft(app: AppHandle, arguments: &Value, graph_id: &str) -> AppResult<Value> {
    let move_templates = draft_move_templates(arguments)?;
    let mut workflow_name = mcp_arg_string(arguments, &["workflowName", "workflow_name"]);
    let mut current_page_id = mcp_arg_string(arguments, &["pageId", "page_id"]);
    let mut move_results = Vec::new();

    for (index, template) in move_templates.iter().enumerate() {
        let move_args =
            draft_move_arguments(arguments, template, graph_id, workflow_name.as_deref())?;
        let result = mcp_local_workflow_book_compose(app.clone(), &move_args).await?;
        workflow_name = extract_workflow_name_from_compose_result(&result).or(workflow_name);
        current_page_id = result
            .pointer("/refreshed/currentPageId")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or(current_page_id);
        move_results.push(draft_move_result(index + 1, &move_args, &result));
    }

    let open_args = draft_open_arguments(
        arguments,
        graph_id,
        workflow_name.as_deref(),
        current_page_id.as_deref(),
    )?;
    let validation = mcp_local_workflow_book_validate(app.clone(), &open_args)?;
    let book = mcp_local_workflow_book_open(app, &open_args)?;
    let authoring = book.get("authoring").cloned().unwrap_or(Value::Null);
    let draft = workflow_authoring_draft_envelope(
        graph_id,
        workflow_name.as_deref(),
        &book,
        &validation,
        &authoring,
        move_results.len(),
    );

    Ok(json!({
        "draft": draft,
        "moves": move_results,
        "validation": validation,
        "book": book,
    }))
}

fn authoring_background(
    app: AppHandle,
    jobs: Arc<LocalJobRegistry>,
    arguments: &Value,
    graph_id: &str,
) -> AppResult<Value> {
    match normalize_background_mode(arguments).as_str() {
        "await" => authoring_background_await(app, jobs, arguments, graph_id),
        "validate" => authoring_background_validate(app, jobs, arguments, graph_id),
        other => Err(AppError::validation(format!(
            "unsupported workflow_authoring_session background mode '{other}'; use await or validate"
        ))),
    }
}

fn authoring_background_await(
    app: AppHandle,
    jobs: Arc<LocalJobRegistry>,
    arguments: &Value,
    graph_id: &str,
) -> AppResult<Value> {
    let mut await_args = arguments.clone();
    let Some(map) = await_args.as_object_mut() else {
        return Err(AppError::validation(
            "workflow_authoring_session arguments must be an object",
        ));
    };
    map.insert("operation".to_string(), json!("await"));

    let workflow_name = mcp_arg_string(&await_args, &["workflowName", "workflow_name"]);
    let page_id = mcp_arg_string(&await_args, &["pageId", "page_id"]);
    let timeout_ms = await_timeout_ms(&await_args);
    let max_ticks = await_max_ticks(&await_args);
    let poll_ms = watch_poll_ms(&await_args);
    let record = jobs.insert_queued(
        WORKFLOW_AUTHORING_AWAIT_JOB_TYPE,
        Some(graph_id.to_string()),
        json!({
            "kind": "workflowAuthoringBackground",
            "mode": "await",
            "graphId": graph_id,
            "workflowName": workflow_name.clone(),
            "pageId": page_id,
            "pollMs": poll_ms,
            "timeoutMs": timeout_ms,
            "maxTicks": max_ticks,
            "arguments": await_args.clone(),
        }),
    )?;
    let progress = AuthoringJobProgress::new(
        jobs.clone(),
        record.job_id.clone(),
        graph_id.to_string(),
        max_ticks as usize,
    );
    progress.report(
        "queued",
        "Workflow authoring await job queued",
        0,
        json!({
            "graphId": graph_id,
            "workflowName": workflow_name.clone(),
            "pollMs": poll_ms,
            "timeoutMs": timeout_ms,
            "maxTicks": max_ticks,
        }),
    );
    let record = jobs.get(&record.job_id)?.unwrap_or(record);
    spawn_authoring_await_job(
        app,
        jobs.clone(),
        record.job_id.clone(),
        graph_id.to_string(),
        await_args,
    );
    let job_id = record.job_id.clone();
    let job = serde_json::to_value(local_job_submit_response(&record))
        .map_err(|error| AppError::serialization(format!("serialize local job: {error}")))?;

    Ok(json!({
        "background": {
            "kind": "workflowAuthoringBackground",
            "mode": "await",
            "job": job,
            "actions": workflow_authoring_job_actions(&job_id),
            "nextAction": {
                "tool": "get_job_status",
                "arguments": {"jobId": job_id},
            },
        }
    }))
}

fn authoring_background_validate(
    app: AppHandle,
    jobs: Arc<LocalJobRegistry>,
    arguments: &Value,
    graph_id: &str,
) -> AppResult<Value> {
    let validate_args = validation_watch_arguments(arguments, graph_id)?;
    let workflow_name = mcp_arg_string(&validate_args, &["workflowName", "workflow_name"]);
    let page_id = mcp_arg_string(&validate_args, &["pageId", "page_id"]);
    let timeout_ms = await_timeout_ms(&validate_args);
    let max_ticks = await_max_ticks(&validate_args);
    let poll_ms = watch_poll_ms(&validate_args);
    let resolve_on_change = validation_resolve_on_change(&validate_args);
    let record = jobs.insert_queued(
        WORKFLOW_AUTHORING_VALIDATE_JOB_TYPE,
        Some(graph_id.to_string()),
        json!({
            "kind": "workflowAuthoringBackground",
            "mode": "validate",
            "graphId": graph_id,
            "workflowName": workflow_name.clone(),
            "pageId": page_id,
            "pollMs": poll_ms,
            "timeoutMs": timeout_ms,
            "maxTicks": max_ticks,
            "resolveOnChange": resolve_on_change,
            "arguments": validate_args.clone(),
        }),
    )?;
    let progress = AuthoringJobProgress::new(
        jobs.clone(),
        record.job_id.clone(),
        graph_id.to_string(),
        max_ticks as usize,
    );
    progress.report(
        "queued",
        "Workflow authoring validation watcher queued",
        0,
        json!({
            "graphId": graph_id,
            "workflowName": workflow_name.clone(),
            "pollMs": poll_ms,
            "timeoutMs": timeout_ms,
            "maxTicks": max_ticks,
            "resolveOnChange": resolve_on_change,
        }),
    );
    let record = jobs.get(&record.job_id)?.unwrap_or(record);
    spawn_authoring_validate_job(
        app,
        jobs.clone(),
        record.job_id.clone(),
        graph_id.to_string(),
        validate_args,
    );
    let job_id = record.job_id.clone();
    let job = serde_json::to_value(local_job_submit_response(&record))
        .map_err(|error| AppError::serialization(format!("serialize local job: {error}")))?;

    Ok(json!({
        "background": {
            "kind": "workflowAuthoringBackground",
            "mode": "validate",
            "job": job,
            "actions": workflow_authoring_job_actions(&job_id),
            "nextAction": {
                "tool": "get_job_status",
                "arguments": {"jobId": job_id},
            },
        }
    }))
}

fn normalize_background_mode(arguments: &Value) -> String {
    let raw = mcp_arg_string(
        arguments,
        &[
            "mode",
            "backgroundMode",
            "background_mode",
            "watchMode",
            "watch_mode",
        ],
    )
    .or_else(|| mcp_arg_string(arguments, &["operation", "op"]))
    .unwrap_or_else(|| "await".to_string());
    let normalized = raw.trim().replace('-', "_");
    match normalized.as_str() {
        "background" | "await" | "await_background" | "background_await" | "watch_background"
        | "background_watch" | "start_background" | "start_watch" | "async_await"
        | "await_async" => "await".to_string(),
        "validate"
        | "validation"
        | "validate_background"
        | "background_validate"
        | "validation_background"
        | "background_validation"
        | "watch_validation"
        | "validation_watch"
        | "start_validation" => "validate".to_string(),
        _ => normalized,
    }
}

fn validation_watch_arguments(arguments: &Value, graph_id: &str) -> AppResult<Value> {
    let mut watch_args = arguments.clone();
    let Some(map) = watch_args.as_object_mut() else {
        return Err(AppError::validation(
            "workflow_authoring_session arguments must be an object",
        ));
    };
    map.insert("graphId".to_string(), json!(graph_id));
    map.insert("operation".to_string(), json!("validate"));
    Ok(watch_args)
}

fn draft_move_templates(arguments: &Value) -> AppResult<Vec<Value>> {
    if let Some(moves) = arguments.get("moves") {
        return match moves {
            Value::Array(values) => {
                let mut templates = Vec::new();
                for (index, value) in values.iter().enumerate() {
                    if !value.is_object() {
                        return Err(AppError::validation(format!(
                            "workflow_authoring_session draft moves[{index}] must be an object"
                        )));
                    }
                    templates.push(value.clone());
                }
                Ok(templates)
            }
            Value::Object(_) => Ok(vec![moves.clone()]),
            _ => Err(AppError::validation(
                "workflow_authoring_session draft moves must be an object or array of objects",
            )),
        };
    }

    if let Some(operation) = draft_single_move_operation(arguments) {
        let mut template = arguments.clone();
        let Some(map) = template.as_object_mut() else {
            return Err(AppError::validation(
                "workflow_authoring_session arguments must be an object",
            ));
        };
        map.insert("operation".to_string(), json!(operation));
        return Ok(vec![template]);
    }

    Ok(Vec::new())
}

fn draft_single_move_operation(arguments: &Value) -> Option<String> {
    mcp_arg_string(
        arguments,
        &[
            "composeOperation",
            "compose_operation",
            "workflowOperation",
            "workflow_operation",
            "draftMove",
            "draft_move",
            "move",
        ],
    )
}

fn draft_move_arguments(
    base: &Value,
    template: &Value,
    graph_id: &str,
    workflow_name: Option<&str>,
) -> AppResult<Value> {
    let Some(template_map) = template.as_object() else {
        return Err(AppError::validation(
            "workflow_authoring_session draft move must be an object",
        ));
    };
    let mut merged = serde_json::Map::new();

    if let Some(base_map) = base.as_object() {
        for (key, value) in base_map {
            if copy_base_key_into_draft_move(key) {
                merged.insert(key.clone(), value.clone());
            }
        }
    }
    if let Some(workflow_name) = workflow_name {
        merged
            .entry("workflowName".to_string())
            .or_insert_with(|| json!(workflow_name));
    }
    for (key, value) in template_map {
        if key != "moves" {
            merged.insert(key.clone(), value.clone());
        }
    }

    let mut move_args = Value::Object(merged);
    let operation = mcp_arg_string(
        &move_args,
        &[
            "operation",
            "op",
            "composeOperation",
            "compose_operation",
            "workflowOperation",
            "workflow_operation",
            "draftMove",
            "draft_move",
            "move",
        ],
    )
    .ok_or_else(|| AppError::validation("draft move operation is required"))?;
    let Some(map) = move_args.as_object_mut() else {
        return Err(AppError::validation(
            "workflow_authoring_session draft move must be an object",
        ));
    };
    map.insert("graphId".to_string(), json!(graph_id));
    map.insert("operation".to_string(), json!(operation));
    Ok(move_args)
}

fn copy_base_key_into_draft_move(key: &str) -> bool {
    !matches!(
        key,
        "operation"
            | "op"
            | "moves"
            | "composeOperation"
            | "compose_operation"
            | "workflowOperation"
            | "workflow_operation"
            | "draftMove"
            | "draft_move"
            | "move"
    )
}

fn draft_open_arguments(
    arguments: &Value,
    graph_id: &str,
    workflow_name: Option<&str>,
    current_page_id: Option<&str>,
) -> AppResult<Value> {
    let mut open_args = arguments.clone();
    let Some(map) = open_args.as_object_mut() else {
        return Err(AppError::validation(
            "workflow_authoring_session arguments must be an object",
        ));
    };
    map.remove("moves");
    map.remove("move");
    map.remove("draftMove");
    map.remove("draft_move");
    map.remove("composeOperation");
    map.remove("compose_operation");
    map.remove("workflowOperation");
    map.remove("workflow_operation");
    map.insert("graphId".to_string(), json!(graph_id));
    if let Some(workflow_name) = workflow_name {
        map.insert("workflowName".to_string(), json!(workflow_name));
    }
    if let Some(current_page_id) = current_page_id {
        map.insert("pageId".to_string(), json!(current_page_id));
    }
    Ok(open_args)
}

fn extract_workflow_name_from_compose_result(result: &Value) -> Option<String> {
    result
        .get("workflowName")
        .and_then(Value::as_str)
        .or_else(|| {
            result
                .pointer("/compose/workflowName")
                .and_then(Value::as_str)
        })
        .or_else(|| {
            result
                .pointer("/refreshed/authoring/workflowName")
                .and_then(Value::as_str)
        })
        .map(str::to_string)
}

fn draft_move_result(index: usize, move_args: &Value, result: &Value) -> Value {
    json!({
        "kind": "workflowAuthoringDraftMove",
        "index": index,
        "operation": move_args.get("operation").cloned().unwrap_or(Value::Null),
        "workflowName": result.get("workflowName").cloned().unwrap_or(Value::Null),
        "targetPageId": result.pointer("/compose/targetPageId").cloned().unwrap_or(Value::Null),
        "validation": result.get("validation").cloned().unwrap_or(Value::Null),
        "authoring": result.get("authoring").cloned().unwrap_or(Value::Null),
        "result": result,
    })
}

fn workflow_authoring_draft_envelope(
    graph_id: &str,
    workflow_name: Option<&str>,
    book: &Value,
    validation: &Value,
    authoring: &Value,
    mutation_count: usize,
) -> Value {
    let current_page_id = book
        .get("currentPageId")
        .and_then(Value::as_str)
        .unwrap_or("catalog");
    let actions = workflow_authoring_draft_actions(graph_id, workflow_name, current_page_id);
    let next_action = workflow_authoring_draft_next_action(authoring, &actions);
    let projections =
        workflow_authoring_draft_projections(graph_id, workflow_name, validation, mutation_count);

    json!({
        "kind": "workflowAuthoringDraft",
        "draftId": workflow_authoring_draft_id(graph_id, workflow_name),
        "graphId": graph_id,
        "workflowName": workflow_name,
        "currentPageId": current_page_id,
        "status": authoring.get("status").cloned().unwrap_or(Value::Null),
        "message": authoring.get("message").cloned().unwrap_or(Value::Null),
        "mutationCount": mutation_count,
        "meaningfulObject": projections.get("draft").cloned().unwrap_or(Value::Null),
        "completenessGaps": projections.get("completenessGaps").cloned().unwrap_or(Value::Null),
        "draftWarnings": projections.get("draftWarnings").cloned().unwrap_or(Value::Null),
        "runnable": projections.pointer("/draft/runnable").cloned().unwrap_or(Value::Null),
        "validation": {
            "state": "complete",
            "validatedAt": timestamp(),
            "passed": validation.get("passed").cloned().unwrap_or(Value::Null),
            "summary": validation.get("summary").cloned().unwrap_or(Value::Null),
            "authoring": authoring.get("validation").cloned().unwrap_or(Value::Null),
        },
        "persistence": {
            "kind": "workflowRdfDraft",
            "owner": "garden",
            "storage": "graph-user-rdf",
            "namedGraph": book.pointer("/book/readGraph").cloned().unwrap_or(Value::Null),
            "description": "The workflow definition itself is the persistent draft; this session envelope is a UI over that graph state.",
        },
        "projections": projections,
        "actions": actions,
        "nextAction": next_action,
    })
}

fn workflow_authoring_draft_projections(
    graph_id: &str,
    workflow_name: Option<&str>,
    validation: &Value,
    mutation_count: usize,
) -> Value {
    let draft_id = workflow_authoring_draft_id(graph_id, workflow_name);
    let draft_subject = format!("virtual:workflow_authoring_session:draft:{draft_id}");
    let mut definition_subject = Value::Null;
    let mut gaps = Vec::new();
    let mut warnings = Vec::new();

    if let Some(reports) = validation.get("reports").and_then(Value::as_array) {
        for report in reports
            .iter()
            .filter(|report| validation_report_matches_workflow(report, workflow_name))
        {
            if definition_subject.is_null() {
                definition_subject = report
                    .get("workflowUri")
                    .cloned()
                    .unwrap_or_else(|| Value::Null);
            }
            let Some(issues) = report.get("issues").and_then(Value::as_array) else {
                continue;
            };
            for issue in issues {
                match issue.get("severity").and_then(Value::as_str) {
                    Some("error") => {
                        let index = gaps.len() + 1;
                        gaps.push(workflow_authoring_completeness_gap(
                            &draft_subject,
                            index,
                            issue,
                            &definition_subject,
                        ));
                    }
                    Some("warning") => {
                        let index = warnings.len() + 1;
                        warnings.push(workflow_authoring_draft_warning(
                            &draft_subject,
                            index,
                            issue,
                            &definition_subject,
                        ));
                    }
                    _ => {}
                }
            }
        }
    }

    let gap_subjects = gaps
        .iter()
        .filter_map(|gap| gap.get("subject").cloned())
        .collect::<Vec<_>>();
    let warning_subjects = warnings
        .iter()
        .filter_map(|warning| warning.get("subject").cloned())
        .collect::<Vec<_>>();
    let runnable = validation.get("passed").and_then(Value::as_bool) == Some(true)
        && !gaps
            .iter()
            .any(|gap| gap.get("gapBlocking").and_then(Value::as_bool) == Some(true));
    let draft_definition_subject = if definition_subject.is_null() {
        json!(draft_subject.clone())
    } else {
        definition_subject.clone()
    };

    json!({
        "draft": {
            "kind": "workflowDraft",
            "rdfType": "wf:Draft",
            "semanticClass": "wf:Draft",
            "sourceKind": "derived",
            "identityKind": "resolve-by-query",
            "storeMode": "virtual",
            "subject": draft_subject,
            "uri": draft_subject,
            "derivedFromQuery": "workflow_authoring_session.draft(definition subject, composition events, workflow contract)",
            "definitionSubject": draft_definition_subject,
            "workflowName": workflow_name,
            "runnable": runnable,
            "mutationCount": mutation_count,
            "hasCompletenessGap": gap_subjects,
            "hasDraftWarning": warning_subjects,
        },
        "completenessGaps": gaps,
        "draftWarnings": warnings,
    })
}

fn validation_report_matches_workflow(report: &Value, workflow_name: Option<&str>) -> bool {
    match workflow_name {
        Some(workflow_name) => {
            report.get("workflowName").and_then(Value::as_str) == Some(workflow_name)
        }
        None => true,
    }
}

fn workflow_authoring_completeness_gap(
    draft_subject: &str,
    index: usize,
    issue: &Value,
    definition_subject: &Value,
) -> Value {
    let code = issue
        .get("code")
        .and_then(Value::as_str)
        .unwrap_or("validation.error");
    let gap_kind = completeness_gap_kind(code);
    json!({
        "kind": "workflowCompletenessGap",
        "rdfType": "wf:CompletenessGap",
        "semanticClass": "wf:CompletenessGap",
        "sourceKind": "derived",
        "identityKind": "resolve-by-query",
        "storeMode": "virtual",
        "subject": format!("{draft_subject}:gap:{index}"),
        "uri": format!("{draft_subject}:gap:{index}"),
        "derivedFromQuery": "workflow_authoring_session.draft.completenessGaps(definition subject, composition events, workflow contract)",
        "gapKind": gap_kind,
        "gapTarget": issue_target(issue, definition_subject),
        "gapBlocking": true,
        "validationCode": code,
        "rationale": issue.get("message").cloned().unwrap_or(Value::Null),
    })
}

fn workflow_authoring_draft_warning(
    draft_subject: &str,
    index: usize,
    issue: &Value,
    definition_subject: &Value,
) -> Value {
    let code = issue
        .get("code")
        .and_then(Value::as_str)
        .unwrap_or("validation.warning");
    json!({
        "kind": "workflowDraftWarning",
        "rdfType": "wf:DraftWarning",
        "semanticClass": "wf:DraftWarning",
        "sourceKind": "derived",
        "identityKind": "resolve-by-query",
        "storeMode": "virtual",
        "subject": format!("{draft_subject}:warning:{index}"),
        "uri": format!("{draft_subject}:warning:{index}"),
        "derivedFromQuery": "workflow_authoring_session.draft.warnings(definition subject, composition events, workflow contract)",
        "warningKind": normalized_issue_kind(code),
        "warningTarget": issue_target(issue, definition_subject),
        "validationCode": code,
        "rationale": issue.get("message").cloned().unwrap_or(Value::Null),
    })
}

fn issue_target(issue: &Value, definition_subject: &Value) -> Value {
    issue
        .get("subject")
        .cloned()
        .filter(|value| !value.is_null())
        .unwrap_or_else(|| definition_subject.clone())
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
    let mut normalized = String::new();
    let mut previous_dash = false;
    let local = code
        .rsplit_once(':')
        .map(|(_, local)| local)
        .unwrap_or(code);
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

fn workflow_authoring_draft_id(graph_id: &str, workflow_name: Option<&str>) -> String {
    match workflow_name {
        Some(workflow_name) => format!("workflow-draft:{graph_id}:{workflow_name}"),
        None => format!("workflow-draft:{graph_id}:catalog"),
    }
}

fn workflow_authoring_draft_actions(
    graph_id: &str,
    workflow_name: Option<&str>,
    current_page_id: &str,
) -> Value {
    let mut refresh_args = json!({
        "graphId": graph_id,
        "operation": "draft",
        "pageId": current_page_id,
    });
    let mut validate_args = json!({
        "graphId": graph_id,
        "operation": "validate",
    });
    let mut raw_args = json!({
        "graphId": graph_id,
        "pageId": "raw",
    });
    let mut compose_args = json!({
        "graphId": graph_id,
        "operation": "draft",
        "composeOperation": "add_phase",
    });
    let mut perception_args = json!({
        "graphId": graph_id,
        "operation": "choose",
        "pageId": "overview",
        "choiceId": "perception-map",
    });
    let mut prepare_args = json!({
        "graphId": graph_id,
        "operation": "prepare_execute",
    });
    let mut execute_args = json!({
        "graphId": graph_id,
        "operation": "execute",
    });
    let mut background_await_args = json!({
        "graphId": graph_id,
        "operation": "background",
        "mode": "await",
        "pageId": "execute",
    });
    let mut background_validate_args = json!({
        "graphId": graph_id,
        "operation": "background",
        "mode": "validate",
        "pageId": current_page_id,
    });

    if let Some(workflow_name) = workflow_name {
        for args in [
            &mut refresh_args,
            &mut validate_args,
            &mut raw_args,
            &mut compose_args,
            &mut perception_args,
            &mut prepare_args,
            &mut execute_args,
            &mut background_await_args,
            &mut background_validate_args,
        ] {
            args["workflowName"] = json!(workflow_name);
        }
    }

    json!({
        "refresh": {
            "tool": "workflow_authoring_session",
            "arguments": refresh_args,
        },
        "composeMove": {
            "tool": "workflow_authoring_session",
            "arguments": compose_args,
        },
        "validate": {
            "tool": "workflow_authoring_session",
            "arguments": validate_args,
        },
        "perception": {
            "tool": "workflow_authoring_session",
            "arguments": perception_args,
        },
        "prepareExecute": {
            "tool": "workflow_authoring_session",
            "arguments": prepare_args,
        },
        "execute": {
            "tool": "workflow_authoring_session",
            "arguments": execute_args,
        },
        "backgroundAwait": {
            "tool": "workflow_authoring_session",
            "arguments": background_await_args,
        },
        "backgroundValidate": {
            "tool": "workflow_authoring_session",
            "arguments": background_validate_args,
        },
        "rawSparql": {
            "tool": "workflow_book_open",
            "arguments": raw_args,
        },
    })
}

fn workflow_authoring_draft_next_action(authoring: &Value, actions: &Value) -> Value {
    match authoring.get("status").and_then(Value::as_str) {
        Some("ready_to_execute") => actions
            .get("prepareExecute")
            .cloned()
            .unwrap_or(Value::Null),
        Some("needs_repair") => actions.get("validate").cloned().unwrap_or(Value::Null),
        Some("catalog_empty") | Some("choose_workflow") => {
            actions.get("composeMove").cloned().unwrap_or(Value::Null)
        }
        _ => actions.get("perception").cloned().unwrap_or(Value::Null),
    }
}

fn spawn_authoring_await_job(
    app: AppHandle,
    jobs: Arc<LocalJobRegistry>,
    job_id: String,
    graph_id: String,
    await_args: Value,
) {
    tokio::spawn(async move {
        if jobs.is_cancelled(&job_id).unwrap_or(false) {
            return;
        }
        match jobs.mark_running(&job_id) {
            Ok(Some(record)) if matches!(record.status, LocalJobStatus::Running) => {}
            Ok(Some(_)) | Ok(None) => return,
            Err(error) => {
                let _ = jobs.finish_existing(&job_id, Err(error.to_string()), "application/json");
                return;
            }
        }

        let progress = AuthoringJobProgress::new(
            jobs.clone(),
            job_id.clone(),
            graph_id.clone(),
            await_max_ticks(&await_args) as usize,
        );
        progress.report(
            "awaiting",
            "Workflow authoring await loop running",
            0,
            json!({
                "graphId": graph_id,
                "workflowName": mcp_arg_string(&await_args, &["workflowName", "workflow_name"]),
            }),
        );

        let result = authoring_await_loop(app, &await_args, &graph_id, Some(progress.clone()))
            .await
            .map(|payload| session_response("await", &graph_id, payload))
            .map_err(String::from);

        if jobs.is_cancelled(&job_id).unwrap_or(false) {
            return;
        }
        if let Ok(value) = &result {
            progress.report(
                "complete",
                "Workflow authoring await job complete",
                progress.total,
                json!({
                    "await": value.pointer("/payload/await").cloned().unwrap_or(Value::Null),
                    "latestRun": value.pointer("/payload/latestRun").cloned().unwrap_or(Value::Null),
                }),
            );
        }
        if let Err(error) = jobs.finish_existing(&job_id, result, "application/json") {
            log::error!("Failed to finish workflow authoring await job {job_id}: {error}");
        }
    });
}

fn spawn_authoring_validate_job(
    app: AppHandle,
    jobs: Arc<LocalJobRegistry>,
    job_id: String,
    graph_id: String,
    validate_args: Value,
) {
    tokio::spawn(async move {
        if jobs.is_cancelled(&job_id).unwrap_or(false) {
            return;
        }
        match jobs.mark_running(&job_id) {
            Ok(Some(record)) if matches!(record.status, LocalJobStatus::Running) => {}
            Ok(Some(_)) | Ok(None) => return,
            Err(error) => {
                let _ = jobs.finish_existing(&job_id, Err(error.to_string()), "application/json");
                return;
            }
        }

        let progress = AuthoringJobProgress::new(
            jobs.clone(),
            job_id.clone(),
            graph_id.clone(),
            await_max_ticks(&validate_args) as usize,
        );
        progress.report(
            "validating",
            "Workflow authoring validation watcher running",
            0,
            json!({
                "graphId": graph_id,
                "workflowName": mcp_arg_string(&validate_args, &["workflowName", "workflow_name"]),
            }),
        );

        let result =
            authoring_validation_loop(app, &validate_args, &graph_id, Some(progress.clone()))
                .await
                .map(|payload| session_response("validate", &graph_id, payload))
                .map_err(String::from);

        if jobs.is_cancelled(&job_id).unwrap_or(false) {
            return;
        }
        if let Ok(value) = &result {
            progress.report(
                "complete",
                "Workflow authoring validation watcher complete",
                progress.total,
                json!({
                    "validationWatch": value.pointer("/payload/validationWatch").cloned().unwrap_or(Value::Null),
                    "authoring": value.pointer("/authoring").cloned().unwrap_or(Value::Null),
                }),
            );
        }
        if let Err(error) = jobs.finish_existing(&job_id, result, "application/json") {
            log::error!("Failed to finish workflow authoring validation job {job_id}: {error}");
        }
    });
}

async fn authoring_validation_loop(
    app: AppHandle,
    arguments: &Value,
    graph_id: &str,
    progress: Option<AuthoringJobProgress>,
) -> AppResult<Value> {
    let max_ticks = await_max_ticks(arguments);
    let timeout_ms = await_timeout_ms(arguments);
    let resolve_on_change = validation_resolve_on_change(arguments);
    let started = Instant::now();
    let deadline = started + Duration::from_millis(timeout_ms);
    let mut previous_signature =
        mcp_arg_string(arguments, &["baselineSignature", "baseline_signature"]);
    let mut last_payload = Value::Null;
    let mut change_count = 0_u64;

    for tick_index in 0..max_ticks {
        if progress
            .as_ref()
            .map(AuthoringJobProgress::is_cancelled)
            .unwrap_or(false)
        {
            attach_validation_watch(
                &mut last_payload,
                ValidationWatchOutcome {
                    status: "cancelled",
                    reason: "cancelled",
                    iterations: tick_index,
                    timeout_ms,
                    max_ticks,
                    started,
                    baseline_signature: previous_signature.clone(),
                    signature: None,
                    changed: false,
                    change_count,
                    resolve_on_change,
                },
            );
            return Ok(last_payload);
        }

        let mut payload = authoring_validation_snapshot(app.clone(), arguments, graph_id)?;
        let signature = validation_watch_signature(&payload);
        let iterations = tick_index + 1;
        let baseline_signature = previous_signature.clone();
        let changed = baseline_signature
            .as_ref()
            .map(|baseline| baseline != &signature)
            .unwrap_or(false);
        if changed {
            change_count += 1;
        }

        if previous_signature.is_none() {
            previous_signature = Some(signature.clone());
        }

        if let Some(progress) = &progress {
            progress.report(
                if changed { "changed" } else { "validate" },
                if changed {
                    "Workflow authoring validation changed"
                } else {
                    "Workflow authoring validation tick complete"
                },
                iterations as usize,
                json!({
                    "signature": signature,
                    "baselineSignature": baseline_signature,
                    "changed": changed,
                    "changeCount": change_count,
                    "validation": payload.get("validation").cloned().unwrap_or(Value::Null),
                    "authoring": payload.pointer("/book/authoring").cloned().unwrap_or(Value::Null),
                }),
            );
        }

        if changed && resolve_on_change {
            attach_validation_watch(
                &mut payload,
                ValidationWatchOutcome {
                    status: "changed",
                    reason: "signature_changed",
                    iterations,
                    timeout_ms,
                    max_ticks,
                    started,
                    baseline_signature,
                    signature: Some(signature),
                    changed: true,
                    change_count,
                    resolve_on_change,
                },
            );
            return Ok(payload);
        }

        last_payload = payload;
        if Instant::now() >= deadline || iterations >= max_ticks {
            attach_validation_watch(
                &mut last_payload,
                ValidationWatchOutcome {
                    status: if change_count > 0 {
                        "changed"
                    } else {
                        "timeout"
                    },
                    reason: if change_count > 0 {
                        "budget_expired_after_change"
                    } else {
                        "timeout"
                    },
                    iterations,
                    timeout_ms,
                    max_ticks,
                    started,
                    baseline_signature: previous_signature.clone(),
                    signature: Some(signature),
                    changed: change_count > 0,
                    change_count,
                    resolve_on_change,
                },
            );
            return Ok(last_payload);
        }

        let poll_ms = watch_poll_ms(arguments);
        if poll_ms > 0 {
            let remaining_ms = deadline
                .saturating_duration_since(Instant::now())
                .as_millis()
                .min(poll_ms as u128) as u64;
            if remaining_ms > 0 {
                sleep(Duration::from_millis(remaining_ms)).await;
            }
        }
    }

    attach_validation_watch(
        &mut last_payload,
        ValidationWatchOutcome {
            status: "timeout",
            reason: "tick_budget_expired",
            iterations: max_ticks,
            timeout_ms,
            max_ticks,
            started,
            baseline_signature: previous_signature,
            signature: None,
            changed: change_count > 0,
            change_count,
            resolve_on_change,
        },
    );
    Ok(last_payload)
}

fn authoring_validation_snapshot(
    app: AppHandle,
    arguments: &Value,
    graph_id: &str,
) -> AppResult<Value> {
    let validation = mcp_local_workflow_book_validate(app.clone(), arguments)?;
    let book = mcp_local_workflow_book_open(app, arguments)?;
    let authoring = book.get("authoring").cloned().unwrap_or(Value::Null);
    let workflow_name = mcp_arg_string(arguments, &["workflowName", "workflow_name"]);
    let draft = workflow_authoring_draft_envelope(
        graph_id,
        workflow_name.as_deref(),
        &book,
        &validation,
        &authoring,
        0,
    );
    let signature = workflow_book_validation_signature(&book, &validation);

    Ok(json!({
        "validation": validation,
        "book": book,
        "draft": draft,
        "validationWatch": {
            "kind": "workflowAuthoringValidationWatch",
            "status": "sampled",
            "signature": signature,
        },
    }))
}

fn validation_resolve_on_change(arguments: &Value) -> bool {
    mcp_arg_bool(
        arguments,
        &[
            "resolveOnChange",
            "resolve_on_change",
            "untilChanged",
            "until_changed",
        ],
        true,
    )
}

fn validation_watch_signature(payload: &Value) -> String {
    let book = payload.get("book").cloned().unwrap_or(Value::Null);
    let validation = payload.get("validation").cloned().unwrap_or(Value::Null);
    workflow_book_validation_signature(&book, &validation)
}

fn workflow_book_validation_signature(book: &Value, validation: &Value) -> String {
    let material = json!({
        "book": book.get("book").cloned().unwrap_or(Value::Null),
        "currentPageId": book.get("currentPageId").cloned().unwrap_or(Value::Null),
        "page": book.get("page").cloned().unwrap_or(Value::Null),
        "validation": validation,
    });
    stable_json_signature(&material)
}

fn stable_json_signature(value: &Value) -> String {
    let serialized = serde_json::to_string(value).unwrap_or_default();
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in serialized.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

struct ValidationWatchOutcome {
    status: &'static str,
    reason: &'static str,
    iterations: u64,
    timeout_ms: u64,
    max_ticks: u64,
    started: Instant,
    baseline_signature: Option<String>,
    signature: Option<String>,
    changed: bool,
    change_count: u64,
    resolve_on_change: bool,
}

fn attach_validation_watch(payload: &mut Value, outcome: ValidationWatchOutcome) {
    let elapsed_ms = outcome.started.elapsed().as_millis() as u64;
    if !payload.is_object() {
        *payload = json!({});
    }
    if let Some(map) = payload.as_object_mut() {
        map.insert(
            "validationWatch".to_string(),
            json!({
                "kind": "workflowAuthoringValidationWatch",
                "status": outcome.status,
                "reason": outcome.reason,
                "iterations": outcome.iterations,
                "elapsedMs": elapsed_ms,
                "timeoutMs": outcome.timeout_ms,
                "maxTicks": outcome.max_ticks,
                "baselineSignature": outcome.baseline_signature,
                "signature": outcome.signature,
                "changed": outcome.changed,
                "changeCount": outcome.change_count,
                "resolveOnChange": outcome.resolve_on_change,
            }),
        );
    }
}

async fn authoring_await_loop(
    app: AppHandle,
    arguments: &Value,
    graph_id: &str,
    progress: Option<AuthoringJobProgress>,
) -> AppResult<Value> {
    let max_ticks = await_max_ticks(arguments);
    let timeout_ms = await_timeout_ms(arguments);
    let started = Instant::now();
    let deadline = started + Duration::from_millis(timeout_ms);
    let mut watch_args = arguments.clone();
    prepare_await_watch_args(&mut watch_args)?;
    let mut last_payload = Value::Null;

    for tick_index in 0..max_ticks {
        if progress
            .as_ref()
            .map(AuthoringJobProgress::is_cancelled)
            .unwrap_or(false)
        {
            let resolution = json!({
                "kind": "workflowAuthoringAwaitResolution",
                "reason": "cancelled",
                "terminal": false,
                "message": "Workflow authoring await job was cancelled.",
                "latestRun": last_payload.get("latestRun").cloned().unwrap_or(Value::Null),
            });
            attach_await_result(
                &mut last_payload,
                "cancelled",
                tick_index,
                timeout_ms,
                max_ticks,
                started,
                resolution,
            );
            return Ok(last_payload);
        }
        let mut payload = authoring_watch(app.clone(), &watch_args, graph_id).await?;
        let mut resolution = await_resolution(&payload);
        let iterations = tick_index + 1;

        if resolution_is_terminal_run(&resolution) && terminal_run_needs_monitor_refresh(&payload) {
            payload = refresh_terminal_monitor_payload(app.clone(), &watch_args, graph_id, payload)
                .await?;
            resolution = if payload_has_terminal_monitor(&payload) {
                await_resolution(&payload)
            } else if payload_monitor_unavailable(&payload) {
                await_monitor_unavailable_resolution(&payload)
            } else {
                None
            };
        }

        last_payload = payload;
        if let Some(progress) = &progress {
            progress.report(
                "watch",
                "Workflow authoring await tick complete",
                iterations as usize,
                json!({
                    "latestRun": last_payload.get("latestRun").cloned().unwrap_or(Value::Null),
                    "monitorStatus": last_payload.pointer("/monitor/status").cloned().unwrap_or(Value::Null),
                    "resolution": resolution.clone().unwrap_or(Value::Null),
                    "nextAction": last_payload.pointer("/watch/nextAction").cloned().unwrap_or(Value::Null),
                }),
            );
        }

        if let Some(resolution) = resolution {
            if let Some(progress) = &progress {
                progress.report(
                    "resolved",
                    "Workflow authoring await resolved",
                    iterations as usize,
                    json!({
                        "resolution": resolution.clone(),
                        "latestRun": last_payload.get("latestRun").cloned().unwrap_or(Value::Null),
                    }),
                );
            }
            attach_await_result(
                &mut last_payload,
                "resolved",
                iterations,
                timeout_ms,
                max_ticks,
                started,
                resolution,
            );
            return Ok(last_payload);
        }

        if Instant::now() >= deadline || iterations >= max_ticks {
            let resolution = json!({
                "kind": "workflowAuthoringAwaitResolution",
                "reason": "timeout",
                "terminal": false,
                "message": "Latest workflow run is still active after the await budget.",
                "latestRun": last_payload.get("latestRun").cloned().unwrap_or(Value::Null),
            });
            if let Some(progress) = &progress {
                progress.report(
                    "timeout",
                    "Workflow authoring await budget expired",
                    iterations as usize,
                    json!({
                        "resolution": resolution.clone(),
                        "latestRun": last_payload.get("latestRun").cloned().unwrap_or(Value::Null),
                    }),
                );
            }
            attach_await_result(
                &mut last_payload,
                "timeout",
                iterations,
                timeout_ms,
                max_ticks,
                started,
                resolution,
            );
            return Ok(last_payload);
        }

        if let Some(next_args) = last_payload.pointer("/watch/nextAction/arguments").cloned() {
            watch_args = next_args;
        }

        let poll_ms = watch_poll_ms(&watch_args);
        if poll_ms > 0 {
            let remaining_ms = deadline
                .saturating_duration_since(Instant::now())
                .as_millis()
                .min(poll_ms as u128) as u64;
            if remaining_ms > 0 {
                sleep(Duration::from_millis(remaining_ms)).await;
            }
        }
    }

    let resolution = json!({
        "kind": "workflowAuthoringAwaitResolution",
        "reason": "timeout",
        "terminal": false,
        "message": "Latest workflow run is still active after the await tick budget.",
        "latestRun": last_payload.get("latestRun").cloned().unwrap_or(Value::Null),
    });
    if let Some(progress) = &progress {
        progress.report(
            "timeout",
            "Workflow authoring await tick budget expired",
            max_ticks as usize,
            json!({
                "resolution": resolution.clone(),
                "latestRun": last_payload.get("latestRun").cloned().unwrap_or(Value::Null),
            }),
        );
    }
    attach_await_result(
        &mut last_payload,
        "timeout",
        max_ticks,
        timeout_ms,
        max_ticks,
        started,
        resolution,
    );
    Ok(last_payload)
}

#[derive(Clone)]
struct AuthoringJobProgress {
    jobs: Arc<LocalJobRegistry>,
    job_id: String,
    graph_id: String,
    total: usize,
}

impl AuthoringJobProgress {
    fn new(jobs: Arc<LocalJobRegistry>, job_id: String, graph_id: String, total: usize) -> Self {
        Self {
            jobs,
            job_id,
            graph_id,
            total: total.max(1),
        }
    }

    fn is_cancelled(&self) -> bool {
        self.jobs.is_cancelled(&self.job_id).unwrap_or(false)
    }

    fn report(&self, phase: &str, message: &str, current: usize, details: Value) {
        let current = current.min(self.total);
        let percent = if self.total == 0 {
            0.0
        } else {
            (current as f64 / self.total as f64) * 100.0
        };
        let progress = LocalJobProgress {
            phase: phase.to_string(),
            message: message.to_string(),
            current,
            total: self.total,
            percent,
            updated_at: timestamp(),
            details: json!({
                "graphId": self.graph_id,
                "workflowAuthoring": details,
            }),
        };
        if let Err(error) = self.jobs.update_progress(&self.job_id, progress) {
            log::warn!(
                "Failed to update workflow authoring job {} progress: {}",
                self.job_id,
                error
            );
        }
    }
}

fn workflow_authoring_job_actions(job_id: &str) -> Value {
    json!({
        "status": {
            "tool": "get_job_status",
            "arguments": {"jobId": job_id},
        },
        "result": {
            "tool": "get_job_result",
            "arguments": {"jobId": job_id},
        },
        "cancel": {
            "tool": "cancel_job",
            "arguments": {"jobId": job_id},
        },
    })
}

fn should_monitor_latest_run(
    arguments: &Value,
    workflow_name: Option<&str>,
    latest_run: &Value,
) -> bool {
    mcp_arg_bool(arguments, &["monitorLatest", "monitor_latest"], true)
        && workflow_name.is_some()
        && latest_run.get("runId").and_then(Value::as_str).is_some()
        && latest_run
            .get("status")
            .and_then(Value::as_str)
            .map(run_status_needs_monitor)
            .unwrap_or(true)
}

fn watch_monitor_arguments(arguments: &Value, graph_id: &str, workflow_name: &str) -> Value {
    json!({
        "graphId": graph_id,
        "workflowName": workflow_name,
        "since": mcp_arg_u64(arguments, &["since"]).unwrap_or(0),
        "includeEvents": mcp_arg_bool(arguments, &["includeEvents", "include_events"], true),
    })
}

fn watch_next_action(
    arguments: &Value,
    graph_id: &str,
    workflow_name: Option<&str>,
    next_since: u64,
) -> Value {
    let mut action_args = json!({
        "graphId": graph_id,
        "operation": "watch",
        "pollMs": watch_poll_ms(arguments),
        "since": next_since,
        "includeEvents": mcp_arg_bool(arguments, &["includeEvents", "include_events"], true),
        "monitorLatest": mcp_arg_bool(arguments, &["monitorLatest", "monitor_latest"], true),
    });
    if let Some(workflow_name) = workflow_name {
        action_args["workflowName"] = json!(workflow_name);
    }
    if let Some(page_id) = mcp_arg_string(arguments, &["pageId", "page_id"]) {
        action_args["pageId"] = json!(page_id);
    }
    json!({
        "tool": "workflow_authoring_session",
        "arguments": action_args,
    })
}

fn watch_poll_ms(arguments: &Value) -> u64 {
    mcp_arg_u64(
        arguments,
        &["pollMs", "poll_ms", "pollAfterMs", "poll_after_ms"],
    )
    .unwrap_or(2_000)
}

fn await_max_ticks(arguments: &Value) -> u64 {
    mcp_arg_u64(
        arguments,
        &["maxTicks", "max_ticks", "maxPolls", "max_polls"],
    )
    .unwrap_or(20)
    .clamp(1, 240)
}

fn await_timeout_ms(arguments: &Value) -> u64 {
    mcp_arg_u64(
        arguments,
        &["timeoutMs", "timeout_ms", "maxWaitMs", "max_wait_ms"],
    )
    .unwrap_or(30_000)
    .clamp(1, 300_000)
}

fn prepare_await_watch_args(arguments: &mut Value) -> AppResult<()> {
    let Some(map) = arguments.as_object_mut() else {
        return Err(AppError::validation(
            "workflow_authoring_session arguments must be an object",
        ));
    };
    map.insert("operation".to_string(), json!("watch"));
    Ok(())
}

fn monitor_next_since(monitor: &Value) -> Option<u64> {
    monitor
        .get("nextSince")
        .and_then(|value| {
            value
                .as_u64()
                .or_else(|| value.as_str().and_then(|raw| raw.parse::<u64>().ok()))
        })
        .or_else(|| {
            monitor
                .pointer("/events/nextSince")
                .and_then(|value| value.as_u64())
        })
}

async fn refresh_terminal_monitor_payload(
    app: AppHandle,
    arguments: &Value,
    graph_id: &str,
    mut payload: Value,
) -> AppResult<Value> {
    let Some(run_id) = payload
        .get("latestRun")
        .and_then(|latest| latest.get("runId"))
        .and_then(Value::as_str)
        .map(str::to_string)
    else {
        return Ok(payload);
    };
    let workflow_name =
        mcp_arg_string(arguments, &["workflowName", "workflow_name"]).or_else(|| {
            payload
                .pointer("/watch/nextAction/arguments/workflowName")
                .and_then(Value::as_str)
                .map(str::to_string)
        });
    let mut monitor_args = json!({
        "graphId": graph_id,
        "runId": run_id,
        "since": mcp_arg_u64(arguments, &["since"]).unwrap_or(0),
        "includeEvents": mcp_arg_bool(arguments, &["includeEvents", "include_events"], true),
    });
    if let Some(workflow_name) = workflow_name.as_deref() {
        monitor_args["workflowName"] = json!(workflow_name);
    }

    let monitor = mcp_local_workflow_run_monitor(app.clone(), &monitor_args)
        .await
        .unwrap_or_else(|error| {
            json!({
                "kind": "workflowRunMonitor",
                "status": "monitor_failed",
                "graphId": graph_id,
                "workflowName": workflow_name,
                "runId": monitor_args.get("runId").cloned().unwrap_or(Value::Null),
                "error": error.to_string(),
            })
        });
    let next_since = monitor_next_since(&monitor)
        .or_else(|| mcp_arg_u64(arguments, &["since"]))
        .unwrap_or(0);
    let monitor_ok = monitor.get("status").and_then(Value::as_str) == Some("ok");
    payload["monitor"] = monitor;
    payload["watch"]["monitorAttempted"] = json!(true);

    if monitor_ok {
        if let Some(workflow_name) = workflow_name.as_deref() {
            let book = mcp_local_workflow_book_open(app, arguments)?;
            let latest_run = latest_workflow_run_from_book(&book).unwrap_or(Value::Null);
            payload["book"] = book;
            payload["latestRun"] = latest_run;
            payload["watch"]["nextAction"] =
                watch_next_action(arguments, graph_id, Some(workflow_name), next_since);
        } else if let Some(next_action) = payload.pointer_mut("/watch/nextAction") {
            next_action["arguments"]["since"] = json!(next_since);
        }
    }

    Ok(payload)
}

fn resolution_is_terminal_run(resolution: &Option<Value>) -> bool {
    resolution
        .as_ref()
        .and_then(|resolution| resolution.get("reason"))
        .and_then(Value::as_str)
        == Some("terminal_run")
}

fn terminal_run_needs_monitor_refresh(payload: &Value) -> bool {
    !payload_has_terminal_monitor(payload)
        && payload
            .get("latestRun")
            .and_then(|latest| latest.get("runId"))
            .and_then(Value::as_str)
            .is_some()
}

fn payload_has_terminal_monitor(payload: &Value) -> bool {
    let Some(monitor) = payload.get("monitor") else {
        return false;
    };
    monitor.get("status").and_then(Value::as_str) == Some("ok")
        && monitor_run_status(monitor)
            .map(|status| !run_status_needs_monitor(status))
            .unwrap_or(false)
}

fn payload_monitor_unavailable(payload: &Value) -> bool {
    let monitor_status = payload
        .get("monitor")
        .and_then(|monitor| monitor.get("status"))
        .and_then(Value::as_str);
    matches!(
        monitor_status,
        Some("monitor_failed" | "service_unavailable" | "service_not_running")
    )
}

fn monitor_run_status(monitor: &Value) -> Option<&str> {
    monitor
        .pointer("/detail/body/header/status")
        .and_then(Value::as_str)
        .or_else(|| {
            monitor
                .pointer("/runRecord/runStatus")
                .and_then(Value::as_str)
        })
        .or_else(|| {
            monitor
                .pointer("/detail/body/status")
                .and_then(Value::as_str)
        })
}

fn await_monitor_unavailable_resolution(payload: &Value) -> Option<Value> {
    let monitor = payload.get("monitor").cloned().unwrap_or(Value::Null);
    Some(json!({
        "kind": "workflowAuthoringAwaitResolution",
        "reason": "monitor_unavailable",
        "terminal": false,
        "message": "The latest run appears terminal in the book, but monitoring is currently unavailable.",
        "latestRun": payload.get("latestRun").cloned().unwrap_or(Value::Null),
        "monitor": monitor,
    }))
}

fn await_resolution(payload: &Value) -> Option<Value> {
    let latest_run = payload.get("latestRun").cloned().unwrap_or(Value::Null);
    let run_id = latest_run.get("runId").and_then(Value::as_str);
    if run_id.is_none() {
        return Some(json!({
            "kind": "workflowAuthoringAwaitResolution",
            "reason": "no_run",
            "terminal": true,
            "message": "No workflow run is currently visible in the book.",
            "latestRun": latest_run,
        }));
    }

    let latest_status = latest_run.get("status").and_then(Value::as_str);
    if latest_status
        .map(|status| !run_status_needs_monitor(status))
        .unwrap_or(false)
    {
        return Some(json!({
            "kind": "workflowAuthoringAwaitResolution",
            "reason": "terminal_run",
            "terminal": true,
            "runId": run_id,
            "runStatus": latest_status,
            "latestRun": latest_run,
        }));
    }

    let monitor_attempted = payload
        .pointer("/watch/monitorAttempted")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if !monitor_attempted {
        return Some(json!({
            "kind": "workflowAuthoringAwaitResolution",
            "reason": "needs_workflow_context",
            "terminal": false,
            "message": "Latest run appears active, but watch could not monitor it without workflow context.",
            "latestRun": latest_run,
        }));
    }

    let monitor = payload.get("monitor").cloned().unwrap_or(Value::Null);
    let monitor_status = monitor.get("status").and_then(Value::as_str);
    if matches!(
        monitor_status,
        Some("monitor_failed" | "service_unavailable" | "service_not_running")
    ) {
        return Some(json!({
            "kind": "workflowAuthoringAwaitResolution",
            "reason": "monitor_unavailable",
            "terminal": false,
            "message": "The latest run is active, but monitoring is currently unavailable.",
            "latestRun": latest_run,
            "monitor": monitor,
        }));
    }

    None
}

fn attach_await_result(
    payload: &mut Value,
    status: &str,
    iterations: u64,
    timeout_ms: u64,
    max_ticks: u64,
    started: Instant,
    resolution: Value,
) {
    let elapsed_ms = started.elapsed().as_millis() as u64;
    let next_action = payload
        .pointer("/watch/nextAction")
        .cloned()
        .unwrap_or(Value::Null);
    if let Some(map) = payload.as_object_mut() {
        map.insert(
            "await".to_string(),
            json!({
                "kind": "workflowAuthoringAwait",
                "status": status,
                "iterations": iterations,
                "elapsedMs": elapsed_ms,
                "timeoutMs": timeout_ms,
                "maxTicks": max_ticks,
                "resolution": resolution,
                "nextAction": next_action,
            }),
        );
    }
}

fn latest_workflow_run_from_book(book: &Value) -> Option<Value> {
    book.pointer("/page/objects")
        .and_then(Value::as_array)
        .and_then(|objects| latest_run_from_objects(objects))
        .or_else(|| {
            book.pointer("/book/pages")
                .and_then(Value::as_array)?
                .iter()
                .filter_map(|page| {
                    page.get("objects")
                        .and_then(Value::as_array)
                        .and_then(|objects| latest_run_from_objects(objects))
                })
                .next()
        })
}

fn latest_run_from_objects(objects: &[Value]) -> Option<Value> {
    objects
        .iter()
        .find(|object| object.get("kind").and_then(Value::as_str) == Some("workflowRun"))
        .cloned()
}

fn run_status_needs_monitor(status: &str) -> bool {
    matches!(
        status.trim().to_ascii_lowercase().as_str(),
        "" | "queued" | "pending" | "running" | "started" | "starting"
    )
}

fn compose_arguments(arguments: &Value) -> AppResult<Value> {
    let session_operation = mcp_arg_string(arguments, &["operation", "op"]).unwrap_or_default();
    let compose_operation = if matches!(
        session_operation.trim().replace('-', "_").as_str(),
        "create_workflow"
            | "add_phase"
            | "attach_agent_node"
            | "bind_source_block"
            | "bind_input_block"
    ) {
        session_operation
    } else {
        mcp_arg_string(
            arguments,
            &[
                "composeOperation",
                "compose_operation",
                "workflowOperation",
                "workflow_operation",
                "move",
            ],
        )
        .ok_or_else(|| {
            AppError::validation(
                "composeOperation is required when workflow_authoring_session operation is compose",
            )
        })?
    };
    let mut compose_args = arguments.clone();
    let Some(map) = compose_args.as_object_mut() else {
        return Err(AppError::validation(
            "workflow_authoring_session arguments must be an object",
        ));
    };
    map.insert("operation".to_string(), json!(compose_operation));
    Ok(compose_args)
}

fn execution_book(app: AppHandle, arguments: &Value) -> AppResult<Value> {
    let mut open_args = arguments.clone();
    let Some(map) = open_args.as_object_mut() else {
        return Err(AppError::validation(
            "workflow_authoring_session arguments must be an object",
        ));
    };
    map.insert("pageId".to_string(), json!("execute"));
    mcp_local_workflow_book_open(app, &open_args)
}

fn execution_choice(book: &Value, arguments: &Value) -> AppResult<(Value, Value)> {
    let page = book
        .get("page")
        .ok_or_else(|| AppError::internal("workflow book response did not include page"))?;
    let choices = page
        .get("choices")
        .and_then(Value::as_array)
        .ok_or_else(|| AppError::internal("workflow book page did not include choices"))?;
    let choice_id = mcp_arg_string(
        arguments,
        &[
            "executionChoiceId",
            "execution_choice_id",
            "runChoiceId",
            "run_choice_id",
            "choiceId",
            "choice_id",
        ],
    )
    .unwrap_or_else(|| default_execution_choice_id(page, choices));
    let choice = choices
        .iter()
        .find(|choice| choice.get("id").and_then(Value::as_str) == Some(choice_id.as_str()))
        .cloned()
        .ok_or_else(|| {
            AppError::validation(format!(
                "unknown execution choice '{choice_id}' on execute page"
            ))
        })?;
    let tool = choice.pointer("/action/tool").and_then(Value::as_str);
    if tool != Some("workflow_run_start") {
        return Err(AppError::validation(format!(
            "execution choice '{choice_id}' is not a workflow_run_start MCP action"
        )));
    }
    let mut action_args = choice
        .pointer("/action/arguments")
        .cloned()
        .ok_or_else(|| {
            AppError::validation(format!(
                "execution choice '{choice_id}' is missing action arguments"
            ))
        })?;
    apply_run_argument_overrides(&mut action_args, arguments)?;
    Ok((choice, action_args))
}

fn default_execution_choice_id(page: &Value, choices: &[Value]) -> String {
    if let Some(suggested) = page.get("suggestedChoiceId").and_then(Value::as_str) {
        if choices.iter().any(|choice| {
            choice.get("id").and_then(Value::as_str) == Some(suggested)
                && choice.pointer("/action/tool").and_then(Value::as_str)
                    == Some("workflow_run_start")
        }) {
            return suggested.to_string();
        }
    }
    for candidate in ["start-run-with-source-block", "start-run-mcp"] {
        if choices.iter().any(|choice| {
            choice.get("id").and_then(Value::as_str) == Some(candidate)
                && choice.pointer("/action/tool").and_then(Value::as_str)
                    == Some("workflow_run_start")
        }) {
            return candidate.to_string();
        }
    }
    "start-run-mcp".to_string()
}

fn apply_run_argument_overrides(action_args: &mut Value, arguments: &Value) -> AppResult<()> {
    let Some(map) = action_args.as_object_mut() else {
        return Err(AppError::validation(
            "workflow_run_start action arguments must be an object",
        ));
    };
    for key in [
        "workflowArgs",
        "workflow_args",
        "workflowArgsBlock",
        "workflow_args_block",
        "workflowSource",
        "workflow_source",
        "workflowBinding",
        "workflow_binding",
        "workflowSourceBlock",
        "workflow_source_block",
        "sourceDocumentId",
        "source_document_id",
        "sourceBlockId",
        "source_block_id",
        "inputDocumentId",
        "input_document_id",
        "inputBlockId",
        "input_block_id",
        "scriptSha256",
        "script_sha256",
        "autoStart",
        "auto_start",
        "allowInvalid",
        "allow_invalid",
    ] {
        if let Some(value) = arguments.get(key) {
            map.insert(key.to_string(), value.clone());
        }
    }
    Ok(())
}

fn session_response(operation: &str, graph_id: &str, payload: Value) -> Value {
    let authoring = extract_authoring(&payload);
    json!({
        "kind": "workflowAuthoringSession",
        "operation": operation,
        "graphId": graph_id,
        "workflowName": extract_workflow_name(&payload, &authoring),
        "authoring": authoring,
        "payload": payload,
    })
}

fn extract_authoring(value: &Value) -> Value {
    value
        .get("authoring")
        .cloned()
        .or_else(|| value.pointer("/book/authoring").cloned())
        .or_else(|| value.pointer("/compose/authoring").cloned())
        .or_else(|| value.pointer("/compose/refreshed/authoring").cloned())
        .unwrap_or(Value::Null)
}

fn extract_workflow_name(payload: &Value, authoring: &Value) -> Value {
    authoring
        .get("workflowName")
        .cloned()
        .filter(|value| !value.is_null())
        .or_else(|| payload.pointer("/book/book/workflowName").cloned())
        .or_else(|| payload.pointer("/compose/workflowName").cloned())
        .or_else(|| payload.pointer("/run/workflowName").cloned())
        .unwrap_or(Value::Null)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_execution_choice_prefers_workflow_run_start() {
        let page = json!({
            "suggestedChoiceId": "validate-graph"
        });
        let choices = vec![
            json!({"id": "validate-graph", "action": null}),
            json!({"id": "start-run-mcp", "action": {"tool": "workflow_run_start"}}),
            json!({"id": "submit-run", "action": {"facade": {"path": "/workflows/runs"}}}),
        ];
        assert_eq!(
            default_execution_choice_id(&page, &choices),
            "start-run-mcp"
        );

        let page = json!({
            "suggestedChoiceId": "start-run-with-source-block"
        });
        let choices = vec![
            json!({"id": "start-run-mcp", "action": {"tool": "workflow_run_start"}}),
            json!({"id": "start-run-with-source-block", "action": {"tool": "workflow_run_start"}}),
        ];
        assert_eq!(
            default_execution_choice_id(&page, &choices),
            "start-run-with-source-block"
        );
    }

    #[test]
    fn compose_arguments_accepts_session_or_direct_operation() {
        let direct = compose_arguments(&json!({
            "graphId": "lab",
            "operation": "add_phase",
            "workflowName": "demo"
        }))
        .unwrap();
        assert_eq!(
            direct.get("operation").and_then(Value::as_str),
            Some("add_phase")
        );

        let wrapped = compose_arguments(&json!({
            "graphId": "lab",
            "operation": "compose",
            "composeOperation": "bind_input_block",
            "workflowName": "demo"
        }))
        .unwrap();
        assert_eq!(
            wrapped.get("operation").and_then(Value::as_str),
            Some("bind_input_block")
        );
    }

    #[test]
    fn draft_helpers_build_inherited_moves_and_actions() {
        assert_eq!(normalize_session_operation("open-draft"), "draft");
        assert_eq!(normalize_session_operation("draft_move"), "draft");
        assert_eq!(
            normalize_session_operation("validate-background"),
            "background"
        );
        assert_eq!(
            normalize_background_mode(&json!({"operation": "background_validate"})),
            "validate"
        );
        assert_eq!(
            normalize_background_mode(&json!({"operation": "background", "mode": "await"})),
            "await"
        );

        let arguments = json!({
            "graphId": "lab",
            "operation": "draft",
            "workflowName": "demo",
            "pageId": "overview",
            "moves": [
                {"operation": "add_phase", "phaseOrder": 1, "phaseTitle": "Plan"},
                {"composeOperation": "bind_input_block", "inputDocumentId": "doc", "inputBlockId": "block"}
            ]
        });
        let templates = draft_move_templates(&arguments).unwrap();
        assert_eq!(templates.len(), 2);
        let first = draft_move_arguments(&arguments, &templates[0], "lab", Some("demo")).unwrap();
        assert_eq!(
            first.get("operation").and_then(Value::as_str),
            Some("add_phase")
        );
        assert_eq!(
            first.get("workflowName").and_then(Value::as_str),
            Some("demo")
        );
        assert!(first.get("moves").is_none());

        let second = draft_move_arguments(&arguments, &templates[1], "lab", Some("demo")).unwrap();
        assert_eq!(
            second.get("operation").and_then(Value::as_str),
            Some("bind_input_block")
        );
        assert_eq!(
            second.get("inputBlockId").and_then(Value::as_str),
            Some("block")
        );

        let actions = workflow_authoring_draft_actions("lab", Some("demo"), "execute");
        assert_eq!(
            actions
                .pointer("/refresh/arguments/operation")
                .and_then(Value::as_str),
            Some("draft")
        );
        assert_eq!(
            actions
                .pointer("/backgroundAwait/arguments/operation")
                .and_then(Value::as_str),
            Some("background")
        );
        assert_eq!(
            actions
                .pointer("/backgroundValidate/arguments/mode")
                .and_then(Value::as_str),
            Some("validate")
        );
        assert_eq!(
            workflow_authoring_draft_next_action(&json!({"status": "ready_to_execute"}), &actions)
                .pointer("/arguments/operation")
                .and_then(Value::as_str),
            Some("prepare_execute")
        );

        let before = workflow_book_validation_signature(
            &json!({"book": {"pages": [{"id": "overview"}]}, "currentPageId": "overview"}),
            &json!({"passed": true}),
        );
        let after = workflow_book_validation_signature(
            &json!({"book": {"pages": [{"id": "overview", "warnings": ["changed"]}]}, "currentPageId": "overview"}),
            &json!({"passed": true}),
        );
        assert_ne!(before, after);
    }

    #[test]
    fn draft_envelope_derives_virtual_meaningful_objects_from_validation() {
        let book = json!({
            "currentPageId": "overview",
            "book": {
                "readGraph": "urn:mnemosyne:local:graph:lab:user:rdf"
            }
        });
        let validation = json!({
            "passed": false,
            "summary": {
                "workflowCount": 1,
                "errors": 1,
                "warnings": 1
            },
            "reports": [{
                "workflowName": "demo",
                "workflowUri": "urn:sophia:workflow:lab:demo",
                "issues": [
                    {
                        "severity": "error",
                        "code": "wf:scriptBlock",
                        "message": "Workflow must point at the script block.",
                        "subject": "urn:sophia:workflow:lab:demo"
                    },
                    {
                        "severity": "warning",
                        "code": "wf:AgentNode.empty",
                        "message": "Workflow has no agent nodes.",
                        "subject": "urn:sophia:workflow:lab:demo"
                    }
                ]
            }]
        });
        let draft = workflow_authoring_draft_envelope(
            "lab",
            Some("demo"),
            &book,
            &validation,
            &json!({"status": "needs_repair"}),
            2,
        );

        assert_eq!(
            draft
                .pointer("/meaningfulObject/rdfType")
                .and_then(Value::as_str),
            Some("wf:Draft")
        );
        assert_eq!(
            draft
                .pointer("/meaningfulObject/semanticClass")
                .and_then(Value::as_str),
            Some("wf:Draft")
        );
        assert_eq!(
            draft
                .pointer("/meaningfulObject/uri")
                .and_then(Value::as_str),
            draft
                .pointer("/meaningfulObject/subject")
                .and_then(Value::as_str)
        );
        assert_eq!(
            draft
                .pointer("/meaningfulObject/runnable")
                .and_then(Value::as_bool),
            Some(false)
        );
        assert_eq!(
            draft
                .pointer("/completenessGaps/0/rdfType")
                .and_then(Value::as_str),
            Some("wf:CompletenessGap")
        );
        assert_eq!(
            draft
                .pointer("/completenessGaps/0/semanticClass")
                .and_then(Value::as_str),
            Some("wf:CompletenessGap")
        );
        assert_eq!(
            draft
                .pointer("/completenessGaps/0/gapKind")
                .and_then(Value::as_str),
            Some("unbound-source")
        );
        assert_eq!(
            draft
                .pointer("/draftWarnings/0/rdfType")
                .and_then(Value::as_str),
            Some("wf:DraftWarning")
        );
        assert_eq!(
            draft
                .pointer("/draftWarnings/0/semanticClass")
                .and_then(Value::as_str),
            Some("wf:DraftWarning")
        );
        assert_eq!(
            draft
                .pointer("/projections/draft/hasCompletenessGap/0")
                .and_then(Value::as_str),
            draft
                .pointer("/completenessGaps/0/subject")
                .and_then(Value::as_str)
        );
    }

    #[test]
    fn watch_helpers_surface_latest_run_and_next_action() {
        let book = json!({
            "page": {
                "objects": [
                    {"kind": "workflow", "name": "demo"}
                ]
            },
            "book": {
                "pages": [
                    {
                        "id": "perception",
                        "objects": [
                            {"kind": "workflow", "name": "demo"}
                        ]
                    },
                    {
                        "id": "execute",
                        "objects": [
                            {"kind": "workflowRun", "runId": "wfr-latest", "status": "running"}
                        ]
                    }
                ]
            }
        });
        let latest = latest_workflow_run_from_book(&book).unwrap();
        assert_eq!(
            latest.get("runId").and_then(Value::as_str),
            Some("wfr-latest")
        );
        assert!(run_status_needs_monitor("running"));
        assert!(run_status_needs_monitor("started"));
        assert!(!run_status_needs_monitor("completed"));
        assert!(should_monitor_latest_run(
            &json!({"monitorLatest": true}),
            Some("demo"),
            &latest
        ));
        assert!(!should_monitor_latest_run(
            &json!({"monitorLatest": false}),
            Some("demo"),
            &latest
        ));

        let action = watch_next_action(
            &json!({
                "pageId": "execute",
                "pollMs": 750,
                "since": 12,
                "includeEvents": false,
            }),
            "lab",
            Some("demo"),
            44,
        );
        assert_eq!(
            action
                .pointer("/arguments/operation")
                .and_then(Value::as_str),
            Some("watch")
        );
        assert_eq!(
            action
                .pointer("/arguments/workflowName")
                .and_then(Value::as_str),
            Some("demo")
        );
        assert_eq!(
            action.pointer("/arguments/pollMs").and_then(Value::as_u64),
            Some(750)
        );
        assert_eq!(
            action.pointer("/arguments/since").and_then(Value::as_u64),
            Some(44)
        );
        assert_eq!(
            action
                .pointer("/arguments/includeEvents")
                .and_then(Value::as_bool),
            Some(false)
        );
    }

    #[test]
    fn await_helpers_resolve_terminal_context_and_cursor() {
        assert_eq!(monitor_next_since(&json!({"nextSince": "7"})), Some(7));
        assert_eq!(monitor_next_since(&json!({"nextSince": 9})), Some(9));
        assert_eq!(
            normalize_session_operation("await-background"),
            "background"
        );
        assert_eq!(normalize_session_operation("await_async"), "background");

        let job_actions = workflow_authoring_job_actions("job-demo");
        assert_eq!(
            job_actions.pointer("/status/tool").and_then(Value::as_str),
            Some("get_job_status")
        );
        assert_eq!(
            job_actions
                .pointer("/result/arguments/jobId")
                .and_then(Value::as_str),
            Some("job-demo")
        );
        assert_eq!(
            job_actions.pointer("/cancel/tool").and_then(Value::as_str),
            Some("cancel_job")
        );

        let terminal = await_resolution(&json!({
            "latestRun": {"kind": "workflowRun", "runId": "wfr-done", "status": "completed"},
            "watch": {"monitorAttempted": false},
        }))
        .unwrap();
        assert_eq!(
            terminal.get("reason").and_then(Value::as_str),
            Some("terminal_run")
        );
        assert_eq!(
            terminal.get("terminal").and_then(Value::as_bool),
            Some(true)
        );

        let missing_context = await_resolution(&json!({
            "latestRun": {"kind": "workflowRun", "runId": "wfr-active", "status": "running"},
            "watch": {"monitorAttempted": false},
        }))
        .unwrap();
        assert_eq!(
            missing_context.get("reason").and_then(Value::as_str),
            Some("needs_workflow_context")
        );

        let still_active = await_resolution(&json!({
            "latestRun": {"kind": "workflowRun", "runId": "wfr-active", "status": "running"},
            "watch": {"monitorAttempted": true},
            "monitor": {"kind": "workflowRunMonitor", "status": "ok"}
        }));
        assert!(still_active.is_none());
    }
}
