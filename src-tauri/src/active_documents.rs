use crate::app_runtime::AppHandle;
use crate::{
    crdt_queue::{enqueue_crdt_operation_outcome, EnqueueCrdtOperationInput},
    BlockSnapshot,
};
use serde::Deserialize;
use std::{collections::HashSet, sync::Mutex, time::Duration};
#[cfg(feature = "desktop")]
use tauri::Manager;

const LIVE_PROJECTION_TIMEOUT_SECS: u64 = 5;

/// Tracks which (graph_id, document_id) pairs the TypeScript runtime currently
/// holds warm in its document-channel pool. The Rust read handlers consult
/// this to decide whether a request can be served by an in-memory live
/// projection (via the `document.liveProjection` CRDT op) or must fall through
/// to the disk-backed projection at `<graph>/documents/<doc>/document.json`.
///
/// Mirrors the Hocuspocus-style "live session vs async materialization" split:
/// reads against an active session see live state; everything else is
/// eventually consistent against the cold-flushed projection.
#[derive(Default)]
pub(crate) struct ActiveDocumentRegistry {
    inner: Mutex<HashSet<(String, String)>>,
}

impl ActiveDocumentRegistry {
    pub(crate) fn replace(&self, documents: Vec<(String, String)>) -> Result<(), String> {
        let mut guard = self
            .inner
            .lock()
            .map_err(|_| "active document registry unavailable: mutex poisoned".to_string())?;
        guard.clear();
        guard.extend(documents);
        Ok(())
    }

    pub(crate) fn contains(&self, graph_id: &str, document_id: &str) -> bool {
        let Ok(guard) = self.inner.lock() else {
            return false;
        };
        guard.contains(&(graph_id.to_string(), document_id.to_string()))
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ActiveDocumentInput {
    graph_id: String,
    document_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SetActiveDocumentsInput {
    documents: Vec<ActiveDocumentInput>,
}

/// Replace the registry of "live" documents currently held in the TS runtime's
/// channel pool. The TS runtime calls this whenever the pool's keyset changes
/// (channel attach, eviction, shutdown). Used by the read-side
/// `try_live_projection` helper to decide whether to route a read through
/// the in-memory Y.Doc or fall through to disk.
#[cfg_attr(feature = "desktop", tauri::command)]
pub(crate) fn set_active_documents(
    app: AppHandle,
    input: SetActiveDocumentsInput,
) -> Result<(), String> {
    let registry = app.state::<ActiveDocumentRegistry>();
    let documents = input
        .documents
        .into_iter()
        .filter(|entry| !entry.graph_id.is_empty() && !entry.document_id.is_empty())
        .map(|entry| (entry.graph_id, entry.document_id))
        .collect();
    registry.replace(documents)
}

pub(crate) struct LiveProjection {
    pub(crate) blocks: Vec<BlockSnapshot>,
    #[allow(dead_code)]
    pub(crate) title: String,
    #[allow(dead_code)]
    pub(crate) body: String,
}

#[derive(Debug, Deserialize)]
struct LiveProjectionEnvelope {
    #[serde(default)]
    active: bool,
    #[serde(default)]
    blocks: Option<Vec<BlockSnapshot>>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    body: Option<String>,
}

/// Returns Some(LiveProjection) if the requested document is currently held
/// warm in the TS runtime's channel pool — indicating that the in-memory
/// Y.Doc is the source of truth and the disk projection may lag. Returns
/// None for unpooled docs, when the round-trip to TS fails, or when the
/// timeout elapses (in which case callers fall through to the
/// disk-backed read).
pub(crate) async fn try_live_projection(
    app: &AppHandle,
    graph_id: &str,
    document_id: &str,
) -> Option<LiveProjection> {
    let registry = app.state::<ActiveDocumentRegistry>();
    if !registry.contains(graph_id, document_id) {
        return None;
    }
    let outcome = tokio::time::timeout(
        Duration::from_secs(LIVE_PROJECTION_TIMEOUT_SECS),
        enqueue_crdt_operation_outcome(
            app.clone(),
            EnqueueCrdtOperationInput {
                kind: "document.liveProjection".to_string(),
                graph_id: graph_id.to_string(),
                document_id: Some(document_id.to_string()),
                payload: serde_json::json!({}),
            },
        ),
    )
    .await;
    match outcome {
        Ok(Ok(value)) => match serde_json::from_value::<LiveProjectionEnvelope>(value.value) {
            Ok(envelope) if envelope.active => Some(LiveProjection {
                blocks: envelope.blocks.unwrap_or_default(),
                title: envelope.title.unwrap_or_default(),
                body: envelope.body.unwrap_or_default(),
            }),
            _ => None,
        },
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_projection_timeout_allows_slow_active_reads() {
        assert!(
            LIVE_PROJECTION_TIMEOUT_SECS >= 5,
            "active document reads fell back below the read-after-write consistency budget"
        );
    }

    #[test]
    fn replace_swaps_active_document_set() {
        let registry = ActiveDocumentRegistry::default();
        registry
            .replace(vec![("graph-1".to_string(), "doc-1".to_string())])
            .expect("replace active documents");

        assert!(registry.contains("graph-1", "doc-1"));
        assert!(!registry.contains("graph-1", "doc-2"));

        registry
            .replace(vec![("graph-1".to_string(), "doc-2".to_string())])
            .expect("replace active documents");

        assert!(!registry.contains("graph-1", "doc-1"));
        assert!(registry.contains("graph-1", "doc-2"));
    }

    #[test]
    fn poisoned_registry_degrades_to_cold_reads() {
        let registry = ActiveDocumentRegistry::default();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = registry.inner.lock().expect("lock registry");
            panic!("poison active document registry");
        }));
        assert!(result.is_err());

        assert!(!registry.contains("graph-1", "doc-1"));
        assert!(registry
            .replace(vec![("graph-1".to_string(), "doc-1".to_string())])
            .is_err());
    }

    #[test]
    fn live_projection_envelope_defaults_missing_optional_fields() {
        let envelope = serde_json::from_value::<LiveProjectionEnvelope>(serde_json::json!({
            "active": true
        }))
        .expect("envelope should deserialize");

        assert!(envelope.active);
        assert!(envelope.blocks.is_none());
        assert!(envelope.title.is_none());
        assert!(envelope.body.is_none());
    }
}
