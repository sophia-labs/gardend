use crate::app_runtime::AppHandle;
use crate::{
    document_service::read_workspace_record,
    json_utils::{json_bool, json_string},
    mcp_utils::{mcp_arg_string, mcp_required_graph_id},
    paths::existing_graph_dir,
    rdf_service::wire_predicate_uri,
    wire_predicates::{builtin_wire_predicates, predicate_label, predicate_short_name},
    wire_state::{wire_is_active, wire_matches_document, wire_matches_iri},
    workspace_entity_projection::{workspace_document_title, workspace_documents, workspace_wires},
};
use std::collections::BTreeSet;

fn wire_summary(
    wire: &serde_json::Value,
    graph_id: &str,
    documents: &[serde_json::Value],
) -> serde_json::Value {
    let predicate = json_string(wire.get("predicate")).unwrap_or_else(|| "relatedTo".to_string());
    let predicate_uri = wire_predicate_uri(&predicate);
    let source_document_id = json_string(wire.get("sourceDocumentId"));
    let target_document_id = json_string(wire.get("targetDocumentId"));
    let source_title = json_string(wire.get("sourceTitle"))
        .or_else(|| workspace_document_title(documents, source_document_id.as_deref()));
    let target_title = json_string(wire.get("targetTitle"))
        .or_else(|| workspace_document_title(documents, target_document_id.as_deref()));

    let mut value = serde_json::json!({
        "id": json_string(wire.get("id")).unwrap_or_default(),
        "sourceGraphId": graph_id,
        "sourceDocumentId": source_document_id,
        "targetGraphId": json_string(wire.get("targetGraphId")).unwrap_or_else(|| graph_id.to_string()),
        "targetDocumentId": target_document_id,
        "predicate": predicate_short_name(&predicate_uri),
        "predicateUri": predicate_uri,
        "predicateLabel": predicate_label(&predicate),
        "bidirectional": json_bool(wire.get("bidirectional")).unwrap_or(false),
    });

    for (json_key, output_key) in [
        ("sourceBlockId", "sourceBlockId"),
        ("targetBlockId", "targetBlockId"),
        ("sourceMarkId", "sourceMarkId"),
        ("targetMarkId", "targetMarkId"),
        ("inverseOf", "inverseOf"),
        ("sourceSnippet", "sourceSnippet"),
        ("targetSnippet", "targetSnippet"),
        // An object end, and the board and arrow a scene wire came from: omitted when absent.
        ("sourceIri", "sourceIri"),
        ("targetIri", "targetIri"),
        ("sceneDocumentId", "sceneDocumentId"),
        ("sceneElementId", "sceneElementId"),
    ] {
        if let Some(field_value) = json_string(wire.get(json_key)) {
            value[output_key] = serde_json::json!(field_value);
        }
    }
    if let Some(title) = source_title {
        value["sourceTitle"] = serde_json::json!(title);
    }
    if let Some(title) = target_title {
        value["targetTitle"] = serde_json::json!(title);
    }
    value
}

pub(super) fn mcp_local_get_wires(
    app: AppHandle,
    arguments: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let graph_id = mcp_required_graph_id(arguments)?;
    // Exactly one of a document and an object: `get_wires {documentId}` or `get_wires {iri}`.
    let document_id = mcp_arg_string(arguments, &["documentId", "document_id"]);
    let iri = mcp_arg_string(arguments, &["iri"]);
    if document_id.is_some() == iri.is_some() {
        return Err("get_wires: give exactly one of documentId and iri".to_string());
    }
    let direction = mcp_arg_string(arguments, &["direction"]).unwrap_or_else(|| "both".to_string());
    if !matches!(direction.as_str(), "outgoing" | "incoming" | "both") {
        return Err("direction must be outgoing, incoming, or both".to_string());
    }
    let predicate =
        mcp_arg_string(arguments, &["predicate"]).map(|value| wire_predicate_uri(&value));
    let graph_dir = existing_graph_dir(&app, &graph_id)?;
    let workspace = read_workspace_record(&graph_dir, &graph_id)?;
    let snapshot = workspace.snapshot.as_ref();
    let documents = workspace_documents(snapshot);
    let wires = workspace_wires(snapshot);
    // Count the whole graph, regardless of the requested end or filter.
    // Historical ghosts stay visible and are never removed by this audit.
    let ghost_wire_count = wires
        .iter()
        .filter(|wire| {
            wire_is_active(wire)
                && !json_string(wire.get("id"))
                    .unwrap_or_default()
                    .ends_with("-inv")
                && crate::mcp_workspace_mutation_service::validate_wire_targets(
                    &app, &graph_id, wire,
                )
                .is_err()
        })
        .count();
    let mut results = Vec::new();

    for wire in wires {
        let id = json_string(wire.get("id")).unwrap_or_default();
        if id.ends_with("-inv") || !wire_is_active(&wire) {
            continue;
        }
        let matches = match (&document_id, &iri) {
            (Some(document_id), _) => wire_matches_document(&wire, document_id, &direction),
            (None, Some(iri)) => wire_matches_iri(&wire, iri, &direction),
            (None, None) => false,
        };
        if !matches {
            continue;
        }
        if let Some(predicate) = &predicate {
            let wire_predicate =
                json_string(wire.get("predicate")).unwrap_or_else(|| "relatedTo".to_string());
            if wire_predicate_uri(&wire_predicate) != *predicate {
                continue;
            }
        }
        results.push(wire_summary(&wire, &graph_id, &documents));
    }

    let mut response = serde_json::json!({
        "graph_id": graph_id,
        "direction": direction,
        "ghostWireCount": ghost_wire_count,
        "wires": results,
        "count": results.len(),
    });
    // The key of the thing asked about: `document_id` as it always was, or `iri`.
    match (document_id, iri) {
        (Some(document_id), _) => response["document_id"] = serde_json::json!(document_id),
        (None, Some(iri)) => response["iri"] = serde_json::json!(iri),
        (None, None) => {}
    }
    Ok(response)
}

