use crate::{
    ids::validate_local_id, loopback_http::loopback_error,
    multipart_pending_upload::stream_field_to_pending_upload, paths::cleanup_pending_upload_file,
    runtime_config::LOCAL_UPLOAD_MAX_BYTES,
};
use axum::{extract::Multipart, http::StatusCode, response::Response};
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::Read,
    path::{Path, PathBuf},
};

pub(super) struct GraphImportUpload {
    pub(super) filename: String,
    pub(super) new_graph_id: String,
    pub(super) new_title: Option<String>,
    pub(super) pending_archive_path: PathBuf,
    pub(super) size_bytes: usize,
}

pub(super) struct ArchiveImportUpload {
    pub(super) filename: String,
    pub(super) folder_name: Option<String>,
    pub(super) pending_archive_path: PathBuf,
    pub(super) size_bytes: usize,
}

pub(super) struct CellArchiveRestoreUpload {
    pub(super) filename: String,
    pub(super) operation_id: String,
    pub(super) archive_sha256: String,
    pub(super) source_graph_id: String,
    pub(super) source_user_id: String,
    pub(super) target_generation: u64,
    pub(super) plan_digest: String,
    pub(super) expected_document_count: usize,
    pub(super) expected_rdf_triple_count: usize,
    pub(super) format_version: u64,
    /// Concession kinds this import declares under the content-parity ruling.
    /// None means strict, which is what every caller that omits the field gets.
    pub(super) content_parity: Option<serde_json::Value>,
    pub(super) pending_archive_path: PathBuf,
    pub(super) size_bytes: usize,
}

