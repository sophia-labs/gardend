use crate::{
    local_jobs::{LocalJobRecord, LocalJobRegistry, LocalJobStatus},
    loopback_http::{loopback_app_error, loopback_error},
};
use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};

pub(crate) fn local_job_result_response(
    jobs: &LocalJobRegistry,
    record: &LocalJobRecord,
) -> Response {
    if let Some(inline) = record.detail.get("result_inline") {
        return Json(inline.clone()).into_response();
    }
    match jobs.read_spilled_result(record) {
        Ok(Some(value)) => return Json(value).into_response(),
        Ok(None) => {}
        Err(error) => return loopback_app_error(error),
    }
    if matches!(record.status, LocalJobStatus::Failed) {
        return (StatusCode::BAD_REQUEST, Json(record.detail.clone())).into_response();
    }
    loopback_error(StatusCode::NOT_FOUND, "result not available")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        path::PathBuf,
        time::{SystemTime, UNIX_EPOCH},
    };

    fn temp_jobs_dir(name: &str) -> PathBuf {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        std::env::temp_dir().join(format!("mnemosyne-loopback-jobs-{name}-{suffix}"))
    }

    #[test]
    fn job_result_response_returns_inline_payload() {
        let dir = temp_jobs_dir("inline-response");
        let registry = LocalJobRegistry::new(dir.clone()).expect("create registry");
        let record = LocalJobRecord {
            job_id: "job-test".to_string(),
            status: LocalJobStatus::Succeeded,
            updated_at: "1".to_string(),
            user_id: None,
            owner_principal: None,
            graph_generation: None,
            initiator_principal: None,
            submitted_role: None,
            policy_revision: None,
            submitted_at: "1".to_string(),
            started_at: "1".to_string(),
            completed_at: "1".to_string(),
            processing_time_ms: 0,
            detail: serde_json::json!({
                "result_inline": {
                    "ok": true
                }
            }),
            progress: None,
            error: None,
        };

        let response = local_job_result_response(&registry, &record);

        assert_eq!(response.status(), StatusCode::OK);
        let _ = fs::remove_dir_all(dir);
    }
}
