//! document.uploadIngest — file upload ingestion for headless cells.
//!
//! Cells don't parse PDFs: parsing is a pure function and lives in the
//! parser pool (platform-next/parser-pool, env GARDEN_PARSER_URL — same
//! pattern as the embeddings pool). This handler:
//!   1. resolves the upload bytes (pendingOriginalPath or dataBase64),
//!   2. markdown/plain text skips the pool entirely,
//!   3. PDFs go to the pool (`POST /parse` → markdown + stats + approach),
//!   4. composes onto the ported document.ingestMarkdownOriginal handler
//!      (original-file storage, workspace entry, room write, projections).
//!
//! Contract note: the desktop runs parsing as local Python subprocess jobs;
//! the cell envelope mirrors ingestMarkdownOriginal's (which the desktop
//! upload path also converges on). Approach ids match the desktop catalog
//! (pdf.docling-accurate / pdf.pymupdf4llm / pdf.fast-text).

use crate::app_runtime::AppHandle;
use crate::crdt_engine::executor::{ApplyOperationError, ApplyOperationResult};
use crate::crdt_queue::CrdtOperation;
use crate::pending_upload_service::{read_pending_upload_file, PendingUploadFileInput};
use base64::Engine;
use serde_json::{json, Map, Value};

pub(crate) const PARSER_POOL_ENV: &str = "GARDEN_PARSER_URL";

fn obj(value: &Value) -> Map<String, Value> {
    value.as_object().cloned().unwrap_or_default()
}

fn coalesce_str(map: &Map<String, Value>, keys: &[&str]) -> Option<String> {
    for key in keys {
        match map.get(*key) {
            Some(Value::String(s)) if !s.is_empty() => return Some(s.clone()),
            Some(Value::Null) | None => continue,
            _ => continue,
        }
    }
    None
}

fn parser_pool_endpoint() -> Option<String> {
    std::env::var(PARSER_POOL_ENV)
        .ok()
        .map(|value| value.trim().trim_end_matches('/').to_string())
        .filter(|value| !value.is_empty())
}

struct PoolParse {
    markdown: String,
    stats: Value,
    warnings: Vec<String>,
    approach_used: Option<String>,
    local_fallback: bool,
}

fn parse_via_pool(
    endpoint: &str,
    filename: &str,
    mime_type: &str,
    approach: &str,
    data_base64: &str,
) -> Result<PoolParse, String> {
    let url = format!("{endpoint}/parse");
    let mut response = ureq::Agent::new_with_defaults()
        .post(&url)
        .send_json(json!({
            "filename": filename,
            "mimeType": mime_type,
            "approach": approach,
            "dataBase64": data_base64,
        }))
        .map_err(|error| format!("parser pool request failed ({url}): {error}"))?;
    let body: Value = response
        .body_mut()
        .read_json()
        .map_err(|error| format!("parser pool returned invalid JSON: {error}"))?;
    let warnings: Vec<String> = body
        .get("warnings")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let Some(markdown) = body.get("markdown").and_then(Value::as_str) else {
        let error = body
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("parser pool returned no markdown");
        return Err(format!("{error}; warnings: {}", warnings.join(" | ")));
    };
    Ok(PoolParse {
        markdown: markdown.to_string(),
        stats: body.get("stats").cloned().unwrap_or_else(|| json!({})),
        warnings,
        approach_used: body
            .get("approachUsed")
            .and_then(Value::as_str)
            .map(str::to_string),
        local_fallback: body
            .get("localFallback")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    })
}

pub(crate) async fn apply(app: &AppHandle, operation: &CrdtOperation) -> Result<Value, String> {
    apply_classified(app, operation)
        .await
        .map_err(ApplyOperationError::into_message)
}

