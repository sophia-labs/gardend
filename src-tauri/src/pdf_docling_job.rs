use crate::{
    clock::timestamp,
    crdt_queue::{enqueue_crdt_operation, EnqueueCrdtOperationInput},
    json_utils::json_string,
    local_jobs::LocalJobProgress,
    loopback_state::LoopbackState,
    paths::cleanup_pending_upload_file,
    pdf_ingest_job_types::DoclingPdfJobInput,
    pdf_ingest_payloads::pdf_accurate_job_result,
    pdf_parsers::run_docling_pdf_to_markdown,
    storage::display_path,
};
use std::{sync::Arc, time::Duration};

fn docling_pdf_job_progress(
    state: &Arc<LoopbackState>,
    job_id: &str,
    phase: &str,
    message: &str,
    current: usize,
    total: usize,
    details: serde_json::Value,
) {
    let total = total.max(current);
    let percent = if total == 0 {
        0.0
    } else {
        (current as f64 / total as f64) * 100.0
    };
    let _ = state.jobs.update_progress(
        job_id,
        LocalJobProgress {
            phase: phase.to_string(),
            message: message.to_string(),
            current,
            total,
            percent,
            updated_at: timestamp(),
            details,
        },
    );
}

pub(crate) fn spawn_docling_pdf_job(
    state: Arc<LoopbackState>,
    job_id: String,
    input: DoclingPdfJobInput,
) {
    tokio::spawn(async move {
        if state.jobs.is_cancelled(&job_id).unwrap_or(false) {
            cleanup_pending_upload_file(&input.pending_original_path);
            return;
        }
        let _ = state.jobs.mark_running(&job_id);
        docling_pdf_job_progress(
            &state,
            &job_id,
            "parse",
            "Parsing PDF with Docling",
            1,
            3,
            serde_json::json!({
                "engineId": "pdf.docling-accurate",
                "runtimeId": input.pipeline_context.effective_engine_id.clone(),
            }),
        );
        let app = state.app.clone();
        let parse_app = app.clone();
        let parse_input = input.clone();
        let parsed = tokio::task::spawn_blocking(move || {
            run_docling_pdf_to_markdown(
                &parse_app,
                &parse_input.pending_original_path,
                &parse_input.filename,
                parse_input.title.as_deref(),
                Duration::from_secs(600),
            )
        })
        .await
        .map_err(|error| format!("Docling task join failed: {error}"))
        .and_then(|result| result);

        let result = match parsed {
            Ok(parsed) => {
                if state.jobs.is_cancelled(&job_id).unwrap_or(false) {
                    cleanup_pending_upload_file(&input.pending_original_path);
                    return;
                }
                let markdown = json_string(parsed.get("markdown")).unwrap_or_default();
                if markdown.trim().is_empty() {
                    Err("Docling conversion returned empty Markdown".to_string())
                } else {
                    docling_pdf_job_progress(
                        &state,
                        &job_id,
                        "write",
                        "Writing Docling Markdown into the local document",
                        2,
                        3,
                        serde_json::json!({
                            "engineId": "pdf.docling-accurate",
                            "charCount": markdown.chars().count(),
                        }),
                    );
                    let docling_title = input
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
                        "title": docling_title,
                        "markdown": markdown,
                        "warnings": parsed.get("warnings").cloned().unwrap_or_else(|| serde_json::json!([])),
                        "stats": parsed.get("stats").cloned().unwrap_or_else(|| serde_json::json!({})),
                        "sourceFile": source_file,
                        "requestedApproachId": "pdf.docling-accurate",
                        "pipelinePreferredEngineId": input.pipeline_context.preferred_engine_id.clone(),
                        "pipelineEffectiveEngineId": input.pipeline_context.effective_engine_id.clone(),
                        "pipelineEffectiveReason": input.pipeline_context.effective_reason.clone(),
                        "doclingVersion": parsed.get("doclingVersion").or_else(|| parsed.get("docling_version")).cloned(),
                    });
                    enqueue_crdt_operation(
                        app,
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
                        if value.get("doclingVersion").is_none() {
                            if let Some(version) = parsed
                                .get("doclingVersion")
                                .or_else(|| parsed.get("docling_version"))
                                .cloned()
                            {
                                if let Some(object) = value.as_object_mut() {
                                    object.insert("doclingVersion".to_string(), version.clone());
                                    object.insert("docling_version".to_string(), version);
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
        if result.is_err() {
            cleanup_pending_upload_file(&input.pending_original_path);
        } else {
            docling_pdf_job_progress(
                &state,
                &job_id,
                "complete",
                "Docling import complete",
                3,
                3,
                serde_json::json!({ "engineId": "pdf.docling-accurate" }),
            );
        }
        if state.jobs.is_cancelled(&job_id).unwrap_or(false) {
            cleanup_pending_upload_file(&input.pending_original_path);
            return;
        }
        let _ = state
            .jobs
            .finish_existing(&job_id, result, "application/json");
    });
}
