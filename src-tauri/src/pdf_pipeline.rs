use crate::app_runtime::AppHandle;
use crate::{
    model_setup_paths::pdf_pipeline_config_path,
    pdf_pipeline_catalog::normalize_pdf_pipeline_engine_id,
    pdf_pipeline_config::{
        read_pdf_pipeline_config, write_pdf_pipeline_config, PDF_PIPELINE_SCHEMA_VERSION,
    },
    pdf_pipeline_descriptors::pdf_pipeline_preference_options,
    pdf_pipeline_resolution::resolve_pdf_pipeline_effective_engine,
    pdf_runtimes::{local_platform_label, DoclingRuntimeStatus},
    storage::display_path,
};
use serde::{Deserialize, Serialize};

pub(crate) use crate::pdf_pipeline_catalog::{
    PDF_DOCLING_ENGINE_ID, PDF_FAST_TEXT_ENGINE_ID, PDF_PIPELINE_AUTO_ENGINE_ID,
    PDF_PYMUPDF_ENGINE_ID,
};
pub(crate) use crate::pdf_pipeline_descriptors::{
    pdf_pipeline_engine_catalog, PdfIngestionEngineDescriptor, PdfIngestionPreferenceOption,
};

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PdfIngestionPipelineConfigInput {
    pub(crate) preferred_engine_id: String,
}

#[derive(Debug, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PdfIngestionPipelineStatus {
    pub(crate) schema_version: u32,
    pub(crate) preferred_engine_id: String,
    pub(crate) effective_engine_id: String,
    pub(crate) effective_reason: String,
    pub(crate) config_path: String,
    pub(crate) platform: String,
    pub(crate) engines: Vec<PdfIngestionEngineDescriptor>,
    pub(crate) preference_options: Vec<PdfIngestionPreferenceOption>,
    pub(crate) docling_runtime_status: DoclingRuntimeStatus,
}

#[derive(Debug, Clone)]
pub(crate) struct PdfPipelineJobContext {
    pub(crate) preferred_engine_id: String,
    pub(crate) effective_engine_id: String,
    pub(crate) effective_reason: String,
}

pub(crate) fn pdf_ingestion_pipeline_status(
    app: &AppHandle,
    docling_status: DoclingRuntimeStatus,
) -> Result<PdfIngestionPipelineStatus, String> {
    let config = read_pdf_pipeline_config(app)?;
    let preferred_engine_id = normalize_pdf_pipeline_engine_id(&config.preferred_engine_id);
    let engines = pdf_pipeline_engine_catalog(&docling_status);
    let (effective_engine_id, effective_reason) =
        resolve_pdf_pipeline_effective_engine(&preferred_engine_id, &engines);
    Ok(PdfIngestionPipelineStatus {
        schema_version: PDF_PIPELINE_SCHEMA_VERSION,
        preferred_engine_id,
        effective_engine_id,
        effective_reason,
        config_path: display_path(&pdf_pipeline_config_path(app)?),
        platform: local_platform_label(),
        engines,
        preference_options: pdf_pipeline_preference_options(),
        docling_runtime_status: docling_status,
    })
}

pub(crate) fn set_pdf_ingestion_pipeline_config_impl(
    app: &AppHandle,
    input: PdfIngestionPipelineConfigInput,
    docling_status: DoclingRuntimeStatus,
) -> Result<PdfIngestionPipelineStatus, String> {
    write_pdf_pipeline_config(app, &input.preferred_engine_id)?;
    pdf_ingestion_pipeline_status(app, docling_status)
}

pub(crate) fn pdf_pipeline_job_context(
    status: &PdfIngestionPipelineStatus,
) -> PdfPipelineJobContext {
    PdfPipelineJobContext {
        preferred_engine_id: status.preferred_engine_id.clone(),
        effective_engine_id: status.effective_engine_id.clone(),
        effective_reason: status.effective_reason.clone(),
    }
}
