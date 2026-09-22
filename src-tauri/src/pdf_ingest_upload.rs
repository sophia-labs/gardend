use crate::{
    ids::{extension_for_filename, safe_filename},
    multipart_pending_upload::stream_field_to_pending_upload,
    paths::cleanup_pending_upload_file,
    runtime_config::LOCAL_UPLOAD_MAX_BYTES,
};
use axum::{extract::Multipart, http::StatusCode};
use std::path::{Path, PathBuf};

pub(crate) struct PdfAccurateUpload {
    pub(crate) filename: String,
    pub(crate) mime_type: String,
    pub(crate) pending_original_path: PathBuf,
    pub(crate) size_bytes: usize,
    pub(crate) parent_id: Option<String>,
    pub(crate) title: Option<String>,
}

pub(crate) struct PdfIngestUploadError {
    pub(crate) status: StatusCode,
    pub(crate) message: String,
}

impl PdfIngestUploadError {
    fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }
}

pub(crate) async fn read_pdf_accurate_upload(
    graph_dir: &Path,
    mut multipart: Multipart,
) -> Result<PdfAccurateUpload, PdfIngestUploadError> {
    let mut filename: Option<String> = None;
    let mut mime_type: Option<String> = None;
    let mut pending_original_path: Option<PathBuf> = None;
    let mut size_bytes: usize = 0;
    let mut parent_id: Option<String> = None;
    let mut title: Option<String> = None;

    loop {
        let field = match multipart.next_field().await {
            Ok(Some(field)) => field,
            Ok(None) => break,
            Err(error) => {
                return Err(PdfIngestUploadError::new(
                    StatusCode::BAD_REQUEST,
                    format!("invalid multipart PDF upload: {error}"),
                ))
            }
        };

        let name = field.name().map(str::to_string).unwrap_or_default();
        if name == "file" {
            let field_filename = field
                .file_name()
                .map(safe_filename)
                .unwrap_or_else(|| "document.pdf".to_string());
            let field_mime_type = field.content_type().map(ToString::to_string);
            let upload = stream_field_to_pending_upload(
                graph_dir,
                field,
                LOCAL_UPLOAD_MAX_BYTES,
                "failed to read uploaded PDF",
            )
            .await
            .map_err(|error| PdfIngestUploadError::new(error.status, error.message))?;
            if let Some(previous_path) = pending_original_path.replace(upload.path) {
                cleanup_pending_upload_file(&previous_path);
            }
            filename = Some(field_filename);
            mime_type = field_mime_type;
            size_bytes = upload.bytes_written;
            continue;
        }

        let text = field.text().await.map_err(|error| {
            PdfIngestUploadError::new(
                StatusCode::BAD_REQUEST,
                format!("failed to read multipart field {name}: {error}"),
            )
        })?;
        let text = text.trim().to_string();
        if text.is_empty() {
            continue;
        }
        match name.as_str() {
            "parent_id" | "parentId" => parent_id = Some(text),
            "title" | "titleOverride" | "title_override" => title = Some(text),
            _ => {}
        }
    }

    let Some(pending_original_path) = pending_original_path else {
        return Err(PdfIngestUploadError::new(
            StatusCode::BAD_REQUEST,
            "missing multipart file field",
        ));
    };
    if size_bytes == 0 {
        cleanup_pending_upload_file(&pending_original_path);
        return Err(PdfIngestUploadError::new(
            StatusCode::BAD_REQUEST,
            "Empty file uploaded.",
        ));
    }

    let filename = filename.unwrap_or_else(|| "document.pdf".to_string());
    if extension_for_filename(&filename).as_deref() != Some("pdf") {
        cleanup_pending_upload_file(&pending_original_path);
        return Err(PdfIngestUploadError::new(
            StatusCode::BAD_REQUEST,
            "Only PDF files are accepted by the accurate ingestion path.",
        ));
    }

    Ok(PdfAccurateUpload {
        filename,
        mime_type: mime_type.unwrap_or_else(|| "application/pdf".to_string()),
        pending_original_path,
        size_bytes,
        parent_id,
        title,
    })
}
