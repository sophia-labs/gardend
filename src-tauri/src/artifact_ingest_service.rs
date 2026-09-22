use crate::app_runtime::AppHandle;
use crate::{
    artifact_ingest_payloads::{
        already_ingested_update_payload, artifact_ingested_payload, artifact_upload_ingest_payload,
        editable_document_payload,
    },
    clock::timestamp,
    crdt_projection_flush::flush_document_projection,
    crdt_queue::{enqueue_crdt_operation, EnqueueCrdtOperationInput},
    document_projection_service::hosted_document_summaries,
    hosted_navigation_projection::hosted_navigation_parts,
    json_utils::json_string,
    local_jobs::{local_job_submit_response, LocalJobProgress, LocalJobRecord, LocalJobRegistry},
    mcp_utils::{mcp_arg_string, mcp_graph_id_or_default},
    original_file_service::{read_artifact_original_file, read_document_original_file},
    paths::{existing_document_dir, existing_graph_dir},
};
use std::sync::Arc;

pub(super) use crate::artifact_upload_service::mcp_local_upload_artifact;

pub(super) async fn local_ingest_artifact(
    app: AppHandle,
    graph_id: String,
    artifact_id: String,
    mode: String,
    requested_title: Option<String>,
    mut parent_id_override: Option<String>,
    keep_ingested_link: bool,
    mark_artifact_ingested: bool,
) -> Result<serde_json::Value, String> {
    let mode = mode.to_ascii_lowercase();
    if mode != "ingest" && mode != "import" {
        return Err("mode must be 'ingest' or 'import'".to_string());
    }
    let read_only = mode != "import";
    if read_only {
        parent_id_override = None;
    }

    let graph_dir = existing_graph_dir(&app, &graph_id)?;
    if existing_document_dir(&graph_dir, &artifact_id).is_ok()
        && read_document_original_file(&app, &graph_id, &artifact_id).is_ok()
    {
        let editable_value = if let Some(payload) =
            already_ingested_update_payload(&artifact_id, read_only, parent_id_override.clone())
        {
            enqueue_crdt_operation(
                app.clone(),
                EnqueueCrdtOperationInput {
                    kind: "workspace.updateDocument".to_string(),
                    graph_id: graph_id.clone(),
                    document_id: Some(artifact_id.clone()),
                    payload,
                },
            )
            .await?
        } else {
            serde_json::Value::Null
        };
        flush_document_projection(app.clone(), &graph_id, &artifact_id).await?;
        let document = hosted_document_summaries(&app, &graph_id)?
            .into_iter()
            .map(|summary| {
                serde_json::to_value(summary).expect("HostedDocumentSummary always serializes")
            })
            .find(|document| {
                json_string(document.get("id")).as_deref() == Some(artifact_id.as_str())
            })
            .unwrap_or_else(|| serde_json::json!({ "id": artifact_id.clone() }));
        let title = json_string(document.get("title")).unwrap_or_else(|| artifact_id.clone());
        return Ok(serde_json::json!({
            "success": true,
            "graph_id": graph_id.clone(),
            "graphId": graph_id,
            "artifact_id": artifact_id.clone(),
            "artifactId": artifact_id.clone(),
            "document_id": artifact_id.clone(),
            "documentId": artifact_id,
            "title": title,
            "mode": mode,
            "readOnly": read_only,
            "auto_ingested": true,
            "autoIngested": true,
            "already_ingested": true,
            "alreadyIngested": true,
            "document": document,
            "value": editable_value,
        }));
    }

    let (_, _, artifacts) = hosted_navigation_parts(&app, &graph_id)?;
    let artifact = artifacts
        .into_iter()
        .find(|artifact| json_string(artifact.get("id")).as_deref() == Some(artifact_id.as_str()))
        .ok_or_else(|| format!("artifact {artifact_id} not found"))?;
    let (manifest, bytes) = read_artifact_original_file(&app, &graph_id, &artifact_id)?;
    if bytes.is_empty() {
        return Err("Empty artifact file".to_string());
    }

    let payload = artifact_upload_ingest_payload(
        &artifact_id,
        &artifact,
        &manifest,
        &bytes,
        requested_title,
        parent_id_override,
    );

    let mut upload_value = enqueue_crdt_operation(
        app.clone(),
        EnqueueCrdtOperationInput {
            kind: "document.uploadIngest".to_string(),
            graph_id: graph_id.clone(),
            document_id: None,
            payload,
        },
    )
    .await?;
    let document_id = json_string(
        upload_value
            .get("documentId")
            .or_else(|| upload_value.get("document_id")),
    )
    .ok_or_else(|| "artifact ingestion did not return a documentId".to_string())?;

    let editable_value = if read_only {
        serde_json::Value::Null
    } else {
        if let Some(object) = upload_value.as_object_mut() {
            object.insert("readOnly".to_string(), serde_json::Value::Bool(false));
        }
        enqueue_crdt_operation(
            app.clone(),
            EnqueueCrdtOperationInput {
                kind: "workspace.updateDocument".to_string(),
                graph_id: graph_id.clone(),
                document_id: Some(document_id.clone()),
                payload: editable_document_payload(&document_id),
            },
        )
        .await?
    };

    let artifact_value = enqueue_crdt_operation(
        app.clone(),
        EnqueueCrdtOperationInput {
            kind: "workspace.putArtifact".to_string(),
            graph_id: graph_id.clone(),
            document_id: Some(artifact_id.clone()),
            payload: artifact_ingested_payload(
                &artifact_id,
                &artifact,
                &manifest,
                bytes.len(),
                &document_id,
                keep_ingested_link,
                mark_artifact_ingested,
            ),
        },
    )
    .await?;
    flush_document_projection(app, &graph_id, &document_id).await?;
    let document_title =
        json_string(upload_value.get("title")).unwrap_or_else(|| document_id.clone());

    Ok(serde_json::json!({
        "success": true,
        "graph_id": graph_id.clone(),
        "graphId": graph_id,
        "artifact_id": artifact_id.clone(),
        "artifactId": artifact_id,
        "document_id": document_id.clone(),
        "documentId": document_id,
        "title": document_title,
        "mode": mode,
        "readOnly": read_only,
        "auto_ingested": false,
        "autoIngested": false,
        "document": upload_value,
        "artifact": artifact_value,
        "value": editable_value,
    }))
}

