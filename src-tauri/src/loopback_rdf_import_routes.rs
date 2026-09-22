use crate::{
    clock::epoch_millis,
    loopback_http::{loopback_app_error, loopback_error, require_loopback_scope},
    loopback_import_job_response::import_job_accepted_response,
    loopback_state::LoopbackState,
    multipart_pending_upload::stream_field_to_pending_upload,
    paths::{cleanup_pending_upload_file, existing_graph_dir},
    rdf_import_service::{detect_rdf_import_mime, import_rdf_into_graph},
    runtime_config::LOCAL_UPLOAD_MAX_BYTES,
    storage::read_bytes,
};
use axum::{
    extract::{Multipart, Path as AxumPath, State},
    http::{HeaderMap, StatusCode},
    response::Response,
};
use std::{path::PathBuf, sync::Arc};

pub(super) async fn loopback_hosted_import_rdf(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    AxumPath(graph_id): AxumPath<String>,
    mut multipart: Multipart,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "rdf.load") {
        return response;
    }
    let graph_dir = match existing_graph_dir(&state.app, &graph_id) {
        Ok(graph_dir) => graph_dir,
        Err(error) => return loopback_error(StatusCode::NOT_FOUND, &error),
    };

    let mut filename: Option<String> = None;
    let mut format_override: Option<String> = None;
    let mut pending_path: Option<PathBuf> = None;
    let mut size_bytes: usize = 0;

    loop {
        let field = match multipart.next_field().await {
            Ok(Some(field)) => field,
            Ok(None) => break,
            Err(error) => {
                return loopback_error(
                    StatusCode::BAD_REQUEST,
                    &format!("invalid multipart import: {error}"),
                )
            }
        };

        let name = field.name().map(str::to_string).unwrap_or_default();
        if name == "file" {
            let field_filename = field
                .file_name()
                .map(str::to_string)
                .unwrap_or_else(|| "import.rdf".to_string());
            let upload = match stream_field_to_pending_upload(
                &graph_dir,
                field,
                LOCAL_UPLOAD_MAX_BYTES,
                "failed to read RDF import file",
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
            size_bytes = upload.bytes_written;
            continue;
        }

        let text = match field.text().await {
            Ok(value) => value.trim().to_string(),
            Err(error) => {
                return loopback_error(
                    StatusCode::BAD_REQUEST,
                    &format!("failed to read multipart field {name}: {error}"),
                )
            }
        };
        if text.is_empty() {
            continue;
        }
        if name == "format" {
            format_override = Some(text);
        }
    }

    let Some(pending_path) = pending_path else {
        return loopback_error(StatusCode::BAD_REQUEST, "missing multipart file field");
    };
    if size_bytes == 0 {
        cleanup_pending_upload_file(&pending_path);
        return loopback_error(StatusCode::BAD_REQUEST, "Empty file uploaded");
    }
    let filename = filename.unwrap_or_else(|| "import.rdf".to_string());
    let mime_type = match detect_rdf_import_mime(&filename, format_override) {
        Ok(mime_type) => mime_type,
        Err(error) => {
            cleanup_pending_upload_file(&pending_path);
            return loopback_error(StatusCode::BAD_REQUEST, &error);
        }
    };
    let bytes = match read_bytes(&pending_path) {
        Ok(bytes) => bytes,
        Err(error) => {
            cleanup_pending_upload_file(&pending_path);
            return loopback_app_error(error);
        }
    };

    let started_at_ms = epoch_millis();
    let result = import_rdf_into_graph(
        state.app.clone(),
        graph_id.clone(),
        filename,
        mime_type,
        bytes,
    );
    cleanup_pending_upload_file(&pending_path);
    let record = match state.jobs.insert_finished(
        "import_rdf",
        Some(graph_id),
        started_at_ms,
        result,
        "application/json",
    ) {
        Ok(record) => record,
        Err(error) => return loopback_app_error(error),
    };
    import_job_accepted_response(&record)
}
