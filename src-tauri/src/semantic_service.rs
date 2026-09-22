use crate::app_runtime::AppHandle;
pub(super) use crate::semantic_embedder::semantic_model_status;
pub(super) use crate::semantic_index_refresh::{
    submit_semantic_index_refresh_job, RefreshSemanticIndexInput,
};
pub(super) use crate::semantic_index_status::remove_document_from_semantic_index;
pub(super) use crate::semantic_reasoner::{semantic_reason, SemanticReasonInput};
pub(super) use crate::semantic_search_service::{semantic_search, SemanticSearchInput};
use crate::{
    app_error::{AppError, AppResult},
    local_jobs::{
        local_job_status_response, LocalJobCancelResponse, LocalJobRegistry,
        LocalJobStatusResponse, LocalJobSubmitResponse,
    },
    paths::{existing_graph_dir, jobs_dir},
    profile_service::ensure_profile,
    semantic_embedder::{ensure_semantic_embedder, semantic_model_catalog},
    semantic_index::SemanticIndexStatus,
    semantic_index_status::semantic_index_status,
    semantic_model_prepare_jobs::submit_prepare_semantic_model_job,
    semantic_models::{
        read_semantic_model_config, semantic_model_spec_by_id, write_semantic_model_config,
        write_semantic_model_setup_manifest, SemanticModelConfigInput, SemanticModelDescriptor,
        SemanticModelStatus,
    },
};
use std::sync::Arc;

#[cfg_attr(feature = "desktop", tauri::command)]
pub(super) fn get_semantic_index_status(
    app: AppHandle,
    graph_id: String,
) -> Result<SemanticIndexStatus, String> {
    get_semantic_index_status_service(&app, graph_id).map_err(AppError::message)
}

pub(super) fn get_semantic_index_status_service(
    app: &AppHandle,
    graph_id: String,
) -> AppResult<SemanticIndexStatus> {
    let graph_dir = existing_graph_dir(app, &graph_id).map_err(AppError::storage)?;
    semantic_index_status(app, &graph_dir, &graph_id).map_err(AppError::storage)
}

#[cfg_attr(feature = "desktop", tauri::command)]
pub(super) fn get_semantic_model_status(app: AppHandle) -> Result<SemanticModelStatus, String> {
    get_semantic_model_status_service(&app).map_err(AppError::message)
}

pub(super) fn get_semantic_model_status_service(app: &AppHandle) -> AppResult<SemanticModelStatus> {
    ensure_profile(app).map_err(AppError::storage)?;
    semantic_model_status(app)
}

#[cfg_attr(feature = "desktop", tauri::command)]
pub(super) fn list_semantic_models(app: AppHandle) -> Result<Vec<SemanticModelDescriptor>, String> {
    list_semantic_models_service(&app).map_err(AppError::message)
}

pub(super) fn list_semantic_models_service(
    app: &AppHandle,
) -> AppResult<Vec<SemanticModelDescriptor>> {
    ensure_profile(app).map_err(AppError::storage)?;
    semantic_model_catalog(app)
}

#[cfg_attr(feature = "desktop", tauri::command)]
pub(super) fn set_semantic_model_config(
    app: AppHandle,
    input: SemanticModelConfigInput,
) -> Result<SemanticModelStatus, String> {
    set_semantic_model_config_service(&app, input).map_err(AppError::message)
}

pub(super) fn set_semantic_model_config_service(
    app: &AppHandle,
    input: SemanticModelConfigInput,
) -> AppResult<SemanticModelStatus> {
    ensure_profile(app).map_err(AppError::storage)?;
    write_semantic_model_config(app, &input.model_id, input.batch_size)
        .map_err(AppError::storage)?;
    semantic_model_status(app)
}

// Legacy synchronous prepare command. Blocks the Tauri runtime for the full
// model load (potentially minutes). Retained for tests/scripts that don't have
// a job-poll loop. New UI surfaces should use `prepare_semantic_model_job`.
#[cfg_attr(feature = "desktop", tauri::command)]
pub(super) fn prepare_semantic_model(app: AppHandle) -> Result<SemanticModelStatus, String> {
    prepare_semantic_model_service(&app).map_err(AppError::message)
}

pub(super) fn prepare_semantic_model_service(app: &AppHandle) -> AppResult<SemanticModelStatus> {
    ensure_profile(app).map_err(AppError::storage)?;
    ensure_semantic_embedder(app, true)?;
    let config = read_semantic_model_config(app).map_err(AppError::storage)?;
    let spec =
        semantic_model_spec_by_id(&config.selected_model_id).map_err(AppError::validation)?;
    write_semantic_model_setup_manifest(app, &spec).map_err(AppError::storage)?;
    semantic_model_status(app)
}

#[cfg_attr(feature = "desktop", tauri::command)]
pub(super) fn prepare_semantic_model_job(app: AppHandle) -> Result<LocalJobSubmitResponse, String> {
    prepare_semantic_model_job_service(app).map_err(AppError::message)
}

