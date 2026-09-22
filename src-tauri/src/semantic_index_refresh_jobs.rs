use crate::app_runtime::AppHandle;
use crate::{
    local_jobs::{
        local_job_submit_response, LocalJobRegistry, LocalJobStatus, LocalJobSubmitResponse,
    },
    paths::existing_graph_dir,
    runtime_config::SEMANTIC_INDEX_REFRESH_JOB_TYPE,
    semantic_index_progress::SemanticIndexProgressSink,
    semantic_index_refresh::refresh_semantic_index_with_progress,
    semantic_index_status::semantic_index_status,
    semantic_models::{
        read_semantic_model_config, semantic_model_effective_batch_size, semantic_model_spec_by_id,
    },
    semantic_scaffold::semantic_scaffold_max_entities,
};
use serde::{Deserialize, Serialize};
use std::{sync::Arc, thread};

#[derive(Debug, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub(super) struct RefreshSemanticIndexInput {
    #[serde(alias = "graph_id")]
    pub(super) graph_id: String,
    #[serde(default, alias = "flush_boundary")]
    pub(super) flush_boundary: SemanticRefreshFlushBoundary,
}

#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(super) enum SemanticRefreshFlushBoundary {
    BestEffort,
    Required,
}

impl Default for SemanticRefreshFlushBoundary {
    fn default() -> Self {
        Self::BestEffort
    }
}

impl SemanticRefreshFlushBoundary {
    pub(super) const fn as_str(self) -> &'static str {
        match self {
            Self::BestEffort => "bestEffort",
            Self::Required => "required",
        }
    }
}

pub(super) fn submit_semantic_index_refresh_job(
    app: AppHandle,
    jobs: Arc<LocalJobRegistry>,
    input: RefreshSemanticIndexInput,
) -> Result<LocalJobSubmitResponse, String> {
    let graph_dir = existing_graph_dir(&app, &input.graph_id)?;
    let status = semantic_index_status(&app, &graph_dir, &input.graph_id)?;
    let config = read_semantic_model_config(&app)?;
    let spec = semantic_model_spec_by_id(&config.selected_model_id)?;
    let record = jobs.insert_queued(
        SEMANTIC_INDEX_REFRESH_JOB_TYPE,
        Some(input.graph_id.clone()),
        serde_json::json!({
            "provider_id": spec.provider_id,
            "providerId": spec.provider_id,
            "model_id": spec.model_id,
            "modelId": spec.model_id,
            "dimensions": spec.dimensions,
            "batch_size": semantic_model_effective_batch_size(&spec, &config),
            "batchSize": semantic_model_effective_batch_size(&spec, &config),
            "document_count": status.document_count,
            "documentCount": status.document_count,
            "stale_document_count": status.stale_document_count,
            "staleDocumentCount": status.stale_document_count,
            "flushBoundary": input.flush_boundary.as_str(),
            "maxEntityCap": semantic_scaffold_max_entities(),
        }),
    )?;
    let sink =
        SemanticIndexProgressSink::new(jobs.clone(), record.job_id.clone(), input.graph_id.clone());
    sink.report(
        "queued",
        "Semantic index refresh queued",
        0,
        status.document_count,
        serde_json::json!({
            "documents": status.document_count,
            "staleDocuments": status.stale_document_count,
        }),
    )?;
    let record = jobs.get(&record.job_id)?.unwrap_or(record);
    spawn_semantic_index_refresh_job(app, jobs, record.job_id.clone(), input);
    Ok(local_job_submit_response(&record))
}

fn spawn_semantic_index_refresh_job(
    app: AppHandle,
    jobs: Arc<LocalJobRegistry>,
    job_id: String,
    input: RefreshSemanticIndexInput,
) {
    thread::spawn(move || {
        if jobs.is_cancelled(&job_id).unwrap_or(false) {
            return;
        }
        match jobs.mark_running(&job_id) {
            Ok(Some(record)) if matches!(record.status, LocalJobStatus::Running) => {}
            Ok(Some(_)) | Ok(None) => return,
            Err(error) => {
                let _ = jobs.finish_existing(&job_id, Err(error.into()), "application/json");
                return;
            }
        }

        let sink =
            SemanticIndexProgressSink::new(jobs.clone(), job_id.clone(), input.graph_id.clone());
        let result = refresh_semantic_index_with_progress(
            &app,
            &input.graph_id,
            input.flush_boundary,
            Some(&sink),
        )
        .and_then(|status| {
            serde_json::to_value(status)
                .map_err(|error| format!("serialize semantic index status: {error}"))
        });

        if jobs.is_cancelled(&job_id).unwrap_or(false) {
            return;
        }
        if let Ok(value) = &result {
            let block_count = value
                .get("blockCount")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or_default() as usize;
            let _ = sink.report(
                "complete",
                "Semantic index refresh complete",
                block_count,
                block_count,
                serde_json::json!({ "result": value }),
            );
        }
        if let Err(error) = jobs.finish_existing(&job_id, result, "application/json") {
            log::error!("Failed to finish semantic index job {job_id}: {error}");
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refresh_input_defaults_to_best_effort_flush_boundary() {
        let input: RefreshSemanticIndexInput =
            serde_json::from_value(serde_json::json!({ "graphId": "graph-a" }))
                .expect("input parses");

        assert_eq!(
            input.flush_boundary,
            SemanticRefreshFlushBoundary::BestEffort
        );
    }

    #[test]
    fn refresh_input_accepts_required_flush_boundary() {
        let input: RefreshSemanticIndexInput = serde_json::from_value(serde_json::json!({
            "graphId": "graph-a",
            "flushBoundary": "required"
        }))
        .expect("input parses");

        assert_eq!(input.flush_boundary, SemanticRefreshFlushBoundary::Required);
        assert_eq!(input.flush_boundary.as_str(), "required");
    }
}
