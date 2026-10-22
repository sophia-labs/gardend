use crate::{
    json_utils::{json_bool, json_string},
    rdf_service::wire_predicate_uri,
    wire_predicates::{predicate_label, predicate_short_name},
    workspace_entity_projection::workspace_entity_title,
};
use std::collections::BTreeMap;

/// Is this walk node an object IRI (not a document id)? A local document id is `[A-Za-z0-9_-]+`
/// (`ids::validate_local_id`), so anything with a scheme colon is an IRI.
pub(super) fn is_iri_node(node: &str) -> bool {
    node.contains(':')
}

/// The node an end of `wire` stands for: its document id, else its object IRI. A walk crosses either (a hop
/// through an object continues the walk).
fn wire_end_node(wire: &serde_json::Value, document_key: &str, iri_key: &str) -> Option<String> {
    json_string(wire.get(document_key)).or_else(|| json_string(wire.get(iri_key)))
}

/// The nodes one hop from `current_document_id` (a document id or an object IRI) along `wire`.
pub(super) fn wire_traversal_targets(
    wire: &serde_json::Value,
    current_document_id: &str,
    direction: &str,
) -> Vec<String> {
    let source_document_id = wire_end_node(wire, "sourceDocumentId", "sourceIri");
    let target_document_id = wire_end_node(wire, "targetDocumentId", "targetIri");
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

/// One step of a path. For a document node the keys are the ones they always were; an IRI node has no document
/// id (`null`) and is reported as `fromIri` / `toIri`, and the wire's own object ends as `sourceIri` /
/// `targetIri`.
pub(super) fn wire_traversal_hop(
    wire: &serde_json::Value,
    from_document_id: &str,
    to_document_id: &str,
) -> serde_json::Value {
    let from_is_iri = is_iri_node(from_document_id);
    let to_is_iri = is_iri_node(to_document_id);
    let predicate = json_string(wire.get("predicate")).unwrap_or_else(|| "relatedTo".to_string());
    let predicate_uri = wire_predicate_uri(&predicate);
    let mut hop = serde_json::json!({
        "wire_id": json_string(wire.get("id")).unwrap_or_default(),
        "wireId": json_string(wire.get("id")).unwrap_or_default(),
        "from_document_id": if from_is_iri { None } else { Some(from_document_id) },
        "fromDocumentId": if from_is_iri { None } else { Some(from_document_id) },
        "to_document_id": if to_is_iri { None } else { Some(to_document_id) },
        "toDocumentId": if to_is_iri { None } else { Some(to_document_id) },
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
        "sourceIri",
        "targetIri",
    ] {
        if let Some(value) = json_string(wire.get(key)) {
            hop[key] = serde_json::json!(value);
        }
    }
    if from_is_iri {
        hop["fromIri"] = serde_json::json!(from_document_id);
    }
    if to_is_iri {
        hop["toIri"] = serde_json::json!(to_document_id);
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

/// A reachable node: a document as it always was, or an object (`documentId` null, `iri` set), titled by the
/// label a wire stored for it (`iri_titles`), else by the IRI.
pub(super) fn traversal_node_entry(
    documents_by_id: &BTreeMap<String, serde_json::Value>,
    iri_titles: &BTreeMap<String, String>,
    node: &str,
    depth: usize,
) -> serde_json::Value {
    if is_iri_node(node) {
        return serde_json::json!({
            "document_id": serde_json::Value::Null,
            "documentId": serde_json::Value::Null,
            "iri": node,
            "title": iri_titles.get(node).cloned().unwrap_or_else(|| node.to_string()),
            "depth": depth,
        });
    }
    traversal_document_entry(documents_by_id, node, depth)
}

/// The labels the wires store for their object ends, by IRI.
pub(super) fn iri_titles_of(wires: &[serde_json::Value]) -> BTreeMap<String, String> {
    let mut titles = BTreeMap::new();
    for wire in wires {
        for (iri_key, title_key) in [("sourceIri", "sourceTitle"), ("targetIri", "targetTitle")] {
            if let (Some(iri), Some(title)) = (json_string(wire.get(iri_key)), json_string(wire.get(title_key))) {
                titles.entry(iri).or_insert(title);
            }
        }
    }
    titles
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

    const OBJECT: &str = "urn:mnemosyne:local:graph:wb-graph:dream:wolf-man";

    /// G3: an object end is a node of the walk, from either side; a wire between documents is untouched.
    #[test]
    fn traversal_targets_cross_an_object_end() {
        let into = serde_json::json!({ "sourceDocumentId": "doc-a", "targetIri": OBJECT });
        let out_of = serde_json::json!({ "sourceIri": OBJECT, "targetDocumentId": "doc-b" });
        assert_eq!(wire_traversal_targets(&into, "doc-a", "outgoing"), vec![OBJECT]);
        assert_eq!(wire_traversal_targets(&into, OBJECT, "incoming"), vec!["doc-a"]);
        assert_eq!(wire_traversal_targets(&into, OBJECT, "both"), vec!["doc-a"]);
        assert!(wire_traversal_targets(&into, "doc-a", "incoming").is_empty());
        assert_eq!(wire_traversal_targets(&out_of, OBJECT, "outgoing"), vec!["doc-b"]);
        assert_eq!(wire_traversal_targets(&out_of, "doc-b", "incoming"), vec![OBJECT]);
        assert!(wire_traversal_targets(&out_of, OBJECT, "incoming").is_empty());
    }

    #[test]
    fn hops_and_entries_name_an_iri_node_by_iri_and_a_document_by_id() {
        let wire = serde_json::json!({
            "id": "wire-1", "sourceDocumentId": "doc-a", "targetIri": OBJECT, "targetTitle": "The Wolf Man dream",
        });
        let hop = wire_traversal_hop(&wire, "doc-a", OBJECT);
        assert_eq!(hop["fromDocumentId"], "doc-a");
        assert!(hop["toDocumentId"].is_null());
        assert_eq!(hop["toIri"], OBJECT);
        assert!(hop.get("fromIri").is_none());
        assert_eq!(hop["targetIri"], OBJECT);
        let back = wire_traversal_hop(&wire, OBJECT, "doc-a");
        assert!(back["fromDocumentId"].is_null());
        assert_eq!(back["fromIri"], OBJECT);
        assert_eq!(back["toDocumentId"], "doc-a");

        let titles = iri_titles_of(&[wire]);
        let documents = BTreeMap::new();
        let entry = traversal_node_entry(&documents, &titles, OBJECT, 1);
        assert!(entry["documentId"].is_null());
        assert_eq!(entry["iri"], OBJECT);
        assert_eq!(entry["title"], "The Wolf Man dream");
        let plain = traversal_node_entry(&documents, &titles, "doc-a", 0);
        assert_eq!(plain["documentId"], "doc-a");
        assert!(plain.get("iri").is_none());
    }
}
