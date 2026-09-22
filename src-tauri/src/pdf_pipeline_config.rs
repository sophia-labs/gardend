use crate::app_runtime::AppHandle;
use crate::{
    clock::timestamp,
    model_setup_paths::{pdf_pipeline_config_path, pdf_pipeline_setup_dir},
    pdf_pipeline_catalog::{
        normalize_pdf_pipeline_engine_id, validate_pdf_pipeline_engine_id,
        PDF_PIPELINE_AUTO_ENGINE_ID,
    },
    storage::{create_dir_all, read_json, write_json},
};
use serde::{Deserialize, Serialize};

pub(super) const PDF_PIPELINE_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub(super) struct PdfIngestionPipelineConfigFile {
    pub(super) schema_version: u32,
    pub(super) preferred_engine_id: String,
    pub(super) updated_at: String,
}

pub(super) fn read_pdf_pipeline_config(
    app: &AppHandle,
) -> Result<PdfIngestionPipelineConfigFile, String> {
    let path = pdf_pipeline_config_path(app)?;
    if !path.is_file() {
        return Ok(PdfIngestionPipelineConfigFile {
            schema_version: PDF_PIPELINE_SCHEMA_VERSION,
            preferred_engine_id: PDF_PIPELINE_AUTO_ENGINE_ID.to_string(),
            updated_at: timestamp(),
        });
    }
    let mut config = read_json::<PdfIngestionPipelineConfigFile>(&path)?;
    config.preferred_engine_id = normalize_pdf_pipeline_engine_id(&config.preferred_engine_id);
    if config.schema_version != PDF_PIPELINE_SCHEMA_VERSION
        || validate_pdf_pipeline_engine_id(&config.preferred_engine_id).is_err()
    {
        config.schema_version = PDF_PIPELINE_SCHEMA_VERSION;
        config.preferred_engine_id = PDF_PIPELINE_AUTO_ENGINE_ID.to_string();
    }
    Ok(config)
}

pub(super) fn write_pdf_pipeline_config(
    app: &AppHandle,
    preferred_engine_id: &str,
) -> Result<PdfIngestionPipelineConfigFile, String> {
    let preferred_engine_id = normalize_pdf_pipeline_engine_id(preferred_engine_id);
    validate_pdf_pipeline_engine_id(&preferred_engine_id)?;
    let setup_dir = pdf_pipeline_setup_dir(app)?;
    create_dir_all(&setup_dir)?;
    let config = PdfIngestionPipelineConfigFile {
        schema_version: PDF_PIPELINE_SCHEMA_VERSION,
        preferred_engine_id,
        updated_at: timestamp(),
    };
    write_json(&pdf_pipeline_config_path(app)?, &config)?;
    Ok(config)
}
