use crate::{
    local_job_types::{local_job_links, LocalJobRecord, LocalJobStatus},
    runtime_config::PROFILE_ID,
};
use uuid::Uuid;

pub(super) fn new_local_job_id() -> String {
    format!("job-{}", Uuid::new_v4().simple())
}

pub(super) fn queued_job_record(
    job_id: String,
    job_type: &str,
    graph_id: Option<&str>,
    mut detail: serde_json::Value,
    now: String,
) -> Result<LocalJobRecord, String> {
    let links = local_job_links(&job_id);
    if !detail.is_object() {
        detail = serde_json::json!({});
    }
    detail["type"] = serde_json::json!(job_type);
    detail["result_ready"] = serde_json::json!(false);
    detail["links"] = serde_json::to_value(&links)
        .map_err(|error| format!("serialize local job links: {error}"))?;
    if let Some(graph_id) = graph_id {
        detail["graph_id"] = serde_json::json!(graph_id);
        detail["graphId"] = serde_json::json!(graph_id);
    }

    Ok(LocalJobRecord {
        job_id,
        status: LocalJobStatus::Queued,
        updated_at: now.clone(),
        user_id: Some(PROFILE_ID.to_string()),
        owner_principal: None,
        graph_generation: None,
        initiator_principal: None,
        submitted_role: None,
        policy_revision: None,
        submitted_at: now,
        started_at: String::new(),
        completed_at: String::new(),
        processing_time_ms: 0,
        detail,
        progress: None,
        error: None,
    })
}

pub(super) fn finished_job_record(
    job_id: String,
    graph_id: Option<&str>,
    started_at_ms: u128,
    completed_at_ms: u128,
    mut detail: serde_json::Value,
    status: LocalJobStatus,
    error: Option<String>,
) -> LocalJobRecord {
    if let Some(graph_id) = graph_id {
        detail["graph_id"] = serde_json::json!(graph_id);
    }

    LocalJobRecord {
        job_id,
        status,
        updated_at: completed_at_ms.to_string(),
        user_id: Some(PROFILE_ID.to_string()),
        owner_principal: None,
        graph_generation: None,
        initiator_principal: None,
        submitted_role: None,
        policy_revision: None,
        submitted_at: started_at_ms.to_string(),
        started_at: started_at_ms.to_string(),
        completed_at: completed_at_ms.to_string(),
        processing_time_ms: completed_at_ms.saturating_sub(started_at_ms),
        detail,
        progress: None,
        error,
    }
}
