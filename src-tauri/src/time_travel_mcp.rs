use crate::app_runtime::AppHandle;
use crate::{
    app_error::{AppError, AppResult},
    local_jobs::LocalJobRegistry,
    mcp_utils::{mcp_arg_bool, mcp_arg_string, mcp_arg_usize, mcp_graph_id_or_default},
    time_travel_restore_service::{cancel_restore_operation, get_restore_operation, start_restore},
    time_travel_service::{
        capture_restore_point, delete_restore_point_response, diff_restore_points_response,
        get_restore_point_response, list_restore_points_response,
    },
    time_travel_types::RestorePointTrigger,
};
use serde_json::Value;
use std::sync::Arc;

fn required_restore_point_id(arguments: &Value) -> AppResult<String> {
    mcp_arg_string(arguments, &["restore_point_id", "restorePointId"])
        .ok_or_else(|| AppError::validation("restore_point_id is required"))
}

pub(super) fn mcp_local_list_restore_points(app: AppHandle, arguments: &Value) -> AppResult<Value> {
    let graph_id = mcp_graph_id_or_default(&app, arguments).map_err(AppError::validation)?;
    let cursor = mcp_arg_string(arguments, &["cursor"]);
    let limit = if arguments.get("limit").is_some() {
        Some(mcp_arg_usize(arguments, &["limit"], 50))
    } else {
        None
    };
    list_restore_points_response(&app, &graph_id, cursor.as_deref(), limit)
}

pub(super) fn mcp_local_get_restore_point(app: AppHandle, arguments: &Value) -> AppResult<Value> {
    let graph_id = mcp_graph_id_or_default(&app, arguments).map_err(AppError::validation)?;
    let restore_point_id = required_restore_point_id(arguments)?;
    get_restore_point_response(&app, &graph_id, &restore_point_id)
}

pub(super) fn mcp_local_create_restore_point(
    app: AppHandle,
    arguments: &Value,
) -> AppResult<Value> {
    let graph_id = mcp_graph_id_or_default(&app, arguments).map_err(AppError::validation)?;
    let label = mcp_arg_string(arguments, &["label"])
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    let trigger = match mcp_arg_string(arguments, &["trigger"]).as_deref() {
        Some("interval") => RestorePointTrigger::Interval,
        Some("checkpoint") => RestorePointTrigger::Checkpoint,
        _ => RestorePointTrigger::Manual,
    };
    capture_restore_point(&app, &graph_id, trigger, label)
}

pub(super) fn mcp_local_diff_restore_points(app: AppHandle, arguments: &Value) -> AppResult<Value> {
    let graph_id = mcp_graph_id_or_default(&app, arguments).map_err(AppError::validation)?;
    let restore_point_id = required_restore_point_id(arguments)?;
    let against = mcp_arg_string(arguments, &["against"]);
    diff_restore_points_response(&app, &graph_id, &restore_point_id, against.as_deref())
}

pub(super) fn mcp_local_delete_restore_point(
    app: AppHandle,
    arguments: &Value,
) -> AppResult<Value> {
    let graph_id = mcp_graph_id_or_default(&app, arguments).map_err(AppError::validation)?;
    let restore_point_id = required_restore_point_id(arguments)?;
    delete_restore_point_response(&app, &graph_id, &restore_point_id)
}

pub(super) fn mcp_local_restore_to_restore_point(
    app: AppHandle,
    jobs: Arc<LocalJobRegistry>,
    arguments: &Value,
) -> AppResult<Value> {
    let graph_id = mcp_graph_id_or_default(&app, arguments).map_err(AppError::validation)?;
    let restore_point_id = required_restore_point_id(arguments)?;
    let dry_run = mcp_arg_bool(arguments, &["dry_run", "dryRun"], false);
    start_restore(&app, jobs, &graph_id, &restore_point_id, dry_run)
}

pub(super) fn mcp_local_get_restore_operation(
    jobs: &LocalJobRegistry,
    arguments: &Value,
) -> AppResult<Value> {
    let operation_id = mcp_arg_string(arguments, &["operation_id", "operationId"])
        .ok_or_else(|| AppError::validation("operation_id is required"))?;
    get_restore_operation(jobs, &operation_id)
}

pub(super) fn mcp_local_cancel_restore_operation(
    jobs: &LocalJobRegistry,
    arguments: &Value,
) -> AppResult<Value> {
    let operation_id = mcp_arg_string(arguments, &["operation_id", "operationId"])
        .ok_or_else(|| AppError::validation("operation_id is required"))?;
    cancel_restore_operation(jobs, &operation_id)
}
