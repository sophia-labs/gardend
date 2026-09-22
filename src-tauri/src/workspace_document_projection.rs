use crate::document_record_store::DocumentInventoryRecord;
use std::collections::HashMap;

pub(super) fn workspace_document_rows(
    snapshot: Option<&serde_json::Value>,
    document_records: &[DocumentInventoryRecord],
) -> Vec<serde_json::Value> {
    let mut documents = snapshot
        .and_then(|value| value.get("documents"))
        .and_then(serde_json::Value::as_array)
        .filter(|documents| !documents.is_empty())
        .cloned()
        .unwrap_or_else(|| fallback_workspace_documents(document_records));
    merge_workspace_document_counts(&mut documents, document_records);
    documents
}

pub(super) fn fallback_workspace_documents(
    document_records: &[DocumentInventoryRecord],
) -> Vec<serde_json::Value> {
    let mut documents = document_records
        .iter()
        .map(|document| {
            serde_json::json!({
                "type": "document",
                "document_id": document.document_id.clone(),
                "documentId": document.document_id.clone(),
                "title": document.title.clone(),
                "updated_at": document.updated_at.clone(),
                "updatedAt": document.updated_at.clone(),
                "block_count": document.block_count,
                "blockCount": document.block_count,
                "rdfTripleCount": document.rdf_triple_count,
                "readOnly": false,
            })
        })
        .collect::<Vec<_>>();
    documents.sort_by(|left, right| {
        left.get("title")
            .and_then(serde_json::Value::as_str)
            .cmp(&right.get("title").and_then(serde_json::Value::as_str))
    });
    documents
}