pub(crate) async fn apply_classified(
    app: &AppHandle,
    operation: &CrdtOperation,
) -> ApplyOperationResult<Value> {
    let payload = obj(&operation.payload);
    let pending_path = coalesce_str(&payload, &["pendingOriginalPath", "pending_original_path"]);
    match crate::operation_completion_ledger::completion_entry_for(app, &operation.operation_id) {
        Ok(Some(entry)) if entry.kind == "document.uploadIngest" => {
            if let Some(path) = pending_path.as_deref() {
                cleanup_pending_best_effort(app, &operation.graph_id, path);
            }
            let mut cached = entry
                .result
                .and_then(|result| result.as_object().cloned())
                .unwrap_or_default();
            cached.insert("replayed".to_string(), json!(true));
            return Ok(Value::Object(cached));
        }
        Ok(Some(entry)) => {
            return Err(ApplyOperationError::terminal(format!(
                "completion ledger operation {} belongs to {}, not document.uploadIngest",
                operation.operation_id, entry.kind
            )));
        }
        Ok(None) => {}
        Err(error) => {
            return Err(ApplyOperationError::retryable_after_hot_commit(format!(
                "read upload ingest completion ledger: {error}"
            )));
        }
    }

    let result = apply_steps_classified(app, operation).await?;
    let entry = crate::operation_completion_ledger::OperationCompletionEntry {
        schema_version: 1,
        operation_id: operation.operation_id.clone(),
        kind: "document.uploadIngest".to_string(),
        graph_id: Some(operation.graph_id.clone()),
        completed_at: operation.enqueue_timestamp.clone(),
        payload_hash: None,
        result: Some(result.clone()),
    };
    crate::operation_completion_ledger::append_completion_entry(app, entry)
        .map_err(ApplyOperationError::retryable_after_hot_commit)?;
    if let Some(path) = pending_path.as_deref() {
        cleanup_pending_best_effort(app, &operation.graph_id, path);
    }
    Ok(result)
}

fn cleanup_pending_best_effort(app: &AppHandle, graph_id: &str, pending_path: &str) {
    if let Err(error) = crate::pending_upload_service::cleanup_pending_upload(
        app.clone(),
        PendingUploadFileInput {
            graph_id: Some(graph_id.to_string()),
            pending_path: pending_path.to_string(),
        },
    ) {
        log::warn!("[document.uploadIngest] pending cleanup failed: {error}");
    }
}

