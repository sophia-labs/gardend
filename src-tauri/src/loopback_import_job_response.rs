use crate::{
    local_jobs::{local_job_submit_response, LocalJobRecord},
    loopback_http::loopback_error,
};
use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};

pub(super) fn import_job_submit_response(
    record: &LocalJobRecord,
) -> Result<serde_json::Value, String> {
    let mut value = serde_json::to_value(local_job_submit_response(record))
        .map_err(|error| format!("serialize local import job response: {error}"))?;
    if let Some(object) = value.as_object_mut() {
        object.insert(
            "jobId".to_string(),
            serde_json::Value::String(record.job_id.clone()),
        );
    }
    Ok(value)
}

pub(super) fn import_job_accepted_response(record: &LocalJobRecord) -> Response {
    match import_job_submit_response(record) {
        Ok(value) => (StatusCode::ACCEPTED, Json(value)).into_response(),
        Err(error) => loopback_error(StatusCode::INTERNAL_SERVER_ERROR, &error),
    }
}
