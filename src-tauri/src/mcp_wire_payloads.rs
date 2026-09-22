use crate::mcp_utils::{mcp_arg_bool_with_fallback, mcp_arg_string_with_fallback};

pub(super) fn wire_inputs(arguments: &serde_json::Value) -> Vec<serde_json::Value> {
    arguments
        .get("wires")
        .and_then(serde_json::Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter(|value| value.is_object())
                .cloned()
                .collect()
        })
        .unwrap_or_else(|| vec![arguments.clone()])
}

pub(super) fn wire_create_payload(
    wire_input: &serde_json::Value,
    arguments: &serde_json::Value,
    graph_id: &str,
) -> Result<(String, serde_json::Value), String> {
    let expected = crate::graph_incarnation_admission::expected_incarnation(arguments)
        .map_err(crate::app_error::AppError::message)?;
    let item_expected = crate::graph_incarnation_admission::expected_incarnation(wire_input)
        .map_err(crate::app_error::AppError::message)?;
    if item_expected.is_some() && item_expected != expected {
        return Err("wire item incarnation must agree with the top-level graphIncarnation".into());
    }
    let source_document_id = mcp_arg_string_with_fallback(
        wire_input,
        arguments,
        &[
            "source_document_id",
            "sourceDocumentId",
            "document_id",
            "documentId",
        ],
    )
    .ok_or_else(|| "source_document_id is required".to_string())?;
    let target_document_id = mcp_arg_string_with_fallback(
        wire_input,
        arguments,
        &["target_document_id", "targetDocumentId"],
    )
    .ok_or_else(|| "target_document_id is required".to_string())?;
    let target_graph_id =
        mcp_arg_string_with_fallback(wire_input, arguments, &["target_graph_id", "targetGraphId"])
            .unwrap_or_else(|| graph_id.to_string());
    let predicate = mcp_arg_string_with_fallback(wire_input, arguments, &["predicate"])
        .unwrap_or_else(|| "isWiredTo".to_string());
    let mut payload = serde_json::json!({
        "wireId": mcp_arg_string_with_fallback(wire_input, arguments, &["wire_id", "wireId", "id"]),
        "sourceDocumentId": source_document_id.clone(),
        "sourceBlockId": mcp_arg_string_with_fallback(wire_input, arguments, &["source_block_id", "sourceBlockId"]),
        "sourceMarkId": mcp_arg_string_with_fallback(wire_input, arguments, &["source_mark_id", "sourceMarkId"]),
        "targetGraphId": target_graph_id,
        "targetDocumentId": target_document_id,
        "targetBlockId": mcp_arg_string_with_fallback(wire_input, arguments, &["target_block_id", "targetBlockId"]),
        "targetMarkId": mcp_arg_string_with_fallback(wire_input, arguments, &["target_mark_id", "targetMarkId"]),
        "predicate": predicate,
        "bidirectional": mcp_arg_bool_with_fallback(wire_input, arguments, &["bidirectional"], false),
    });
    if let Some(expected) = expected {
        payload[crate::crdt_queue::GRAPH_INCARNATION_PAYLOAD_KEY] = serde_json::json!(expected);
    }
    Ok((source_document_id, payload))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_create_payload_uses_item_values_before_fallback_arguments() {
        let arguments = serde_json::json!({
            "source_document_id": "source-top",
            "target_document_id": "target-top",
            "target_graph_id": "target-graph-top",
            "predicate": "relatesTo",
            "bidirectional": true,
        });
        let wire = serde_json::json!({
            "id": "wire-a",
            "sourceDocumentId": "source-item",
            "targetDocumentId": "target-item",
            "source_block_id": "source-block",
        });

        let (source_document_id, payload) =
            wire_create_payload(&wire, &arguments, "graph-a").expect("wire payload");

        assert_eq!(source_document_id, "source-item");
        assert_eq!(payload.get("wireId"), Some(&serde_json::json!("wire-a")));
        assert_eq!(
            payload.get("sourceDocumentId"),
            Some(&serde_json::json!("source-item"))
        );
        assert_eq!(
            payload.get("targetDocumentId"),
            Some(&serde_json::json!("target-item"))
        );
        assert_eq!(
            payload.get("targetGraphId"),
            Some(&serde_json::json!("target-graph-top"))
        );
        assert_eq!(
            payload.get("sourceBlockId"),
            Some(&serde_json::json!("source-block"))
        );
        assert_eq!(
            payload.get("predicate"),
            Some(&serde_json::json!("relatesTo"))
        );
        assert_eq!(payload.get("bidirectional"), Some(&serde_json::json!(true)));
    }

    #[test]
    fn wire_inputs_ignores_non_object_batch_items() {
        assert_eq!(
            wire_inputs(&serde_json::json!({ "wires": [null, { "id": "wire-a" }, "x"] })),
            vec![serde_json::json!({ "id": "wire-a" })]
        );
    }
}
