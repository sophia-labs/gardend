use crate::app_runtime::AppHandle;
use crate::{
    document_service::{read_document, read_workspace_record},
    document_sidecar_store::{canonicalize_ydoc_update_bytes, empty_ydoc_update_v1},
    paths::{document_ydoc_state_path, existing_graph_dir, workspace_ydoc_state_path},
    storage::read_bytes,
};
use axum::{
    body::Body,
    http::{header, HeaderName, HeaderValue, StatusCode},
    response::Response,
};
use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _};
use std::path::Path;

pub(super) fn ydoc_blob_response(bytes: Vec<u8>, source: &str) -> Result<Response, String> {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(header::CONTENT_LENGTH, bytes.len().to_string())
        .header("x-blob-source", source)
        .body(Body::from(bytes))
        .map_err(|error| format!("build Y.Doc blob response: {error}"))
}

pub(super) fn read_ydoc_blob(
    state_path: &Path,
    fallback_base64: &str,
    label: &str,
) -> Result<(Vec<u8>, &'static str), String> {
    if state_path.is_file() {
        let bytes = canonicalize_ydoc_update_bytes(read_bytes(state_path)?);
        return Ok((bytes, "local-file"));
    }

    if !fallback_base64.trim().is_empty() {
        let bytes = canonicalize_ydoc_update_bytes(
            BASE64_STANDARD
                .decode(fallback_base64)
                .map_err(|error| format!("decode {label}: {error}"))?,
        );
        return Ok((bytes, "manifest"));
    }

    // The caller has already established that the graph/document exists.
    // Older profiles represented an untouched Y.Doc as no sidecar plus an
    // empty manifest field; expose that state as a real empty Yjs update.
    Ok((empty_ydoc_update_v1(), "implicit-empty"))
}

pub(super) fn hosted_workspace_blob_response(
    app: &AppHandle,
    graph_id: &str,
) -> Result<Response, String> {
    let graph_dir = existing_graph_dir(app, graph_id)?;
    let workspace = read_workspace_record(&graph_dir, graph_id)?;
    let (bytes, source) = read_ydoc_blob(
        &workspace_ydoc_state_path(&graph_dir),
        &workspace.ydoc_update_base64,
        "workspace Y.Doc blob",
    )?;
    let incarnation = crate::graph_record_store::ensure_graph_incarnation(app, graph_id)
        .map_err(crate::app_error::AppError::message)?;
    let mut response = ydoc_blob_response(bytes, source)?;
    response.headers_mut().insert(
        HeaderName::from_static(crate::graph_record_store::GRAPH_INCARNATION_HEADER),
        HeaderValue::from_str(&incarnation)
            .map_err(|error| format!("encode graph incarnation header: {error}"))?,
    );
    Ok(response)
}

pub(super) fn hosted_document_blob_response(
    app: &AppHandle,
    graph_id: &str,
    document_id: &str,
) -> Result<Response, String> {
    let graph_dir = existing_graph_dir(app, graph_id)?;
    let document = read_document(app.clone(), graph_id.to_string(), document_id.to_string())?;
    let (bytes, source) = read_ydoc_blob(
        &document_ydoc_state_path(&graph_dir, document_id),
        &document.ydoc_update_base64,
        "document Y.Doc blob",
    )?;
    let incarnation =
        crate::document_incarnation_store::ensure_document_incarnation_id(&graph_dir, document_id)?;
    let mut response = ydoc_blob_response(bytes, source)?;
    response.headers_mut().insert(
        HeaderName::from_static(crate::document_incarnation_store::DOCUMENT_INCARNATION_HEADER),
        HeaderValue::from_str(&incarnation)
            .map_err(|error| format!("encode document incarnation header: {error}"))?,
    );
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use uuid::Uuid;

    #[test]
    fn read_ydoc_blob_prefers_sidecar_file_over_manifest_fallback() {
        let dir = std::env::temp_dir().join(format!("sophia-ydoc-blob-{}", Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let state_path = dir.join("update-v1.bin");
        fs::write(&state_path, b"sidecar").unwrap();

        let (bytes, source) = read_ydoc_blob(
            &state_path,
            &BASE64_STANDARD.encode(b"manifest"),
            "test blob",
        )
        .unwrap();

        assert_eq!(bytes, b"sidecar");
        assert_eq!(source, "local-file");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn read_ydoc_blob_normalizes_legacy_empty_state() {
        use yrs::{updates::decoder::Decode, Update};

        let dir = std::env::temp_dir().join(format!("sophia-empty-ydoc-blob-{}", Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let state_path = dir.join("update-v1.bin");
        fs::write(&state_path, []).unwrap();

        let (bytes, source) = read_ydoc_blob(&state_path, "", "test blob").unwrap();

        assert!(!bytes.is_empty());
        Update::decode_v1(&bytes).expect("blob boundary returns a valid Yjs update");
        assert_eq!(source, "local-file");
        fs::remove_dir_all(dir).unwrap();
    }
}
