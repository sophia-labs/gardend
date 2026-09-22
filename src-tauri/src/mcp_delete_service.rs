use crate::app_runtime::AppHandle;
use crate::{
    crdt_projection_flush::{flush_document_projection, flush_graph_projection},
    crdt_queue::{enqueue_crdt_operation, EnqueueCrdtOperationInput},
    graph_service::soft_delete_graph_service_async,
    mcp_utils::{
        mcp_arg_bool, mcp_arg_string, mcp_arg_string_vec, mcp_block_delete_payload,
        mcp_block_target, mcp_graph_id_or_default,
    },
    mcp_workspace_delete_targets::wire_ids_for_delete_request,
};

pub(super) async fn mcp_local_delete(
    app: AppHandle,
    arguments: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let delete_type = mcp_arg_string(arguments, &["type"])
        .ok_or_else(|| "type is required".to_string())?
        .to_ascii_lowercase();
    let graph_id = mcp_graph_id_or_default(&app, arguments)?;
    let hard = mcp_arg_bool(arguments, &["hard"], true);
    let cascade = mcp_arg_bool(arguments, &["cascade"], false);

    match delete_type.as_str() {
        "documents" | "document" => {
            let mut ids = mcp_arg_string_vec(arguments, &["document_ids", "documentIds"]);
            if let Some(document_id) = mcp_arg_string(arguments, &["document_id", "documentId"]) {
                ids.push(document_id);
            }
            ids.sort();
            ids.dedup();
            if ids.is_empty() {
                return Err("document_id or document_ids is required".to_string());
            }
            let mut deleted = Vec::new();
            for document_id in ids {
                let value = enqueue_crdt_operation(
                    app.clone(),
                    EnqueueCrdtOperationInput {
                        kind: "workspace.deleteDocument".to_string(),
                        graph_id: graph_id.clone(),
                        document_id: Some(document_id.clone()),
                        payload: serde_json::json!({ "hard": hard }),
                    },
                )
                .await?;
                deleted.push(serde_json::json!({
                    "document_id": document_id,
                    "documentId": document_id,
                    "value": value,
                }));
            }
            flush_graph_projection(app, &graph_id).await?;
            Ok(serde_json::json!({
                "success": true,
                "type": "documents",
                "graph_id": graph_id,
                "hard": hard,
                "deleted": deleted,
                "deleted_count": deleted.len(),
            }))
        }
        "blocks" | "block" => {
            let (graph_id, document_id) = mcp_block_target(arguments)?;
            let payload = serde_json::to_value(mcp_block_delete_payload(arguments))
                .expect("McpBlockDeletePayload always serializes");
            let value = enqueue_crdt_operation(
                app.clone(),
                EnqueueCrdtOperationInput {
                    kind: "block.delete".to_string(),
                    graph_id: graph_id.clone(),
                    document_id: Some(document_id.clone()),
                    payload,
                },
            )
            .await?;
            flush_document_projection(app, &graph_id, &document_id).await?;
            Ok(serde_json::json!({
                "success": true,
                "type": "blocks",
                "graph_id": graph_id,
                "document_id": document_id,
                "value": value,
            }))
        }
        "folder" | "folders" => {
            let folder_id = mcp_arg_string(arguments, &["folder_id", "folderId", "id"])
                .ok_or_else(|| "folder_id is required".to_string())?;
            let value = enqueue_crdt_operation(
                app.clone(),
                EnqueueCrdtOperationInput {
                    kind: "workspace.deleteFolder".to_string(),
                    graph_id: graph_id.clone(),
                    document_id: Some(folder_id.clone()),
                    payload: serde_json::json!({
                        "folderId": folder_id,
                        "cascade": cascade,
                        "hard": hard,
                    }),
                },
            )
            .await?;
            flush_graph_projection(app, &graph_id).await?;
            Ok(serde_json::json!({
                "success": true,
                "type": "folder",
                "graph_id": graph_id,
                "folder_id": folder_id,
                "value": value,
            }))
        }
        "wires" | "wire" => {
            let ids = wire_ids_for_delete_request(&app, &graph_id, arguments)?;
            if ids.is_empty() {
                return Err("no matching wires found".to_string());
            }
            let mut deleted = Vec::new();
            for wire_id in ids {
                let value = enqueue_crdt_operation(
                    app.clone(),
                    EnqueueCrdtOperationInput {
                        kind: "workspace.deleteWire".to_string(),
                        graph_id: graph_id.clone(),
                        document_id: None,
                        payload: serde_json::json!({ "wireId": wire_id }),
                    },
                )
                .await?;
                deleted.push(serde_json::json!({
                    "wire_id": wire_id,
                    "wireId": wire_id,
                    "value": value,
                }));
            }
            flush_graph_projection(app, &graph_id).await?;
            Ok(serde_json::json!({
                "success": true,
                "type": "wires",
                "graph_id": graph_id,
                "deleted": deleted,
                "deleted_count": deleted.len(),
            }))
        }
        "graph" | "graphs" => {
            let value = soft_delete_graph_service_async(&app, graph_id.clone(), hard)
                .await
                .map_err(crate::app_error::AppError::message)?;
            Ok(serde_json::json!({
                "success": true,
                "type": "graph",
                "graph_id": graph_id,
                "hard": hard,
                "value": value,
            }))
        }
        _ => Err("type must be documents, blocks, folder, wires, or graph".to_string()),
    }
}

#[cfg(all(test, feature = "headless"))]
mod tests {
    use super::*;

    #[test]
    fn async_generic_graph_delete_does_not_block_current_thread_while_lease_is_held() {
        let outcome = crate::mcp_graph_service::async_graph_alias_test_support::assert_alias_yields_while_graph_lease_is_held(
            "garden-mcp-generic-delete-alias-lease",
            "mcp-generic-delete-alias-held-lease",
            |app, graph_id| async move {
                let arguments = serde_json::json!({
                    "graph_id": graph_id,
                    "type": "graph",
                    "hard": false,
                });
                mcp_local_delete(app, &arguments).await
            },
        );
        assert_eq!(outcome["success"], true);
        assert_eq!(outcome["type"], "graph");
        assert_eq!(outcome["hard"], false);
    }
}
