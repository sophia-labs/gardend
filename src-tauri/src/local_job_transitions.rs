use crate::{
    clock::{epoch_millis, timestamp},
    local_job_results::apply_job_result_to_detail,
    local_job_types::{
        local_job_status_label, LocalJobCancelResponse, LocalJobProgress, LocalJobRecord,
        LocalJobStatus,
    },
};
use std::path::Path;

pub(crate) fn mark_record_running(record: &mut LocalJobRecord) -> bool {
    if !matches!(record.status, LocalJobStatus::Queued) {
        return false;
    }
    let now = epoch_millis().to_string();
    record.status = LocalJobStatus::Running;
    record.updated_at = now.clone();
    record.started_at = now;
    true
}

pub(crate) fn finish_record(
    record: &mut LocalJobRecord,
    result_path: &Path,
    result: Result<serde_json::Value, String>,
    result_mime_type: &str,
) -> Result<bool, String> {
    if matches!(record.status, LocalJobStatus::Cancelled) {
        return Ok(false);
    }
    let completed_at_ms = epoch_millis();
    let completed_at = completed_at_ms.to_string();
    if record.started_at.is_empty() {
        record.started_at = record.submitted_at.clone();
    }
    let started_at_ms = record.started_at.parse::<u128>().unwrap_or(completed_at_ms);
    let materialized = apply_job_result_to_detail(
        &mut record.detail,
        &record.job_id,
        result_path,
        result,
        result_mime_type,
    )?;
    record.status = materialized.status;
    record.error = materialized.error;
    record.updated_at = completed_at.clone();
    record.completed_at = completed_at;
    record.processing_time_ms = completed_at_ms.saturating_sub(started_at_ms);
    Ok(true)
}

pub(crate) fn apply_record_progress(
    record: &mut LocalJobRecord,
    mut progress: LocalJobProgress,
) -> Result<bool, String> {
    if matches!(
        record.status,
        LocalJobStatus::Succeeded | LocalJobStatus::Failed | LocalJobStatus::Cancelled
    ) {
        return Ok(false);
    }
    if progress.updated_at.trim().is_empty() {
        progress.updated_at = timestamp();
    }
    progress.percent = progress.percent.clamp(0.0, 100.0);
    record.updated_at = progress.updated_at.clone();
    record.progress = Some(progress.clone());
    record.detail["progress"] = serde_json::to_value(&progress)
        .map_err(|error| format!("serialize job progress: {error}"))?;
    Ok(true)
}

pub(crate) fn cancel_record(record: &mut LocalJobRecord) -> Result<LocalJobCancelResponse, String> {
    let previous_status = record.status.clone();
    let cancelled = matches!(
        previous_status,
        LocalJobStatus::Queued | LocalJobStatus::Running
    );

    if cancelled {
        let now_ms = epoch_millis();
        let now = now_ms.to_string();
        if record.started_at.is_empty() {
            record.started_at = record.submitted_at.clone();
        }
        let started_at_ms = record.started_at.parse::<u128>().unwrap_or(now_ms);
        record.status = LocalJobStatus::Cancelled;
        record.updated_at = now.clone();
        record.completed_at = now;
        record.processing_time_ms = now_ms.saturating_sub(started_at_ms);
        record.detail["reason"] = serde_json::json!("user_cancelled");
        record.detail["message"] = serde_json::json!("Job cancelled");
        record.detail["result_ready"] = serde_json::json!(false);
        let progress = LocalJobProgress {
            phase: "cancelled".to_string(),
            message: "Job cancelled".to_string(),
            current: 0,
            total: 0,
            percent: 100.0,
            updated_at: record.updated_at.clone(),
            details: serde_json::json!({ "reason": "user_cancelled" }),
        };
        record.progress = Some(progress.clone());
        record.detail["progress"] = serde_json::to_value(&progress)
            .map_err(|error| format!("serialize job progress: {error}"))?;
        record.error = None;
    }

    let message = if cancelled {
        match previous_status {
            LocalJobStatus::Running => {
                "Cancellation requested; the running job will stop at the next checkpoint"
                    .to_string()
            }
            _ => "Job cancelled".to_string(),
        }
    } else {
        format!(
            "Cannot cancel job in {} state",
            local_job_status_label(&previous_status)
        )
    };

    Ok(LocalJobCancelResponse {
        job_id: record.job_id.clone(),
        cancelled,
        previous_status,
        message,
    })
}
