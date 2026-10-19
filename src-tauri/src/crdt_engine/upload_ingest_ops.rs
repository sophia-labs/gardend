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
/// How long one call to the parser pool may take, connection to last byte, in seconds (default 600: docling takes
/// minutes on a large PDF). A pool that accepts a connection and never answers must not hold the operation, and the
/// graph's writes queued behind it, forever: past this the call fails as transient and the queue's backoff retries it.
pub(crate) const PARSER_POOL_TIMEOUT_ENV: &str = "GARDEN_PARSER_TIMEOUT_SECS";
const PARSER_POOL_TIMEOUT_DEFAULT_SECS: u64 = 600;

fn parser_pool_timeout() -> std::time::Duration {
    let secs = std::env::var(PARSER_POOL_TIMEOUT_ENV)
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|secs| *secs > 0)
        .unwrap_or(PARSER_POOL_TIMEOUT_DEFAULT_SECS);
    std::time::Duration::from_secs(secs)
}

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

/// Why the parser pool gave no markdown. Read off platform-next
/// `parser-pool/app.py` (2026-10-14): `POST /parse` answers 200 with markdown,
/// or 200 with `error` and no markdown when every engine failed on the file
/// (it is not a document they can open; the last engine, PyMuPDF's text
/// extraction, is local and deterministic, so the same file fails again); 422
/// when the request does not match its model; 404/405 under a wrong base URL;
/// 500 only for an exception outside the engines (its `b64decode` of base64
/// that does not decode, refused before sending, or the process failing).
enum PoolRefusal {
    /// The pool will give the same answer for this file: a 4xx (but 408 and
    /// 429, which ask to try again), an answer with no markdown, or one larger
    /// than the cell reads. Refused, so the operation completes with it.
    Final(String),
    /// The pool could not be reached or failed in itself (a refused or dropped
    /// connection, a timeout, a 5xx): retried with the queue's backoff.
    Transient(String),
}