pub(super) async fn read_cell_archive_restore_upload(
    graph_dir: &Path,
    mut multipart: Multipart,
) -> Result<CellArchiveRestoreUpload, Response> {
    let mut filename: Option<String> = None;
    let mut operation_id: Option<String> = None;
    let mut archive_sha256: Option<String> = None;
    let mut source_graph_id: Option<String> = None;
    let mut source_user_id: Option<String> = None;
    let mut target_generation: Option<u64> = None;
    let mut plan_digest: Option<String> = None;
    let mut expected_document_count: Option<usize> = None;
    let mut expected_rdf_triple_count: Option<usize> = None;
    let mut format_version: Option<u64> = None;
    let mut content_parity: Option<serde_json::Value> = None;
    let mut pending_archive_path: Option<PathBuf> = None;
    let mut size_bytes: usize = 0;
    let mut seen = std::collections::BTreeSet::new();

    loop {
        let field = match multipart.next_field().await {
            Ok(Some(field)) => field,
            Ok(None) => break,
            Err(error) => {
                cleanup_optional_pending_upload(&pending_archive_path);
                return Err(loopback_error(
                    StatusCode::BAD_REQUEST,
                    &format!("invalid multipart cell archive restore: {error}"),
                ));
            }
        };
        let name = field.name().map(str::to_string).unwrap_or_default();
        if !matches!(
            name.as_str(),
            "file"
                | "operation_id"
                | "operationId"
                | "archive_sha256"
                | "archiveSha256"
                | "source_graph_id"
                | "sourceGraphId"
                | "source_user_id"
                | "sourceUserId"
                | "target_generation"
                | "targetGeneration"
                | "plan_digest"
                | "planDigest"
                | "expected_document_count"
                | "expectedDocumentCount"
                | "expected_rdf_triple_count"
                | "expectedRdfTripleCount"
                | "formatVersion"
                | "format_version"
                | "contentParity"
                | "content_parity"
        ) {
            cleanup_optional_pending_upload(&pending_archive_path);
            return Err(loopback_error(
                StatusCode::BAD_REQUEST,
                &format!("unknown multipart cell archive restore field {name}"),
            ));
        }
        let canonical_name = match name.as_str() {
            "operationId" => "operation_id",
            "archiveSha256" => "archive_sha256",
            "sourceGraphId" => "source_graph_id",
            "sourceUserId" => "source_user_id",
            "targetGeneration" => "target_generation",
            "planDigest" => "plan_digest",
            "expectedDocumentCount" => "expected_document_count",
            "expectedRdfTripleCount" => "expected_rdf_triple_count",
            "formatVersion" => "format_version",
            "contentParity" => "content_parity",
            other => other,
        };
        if !seen.insert(canonical_name.to_string()) {
            cleanup_optional_pending_upload(&pending_archive_path);
            return Err(loopback_error(
                StatusCode::BAD_REQUEST,
                &format!("duplicate multipart cell archive restore field {canonical_name}"),
            ));
        }

        if canonical_name == "file" {
            let field_filename = field
                .file_name()
                .map(str::to_string)
                .unwrap_or_else(|| "graph.tar.gz".to_string());
            let upload = match stream_field_to_pending_upload(
                graph_dir,
                field,
                LOCAL_UPLOAD_MAX_BYTES,
                "failed to read cell graph restore archive",
            )
            .await
            {
                Ok(upload) => upload,
                Err(error) => {
                    cleanup_optional_pending_upload(&pending_archive_path);
                    return Err(loopback_error(error.status, &error.message));
                }
            };
            if !pending_file_has_gzip_header(&upload.path).unwrap_or(false) {
                cleanup_pending_upload_file(&upload.path);
                return Err(loopback_error(
                    StatusCode::BAD_REQUEST,
                    "Invalid archive format. Expected a gzip-compressed tar file (.tar.gz)",
                ));
            }
            filename = Some(field_filename);
            size_bytes = upload.bytes_written;
            pending_archive_path = Some(upload.path);
            continue;
        }

        let text = match field.text().await {
            Ok(value) => value.trim().to_string(),
            Err(error) => {
                cleanup_optional_pending_upload(&pending_archive_path);
                return Err(loopback_error(
                    StatusCode::BAD_REQUEST,
                    &format!("failed to read multipart field {canonical_name}: {error}"),
                ));
            }
        };
        if text.is_empty() {
            cleanup_optional_pending_upload(&pending_archive_path);
            return Err(loopback_error(
                StatusCode::BAD_REQUEST,
                &format!("multipart cell archive restore field {canonical_name} must not be empty"),
            ));
        }
        let invalid_integer = || {
            cleanup_optional_pending_upload(&pending_archive_path);
            loopback_error(
                StatusCode::BAD_REQUEST,
                &format!("multipart cell archive restore field {canonical_name} must be a non-negative integer"),
            )
        };
        match canonical_name {
            "operation_id" => operation_id = Some(text),
            "archive_sha256" => archive_sha256 = Some(text),
            "source_graph_id" => source_graph_id = Some(text),
            "source_user_id" => source_user_id = Some(text),
            "target_generation" => {
                target_generation = Some(text.parse().map_err(|_| invalid_integer())?)
            }
            "plan_digest" => plan_digest = Some(text),
            "expected_document_count" => {
                expected_document_count = Some(text.parse().map_err(|_| invalid_integer())?)
            }
            "expected_rdf_triple_count" => {
                expected_rdf_triple_count = Some(text.parse().map_err(|_| invalid_integer())?)
            }
            "format_version" => {
                format_version = Some(text.parse().map_err(|_| invalid_integer())?)
            }
            "content_parity" => {
                // Parsed as JSON here and validated against the known concession kinds
                // where it is consumed, so one list of kinds stays authoritative.
                content_parity = Some(serde_json::from_str(&text).map_err(|_| {
                    loopback_error(
                        StatusCode::BAD_REQUEST,
                        "multipart cell archive restore field contentParity must be a JSON array of concession kinds",
                    )
                })?)
            }
            _ => unreachable!("closed restore multipart field set"),
        }
    }

    let pending_archive_path = match pending_archive_path {
        Some(path) => path,
        None => {
            return Err(loopback_error(
                StatusCode::BAD_REQUEST,
                "multipart cell archive restore field file is required",
            ));
        }
    };
    let result = (|| {
        let operation_id = required_restore_field(operation_id, "operation_id", &None)?;
        validate_operation_id(&operation_id)?;
        let archive_sha256 = required_restore_field(archive_sha256, "archive_sha256", &None)?;
        validate_lower_sha256(&archive_sha256, "archive_sha256")?;
        let source_graph_id = required_restore_field(source_graph_id, "source_graph_id", &None)?;
        validate_local_id(&source_graph_id, "source_graph_id")
            .map_err(|error| invalid_restore_field("source_graph_id", &error))?;
        let source_user_id = required_restore_field(source_user_id, "source_user_id", &None)?;
        validate_local_id(&source_user_id, "source_user_id")
            .map_err(|error| invalid_restore_field("source_user_id", &error))?;
        let target_generation =
            required_restore_field(target_generation, "target_generation", &None)?;
        if target_generation == 0 {
            return Err(invalid_restore_field(
                "target_generation",
                "must be greater than zero",
            ));
        }
        let plan_digest = required_restore_field(plan_digest, "plan_digest", &None)?;
        validate_lower_sha256(&plan_digest, "plan_digest")?;
        let expected_document_count =
            required_restore_field(expected_document_count, "expected_document_count", &None)?;
        let expected_rdf_triple_count = required_restore_field(
            expected_rdf_triple_count,
            "expected_rdf_triple_count",
            &None,
        )?;
        let actual_archive_sha256 = sha256_file(&pending_archive_path)
            .map_err(|error| invalid_restore_field("file", &error))?;
        if actual_archive_sha256 != archive_sha256 {
            return Err(invalid_restore_field(
                "archive_sha256",
                "does not match the uploaded archive bytes",
            ));
        }
        let format_version = format_version.unwrap_or(1);
        if !matches!(format_version, 1 | 2) {
            return Err(invalid_restore_field("format_version", "only versions 1 and 2 are supported"));
        }
        Ok(CellArchiveRestoreUpload {
            filename: filename.unwrap_or_else(|| "graph.tar.gz".to_string()),
            operation_id,
            archive_sha256,
            source_graph_id,
            source_user_id,
            target_generation,
            plan_digest,
            expected_document_count,
            expected_rdf_triple_count,
            format_version,
            content_parity,
            pending_archive_path: pending_archive_path.clone(),
            size_bytes,
        })
    })();
    if result.is_err() {
        cleanup_pending_upload_file(&pending_archive_path);
    }
    result
}

