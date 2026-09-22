use crate::{
    local_job_types::{local_job_links, LocalJobRecord, LocalJobStatus},
    runtime_config::LOCAL_JOB_INLINE_RESULT_MAX_BYTES,
    storage::{display_path, read_json, write_bytes},
};
use std::path::Path;

pub(crate) struct LocalJobResultMaterialization {
    pub(crate) status: LocalJobStatus,
    pub(crate) error: Option<String>,
}

pub(crate) fn apply_job_result_to_detail(
    detail: &mut serde_json::Value,
    job_id: &str,
    result_path: &Path,
    result: Result<serde_json::Value, String>,
    result_mime_type: &str,
) -> Result<LocalJobResultMaterialization, String> {
    match result {
        Ok(value) => {
            let result_bytes = serde_json::to_vec(&value)
                .map_err(|error| format!("serialize job result: {error}"))?;
            detail["result_ready"] = serde_json::json!(true);
            detail["result_mime_type"] = serde_json::json!(result_mime_type);
            detail["result_bytes"] = serde_json::json!(result_bytes.len());
            detail["links"] = serde_json::to_value(local_job_links(job_id))
                .map_err(|error| format!("serialize local job links: {error}"))?;
            if result_bytes.len() <= LOCAL_JOB_INLINE_RESULT_MAX_BYTES {
                detail["result_inline"] = value;
                detail.as_object_mut().map(|object| {
                    object.remove("result_location");
                    object.remove("result_path");
                    object.remove("result_truncated");
                });
            } else {
                write_bytes(result_path, &result_bytes).map_err(|error| {
                    format!(
                        "write local job result {}: {error}",
                        display_path(result_path)
                    )
                })?;
                detail["result_location"] =
                    serde_json::json!(format!("local://jobs/{job_id}/result"));
                detail["result_path"] = serde_json::json!(display_path(result_path));
                detail["result_truncated"] = serde_json::json!(false);
            }
            Ok(LocalJobResultMaterialization {
                status: LocalJobStatus::Succeeded,
                error: None,
            })
        }
        Err(message) => {
            detail["error_code"] = serde_json::json!("TASK_FAILED");
            detail["message"] = serde_json::json!(message);
            detail["result_ready"] = serde_json::json!(false);
            let error = detail
                .get("message")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("job failed")
                .to_string();
            Ok(LocalJobResultMaterialization {
                status: LocalJobStatus::Failed,
                error: Some(error),
            })
        }
    }
}

pub(crate) fn read_spilled_job_result(
    record: &LocalJobRecord,
    result_path: &Path,
) -> Result<Option<serde_json::Value>, String> {
    let result_location = record
        .detail
        .get("result_location")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    if !result_location.starts_with("local://jobs/") {
        return Ok(None);
    }

    if !result_path.is_file() {
        return Ok(None);
    }
    read_json::<serde_json::Value>(result_path)
        .map(Some)
        .map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_result_path(name: &str) -> std::path::PathBuf {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        std::env::temp_dir()
            .join(format!("mnemosyne-job-result-{name}-{suffix}"))
            .join("result.json")
    }

    #[test]
    fn large_job_results_spill_to_result_file() {
        let result_path = temp_result_path("spill");
        let mut detail = serde_json::json!({ "type": "test_job" });
        let payload = "x".repeat(LOCAL_JOB_INLINE_RESULT_MAX_BYTES + 1);

        let materialized = apply_job_result_to_detail(
            &mut detail,
            "job-test",
            &result_path,
            Ok(serde_json::json!({ "payload": payload })),
            "application/json",
        )
        .expect("materialize result");

        assert!(matches!(materialized.status, LocalJobStatus::Succeeded));
        assert!(detail.get("result_inline").is_none());
        assert_eq!(detail["result_location"], "local://jobs/job-test/result");
        assert!(result_path.is_file());
        let _ = std::fs::remove_dir_all(result_path.parent().unwrap());
    }
}
