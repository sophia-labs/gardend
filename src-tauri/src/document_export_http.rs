use crate::{
    document_export_rendering::{document_html, document_json, document_markdown, document_xml},
    document_service::DocumentRecord,
    ids::{content_disposition_filename, safe_filename},
};
use axum::{
    body::Body,
    http::{header, StatusCode},
    response::Response,
};

pub(super) fn document_export_response(
    document: &DocumentRecord,
    export_format: &str,
    theme: Option<&str>,
) -> Result<Response, String> {
    let (extension, content_type, content) = match export_format {
        "json" => (
            "json",
            "application/json; charset=utf-8",
            document_json(document)?,
        ),
        "xml" => ("xml", "application/xml", document_xml(document)),
        "markdown" => (
            "md",
            "text/markdown; charset=utf-8",
            document_markdown(document),
        ),
        "html" => (
            "html",
            "text/html; charset=utf-8",
            document_html(document, theme),
        ),
        other => {
            return Err(format!(
                "Invalid format '{other}'. Must be one of: html, json, markdown, xml"
            ))
        }
    };
    let title = if document.title.trim().is_empty() {
        "Untitled"
    } else {
        document.title.as_str()
    };
    let filename = format!("{}.{}", safe_filename(title), extension);
    let bytes = content.into_bytes();
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, content_type)
        .header(header::CONTENT_LENGTH, bytes.len().to_string())
        .header(
            header::CONTENT_DISPOSITION,
            format!(
                "attachment; filename=\"{}\"",
                content_disposition_filename(&filename)
            ),
        )
        .body(Body::from(bytes))
        .map_err(|error| format!("build document export response: {error}"))
}
