use crate::app_runtime::AppHandle;
use crate::{
    crdt_queue::{enqueue_crdt_operation, EnqueueCrdtOperationInput},
    mcp_utils::{
        mcp_block_delete_payload, mcp_block_edit_text_payload, mcp_block_insert_payload,
        mcp_block_target, mcp_block_update_payload,
    },
};

pub(super) async fn mcp_local_insert_blocks(
    app: AppHandle,
    arguments: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let (graph_id, document_id) = mcp_block_target(arguments)?;
    let payload = serde_json::to_value(mcp_block_insert_payload(arguments))
        .expect("McpBlockInsertPayload always serializes");
    enqueue_crdt_operation(
        app,
        EnqueueCrdtOperationInput {
            kind: "block.insert".to_string(),
            graph_id,
            document_id: Some(document_id),
            payload,
        },
    )
    .await
}

pub(super) async fn mcp_local_update_blocks(
    app: AppHandle,
    arguments: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let (graph_id, document_id) = mcp_block_target(arguments)?;
    let payload = serde_json::to_value(mcp_block_update_payload(arguments))
        .expect("McpBlockUpdatePayload always serializes");
    enqueue_crdt_operation(
        app,
        EnqueueCrdtOperationInput {
            kind: "block.update".to_string(),
            graph_id,
            document_id: Some(document_id),
            payload,
        },
    )
    .await
}

pub(super) async fn mcp_local_edit_block_text(
    app: AppHandle,
    arguments: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let (graph_id, document_id) = mcp_block_target(arguments)?;
    let payload = serde_json::to_value(mcp_block_edit_text_payload(arguments))
        .expect("McpBlockEditTextPayload always serializes");
    enqueue_crdt_operation(
        app,
        EnqueueCrdtOperationInput {
            kind: "block.editText".to_string(),
            graph_id,
            document_id: Some(document_id),
            payload,
        },
    )
    .await
}

pub(super) async fn mcp_local_delete_blocks(
    app: AppHandle,
    arguments: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let (graph_id, document_id) = mcp_block_target(arguments)?;
    let payload = serde_json::to_value(mcp_block_delete_payload(arguments))
        .expect("McpBlockDeletePayload always serializes");
    enqueue_crdt_operation(
        app,
        EnqueueCrdtOperationInput {
            kind: "block.delete".to_string(),
            graph_id,
            document_id: Some(document_id),
            payload,
        },
    )
    .await
}