pub(super) fn merge_workspace_document_counts(
    documents: &mut [serde_json::Value],
    document_records: &[DocumentInventoryRecord],
) {
    let mut by_id = HashMap::with_capacity(document_records.len());
    for record in document_records {
        // Preserve the old .find() semantics for duplicate embedded IDs:
        // the first record in the stable updated_at-sorted inventory wins.
        by_id.entry(record.document_id.as_str()).or_insert(record);
    }
    for document in documents {
        let Some(object) = document.as_object_mut() else {
            continue;
        };
        let document_id = object
            .get("documentId")
            .or_else(|| object.get("document_id"))
            .or_else(|| object.get("id"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        let Some(record) = by_id.get(document_id) else {
            continue;
        };
        object.insert(
            "document_id".to_string(),
            serde_json::json!(record.document_id),
        );
        object.insert(
            "documentId".to_string(),
            serde_json::json!(record.document_id),
        );
        object.insert(
            "block_count".to_string(),
            serde_json::json!(record.block_count),
        );
        object.insert(
            "blockCount".to_string(),
            serde_json::json!(record.block_count),
        );
        object.insert(
            "rdfTripleCount".to_string(),
            serde_json::json!(record.rdf_triple_count),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document_service::{BlockSnapshot, DocumentRecord};
    use crate::runtime_config::{DOCUMENT_SCHEMA_VERSION, LOCAL_GRAPH_ORIGIN, LOCAL_PROVIDER_ID};

    fn test_block(id: &str) -> BlockSnapshot {
        BlockSnapshot {
            id: id.to_string(),
            block_type: "paragraph".to_string(),
            content: format!("content {id}"),
            parent_id: None,
            order: 0.0,
            level: None,
            checked: None,
            language: None,
            marks: vec![],
        }
    }

    fn test_document_record(id: &str, title: &str, blocks: Vec<BlockSnapshot>) -> DocumentRecord {
        DocumentRecord {
            document_id: id.to_string(),
            graph_id: "graph-a".to_string(),
            title: title.to_string(),
            revision: 1,
            body: String::new(),
            origin: LOCAL_GRAPH_ORIGIN.to_string(),
            provider_id: LOCAL_PROVIDER_ID.to_string(),
            local_path: format!("/tmp/{id}"),
            rdf_subject: format!("urn:test:{id}"),
            created_at: "1".to_string(),
            updated_at: "2".to_string(),
            capabilities: vec![],
            schema_version: DOCUMENT_SCHEMA_VERSION,
            tiptap_xml: String::new(),
            tiptap_json: None,
            ydoc_update_base64: String::new(),
            ydoc_state_path: String::new(),
            tree: None,
            blocks,
            rdf_triple_count: 0,
        }
    }

    #[test]
    fn fallback_workspace_documents_sort_and_include_projection_counts() {
        let mut doc_b = test_document_record("doc-b", "B", vec![test_block("b1")]);
        doc_b.rdf_triple_count = 7;
        let doc_a = test_document_record("doc-a", "A", vec![test_block("a1"), test_block("a2")]);

        let values = fallback_workspace_documents(&[doc_b.into(), doc_a.into()]);

        assert_eq!(values[0]["documentId"], "doc-a");
        assert_eq!(values[0]["blockCount"], 2);
        assert_eq!(values[1]["document_id"], "doc-b");
        assert_eq!(values[1]["rdfTripleCount"], 7);
        assert_eq!(values[1]["readOnly"], false);
    }

    #[test]
    fn workspace_document_counts_merge_into_snapshot_documents() {
        let record = test_document_record("doc-a", "A", vec![test_block("a1"), test_block("a2")]);
        let mut documents = vec![serde_json::json!({
            "id": "doc-a",
            "title": "Snapshot Title"
        })];

        merge_workspace_document_counts(&mut documents, &[record.into()]);

        assert_eq!(documents[0]["document_id"], "doc-a");
        assert_eq!(documents[0]["documentId"], "doc-a");
        assert_eq!(documents[0]["block_count"], 2);
        assert_eq!(documents[0]["blockCount"], 2);
    }

    #[test]
    fn workspace_inventory_index_preserves_first_wins_aliases_and_unknown_rows() {
        let mut first = DocumentInventoryRecord::from(test_document_record(
            "doc-a", "First", vec![test_block("a1"), test_block("a2")],
        ));
        first.rdf_triple_count = 7;
        let later = DocumentInventoryRecord::from(test_document_record("doc-a", "Later", vec![]));
        let untouched = serde_json::json!({"id": "missing", "blockCount": 88, "extra": {"x": 1}});
        let blocked_alias = serde_json::json!({"documentId": null, "id": "doc-a", "blockCount": 99});
        let mut rows = vec![
            serde_json::json!({"id": "doc-a", "title": "Workspace title", "parentId": "p", "order": 4,
                "readOnly": true, "extra": {"custom": [1, 2]}}),
            serde_json::json!({"document_id": "doc-a"}),
            untouched.clone(), blocked_alias.clone(), serde_json::json!(42),
        ];
        merge_workspace_document_counts(&mut rows, &[first, later]);
        assert_eq!(rows[0], serde_json::json!({
            "id": "doc-a", "document_id": "doc-a", "documentId": "doc-a",
            "title": "Workspace title", "parentId": "p", "order": 4, "readOnly": true,
            "extra": {"custom": [1, 2]}, "blockCount": 2, "block_count": 2, "rdfTripleCount": 7,
        }));
        assert_eq!(rows[1]["blockCount"], 2);
        assert_eq!(rows[2], untouched);
        assert_eq!(rows[3], blocked_alias, "do not heal invalid preferred aliases");
        assert_eq!(rows[4], 42);
    }

    #[test]
    fn workspace_inventory_rows_preserve_snapshot_authority_and_fallback_rules() {
        let records = vec![DocumentInventoryRecord::from(test_document_record("doc-a", "Manifest title", vec![]))];
        let snapshot = serde_json::json!({
            "documents": [{"id": "missing", "title": "Only snapshot row", "custom": "keep"}],
            "other": {"unchanged": true},
        });
        let before = snapshot.clone();
        let rows = workspace_document_rows(Some(&snapshot), &records);
        assert_eq!(rows, snapshot["documents"].as_array().unwrap().clone());
        assert_eq!(snapshot, before, "count overlays may not mutate the authoritative snapshot");
        for empty in [serde_json::json!({}), serde_json::json!({"documents": []}),
            serde_json::json!({"documents": null})] {
            assert_eq!(workspace_document_rows(Some(&empty), &records), workspace_document_rows(None, &records));
        }
        let fallback = workspace_document_rows(None, &records);
        assert_eq!(fallback[0]["title"], "Manifest title");
        assert_eq!(fallback[0]["readOnly"], false);
    }

    #[test]
    fn workspace_inventory_index_matches_legacy_linear_join_for_many_rows() {
        let records = (0..2048).map(|index| DocumentInventoryRecord {
            document_id: format!("doc-{index}"), title: format!("Title {index}"),
            updated_at: "1".into(), block_count: index, rdf_triple_count: index * 2,
        }).collect::<Vec<_>>();
        let mut rows = (0..2048).rev().map(|index| serde_json::json!({
            "id": format!("doc-{index}"), "title": "Workspace", "custom": index,
        })).collect::<Vec<_>>();
        merge_workspace_document_counts(&mut rows, &records);
        for row in rows {
            let record = records.iter().find(|record| Some(record.document_id.as_str()) == row["id"].as_str()).unwrap();
            assert_eq!(row["blockCount"], record.block_count);
            assert_eq!(row["rdfTripleCount"], record.rdf_triple_count);
            assert_eq!(row["title"], "Workspace");
            assert_eq!(row["custom"], record.block_count);
        }
    }
}
