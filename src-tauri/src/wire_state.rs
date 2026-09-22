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
}
