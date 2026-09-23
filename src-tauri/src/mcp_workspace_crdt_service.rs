use crate::app_runtime::AppHandle;
use crate::{
    crdt_projection_flush::flush_graph_projection,
    crdt_queue::{enqueue_crdt_operation, EnqueueCrdtOperationInput},
    mcp_utils::{mcp_arg_string, mcp_graph_id_or_default, mcp_required_graph_id},
    mcp_workspace_payloads::{
        create_document_payload, create_folder_payload, mcp_raw_graph_id_argument,
        move_documents_payload, move_folder_payload,
    },
};

pub(super) async fn mcp_local_move_folder(
    app: AppHandle,
    arguments: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let graph_id = mcp_required_graph_id(arguments)?;
    let folder_id = mcp_arg_string(arguments, &["folder_id", "folderId", "id"])
        .ok_or_else(|| "folder_id is required".to_string())?;
    let value = enqueue_crdt_operation(
        app.clone(),
        EnqueueCrdtOperationInput {
            kind: "workspace.moveFolder".to_string(),
            graph_id: graph_id.clone(),
            document_id: None,
            payload: move_folder_payload(arguments, folder_id),
        },
    )
    .await?;
    flush_graph_projection(app, &graph_id).await?;
    Ok(value)
}

pub(super) async fn mcp_local_make_document_editable(
    app: AppHandle,
    arguments: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let graph_id = mcp_graph_id_or_default(&app, arguments)?;
    let document_id = mcp_arg_string(arguments, &["document_id", "documentId"])
        .ok_or_else(|| "document_id is required".to_string())?;
    let value = enqueue_crdt_operation(
        app.clone(),
        EnqueueCrdtOperationInput {
            kind: "workspace.updateDocument".to_string(),
            graph_id: graph_id.clone(),
            document_id: Some(document_id.clone()),
            payload: serde_json::json!({
                "documentId": document_id,
                "readOnly": false,
            }),
        },
    )
    .await?;
    flush_graph_projection(app, &graph_id).await?;
    Ok(serde_json::json!({
        "success": true,
        "graph_id": graph_id.clone(),
        "graphId": graph_id,
        "document_id": document_id.clone(),
        "documentId": document_id,
        "readOnly": false,
        "value": value,
    }))
}

pub(super) async fn mcp_local_create_document(
    app: AppHandle,
    arguments: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let graph_id = mcp_raw_graph_id_argument(arguments);
    let value = enqueue_crdt_operation(
        app.clone(),
        EnqueueCrdtOperationInput {
            kind: "workspace.createDocument".to_string(),
            graph_id: graph_id.clone(),
            document_id: None,
            payload: create_document_payload(arguments),
        },
    )
    .await?;
    flush_graph_projection(app, &graph_id).await?;
    Ok(value)
}

pub(super) async fn mcp_local_delete_document(
    app: AppHandle,
    arguments: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    // A missing id was an empty string here, which the engine then failed on
    // with a message about a document that "does not exist". Say what is
    // actually wrong before anything is enqueued.
    let document_id = arguments
        .get("documentId")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "documentId is required".to_string())?
        .to_string();
    let graph_id = mcp_raw_graph_id_argument(arguments);
    let value = enqueue_crdt_operation(
        app.clone(),
        EnqueueCrdtOperationInput {
            kind: "workspace.deleteDocument".to_string(),
            graph_id: graph_id.clone(),
            document_id: Some(document_id),
            payload: serde_json::json!({}),
        },
    )
    .await?;
    flush_graph_projection(app, &graph_id).await?;
    Ok(value)
}

pub(super) async fn mcp_local_create_folder(
    app: AppHandle,
    arguments: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let graph_id = mcp_raw_graph_id_argument(arguments);
    let value = enqueue_crdt_operation(
        app.clone(),
        EnqueueCrdtOperationInput {
            kind: "workspace.createFolder".to_string(),
            graph_id: graph_id.clone(),
            document_id: None,
            payload: create_folder_payload(arguments),
        },
    )
    .await?;
    flush_graph_projection(app, &graph_id).await?;
    Ok(value)
}

pub(super) async fn mcp_local_move_documents(
    app: AppHandle,
    arguments: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let graph_id = mcp_raw_graph_id_argument(arguments);
    let value = enqueue_crdt_operation(
        app.clone(),
        EnqueueCrdtOperationInput {
            kind: "workspace.moveDocuments".to_string(),
            graph_id: graph_id.clone(),
            document_id: None,
            payload: move_documents_payload(arguments),
        },
    )
    .await?;
    flush_graph_projection(app, &graph_id).await?;
    Ok(value)
}

pub(super) async fn mcp_local_flush_crdt(
    app: AppHandle,
    arguments: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let document_id = arguments
        .get("documentId")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);
    enqueue_crdt_operation(
        app,
        EnqueueCrdtOperationInput {
            kind: "crdt.flush".to_string(),
            graph_id: mcp_raw_graph_id_argument(arguments),
            document_id,
            payload: serde_json::json!({}),
        },
    )
    .await
}

pub(super) async fn mcp_local_crdt_operation(
    app: AppHandle,
    arguments: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let kind = arguments
        .get("kind")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "kind is required".to_string())?
        .to_string();
    let document_id = arguments
        .get("documentId")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);
    let payload = arguments
        .get("payload")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));
    enqueue_crdt_operation(
        app,
        EnqueueCrdtOperationInput {
            kind,
            graph_id: mcp_raw_graph_id_argument(arguments),
            document_id,
            payload,
        },
    )
    .await
}
