use crate::app_runtime::AppHandle;
use crate::{
    crdt_projection_flush::flush_document_projection_phase,
    crdt_queue::{enqueue_crdt_operation_outcome, EnqueueCrdtOperationInput},
    document_mcp_write_payloads::{
        mcp_write_document_payload, mcp_write_document_response, write_durability_verdict,
    },
    document_projection_service::{
        document_blocks_for_read, hosted_document_response, hosted_document_write_payload,
    },
    document_service::{read_document, read_document_record},
    ids::validate_local_id,
    mcp_utils::{mcp_arg_bool, mcp_required_document_id, mcp_required_graph_id},
    paths::{documents_dir, existing_graph_dir},
};
use uuid::Uuid;

pub(super) fn current_document_revision(
    app: &AppHandle,
    graph_id: &str,
    document_id: &str,
) -> Result<u64, String> {
    let graph_dir = existing_graph_dir(app, graph_id)?;
    validate_local_id(document_id, "document_id")?;
    let document_dir = documents_dir(&graph_dir).join(document_id);
    if !document_dir.is_dir() {
        return Ok(0);
    }
    read_document_record(&graph_dir, &document_dir.join("document.json"))
        .map(|document| document.revision)
}

pub(super) async fn hosted_duplicate_document_result(
    app: &AppHandle,
    graph_id: &str,
    source_document_id: &str,
) -> Result<serde_json::Value, String> {
    let source = read_document(
        app.clone(),
        graph_id.to_string(),
        source_document_id.to_string(),
    )?;
    let source_envelope =
        serde_json::to_value(hosted_document_response(app, graph_id, source_document_id)?)
            .map_err(|error| format!("serialize source envelope: {error}"))?;
    let new_document_id = Uuid::new_v4()
        .simple()
        .to_string()
        .chars()
        .take(12)
        .collect::<String>();
    let title = duplicate_document_title(&source.title);
    let parent_id = source_envelope
        .get("parentId")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let blocks = serde_json::to_value(document_blocks_for_read(&source))
        .map_err(|error| format!("serialize duplicate blocks: {error}"))?;
    let payload = hosted_document_write_payload(
        &new_document_id,
        serde_json::json!({
            "title": title,
            "parentId": parent_id,
            "blocks": blocks,
        }),
    )?;

    let outcome = enqueue_crdt_operation_outcome(
        app.clone(),
        EnqueueCrdtOperationInput {
            kind: "document.write".to_string(),
            graph_id: graph_id.to_string(),
            document_id: Some(new_document_id.clone()),
            payload,
        },
    )
    .await?;
    flush_document_projection_phase(
        app.clone(),
        graph_id,
        &new_document_id,
        &outcome.operation_id,
        "duplicateDocumentWorkspaceFlushMs",
    )
    .await?;

    let document = hosted_document_response(app, graph_id, &new_document_id)?;
    Ok(serde_json::json!({
        "graph_id": graph_id,
        "graphId": graph_id,
        "document_id": new_document_id,
        "documentId": new_document_id,
        "source_document_id": source_document_id,
        "sourceDocumentId": source_document_id,
        "title": title,
        "document": document,
    }))
}

pub(super) async fn mcp_local_write_document(
    app: AppHandle,
    arguments: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let graph_id = mcp_required_graph_id(arguments)?;
    let document_id = mcp_required_document_id(arguments)?;
    let payload = mcp_write_document_payload(arguments, &document_id)?;

    let outcome = enqueue_crdt_operation_outcome(
        app.clone(),
        EnqueueCrdtOperationInput {
            kind: "document.write".to_string(),
            graph_id: graph_id.clone(),
            document_id: Some(document_id.clone()),
            payload,
        },
    )
    .await?;
    flush_document_projection_phase(
        app.clone(),
        &graph_id,
        &document_id,
        &outcome.operation_id,
        "mcpWriteDocumentWorkspaceFlushMs",
    )
    .await?;

    // Durability verdict — bound to actual events, never to the request flag
    // (`awaitDurable` selects whether the check is REPORTED; it cannot
    // manufacture a positive). Both awaits above have completed, and every
    // persistence transaction they performed marked its completion epoch
    // under the durability write guard before its operation resolved, so
    // `current_write_epoch()` here is >= every mark this write produced.
    // Reading the resolved watermark AFTER the write epoch keeps the claim
    // sound: the watermark only ever advances to epochs captured before a
    // completed flush's dirty computation, so `resolved >= write_epoch`
    // implies that flush began after this write fully persisted locally —
    // i.e. the published (or confirmed-already-published) snapshot contains
    // it. When the durable plane cannot pay the claim yet, the response says
    // so (`durabilityChecked=false`, `durability="pending"`) instead of
    // promising; the periodic dirty-driven flusher still captures the write
    // within its debounce/max-RPO bounds. And when the write-lease fence has
    // discarded the write's range (the lease went terminal with the write
    // acked but unflushed — the 2026-08-21 incident), the response answers
    // `durability="revoked"` from the revocation watermark recorded in the
    // same stroke as the discard, instead of pending-forever.
    let requested_durability_check =
        mcp_arg_bool(arguments, &["awaitDurable", "await_durable"], true);
    let write_epoch = crate::cell_durability::current_write_epoch();
    let durability = write_durability_verdict(
        requested_durability_check,
        crate::cell_durability::durable_plane_semantics(),
        crate::cell_durability::durability_watermarks(),
        write_epoch,
    );

    let document = serde_json::to_value(hosted_document_response(&app, &graph_id, &document_id)?)
        .map_err(|error| format!("serialize document envelope: {error}"))?;
    Ok(mcp_write_document_response(
        arguments,
        &graph_id,
        &document_id,
        &document,
        &outcome.value,
        durability,
    ))
}

fn duplicate_document_title(source_title: &str) -> String {
    let source_title = if source_title.trim().is_empty() {
        "Untitled"
    } else {
        source_title
    };
    format!("Copy of {source_title}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duplicate_document_title_uses_untitled_for_blank_sources() {
        assert_eq!(duplicate_document_title("Original"), "Copy of Original");
        assert_eq!(duplicate_document_title("   "), "Copy of Untitled");
    }
}
