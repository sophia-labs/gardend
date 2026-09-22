use crate::app_runtime::AppHandle;
use crate::{
    document_service::read_graph_documents_cold,
    local_jobs::LocalJobRegistry,
    paths::existing_graph_dir,
    rdf_service::ensure_graph_store_seeded,
    semantic_service::{
        semantic_model_status, submit_semantic_index_refresh_job, RefreshSemanticIndexInput,
    },
};
use serde::Deserialize;
use std::sync::Arc;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RematerializeGraphInput {
    #[serde(alias = "graph_id")]
    pub(crate) graph_id: String,
    #[serde(default, alias = "reindex_after")]
    pub(crate) reindex_after: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ReindexGraphInput {
    #[serde(alias = "graph_id")]
    pub(crate) graph_id: String,
}

pub(crate) fn reindex_graph_job_response(
    app: AppHandle,
    jobs: Arc<LocalJobRegistry>,
    input: ReindexGraphInput,
) -> Result<serde_json::Value, String> {
    let model_status = semantic_model_status(&app)?;
    if model_status.setup_required {
        return Err(
            "local embedding model is not prepared; run prepare_semantic_model first".to_string(),
        );
    }

    let response = submit_semantic_index_refresh_job(
        app,
        jobs,
        RefreshSemanticIndexInput {
            graph_id: input.graph_id.clone(),
            flush_boundary: Default::default(),
        },
    )?;
    let value = serde_json::to_value(response)
        .map_err(|error| format!("serialize reindex job: {error}"))?;
    Ok(reindex_job_response_value(&input.graph_id, value))
}

pub(crate) fn rematerialize_graph_response(
    app: AppHandle,
    input: RematerializeGraphInput,
) -> Result<serde_json::Value, String> {
    let graph_dir = existing_graph_dir(&app, &input.graph_id)?;
    let documents = read_graph_documents_cold(&graph_dir)?;
    let _reindex_after = input.reindex_after;
    ensure_graph_store_seeded(&graph_dir)?;
    Ok(serde_json::json!({
        "graph_id": input.graph_id,
        "total_docs": documents.len(),
        "materialized": documents.len(),
        "skipped": 0,
        "errors": 0,
        "reindex_queued": false,
    }))
}

fn reindex_job_response_value(graph_id: &str, mut value: serde_json::Value) -> serde_json::Value {
    let document_count = value
        .pointer("/detail/documentCount")
        .or_else(|| value.pointer("/detail/document_count"))
        .and_then(serde_json::Value::as_u64)
        .unwrap_or_default();
    if let Some(object) = value.as_object_mut() {
        object.insert("graph_id".to_string(), serde_json::json!(graph_id));
        object.insert("total_docs".to_string(), serde_json::json!(document_count));
        object.insert("queued".to_string(), serde_json::json!(document_count));
        object.insert("async".to_string(), serde_json::json!(true));
        object.insert("setup_required".to_string(), serde_json::json!(false));
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn graph_maintenance_inputs_accept_snake_case_aliases() {
        let rematerialize: RematerializeGraphInput = serde_json::from_value(serde_json::json!({
            "graph_id": "graph-a",
            "reindex_after": true,
        }))
        .unwrap();
        let reindex: ReindexGraphInput =
            serde_json::from_value(serde_json::json!({ "graph_id": "graph-a" })).unwrap();

        assert_eq!(rematerialize.graph_id, "graph-a");
        assert!(rematerialize.reindex_after);
        assert_eq!(reindex.graph_id, "graph-a");
    }

    #[test]
    fn reindex_job_response_preserves_legacy_envelope_fields() {
        let value = reindex_job_response_value(
            "graph-a",
            serde_json::json!({
                "job_id": "job-1",
                "status": "queued",
                "detail": {
                    "documentCount": 3,
                    "staleDocumentCount": 2
                }
            }),
        );

        assert_eq!(
            value.get("job_id").and_then(serde_json::Value::as_str),
            Some("job-1")
        );
        assert_eq!(
            value.get("graph_id").and_then(serde_json::Value::as_str),
            Some("graph-a")
        );
        assert_eq!(
            value.get("total_docs").and_then(serde_json::Value::as_u64),
            Some(3)
        );
        assert_eq!(
            value.get("queued").and_then(serde_json::Value::as_u64),
            Some(3)
        );
        assert_eq!(
            value.get("async").and_then(serde_json::Value::as_bool),
            Some(true)
        );
        assert_eq!(
            value
                .get("setup_required")
                .and_then(serde_json::Value::as_bool),
            Some(false)
        );
    }
}
