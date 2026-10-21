//! A scan's words, read and kept: `read_scan_text` starts reading a document's original (a PDF whose pages are
//! pictures) in the background, and `scan_text_layer` answers with where that stands or the words themselves.
//!
//! The original is never rewritten. The words (`sophia.ocr-text-layer/1`, see pdf_ocr_text_layer.rs) are kept beside
//! the document as `renditions/ocr-text-layer.json`; a reader lays them over the pages. When the PDF had no text at all
//! (every page was read), the document made from it also gets the words, so search and agents can read them.

use crate::app_runtime::AppHandle;
use crate::{
    clock::timestamp,
    crdt_queue::{enqueue_crdt_operation, EnqueueCrdtOperationInput},
    files_service::workspace_snapshot,
    local_jobs::{LocalJobProgress, LocalJobRegistry},
    mcp_utils::{mcp_arg_string, mcp_graph_id_or_default},
    original_file_service::{read_artifact_original_file, read_document_original_file},
    paths::{existing_document_dir, existing_graph_dir},
    pdf_ocr_text_layer::{ocr_pdf_text_layer, pdfium, tesseract_version, OcrOptions, OcrTextLayer},
    rdf_service::snapshot_array,
    workspace_entity_projection::{workspace_entity_id, workspace_entity_title},
};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

const LAYER_FILE: &str = "ocr-text-layer.json";

#[derive(Debug, Clone)]
enum Reading {
    Running {
        job_id: String,
        done: u32,
        total: u32,
    },
    Failed {
        message: String,
    },
}

/// Readings in this process, by graph and document: what `scan_text_layer` reports while one runs.
fn readings() -> &'static Mutex<HashMap<String, Reading>> {
    static READINGS: OnceLock<Mutex<HashMap<String, Reading>>> = OnceLock::new();
    READINGS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn reading_key(graph_id: &str, document_id: &str) -> String {
    format!("{graph_id}\u{0}{document_id}")
}

fn set_reading(graph_id: &str, document_id: &str, reading: Option<Reading>) {
    if let Ok(mut map) = readings().lock() {
        match reading {
            Some(reading) => map.insert(reading_key(graph_id, document_id), reading),
            None => map.remove(&reading_key(graph_id, document_id)),
        };
    }
}

fn current_reading(graph_id: &str, document_id: &str) -> Option<Reading> {
    readings()
        .lock()
        .ok()
        .and_then(|map| map.get(&reading_key(graph_id, document_id)).cloned())
}

/// Why scans cannot be read on this host, when they cannot.
fn engines_missing() -> Option<String> {
    let mut missing = Vec::new();
    if let Err(error) = tesseract_version() {
        missing.push(error);
    }
    if let Err(error) = pdfium() {
        missing.push(error);
    }
    (!missing.is_empty()).then(|| missing.join(" "))
}

fn layer_path(app: &AppHandle, graph_id: &str, document_id: &str) -> Result<PathBuf, String> {
    let graph_dir = existing_graph_dir(app, graph_id)?;
    let document_dir = existing_document_dir(&graph_dir, document_id)?;
    Ok(document_dir.join("renditions").join(LAYER_FILE))
}

fn read_layer(app: &AppHandle, graph_id: &str, document_id: &str) -> Option<OcrTextLayer> {
    let path = layer_path(app, graph_id, document_id).ok()?;
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

fn write_layer(
    app: &AppHandle,
    graph_id: &str,
    document_id: &str,
    layer: &OcrTextLayer,
) -> Result<(), String> {
    let _lease =
        crate::crdt_engine::persistence_coordinator::acquire_hot_write_blocking_if_managed(
            app, graph_id,
        )?;
    let _flush_guard = crate::cell_durability::write_guard();
    let path = layer_path(app, graph_id, document_id)?;
    let dir = path.parent().ok_or("no renditions folder")?;
    std::fs::create_dir_all(dir)
        .map_err(|error| format!("could not make {}: {error}", dir.display()))?;
    let bytes = serde_json::to_vec(layer).map_err(|error| error.to_string())?;
    let partial = dir.join(format!("{LAYER_FILE}.partial"));
    std::fs::write(&partial, bytes)
        .map_err(|error| format!("could not write the scan's words: {error}"))?;
    std::fs::rename(&partial, &path)
        .map_err(|error| format!("could not keep the scan's words: {error}"))
}

/// The document's workspace entry (title, and the source file a browser import points at).
fn document_entry(app: &AppHandle, graph_id: &str, document_id: &str) -> Option<Value> {
    let snapshot = workspace_snapshot(app, graph_id).ok()??;
    snapshot_array(&snapshot, "documents")
        .iter()
        .find(|entity| workspace_entity_id(entity).as_deref() == Some(document_id))
        .cloned()
}

/// The document's original bytes: kept with the document, or (a browser import) the artifact its source file names.
fn original_bytes(
    app: &AppHandle,
    graph_id: &str,
    document_id: &str,
    entry: Option<&Value>,
) -> Result<(String, Vec<u8>), String> {
    if let Ok((manifest, bytes)) = read_document_original_file(app, graph_id, document_id) {
        return Ok((manifest.mime_type, bytes));
    }
    let storage_key = entry
        .and_then(|entry| entry.get("sourceFile"))
        .and_then(|source| {
            source
                .get("storageKey")
                .or_else(|| source.get("sf_storageKey"))
        })
        .and_then(Value::as_str)
        .unwrap_or_default();
    let artifact_id = storage_key
        .strip_prefix("local://artifacts/")
        .and_then(|rest| rest.split('/').next())
        .filter(|id| !id.is_empty())
        .ok_or_else(|| "this document has no original file to read".to_string())?;
    read_artifact_original_file(app, graph_id, artifact_id)
        .map(|(manifest, bytes)| (manifest.mime_type, bytes))
        .map_err(|error| format!("the original could not be read: {error}"))
}

/// The words as the document's text: paragraphs in reading order, a page's start marked so a reader can find it.
fn layer_markdown(layer: &OcrTextLayer) -> String {
    let mut out = String::new();
    let mut last_page = 0;
    for (page, paragraph) in layer.paragraphs() {
        if page != last_page {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&format!("*p. {page}*\n\n"));
            last_page = page;
        }
        out.push_str(&paragraph.replace('*', "\\*"));
        out.push_str("\n\n");
    }
    out
}

