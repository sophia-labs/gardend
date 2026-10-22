use crate::json_utils::json_string;

pub(crate) fn wire_is_active(wire: &serde_json::Value) -> bool {
    json_string(wire.get("deletedAt")).is_none()
        && json_string(wire.get("deleted_at")).is_none()
        && json_string(wire.get("tombstonedAt")).is_none()
        && json_string(wire.get("_tombstonedAt")).is_none()
}

pub(crate) fn wire_matches_document(
    wire: &serde_json::Value,
    document_id: &str,
    direction: &str,
) -> bool {
    let is_source = json_string(wire.get("sourceDocumentId")).as_deref() == Some(document_id);
    let is_target = json_string(wire.get("targetDocumentId")).as_deref() == Some(document_id);
    // A board lists the wires drawn on it too (`sceneDocumentId`), under "both" only: the arrow belongs to the
    // board without being an edge out of it or into it.
    let drawn_here = json_string(wire.get("sceneDocumentId")).as_deref() == Some(document_id);
    match direction {
        "outgoing" => is_source,
        "incoming" => is_target,
        "both" => is_source || is_target || drawn_here,
        _ => false,
    }
}

/// Does `wire` start or end at the object `iri`? Same directions as [`wire_matches_document`]; an object end is
/// `sourceIri` / `targetIri`, and a wire to an object is never bidirectional.
pub(crate) fn wire_matches_iri(wire: &serde_json::Value, iri: &str, direction: &str) -> bool {
    let is_source = json_string(wire.get("sourceIri")).as_deref() == Some(iri);
    let is_target = json_string(wire.get("targetIri")).as_deref() == Some(iri);
    match direction {
        "outgoing" => is_source,
        "incoming" => is_target,
        "both" => is_source || is_target,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_activity_recognizes_deletion_aliases() {
        assert!(wire_is_active(&serde_json::json!({ "id": "wire-a" })));
        assert!(!wire_is_active(&serde_json::json!({ "deletedAt": "1000" })));
        assert!(!wire_is_active(
            &serde_json::json!({ "deleted_at": "1000" })
        ));
        assert!(!wire_is_active(
            &serde_json::json!({ "tombstonedAt": "1000" })
        ));
        assert!(!wire_is_active(
            &serde_json::json!({ "_tombstonedAt": "1000" })
        ));
    }

    #[test]
    fn wire_matching_respects_direction_aliases() {
        let wire = serde_json::json!({
            "sourceDocumentId": "doc-a",
            "targetDocumentId": "doc-b",
        });

        assert!(wire_matches_document(&wire, "doc-a", "outgoing"));
        assert!(!wire_matches_document(&wire, "doc-a", "incoming"));
        assert!(wire_matches_document(&wire, "doc-b", "incoming"));
        assert!(wire_matches_document(&wire, "doc-b", "both"));
        assert!(!wire_matches_document(&wire, "doc-c", "both"));
    }

    #[test]
    fn a_board_lists_the_wires_drawn_on_it_under_both_only() {
        let wire = serde_json::json!({
            "sourceDocumentId": "doc-a",
            "targetDocumentId": "doc-b",
            "sceneDocumentId": "wb-board",
        });
        assert!(wire_matches_document(&wire, "wb-board", "both"));
        assert!(!wire_matches_document(&wire, "wb-board", "outgoing"));
        assert!(!wire_matches_document(&wire, "wb-board", "incoming"));
        // a wire with no scene board is as it was
        let plain = serde_json::json!({ "sourceDocumentId": "doc-a", "targetDocumentId": "doc-b" });
        assert!(!wire_matches_document(&plain, "wb-board", "both"));
    }

    #[test]
    fn wire_matching_by_iri_respects_direction() {
        let object = "urn:mnemosyne:local:graph:g:dream:wolf-man";
        let into = serde_json::json!({ "sourceDocumentId": "doc-a", "targetIri": object });
        let out_of = serde_json::json!({ "sourceIri": object, "targetDocumentId": "doc-b" });
        assert!(wire_matches_iri(&into, object, "incoming"));
        assert!(!wire_matches_iri(&into, object, "outgoing"));
        assert!(wire_matches_iri(&out_of, object, "outgoing"));
        assert!(!wire_matches_iri(&out_of, object, "incoming"));
        assert!(
            wire_matches_iri(&into, object, "both") && wire_matches_iri(&out_of, object, "both")
        );
        assert!(!wire_matches_iri(&into, "urn:other", "both"));
        assert!(!wire_matches_iri(&into, object, "sideways"));
        // a document-to-document wire never matches an IRI
        let plain = serde_json::json!({ "sourceDocumentId": "doc-a", "targetDocumentId": "doc-b" });
        assert!(!wire_matches_iri(&plain, "doc-a", "both"));
    }
}
