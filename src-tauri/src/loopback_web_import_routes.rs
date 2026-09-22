use crate::{
    crdt_projection_flush::{
        flush_document_projection_phase, prefers_synchronous_flush,
        spawn_deferred_document_projection_flush,
    },
    crdt_queue::{enqueue_crdt_operation_outcome, EnqueueCrdtOperationInput},
    loopback_http::{loopback_error, require_loopback_scope},
    loopback_state::LoopbackState,
    paths::existing_graph_dir,
    runtime_config::LOCAL_WEB_CLIP_MAX_BYTES,
    web_fetch::{fetch_limited_url_bytes, local_web_fetch_client, validate_http_url},
    youtube_transcript_service::{
        fetch_youtube_transcript_markdown, YoutubeTranscriptRequest, YOUTUBE_NOTICE,
    },
};
use axum::{
    extract::{Path as AxumPath, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;
use std::sync::Arc;
use uuid::Uuid;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ClipUrlRequest {
    url: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default, alias = "folder_id")]
    folder_id: Option<String>,
    /// Optional pre-fetched HTML. When provided the route skips the live
    /// fetch — used by gateways/harnesses that already hold the page (and
    /// prevents requests silently testing the live internet).
    #[serde(default)]
    html: Option<String>,
}

fn trim_optional(value: Option<String>) -> Option<String> {
    value.and_then(|value| {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    })
}

fn local_import_response(
    status: &str,
    documents_created: i64,
    document_ids: Vec<String>,
    warnings: Vec<String>,
    errors: Vec<String>,
) -> serde_json::Value {
    serde_json::json!({
        "status": status,
        "documents_created": documents_created,
        "documentsCreated": documents_created,
        "folders_created": 0,
        "foldersCreated": 0,
        "wires_created": 0,
        "wiresCreated": 0,
        "tags_created": 0,
        "tagsCreated": 0,
        "unresolved_links": 0,
        "unresolvedLinks": 0,
        "document_ids": document_ids.clone(),
        "documentIds": document_ids,
        "warnings": warnings,
        "errors": errors,
    })
}

fn local_import_error_response(message: impl Into<String>) -> serde_json::Value {
    local_import_response("error", 0, Vec::new(), Vec::new(), vec![message.into()])
}

fn local_import_success_response(document_id: String, warnings: Vec<String>) -> serde_json::Value {
    local_import_response("complete", 1, vec![document_id], warnings, Vec::new())
}

