use crate::{
    json_utils::json_string,
    wire_projection_service::{predicate_short_name, wire_is_active, wire_matches_document},
    workspace_entity_projection::workspace_document_title,
};
use std::collections::BTreeMap;

pub(super) struct DocumentDigestWireSummary {
    pub(super) incoming_count: usize,
    pub(super) outgoing_count: usize,
    pub(super) total_count: usize,
    pub(super) predicate_counts: BTreeMap<String, usize>,
    pub(super) top_connected_documents: Vec<serde_json::Value>,
}

pub(super) fn document_digest_wire_summary(
    wires: &[serde_json::Value],
    documents: &[serde_json::Value],
    document_id: &str,
    max_connected_documents: usize,
) -> DocumentDigestWireSummary {
    let connected_wires = wires
        .iter()
        .filter(|wire| wire_is_active(wire) && wire_matches_document(wire, document_id, "both"))
        .collect::<Vec<_>>();
    let outgoing_count = connected_wires
        .iter()
        .filter(|wire| wire_matches_document(wire, document_id, "outgoing"))
        .count();
    let incoming_count = connected_wires
        .iter()
        .filter(|wire| wire_matches_document(wire, document_id, "incoming"))
        .count();
    let mut predicate_counts: BTreeMap<String, usize> = BTreeMap::new();
    let mut connected_documents: BTreeMap<String, (usize, Option<String>)> = BTreeMap::new();

    for wire in &connected_wires {
        let predicate =
            json_string(wire.get("predicate")).unwrap_or_else(|| "relatedTo".to_string());
        *predicate_counts
            .entry(predicate_short_name(&predicate))
            .or_insert(0) += 1;

        let other_document_id =
            if json_string(wire.get("sourceDocumentId")).as_deref() == Some(document_id) {
                json_string(wire.get("targetDocumentId"))
            } else {
                json_string(wire.get("sourceDocumentId"))
            };
        if let Some(other_document_id) = other_document_id {
            let title = workspace_document_title(documents, Some(&other_document_id));
            let entry = connected_documents
                .entry(other_document_id)
                .or_insert((0, title));
            entry.0 += 1;
        }
    }

    DocumentDigestWireSummary {
        incoming_count,
        outgoing_count,
        total_count: connected_wires.len(),
        predicate_counts,
        top_connected_documents: top_connected_document_values(
            connected_documents,
            max_connected_documents,
        ),
    }
}

fn top_connected_document_values(
    connected_documents: BTreeMap<String, (usize, Option<String>)>,
    max_count: usize,
) -> Vec<serde_json::Value> {
    let mut values = connected_documents
        .into_iter()
        .map(|(document_id, (count, title))| {
            serde_json::json!({
                "document_id": document_id,
                "documentId": document_id,
                "title": title,
                "count": count,
            })
        })
        .collect::<Vec<_>>();
    values.sort_by(|left, right| {
        right
            .get("count")
            .and_then(serde_json::Value::as_u64)
            .cmp(&left.get("count").and_then(serde_json::Value::as_u64))
    });
    values.truncate(max_count);
    values
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn top_connected_documents_sort_descending_and_truncate() {
        let values = top_connected_document_values(
            BTreeMap::from([
                ("doc-a".to_string(), (2, Some("A".to_string()))),
                ("doc-b".to_string(), (5, Some("B".to_string()))),
                ("doc-c".to_string(), (1, None)),
            ]),
            2,
        );

        assert_eq!(values.len(), 2);
        assert_eq!(
            json_string(values[0].get("document_id")).as_deref(),
            Some("doc-b")
        );
        assert_eq!(
            values[0].get("count").and_then(serde_json::Value::as_u64),
            Some(5)
        );
        assert_eq!(
            json_string(values[1].get("documentId")).as_deref(),
            Some("doc-a")
        );
        assert_eq!(
            values[1].get("count").and_then(serde_json::Value::as_u64),
            Some(2)
        );
    }
}
