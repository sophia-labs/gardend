use crate::{
    local_jobs::{local_job_status_response, LocalJobRegistry, LocalJobStatus},
    mcp_utils::mcp_required_job_id,
};

pub(super) fn mcp_local_get_job_status(
    jobs: &LocalJobRegistry,
    arguments: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let job_id = mcp_required_job_id(arguments)?;
    let record = jobs
        .get(&job_id)?
        .ok_or_else(|| format!("job not found: {job_id}"))?;
    serde_json::to_value(local_job_status_response(&record)).map_err(|error| error.to_string())
}

pub(super) fn mcp_local_get_job_result(
    jobs: &LocalJobRegistry,
    arguments: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let job_id = mcp_required_job_id(arguments)?;
    let record = jobs
        .get(&job_id)?
        .ok_or_else(|| format!("job not found: {job_id}"))?;
    if let Some(inline) = record.detail.get("result_inline") {
        return Ok(inline.clone());
    }
    if let Some(value) = jobs.read_spilled_result(&record)? {
        return Ok(value);
    }
    if matches!(record.status, LocalJobStatus::Failed) {
        return Err(record
            .error
            .clone()
            .or_else(|| {
                record
                    .detail
                    .get("error")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string)
            })
            .unwrap_or_else(|| format!("job {job_id} failed")));
    }
    Err(format!("result not available for job {job_id}"))
}

pub(super) fn mcp_local_cancel_job(
    jobs: &LocalJobRegistry,
    arguments: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let job_id = mcp_required_job_id(arguments)?;
    let response = jobs
        .cancel(&job_id)?
        .ok_or_else(|| format!("job not found: {job_id}"))?;
    serde_json::to_value(response).map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use uuid::Uuid;

    #[test]
    fn mcp_local_cancel_job_accepts_hosted_alias_and_returns_cancel_envelope() {
        let jobs_dir = std::env::temp_dir().join(format!("sophia-mcp-job-{}", Uuid::new_v4()));
        let jobs = LocalJobRegistry::new(jobs_dir.clone()).expect("job registry");
        let record = jobs
            .insert_queued("test", Some("graph-a".to_string()), serde_json::json!({}))
            .expect("queued job");

        let response = mcp_local_cancel_job(
            &jobs,
            &serde_json::json!({
                "jobId": record.job_id,
            }),
        )
        .expect("cancel response");

        assert_eq!(response.get("cancelled"), Some(&serde_json::json!(true)));
        assert_eq!(
            response.get("previous_status"),
            Some(&serde_json::json!("queued"))
        );

        let _ = fs::remove_dir_all(jobs_dir);
    }

    #[test]
    fn mcp_local_get_job_status_and_result_return_finished_payload() {
        let jobs_dir = std::env::temp_dir().join(format!("sophia-mcp-job-{}", Uuid::new_v4()));
        let jobs = LocalJobRegistry::new(jobs_dir.clone()).expect("job registry");
        let record = jobs
            .insert_finished(
                "test",
                Some("graph-a".to_string()),
                1,
                Ok(serde_json::json!({ "ok": true })),
                "application/json",
            )
            .expect("finished job");

        let status = mcp_local_get_job_status(
            &jobs,
            &serde_json::json!({
                "job_id": record.job_id.clone(),
            }),
        )
        .expect("job status");
        assert_eq!(status.get("status"), Some(&serde_json::json!("succeeded")));

        let result = mcp_local_get_job_result(
            &jobs,
            &serde_json::json!({
                "jobId": record.job_id,
            }),
        )
        .expect("job result");
        assert_eq!(result, serde_json::json!({ "ok": true }));

        let _ = fs::remove_dir_all(jobs_dir);
    }
}

/// Exercise the agent doors and the real durable job registry without loading
/// weights or contacting a provider. A broken on-disk setup manifest makes an
/// uncached model fail before download; an already loaded model needs no I/O.
#[cfg(all(test, feature = "headless"))]
mod semantic_job_tests {
    use super::*;
    use serde_json::json;
    use std::sync::Arc;

