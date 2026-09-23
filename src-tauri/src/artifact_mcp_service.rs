//! MCP tool handlers for the artifact lifecycle — perceive, type, version, and
//! edit — so the local agent can work with artifacts, not just enumerate them.
//!
//! - `read_artifact`     — perceive an artifact (image → bytes for the agent's
//!                          vision model; text → text; else metadata)
//! - `list_artifact_kinds` — the kind→capability registry (perceive/render/derive)
//! - `list_artifact_revisions` / `restore_artifact_revision` — version control
//! - `edit_artifact_image` — edit/generate via an image model + save as a revision

use crate::app_runtime::AppHandle;
use crate::{
    app_error::{AppError, AppResult},
    artifact_kinds::{kind_from_mime, ARTIFACT_KINDS},
    artifact_revisions::{
        create_artifact_revision, list_artifact_revisions, restore_artifact_revision,
    },
    local_provider_keys::provider_key,
    mcp_utils::{mcp_arg_string, mcp_required_graph_id},
    original_file_service::read_artifact_original_file,
};
use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _};
use serde_json::{json, Value};

pub(crate) const IMAGE_MODEL: &str = "google/gemini-2.5-flash-image";

/// Bound on one image-model round trip; generation is slow but never unbounded.
const IMAGE_REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(180);

fn required_artifact_id(args: &Value) -> AppResult<String> {
    mcp_arg_string(args, &["artifactId", "artifact_id"])
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| AppError::validation("artifact_id is required"))
}

fn graph_id(args: &Value) -> AppResult<String> {
    mcp_required_graph_id(args).map_err(AppError::validation)
}

/// Perceive an artifact. Images return base64 bytes (the frontend tool wrapper
/// turns this into an image block for the agent's vision model); text returns
/// decoded text; everything else returns metadata.
pub(super) async fn mcp_local_read_artifact(app: AppHandle, args: &Value) -> AppResult<Value> {
    let graph_id = graph_id(args)?;
    let artifact_id = required_artifact_id(args)?;
    if args.get("strict_utf8").is_some_and(|v| !v.is_boolean()) {
        return Err(AppError::validation("strict_utf8 must be boolean"));
    }
    if args.get("strict_utf8").and_then(Value::as_bool) == Some(true) {
        return crate::artifact_text_service::read_text(&app, &graph_id, &artifact_id).await;
    }
    let (manifest, bytes) = read_artifact_original_file(&app, &graph_id, &artifact_id)?;
    let kind = kind_from_mime(&manifest.mime_type, None);
    let base = json!({
        "artifactId": artifact_id,
        "kind": kind,
        "mimeType": manifest.mime_type,
        "filename": manifest.filename,
        "sizeBytes": bytes.len(),
    });
    let mut out = base.as_object().cloned().unwrap_or_default();
    match kind {
        "image" => {
            out.insert("dataBase64".into(), json!(BASE64_STANDARD.encode(&bytes)));
        }
        // Text and scenes (Excalidraw JSON) return their content as text so the
        // agent can read it structurally.
        "text" | "scene" => {
            out.insert("text".into(), json!(String::from_utf8_lossy(&bytes)));
        }
        _ => {
            out.insert("note".into(), json!("binary artifact; no inline preview"));
        }
    }
    Ok(Value::Object(out))
}

/// The artifact-kind registry: each kind's RDF class + perceive/render/derive
/// capabilities. Lets the agent discover what it can do with each kind.
pub(super) fn mcp_local_list_artifact_kinds(_app: AppHandle, _args: &Value) -> AppResult<Value> {
    let kinds: Vec<Value> = ARTIFACT_KINDS
        .iter()
        .map(|kind| {
            json!({
                "kind": kind.slug,
                "rdfClass": kind.class,
                "perceive": kind.perceive,
                "render": kind.render,
                "derive": kind.derive,
            })
        })
        .collect();
    Ok(json!({ "kinds": kinds }))
}

pub(super) fn mcp_local_list_artifact_revisions(app: AppHandle, args: &Value) -> AppResult<Value> {
    let graph_id = graph_id(args)?;
    let artifact_id = required_artifact_id(args)?;
    let revisions = list_artifact_revisions(&app, &graph_id, &artifact_id)?;
    Ok(json!({ "revisions": revisions }))
}

