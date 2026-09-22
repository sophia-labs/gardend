use crate::pdf_pipeline::PdfPipelineJobContext;
use std::path::PathBuf;

#[derive(Clone)]
pub(crate) struct DoclingPdfJobInput {
    pub(crate) graph_id: String,
    pub(crate) document_id: String,
    pub(crate) filename: String,
    pub(crate) mime_type: String,
    pub(crate) size_bytes: usize,
    pub(crate) pending_original_path: PathBuf,
    pub(crate) parent_id: Option<String>,
    pub(crate) title: Option<String>,
    pub(crate) source_storage_key: String,
    pub(crate) pipeline_context: PdfPipelineJobContext,
}

#[derive(Clone)]
pub(crate) struct Pymupdf4llmPdfJobInput {
    pub(crate) graph_id: String,
    pub(crate) document_id: String,
    pub(crate) filename: String,
    pub(crate) mime_type: String,
    pub(crate) size_bytes: usize,
    pub(crate) pending_original_path: PathBuf,
    pub(crate) parent_id: Option<String>,
    pub(crate) title: Option<String>,
    pub(crate) source_storage_key: String,
    pub(crate) pipeline_context: PdfPipelineJobContext,
}