pub(super) fn prepare_semantic_model_job_service(
    app: AppHandle,
) -> AppResult<LocalJobSubmitResponse> {
    ensure_profile(&app).map_err(AppError::storage)?;
    let jobs = Arc::new(LocalJobRegistry::new(
        jobs_dir(&app).map_err(AppError::storage)?,
    )?);
    submit_prepare_semantic_model_job(app, jobs)
}

#[cfg_attr(feature = "desktop", tauri::command)]
pub(super) fn get_prepare_semantic_model_job(
    app: AppHandle,
    job_id: String,
) -> Result<LocalJobStatusResponse, String> {
    get_prepare_semantic_model_job_service(&app, job_id).map_err(AppError::message)
}

pub(super) fn get_prepare_semantic_model_job_service(
    app: &AppHandle,
    job_id: String,
) -> AppResult<LocalJobStatusResponse> {
    ensure_profile(app).map_err(AppError::storage)?;
    let jobs = LocalJobRegistry::new(jobs_dir(app).map_err(AppError::storage)?)?;
    let record = jobs
        .get_fresh(&job_id)?
        .ok_or_else(|| AppError::not_found("semantic model prepare job not found"))?;
    Ok(local_job_status_response(&record))
}

#[cfg_attr(feature = "desktop", tauri::command)]
pub(super) fn cancel_prepare_semantic_model_job(
    app: AppHandle,
    job_id: String,
) -> Result<LocalJobCancelResponse, String> {
    cancel_prepare_semantic_model_job_service(&app, job_id).map_err(AppError::message)
}

pub(super) fn cancel_prepare_semantic_model_job_service(
    app: &AppHandle,
    job_id: String,
) -> AppResult<LocalJobCancelResponse> {
    ensure_profile(app).map_err(AppError::storage)?;
    let jobs = LocalJobRegistry::new(jobs_dir(app).map_err(AppError::storage)?)?;
    jobs.cancel(&job_id)?
        .ok_or_else(|| AppError::not_found("semantic model prepare job not found"))
}

#[cfg_attr(feature = "desktop", tauri::command)]
pub(super) fn start_semantic_index_refresh(
    app: AppHandle,
    input: RefreshSemanticIndexInput,
) -> Result<LocalJobSubmitResponse, String> {
    start_semantic_index_refresh_service(app, input).map_err(AppError::message)
}

pub(super) fn start_semantic_index_refresh_service(
    app: AppHandle,
    input: RefreshSemanticIndexInput,
) -> AppResult<LocalJobSubmitResponse> {
    ensure_profile(&app).map_err(AppError::storage)?;
    let jobs = Arc::new(LocalJobRegistry::new(
        jobs_dir(&app).map_err(AppError::storage)?,
    )?);
    submit_semantic_index_refresh_job(app, jobs, input).map_err(AppError::internal)
}

#[cfg_attr(feature = "desktop", tauri::command)]
pub(super) fn get_semantic_index_refresh_job(
    app: AppHandle,
    job_id: String,
) -> Result<LocalJobStatusResponse, String> {
    get_semantic_index_refresh_job_service(&app, job_id).map_err(AppError::message)
}

pub(super) fn get_semantic_index_refresh_job_service(
    app: &AppHandle,
    job_id: String,
) -> AppResult<LocalJobStatusResponse> {
    ensure_profile(app).map_err(AppError::storage)?;
    let jobs = LocalJobRegistry::new(jobs_dir(app).map_err(AppError::storage)?)?;
    let record = jobs
        .get_fresh(&job_id)?
        .ok_or_else(|| AppError::not_found("semantic index refresh job not found"))?;
    Ok(local_job_status_response(&record))
}

#[cfg_attr(feature = "desktop", tauri::command)]
pub(super) fn cancel_semantic_index_refresh_job(
    app: AppHandle,
    job_id: String,
) -> Result<LocalJobCancelResponse, String> {
    cancel_semantic_index_refresh_job_service(&app, job_id).map_err(AppError::message)
}

pub(super) fn cancel_semantic_index_refresh_job_service(
    app: &AppHandle,
    job_id: String,
) -> AppResult<LocalJobCancelResponse> {
    ensure_profile(app).map_err(AppError::storage)?;
    let jobs = LocalJobRegistry::new(jobs_dir(app).map_err(AppError::storage)?)?;
    jobs.cancel(&job_id)?
        .ok_or_else(|| AppError::not_found("semantic index refresh job not found"))
}

#[cfg_attr(feature = "desktop", tauri::command)]
pub(super) fn refresh_semantic_index(
    app: AppHandle,
    input: RefreshSemanticIndexInput,
) -> Result<SemanticIndexStatus, String> {
    crate::semantic_index_refresh::refresh_semantic_index_with_progress(
        &app,
        &input.graph_id,
        input.flush_boundary,
        None,
    )
}
