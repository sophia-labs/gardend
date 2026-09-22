use crate::app_runtime::AppHandle;
use crate::{
    ingestion_approaches::{ingestion_approach_catalog, IngestionApproachDescriptor},
    pdf_parsers::prepare_docling_runtime_impl,
    pdf_pipeline::{
        pdf_ingestion_pipeline_status as pdf_ingestion_pipeline_status_with_docling,
        set_pdf_ingestion_pipeline_config_impl as set_pdf_ingestion_pipeline_config_with_docling,
        PdfIngestionPipelineConfigInput, PdfIngestionPipelineStatus,
    },
    pdf_runtimes::{docling_runtime_status, DoclingRuntimeStatus},
    profile_service::ensure_profile,
};

#[cfg_attr(feature = "desktop", tauri::command)]
pub(crate) fn get_docling_runtime_status(app: AppHandle) -> Result<DoclingRuntimeStatus, String> {
    ensure_profile(&app)?;
    docling_runtime_status(&app)
}

#[cfg_attr(feature = "desktop", tauri::command)]
pub(crate) fn get_pdf_ingestion_pipeline_status(
    app: AppHandle,
) -> Result<PdfIngestionPipelineStatus, String> {
    ensure_profile(&app)?;
    pdf_ingestion_pipeline_status(&app)
}

#[cfg_attr(feature = "desktop", tauri::command)]
pub(crate) fn set_pdf_ingestion_pipeline_config(
    app: AppHandle,
    input: PdfIngestionPipelineConfigInput,
) -> Result<PdfIngestionPipelineStatus, String> {
    ensure_profile(&app)?;
    set_pdf_ingestion_pipeline_config_impl(&app, input)
}

#[cfg_attr(feature = "desktop", tauri::command)]
pub(crate) fn list_ingestion_approaches(
    app: AppHandle,
) -> Result<Vec<IngestionApproachDescriptor>, String> {
    ensure_profile(&app)?;
    let docling_status = docling_runtime_status(&app)?;
    Ok(ingestion_approach_catalog(Some(&docling_status)))
}

#[cfg_attr(feature = "desktop", tauri::command)]
pub(crate) fn prepare_docling_runtime(app: AppHandle) -> Result<DoclingRuntimeStatus, String> {
    ensure_profile(&app)?;
    prepare_docling_runtime_impl(&app)
}

pub(crate) fn pdf_ingestion_pipeline_status(
    app: &AppHandle,
) -> Result<PdfIngestionPipelineStatus, String> {
    let docling_status = docling_runtime_status(app)?;
    pdf_ingestion_pipeline_status_with_docling(app, docling_status)
}

fn set_pdf_ingestion_pipeline_config_impl(
    app: &AppHandle,
    input: PdfIngestionPipelineConfigInput,
) -> Result<PdfIngestionPipelineStatus, String> {
    let docling_status = docling_runtime_status(app)?;
    set_pdf_ingestion_pipeline_config_with_docling(app, input, docling_status)
}