pub(super) fn mcp_local_restore_artifact_revision(
    app: AppHandle,
    args: &Value,
) -> AppResult<Value> {
    let graph_id = graph_id(args)?;
    let artifact_id = required_artifact_id(args)?;
    let revision_id = mcp_arg_string(args, &["revisionId", "revision_id"])
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| AppError::validation("revision_id is required"))?;
    let checkpoint = restore_artifact_revision(&app, &graph_id, &artifact_id, &revision_id)?;
    Ok(json!({ "restored": revision_id, "checkpoint": checkpoint }))
}

/// Edit/generate an image artifact from a prompt via an OpenRouter image model,
/// saving the result as a new revision. The agent supplies a natural-language
/// instruction; the current image + instruction go to the model server-side.
pub(super) async fn mcp_local_edit_artifact_image(
    app: AppHandle,
    args: &Value,
) -> AppResult<Value> {
    let graph_id = graph_id(args)?;
    let artifact_id = required_artifact_id(args)?;
    let dir = crate::paths::existing_graph_dir(&app, &graph_id).map_err(AppError::storage)?;
    crate::artifact_text_service::refuse_legacy_writer(&dir, &artifact_id)?;
    let prompt = mcp_arg_string(args, &["prompt", "instruction"])
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| AppError::validation("prompt is required"))?;

    let key = provider_key(&app, "openrouter").ok_or_else(|| {
        AppError::validation(
            "No OpenRouter API key: set SOPHIA_OPENROUTER_API_KEY, or the macOS Keychain item dev.sophia.garden/openrouter.",
        )
    })?;
    let (manifest, bytes) = read_artifact_original_file(&app, &graph_id, &artifact_id)?;
    let (out_mime, out_b64) =
        openrouter_image(&key, &prompt, Some((&manifest.mime_type, &bytes))).await?;

    let entry = create_artifact_revision(
        &app,
        &graph_id,
        &artifact_id,
        &manifest.filename,
        &out_mime,
        &out_b64,
        Some(format!(
            "AI: {}",
            prompt.chars().take(60).collect::<String>()
        )),
    )?;
    Ok(json!({ "artifactId": artifact_id, "revision": entry }))
}

/// One OpenRouter image-model call: a text prompt, optionally with an input
/// image to edit. Returns the output image as `(mime, base64)`. Shared by
/// `edit_artifact_image` and `agent_self_image`.
pub(crate) async fn openrouter_image(
    key: &str,
    prompt: &str,
    input: Option<(&str, &[u8])>,
) -> AppResult<(String, String)> {
    let mut content = vec![json!({ "type": "text", "text": prompt })];
    if let Some((mime, bytes)) = input {
        content.push(json!({
            "type": "image_url",
            "image_url": { "url": format!("data:{mime};base64,{}", BASE64_STANDARD.encode(bytes)) },
        }));
    }
    let response: Value = reqwest::Client::builder()
        .timeout(IMAGE_REQUEST_TIMEOUT)
        .build()
        .map_err(|error| AppError::internal(format!("image model client: {error}")))?
        .post("https://openrouter.ai/api/v1/chat/completions")
        .bearer_auth(key)
        .json(&json!({
            "model": IMAGE_MODEL,
            "modalities": ["image", "text"],
            "messages": [{ "role": "user", "content": content }],
        }))
        .send()
        .await
        .map_err(|error| AppError::internal(format!("image model request failed: {error}")))?
        .json()
        .await
        .map_err(|error| AppError::internal(format!("image model response not JSON: {error}")))?;

    if let Some(message) = response
        .get("error")
        .and_then(|e| e.get("message"))
        .and_then(Value::as_str)
    {
        return Err(AppError::internal(format!("image model error: {message}")));
    }
    let out_url = response
        .pointer("/choices/0/message/images/0/image_url/url")
        .and_then(Value::as_str)
        .ok_or_else(|| AppError::internal("image model returned no image"))?;
    let (mime, payload) = parse_data_url(out_url)
        .ok_or_else(|| AppError::internal("image model returned a non-data-url image"))?;
    Ok((mime.to_string(), payload.to_string()))
}

/// Split a `data:<mime>;base64,<payload>` URL into (mime, payload).
fn parse_data_url(url: &str) -> Option<(&str, &str)> {
    let rest = url.strip_prefix("data:")?;
    let (mime, payload) = rest.split_once(";base64,")?;
    Some((mime, payload))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_data_urls() {
        assert_eq!(
            parse_data_url("data:image/png;base64,QUJD"),
            Some(("image/png", "QUJD"))
        );
        assert_eq!(parse_data_url("not a data url"), None);
    }
}