/// What a pool's error body says: FastAPI's `detail` (a string, or the
/// validation errors' messages), else the text itself, shortened.
fn pool_detail(text: &str) -> String {
    let detail = serde_json::from_str::<Value>(text)
        .ok()
        .and_then(|body| match body.get("detail") {
            Some(Value::String(detail)) => Some(detail.clone()),
            Some(Value::Array(errors)) => Some(
                errors
                    .iter()
                    .map(|error| {
                        let at = error
                            .get("loc")
                            .and_then(Value::as_array)
                            .map(|loc| {
                                loc.iter()
                                    .map(|part| match part {
                                        Value::String(part) => part.clone(),
                                        other => other.to_string(),
                                    })
                                    .collect::<Vec<_>>()
                                    .join(".")
                            })
                            .unwrap_or_default();
                        let message = error.get("msg").and_then(Value::as_str).unwrap_or("invalid");
                        if at.is_empty() { message.to_string() } else { format!("{at}: {message}") }
                    })
                    .collect::<Vec<_>>()
                    .join("; "),
            ),
            _ => None,
        })
        .unwrap_or_else(|| text.trim().to_string());
    if detail.chars().count() > 500 {
        format!("{}…", detail.chars().take(500).collect::<String>())
    } else {
        detail
    }
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
    timeout: std::time::Duration,
) -> Result<PoolParse, PoolRefusal> {
    let url = format!("{endpoint}/parse");
    // Statuses are answers, read below, not transport errors.
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(timeout))
        .build()
        .into();
    let mut response = agent
        .post(&url)
        .send_json(json!({
            "filename": filename,
            "mimeType": mime_type,
            "approach": approach,
            "dataBase64": data_base64,
        }))
        .map_err(|error| match error {
            ureq::Error::Timeout(_) => PoolRefusal::Transient(format!(
                "the parser pool did not answer within {} s ({url})",
                timeout.as_secs_f64()
            )),
            error => PoolRefusal::Transient(format!("parser pool request failed ({url}): {error}")),
        })?;
    let status = response.status().as_u16();
    let text = response.body_mut().read_to_string().map_err(|error| match error {
        ureq::Error::BodyExceedsLimit(limit) => PoolRefusal::Final(format!(
            "the parser pool's answer for {filename} is larger than the {limit} bytes a cell reads"
        )),
        error => PoolRefusal::Transient(format!(
            "parser pool answer could not be read ({url}, HTTP {status}): {error}"
        )),
    })?;
    match status {
        200..=299 => {}
        408 | 429 => {
            return Err(PoolRefusal::Transient(format!(
                "parser pool asked to try again (HTTP {status}): {}",
                pool_detail(&text)
            )))
        }
        400..=499 => {
            return Err(PoolRefusal::Final(format!(
                "the parser pool refused {filename} (HTTP {status}): {}",
                pool_detail(&text)
            )))
        }
        _ => {
            return Err(PoolRefusal::Transient(format!(
                "parser pool failed (HTTP {status}): {}",
                pool_detail(&text)
            )))
        }
    }
    let body: Value = serde_json::from_str(&text).map_err(|error| {
        PoolRefusal::Final(format!(
            "the parser pool's answer for {filename} is not JSON ({error}): {}",
            pool_detail(&text)
        ))
    })?;
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
            .unwrap_or("no markdown in its answer");
        return Err(PoolRefusal::Final(format!(
            "the parser pool could not read {filename}: {error}; warnings: {}",
            warnings.join(" | ")
        )));
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
            // A staged file that is not there, or not in this graph's staging
            // folder, never will be: refused, where the read below is retried
            // (it parked the operation, and every later write, forever).
            crate::pending_upload_service::resolved_pending_upload_path(
                app,
                Some(graph_id.as_str()),
                pending,
            )
            .map_err(ApplyOperationError::terminal)?;
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
        if inline_base64.is_some() {
            // The pool answers base64 it cannot decode with a 500, which is
            // retried: refused here instead, as the markdown branch does.
            base64::engine::general_purpose::STANDARD
                .decode(&data_base64)
                .map_err(|error| {
                    ApplyOperationError::terminal(format!(
                        "document.uploadIngest: invalid base64 payload: {error}"
                    ))
                })?;
        }
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
            parser_pool_timeout(),
        )
        .map_err(|refusal| match refusal {
            PoolRefusal::Final(message) => {
                ApplyOperationError::terminal(format!("document.uploadIngest: {message}"))
            }
            PoolRefusal::Transient(message) => {
                ApplyOperationError::retryable_after_hot_commit(message)
            }
        })?;
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

#[cfg(test)]
mod pool_timeout_tests {
    use super::*;
    use std::{net::TcpListener, sync::mpsc, thread, time::{Duration, Instant}};

    /// A pool that accepts the connection and never answers: the call gives up at its timeout (transient, so the
    /// queue retries it later) instead of holding the operation forever.
    #[test]
    fn a_pool_that_never_answers_times_out_as_transient() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let endpoint = format!("http://{}", listener.local_addr().expect("addr"));
        let _held = thread::spawn(move || {
            let (_socket, _) = listener.accept().expect("accept");
            thread::sleep(Duration::from_secs(30));
        });
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let started = Instant::now();
            let result = parse_via_pool(&endpoint, "a.pdf", "application/pdf", "auto", "", Duration::from_millis(500));
            let _ = tx.send((result.err().map(|refusal| match refusal {
                PoolRefusal::Transient(message) => format!("transient: {message}"),
                PoolRefusal::Final(message) => format!("final: {message}"),
            }), started.elapsed()));
        });
        let (refusal, elapsed) = rx.recv_timeout(Duration::from_secs(10)).expect("the call returned (it did not hang)");
        let refusal = refusal.expect("a silent pool is a refusal");
        assert!(refusal.starts_with("transient: the parser pool did not answer within 0.5 s"), "{refusal}");
        assert!(elapsed < Duration::from_secs(5), "{elapsed:?}");
    }
}
