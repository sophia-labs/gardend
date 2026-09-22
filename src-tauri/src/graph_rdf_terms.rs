use crate::rdf_service::SparqlQueryResult;

pub(super) fn sparql_count(result: SparqlQueryResult, key: &str) -> usize {
    result
        .rows
        .first()
        .and_then(|row| rdf_count_literal(row.get(key)))
        .unwrap_or(0)
}

pub(super) fn rdf_count_literal(raw: Option<&String>) -> Option<usize> {
    let value = raw?.trim();
    let lexical = if let Some(stripped) = value.strip_prefix('"') {
        stripped.split('"').next().unwrap_or_default()
    } else {
        value
    };
    lexical.parse::<usize>().ok()
}

pub(super) fn rdf_term_lexical(raw: &String) -> Option<String> {
    let value = raw.trim();
    if value.is_empty() {
        return None;
    }
    if let Some(stripped) = value
        .strip_prefix('<')
        .and_then(|text| text.strip_suffix('>'))
    {
        return Some(stripped.to_string());
    }
    if let Some(stripped) = value.strip_prefix('"') {
        return Some(stripped.split('"').next().unwrap_or_default().to_string());
    }
    Some(value.to_string())
}

pub(super) fn rdf_friendly_label(iri: &str) -> String {
    let trimmed = iri.trim_end_matches(['/', '#']);
    if let Some((_, label)) = trimmed.rsplit_once('#') {
        if !label.is_empty() {
            return label.to_string();
        }
    }
    if let Some((_, label)) = trimmed.rsplit_once('/') {
        if !label.is_empty() {
            return label.to_string();
        }
    }
    iri.to_string()
}

pub(super) fn rdf_node_kind(iri: &str) -> &'static str {
    if iri.contains(":artifact:") {
        "artifact"
    } else if iri.contains(":document:")
        || iri.contains(":doc:")
        || iri.ends_with("#document")
        || iri.ends_with("/document")
    {
        "document"
    } else if iri.contains(":tag:") {
        "tag"
    } else {
        "entity"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rdf_count_literal_accepts_plain_and_typed_literals() {
        assert_eq!(rdf_count_literal(Some(&"42".to_string())), Some(42));
        assert_eq!(
            rdf_count_literal(Some(
                &"\"42\"^^<http://www.w3.org/2001/XMLSchema#integer>".to_string()
            )),
            Some(42)
        );
        assert_eq!(rdf_count_literal(Some(&"not-a-count".to_string())), None);
        assert_eq!(rdf_count_literal(None), None);
    }

    #[test]
    fn rdf_term_lexical_normalizes_iri_and_literal_rows() {
        assert_eq!(
            rdf_term_lexical(&"<urn:mnemosyne:local:graph:graph-a>".to_string()),
            Some("urn:mnemosyne:local:graph:graph-a".to_string())
        );
        assert_eq!(
            rdf_term_lexical(&"\"Graph A\"@en".to_string()),
            Some("Graph A".to_string())
        );
        assert_eq!(rdf_term_lexical(&"".to_string()), None);
    }

    #[test]
    fn rdf_label_and_node_kind_preserve_hosted_viz_compatibility() {
        assert_eq!(
            rdf_friendly_label("https://example.test/ns#supports"),
            "supports"
        );
        assert_eq!(
            rdf_friendly_label("https://example.test/ns/supports/"),
            "supports"
        );
        assert_eq!(
            rdf_node_kind("urn:mnemosyne:local:artifact:artifact-a"),
            "artifact"
        );
        assert_eq!(
            rdf_node_kind("urn:mnemosyne:local:document:doc-a"),
            "document"
        );
        assert_eq!(rdf_node_kind("urn:mnemosyne:local:tag:tag-a"), "tag");
        assert_eq!(rdf_node_kind("urn:mnemosyne:local:entity:x"), "entity");
    }
}
