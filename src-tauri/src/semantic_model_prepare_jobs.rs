use crate::app_runtime::AppHandle;
use crate::{
    app_error::{AppError, AppResult},
    clock::timestamp,
    local_jobs::{
        local_job_submit_response, LocalJobProgress, LocalJobRegistry, LocalJobStatus,
        LocalJobSubmitResponse,
    },
    runtime_config::SEMANTIC_MODEL_PREPARE_JOB_TYPE,
    semantic_embedder::{ensure_semantic_embedder, semantic_model_status},
    semantic_models::{
        read_semantic_model_config, semantic_model_spec_by_id, write_semantic_model_setup_manifest,
    },
};
use std::sync::Arc;

pub(super) fn submit_prepare_semantic_model_job(
    app: AppHandle,
    jobs: Arc<LocalJobRegistry>,
) -> AppResult<LocalJobSubmitResponse> {
    let config = read_semantic_model_config(&app).map_err(AppError::storage)?;
    let spec =
        semantic_model_spec_by_id(&config.selected_model_id).map_err(AppError::validation)?;
    let detail = serde_json::json!({
        "provider_id": spec.provider_id,
        "providerId": spec.provider_id,
        "model_id": spec.model_id,
        "modelId": spec.model_id,
        "display_name": spec.display_name,
        "displayName": spec.display_name,
        "dimensions": spec.dimensions,
    });
    let record = jobs.insert_queued(SEMANTIC_MODEL_PREPARE_JOB_TYPE, None, detail)?;
    let job_id = record.job_id.clone();
    let queued_progress = LocalJobProgress {
        phase: "queued".to_string(),
        message: format!("Preparing local embedding model {}", spec.display_name),
        current: 0,
        total: 1,
        percent: 0.0,
        updated_at: timestamp(),
        details: serde_json::json!({
            "modelId": spec.model_id,
            "displayName": spec.display_name,
        }),
    };
    let _ = jobs.update_progress(&job_id, queued_progress);
    let record = jobs.get(&job_id)?.unwrap_or(record);
    spawn_prepare_semantic_model_job(app, jobs, job_id);
    Ok(local_job_submit_response(&record))
}

fn spawn_prepare_semantic_model_job(app: AppHandle, jobs: Arc<LocalJobRegistry>, job_id: String) {
    crate::app_runtime::async_runtime::spawn(async move {
        if jobs.is_cancelled(&job_id).unwrap_or(false) {
            return;
        }
        match jobs.mark_running(&job_id) {
            Ok(Some(record)) if matches!(record.status, LocalJobStatus::Running) => {}
            Ok(Some(_)) | Ok(None) => return,
            Err(error) => {
                let _ = jobs.finish_existing(&job_id, Err(error.into()), "application/json");
                return;
            }
        }

        report_prepare_progress(
            &jobs,
            &job_id,
            "loading",
            "Loading local embedding model",
            0,
            1,
            serde_json::json!({}),
        );

        let load_app = app.clone();
        let load_result =
            crate::app_runtime::async_runtime::spawn_blocking(move || ensure_semantic_embedder(&load_app, true))
                .await;
        let load_outcome: AppResult<()> = match load_result {
            Ok(inner) => inner,
            Err(error) => Err(AppError::internal(format!(
                "semantic model prepare task failed: {error}"
            ))),
        };

        if jobs.is_cancelled(&job_id).unwrap_or(false) {
            return;
        }

        let result: Result<serde_json::Value, String> = (|| -> AppResult<serde_json::Value> {
            load_outcome?;
            let config = read_semantic_model_config(&app).map_err(AppError::storage)?;
            let spec = semantic_model_spec_by_id(&config.selected_model_id)
                .map_err(AppError::validation)?;
            write_semantic_model_setup_manifest(&app, &spec).map_err(AppError::storage)?;
            let status = semantic_model_status(&app)?;
            serde_json::to_value(status).map_err(|error| {
                AppError::serialization(format!("serialize semantic model status: {error}"))
            })
        })()
        .map_err(AppError::message);

        if let Ok(value) = &result {
            report_prepare_progress(
                &jobs,
                &job_id,
                "complete",
                "Local embedding model ready",
                1,
                1,
                serde_json::json!({ "result": value }),
            );
        }

        if let Err(error) = jobs.finish_existing(&job_id, result, "application/json") {
            log::error!("Failed to finish semantic model prepare job {job_id}: {error}");
        }
    });
}

fn report_prepare_progress(
    jobs: &Arc<LocalJobRegistry>,
    job_id: &str,
    phase: &str,
    message: &str,
    current: usize,
    total: usize,
    details: serde_json::Value,
) {
    let total = total.max(current).max(1);
    let percent = if phase == "complete" {
        100.0
    } else {
        (current as f64 / total as f64) * 100.0
    };
    let progress = LocalJobProgress {
        phase: phase.to_string(),
        message: message.to_string(),
        current,
        total,
        percent,
        updated_at: timestamp(),
        details,
    };
    if let Err(error) = jobs.update_progress(job_id, progress) {
        log::warn!("Failed to update prepare semantic model progress for {job_id}: {error}");
    }
}