fn required_restore_field<T>(
    value: Option<T>,
    field: &str,
    cleanup_path: &Option<PathBuf>,
) -> Result<T, Response> {
    value.ok_or_else(|| {
        cleanup_optional_pending_upload(cleanup_path);
        loopback_error(
            StatusCode::BAD_REQUEST,
            &format!("multipart cell archive restore field {field} is required"),
        )
    })
}

fn invalid_restore_field(field: &str, reason: &str) -> Response {
    loopback_error(
        StatusCode::BAD_REQUEST,
        &format!("invalid multipart cell archive restore field {field}: {reason}"),
    )
}

fn validate_lower_sha256(value: &str, field: &str) -> Result<(), Response> {
    if value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        Ok(())
    } else {
        Err(invalid_restore_field(
            field,
            "must be exactly 64 lowercase hexadecimal characters",
        ))
    }
}

fn validate_operation_id(value: &str) -> Result<(), Response> {
    if value.len() <= 160
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':'))
    {
        Ok(())
    } else {
        Err(invalid_restore_field(
            "operation_id",
            "must be <=160 characters of [A-Za-z0-9._:-]",
        ))
    }
}

fn sha256_file(path: &Path) -> Result<String, String> {
    let mut file = File::open(path)
        .map_err(|error| format!("open pending cell graph restore archive: {error}"))?;
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = file
            .read(&mut buffer)
            .map_err(|error| format!("read pending cell graph restore archive: {error}"))?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
    }
    Ok(format!("{:x}", digest.finalize()))
}

pub(super) async fn read_graph_import_upload(
    profile_dir: &Path,
    mut multipart: Multipart,
) -> Result<GraphImportUpload, Response> {
    let mut filename: Option<String> = None;
    let mut new_graph_id: Option<String> = None;
    let mut new_title: Option<String> = None;
    let mut pending_archive_path: Option<PathBuf> = None;
    let mut size_bytes: usize = 0;

    loop {
        let field = match multipart.next_field().await {
            Ok(Some(field)) => field,
            Ok(None) => break,
            Err(error) => {
                cleanup_optional_pending_upload(&pending_archive_path);
                return Err(loopback_error(
                    StatusCode::BAD_REQUEST,
                    &format!("invalid multipart graph import: {error}"),
                ));
            }
        };

        let name = field.name().map(str::to_string).unwrap_or_default();
        if name == "file" {
            let field_filename = field
                .file_name()
                .map(str::to_string)
                .unwrap_or_else(|| "graph.tar.gz".to_string());
            if let Some(previous_path) = pending_archive_path.take() {
                cleanup_pending_upload_file(&previous_path);
            }
            let upload = match stream_field_to_pending_upload(
                profile_dir,
                field,
                LOCAL_UPLOAD_MAX_BYTES,
                "failed to read graph import archive",
            )
            .await
            {
                Ok(upload) => upload,
                Err(error) => {
                    cleanup_optional_pending_upload(&pending_archive_path);
                    return Err(loopback_error(error.status, &error.message));
                }
            };
            if !pending_file_has_gzip_header(&upload.path).unwrap_or(false) {
                cleanup_pending_upload_file(&upload.path);
                return Err(loopback_error(
                    StatusCode::BAD_REQUEST,
                    "Invalid archive format. Expected a gzip-compressed tar file (.tar.gz)",
                ));
            }
            filename = Some(field_filename);
            size_bytes = upload.bytes_written;
            pending_archive_path = Some(upload.path);
            continue;
        }

        let text = match field.text().await {
            Ok(value) => value.trim().to_string(),
            Err(error) => {
                cleanup_optional_pending_upload(&pending_archive_path);
                return Err(loopback_error(
                    StatusCode::BAD_REQUEST,
                    &format!("failed to read multipart field {name}: {error}"),
                ));
            }
        };
        if text.is_empty() {
            continue;
        }
        match name.as_str() {
            "new_graph_id" | "newGraphId" => new_graph_id = Some(text),
            "new_title" | "newTitle" => new_title = Some(text),
            _ => {}
        }
    }

    let Some(pending_archive_path) = pending_archive_path else {
        return Err(loopback_error(
            StatusCode::BAD_REQUEST,
            "missing multipart file field",
        ));
    };
    let Some(new_graph_id) = new_graph_id else {
        return Err(loopback_error(
            StatusCode::BAD_REQUEST,
            "new_graph_id is required",
        ));
    };
    if new_graph_id.len() > 128 {
        cleanup_pending_upload_file(&pending_archive_path);
        return Err(invalid_graph_id_error(None));
    }
    if let Err(error) = validate_local_id(&new_graph_id, "new_graph_id") {
        cleanup_pending_upload_file(&pending_archive_path);
        return Err(invalid_graph_id_error(Some(error)));
    }

    Ok(GraphImportUpload {
        filename: filename.unwrap_or_else(|| "graph.tar.gz".to_string()),
        new_graph_id,
        new_title,
        pending_archive_path,
        size_bytes,
    })
}

