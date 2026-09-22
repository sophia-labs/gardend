use crate::runtime_config::XSD_NS;
use oxigraph::io::RdfFormat;

#[derive(Debug, Clone)]
enum RdfObject {
    Uri(String),
    Literal(String),
}

#[derive(Debug, Clone)]
pub(crate) struct RdfTriple {
    subject: String,
    predicate: String,
    object: RdfObject,
}

pub(crate) fn push_uri_triple(
    triples: &mut Vec<RdfTriple>,
    subject: &str,
    predicate: &str,
    object: &str,
) {
    triples.push(RdfTriple {
        subject: subject.to_string(),
        predicate: predicate.to_string(),
        object: RdfObject::Uri(object.to_string()),
    });
}

pub(crate) fn push_string_triple(
    triples: &mut Vec<RdfTriple>,
    subject: &str,
    predicate: &str,
    value: &str,
) {
    triples.push(RdfTriple {
        subject: subject.to_string(),
        predicate: predicate.to_string(),
        object: RdfObject::Literal(sparql_string_literal(value)),
    });
}

pub(crate) fn push_integer_triple(
    triples: &mut Vec<RdfTriple>,
    subject: &str,
    predicate: &str,
    value: i64,
) {
    triples.push(RdfTriple {
        subject: subject.to_string(),
        predicate: predicate.to_string(),
        object: RdfObject::Literal(integer_literal(value)),
    });
}

pub(crate) fn push_float_triple(
    triples: &mut Vec<RdfTriple>,
    subject: &str,
    predicate: &str,
    value: f64,
) {
    triples.push(RdfTriple {
        subject: subject.to_string(),
        predicate: predicate.to_string(),
        object: RdfObject::Literal(float_literal(value)),
    });
}

pub(crate) fn push_boolean_triple(
    triples: &mut Vec<RdfTriple>,
    subject: &str,
    predicate: &str,
    value: bool,
) {
    triples.push(RdfTriple {
        subject: subject.to_string(),
        predicate: predicate.to_string(),
        object: RdfObject::Literal(boolean_literal(value)),
    });
}

pub(crate) fn push_typed_literal_triple(
    triples: &mut Vec<RdfTriple>,
    subject: &str,
    predicate: &str,
    value: &str,
    datatype: &str,
) {
    triples.push(RdfTriple {
        subject: subject.to_string(),
        predicate: predicate.to_string(),
        object: RdfObject::Literal(typed_literal(value, datatype)),
    });
}

pub(crate) fn format_rdf_triple(triple: &RdfTriple) -> String {
    let object = match &triple.object {
        RdfObject::Uri(uri) => format!("<{uri}>"),
        RdfObject::Literal(literal) => literal.clone(),
    };
    format!("<{}> <{}> {} .", triple.subject, triple.predicate, object)
}

pub(crate) fn parse_rdf_format(value: &str) -> Result<RdfFormat, String> {
    let normalized = value.trim();
    let lower = normalized.to_ascii_lowercase();
    match lower.as_str() {
        "ttl" | "turtle" => Ok(RdfFormat::Turtle),
        "nt" | "ntriples" | "n-triples" => Ok(RdfFormat::NTriples),
        "nq" | "nquads" | "n-quads" => Ok(RdfFormat::NQuads),
        "trig" => Ok(RdfFormat::TriG),
        "rdf" | "xml" | "rdfxml" | "rdf/xml" => Ok(RdfFormat::RdfXml),
        "n3" => Ok(RdfFormat::N3),
        "json" | "jsonld" | "json-ld" => RdfFormat::from_extension("jsonld")
            .ok_or_else(|| "JSON-LD format is unavailable".to_string()),
        _ => RdfFormat::from_media_type(normalized)
            .or_else(|| RdfFormat::from_extension(normalized))
            .ok_or_else(|| format!("unsupported RDF format: {value}")),
    }
}

pub(crate) fn graph_subject(graph_id: &str) -> String {
    format!("urn:mnemosyne:local:graph:{graph_id}")
}

pub(crate) fn document_subject(document_id: &str) -> String {
    format!("urn:mnemosyne:local:document:{document_id}")
}

pub(crate) fn sparql_string_literal(value: &str) -> String {
    let escaped = value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
        .replace('\t', "\\t");
    format!("\"{escaped}\"")
}

pub(crate) fn integer_literal(value: i64) -> String {
    format!("\"{value}\"^^<{XSD_NS}integer>")
}

pub(crate) fn float_literal(value: f64) -> String {
    format!("\"{value}\"^^<{XSD_NS}float>")
}

pub(crate) fn boolean_literal(value: bool) -> String {
    format!("\"{}\"^^<{XSD_NS}boolean>", value)
}

fn typed_literal(value: &str, datatype: &str) -> String {
    let escaped = value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
        .replace('\t', "\\t");
    format!("\"{escaped}\"^^<{datatype}>")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sparql_string_literal_escapes_control_characters() {
        assert_eq!(
            sparql_string_literal("a \"quoted\"\nline\\tail"),
            "\"a \\\"quoted\\\"\\nline\\\\tail\""
        );
    }

    #[test]
    fn format_rdf_triple_renders_uri_and_literal_objects() {
        let mut triples = Vec::new();
        push_uri_triple(&mut triples, "urn:s", "urn:p", "urn:o");
        push_string_triple(&mut triples, "urn:s", "urn:p2", "value");
        assert_eq!(format_rdf_triple(&triples[0]), "<urn:s> <urn:p> <urn:o> .");
        assert_eq!(
            format_rdf_triple(&triples[1]),
            "<urn:s> <urn:p2> \"value\" ."
        );
    }

    #[test]
    fn parse_rdf_format_accepts_common_aliases() {
        assert_eq!(parse_rdf_format("ttl").expect("ttl"), RdfFormat::Turtle);
        assert_eq!(
            parse_rdf_format("rdf/xml").expect("rdf/xml"),
            RdfFormat::RdfXml
        );
        assert!(parse_rdf_format("unknown-format").is_err());
    }
}
