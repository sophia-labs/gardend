use crate::{
    app_runtime::AppHandle,
    crdt_queue::{enqueue_crdt_operation_outcome, EnqueueCrdtOperationInput},
    document_mcp_write_payloads::{mcp_write_document_response, write_durability_verdict},
};
use serde_json::{json, Value};

pub(crate) async fn create_document_once(
    app: AppHandle,
    arguments: &Value,
) -> Result<Value, String> {
    let expected = crate::graph_incarnation_admission::expected_incarnation(arguments)
        .map_err(crate::app_error::AppError::message)?;
    let (graph_id, document_id, payload) = crate::crdt_engine::create_once::mcp_input(arguments)
        .map_err(|error| format!("create_document_once refused: {error}"))?;
    let outcome = enqueue_crdt_operation_outcome(
        app.clone(),
        EnqueueCrdtOperationInput {
            kind: "document.createOnce".into(),
            graph_id: graph_id.clone(),
            document_id: Some(document_id.clone()),
            payload,
        },
    )
    .await?;
    // This operation is already terminal; an envelope/projection error must
    // never make its admitted content safe to resend.
    let result: Result<Value, String> = async {
        if let Some(expected) = &expected {
            crate::crdt_projection_flush::flush_projection_incarnation(
                app.clone(),
                &graph_id,
                Some(&document_id),
                expected,
                Some((
                    &outcome.operation_id,
                    "mcpCreateDocumentOnceWorkspaceFlushMs",
                )),
            )
            .await?;
        } else {
            crate::crdt_projection_flush::flush_document_projection_phase(
                app.clone(),
                &graph_id,
                &document_id,
                &outcome.operation_id,
                "mcpCreateDocumentOnceWorkspaceFlushMs",
            )
            .await?;
        }
        let _read_lifetime = crate::graph_incarnation_admission::acquire_expected_lifetime(
            &app,
            &graph_id,
            expected.as_deref(),
        )
        .await
        .map_err(crate::app_error::AppError::message)?;
        let epoch = crate::cell_durability::current_write_epoch();
        let durability = write_durability_verdict(
            arguments
                .get("awaitDurable")
                .and_then(Value::as_bool)
                .unwrap_or(true),
            crate::cell_durability::durable_plane_semantics(),
            crate::cell_durability::durability_watermarks(),
            epoch,
        );
        let document = serde_json::to_value(
            crate::document_projection_service::hosted_document_response(
                &app,
                &graph_id,
                &document_id,
            )?,
        )
        .map_err(|e| e.to_string())?;
        let mut response = mcp_write_document_response(
            arguments,
            &graph_id,
            &document_id,
            &document,
            &outcome.value,
            durability,
        );
        response["outcome"] = json!("created");
        response["order"] = arguments["order"].clone();
        response["parentId"] = Value::Null;
        if let Some(expected) = &expected {
            response["graphIncarnation"] = json!(expected);
        }
        Ok(response)
    }
    .await;
    result.map_err(|error| format!("create_document_once uncertain: admitted content may exist; inspect without resending: {error}"))
}
