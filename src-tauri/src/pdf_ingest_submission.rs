use crate::{
    ids::safe_filename,
    local_crdt_jobs::{insert_and_spawn_crdt_job, LocalCrdtJobInput},
    local_jobs::LocalJobRecord,
    loopback_state::LoopbackState,
    paths::cleanup_pending_upload_file,
    pdf_ingest_jobs::{
        spawn_docling_pdf_job, spawn_pymupdf4llm_pdf_job, DoclingPdfJobInput,
        Pymupdf4llmPdfJobInput,
    },
    pdf_ingest_payloads::pdf_accurate_job_result,
    pdf_ingest_upload::PdfAccurateUpload,
    pdf_ingestion_commands::pdf_ingestion_pipeline_status,
    pdf_pipeline::{pdf_pipeline_job_context, PDF_DOCLING_ENGINE_ID, PDF_PYMUPDF_ENGINE_ID},
    pdf_runtimes::PYMUPDF4LLM_RUNTIME_ID,
    storage::display_path,
};
use std::sync::Arc;
use uuid::Uuid;

pub(crate) async fn submit_pdf_accurate_ingest(
    state: Arc<LoopbackState>,
    graph_id: String,
    upload: PdfAccurateUpload,
) -> Result<(LocalJobRecord, String), String> {
    let PdfAccurateUpload {
        filename,
        mime_type,
        pending_original_path,
        size_bytes,
        parent_id,
        title,
    } = upload;

    let document_id = format!("doc-{}", Uuid::new_v4().simple());
    let source_storage_key = format!(
        "local://documents/{document_id}/original/{}",
        safe_filename(&filename)
    );
    let pipeline_status = match pdf_ingestion_pipeline_status(&state.app) {
        Ok(status) => status,
        Err(error) => {
            cleanup_pending_upload_file(&pending_original_path);
            return Err(error);
        }
    };
    let pipeline_context = pdf_pipeline_job_context(&pipeline_status);
    let docling_status = pipeline_status.docling_runtime_status.clone();

    if pipeline_status.effective_engine_id == PDF_DOCLING_ENGINE_ID && docling_status.available {
        let detail = serde_json::json!({
            "graph_id": graph_id.clone(),
            "graphId": graph_id.clone(),
            "document_id": document_id.clone(),
            "documentId": document_id.clone(),
            "filename": filename.clone(),
            "requestedApproachId": "pdf.docling-accurate",
            "requested_approach_id": "pdf.docling-accurate",
            "ingestionApproachId": "pdf.docling-accurate",
            "ingestion_approach_id": "pdf.docling-accurate",
            "localFallback": false,
            "local_fallback": false,
            "runtime": docling_status.runtime_id.clone(),
            "doclingVersion": docling_status.docling_version.clone(),
            "pipelinePreferredEngineId": pipeline_context.preferred_engine_id.clone(),
            "pipelineEffectiveEngineId": pipeline_context.effective_engine_id.clone(),
            "pipelineEffectiveReason": pipeline_context.effective_reason.clone(),
        });
        let record =
            match state
                .jobs
                .insert_queued("ingest_pdf_docling", Some(graph_id.clone()), detail)
            {
                Ok(record) => record,
                Err(error) => {
                    cleanup_pending_upload_file(&pending_original_path);
                    return Err(error.into());
                }
            };
        spawn_docling_pdf_job(
            state,
            record.job_id.clone(),
            DoclingPdfJobInput {
                graph_id,
                document_id: document_id.clone(),
                filename,
                mime_type,
                size_bytes,
                pending_original_path,
                parent_id,
                title,
                source_storage_key,
                pipeline_context,
            },
        );
        return Ok((record, document_id));
    }

    let pymupdf4llm_available = pipeline_status
        .engines
        .iter()
        .find(|engine| engine.engine_id == PDF_PYMUPDF_ENGINE_ID)
        .map(|engine| engine.available)
        .unwrap_or(false);
    if pipeline_status.effective_engine_id == PDF_PYMUPDF_ENGINE_ID && pymupdf4llm_available {
        let detail = serde_json::json!({
            "graph_id": graph_id.clone(),
            "graphId": graph_id.clone(),
            "document_id": document_id.clone(),
            "documentId": document_id.clone(),
            "filename": filename.clone(),
            "requestedApproachId": "pdf.docling-accurate",
            "requested_approach_id": "pdf.docling-accurate",
            "ingestionApproachId": PDF_PYMUPDF_ENGINE_ID,
            "ingestion_approach_id": PDF_PYMUPDF_ENGINE_ID,
            "localFallback": false,
            "local_fallback": false,
            "ocrAvailable": false,
            "ocr_available": false,
            "runtime": PYMUPDF4LLM_RUNTIME_ID,
            "pipelinePreferredEngineId": pipeline_context.preferred_engine_id.clone(),
            "pipelineEffectiveEngineId": pipeline_context.effective_engine_id.clone(),
            "pipelineEffectiveReason": pipeline_context.effective_reason.clone(),
        });
        let record =
            match state
                .jobs
                .insert_queued("ingest_pdf_pymupdf4llm", Some(graph_id.clone()), detail)
            {
                Ok(record) => record,
                Err(error) => {
                    cleanup_pending_upload_file(&pending_original_path);
                    return Err(error.into());
                }
            };
        spawn_pymupdf4llm_pdf_job(
            state,
            record.job_id.clone(),
            Pymupdf4llmPdfJobInput {
                graph_id,
                document_id: document_id.clone(),
                filename,
                mime_type,
                size_bytes,
                pending_original_path,
                parent_id,
                title,
                source_storage_key,
                pipeline_context,
            },
        );
        return Ok((record, document_id));
    }

    let payload = serde_json::json!({
        "filename": filename.clone(),
        "mimeType": mime_type.clone(),
        "sizeBytes": size_bytes,
        "pendingOriginalPath": display_path(&pending_original_path),
        "parentId": parent_id,
        "documentId": document_id.clone(),
        "title": title,
        "sourceFile": {
            "storageKey": source_storage_key,
            "originalFilename": filename.clone(),
            "mimeType": mime_type,
            "sizeBytes": size_bytes,
            "fileType": "pdf",
        },
        "requestedApproachId": "pdf.docling-accurate",
        "pipelinePreferredEngineId": pipeline_context.preferred_engine_id.clone(),
        "pipelineEffectiveEngineId": pipeline_context.effective_engine_id.clone(),
        "pipelineEffectiveReason": pipeline_context.effective_reason.clone(),
    });
    let detail = serde_json::json!({
        "graph_id": graph_id.clone(),
        "graphId": graph_id.clone(),
        "document_id": document_id.clone(),
        "documentId": document_id.clone(),
        "filename": filename.clone(),
        "requestedApproachId": "pdf.docling-accurate",
        "requested_approach_id": "pdf.docling-accurate",
        "ingestionApproachId": "pdf.fast-text",
        "ingestion_approach_id": "pdf.fast-text",
        "localFallback": true,
        "local_fallback": true,
        "ocrAvailable": false,
        "ocr_available": false,
        "pipelinePreferredEngineId": pipeline_context.preferred_engine_id.clone(),
        "pipelineEffectiveEngineId": pipeline_context.effective_engine_id.clone(),
        "pipelineEffectiveReason": pipeline_context.effective_reason.clone(),
    });
    let result_graph_id = graph_id.clone();
    let result_document_id = document_id.clone();
    let result_filename = filename.clone();
    let result_pipeline_context = pipeline_context.clone();
    let record = insert_and_spawn_crdt_job(
        state,
        LocalCrdtJobInput {
            job_type: "ingest_pdf_fast_text".to_string(),
            graph_id,
            operation_kind: "document.uploadIngest".to_string(),
            document_id: Some(document_id.clone()),
            payload,
            detail,
            pending_cleanup_path: Some(pending_original_path),
            running_message: "Ingesting PDF with fast-text fallback".to_string(),
            success_message: "PDF fast-text import complete".to_string(),
            result_mapper: Some(Box::new(move |value| {
                pdf_accurate_job_result(
                    result_graph_id,
                    result_document_id,
                    result_filename,
                    value,
                    Some(&result_pipeline_context),
                )
            })),
        },
    )?;
    Ok((record, document_id))
}
