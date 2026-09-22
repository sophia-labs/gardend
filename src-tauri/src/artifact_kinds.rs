//! Flexible artifact-type abstraction.
//!
//! Maps an artifact's mime type to a semantic `kind`, and declares each kind's
//! capabilities along three axes:
//!   - `perceive`: how an agent reads it — `text` | `vision` | `transcript` | `metadata`
//!   - `render`:   how the UI shows it   — `inline` | `thumbnail` | `player` | `icon`
//!   - `derive`:   optional artifact→knowledge transforms (e.g. `ocr`, `extract-text`)
//!
//! The graph is the canonical home for this typing: each artifact is emitted
//! with `rdf:type mdoc:<Class>` (see `rdf_workspace_entity_triples`) and the kind
//! classes + capability predicates are materialized as ontology triples into the
//! workspace projection graph (see `rdf_workspace_materializer`). `kind_from_mime`
//! is the single piece of logic — kind is a deterministic function of mime, so
//! existing artifacts type correctly with no data migration, and consumers
//! (UI render, agent perception) can mirror it or read capabilities from the graph.

use crate::{
    rdf::{push_string_triple, push_uri_triple, RdfTriple},
    runtime_config::MDOC_NS,
};

const RDFS_SUBCLASS_OF: &str = "http://www.w3.org/2000/01/rdf-schema#subClassOf";

/// Capability descriptor for one artifact kind. `class` is the RDF class local
/// name under `mdoc:` (e.g. "Image" → `mdoc:Image`); `slug` is the lowercase id
/// mirrored onto the artifact and used by the UI/agent.
pub(crate) struct ArtifactKind {
    pub(crate) slug: &'static str,
    pub(crate) class: &'static str,
    pub(crate) perceive: &'static str,
    pub(crate) render: &'static str,
    pub(crate) derive: &'static [&'static str],
}

pub(crate) const ARTIFACT_KINDS: &[ArtifactKind] = &[
    ArtifactKind {
        slug: "image",
        class: "Image",
        perceive: "vision",
        render: "thumbnail",
        derive: &["caption", "ocr"],
    },
    ArtifactKind {
        slug: "pdf",
        class: "Pdf",
        perceive: "text",
        render: "thumbnail",
        derive: &["extract-text"],
    },
    ArtifactKind {
        slug: "audio",
        class: "Audio",
        perceive: "transcript",
        render: "player",
        derive: &["transcribe"],
    },
    ArtifactKind {
        slug: "video",
        class: "Video",
        perceive: "transcript",
        render: "player",
        derive: &["transcribe"],
    },
    ArtifactKind {
        slug: "scene",
        class: "Scene",
        perceive: "structural",
        render: "icon",
        derive: &["flatten-to-image"],
    },
    ArtifactKind {
        slug: "text",
        class: "TextFile",
        perceive: "text",
        render: "inline",
        derive: &[],
    },
    // Fallback: anything unrecognized is still a first-class artifact — rendered
    // as an icon, perceivable as metadata — never silently hidden.
    ArtifactKind {
        slug: "binary",
        class: "Binary",
        perceive: "metadata",
        render: "icon",
        derive: &[],
    },
];

pub(crate) const FALLBACK_KIND: &str = "binary";

/// Derive an artifact's semantic kind from its mime type (and optional fileType
/// hint). Deterministic and total — unrecognized inputs map to `binary`.
pub(crate) fn kind_from_mime(mime: &str, file_type: Option<&str>) -> &'static str {
    let m = mime.trim().to_ascii_lowercase();
    if m.starts_with("image/") {
        return "image";
    }
    if m == "application/pdf" || matches!(file_type, Some(ft) if ft.eq_ignore_ascii_case("pdf")) {
        return "pdf";
    }
    if m.starts_with("audio/") {
        return "audio";
    }
    if m.starts_with("video/") {
        return "video";
    }
    if m == "application/vnd.excalidraw+json"
        || matches!(file_type, Some(ft) if ft.eq_ignore_ascii_case("excalidraw"))
    {
        return "scene";
    }
    if m.starts_with("text/")
        || m == "application/json"
        || m == "application/xml"
        || m == "application/xhtml+xml"
    {
        return "text";
    }
    FALLBACK_KIND
}

