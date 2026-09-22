use crate::app_runtime::AppHandle;
use crate::{
    rdf::parse_rdf_format,
    rdf_service::{load_rdf, RdfLoadInput},
};
use std::path::Path;

fn rdf_import_mime_for_extension(filename: &str) -> Option<&'static str> {
    match Path::new(filename)
        .extension()
        .and_then(|value| value.to_str())
        .map(|value| value.to_ascii_lowercase())
        .as_deref()
    {
        Some("ttl") => Some("text/turtle"),
        Some("nt") => Some("application/n-triples"),
        Some("nq") => Some("application/n-quads"),
        Some("rdf") | Some("xml") => Some("application/rdf+xml"),
        Some("trig") => Some("application/trig"),
        Some("n3") => Some("text/n3"),
        Some("jsonld") => Some("application/ld+json"),
        _ => None,
    }
}

pub(super) fn detect_rdf_import_mime(
    filename: &str,
    format_override: Option<String>,
) -> Result<String, String> {
    if let Some(format_override) = format_override
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
    {
        parse_rdf_format(&format_override)?;
        return Ok(format_override);
    }

    let Some(mime_type) = rdf_import_mime_for_extension(filename) else {
        return Err(format!(
            "Cannot detect RDF format from filename '{filename}'. Supported extensions: .jsonld, .n3, .nq, .nt, .rdf, .trig, .ttl, .xml. Or provide the 'format' field with a supported MIME type."
        ));
    };
    parse_rdf_format(mime_type)?;
    Ok(mime_type.to_string())
}

pub(super) fn import_rdf_into_graph(
    app: AppHandle,
    graph_id: String,
    filename: String,
    mime_type: String,
    bytes: Vec<u8>,
) -> Result<serde_json::Value, String> {
    let data = rdf_import_utf8_data(bytes)?;
    let result = load_rdf(
        app,
        RdfLoadInput {
            graph_id: graph_id.clone(),
            data,
            format: mime_type.clone(),
            base_iri: None,
            target_graph_iri: None,
        },
    )?;

    Ok(serde_json::json!({
        "graph_id": graph_id.clone(),
        "graphId": graph_id,
        "filename": filename,
        "mime_type": mime_type.clone(),
        "mimeType": mime_type,
        "quad_count": result.quad_count,
        "quadCount": result.quad_count,
    }))
}

fn rdf_import_utf8_data(bytes: Vec<u8>) -> Result<String, String> {
    String::from_utf8(bytes).map_err(|error| format!("RDF import file is not UTF-8 text: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rdf_import_mime_detection_uses_supported_extensions_case_insensitively() {
        assert_eq!(
            rdf_import_mime_for_extension("graph.TTL"),
            Some("text/turtle")
        );
        assert_eq!(
            rdf_import_mime_for_extension("graph.JSONLD"),
            Some("application/ld+json")
        );
        assert_eq!(
            rdf_import_mime_for_extension("graph.rdf"),
            Some("application/rdf+xml")
        );
        assert_eq!(
            rdf_import_mime_for_extension("graph.xml"),
            Some("application/rdf+xml")
        );
        assert_eq!(rdf_import_mime_for_extension("graph.txt"), None);
    }

    #[test]
    fn rdf_import_format_override_is_trimmed_and_validated() {
        assert_eq!(
            detect_rdf_import_mime("graph.unknown", Some(" text/turtle ".to_string())).unwrap(),
            "text/turtle"
        );

        let error = detect_rdf_import_mime("graph.unknown", Some("unknown-format".to_string()))
            .unwrap_err();
        assert!(error.contains("unsupported RDF format"));
    }

    #[test]
    fn rdf_import_detection_reports_supported_extensions() {
        let error = detect_rdf_import_mime("graph.txt", None).unwrap_err();

        assert!(error.contains("Cannot detect RDF format"));
        assert!(error.contains(".jsonld"));
        assert!(error.contains("format"));
    }

    #[test]
    fn rdf_import_rejects_non_utf8_payload_before_store_load() {
        let error = rdf_import_utf8_data(vec![0xff]).unwrap_err();
        assert!(error.contains("RDF import file is not UTF-8 text"));
    }
}
