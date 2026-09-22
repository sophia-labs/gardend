use crate::{rdf::document_subject, runtime_config::WIRE_NS};
use std::sync::OnceLock;

pub(super) fn snapshot_array<'a>(
    snapshot: &'a serde_json::Value,
    key: &str,
) -> &'a Vec<serde_json::Value> {
    static EMPTY: OnceLock<Vec<serde_json::Value>> = OnceLock::new();
    snapshot
        .get(key)
        .and_then(serde_json::Value::as_array)
        .unwrap_or_else(|| EMPTY.get_or_init(Vec::new))
}

pub(super) fn workspace_entity_subject(
    graph_id: &str,
    entity_type: &str,
    entity_id: &str,
) -> String {
    format!("urn:mnemosyne:local:graph:{graph_id}:{entity_type}:{entity_id}")
}

pub(super) fn folder_ref_uri(graph_id: &str, folder_id: &str) -> String {
    if folder_id.starts_with("urn:mnemosyne:") {
        folder_id.to_string()
    } else {
        workspace_entity_subject(graph_id, "folder", folder_id)
    }
}

pub(super) fn wire_ref_uri(graph_id: &str, wire_id: &str) -> String {
    if wire_id.starts_with("urn:mnemosyne:") {
        wire_id.to_string()
    } else {
        workspace_entity_subject(graph_id, "wire", wire_id)
    }
}

pub(super) fn document_ref_uri(document_id: &str) -> String {
    if document_id.starts_with("urn:mnemosyne:") {
        document_id.to_string()
    } else {
        document_subject(document_id)
    }
}

pub(super) fn block_ref_uri(document_id: &str, block_id: &str) -> String {
    if block_id.starts_with("urn:mnemosyne:") && block_id.contains("#block-") {
        block_id.to_string()
    } else {
        format!("{}#block-{block_id}", document_ref_uri(document_id))
    }
}

pub(super) fn mark_ref_uri(document_id: &str, block_id: &str, mark_id: &str) -> String {
    if mark_id.starts_with("urn:mnemosyne:") && mark_id.contains("#mark-") {
        mark_id.to_string()
    } else {
        format!(
            "{}#mark-{block_id}-{mark_id}",
            document_ref_uri(document_id)
        )
    }
}

pub(super) fn wire_predicate_uri(predicate: &str) -> String {
    if predicate.starts_with("http://")
        || predicate.starts_with("https://")
        || predicate.starts_with("urn:")
    {
        predicate.to_string()
    } else {
        format!("{WIRE_NS}{predicate}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_array_returns_existing_array_or_static_empty() {
        let snapshot = serde_json::json!({
            "documents": [{ "id": "doc-a" }],
            "folders": "not-array"
        });

        assert_eq!(snapshot_array(&snapshot, "documents").len(), 1);
        assert!(snapshot_array(&snapshot, "folders").is_empty());
        assert!(snapshot_array(&snapshot, "missing").is_empty());
    }

    #[test]
    fn workspace_entity_refs_preserve_existing_urns_and_wrap_local_ids() {
        assert_eq!(
            workspace_entity_subject("graph-a", "folder", "folder-a"),
            "urn:mnemosyne:local:graph:graph-a:folder:folder-a"
        );
        assert_eq!(
            folder_ref_uri("graph-a", "folder-a"),
            "urn:mnemosyne:local:graph:graph-a:folder:folder-a"
        );
        assert_eq!(
            wire_ref_uri("graph-a", "urn:mnemosyne:wire:x"),
            "urn:mnemosyne:wire:x"
        );
    }

    #[test]
    fn document_block_and_mark_refs_anchor_to_document_subjects() {
        assert_eq!(
            document_ref_uri("urn:mnemosyne:document:doc-a"),
            "urn:mnemosyne:document:doc-a"
        );
        assert_eq!(
            block_ref_uri("doc-a", "block-a"),
            "urn:mnemosyne:local:document:doc-a#block-block-a"
        );
        assert_eq!(
            mark_ref_uri("doc-a", "block-a", "mark-a"),
            "urn:mnemosyne:local:document:doc-a#mark-block-a-mark-a"
        );
        assert_eq!(
            block_ref_uri("doc-a", "urn:mnemosyne:document:doc-a#block-existing"),
            "urn:mnemosyne:document:doc-a#block-existing"
        );
    }

    #[test]
    fn wire_predicate_uri_preserves_absolute_terms_and_expands_names() {
        assert_eq!(
            wire_predicate_uri("https://example.test/p"),
            "https://example.test/p"
        );
        assert_eq!(wire_predicate_uri("urn:test:p"), "urn:test:p");
        assert_eq!(wire_predicate_uri("supports"), format!("{WIRE_NS}supports"));
    }
}
