use crate::app_runtime::AppHandle;
use crate::{
    crdt_projection_flush::flush_graph_projection,
    crdt_queue::{enqueue_crdt_operation, EnqueueCrdtOperationInput},
    document_mutation_service::current_document_revision,
    document_projection_service::{hosted_document_response, hosted_document_write_payload},
    hosted_entity_payloads::{
        normalize_entity_artifact_payload, normalize_entity_folder_payload,
        require_hosted_entity_string,
    },
    hosted_entity_read_service::{hosted_entity_response, validate_hosted_entity_type},
    json_utils::json_u64,
    loopback_http::loopback_error,
    original_file_service::delete_artifact_original_files,
};
use axum::{http::StatusCode, response::Response};

fn hosted_entity_delete_response(
    graph_id: &str,
    entity_type: &str,
    entity_id: &str,
) -> serde_json::Value {
    serde_json::json!({
        "id": entity_id,
        "graphId": graph_id,
        "graph_id": graph_id,
        "entityType": entity_type,
        "entity_type": entity_type,
        "status": "deleted",
    })
}

pub(super) async fn put_hosted_entity(
    app: AppHandle,
    graph_id: String,
    entity_type: String,
    entity_id: String,
    mut input: serde_json::Value,
) -> Result<serde_json::Value, String> {
    validate_hosted_entity_type(&entity_type)?;
    match entity_type.as_str() {
        "document" => {
            if let Some(expected_revision) = json_u64(
                input
                    .get("expectedRevision")
                    .or_else(|| input.get("expected_revision")),
            ) {
                let current_revision = current_document_revision(&app, &graph_id, &entity_id)?;
                if current_revision != expected_revision {
                    return Err(format!(
                        "revision mismatch: expected {expected_revision}, actual {current_revision}"
                    ));
                }
            }
            let payload = hosted_document_write_payload(&entity_id, input)?;
            enqueue_crdt_operation(
                app.clone(),
                EnqueueCrdtOperationInput {
                    kind: "document.write".to_string(),
                    graph_id: graph_id.clone(),
                    document_id: Some(entity_id.clone()),
                    payload,
                },
            )
            .await?;
            flush_graph_projection(app.clone(), &graph_id).await?;
            hosted_document_response(&app, &graph_id, &entity_id).map(|envelope| {
                serde_json::to_value(envelope).expect("HostedDocumentEnvelope always serializes")
            })
        }
        "folder" => {
            require_hosted_entity_string(&input, &["label"], "label")?;
            normalize_entity_folder_payload(&entity_id, &mut input);
            enqueue_crdt_operation(
                app.clone(),
                EnqueueCrdtOperationInput {
                    kind: "workspace.createFolder".to_string(),
                    graph_id: graph_id.clone(),
                    document_id: Some(entity_id.clone()),
                    payload: input,
                },
            )
            .await?;
            flush_graph_projection(app.clone(), &graph_id).await?;
            hosted_entity_response(&app, &graph_id, "folder", &entity_id)
        }
        "artifact" => {
            require_hosted_entity_string(&input, &["label"], "label")?;
            require_hosted_entity_string(
                &input,
                &["originalFilename", "original_filename"],
                "originalFilename",
            )?;
            normalize_entity_artifact_payload(&app, &graph_id, &entity_id, &mut input)?;
            enqueue_crdt_operation(
                app.clone(),
                EnqueueCrdtOperationInput {
                    kind: "workspace.putArtifact".to_string(),
                    graph_id: graph_id.clone(),
                    document_id: Some(entity_id.clone()),
                    payload: input,
                },
            )
            .await?;
            flush_graph_projection(app.clone(), &graph_id).await?;
            hosted_entity_response(&app, &graph_id, "artifact", &entity_id)
        }
        _ => unreachable!(),
    }
}

pub(super) async fn delete_hosted_entity(
    app: AppHandle,
    graph_id: String,
    entity_type: String,
    entity_id: String,
    cascade: bool,
) -> Result<serde_json::Value, String> {
    validate_hosted_entity_type(&entity_type)?;
    match entity_type.as_str() {
        "document" => {
            enqueue_crdt_operation(
                app.clone(),
                EnqueueCrdtOperationInput {
                    kind: "workspace.deleteDocument".to_string(),
                    graph_id: graph_id.clone(),
                    document_id: Some(entity_id.clone()),
                    payload: serde_json::json!({}),
                },
            )
            .await?;
            flush_graph_projection(app, &graph_id).await?;
            Ok(hosted_entity_delete_response(
                &graph_id,
                &entity_type,
                &entity_id,
            ))
        }
        "folder" => {
            enqueue_crdt_operation(
                app.clone(),
                EnqueueCrdtOperationInput {
                    kind: "workspace.deleteFolder".to_string(),
                    graph_id: graph_id.clone(),
                    document_id: Some(entity_id.clone()),
                    payload: serde_json::json!({
                        "folderId": entity_id,
                        "cascade": cascade,
                    }),
                },
            )
            .await?;
            flush_graph_projection(app, &graph_id).await?;
            Ok(hosted_entity_delete_response(
                &graph_id,
                &entity_type,
                &entity_id,
            ))
        }
        "artifact" => {
            enqueue_crdt_operation(
                app.clone(),
                EnqueueCrdtOperationInput {
                    kind: "workspace.deleteArtifact".to_string(),
                    graph_id: graph_id.clone(),
                    document_id: Some(entity_id.clone()),
                    payload: serde_json::json!({ "artifactId": entity_id }),
                },
            )
            .await?;
            flush_graph_projection(app.clone(), &graph_id).await?;
            delete_artifact_original_files(&app, &graph_id, &entity_id)?;
            Ok(hosted_entity_delete_response(
                &graph_id,
                &entity_type,
                &entity_id,
            ))
        }
        _ => unreachable!(),
    }
}

pub(super) fn entity_error_response(error: &str) -> Response {
    if error.contains("not found") {
        loopback_error(StatusCode::NOT_FOUND, error)
    } else if error.contains("not empty") {
        loopback_error(StatusCode::CONFLICT, error)
    } else if error.contains("revision mismatch") {
        loopback_error(StatusCode::CONFLICT, error)
    } else if error.contains(" is required") {
        loopback_error(StatusCode::UNPROCESSABLE_ENTITY, error)
    } else {
        loopback_error(StatusCode::BAD_REQUEST, error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delete_response_includes_hosted_and_local_aliases() {
        let value = hosted_entity_delete_response("graph-a", "folder", "folder-a");

        assert_eq!(value["id"], "folder-a");
        assert_eq!(value["graphId"], "graph-a");
        assert_eq!(value["graph_id"], "graph-a");
        assert_eq!(value["entityType"], "folder");
        assert_eq!(value["entity_type"], "folder");
        assert_eq!(value["status"], "deleted");
    }

    #[test]
    fn entity_error_mapping_uses_hosted_http_statuses() {
        assert_eq!(
            entity_error_response("document x not found").status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            entity_error_response("folder not empty").status(),
            StatusCode::CONFLICT
        );
        assert_eq!(
            entity_error_response("revision mismatch: expected 1, actual 2").status(),
            StatusCode::CONFLICT
        );
        assert_eq!(
            entity_error_response("label is required").status(),
            StatusCode::UNPROCESSABLE_ENTITY
        );
        assert_eq!(
            entity_error_response("bad input").status(),
            StatusCode::BAD_REQUEST
        );
    }
}
