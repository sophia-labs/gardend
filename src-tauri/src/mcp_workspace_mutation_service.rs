use crate::app_runtime::AppHandle;
use crate::{
    crdt_projection_flush::flush_graph_projection,
    crdt_queue::{enqueue_crdt_operation, EnqueueCrdtOperationInput},
    graph_projection_service::hosted_graph_entry_from_record,
    graph_service::{update_graph_metadata_service_async, UpdateGraphMetadataInput},
    json_utils::json_string,
    mcp_utils::{mcp_arg_string, mcp_graph_id_or_default, mcp_required_graph_id},
    mcp_workspace_payloads::{edit_comment_payload, wire_create_payload, wire_inputs},
};

pub(super) async fn mcp_local_rename(
    app: AppHandle,
    arguments: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let graph_id = mcp_required_graph_id(arguments)?;
    let entity_type = mcp_arg_string(arguments, &["entity_type", "entityType", "type"])
        .ok_or_else(|| "entity_type is required".to_string())?
        .to_ascii_lowercase();
    let new_name = mcp_arg_string(arguments, &["new_name", "newName", "name", "title"])
        .ok_or_else(|| "new_name is required".to_string())?;

    match entity_type.as_str() {
        "graph" => {
            let graph = update_graph_metadata_service_async(
                &app,
                graph_id.clone(),
                UpdateGraphMetadataInput {
                    title: Some(new_name.clone()),
                    description: None,
                },
            )
            .await
            .map_err(crate::app_error::AppError::message)?;
            Ok(serde_json::json!({
                "success": true,
                "entity_type": "graph",
                "graph_id": graph_id,
                "new_name": new_name,
                "graph": hosted_graph_entry_from_record(graph, true),
            }))
        }
        "document" => {
            let entity_id = mcp_arg_string(
                arguments,
                &["entity_id", "entityId", "document_id", "documentId"],
            )
            .ok_or_else(|| "entity_id is required for document rename".to_string())?;
            let value = enqueue_crdt_operation(
                app.clone(),
                EnqueueCrdtOperationInput {
                    kind: "workspace.updateDocument".to_string(),
                    graph_id: graph_id.clone(),
                    document_id: Some(entity_id.clone()),
                    payload: serde_json::json!({
                        "documentId": entity_id,
                        "title": new_name,
                    }),
                },
            )
            .await?;
            flush_graph_projection(app, &graph_id).await?;
            Ok(serde_json::json!({
                "success": true,
                "entity_type": "document",
                "entity_id": json_string(value.get("documentId").or_else(|| value.get("id"))),
                "graph_id": graph_id,
                "new_name": json_string(value.get("title")),
                "value": value,
            }))
        }
        "folder" => {
            let entity_id = mcp_arg_string(
                arguments,
                &["entity_id", "entityId", "folder_id", "folderId"],
            )
            .ok_or_else(|| "entity_id is required for folder rename".to_string())?;
            let value = enqueue_crdt_operation(
                app.clone(),
                EnqueueCrdtOperationInput {
                    kind: "workspace.updateFolder".to_string(),
                    graph_id: graph_id.clone(),
                    document_id: Some(entity_id.clone()),
                    payload: serde_json::json!({
                        "folderId": entity_id,
                        "name": new_name,
                    }),
                },
            )
            .await?;
            flush_graph_projection(app, &graph_id).await?;
            Ok(serde_json::json!({
                "success": true,
                "entity_type": "folder",
                "entity_id": json_string(value.get("folderId").or_else(|| value.get("id"))),
                "graph_id": graph_id,
                "new_name": json_string(value.get("name")),
                "value": value,
            }))
        }
        _ => Err("entity_type must be graph, document, or folder".to_string()),
    }
}

pub(super) async fn mcp_local_edit_comment(
    app: AppHandle,
    arguments: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let graph_id = mcp_graph_id_or_default(&app, arguments)?;
    let document_id = mcp_arg_string(arguments, &["document_id", "documentId"])
        .ok_or_else(|| "document_id is required".to_string())?;
    let comment_id = mcp_arg_string(arguments, &["comment_id", "commentId"])
        .ok_or_else(|| "comment_id is required".to_string())?;
    let action = mcp_arg_string(arguments, &["action"])
        .ok_or_else(|| "action is required".to_string())?
        .to_ascii_lowercase();
    if action != "set" && action != "resolve" && action != "delete" {
        return Err("action must be set, resolve, or delete".to_string());
    }
    if action == "set" {
        if let Some(object) = arguments.as_object() {
            crate::comment_reader_keys::validate(object)?;
        }
        // A highlight is a mark with no note; any other comment needs text.
        // With no kind in this call, the engine decides from the stored entry.
        let kind = arguments.get("kind").filter(|kind| !kind.is_null());
        if mcp_arg_string(arguments, &["text"]).is_none()
            && kind.map_or(false, |kind| kind.as_str() != Some("highlight"))
        {
            return Err("text is required for action=set".to_string());
        }
    }

    let mut value = enqueue_crdt_operation(
        app,
        EnqueueCrdtOperationInput {
            kind: "document.editComment".to_string(),
            graph_id: graph_id.clone(),
            document_id: Some(document_id.clone()),
            payload: edit_comment_payload(arguments, &document_id, &comment_id, &action),
        },
    )
    .await?;
    if let Some(object) = value.as_object_mut() {
        object.insert("success".to_string(), serde_json::Value::Bool(true));
        object.insert(
            "graph_id".to_string(),
            serde_json::Value::String(graph_id.clone()),
        );
        object.insert("graphId".to_string(), serde_json::Value::String(graph_id));
        object.insert(
            "document_id".to_string(),
            serde_json::Value::String(document_id.clone()),
        );
        object.insert(
            "documentId".to_string(),
            serde_json::Value::String(document_id),
        );
        object.insert(
            "comment_id".to_string(),
            serde_json::Value::String(comment_id.clone()),
        );
        object.insert(
            "commentId".to_string(),
            serde_json::Value::String(comment_id),
        );
    }
    Ok(value)
}

