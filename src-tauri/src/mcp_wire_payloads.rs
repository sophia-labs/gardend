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
    // An end is a document OR an object IRI (whiteboard semantics G3). A wire drawn on a board may start at
    // an object; its queue document is then the board (`sceneDocumentId`), which is not stored as the source.
    let source_iri =
        mcp_arg_string_with_fallback(wire_input, arguments, &["source_iri", "sourceIri"]);
    let target_iri =
        mcp_arg_string_with_fallback(wire_input, arguments, &["target_iri", "targetIri"]);
    let scene_document_id = mcp_arg_string_with_fallback(
        wire_input,
        arguments,
        &["scene_document_id", "sceneDocumentId"],
    );
    let named_source = mcp_arg_string_with_fallback(
        wire_input,
        arguments,
        &["source_document_id", "sourceDocumentId"],
    );
    let named_target = mcp_arg_string_with_fallback(
        wire_input,
        arguments,
        &["target_document_id", "targetDocumentId"],
    );
    if source_iri.is_some() && named_source.is_some() {
        return Err("a wire's source is a document or an iri, not both".to_string());
    }
    if target_iri.is_some() && named_target.is_some() {
        return Err("a wire's target is a document or an iri, not both".to_string());
    }
    let (queue_document_id, source_document_id) = if source_iri.is_some() {
        let board = scene_document_id
            .clone()
            .ok_or_else(|| "sceneDocumentId is required when sourceIri is given".to_string())?;
        (board, None)
    } else {
        let document_id = named_source
            .or_else(|| {
                mcp_arg_string_with_fallback(wire_input, arguments, &["document_id", "documentId"])
            })
            .ok_or_else(|| "source_document_id is required".to_string())?;
        (document_id.clone(), Some(document_id))
    };
    let target_document_id = if target_iri.is_some() {
        None
    } else {
        Some(named_target.ok_or_else(|| "target_document_id is required".to_string())?)
    };
    let target_graph_id =
        mcp_arg_string_with_fallback(wire_input, arguments, &["target_graph_id", "targetGraphId"])
            .unwrap_or_else(|| graph_id.to_string());
    let supplied_predicate = wire_input
        .get("predicate")
        .or_else(|| arguments.get("predicate"));
    if supplied_predicate
        .is_some_and(|value| value.as_str().map_or(true, |text| text.trim().is_empty()))
    {
        return Err("predicate must be a non-empty string (omit it to use isWiredTo)".to_string());
    }
    let predicate = mcp_arg_string_with_fallback(wire_input, arguments, &["predicate"])
        .unwrap_or_else(|| "isWiredTo".to_string());
    // An IRI end names no block or mark: the stored wire does not carry the carrier.
    let source_block_id =
        mcp_arg_string_with_fallback(wire_input, arguments, &["source_block_id", "sourceBlockId"])
            .filter(|_| source_iri.is_none());
    let source_mark_id =
        mcp_arg_string_with_fallback(wire_input, arguments, &["source_mark_id", "sourceMarkId"])
            .filter(|_| source_iri.is_none());
    let target_block_id =
        mcp_arg_string_with_fallback(wire_input, arguments, &["target_block_id", "targetBlockId"])
            .filter(|_| target_iri.is_none());
    let target_mark_id =
        mcp_arg_string_with_fallback(wire_input, arguments, &["target_mark_id", "targetMarkId"])
            .filter(|_| target_iri.is_none());
    let mut payload = serde_json::json!({
        "wireId": mcp_arg_string_with_fallback(wire_input, arguments, &["wire_id", "wireId", "id"]),
        "sourceDocumentId": source_document_id,
        "sourceBlockId": source_block_id,
        "sourceMarkId": source_mark_id,
        "targetGraphId": target_graph_id,
        "targetDocumentId": target_document_id,
        "targetBlockId": target_block_id,
        "targetMarkId": target_mark_id,
        "predicate": predicate,
        "bidirectional": mcp_arg_bool_with_fallback(wire_input, arguments, &["bidirectional"], false),
    });
    // The new fields ride along only when given, so a plain wire's payload is the payload it always was.
    for (key, value) in [
        ("sourceIri", source_iri),
        ("targetIri", target_iri),
        ("sceneDocumentId", scene_document_id),
        (
            "sceneGraphId",
            mcp_arg_string_with_fallback(
                wire_input,
                arguments,
                &["scene_graph_id", "sceneGraphId"],
            ),
        ),
        (
            "sceneElementId",
            mcp_arg_string_with_fallback(
                wire_input,
                arguments,
                &["scene_element_id", "sceneElementId"],
            ),
        ),
        (
            "sceneSourceElementId",
            mcp_arg_string_with_fallback(
                wire_input,
                arguments,
                &["scene_source_element_id", "sceneSourceElementId"],
            ),
        ),
        (
            "sceneTargetElementId",
            mcp_arg_string_with_fallback(
                wire_input,
                arguments,
                &["scene_target_element_id", "sceneTargetElementId"],
            ),
        ),
        (
            "sceneStableKey",
            mcp_arg_string_with_fallback(
                wire_input,
                arguments,
                &["scene_stable_key", "sceneStableKey"],
            ),
        ),
    ] {
        if let Some(value) = value {
            payload[key] = serde_json::json!(value);
        }
    }
    if let Some(expected) = expected {
        payload[crate::crdt_queue::GRAPH_INCARNATION_PAYLOAD_KEY] = serde_json::json!(expected);
    }
    Ok((queue_document_id, payload))
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

    /// G3: `create_wires` used to drop every `scene*` field (the REST route kept them); they ride through now, in
    /// both spellings, from the item or the top level; a plain wire's payload gains no key.
    #[test]
    fn wire_create_payload_passes_scene_provenance_through() {
        let arguments = serde_json::json!({ "sceneGraphId": "wb-graph" });
        let wire = serde_json::json!({
            "id": "wire-a",
            "sourceDocumentId": "doc-a",
            "target_document_id": "doc-b",
            "scene_document_id": "wb-board",
            "sceneElementId": "arw1",
            "scene_source_element_id": "shp1",
            "sceneTargetElementId": "shp2",
            "sceneStableKey": "wb:wb-board:arw1",
        });
        let (queue_document, payload) =
            wire_create_payload(&wire, &arguments, "wb-graph").expect("payload");
        assert_eq!(queue_document, "doc-a");
        for (key, value) in [
            ("sceneGraphId", "wb-graph"),
            ("sceneDocumentId", "wb-board"),
            ("sceneElementId", "arw1"),
            ("sceneSourceElementId", "shp1"),
            ("sceneTargetElementId", "shp2"),
            ("sceneStableKey", "wb:wb-board:arw1"),
        ] {
            assert_eq!(payload.get(key), Some(&serde_json::json!(value)), "{key}");
        }

        let (_, plain) = wire_create_payload(
            &serde_json::json!({ "sourceDocumentId": "doc-a", "targetDocumentId": "doc-b" }),
            &serde_json::json!({}),
            "wb-graph",
        )
        .expect("plain");
        for key in [
            "sourceIri",
            "targetIri",
            "sceneDocumentId",
            "sceneGraphId",
            "sceneElementId",
            "sceneStableKey",
        ] {
            assert!(
                plain.get(key).is_none(),
                "{key} appears in a plain wire's payload"
            );
        }
    }

    /// G3: an end may be an object IRI. The source is then the board (queue document `sceneDocumentId`), the
    /// stored source document is null, and a document id AND an IRI for one end is refused.
    #[test]
    fn wire_create_payload_accepts_object_iri_ends_and_refuses_both() {
        let object = "urn:mnemosyne:local:graph:wb-graph:dream:wolf-man";
        let (queue_document, payload) = wire_create_payload(
            &serde_json::json!({ "sourceDocumentId": "doc-a", "targetIri": object, "source_block_id": "b1" }),
            &serde_json::json!({}),
            "wb-graph",
        )
        .expect("iri target needs no target document");
        assert_eq!(queue_document, "doc-a");
        assert_eq!(payload["targetIri"], object);
        assert!(payload["targetDocumentId"].is_null());
        assert_eq!(payload["sourceBlockId"], "b1");

        let (queue_document, payload) = wire_create_payload(
            &serde_json::json!({ "source_iri": object, "targetDocumentId": "doc-b", "sceneDocumentId": "wb-board",
                                 "sourceBlockId": "ignored" }),
            &serde_json::json!({}),
            "wb-graph",
        )
        .expect("iri source rides on the board");
        assert_eq!(queue_document, "wb-board");
        assert!(payload["sourceDocumentId"].is_null());
        assert!(payload["sourceBlockId"].is_null());
        assert_eq!(payload["sourceIri"], object);

        let error = wire_create_payload(
            &serde_json::json!({ "sourceIri": object, "targetDocumentId": "doc-b" }),
            &serde_json::json!({}),
            "wb-graph",
        )
        .unwrap_err();
        assert_eq!(error, "sceneDocumentId is required when sourceIri is given");
        let error = wire_create_payload(
            &serde_json::json!({ "sourceDocumentId": "doc-a", "targetDocumentId": "doc-b", "targetIri": object }),
            &serde_json::json!({}),
            "wb-graph",
        )
        .unwrap_err();
        assert_eq!(error, "a wire's target is a document or an iri, not both");
        let error = wire_create_payload(
            &serde_json::json!({ "sourceDocumentId": "doc-a" }),
            &serde_json::json!({}),
            "wb-graph",
        )
        .unwrap_err();
        assert_eq!(error, "target_document_id is required");
    }

    #[test]
    fn wire_inputs_ignores_non_object_batch_items() {
        assert_eq!(
            wire_inputs(&serde_json::json!({ "wires": [null, { "id": "wire-a" }, "x"] })),
            vec![serde_json::json!({ "id": "wire-a" })]
        );
    }
}
