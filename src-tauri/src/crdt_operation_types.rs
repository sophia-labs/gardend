use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CrdtOperation {
    pub(crate) operation_id: String,
    pub(crate) kind: String,
    pub(crate) graph_id: String,
    pub(crate) document_id: Option<String>,
    #[serde(default)]
    pub(crate) payload: serde_json::Value,
    pub(crate) enqueue_timestamp: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct EnqueueCrdtOperationInput {
    pub(crate) kind: String,
    pub(crate) graph_id: String,
    pub(crate) document_id: Option<String>,
    #[serde(default)]
    pub(crate) payload: serde_json::Value,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct CrdtOperationResult {
    pub(super) ok: bool,
    pub(super) value: Option<serde_json::Value>,
    pub(super) error: Option<String>,
    pub(super) timing: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CompleteCrdtOperationInput {
    pub(crate) operation_id: String,
    pub(crate) ok: bool,
    pub(crate) value: Option<serde_json::Value>,
    pub(crate) error: Option<String>,
    pub(crate) timing: Option<serde_json::Value>,
}

pub(crate) struct EnqueuedCrdtOutcome {
    pub(crate) operation_id: String,
    pub(crate) value: serde_json::Value,
}