pub(super) async fn loopback_hosted_import_clip(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    AxumPath(graph_id): AxumPath<String>,
    Json(body): Json<ClipUrlRequest>,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "imports.web.write") {
        return response;
    }
    if let Err(error) = existing_graph_dir(&state.app, &graph_id) {
        return loopback_error(StatusCode::NOT_FOUND, &error);
    }

    let url = match validate_http_url(&body.url) {
        Ok(url) => url,
        Err(error) => return Json(local_import_error_response(error)).into_response(),
    };
    let provided_html = body
        .html
        .as_deref()
        .map(str::trim)
        .filter(|html| !html.is_empty())
        .map(str::to_string);
    let (bytes, content_type) = match provided_html {
        Some(html) => (html.into_bytes(), "text/html".to_string()),
        None => {
            let client = match local_web_fetch_client() {
                Ok(client) => client,
                Err(error) => return loopback_error(StatusCode::INTERNAL_SERVER_ERROR, &error),
            };
            match fetch_limited_url_bytes(&client, url.clone(), LOCAL_WEB_CLIP_MAX_BYTES).await {
                Ok(result) => result,
                Err(error) => return Json(local_import_error_response(error)).into_response(),
            }
        }
    };
    let normalized_content_type = content_type.to_ascii_lowercase();
    if !normalized_content_type.contains("text/html")
        && !normalized_content_type.contains("application/xhtml")
    {
        return Json(local_import_error_response(format!(
            "URL returned non-HTML content type: {content_type}"
        )))
        .into_response();
    }

    let wait_for_flush = prefers_synchronous_flush(&headers);
    let html = String::from_utf8_lossy(&bytes).to_string();
    let document_id = format!("doc-{}", Uuid::new_v4().simple());
    let payload = serde_json::json!({
        "documentId": document_id.clone(),
        "url": url.as_str(),
        "html": html,
        "title": trim_optional(body.title),
        "folderId": trim_optional(body.folder_id),
        "contentType": content_type,
    });
    match enqueue_crdt_operation_outcome(
        state.app.clone(),
        EnqueueCrdtOperationInput {
            kind: "import.webClip".to_string(),
            graph_id: graph_id.clone(),
            document_id: Some(document_id.clone()),
            payload,
        },
    )
    .await
    {
        Ok(outcome) => {
            if wait_for_flush {
                match flush_document_projection_phase(
                    state.app.clone(),
                    &graph_id,
                    &document_id,
                    &outcome.operation_id,
                    "webClipWorkspaceFlushMs",
                )
                .await
                {
                    Ok(()) => Json(outcome.value).into_response(),
                    Err(error) => Json(local_import_error_response(format!(
                        "Failed to materialize document: {error}"
                    )))
                    .into_response(),
                }
            } else {
                spawn_deferred_document_projection_flush(
                    state.app.clone(),
                    graph_id,
                    document_id,
                    "imports.web.clip",
                );
                Json(outcome.value).into_response()
            }
        }
        Err(error) => Json(local_import_error_response(format!(
            "Failed to create document: {error}"
        )))
        .into_response(),
    }
}

pub(super) async fn loopback_hosted_import_youtube(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    AxumPath(graph_id): AxumPath<String>,
    Json(body): Json<YoutubeTranscriptRequest>,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "imports.web.write") {
        return response;
    }
    if let Err(error) = existing_graph_dir(&state.app, &graph_id) {
        return loopback_error(StatusCode::NOT_FOUND, &error);
    }

    let (markdown, title, video_id) = match fetch_youtube_transcript_markdown(&body).await {
        Ok(value) => value,
        Err(error) => {
            return Json(local_import_error_response(format!(
                "Failed to fetch transcript: {error}"
            )))
            .into_response()
        }
    };
    let wait_for_flush = prefers_synchronous_flush(&headers);
    let document_id = format!("doc-{}", Uuid::new_v4().simple());
    let payload = serde_json::json!({
        "documentId": document_id.clone(),
        "title": title,
        "content": markdown,
        "format": "markdown",
        "parentId": trim_optional(body.folder_id),
        "readOnly": false,
    });
    let success_warnings = vec![
        YOUTUBE_NOTICE.to_string(),
        format!("Imported local YouTube transcript for video {video_id}."),
    ];
    match enqueue_crdt_operation_outcome(
        state.app.clone(),
        EnqueueCrdtOperationInput {
            kind: "document.write".to_string(),
            graph_id: graph_id.clone(),
            document_id: Some(document_id.clone()),
            payload,
        },
    )
    .await
    {
        Ok(outcome) => {
            if wait_for_flush {
                match flush_document_projection_phase(
                    state.app.clone(),
                    &graph_id,
                    &document_id,
                    &outcome.operation_id,
                    "youtubeImportWorkspaceFlushMs",
                )
                .await
                {
                    Ok(()) => Json(local_import_success_response(document_id, success_warnings))
                        .into_response(),
                    Err(error) => Json(local_import_error_response(format!(
                        "Failed to materialize document: {error}"
                    )))
                    .into_response(),
                }
            } else {
                spawn_deferred_document_projection_flush(
                    state.app.clone(),
                    graph_id,
                    document_id.clone(),
                    "imports.web.youtube",
                );
                Json(local_import_success_response(document_id, success_warnings)).into_response()
            }
        }
        Err(error) => Json(local_import_error_response(format!(
            "Failed to create document: {error}"
        )))
        .into_response(),
    }
}
