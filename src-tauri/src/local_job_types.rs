use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct LocalJobRecord {
    pub(crate) job_id: String,
    pub(crate) status: LocalJobStatus,
    pub(crate) updated_at: String,
    pub(crate) user_id: Option<String>,
    #[serde(default)]
    pub(crate) owner_principal: Option<String>,
    #[serde(default)]
    pub(crate) graph_generation: Option<u64>,
    #[serde(default)]
    pub(crate) initiator_principal: Option<String>,
    #[serde(default)]
    pub(crate) submitted_role: Option<String>,
    #[serde(default)]
    pub(crate) policy_revision: Option<u64>,
    pub(crate) submitted_at: String,
    pub(crate) started_at: String,
    pub(crate) completed_at: String,
    pub(crate) processing_time_ms: u128,
    pub(crate) detail: serde_json::Value,
    #[serde(default)]
    pub(crate) progress: Option<LocalJobProgress>,
    pub(crate) error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct LocalJobProgress {
    pub(crate) phase: String,
    pub(crate) message: String,
    pub(crate) current: usize,
    pub(crate) total: usize,
    pub(crate) percent: f64,
    pub(crate) updated_at: String,
    #[serde(default)]
    pub(crate) details: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum LocalJobStatus {
    Queued,
    Running,
    Succeeded,
    Failed,
    Cancelled,
}

#[derive(Debug, Serialize)]
pub(crate) struct LocalGraphQuerySubmitResponse {
    job_id: String,
    status: LocalJobStatus,
    trace_id: String,
    poll_url: String,
    result_url: String,
}

#[derive(Debug, Serialize)]
pub(crate) struct LocalJobSubmitResponse {
    job_id: String,
    status: LocalJobStatus,
    detail: serde_json::Value,
    progress: Option<LocalJobProgress>,
    trace_id: String,
    links: LocalJobLinks,
}

#[derive(Debug, Serialize)]
pub(crate) struct LocalJobStatusResponse {
    job_id: String,
    status: LocalJobStatus,
    updated_at: String,
    user_id: Option<String>,
    owner_principal: Option<String>,
    graph_generation: Option<u64>,
    initiator_principal: Option<String>,
    submitted_role: Option<String>,
    policy_revision: Option<u64>,
    submitted_at: String,
    started_at: String,
    completed_at: String,
    processing_time_ms: u128,
    detail: serde_json::Value,
    progress: Option<LocalJobProgress>,
    error: Option<String>,
    links: LocalJobLinks,
}

#[derive(Debug, Serialize)]
pub(crate) struct LocalJobCancelResponse {
    pub(crate) job_id: String,
    pub(crate) cancelled: bool,
    pub(crate) previous_status: LocalJobStatus,
    pub(crate) message: String,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct LocalJobLinks {
    status: String,
    result: Option<String>,
    websocket: LocalWebSocketSubscriptionHint,
}

#[derive(Debug, Clone, Serialize)]
struct LocalWebSocketSubscriptionHint {
    description: &'static str,
    payload: serde_json::Value,
}

pub(crate) fn local_job_status_label(status: &LocalJobStatus) -> &'static str {
    match status {
        LocalJobStatus::Queued => "queued",
        LocalJobStatus::Running => "running",
        LocalJobStatus::Succeeded => "succeeded",
        LocalJobStatus::Failed => "failed",
        LocalJobStatus::Cancelled => "cancelled",
    }
}

pub(crate) fn local_job_links(job_id: &str) -> LocalJobLinks {
    LocalJobLinks {
        status: format!("/graphs/jobs/{job_id}"),
        result: Some(format!("/graphs/jobs/{job_id}/result")),
        websocket: LocalWebSocketSubscriptionHint {
            description: "Send this payload after connecting to /ws to stream job updates.",
            payload: serde_json::json!({ "type": "subscribe", "job_id": job_id }),
        },
    }
}

pub(crate) fn local_graph_query_submit_response(
    record: &LocalJobRecord,
) -> LocalGraphQuerySubmitResponse {
    LocalGraphQuerySubmitResponse {
        job_id: record.job_id.clone(),
        status: record.status.clone(),
        trace_id: record.job_id.clone(),
        poll_url: format!("/graphs/jobs/{}", record.job_id),
        result_url: format!("/graphs/jobs/{}/result", record.job_id),
    }
}

pub(crate) fn local_job_submit_response(record: &LocalJobRecord) -> LocalJobSubmitResponse {
    LocalJobSubmitResponse {
        job_id: record.job_id.clone(),
        status: record.status.clone(),
        detail: record.detail.clone(),
        progress: record.progress.clone(),
        trace_id: record.job_id.clone(),
        links: local_job_links(&record.job_id),
    }
}

pub(crate) fn local_job_status_response(record: &LocalJobRecord) -> LocalJobStatusResponse {
    LocalJobStatusResponse {
        job_id: record.job_id.clone(),
        status: record.status.clone(),
        updated_at: record.updated_at.clone(),
        user_id: record.user_id.clone(),
        owner_principal: record.owner_principal.clone(),
        graph_generation: record.graph_generation,
        initiator_principal: record.initiator_principal.clone(),
        submitted_role: record.submitted_role.clone(),
        policy_revision: record.policy_revision,
        submitted_at: record.submitted_at.clone(),
        started_at: record.started_at.clone(),
        completed_at: record.completed_at.clone(),
        processing_time_ms: record.processing_time_ms,
        detail: record.detail.clone(),
        progress: record.progress.clone(),
        error: record.error.clone(),
        links: local_job_links(&record.job_id),
    }
}

pub(crate) fn local_job_graph_id(record: &LocalJobRecord) -> Option<String> {
    for key in ["graph_id", "graphId"] {
        if let Some(value) = record.detail.get(key).and_then(serde_json::Value::as_str) {
            if !value.trim().is_empty() {
                return Some(value.to_string());
            }
        }
    }
    if let Some(inline) = record.detail.get("result_inline") {
        for key in ["graph_id", "graphId"] {
            if let Some(value) = inline.get(key).and_then(serde_json::Value::as_str) {
                if !value.trim().is_empty() {
                    return Some(value.to_string());
                }
            }
        }
    }
    None
}
