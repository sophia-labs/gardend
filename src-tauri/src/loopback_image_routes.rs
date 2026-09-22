use crate::{
    app_error::AppError,
    ids::{normalize_image_mime_type, safe_filename, url_component},
    loopback_http::{
        bearer_token, loopback_error, loopback_original_file_error, loopback_token_has_scope,
        origin_ok, require_loopback_scope,
    },
    loopback_state::LoopbackState,
    multipart_pending_upload::stream_field_to_pending_upload,
    original_file_service::{
        adopt_pending_image_file, image_access_token_matches, image_file_response, read_image_file,
        write_image_access_token,
    },
    paths::{cleanup_pending_upload_file, existing_graph_dir},
    runtime_config::LOCAL_IMAGE_UPLOAD_MAX_BYTES,
};
use axum::{
    extract::{Multipart, Path as AxumPath, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde::Serialize;
use std::{collections::BTreeMap, path::PathBuf, sync::Arc};
use uuid::Uuid;

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ImageUploadResponse {
    image_id: String,
    src: String,
}

pub(super) async fn loopback_hosted_upload_image(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    AxumPath(graph_id): AxumPath<String>,
    mut multipart: Multipart,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "images.write") {
        return response;
    }

    let mut filename: Option<String> = None;
    let mut mime_type: Option<String> = None;
    let graph_dir = match existing_graph_dir(&state.app, &graph_id) {
        Ok(graph_dir) => graph_dir,
        Err(error) => return loopback_error(StatusCode::NOT_FOUND, &error),
    };
    let mut pending_path: Option<PathBuf> = None;
    let mut size_bytes: usize = 0;

    loop {
        let field = match multipart.next_field().await {
            Ok(Some(field)) => field,
            Ok(None) => break,
            Err(error) => {
                return loopback_error(
                    StatusCode::BAD_REQUEST,
                    &format!("invalid multipart image upload: {error}"),
                )
            }
        };

        let name = field.name().map(str::to_string).unwrap_or_default();
        if name != "file" {
            continue;
        }

        let field_filename = field
            .file_name()
            .map(str::to_string)
            .unwrap_or_else(|| "image".to_string());
        let field_mime_type = field.content_type().map(ToString::to_string);
        let upload = match stream_field_to_pending_upload(
            &graph_dir,
            field,
            LOCAL_IMAGE_UPLOAD_MAX_BYTES,
            "failed to read uploaded image",
        )
        .await
        {
            Ok(upload) => upload,
            Err(error) => return loopback_error(error.status, &error.message),
        };
        if let Some(previous_path) = pending_path.replace(upload.path) {
            cleanup_pending_upload_file(&previous_path);
        }
        filename = Some(field_filename);
        mime_type = field_mime_type;
        size_bytes = upload.bytes_written;
    }

    let Some(pending_path) = pending_path else {
        return loopback_error(StatusCode::BAD_REQUEST, "missing multipart file field");
    };
    if size_bytes == 0 {
        cleanup_pending_upload_file(&pending_path);
        return loopback_error(StatusCode::BAD_REQUEST, "Empty image uploaded");
    }

    let filename = filename.unwrap_or_else(|| "image".to_string());
    let mime_type = match normalize_image_mime_type(mime_type.as_deref(), &filename) {
        Ok(mime_type) => mime_type,
        Err(error) => {
            cleanup_pending_upload_file(&pending_path);
            return loopback_error(StatusCode::UNPROCESSABLE_ENTITY, &error);
        }
    };
    let image_id = Uuid::new_v4().to_string();
    let manifest = match adopt_pending_image_file(
        &state.app,
        &graph_id,
        &image_id,
        &filename,
        &mime_type,
        &pending_path,
    ) {
        Ok(manifest) => manifest,
        Err(error) => {
            cleanup_pending_upload_file(&pending_path);
            return loopback_error(StatusCode::BAD_REQUEST, error.message_ref());
        }
    };
    let access_token = match write_image_access_token(&state.app, &graph_id, &image_id) {
        Ok(token) => token,
        Err(error) => return loopback_error(StatusCode::INTERNAL_SERVER_ERROR, &error),
    };
    // Behind a gateway, path-only srcs resolve against the proxy origin and
    // lose the /g/{graph} prefix — prefix with the public base when set.
    let public_prefix = std::env::var("GARDEN_PUBLIC_BASE_URL")
        .ok()
        .map(|base| base.trim().trim_end_matches('/').to_string())
        .filter(|base| !base.is_empty())
        .unwrap_or_default();
    let src = format!(
        "{}/artifacts/{}/images/{}?token={}&exp={}&fn={}",
        public_prefix,
        url_component(&graph_id),
        url_component(&image_id),
        url_component(&access_token.token),
        url_component(&access_token.expires_at),
        url_component(&manifest.filename),
    );

    (
        StatusCode::CREATED,
        Json(ImageUploadResponse { image_id, src }),
    )
        .into_response()
}

pub(super) async fn loopback_hosted_serve_image(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    AxumPath((graph_id, image_id)): AxumPath<(String, String)>,
    Query(params): Query<BTreeMap<String, String>>,
) -> Response {
    if !origin_ok(&headers) {
        return loopback_error(StatusCode::FORBIDDEN, "invalid origin");
    }
    if bearer_token(&headers).is_some() {
        match loopback_token_has_scope(&headers, &state, "images.read") {
            Ok(true) => {}
            Ok(false) => {
                return loopback_error(
                    StatusCode::FORBIDDEN,
                    "loopback token missing required scope images.read",
                )
            }
            Err(response) => return response,
        }
    } else {
        match image_access_token_matches(
            &state.app,
            &graph_id,
            &image_id,
            params.get("token"),
            params.get("exp").or_else(|| params.get("expiresAt")),
        ) {
            Ok(true) => {}
            Ok(false) => {
                return loopback_error(
                    StatusCode::UNAUTHORIZED,
                    "missing or invalid image access token",
                )
            }
            Err(error) => return loopback_original_file_error(&error),
        }
    }

    match read_image_file(&state.app, &graph_id, &image_id).and_then(|(manifest, bytes)| {
        if let Some(filename) = params.get("fn") {
            if safe_filename(filename) != manifest.filename {
                return Err(AppError::not_found(format!("image {image_id} not found")));
            }
        }
        image_file_response(&manifest, bytes).map_err(AppError::storage)
    }) {
        Ok(response) => response,
        Err(error) => loopback_original_file_error(error.message_ref()),
    }
}