pub(super) fn mcp_local_list_wire_predicates(
    app: AppHandle,
    arguments: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let graph_id = mcp_required_graph_id(arguments)?;
    let graph_dir = existing_graph_dir(&app, &graph_id)?;
    let workspace = read_workspace_record(&graph_dir, &graph_id)?;
    let mut predicates = Vec::new();
    let mut seen = BTreeSet::new();

    for (name, label, category) in builtin_wire_predicates() {
        seen.insert(name.to_string());
        predicates.push(serde_json::json!({
            "name": name,
            "label": label,
            "category": if category.is_empty() { serde_json::Value::Null } else { serde_json::json!(category) },
        }));
    }

    for wire in workspace_wires(workspace.snapshot.as_ref()) {
        let Some(predicate) = json_string(wire.get("predicate")) else {
            continue;
        };
        let short_name = predicate_short_name(&predicate);
        if seen.insert(short_name.clone()) {
            predicates.push(serde_json::json!({
                "name": short_name,
                "label": predicate_label(&predicate),
                "category": "Custom",
            }));
        }
    }

    Ok(serde_json::json!({
        "graph_id": graph_id,
        "predicates": predicates,
        "count": predicates.len(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_summary_normalizes_predicates_and_document_titles() {
        let wire = serde_json::json!({
            "id": "wire-a",
            "sourceDocumentId": "doc-a",
            "targetDocumentId": "doc-b",
            "predicate": "http://mnemosyne.ai/vocab#supports",
            "sourceBlockId": "block-a",
            "targetSnippet": "target text",
            "bidirectional": true
        });
        let documents = vec![
            serde_json::json!({ "documentId": "doc-a", "title": "Source" }),
            serde_json::json!({ "documentId": "doc-b", "title": "Target" }),
        ];

        let value = wire_summary(&wire, "graph-a", &documents);

        assert_eq!(value["id"], "wire-a");
        assert_eq!(value["sourceGraphId"], "graph-a");
        assert_eq!(value["targetGraphId"], "graph-a");
        assert_eq!(value["predicate"], "supports");
        assert_eq!(value["predicateLabel"], "supports");
        assert_eq!(value["sourceTitle"], "Source");
        assert_eq!(value["targetTitle"], "Target");
        assert_eq!(value["sourceBlockId"], "block-a");
        assert_eq!(value["targetSnippet"], "target text");
        assert_eq!(value["bidirectional"], true);
        // a document-to-document wire gains none of the new keys
        for key in [
            "sourceIri",
            "targetIri",
            "sceneDocumentId",
            "sceneElementId",
        ] {
            assert!(
                value.get(key).is_none(),
                "{key} appears on a plain wire: {value}"
            );
        }
    }

    /// G3: a wire to an object says so (`targetIri`), with the board and arrow it came from; the object end has
    /// no document id and is titled by its stored label.
    #[test]
    fn wire_summary_reports_object_ends_and_scene() {
        let object = "urn:mnemosyne:local:graph:wb-graph:dream:wolf-man";
        let wire = serde_json::json!({
            "id": "wire-wb-0001",
            "sourceDocumentId": "doc-traumdeutung",
            "targetIri": object,
            "targetTitle": "The Wolf Man dream",
            "predicate": "supports",
            "sceneDocumentId": "wb-board",
            "sceneElementId": "arw0000000000000000002",
        });
        let documents =
            vec![serde_json::json!({ "documentId": "doc-traumdeutung", "title": "Traumdeutung" })];
        let value = wire_summary(&wire, "wb-graph", &documents);
        assert_eq!(value["targetIri"], object);
        assert!(value["targetDocumentId"].is_null());
        assert_eq!(value["targetTitle"], "The Wolf Man dream");
        assert_eq!(value["sourceTitle"], "Traumdeutung");
        assert_eq!(value["sceneDocumentId"], "wb-board");
        assert_eq!(value["sceneElementId"], "arw0000000000000000002");
        assert!(value.get("sourceIri").is_none());
    }
}