/// A typed-object endpoint must have a type in its graph's real object store.
fn require_wire_object(app: &AppHandle, graph_id: &str, iri: &str) -> Result<(), String> {
    let graph_dir = crate::paths::existing_graph_dir(app, graph_id)?;
    let store = crate::rdf_service::open_graph_store(&graph_dir)?;
    let subject = oxigraph::model::NamedNode::new(iri).map_err(|error| error.to_string())?;
    let rdf_type =
        oxigraph::model::NamedNode::new("http://www.w3.org/1999/02/22-rdf-syntax-ns#type")
            .map_err(|error| error.to_string())?;
    let found = store
        .quads_for_pattern(
            Some(subject.as_ref().into()),
            Some(rdf_type.as_ref()),
            None,
            None,
        )
        .next()
        .transpose()
        .map_err(|error| error.to_string())?
        .is_some();
    if !found {
        return Err(format!("typed object not found in graph {graph_id}: {iri}"));
    }
    Ok(())
}

pub(crate) fn validate_wire_targets(
    app: &AppHandle,
    graph_id: &str,
    wire: &serde_json::Value,
) -> Result<(), String> {
    let target_graph = wire
        .get("targetGraphId")
        .and_then(serde_json::Value::as_str)
        .unwrap_or(graph_id);
    for (prefix, end_graph) in [("source", graph_id), ("target", target_graph)] {
        if let Some(iri) = wire
            .get(format!("{prefix}Iri"))
            .and_then(serde_json::Value::as_str)
        {
            require_wire_object(app, end_graph, iri)?;
        } else {
            let document_id = wire
                .get(format!("{prefix}DocumentId"))
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| format!("{prefix}DocumentId is required"))?;
            let block_id = wire
                .get(format!("{prefix}BlockId"))
                .and_then(serde_json::Value::as_str);
            crate::document_mcp_service::require_document_target(
                app,
                end_graph,
                document_id,
                block_id,
            )?;
        }
    }
    Ok(())
}

pub(super) async fn mcp_local_create_wires(
    app: AppHandle,
    arguments: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let graph_id = mcp_graph_id_or_default(&app, arguments)?;
    let expected = crate::graph_incarnation_admission::expected_incarnation(arguments)
        .map_err(crate::app_error::AppError::message)?;
    let wire_inputs = wire_inputs(arguments);
    if wire_inputs.is_empty() {
        return Err("wires must contain at least one wire object".to_string());
    }

    // Validate all supplied fence carriers before any child is admitted.
    // Execution remains sequential, not one cross-operation transaction.
    let prepared = wire_inputs
        .iter()
        .map(|wire_input| wire_create_payload(wire_input, arguments, &graph_id))
        .collect::<Result<Vec<_>, _>>()?;
    // Keep historical dangling projections readable. Only new MCP writes
    // require live endpoints; the vocabulary imposes no irreflexivity rule.
    for (_, payload) in &prepared {
        validate_wire_targets(&app, &graph_id, payload)?;
    }
    let mut created = Vec::new();
    for (source_document_id, payload) in prepared {
        let value = enqueue_crdt_operation(
            app.clone(),
            EnqueueCrdtOperationInput {
                kind: "workspace.createWire".to_string(),
                graph_id: graph_id.clone(),
                document_id: Some(source_document_id),
                payload,
            },
        )
        .await?;
        created.push(value);
    }
    let created_count = created.len();
    if let Some(expected) = &expected {
        crate::crdt_projection_flush::flush_projection_incarnation(
            app, &graph_id, None, expected, None,
        )
        .await?;
    } else {
        flush_graph_projection(app, &graph_id).await?;
    }

    let mut response = serde_json::json!({
        "success": true,
        "graph_id": graph_id.clone(),
        "graphId": graph_id,
        "wires": created,
        "count": created_count,
        "created_count": created_count,
        "createdCount": created_count,
    });
    if let Some(expected) = expected {
        response["graphIncarnation"] = serde_json::json!(expected);
    }
    Ok(response)
}

#[cfg(all(test, feature = "headless"))]
mod tests {
    use super::*;

    #[test]
    fn async_generic_graph_rename_does_not_block_current_thread_while_lease_is_held() {
        let outcome = crate::mcp_graph_service::async_graph_alias_test_support::assert_alias_yields_while_graph_lease_is_held(
            "garden-mcp-generic-rename-alias-lease",
            "mcp-generic-rename-alias-held-lease",
            |app, graph_id| async move {
                let arguments = serde_json::json!({
                    "graph_id": graph_id,
                    "entity_type": "graph",
                    "new_name": "After generic rename",
                });
                mcp_local_rename(app, &arguments).await
            },
        );
        assert_eq!(outcome["success"], true);
        assert_eq!(outcome["new_name"], "After generic rename");
        assert_eq!(outcome["graph"]["title"], "After generic rename");
    }
}
