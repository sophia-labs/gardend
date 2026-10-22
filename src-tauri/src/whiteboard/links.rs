//! The board's link vocabulary: what a shape's `link` string and `customData.mnemosyne` record say (SPRINT D7).
//! FINAL: no unit edits this file; G1, G2 and G5 read it.
//!
//! A shape or card points at something with its Excalidraw `link`:
//!
//! - `mnemosyne://document/<graph>/<documentId>`
//! - `mnemosyne://object/<graph>/<urlencoded object IRI>`
//! - `mnemosyne://artifact/<graph>/<artifactId>`
//! - anything else (`https://...`) is a web address and means nothing to the graph.
//!
//! Each segment is percent-encoded (`encode`: everything but `A-Za-z0-9-_.!~*'()`). The same element also carries
//! `customData.mnemosyne`, the snapshot the board keeps so it can show a name when the target is gone:
//!
//! - document: `{kind: "document", graphId, id, title}`
//! - object:   `{kind: "object", graphId, iri, label, classLabel}`
//!
//! A card (`type: "embeddable"`) always carries an object or document link and this record.
//! `customData.mnemosyneWire {wireId, predicate}` marks an arrow that was promoted to a wire.

/// What a `link` string points at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LinkTarget {
    Document {
        graph_id: String,
        document_id: String,
    },
    Object {
        graph_id: String,
        iri: String,
    },
    Artifact {
        graph_id: String,
        artifact_id: String,
    },
}

/// Percent-encode one path segment of a `mnemosyne://` link.
pub(crate) fn encode(value: &str) -> String {
    let mut out = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&byte) {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// Undo [`encode`]. A malformed escape or invalid UTF-8 gives `None`.
pub(crate) fn decode(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = value.get(i + 1..i + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

pub(crate) fn document_link(graph_id: &str, document_id: &str) -> String {
    format!(
        "mnemosyne://document/{}/{}",
        encode(graph_id),
        encode(document_id)
    )
}

pub(crate) fn object_link(graph_id: &str, iri: &str) -> String {
    format!("mnemosyne://object/{}/{}", encode(graph_id), encode(iri))
}

pub(crate) fn artifact_link(graph_id: &str, artifact_id: &str) -> String {
    format!(
        "mnemosyne://artifact/{}/{}",
        encode(graph_id),
        encode(artifact_id)
    )
}

/// Parse a `link` string. Anything that is not a well-formed `mnemosyne://document|object|artifact/<g>/<x>` is `None`.
/// A trailing `?query` or `#fragment` is ignored.
pub(crate) fn parse_link(link: &str) -> Option<LinkTarget> {
    let rest = link.strip_prefix("mnemosyne://")?;
    let rest = rest.split(['?', '#']).next().unwrap_or(rest);
    let mut parts = rest.splitn(3, '/');
    let kind = parts.next()?;
    let graph_id = decode(parts.next()?)?;
    let tail = decode(parts.next()?)?;
    if graph_id.is_empty() || tail.is_empty() {
        return None;
    }
    match kind {
        "document" => Some(LinkTarget::Document {
            graph_id,
            document_id: tail,
        }),
        "object" => Some(LinkTarget::Object {
            graph_id,
            iri: tail,
        }),
        "artifact" => Some(LinkTarget::Artifact {
            graph_id,
            artifact_id: tail,
        }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links_round_trip() {
        let iri = "urn:mnemosyne:local:graph:freud:run:r1#item/1 a";
        assert_eq!(
            parse_link(&object_link("freud", iri)),
            Some(LinkTarget::Object {
                graph_id: "freud".into(),
                iri: iri.into()
            })
        );
        assert_eq!(
            parse_link(&document_link("freud", "doc-1")),
            Some(LinkTarget::Document {
                graph_id: "freud".into(),
                document_id: "doc-1".into()
            })
        );
        assert_eq!(
            parse_link(&artifact_link("g", "a/b")),
            Some(LinkTarget::Artifact {
                graph_id: "g".into(),
                artifact_id: "a/b".into()
            })
        );
    }

    #[test]
    fn foreign_and_malformed_links_are_none() {
        assert_eq!(parse_link("https://example.com/x"), None);
        assert_eq!(parse_link("mnemosyne://document/g"), None);
        assert_eq!(parse_link("mnemosyne://widget/g/x"), None);
        assert_eq!(parse_link("mnemosyne://document/g/%zz"), None);
    }
}
