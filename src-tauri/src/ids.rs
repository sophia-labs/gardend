use crate::clock::epoch_millis;

pub(crate) fn normalize_title(title: &str) -> Result<String, String> {
    let title = normalize_stored_title(title)?;
    if title.chars().count() > 96 {
        return Err("title must be 96 characters or fewer".to_string());
    }
    Ok(title)
}

/// Existing/imported titles are data, not newly generated short labels.
/// Keep a finite UTF-8 byte budget while preserving the creation-time policy.
pub(crate) fn normalize_stored_title(title: &str) -> Result<String, String> {
    if title.len() > 64 * 1024 {
        return Err("stored title exceeds 65536 UTF-8 bytes".to_string());
    }
    let title = title.trim();
    if title.is_empty() {
        return Err("title cannot be empty".to_string());
    }
    Ok(title.to_string())
}

pub(crate) fn validate_local_id(value: &str, field: &str) -> Result<(), String> {
    if value.is_empty() {
        return Err(format!("{field} cannot be empty"));
    }
    if value
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || character == '-' || character == '_')
    {
        Ok(())
    } else {
        Err(format!("{field} contains invalid characters"))
    }
}

pub(crate) fn make_graph_id(title: &str) -> String {
    format!("local-{}-{}", slugify(title), epoch_millis())
}

pub(crate) fn make_document_id(title: &str) -> String {
    format!("doc-{}-{}", slugify(title), epoch_millis())
}

fn slugify(value: &str) -> String {
    let slug = value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>()
        .split('-')
        .filter(|part| !part.is_empty())
        .take(6)
        .collect::<Vec<_>>()
        .join("-");

    if slug.is_empty() {
        "item".to_string()
    } else {
        slug
    }
}

pub(crate) fn safe_filename(value: &str) -> String {
    let filename = value
        .chars()
        .map(|character| {
            if character == '/' || character == '\\' || character.is_control() {
                '_'
            } else {
                character
            }
        })
        .collect::<String>()
        .trim()
        .to_string();

    if filename.is_empty() {
        "original.bin".to_string()
    } else {
        filename
    }
}

pub(crate) fn content_disposition_filename(value: &str) -> String {
    safe_filename(value)
        .chars()
        .map(|character| {
            if character == '"' || character == '\\' || character.is_control() {
                '_'
            } else {
                character
            }
        })
        .collect()
}

pub(crate) fn extension_for_filename(filename: &str) -> Option<String> {
    let sanitized = safe_filename(filename);
    let (_, ext) = sanitized.rsplit_once('.')?;
    let ext = ext.trim().to_ascii_lowercase();
    if ext.is_empty() {
        None
    } else {
        Some(ext)
    }
}

pub(crate) fn local_upload_mime_type_for_filename(filename: &str) -> String {
    match extension_for_filename(filename).as_deref() {
        Some("md") | Some("markdown") => "text/markdown",
        Some("html") | Some("htm") => "text/html",
        Some("txt") | Some("text") | Some("log") => "text/plain",
        Some("csv") => "text/csv",
        Some("excalidraw") => "application/vnd.excalidraw+json",
        Some("json") | Some("jsonl") | Some("jsonld") => "application/json",
        Some("xml") => "application/xml",
        Some("yaml") | Some("yml") => "application/yaml",
        Some("pdf") => "application/pdf",
        Some("epub") => "application/epub+zip",
        Some("docx") => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        _ => "application/octet-stream",
    }
    .to_string()
}

pub(crate) fn normalize_image_mime_type(
    declared: Option<&str>,
    filename: &str,
) -> Result<String, String> {
    let declared = declared
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| value.to_ascii_lowercase());
    let ext = extension_for_filename(filename);

    let resolved = match declared.as_deref() {
        Some("image/png") => Some("image/png"),
        Some("image/jpeg") | Some("image/jpg") => Some("image/jpeg"),
        Some("image/gif") => Some("image/gif"),
        Some("image/webp") => Some("image/webp"),
        Some("image/svg+xml") | Some("image/svg") => Some("image/svg+xml"),
        Some("application/octet-stream") | None => match ext.as_deref() {
            Some("png") => Some("image/png"),
            Some("jpg") | Some("jpeg") => Some("image/jpeg"),
            Some("gif") => Some("image/gif"),
            Some("webp") => Some("image/webp"),
            Some("svg") => Some("image/svg+xml"),
            _ => None,
        },
        _ => None,
    };

    resolved.map(ToString::to_string).ok_or_else(|| {
        "Unsupported image type. Supported locally: PNG, JPG, GIF, WebP, SVG".to_string()
    })
}

pub(crate) fn url_component(value: &str) -> String {
    let mut output = String::new();
    for byte in value.as_bytes() {
        match *byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                output.push(*byte as char)
            }
            _ => output.push_str(&format!("%{byte:02X}")),
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stored_titles_preserve_long_unicode_without_widening_creation_policy() {
        let title = "題".repeat(200);
        assert_eq!(normalize_stored_title(&title).unwrap(), title);
        assert!(normalize_title(&title).is_err());
        assert!(normalize_stored_title(&"x".repeat(65536)).is_ok());
        assert!(normalize_stored_title(&"題".repeat(21846)).is_err());
        assert!(normalize_stored_title("   ").is_err());
        assert_eq!(normalize_title(" short ").unwrap(), "short");
    }

    #[test]
    fn validate_local_id_accepts_url_safe_ids_only() {
        assert!(validate_local_id("abc-123_X", "graph_id").is_ok());
        assert_eq!(
            validate_local_id("abc/123", "graph_id").expect_err("slash is unsafe"),
            "graph_id contains invalid characters"
        );
    }

    #[test]
    fn filenames_are_sanitized_for_local_paths_and_headers() {
        assert_eq!(safe_filename("../a\nb.pdf"), ".._a_b.pdf");
        assert_eq!(
            content_disposition_filename("quote\"name.pdf"),
            "quote_name.pdf"
        );
        assert_eq!(safe_filename(" \n "), "_");
        assert_eq!(safe_filename("   "), "original.bin");
    }

    #[test]
    fn mime_type_resolution_uses_sanitized_extension_fallbacks() {
        assert_eq!(
            local_upload_mime_type_for_filename("report.PDF"),
            "application/pdf"
        );
        assert_eq!(
            normalize_image_mime_type(Some("application/octet-stream"), "image.PNG")
                .expect("png extension fallback"),
            "image/png"
        );
        assert!(normalize_image_mime_type(Some("text/plain"), "image.png").is_err());
    }

    #[test]
    fn url_component_percent_encodes_non_unreserved_bytes() {
        assert_eq!(url_component("a b/c"), "a%20b%2Fc");
        assert_eq!(url_component("AZaz09-_.~"), "AZaz09-_.~");
    }
}
