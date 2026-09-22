use crate::paths::{create_pending_upload_file_writer, PendingUploadWriteError};
use axum::{extract::multipart::Field, http::StatusCode};
use std::path::{Path, PathBuf};

pub(crate) struct MultipartPendingUpload {
    pub(crate) path: PathBuf,
    pub(crate) bytes_written: usize,
}

pub(crate) struct MultipartPendingUploadError {
    pub(crate) status: StatusCode,
    pub(crate) message: String,
}

impl MultipartPendingUploadError {
    fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }
}

pub(crate) async fn stream_field_to_pending_upload(
    graph_dir: &Path,
    mut field: Field<'_>,
    max_bytes: usize,
    read_error_context: &str,
) -> Result<MultipartPendingUpload, MultipartPendingUploadError> {
    let mut writer = create_pending_upload_file_writer(graph_dir).map_err(|error| {
        MultipartPendingUploadError::new(StatusCode::INTERNAL_SERVER_ERROR, error)
    })?;

    while let Some(chunk) = field.chunk().await.map_err(|error| {
        MultipartPendingUploadError::new(
            StatusCode::BAD_REQUEST,
            format!("{read_error_context}: {error}"),
        )
    })? {
        writer.write_chunk(&chunk, max_bytes).map_err(|error| {
            let status = match error {
                PendingUploadWriteError::TooLarge { .. } => StatusCode::PAYLOAD_TOO_LARGE,
                PendingUploadWriteError::Write(_) => StatusCode::INTERNAL_SERVER_ERROR,
            };
            MultipartPendingUploadError::new(status, error.message())
        })?;
    }

    let (path, bytes_written) = writer.finish().map_err(|error| {
        MultipartPendingUploadError::new(StatusCode::INTERNAL_SERVER_ERROR, error)
    })?;

    Ok(MultipartPendingUpload {
        path,
        bytes_written,
    })
}
