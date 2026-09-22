use crate::{
    clock::timestamp,
    crdt_queue::{enqueue_crdt_operation, EnqueueCrdtOperationInput},
    json_utils::json_string,
    local_jobs::LocalJobProgress,
    loopback_state::LoopbackState,
    paths::cleanup_pending_upload_file,
    pdf_ingest_job_types::Pymupdf4llmPdfJobInput,
    pdf_ingest_payloads::pdf_accurate_job_result,
    pdf_parsers::run_pymupdf4llm_pdf_to_markdown,
    pdf_pipeline::PDF_PYMUPDF_ENGINE_ID,
    pdf_runtimes::PYMUPDF4LLM_RUNTIME_ID,
    storage::display_path,
};
use std::{sync::Arc, time::Duration};

fn pdf_job_progress(
    state: &Arc<LoopbackState>,
    job_id: &str,
    phase: &str,
    message: &str,
    current: usize,
    total: usize,
    details: serde_json::Value,
) {
    let percent = if total == 0 {
        0.0
    } else {
        (current as f64 / total.max(current) as f64) * 100.0
    };
    let _ = state.jobs.update_progress(
        job_id,
        LocalJobProgress {
            phase: phase.to_string(),
            message: message.to_string(),
            current,
            total: total.max(current),
            percent,
            updated_at: timestamp(),
            details,
        },
    );
}

pub(crate) fn spawn_pymupdf4llm_pdf_job(
    state: Arc<LoopbackState>,
    job_id: String,
    input: Pymupdf4llmPdfJobInput,
) {
    tokio::spawn(async move {
        let _ = state.jobs.mark_running(&job_id);
        pdf_job_progress(
            &state,
            &job_id,
            "parse",
            "Parsing PDF with PyMuPDF4LLM",
            1,
            3,
            serde_json::json!({
                "engineId": PDF_PYMUPDF_ENGINE_ID,
                "runtimeId": PYMUPDF4LLM_RUNTIME_ID,
            }),
        );
        let parse_input = input.clone();
        let parse_jobs = state.jobs.clone();
        let parse_job_id = job_id.clone();
        let parsed = tokio::task::spawn_blocking(move || {
            run_pymupdf4llm_pdf_to_markdown(
                &parse_input.pending_original_path,
                &parse_input.filename,
                parse_input.title.as_deref(),
                Duration::from_secs(900),
                &parse_jobs,
                &parse_job_id,
            )
        })
        .await
        .map_err(|error| format!("PyMuPDF4LLM task join failed: {error}"))
        .and_then(|result| result);

        let result = match parsed {
            Ok(parsed) => {
                if state.jobs.is_cancelled(&job_id).unwrap_or(false) {
                    cleanup_pending_upload_file(&input.pending_original_path);
                    return;
                }
                let markdown = json_string(parsed.get("markdown")).unwrap_or_default();
                if markdown.trim().is_empty() {
                    Err("PyMuPDF4LLM conversion returned empty Markdown".to_string())
                } else {
                    pdf_job_progress(
                        &state,
                        &job_id,
                        "write",
                        "Writing PyMuPDF4LLM Markdown into the local document",
                        2,
                        3,
                        serde_json::json!({
                            "engineId": PDF_PYMUPDF_ENGINE_ID,
                            "charCount": markdown.chars().count(),
                        }),
                    );
                    let document_title = input
                        .title
                        .clone()
                        .or_else(|| json_string(parsed.get("title")));
                    let source_file = serde_json::json!({
                        "storageKey": input.source_storage_key.clone(),
                        "originalFilename": input.filename.clone(),
                        "mimeType": input.mime_type.clone(),
                        "sizeBytes": input.size_bytes,
                        "fileType": "pdf",
                    });
                    let payload = serde_json::json!({
                        "filename": input.filename.clone(),
                        "mimeType": input.mime_type.clone(),
                        "sizeBytes": input.size_bytes,
                        "pendingOriginalPath": display_path(&input.pending_original_path),
                        "parentId": input.parent_id.clone(),
                        "documentId": input.document_id.clone(),
                        "title": document_title,
                        "markdown": markdown,
                        "warnings": parsed.get("warnings").cloned().unwrap_or_else(|| serde_json::json!([])),
                        "stats": parsed.get("stats").cloned().unwrap_or_else(|| serde_json::json!({})),
                        "sourceFile": source_file,
                        "requestedApproachId": "pdf.docling-accurate",
                        "ingestionApproachId": PDF_PYMUPDF_ENGINE_ID,
                        "localFallback": false,
                        "ocrAvailable": false,
                        "pipelinePreferredEngineId": input.pipeline_context.preferred_engine_id.clone(),
                        "pipelineEffectiveEngineId": input.pipeline_context.effective_engine_id.clone(),
                        "pipelineEffectiveReason": input.pipeline_context.effective_reason.clone(),
                        "pymupdf4llmVersion": parsed.get("pymupdf4llmVersion").or_else(|| parsed.get("pymupdf4llm_version")).cloned(),
                        "pymupdf4llm_version": parsed.get("pymupdf4llmVersion").or_else(|| parsed.get("pymupdf4llm_version")).cloned(),
                    });
                    enqueue_crdt_operation(
                        state.app.clone(),
                        EnqueueCrdtOperationInput {
                            kind: "document.ingestMarkdownOriginal".to_string(),
                            graph_id: input.graph_id.clone(),
                            document_id: Some(input.document_id.clone()),
                            payload,
                        },
                    )
                    .await
                    .and_then(|value| {
                        let mut value = value;
                        if value.get("pymupdf4llmVersion").is_none() {
                            if let Some(version) = parsed
                                .get("pymupdf4llmVersion")
                                .or_else(|| parsed.get("pymupdf4llm_version"))
                                .cloned()
                            {
                                if let Some(object) = value.as_object_mut() {
                                    object
                                        .insert("pymupdf4llmVersion".to_string(), version.clone());
                                    object.insert("pymupdf4llm_version".to_string(), version);
                                }
                            }
                        }
                        pdf_accurate_job_result(
                            input.graph_id.clone(),
                            input.document_id.clone(),
                            input.filename.clone(),
                            value,
                            Some(&input.pipeline_context),
                        )
                    })
                }
            }
            Err(error) => {
                cleanup_pending_upload_file(&input.pending_original_path);
                Err(error)
            }
        };
        if result.is_ok() {
            pdf_job_progress(
                &state,
                &job_id,
                "complete",
                "PyMuPDF4LLM import complete",
                3,
                3,
                serde_json::json!({ "engineId": PDF_PYMUPDF_ENGINE_ID }),
            );
        } else {
            cleanup_pending_upload_file(&input.pending_original_path);
        }
        let _ = state
            .jobs
            .finish_existing(&job_id, result, "application/json");
    });
}