async fn apply_steps_classified(
    app: &AppHandle,
    operation: &CrdtOperation,
) -> ApplyOperationResult<Value> {
    let payload = obj(&operation.payload);
    let graph_id = operation.graph_id.clone();
    let document_id = operation
        .document_id
        .clone()
        .or_else(|| coalesce_str(&payload, &["documentId", "document_id"]))
        .ok_or_else(|| {
            "document.uploadIngest: documentId is required (provide via payload or rely on enqueue normalization)"
                .to_string()
        })?;
    let filename = coalesce_str(
        &payload,
        &["filename", "originalFilename", "original_filename"],
    )
    .unwrap_or_else(|| "document.pdf".to_string());
    let mime_type = coalesce_str(
        &payload,
        &["mimeType", "mime_type", "contentType", "content_type"],
    )
    .unwrap_or_else(|| "application/pdf".to_string());
    let pending_path = coalesce_str(&payload, &["pendingOriginalPath", "pending_original_path"]);
    let inline_base64 = coalesce_str(&payload, &["dataBase64", "data_base64"]);
    if pending_path.is_none() && inline_base64.is_none() {
        return Err(ApplyOperationError::terminal(
            "document.uploadIngest: pendingOriginalPath or dataBase64 is required".to_string(),
        ));
    }
    let requested_approach = coalesce_str(
        &payload,
        &[
            "requestedApproachId",
            "requested_approach_id",
            "ingestionApproachId",
            "ingestion_approach_id",
        ],
    )
    .unwrap_or_else(|| "pdf.docling-accurate".to_string());

    // Resolve upload bytes once (the pool needs them; the original-file
    // store reuses the pending path so the file is moved, not duplicated).
    let data_base64 = match (&inline_base64, &pending_path) {
        (Some(b64), _) => b64.clone(),
        (None, Some(pending)) => {
            let record = read_pending_upload_file(
                app.clone(),
                PendingUploadFileInput {
                    graph_id: Some(graph_id.clone()),
                    pending_path: pending.clone(),
                },
            )
            .map_err(ApplyOperationError::retryable_after_hot_commit)?;
            record.data_base64
        }
        (None, None) => unreachable!(),
    };

    let lower_name = filename.to_lowercase();
    let is_markdownish = mime_type.starts_with("text/markdown")
        || mime_type.starts_with("text/plain")
        || lower_name.ends_with(".md")
        || lower_name.ends_with(".markdown")
        || lower_name.ends_with(".txt");

    let (markdown, stats, mut warnings, approach_used, local_fallback) = if is_markdownish {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(&data_base64)
            .map_err(|error| format!("document.uploadIngest: invalid base64 payload: {error}"))?;
        let markdown = String::from_utf8_lossy(&bytes).into_owned();
        (
            markdown,
            json!({ "sizeBytes": bytes.len() }),
            Vec::new(),
            Some("text.passthrough".to_string()),
            false,
        )
    } else {
        let Some(endpoint) = parser_pool_endpoint() else {
            return Err(ApplyOperationError::terminal(format!(
                "document.uploadIngest: no parser pool configured (set {PARSER_POOL_ENV}); \
                 {mime_type} uploads require the parser pool in headless cells"
            )));
        };
        let parsed = parse_via_pool(
            &endpoint,
            &filename,
            &mime_type,
            &requested_approach,
            &data_base64,
        )
        .map_err(ApplyOperationError::retryable_after_hot_commit)?;
        (
            parsed.markdown,
            parsed.stats,
            parsed.warnings,
            parsed.approach_used,
            parsed.local_fallback,
        )
    };
    if markdown.trim().is_empty() {
        warnings.push("parser produced empty markdown for this upload".to_string());
    }

    // Compose onto the ported ingestMarkdownOriginal path: original-file
    // storage + workspace entry + room write + projections + envelope.
    let mut ingest_payload = json!({
        "documentId": document_id,
        "markdown": markdown,
        "filename": filename,
        "mimeType": mime_type,
        "requestedApproachId": requested_approach,
        "ingestionApproachId": approach_used,
        "localFallback": local_fallback,
        "stats": stats,
        "warnings": warnings,
    });
    if let Some(map) = ingest_payload.as_object_mut() {
        // ingestMarkdownOriginal is a PDF-flow helper and defaults
        // fileType to "pdf" — override from the actual filename unless the
        // caller supplied an explicit sourceFile.
        if payload.get("sourceFile").is_none() && payload.get("source_file").is_none() {
            map.insert(
                "sourceFile".into(),
                json!({ "fileType": super::workspace_ops::artifact_file_type(&filename) }),
            );
        }
        for key in [
            "parentId",
            "parent_id",
            "title",
            "titleOverride",
            "title_override",
            "ocrAvailable",
            "ocr_available",
            "sourceFile",
            "source_file",
            "order",
            "updatedAt",
        ] {
            if let Some(value) = payload.get(key) {
                map.insert(key.to_string(), value.clone());
            }
        }
        match &pending_path {
            Some(pending) => {
                map.insert("pendingOriginalPath".into(), json!(pending));
            }
            None => {
                map.insert("dataBase64".into(), json!(data_base64));
            }
        }
    }
    let ingest_operation = CrdtOperation {
        operation_id: operation.operation_id.clone(),
        kind: "document.ingestMarkdownOriginal".to_string(),
        graph_id,
        document_id: Some(document_id),
        payload: ingest_payload,
        enqueue_timestamp: operation.enqueue_timestamp.clone(),
    };
    let mut envelope =
        super::document_ops::ingest_markdown_original_steps_classified(app, &ingest_operation)
            .await?;
    // ingestMarkdownOriginal is a PDF-flow helper whose envelope hardcodes
    // fileType "pdf" (TS does the same); the desktop uploadIngest contract
    // reports the actual uploaded file type at the top level.
    if let Some(map) = envelope.as_object_mut() {
        map.insert(
            "fileType".into(),
            json!(super::workspace_ops::artifact_file_type(&filename)),
        );
    }
    Ok(envelope)
}
