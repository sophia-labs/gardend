use crate::{
    cell_lifecycle::CellLifecycle,
    local_jobs::{local_job_graph_id, LocalJobRecord, LocalJobRegistry, LocalJobStatus},
};
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

fn temp_jobs_dir(name: &str) -> PathBuf {
    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    std::env::temp_dir().join(format!("mnemosyne-jobs-{name}-{suffix}"))
}

#[test]
fn queued_job_persists_and_finishes_inline_result() {
    let dir = temp_jobs_dir("finish");
    let registry = LocalJobRegistry::new(dir.clone()).expect("create registry");
    let record = registry
        .insert_queued(
            "test_job",
            Some("graph-one".to_string()),
            serde_json::json!({ "input": true }),
        )
        .expect("insert queued");
    assert!(matches!(record.status, LocalJobStatus::Queued));

    let running = registry
        .mark_running(&record.job_id)
        .expect("mark running")
        .expect("job exists");
    assert!(matches!(running.status, LocalJobStatus::Running));

    let finished = registry
        .finish_existing(
            &record.job_id,
            Ok(serde_json::json!({ "ok": true })),
            "application/json",
        )
        .expect("finish")
        .expect("job exists");
    assert!(matches!(finished.status, LocalJobStatus::Succeeded));
    assert_eq!(finished.detail["result_ready"], true);
    assert_eq!(finished.detail["result_inline"]["ok"], true);

    let reloaded = LocalJobRegistry::new(dir.clone())
        .expect("reload registry")
        .get(&record.job_id)
        .expect("get reloaded")
        .expect("record exists");
    assert!(matches!(reloaded.status, LocalJobStatus::Succeeded));
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn queued_job_can_be_cancelled() {
    let dir = temp_jobs_dir("cancel");
    let registry = LocalJobRegistry::new(dir.clone()).expect("create registry");
    let record = registry
        .insert_queued("test_job", None, serde_json::json!({}))
        .expect("insert queued");
    let cancel = registry
        .cancel(&record.job_id)
        .expect("cancel")
        .expect("job exists");
    assert!(cancel.cancelled);
    assert!(matches!(cancel.previous_status, LocalJobStatus::Queued));
    assert!(registry.is_cancelled(&record.job_id).expect("is cancelled"));
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn queued_and_running_jobs_hold_the_cell_until_terminal() {
    let dir = temp_jobs_dir("lifecycle");
    let lifecycle = Arc::new(CellLifecycle::new());
    let registry = LocalJobRegistry::new_with_lifecycle(dir.clone(), Some(Arc::clone(&lifecycle)))
        .expect("create tracked registry");
    let record = registry
        .insert_queued("test_job", None, serde_json::json!({}))
        .expect("insert queued");
    assert_eq!(lifecycle.snapshot().background_jobs, 1);

    registry
        .mark_running(&record.job_id)
        .expect("mark running")
        .expect("job exists");
    assert_eq!(
        lifecycle.snapshot().background_jobs,
        1,
        "queued -> running is not a terminal transition"
    );

    registry
        .finish_existing(
            &record.job_id,
            Ok(serde_json::json!({})),
            "application/json",
        )
        .expect("finish")
        .expect("job exists");
    assert_eq!(lifecycle.snapshot().background_jobs, 0);
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn job_graph_id_prefers_detail_then_inline_result_aliases() {
    let mut record = LocalJobRecord {
        job_id: "job-test".to_string(),
        status: LocalJobStatus::Succeeded,
        updated_at: "1".to_string(),
        user_id: None,
        owner_principal: None,
        graph_generation: None,
        initiator_principal: None,
        submitted_role: None,
        policy_revision: None,
        submitted_at: "1".to_string(),
        started_at: "1".to_string(),
        completed_at: "1".to_string(),
        processing_time_ms: 0,
        detail: serde_json::json!({
            "result_inline": {
                "graphId": "inline-graph"
            }
        }),
        progress: None,
        error: None,
    };
    assert_eq!(local_job_graph_id(&record).as_deref(), Some("inline-graph"));

    record.detail["graph_id"] = serde_json::json!("detail-graph");
    assert_eq!(local_job_graph_id(&record).as_deref(), Some("detail-graph"));
}