pub(super) async fn mcp_local_read_scan_text(
    app: AppHandle,
    jobs: Arc<LocalJobRegistry>,
    arguments: &Value,
) -> Result<Value, String> {
    let graph_id = mcp_graph_id_or_default(&app, arguments)?;
    let document_id = mcp_arg_string(arguments, &["document_id", "documentId"])
        .ok_or("document_id is required")?;
    let lang =
        mcp_arg_string(arguments, &["lang", "language"]).unwrap_or_else(|| "eng".to_string());
    if !lang
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '_')
    {
        return Err("lang must be Tesseract language codes like eng or eng+fra".to_string());
    }
    let force = arguments
        .get("force")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if let Some(Reading::Running {
        job_id,
        done,
        total,
    }) = current_reading(&graph_id, &document_id)
    {
        return Ok(
            json!({ "status": "running", "jobId": job_id, "progress": { "done": done, "total": total } }),
        );
    }
    if !force && read_layer(&app, &graph_id, &document_id).is_some() {
        return Ok(json!({ "status": "done" }));
    }
    if let Some(message) = engines_missing() {
        return Ok(json!({ "status": "unavailable", "message": message }));
    }
    let entry = document_entry(&app, &graph_id, &document_id);
    let (mime_type, bytes) = original_bytes(&app, &graph_id, &document_id, entry.as_ref())?;
    if !mime_type.contains("pdf") && !bytes.starts_with(b"%PDF") {
        return Err(format!(
            "only a PDF's pages can be read; this original is {mime_type}"
        ));
    }
    let record = jobs
        .insert_queued(
            "read_scan_text",
            Some(graph_id.clone()),
            json!({ "graphId": graph_id, "documentId": document_id, "lang": lang }),
        )
        .map_err(|error| error.to_string())?;
    let job_id = record.job_id.clone();
    set_reading(
        &graph_id,
        &document_id,
        Some(Reading::Running {
            job_id: job_id.clone(),
            done: 0,
            total: 0,
        }),
    );
    spawn_reading(
        app,
        jobs,
        job_id.clone(),
        graph_id,
        document_id,
        entry,
        bytes,
        lang,
        force,
    );
    Ok(json!({ "status": "running", "jobId": job_id }))
}

#[allow(clippy::too_many_arguments)]
fn spawn_reading(
    app: AppHandle,
    jobs: Arc<LocalJobRegistry>,
    job_id: String,
    graph_id: String,
    document_id: String,
    entry: Option<Value>,
    bytes: Vec<u8>,
    lang: String,
    force: bool,
) {
    tokio::spawn(async move {
        let _ = jobs.mark_running(&job_id);
        let work_dir = std::env::temp_dir().join(format!("gardend-scan-{job_id}"));
        let (read_jobs, read_job, read_graph, read_document) = (
            jobs.clone(),
            job_id.clone(),
            graph_id.clone(),
            document_id.clone(),
        );
        let options = OcrOptions { lang, force };
        let read = tokio::task::spawn_blocking(move || {
            ocr_pdf_text_layer(
                &bytes,
                &options,
                &work_dir,
                |done, total| {
                    set_reading(
                        &read_graph,
                        &read_document,
                        Some(Reading::Running {
                            job_id: read_job.clone(),
                            done,
                            total,
                        }),
                    );
                    let _ = read_jobs.update_progress(
                        &read_job,
                        LocalJobProgress {
                            phase: "read".to_string(),
                            message: format!("Read {done} of {total} pages"),
                            current: done as usize,
                            total: total as usize,
                            percent: if total == 0 {
                                0.0
                            } else {
                                done as f64 * 100.0 / total as f64
                            },
                            updated_at: timestamp(),
                            details: json!({}),
                        },
                    );
                },
                || read_jobs.is_cancelled(&read_job).unwrap_or(false),
            )
            .inspect(|_| {
                let _ = std::fs::remove_dir_all(&work_dir);
            })
        })
        .await
        .map_err(|error| format!("the reading stopped: {error}"))
        .and_then(|result| result);
        let result = match read {
            Ok(layer) => finish_reading(&app, &graph_id, &document_id, entry.as_ref(), layer).await,
            Err(error) => Err(error),
        };
        match &result {
            Ok(_) => set_reading(&graph_id, &document_id, None),
            Err(message) => set_reading(
                &graph_id,
                &document_id,
                Some(Reading::Failed {
                    message: message.clone(),
                }),
            ),
        }
        let _ = jobs.finish_existing(&job_id, result, "application/json");
    });
}

