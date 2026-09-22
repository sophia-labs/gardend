use crate::app_runtime::AppHandle;
use crate::{
    document_projection_service::document_snippet,
    document_service::DocumentRecord,
    geist_memory_store::{memory_json, memory_sort_key, memory_text, read_memory_store},
    mcp_utils::{mcp_arg_string, mcp_arg_u64, mcp_arg_usize, mcp_graph_id_or_default},
    paths::existing_graph_dir,
};

pub(super) fn mcp_local_recent_document_recall(
    documents: &[DocumentRecord],
    limit: usize,
) -> serde_json::Value {
    let memories = documents
        .iter()
        .take(limit.min(20))
        .enumerate()
        .map(|(index, document)| {
            let preview =
                document_snippet(document).unwrap_or_else(|| "No preview available".to_string());
            serde_json::json!({
                "number": index + 1,
                "text": format!("{}: {}", document.title, preview),
                "document_id": document.document_id.clone(),
                "documentId": document.document_id.clone(),
                "title": document.title.clone(),
                "created_at": document.created_at.clone(),
                "createdAt": document.created_at.clone(),
                "last_active": document.updated_at.clone(),
                "lastActive": document.updated_at.clone(),
                "source": "local-recent-document",
            })
        })
        .collect::<Vec<_>>();
    let count = memories.len();
    serde_json::json!({
        "memories": memories,
        "count": count,
        "source": "local-recent-documents",
        "memory_queue_available": false,
        "memoryQueueAvailable": false,
    })
}

pub(super) fn mcp_local_recall_memories(
    app: AppHandle,
    arguments: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let graph_id = mcp_graph_id_or_default(&app, arguments)?;
    let graph_dir = existing_graph_dir(&app, &graph_id)?;
    let store = read_memory_store(&graph_dir, &graph_id)?;
    let number = mcp_arg_u64(arguments, &["number"]);
    let query = mcp_arg_string(arguments, &["query"]);
    let limit = mcp_arg_usize(arguments, &["limit"], 5).clamp(0, 100);
    let observer = mcp_arg_string(arguments, &["observer_agent_id", "observerAgentId"])
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());

    // Per-observer recall (Variant B): when the caller supplies an observer identity
    // (the agt:Agent IRI the choreograph `withObserver` seam injects), read from THAT
    // witness's perspective graph via the current-head SPARQL path (`recall_for_observer`)
    // — never the flat shared queue, which would leak other agents' memories. Commons
    // recall (no observer) keeps the legacy behavior. Note: `number` lookup is not
    // supported per-observer; an observer-scoped call returns the current-head/query
    // recall from that perspective graph.
    if let Some(observer) = observer.as_deref() {
        let hits = crate::emporium::memory_applier::recall_for_observer(
            &app,
            &graph_id,
            observer,
            query.as_deref().unwrap_or(""),
            limit,
        )?;
        let memories = hits
            .iter()
            .enumerate()
            .map(|(index, hit)| {
                serde_json::json!({
                    "number": index + 1,
                    "text": hit.content.clone(),
                    "content": hit.content.clone(),
                    "subject": hit.subject.clone(),
                    "created_at": hit.created_at,
                    "createdAt": hit.created_at,
                    "observer_agent_id": observer,
                    "observedBy": observer,
                    "source": "per-observer-projection",
                })
            })
            .collect::<Vec<_>>();
        let count = memories.len();
        return Ok(serde_json::json!({
            "memories": memories,
            "count": count,
            "source": "per-observer-projection",
            "observer_agent_id": observer,
        }));
    }

    if let Some(number) = number {
        let memory = store.memories.get(&number.to_string());
        let memories = memory
            .map(|memory| vec![memory_json(memory)])
            .unwrap_or_default();
        let count = memories.len();
        let mut output = serde_json::json!({
            "memories": memories,
            "count": count,
            "source": "local-memory-store",
        });
        if memory.is_none() {
            output["note"] = serde_json::json!(format!("Memory #{number} not found"));
        }
        return Ok(output);
    }

    let query_lower = query.as_ref().map(|value| value.to_ascii_lowercase());
    let mut memories = store
        .memories
        .values()
        .filter(|memory| {
            query_lower
                .as_ref()
                .map(|query| memory_text(memory).to_ascii_lowercase().contains(query))
                .unwrap_or(true)
        })
        .cloned()
        .collect::<Vec<_>>();
    memories.sort_by(|left, right| memory_sort_key(right).cmp(memory_sort_key(left)));
    memories.truncate(limit);
    let rows = memories.iter().map(memory_json).collect::<Vec<_>>();
    let count = rows.len();
    Ok(serde_json::json!({
        "memories": rows,
        "count": count,
        "source": "local-memory-store",
    }))
}

pub(super) fn mcp_local_recall_or_recent_documents(
    app: AppHandle,
    graph_id: &str,
    documents: &[DocumentRecord],
    limit: usize,
) -> serde_json::Value {
    let arguments = serde_json::json!({
        "graphId": graph_id,
        "limit": limit,
    });
    match mcp_local_recall_memories(app, &arguments) {
        Ok(value)
            if value
                .get("count")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0)
                > 0 =>
        {
            value
        }
        _ => mcp_local_recent_document_recall(documents, limit),
    }
}
