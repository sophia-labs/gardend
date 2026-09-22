use crate::{
    json_utils::{json_bool, json_string},
    rdf_service::wire_predicate_uri,
    wire_predicates::{predicate_label, predicate_short_name},
    workspace_entity_projection::workspace_entity_title,
};
use std::collections::BTreeMap;

pub(super) fn wire_traversal_targets(
    wire: &serde_json::Value,
    current_document_id: &str,
    direction: &str,
) -> Vec<String> {
    let source_document_id = json_string(wire.get("sourceDocumentId"));
    let target_document_id = json_string(wire.get("targetDocumentId"));
    let bidirectional = json_bool(wire.get("bidirectional")).unwrap_or(false);
    let mut targets = Vec::new();

    if matches!(direction, "outgoing" | "both")
        && source_document_id.as_deref() == Some(current_document_id)
    {
        if let Some(target_document_id) = target_document_id.clone() {
            targets.push(target_document_id);
        }
    }
    if matches!(direction, "incoming" | "both")
        && target_document_id.as_deref() == Some(current_document_id)
    {
        if let Some(source_document_id) = source_document_id.clone() {
            targets.push(source_document_id);
        }
    }
    if bidirectional
        && matches!(direction, "outgoing" | "both")
        && target_document_id.as_deref() == Some(current_document_id)
    {
        if let Some(source_document_id) = source_document_id.clone() {
            targets.push(source_document_id);
        }
    }
    if bidirectional
        && matches!(direction, "incoming" | "both")
        && source_document_id.as_deref() == Some(current_document_id)
    {
        if let Some(target_document_id) = target_document_id {
            targets.push(target_document_id);
        }
    }

    targets.sort();
    targets.dedup();
    targets
}

pub(super) fn wire_traversal_hop(
    wire: &serde_json::Value,
    from_document_id: &str,
    to_document_id: &str,
) -> serde_json::Value {
    let predicate = json_string(wire.get("predicate")).unwrap_or_else(|| "relatedTo".to_string());
    let predicate_uri = wire_predicate_uri(&predicate);
    let mut hop = serde_json::json!({
        "wire_id": json_string(wire.get("id")).unwrap_or_default(),
        "wireId": json_string(wire.get("id")).unwrap_or_default(),
        "from_document_id": from_document_id,
        "fromDocumentId": from_document_id,
        "to_document_id": to_document_id,
        "toDocumentId": to_document_id,
        "sourceDocumentId": json_string(wire.get("sourceDocumentId")),
        "targetDocumentId": json_string(wire.get("targetDocumentId")),
        "predicate": predicate_short_name(&predicate_uri),
        "predicateUri": predicate_uri,
        "predicateLabel": predicate_label(&predicate),
        "bidirectional": json_bool(wire.get("bidirectional")).unwrap_or(false),
    });
    for key in [
        "sourceBlockId",
        "targetBlockId",
        "sourceMarkId",
        "targetMarkId",
        "sourceTitle",
        "targetTitle",
        "sourceSnippet",
        "targetSnippet",
    ] {
        if let Some(value) = json_string(wire.get(key)) {
            hop[key] = serde_json::json!(value);
        }
    }
    hop
}

pub(super) fn document_title_for_traversal(
    documents_by_id: &BTreeMap<String, serde_json::Value>,
    document_id: &str,
) -> Option<String> {
    documents_by_id
        .get(document_id)
        .and_then(workspace_entity_title)
}

pub(super) fn traversal_document_entry(
    documents_by_id: &BTreeMap<String, serde_json::Value>,
    document_id: &str,
    depth: usize,
) -> serde_json::Value {
    serde_json::json!({
        "document_id": document_id,
        "documentId": document_id,
        "title": document_title_for_traversal(documents_by_id, document_id).unwrap_or_else(|| document_id.to_string()),
        "depth": depth,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn traversal_targets_include_bidirectional_inverse_edges() {
        let wire = serde_json::json!({
            "sourceDocumentId": "doc-a",
            "targetDocumentId": "doc-b",
            "bidirectional": true
        });

        assert_eq!(
            wire_traversal_targets(&wire, "doc-a", "outgoing"),
            vec!["doc-b"]
        );
        assert_eq!(
            wire_traversal_targets(&wire, "doc-b", "outgoing"),
            vec!["doc-a"]
        );
        assert_eq!(
            wire_traversal_targets(&wire, "doc-a", "incoming"),
            vec!["doc-b"]
        );
        assert_eq!(
            wire_traversal_targets(&wire, "doc-b", "incoming"),
            vec!["doc-a"]
        );
    }
}
