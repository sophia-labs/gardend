use crate::{
    local_jobs::{local_job_status_response, LocalJobRegistry, LocalJobStatus},
    mcp_utils::mcp_required_job_id,
};

pub(super) fn mcp_local_get_job_status(
    jobs: &LocalJobRegistry,
    arguments: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let job_id = mcp_required_job_id(arguments)?;
    let record = jobs
        .get(&job_id)?
        .ok_or_else(|| format!("job not found: {job_id}"))?;
    serde_json::to_value(local_job_status_response(&record)).map_err(|error| error.to_string())
}

pub(super) fn mcp_local_get_job_result(
    jobs: &LocalJobRegistry,
    arguments: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let job_id = mcp_required_job_id(arguments)?;
    let record = jobs
        .get(&job_id)?
        .ok_or_else(|| format!("job not found: {job_id}"))?;
    if let Some(inline) = record.detail.get("result_inline") {
        return Ok(inline.clone());
    }
    if let Some(value) = jobs.read_spilled_result(&record)? {
        return Ok(value);
    }
    if matches!(record.status, LocalJobStatus::Failed) {
        return Err(record
            .error
            .clone()
            .or_else(|| {
                record
                    .detail
                    .get("error")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string)
            })
            .unwrap_or_else(|| format!("job {job_id} failed")));
    }
    Err(format!("result not available for job {job_id}"))
}

pub(super) fn mcp_local_cancel_job(
    jobs: &LocalJobRegistry,
    arguments: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let job_id = mcp_required_job_id(arguments)?;
    let response = jobs
        .cancel(&job_id)?
        .ok_or_else(|| format!("job not found: {job_id}"))?;
    serde_json::to_value(response).map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use uuid::Uuid;

    #[test]
    fn mcp_local_cancel_job_accepts_hosted_alias_and_returns_cancel_envelope() {
        let jobs_dir = std::env::temp_dir().join(format!("sophia-mcp-job-{}", Uuid::new_v4()));
        let jobs = LocalJobRegistry::new(jobs_dir.clone()).expect("job registry");
        let record = jobs
            .insert_queued("test", Some("graph-a".to_string()), serde_json::json!({}))
            .expect("queued job");

        let response = mcp_local_cancel_job(
            &jobs,
            &serde_json::json!({
                "jobId": record.job_id,
            }),
        )
        .expect("cancel response");

        assert_eq!(response.get("cancelled"), Some(&serde_json::json!(true)));
        assert_eq!(
            response.get("previous_status"),
            Some(&serde_json::json!("queued"))
        );

        let _ = fs::remove_dir_all(jobs_dir);
    }

    #[test]
    fn mcp_local_get_job_status_and_result_return_finished_payload() {
        let jobs_dir = std::env::temp_dir().join(format!("sophia-mcp-job-{}", Uuid::new_v4()));
        let jobs = LocalJobRegistry::new(jobs_dir.clone()).expect("job registry");
        let record = jobs
            .insert_finished(
                "test",
                Some("graph-a".to_string()),
                1,
                Ok(serde_json::json!({ "ok": true })),
                "application/json",
            )
            .expect("finished job");

        let status = mcp_local_get_job_status(
            &jobs,
            &serde_json::json!({
                "job_id": record.job_id.clone(),
            }),
        )
        .expect("job status");
        assert_eq!(status.get("status"), Some(&serde_json::json!("succeeded")));

        let result = mcp_local_get_job_result(
            &jobs,
            &serde_json::json!({
                "jobId": record.job_id,
            }),
        )
        .expect("job result");
        assert_eq!(result, serde_json::json!({ "ok": true }));

        let _ = fs::remove_dir_all(jobs_dir);
    }
}