    #[test]
    fn semantic_preparation_and_refresh_are_pollable_graph_jobs() {
        let _remote = crate::semantic_model_remote::test_support::remote_embeddings_test_serial()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let _serial = crate::tauri_runtime::profile_env_serial()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let profile =
            std::env::temp_dir().join(format!("garden-semantic-mcp-jobs-{}", uuid::Uuid::new_v4()));
        let previous_profile = std::env::var_os("GARDEN_PROFILE_DIR");
        let remote_key = crate::semantic_model_remote::REMOTE_EMBEDDINGS_ENV;
        let previous_remote = std::env::var_os(remote_key);
        std::env::set_var("GARDEN_PROFILE_DIR", &profile);
        std::env::remove_var(remote_key);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            crate::app_runtime::async_runtime::block_on(async {
                let app = crate::tauri_runtime::build_mock_app_for_tests(true);
                let graph = "semantic-job-graph";
                crate::graph_service::create_graph_service(
                    &app,
                    crate::graph_service::CreateGraphInput {
                        graph_id: Some(graph.into()),
                        title: "Semantic jobs".into(),
                        description: None,
                        operation_id: None,
                    },
                )
                .unwrap();
                crate::crdt_queue::enqueue_crdt_operation(app.clone(), crate::crdt_queue::EnqueueCrdtOperationInput {
                    kind: "document.write".into(), graph_id: graph.into(), document_id: Some("job-document".into()),
                    payload: json!({"title": "Index me", "content": "A real document in the semantic index.", "format": "markdown"}),
                }).await.unwrap();
                let config = crate::semantic_models::read_semantic_model_config(&app).unwrap();
                let setup = crate::model_setup_paths::semantic_model_setup_path(
                    &app,
                    &config.selected_model_id,
                )
                .unwrap();
                std::fs::create_dir_all(setup.parent().unwrap()).unwrap();
                std::fs::write(&setup, b"{").unwrap();
                let jobs = Arc::new(LocalJobRegistry::new(profile.join("jobs")).unwrap());
                let ctx = crate::mcp_dispatch_registry::McpCallCtx { app, jobs: &jobs };
                for name in ["semantic_search", "semantic_reason"] {
                    let entry = crate::mcp_dispatch_registry::lookup(name).unwrap();
                    let error =
                        (entry.handler)(&ctx, &json!({"graph_id": graph, "query": "find this"}))
                            .await
                            .unwrap_err()
                            .to_string();
                    assert!(
                        error.contains("run refresh_semantic_index first"),
                        "{error}"
                    );
                    assert!(crate::mcp_tool_registry::is_known_mcp_tool_name(
                        "refresh_semantic_index"
                    ));
                }
                for name in ["prepare_semantic_model", "refresh_semantic_index"] {
                    let entry = crate::mcp_dispatch_registry::lookup(name).unwrap();
                    let submitted = (entry.handler)(&ctx, &json!({"graph_id": graph}))
                        .await
                        .unwrap();
                    let job_id = submitted["job_id"].as_str().unwrap();
                    assert_eq!(submitted["status"], "queued", "{submitted}");
                    assert_eq!(
                        crate::local_jobs::local_job_graph_id(&jobs.get(job_id).unwrap().unwrap())
                            .as_deref(),
                        Some(graph)
                    );
                    let args = json!({"job_id": job_id});
                    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
                    loop {
                        let status = mcp_local_get_job_status(&jobs, &args).unwrap();
                        match status["status"].as_str().unwrap() {
                            "failed" => {
                                assert!(mcp_local_get_job_result(&jobs, &args).is_err());
                                assert!(status["error"]
                                    .as_str()
                                    .is_some_and(|error| !error.is_empty()));
                                break;
                            }
                            "succeeded" => {
                                assert!(mcp_local_get_job_result(&jobs, &args).is_ok());
                                break;
                            }
                            _ => {
                                assert!(std::time::Instant::now() < deadline, "{status}");
                                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                            }
                        }
                    }
                }
            });
        }));
        match previous_profile {
            Some(value) => std::env::set_var("GARDEN_PROFILE_DIR", value),
            None => std::env::remove_var("GARDEN_PROFILE_DIR"),
        }
        match previous_remote {
            Some(value) => std::env::set_var(remote_key, value),
            None => std::env::remove_var(remote_key),
        }
        let _ = std::fs::remove_dir_all(profile);
        if let Err(error) = result {
            std::panic::resume_unwind(error);
        }
    }
}