fn kind_descriptor(slug: &str) -> &'static ArtifactKind {
    ARTIFACT_KINDS
        .iter()
        .find(|k| k.slug == slug)
        .unwrap_or_else(|| {
            ARTIFACT_KINDS
                .iter()
                .find(|k| k.slug == FALLBACK_KIND)
                .expect("fallback artifact kind is always present")
        })
}

/// RDF class local name for a kind slug (falls back to `Binary`).
pub(crate) fn kind_class(slug: &str) -> &'static str {
    kind_descriptor(slug).class
}

/// Graph-canonical ontology: each kind class `rdfs:subClassOf mdoc:Artifact`,
/// plus its `perceive`/`render`/`derive` capability predicates. Materialized
/// into the workspace projection graph so artifact typing + capabilities are
/// SPARQL-discoverable (e.g. `?a a mdoc:Image`; `mdoc:Image mdoc:perceive ?p`).
pub(crate) fn ontology_triples() -> Vec<RdfTriple> {
    let mut triples = Vec::new();
    let artifact_class = format!("{MDOC_NS}Artifact");
    for kind in ARTIFACT_KINDS {
        let class_iri = format!("{MDOC_NS}{}", kind.class);
        push_uri_triple(&mut triples, &class_iri, RDFS_SUBCLASS_OF, &artifact_class);
        push_string_triple(
            &mut triples,
            &class_iri,
            &format!("{MDOC_NS}kindSlug"),
            kind.slug,
        );
        push_string_triple(
            &mut triples,
            &class_iri,
            &format!("{MDOC_NS}perceive"),
            kind.perceive,
        );
        push_string_triple(
            &mut triples,
            &class_iri,
            &format!("{MDOC_NS}render"),
            kind.render,
        );
        for derive in kind.derive {
            push_string_triple(
                &mut triples,
                &class_iri,
                &format!("{MDOC_NS}derive"),
                derive,
            );
        }
    }
    triples
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_common_mimes() {
        assert_eq!(kind_from_mime("image/png", None), "image");
        assert_eq!(kind_from_mime("image/jpeg", Some("jpg")), "image");
        assert_eq!(kind_from_mime("application/pdf", None), "pdf");
        assert_eq!(kind_from_mime("", Some("pdf")), "pdf");
        assert_eq!(kind_from_mime("audio/mpeg", None), "audio");
        assert_eq!(kind_from_mime("video/mp4", None), "video");
        assert_eq!(kind_from_mime("text/markdown", None), "text");
        assert_eq!(kind_from_mime("application/json", None), "text");
        assert_eq!(
            kind_from_mime("application/vnd.excalidraw+json", None),
            "scene"
        );
        assert_eq!(
            kind_from_mime("application/json", Some("excalidraw")),
            "scene"
        );
        assert_eq!(kind_from_mime("application/octet-stream", None), "binary");
        assert_eq!(kind_from_mime("", None), "binary");
    }

    #[test]
    fn kind_class_resolves_with_fallback() {
        assert_eq!(kind_class("image"), "Image");
        assert_eq!(kind_class("pdf"), "Pdf");
        assert_eq!(kind_class("nonsense"), "Binary");
    }

    #[test]
    fn every_kind_declares_capabilities() {
        for kind in ARTIFACT_KINDS {
            assert!(!kind.class.is_empty());
            assert!(!kind.perceive.is_empty());
            assert!(!kind.render.is_empty());
        }
    }

    #[test]
    fn ontology_emits_subclass_and_capabilities() {
        let triples = ontology_triples();
        // subClassOf + kindSlug + perceive + render per kind, at minimum.
        assert!(triples.len() >= ARTIFACT_KINDS.len() * 4);
    }
}
