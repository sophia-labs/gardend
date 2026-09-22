use crate::app_runtime::AppHandle;
use crate::{
    ids::{local_upload_mime_type_for_filename, safe_filename},
    local_crdt_jobs::{insert_and_spawn_crdt_job_for, LocalCrdtJobInput},
    local_jobs::{local_job_submit_response, LocalJobRecord, LocalJobRegistry},
    mcp_utils::{mcp_arg_string, mcp_graph_id_or_default},
    paths::{copy_pending_upload_file, existing_graph_dir},
    runtime_config::LOCAL_UPLOAD_MAX_BYTES,
    storage::display_path,
};
use std::{fs, path::PathBuf, sync::Arc};
use uuid::Uuid;

fn mcp_artifact_upload_job_response(
    record: &LocalJobRecord,
    document_id: &str,
    graph_id: &str,
) -> Result<serde_json::Value, String> {
    let mut value = serde_json::to_value(local_job_submit_response(record))
        .map_err(|error| format!("serialize MCP artifact upload job response: {error}"))?;
    if let Some(object) = value.as_object_mut() {
        object.insert(
            "jobId".to_string(),
            serde_json::Value::String(record.job_id.clone()),
        );
        object.insert(
            "documentId".to_string(),
            serde_json::Value::String(document_id.to_string()),
        );
        object.insert(
            "document_id".to_string(),
            serde_json::Value::String(document_id.to_string()),
        );
        object.insert(
            "graphId".to_string(),
            serde_json::Value::String(graph_id.to_string()),
        );
        object.insert(
            "graph_id".to_string(),
            serde_json::Value::String(graph_id.to_string()),
        );
        object.insert("accepted".to_string(), serde_json::Value::Bool(true));
        object.insert("asyncWork".to_string(), serde_json::Value::Bool(true));
    }
    Ok(value)
}

pub(super) async fn mcp_local_upload_artifact(
    app: AppHandle,
    jobs: Arc<LocalJobRegistry>,
    arguments: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let graph_id = mcp_graph_id_or_default(&app, arguments)?;
    let file_path = mcp_arg_string(arguments, &["file_path", "filePath"])
        .ok_or_else(|| "file_path is required".to_string())?;
    let path = PathBuf::from(&file_path);
    let metadata = fs::metadata(&path)
        .map_err(|error| format!("read upload file metadata {file_path}: {error}"))?;
    if !metadata.is_file() {
        return Err(format!("upload path is not a file: {file_path}"));
    }
    if metadata.len() > LOCAL_UPLOAD_MAX_BYTES as u64 {
        return Err(format!(
            "File too large. Maximum size: {}MB",
            LOCAL_UPLOAD_MAX_BYTES / (1024 * 1024)
        ));
    }

    if metadata.len() == 0 {
        return Err("Empty file uploaded".to_string());
    }

    let filename = path
        .file_name()
        .and_then(|value| value.to_str())
        .map(safe_filename)
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "unnamed".to_string());
    let mime_type = local_upload_mime_type_for_filename(&filename);
    let parent_id = mcp_arg_string(arguments, &["parent_id", "parentId"]);
    let label = mcp_arg_string(arguments, &["label"]);

    let graph_dir = existing_graph_dir(&app, &graph_id)?;
    let pending_original_path = copy_pending_upload_file(&graph_dir, &path)?;
    let document_id = format!("doc-{}", Uuid::new_v4().simple());
    let source_file = serde_json::json!({
        "storageKey": format!(
            "local://documents/{document_id}/original/{}",
            safe_filename(&filename)
        ),
        "originalFilename": filename.clone(),
        "mimeType": mime_type.clone(),
        "sizeBytes": metadata.len(),
    });
    let payload = serde_json::json!({
        "filename": filename,
        "mimeType": mime_type,
        "sizeBytes": metadata.len(),
        "pendingOriginalPath": display_path(&pending_original_path),
        "parentId": parent_id,
        "label": label,
        "documentId": document_id.clone(),
        "sourceFile": source_file,
    });

    let record = insert_and_spawn_crdt_job_for(
        app,
        jobs,
        LocalCrdtJobInput {
            job_type: "mcp_artifact_upload".to_string(),
            graph_id: graph_id.clone(),
            operation_kind: "document.uploadIngest".to_string(),
            document_id: Some(document_id.clone()),
            payload,
            detail: serde_json::json!({
                "graphId": graph_id.clone(),
                "documentId": document_id.clone(),
                "filename": path.file_name().and_then(|value| value.to_str()).unwrap_or("unnamed"),
                "sizeBytes": metadata.len(),
                "asyncWork": true,
                "mcpTool": "upload_artifact",
            }),
            pending_cleanup_path: Some(pending_original_path),
            running_message: "Ingesting MCP artifact upload".to_string(),
            success_message: "MCP artifact upload import complete".to_string(),
            result_mapper: Some(Box::new({
                let graph_id = graph_id.clone();
                let document_id = document_id.clone();
                move |mut value| {
                    if let Some(object) = value.as_object_mut() {
                        object.insert("success".to_string(), serde_json::Value::Bool(true));
                        object.insert(
                            "graph_id".to_string(),
                            serde_json::Value::String(graph_id.clone()),
                        );
                        object.insert("graphId".to_string(), serde_json::Value::String(graph_id));
                        object
                            .entry("artifact_id".to_string())
                            .or_insert_with(|| serde_json::Value::String(document_id.clone()));
                        object
                            .entry("artifactId".to_string())
                            .or_insert_with(|| serde_json::Value::String(document_id));
                        object.insert("auto_ingested".to_string(), serde_json::Value::Bool(true));
                        object.insert("autoIngested".to_string(), serde_json::Value::Bool(true));
                        return Ok(value);
                    }
                    Ok(serde_json::json!({
                        "success": true,
                        "graph_id": graph_id.clone(),
                        "graphId": graph_id,
                        "artifact_id": document_id.clone(),
                        "artifactId": document_id,
                        "auto_ingested": true,
                        "autoIngested": true,
                        "value": value,
                    }))
                }
            })),
        },
    )?;

    mcp_artifact_upload_job_response(&record, &document_id, &graph_id)
}
