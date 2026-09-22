use crate::{
    app_error::{AppError, AppResult},
    clock::timestamp,
    local_jobs::{LocalJobProgress, LocalJobRegistry},
    semantic_embedder::SemanticEmbeddingProgress,
};
use std::sync::Arc;

#[derive(Clone)]
pub(super) struct SemanticIndexProgressSink {
    jobs: Arc<LocalJobRegistry>,
    job_id: String,
    graph_id: String,
}

impl SemanticIndexProgressSink {
    pub(super) fn new(
        jobs: Arc<LocalJobRegistry>,
        job_id: String,
        graph_id: String,
    ) -> SemanticIndexProgressSink {
        SemanticIndexProgressSink {
            jobs,
            job_id,
            graph_id,
        }
    }

    pub(super) fn report(
        &self,
        phase: &str,
        message: &str,
        current: usize,
        total: usize,
        details: serde_json::Value,
    ) -> AppResult<()> {
        let total = total.max(current);
        let percent = if total == 0 {
            if phase == "complete" || phase == "cancelled" {
                100.0
            } else {
                0.0
            }
        } else {
            (current as f64 / total as f64) * 100.0
        };
        let detail_value = if details.is_object() {
            details
        } else {
            serde_json::json!({})
        };
        self.jobs.update_progress(
            &self.job_id,
            LocalJobProgress {
                phase: phase.to_string(),
                message: message.to_string(),
                current,
                total,
                percent,
                updated_at: timestamp(),
                details: serde_json::json!({
                    "graph_id": self.graph_id.as_str(),
                    "graphId": self.graph_id.as_str(),
                    "extra": detail_value,
                }),
            },
        )?;
        Ok(())
    }

    pub(super) fn is_cancelled(&self) -> AppResult<bool> {
        self.jobs.is_cancelled(&self.job_id)
    }
}

impl SemanticEmbeddingProgress for SemanticIndexProgressSink {
    fn check_cancelled(&self) -> AppResult<()> {
        if self.is_cancelled()? {
            return Err(AppError::internal("semantic index refresh cancelled"));
        }
        Ok(())
    }

    fn report_embedding(
        &self,
        current: usize,
        total: usize,
        model_id: &str,
        dimensions: usize,
        batch_size: usize,
    ) -> AppResult<()> {
        self.report(
            "embedding",
            "Embedding local semantic blocks",
            current,
            total,
            serde_json::json!({
                "modelId": model_id,
                "dimensions": dimensions,
                "batchSize": batch_size,
            }),
        )
    }
}