pub(super) async fn mcp_local_ingest_artifact(
    app: AppHandle,
    jobs: Arc<LocalJobRegistry>,
    arguments: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let graph_id = mcp_graph_id_or_default(&app, arguments)?;
    let artifact_id = mcp_arg_string(arguments, &["artifact_id", "artifactId"])
        .ok_or_else(|| "artifact_id is required".to_string())?;
    let mode = mcp_arg_string(arguments, &["mode"]).unwrap_or_else(|| "ingest".to_string());
    let normalized_mode = mode.to_ascii_lowercase();
    if normalized_mode != "ingest" && normalized_mode != "import" {
        return Err("mode must be 'ingest' or 'import'".to_string());
    }
    let requested_title = mcp_arg_string(arguments, &["title"]);
    let parent_id = mcp_arg_string(arguments, &["parent_id", "parentId"]);

    let record = insert_mcp_artifact_ingest_job(
        app,
        jobs,
        graph_id,
        artifact_id,
        normalized_mode,
        requested_title,
        parent_id,
    )?;
    mcp_artifact_ingest_job_response(&record)
}

fn insert_mcp_artifact_ingest_job(
    app: AppHandle,
    jobs: Arc<LocalJobRegistry>,
    graph_id: String,
    artifact_id: String,
    mode: String,
    requested_title: Option<String>,
    parent_id: Option<String>,
) -> Result<LocalJobRecord, String> {
    let detail = serde_json::json!({
        "graphId": graph_id.clone(),
        "artifactId": artifact_id.clone(),
        "mode": mode.clone(),
        "title": requested_title.clone(),
        "parentId": parent_id.clone(),
        "asyncWork": true,
        "mcpTool": "ingest_artifact",
    });
    let record = jobs.insert_queued("mcp_artifact_ingest", Some(graph_id.clone()), detail)?;
    spawn_mcp_artifact_ingest_job(
        app,
        jobs,
        record.job_id.clone(),
        graph_id,
        artifact_id,
        mode,
        requested_title,
        parent_id,
    );
    Ok(record)
}

fn spawn_mcp_artifact_ingest_job(
    app: AppHandle,
    jobs: Arc<LocalJobRegistry>,
    job_id: String,
    graph_id: String,
    artifact_id: String,
    mode: String,
    requested_title: Option<String>,
    parent_id: Option<String>,
) {
    tokio::spawn(async move {
        if jobs.is_cancelled(&job_id).unwrap_or(false) {
            return;
        }
        let _ = jobs.mark_running(&job_id);
        report_mcp_artifact_ingest_progress(
            &jobs,
            &job_id,
            "running",
            "Importing MCP artifact",
            1,
            2,
            serde_json::json!({
                "artifactId": artifact_id.clone(),
                "mode": mode.clone(),
            }),
        );
        let result = local_ingest_artifact(
            app,
            graph_id,
            artifact_id.clone(),
            mode,
            requested_title,
            parent_id,
            true,
            true,
        )
        .await;
        if jobs.is_cancelled(&job_id).unwrap_or(false) {
            return;
        }
        if result.is_ok() {
            report_mcp_artifact_ingest_progress(
                &jobs,
                &job_id,
                "complete",
                "MCP artifact import complete",
                2,
                2,
                serde_json::json!({ "artifactId": artifact_id }),
            );
        }
        let _ = jobs.finish_existing(&job_id, result, "application/json");
    });
}

fn report_mcp_artifact_ingest_progress(
    jobs: &Arc<LocalJobRegistry>,
    job_id: &str,
    phase: &str,
    message: &str,
    current: usize,
    total: usize,
    details: serde_json::Value,
) {
    let total = total.max(current);
    let percent = if total == 0 {
        0.0
    } else {
        (current as f64 / total as f64) * 100.0
    };
    let _ = jobs.update_progress(
        job_id,
        LocalJobProgress {
            phase: phase.to_string(),
            message: message.to_string(),
            current,
            total,
            percent,
            updated_at: timestamp(),
            details,
        },
    );
}

fn mcp_artifact_ingest_job_response(record: &LocalJobRecord) -> Result<serde_json::Value, String> {
    let mut value = serde_json::to_value(local_job_submit_response(record))
        .map_err(|error| format!("serialize MCP artifact ingest job response: {error}"))?;
    if let Some(object) = value.as_object_mut() {
        object.insert(
            "jobId".to_string(),
            serde_json::Value::String(record.job_id.clone()),
        );
        object.insert("accepted".to_string(), serde_json::Value::Bool(true));
        object.insert("asyncWork".to_string(), serde_json::Value::Bool(true));
    }
    Ok(value)
}