pub(super) async fn read_archive_import_upload(
    source_type: &str,
    graph_dir: &Path,
    mut multipart: Multipart,
) -> Result<ArchiveImportUpload, Response> {
    let mut filename: Option<String> = None;
    let mut folder_name: Option<String> = None;
    let mut pending_archive_path: Option<PathBuf> = None;
    let mut size_bytes: usize = 0;

    loop {
        let field = match multipart.next_field().await {
            Ok(Some(field)) => field,
            Ok(None) => break,
            Err(error) => {
                cleanup_optional_pending_upload(&pending_archive_path);
                return Err(loopback_error(
                    StatusCode::BAD_REQUEST,
                    &format!("invalid multipart import: {error}"),
                ));
            }
        };

        let name = field.name().map(str::to_string).unwrap_or_default();
        if name == "file" {
            let field_filename = field
                .file_name()
                .map(str::to_string)
                .unwrap_or_else(|| format!("{source_type}.zip"));
            if !field_filename.to_ascii_lowercase().ends_with(".zip") {
                cleanup_optional_pending_upload(&pending_archive_path);
                return Err(loopback_error(
                    StatusCode::BAD_REQUEST,
                    "File must be a ZIP archive",
                ));
            }
            if let Some(previous_path) = pending_archive_path.take() {
                cleanup_pending_upload_file(&previous_path);
            }
            let upload = match stream_field_to_pending_upload(
                graph_dir,
                field,
                LOCAL_UPLOAD_MAX_BYTES,
                "failed to read import archive",
            )
            .await
            {
                Ok(upload) => upload,
                Err(error) => {
                    cleanup_optional_pending_upload(&pending_archive_path);
                    return Err(loopback_error(error.status, &error.message));
                }
            };
            filename = Some(field_filename);
            size_bytes = upload.bytes_written;
            pending_archive_path = Some(upload.path);
            continue;
        }

        let text = match field.text().await {
            Ok(value) => value.trim().to_string(),
            Err(error) => {
                cleanup_optional_pending_upload(&pending_archive_path);
                return Err(loopback_error(
                    StatusCode::BAD_REQUEST,
                    &format!("failed to read multipart field {name}: {error}"),
                ));
            }
        };
        if text.is_empty() {
            continue;
        }
        if name == "folder_name" || name == "folderName" {
            folder_name = Some(text);
        }
    }

    let Some(pending_archive_path) = pending_archive_path else {
        return Err(loopback_error(
            StatusCode::BAD_REQUEST,
            "missing multipart file field",
        ));
    };

    Ok(ArchiveImportUpload {
        filename: filename.unwrap_or_else(|| format!("{source_type}.zip")),
        folder_name,
        pending_archive_path,
        size_bytes,
    })
}

fn invalid_graph_id_error(error: Option<String>) -> Response {
    let base = "Invalid graph ID. Must match pattern: ^[A-Za-z0-9_-]{1,128}$";
    let message = if let Some(error) = error {
        format!("{base} ({error})")
    } else {
        base.to_string()
    };
    loopback_error(StatusCode::BAD_REQUEST, &message)
}

fn cleanup_optional_pending_upload(path: &Option<PathBuf>) {
    if let Some(path) = path {
        cleanup_pending_upload_file(path);
    }
}

fn pending_file_has_gzip_header(path: &Path) -> Result<bool, String> {
    let mut file =
        File::open(path).map_err(|error| format!("open pending graph archive: {error}"))?;
    let mut header = [0_u8; 2];
    file.read_exact(&mut header)
        .map_err(|error| format!("read pending graph archive header: {error}"))?;
    Ok(header == [0x1f, 0x8b])
}
