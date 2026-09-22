use crate::{
    json_utils::json_string,
    local_jobs::{local_job_submit_response, LocalJobRecord},
    pdf_pipeline::{PdfPipelineJobContext, PDF_FAST_TEXT_ENGINE_ID, PDF_PIPELINE_AUTO_ENGINE_ID},
};

pub(crate) fn pdf_accurate_upload_response(
    record: &LocalJobRecord,
    document_id: &str,
) -> Result<serde_json::Value, String> {
    let mut value = serde_json::to_value(local_job_submit_response(record))
        .map_err(|error| format!("serialize local PDF accurate job response: {error}"))?;
    if let Some(object) = value.as_object_mut() {
        object.insert(
            "jobId".to_string(),
            serde_json::Value::String(record.job_id.clone()),
        );
        object.insert(
            "documentId".to_string(),
            serde_json::Value::String(document_id.to_string()),
        );
        object.insert(
            "document_id".to_string(),
            serde_json::Value::String(document_id.to_string()),
        );
    }
    Ok(value)
}

pub(crate) fn pdf_accurate_job_result(
    graph_id: String,
    document_id: String,
    filename: String,
    upload_value: serde_json::Value,
    pipeline_context: Option<&PdfPipelineJobContext>,
) -> Result<serde_json::Value, String> {
    let created_document_id = json_string(
        upload_value
            .get("documentId")
            .or_else(|| upload_value.get("document_id")),
    )
    .unwrap_or_else(|| document_id.clone());
    let title =
        json_string(upload_value.get("title")).unwrap_or_else(|| created_document_id.clone());
    let ingestion_approach_id = json_string(
        upload_value
            .get("ingestionApproachId")
            .or_else(|| upload_value.get("ingestion_approach_id")),
    )
    .unwrap_or_else(|| "pdf.fast-text".to_string());
    let local_fallback = upload_value
        .get("localFallback")
        .or_else(|| upload_value.get("local_fallback"))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or_else(|| ingestion_approach_id == PDF_FAST_TEXT_ENGINE_ID);
    let ocr_available = upload_value
        .get("ocrAvailable")
        .or_else(|| upload_value.get("ocr_available"))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(!local_fallback);
    let mut warnings = upload_value
        .get("warnings")
        .and_then(serde_json::Value::as_array)
        .cloned()
        .unwrap_or_default();
    if local_fallback {
        warnings.push(serde_json::Value::String(
            "Local Docling/OCR is not available; used the optimized local PDF fast-text layout path."
                .to_string(),
        ));
    }
    let pipeline_preferred_engine_id = pipeline_context
        .map(|context| context.preferred_engine_id.clone())
        .unwrap_or_else(|| PDF_PIPELINE_AUTO_ENGINE_ID.to_string());
    let pipeline_effective_engine_id = pipeline_context
        .map(|context| context.effective_engine_id.clone())
        .unwrap_or_else(|| ingestion_approach_id.clone());
    let pipeline_effective_reason = pipeline_context
        .map(|context| context.effective_reason.clone())
        .unwrap_or_else(|| {
            "PDF accurate facade used the current local runtime decision.".to_string()
        });

    Ok(serde_json::json!({
        "document_id": created_document_id.clone(),
        "documentId": created_document_id,
        "title": title,
        "graph_id": graph_id.clone(),
        "graphId": graph_id,
        "filename": filename,
        "requestedApproachId": "pdf.docling-accurate",
        "requested_approach_id": "pdf.docling-accurate",
        "ingestionApproachId": ingestion_approach_id,
        "ingestion_approach_id": ingestion_approach_id,
        "pipelinePreferredEngineId": pipeline_preferred_engine_id,
        "pipeline_preferred_engine_id": pipeline_preferred_engine_id,
        "pipelineEffectiveEngineId": pipeline_effective_engine_id,
        "pipeline_effective_engine_id": pipeline_effective_engine_id,
        "pipelineEffectiveReason": pipeline_effective_reason,
        "pipeline_effective_reason": pipeline_effective_reason,
        "localFallback": local_fallback,
        "local_fallback": local_fallback,
        "ocrAvailable": ocr_available,
        "ocr_available": ocr_available,
        "warnings": warnings,
        "stats": upload_value.get("stats").cloned().unwrap_or_else(|| serde_json::json!({})),
        "doclingVersion": upload_value.get("doclingVersion").or_else(|| upload_value.get("docling_version")).cloned(),
        "docling_version": upload_value.get("doclingVersion").or_else(|| upload_value.get("docling_version")).cloned(),
        "pymupdf4llmVersion": upload_value.get("pymupdf4llmVersion").or_else(|| upload_value.get("pymupdf4llm_version")).cloned(),
        "pymupdf4llm_version": upload_value.get("pymupdf4llmVersion").or_else(|| upload_value.get("pymupdf4llm_version")).cloned(),
        "document": upload_value,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pdf_accurate_job_result_marks_fast_text_as_local_fallback() {
        let result = pdf_accurate_job_result(
            "graph-1".to_string(),
            "doc-1".to_string(),
            "paper.pdf".to_string(),
            serde_json::json!({
                "documentId": "doc-1",
                "title": "Paper",
                "ingestionApproachId": PDF_FAST_TEXT_ENGINE_ID,
                "warnings": [],
            }),
            None,
        )
        .expect("payload should serialize");

        assert_eq!(
            result
                .get("localFallback")
                .and_then(serde_json::Value::as_bool),
            Some(true)
        );
        assert_eq!(
            result
                .get("pipelinePreferredEngineId")
                .and_then(serde_json::Value::as_str),
            Some(PDF_PIPELINE_AUTO_ENGINE_ID)
        );
        assert_eq!(
            result
                .get("warnings")
                .and_then(serde_json::Value::as_array)
                .map(Vec::len),
            Some(1)
        );
    }
}
