use crate::{
    json_utils::{json_bool, json_number, json_scalar_lexical, json_string},
    rdf::{
        push_boolean_triple, push_float_triple, push_string_triple, push_typed_literal_triple,
        push_uri_triple, RdfTriple,
    },
    rdf_workspace_terms::folder_ref_uri,
    runtime_config::{MDOC_NS, NFO_NS, XSD_NS},
};

pub(super) fn push_parent_folder_triple(
    triples: &mut Vec<RdfTriple>,
    graph_id: &str,
    subject: &str,
    entity: &serde_json::Value,
) {
    if let Some(parent_id) = json_string(
        entity
            .get("parentId")
            .or_else(|| extra_value(entity, "parentId")),
    ) {
        push_uri_triple(
            triples,
            subject,
            &format!("{NFO_NS}belongsToContainer"),
            &folder_ref_uri(graph_id, &parent_id),
        );
    }
}

pub(super) fn push_workspace_number_triple(
    triples: &mut Vec<RdfTriple>,
    subject: &str,
    predicate: &str,
    entity: &serde_json::Value,
    field: &str,
) {
    if let Some(number) = json_number(entity.get(field).or_else(|| extra_value(entity, field))) {
        push_float_triple(triples, subject, predicate, number);
    }
}

pub(super) fn push_workspace_extra_triples(
    triples: &mut Vec<RdfTriple>,
    _graph_id: &str,
    subject: &str,
    entity: &serde_json::Value,
    fields: &[(&str, String, Option<String>)],
) {
    for (field, predicate, datatype) in fields {
        push_workspace_value_triple(
            triples,
            subject,
            predicate,
            entity.get(*field).or_else(|| extra_value(entity, field)),
            datatype.as_deref(),
        );
    }
}

pub(super) fn push_document_source_file_triples(
    triples: &mut Vec<RdfTriple>,
    subject: &str,
    document: &serde_json::Value,
) {
    let source_file = document
        .get("sourceFile")
        .and_then(serde_json::Value::as_object);
    for (field, predicate, datatype) in [
        ("sf_storageKey", format!("{MDOC_NS}sourceStorageKey"), None),
        (
            "sf_originalFilename",
            format!("{MDOC_NS}sourceOriginalFilename"),
            None,
        ),
        ("sf_mimeType", format!("{MDOC_NS}sourceMimeType"), None),
        (
            "sf_sizeBytes",
            format!("{MDOC_NS}sourceContentSize"),
            Some(format!("{XSD_NS}integer")),
        ),
        ("sf_fileType", format!("{MDOC_NS}sourceFileType"), None),
    ] {
        let value = document
            .get(field)
            .or_else(|| source_file.and_then(|source_file| source_file.get(field)))
            .or_else(|| extra_value(document, field));
        push_workspace_value_triple(triples, subject, &predicate, value, datatype.as_deref());
    }
}

pub(super) fn push_workspace_value_triple(
    triples: &mut Vec<RdfTriple>,
    subject: &str,
    predicate: &str,
    value: Option<&serde_json::Value>,
    datatype: Option<&str>,
) {
    let Some(value) = value else {
        return;
    };
    if value.is_null() {
        return;
    }
    if let Some(datatype) = datatype {
        if datatype == format!("{XSD_NS}boolean") {
            if let Some(value) = json_bool(Some(value)) {
                push_boolean_triple(triples, subject, predicate, value);
            }
            return;
        }
        let Some(lexical) = json_scalar_lexical(Some(value)) else {
            return;
        };
        push_typed_literal_triple(triples, subject, predicate, &lexical, datatype);
        return;
    }
    if let Some(value) = json_scalar_lexical(Some(value)) {
        push_string_triple(triples, subject, predicate, &value);
    }
}

pub(super) fn extra_value<'a>(
    entity: &'a serde_json::Value,
    key: &str,
) -> Option<&'a serde_json::Value> {
    entity
        .get("extra")
        .and_then(serde_json::Value::as_object)
        .and_then(|extra| extra.get(key))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rdf::format_rdf_triple;

    #[test]
    fn extra_value_reads_snapshot_extra_fields_only() {
        let entity = serde_json::json!({
            "title": "Direct",
            "extra": { "title": "Extra", "createdAt": "1000" }
        });

        assert_eq!(
            extra_value(&entity, "title").and_then(|value| value.as_str()),
            Some("Extra")
        );
        assert_eq!(
            extra_value(&entity, "createdAt").and_then(|value| value.as_str()),
            Some("1000")
        );
        assert!(extra_value(&entity, "missing").is_none());
    }

    #[test]
    fn workspace_value_triple_emits_strings_booleans_and_typed_literals() {
        let mut triples = Vec::new();
        push_workspace_value_triple(
            &mut triples,
            "urn:test:s",
            "urn:test:name",
            Some(&serde_json::json!("Name")),
            None,
        );
        push_workspace_value_triple(
            &mut triples,
            "urn:test:s",
            "urn:test:flag",
            Some(&serde_json::json!(true)),
            Some(&format!("{XSD_NS}boolean")),
        );
        push_workspace_value_triple(
            &mut triples,
            "urn:test:s",
            "urn:test:size",
            Some(&serde_json::json!(42)),
            Some(&format!("{XSD_NS}integer")),
        );

        let rendered = triples.iter().map(format_rdf_triple).collect::<Vec<_>>();
        assert!(rendered
            .iter()
            .any(|triple| triple.contains("<urn:test:name> \"Name\"")));
        assert!(rendered
            .iter()
            .any(|triple| triple.contains("<urn:test:flag> \"true\"^^")));
        assert!(rendered
            .iter()
            .any(|triple| triple.contains("<urn:test:size> \"42\"^^")));
    }

    #[test]
    fn document_source_file_byte_count_is_an_xsd_integer() {
        let mut triples = Vec::new();
        push_document_source_file_triples(
            &mut triples,
            "urn:test:document",
            &serde_json::json!({
                "sourceFile": {
                    "sf_sizeBytes": 1234
                }
            }),
        );

        let rendered = triples.iter().map(format_rdf_triple).collect::<Vec<_>>();
        assert!(rendered.iter().any(|triple| {
            triple.contains("sourceContentSize")
                && triple.contains("\"1234\"^^")
                && triple.contains("#integer")
        }));
    }
}