async fn finish_reading(
    app: &AppHandle,
    graph_id: &str,
    document_id: &str,
    entry: Option<&Value>,
    layer: OcrTextLayer,
) -> Result<Value, String> {
    write_layer(app, graph_id, document_id, &layer)?;
    let read = layer
        .pages
        .iter()
        .filter(|page| page.status == "ocr")
        .count();
    let words: usize = layer.pages.iter().map(|page| page.words.len()).sum();
    // The document gets the words only when the PDF had none of its own: then its text is the import's placeholder.
    let all_read = read > 0 && layer.pages.iter().all(|page| page.status == "ocr");
    let mut wrote_text = false;
    if all_read && words > 0 {
        let mut payload = json!({ "documentId": document_id, "content": layer_markdown(&layer), "format": "markdown", "readOnly": true });
        if let Some(title) = entry.and_then(workspace_entity_title) {
            payload["title"] = json!(title);
        }
        enqueue_crdt_operation(
            app.clone(),
            EnqueueCrdtOperationInput {
                kind: "document.write".to_string(),
                graph_id: graph_id.to_string(),
                document_id: Some(document_id.to_string()),
                payload,
            },
        )
        .await?;
        wrote_text = true;
    }
    Ok(
        json!({ "status": "done", "pages": layer.page_count, "pagesRead": read, "words": words, "documentText": wrote_text, "engine": layer.engine }),
    )
}

pub(super) async fn mcp_local_scan_text_layer(
    app: AppHandle,
    arguments: &Value,
) -> Result<Value, String> {
    let graph_id = mcp_graph_id_or_default(&app, arguments)?;
    let document_id = mcp_arg_string(arguments, &["document_id", "documentId"])
        .ok_or("document_id is required")?;
    match current_reading(&graph_id, &document_id) {
        Some(Reading::Running {
            job_id,
            done,
            total,
        }) => {
            return Ok(
                json!({ "status": "running", "jobId": job_id, "progress": { "done": done, "total": total } }),
            );
        }
        Some(Reading::Failed { message }) => {
            return Ok(json!({ "status": "failed", "message": message }))
        }
        None => {}
    }
    match read_layer(&app, &graph_id, &document_id) {
        Some(layer) => {
            let words = arguments
                .get("includeWords")
                .and_then(Value::as_bool)
                .unwrap_or(true);
            if words {
                Ok(json!({ "status": "done", "layer": layer }))
            } else {
                Ok(json!({ "status": "done", "pages": layer.page_count, "engine": layer.engine }))
            }
        }
        None => Ok(json!({ "status": "none" })),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pdf_ocr_text_layer::{OcrPage, OcrWord, OCR_TEXT_LAYER_SCHEMA};

    #[test]
    fn the_words_become_paragraphs_with_each_page_marked() {
        let word = |text: &str, par, line| OcrWord {
            text: text.into(),
            x0: 0.0,
            y0: 0.0,
            x1: 1.0,
            y1: 1.0,
            conf: 90.0,
            block: 1,
            par,
            line,
        };
        let page = |n, words| OcrPage {
            page: n,
            status: "ocr".into(),
            rotation: 0,
            left: 0.0,
            bottom: 0.0,
            width: 612.0,
            height: 792.0,
            words,
            note: None,
        };
        let layer = OcrTextLayer {
            schema: OCR_TEXT_LAYER_SCHEMA.into(),
            engine: "tesseract 5.5.2".into(),
            lang: "eng".into(),
            dpi: 300,
            page_count: 2,
            pages: vec![
                page(
                    1,
                    vec![
                        word("THE", 1, 1),
                        word("TREE", 1, 1),
                        word("Knowledge", 2, 1),
                        word("*is*", 2, 1),
                    ],
                ),
                page(2, vec![word("Next", 1, 1)]),
            ],
        };
        assert_eq!(
            layer_markdown(&layer),
            "*p. 1*\n\nTHE TREE\n\nKnowledge \\*is\\*\n\n\n*p. 2*\n\nNext\n\n"
        );
    }
}
