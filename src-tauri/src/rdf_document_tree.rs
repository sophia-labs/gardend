use crate::{
    document_service::{DocumentRecord, TreeNodeSnapshot},
    rdf::{
        push_boolean_triple, push_integer_triple, push_string_triple, push_uri_triple, RdfTriple,
    },
    rdf_document_tree_terms::node_type_uri,
    rdf_document_tree_text::normalize_node_text,
    runtime_config::{MDOC_NS, MNEMO_NS, RDF_TYPE},
};

/// The Document projection's DECLARED bare-`<subject>` predicate vocabulary
/// (locals under [`MNEMO_NS`]). These are the predicates the Document projection
/// CO-MANAGES on the shared bare doc subject `<{subject}>` — the subject other
/// projections (salience / semantic / wires) also attach triples to, so the
/// Document projection may retract only ITS OWN predicates there.
///
/// This is the **single source of truth** for that vocabulary: the old wholesale
/// materializer's DELETE `VALUES ?local_p { … }` allowlist
/// ([`crate::rdf_record_materializer`]) and the Meaningful Object's owned-span
/// derivation ([`crate::document_meaningful_object`]) both reference THIS list,
/// so the parity contract can no longer silently drift between the two paths.
///
/// Note `document_tree_triples` itself never EMITS a bare-subject triple (it only
/// emits `{subject}#frag` / `#block-…` / `#node-…` subjects); these predicates
/// are written onto the bare subject by other persistence paths. The
/// `document_tree_triples_emit_only_declared_bare_predicates` test pins that
/// invariant so the declared set provably bounds what the projection emits.
pub(crate) const DOCUMENT_LEVEL_PREDICATES: &[&str] = &[
    "graphId",
    "origin",
    "providerId",
    "localPath",
    "body",
    "documentId",
    "schemaVersion",
    "ydocStatePath",
    "tiptapXml",
    "rdfTripleCount",
];

/// The 7 bare-`<subject>` predicates whose VALUE is FILESYSTEM/record-sourced —
/// the durable-store manifest-and-path values, re-derivable from the on-disk
/// `DocumentRecord` (hydrated from the Y.Doc snapshot on boot). This is the
/// vocabulary the storage-metadata Face
/// ([`crate::document_meaningful_object::Face::StorageMetadataToRdf`]) projects
/// onto the bare doc subject and OWNS in its reclaim span. It is a PARTITION of
/// [`DOCUMENT_LEVEL_PREDICATES`]: the 10 split disjointly into FS-sourced (7),
/// content (2), and projection-meta (1).
pub(crate) const FS_SOURCED_PREDICATES: &[&str] = &[
    "graphId",
    "origin",
    "providerId",
    "localPath",
    "documentId",
    "schemaVersion",
    "ydocStatePath",
];

/// The 2 bare-`<subject>` predicates carrying serializations of Y.Doc CONTENT
/// (authority = `SourceKind::Ydoc`): the plaintext `body` and the serialized
/// `tiptapXml`. NOT filesystem-authored (persisted to fs, but born in the CRDT);
/// they belong with the tree face's source family, NOT the storage-metadata
/// face. Deferred to a content-face iteration (OUT of scope here).
pub(crate) const CONTENT_PREDICATES: &[&str] = &["body", "tiptapXml"];

/// The 1 bare-`<subject>` predicate that is self-referential PROJECTION metadata
/// — `rdfTripleCount`, computed DURING projection over footprint (1)'s tree. Has
/// an ordering dependency (the tree face must run first), so it belongs on a tiny
/// separate projection-meta emitter, NOT the fs face (OUT of scope here).
pub(crate) const PROJECTION_META_PREDICATES: &[&str] = &["rdfTripleCount"];

#[cfg(test)]
const _: () = {
    // The three partitions reconstruct the 10-union exactly (sizes; membership is
    // pinned by the `partition_is_disjoint_cover_of_document_level_predicates`
    // test, which can use runtime set ops the const-evaluator can't).
    assert!(
        FS_SOURCED_PREDICATES.len() + CONTENT_PREDICATES.len() + PROJECTION_META_PREDICATES.len()
            == DOCUMENT_LEVEL_PREDICATES.len()
    );
};

pub(super) fn document_tree_triples(document: &DocumentRecord) -> Vec<RdfTriple> {
    let Some(tree) = &document.tree else {
        return Vec::new();
    };

    let mut triples = Vec::new();
    let mut counter = 0usize;
    let fragment_uri = document_fragment_uri(document);
    push_uri_triple(
        &mut triples,
        &fragment_uri,
        RDF_TYPE,
        &node_type_uri("fragment"),
    );
    push_string_triple(
        &mut triples,
        &fragment_uri,
        &format!("{MNEMO_NS}documentId"),
        &document.document_id,
    );

    for (index, child) in tree.root.children.iter().enumerate() {
        let child_uri = serialize_tree_node(document, child, &mut counter, &mut triples);
        push_uri_triple(
            &mut triples,
            &fragment_uri,
            &format!("{MDOC_NS}childNode"),
            &child_uri,
        );
        push_integer_triple(
            &mut triples,
            &child_uri,
            &format!("{MDOC_NS}siblingOrder"),
            index as i64,
        );
    }

    triples
}

fn serialize_tree_node(
    document: &DocumentRecord,
    node: &TreeNodeSnapshot,
    counter: &mut usize,
    triples: &mut Vec<RdfTriple>,
) -> String {
    if node.kind == "text" {
        let node_uri = anonymous_node_uri(document, counter);
        push_uri_triple(triples, &node_uri, RDF_TYPE, &node_type_uri("text"));
        push_string_triple(
            triples,
            &node_uri,
            &format!("{MNEMO_NS}documentId"),
            &document.document_id,
        );
        push_string_triple(
            triples,
            &node_uri,
            &format!("{MDOC_NS}content"),
            node.text_content.as_deref().unwrap_or_default(),
        );
        return node_uri;
    }

    let tag_name = node.tag_name.as_deref().unwrap_or("paragraph");
    let node_uri = if let Some(block_id) = &node.attributes.block_id {
        document_block_uri(document, block_id)
    } else {
        anonymous_node_uri(document, counter)
    };

    push_uri_triple(triples, &node_uri, RDF_TYPE, &node_type_uri(tag_name));
    push_string_triple(
        triples,
        &node_uri,
        &format!("{MNEMO_NS}documentId"),
        &document.document_id,
    );

    if let Some(block_id) = &node.attributes.block_id {
        push_string_triple(triples, &node_uri, &format!("{MDOC_NS}nodeId"), block_id);
        let block_text = normalize_node_text(node);
        if !block_text.is_empty() {
            push_string_triple(
                triples,
                &node_uri,
                &format!("{MDOC_NS}textContent"),
                &block_text,
            );
        }
    }
    if let Some(level) = node.attributes.level {
        push_integer_triple(triples, &node_uri, &format!("{MDOC_NS}level"), level);
    }
    if let Some(href) = &node.attributes.href {
        push_string_triple(triples, &node_uri, &format!("{MDOC_NS}href"), href);
    }
    if let Some(target) = &node.attributes.target {
        push_string_triple(triples, &node_uri, &format!("{MDOC_NS}target"), target);
    }
    if let Some(language) = &node.attributes.language {
        push_string_triple(triples, &node_uri, &format!("{MDOC_NS}language"), language);
    }
    if let Some(checked) = node.attributes.checked {
        push_boolean_triple(triples, &node_uri, &format!("{MDOC_NS}checked"), checked);
    }
    if let Some(footnote_content) = &node.attributes.footnote_content {
        push_string_triple(
            triples,
            &node_uri,
            &format!("{MDOC_NS}footnoteContent"),
            footnote_content,
        );
    }
    if let Some(annotation_id) = &node.attributes.annotation_id {
        push_string_triple(
            triples,
            &node_uri,
            &format!("{MDOC_NS}annotationId"),
            annotation_id,
        );
    }
    if let Some(wire_id) = &node.attributes.wire_id {
        push_string_triple(triples, &node_uri, &format!("{MDOC_NS}wireId"), wire_id);
    }
    if let Some(src) = &node.attributes.src {
        let predicate = match tag_name {
            "image" => "imageSrc",
            "mathInline" | "mathBlock" => "mathSource",
            _ => "src",
        };
        push_string_triple(triples, &node_uri, &format!("{MDOC_NS}{predicate}"), src);
    }
    if let Some(alt) = &node.attributes.alt {
        push_string_triple(triples, &node_uri, &format!("{MDOC_NS}altText"), alt);
    }
    for (key, value) in &node.attributes.extra {
        push_string_triple(triples, &node_uri, &format!("{MDOC_NS}{key}"), value);
    }

    if tag_name != "wikilink" {
        for (index, child) in node.children.iter().enumerate() {
            let child_uri = serialize_tree_node(document, child, counter, triples);
            push_uri_triple(
                triples,
                &node_uri,
                &format!("{MDOC_NS}childNode"),
                &child_uri,
            );
            push_integer_triple(
                triples,
                &child_uri,
                &format!("{MDOC_NS}siblingOrder"),
                index as i64,
            );
        }
    }

    node_uri
}

fn document_fragment_uri(document: &DocumentRecord) -> String {
    format!("{}#frag", document.rdf_subject)
}

fn document_block_uri(document: &DocumentRecord, block_id: &str) -> String {
    format!("{}#block-{block_id}", document.rdf_subject)
}

fn anonymous_node_uri(document: &DocumentRecord, counter: &mut usize) -> String {
    *counter += 1;
    format!("{}#node-{}", document.rdf_subject, *counter)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document_service::{DocumentTreeSnapshot, TreeNodeAttributes, TreeNodeSnapshot};
    use crate::rdf::{document_subject, format_rdf_triple};
    use crate::runtime_config::{DOCUMENT_SCHEMA_VERSION, LOCAL_GRAPH_ORIGIN, LOCAL_PROVIDER_ID};
    use std::collections::BTreeSet;

    /// A representative DocumentRecord whose tree exercises many node kinds:
    /// a heading (level), a block paragraph (nodeId + textContent), a nested
    /// text node, and a link (href/target) — so `document_tree_triples` emits a
    /// broad predicate spread.
    fn rich_record(paragraph_text: &str) -> DocumentRecord {
        let doc_id = "doc-drift";
        DocumentRecord {
            document_id: doc_id.to_string(),
            graph_id: "graph-drift".to_string(),
            title: "Drift".to_string(),
            revision: 1,
            body: String::new(),
            origin: LOCAL_GRAPH_ORIGIN.to_string(),
            provider_id: LOCAL_PROVIDER_ID.to_string(),
            local_path: format!("/tmp/{doc_id}"),
            rdf_subject: document_subject(doc_id),
            created_at: "1".to_string(),
            updated_at: "2".to_string(),
            capabilities: Vec::new(),
            schema_version: DOCUMENT_SCHEMA_VERSION,
            tiptap_xml: String::new(),
            tiptap_json: None,
            ydoc_update_base64: String::new(),
            ydoc_state_path: String::new(),
            tree: Some(DocumentTreeSnapshot {
                doc_id: doc_id.to_string(),
                root: TreeNodeSnapshot {
                    kind: "element".to_string(),
                    tag_name: Some("doc".to_string()),
                    text_content: None,
                    attributes: TreeNodeAttributes::default(),
                    children: vec![
                        TreeNodeSnapshot {
                            kind: "element".to_string(),
                            tag_name: Some("heading".to_string()),
                            text_content: None,
                            attributes: TreeNodeAttributes {
                                block_id: Some("block-h".to_string()),
                                level: Some(2),
                                ..TreeNodeAttributes::default()
                            },
                            children: vec![TreeNodeSnapshot {
                                kind: "text".to_string(),
                                tag_name: None,
                                text_content: Some("A heading".to_string()),
                                attributes: TreeNodeAttributes::default(),
                                children: Vec::new(),
                            }],
                        },
                        TreeNodeSnapshot {
                            kind: "element".to_string(),
                            tag_name: Some("paragraph".to_string()),
                            text_content: None,
                            attributes: TreeNodeAttributes {
                                block_id: Some("block-p".to_string()),
                                ..TreeNodeAttributes::default()
                            },
                            children: vec![
                                TreeNodeSnapshot {
                                    kind: "text".to_string(),
                                    tag_name: None,
                                    text_content: Some(paragraph_text.to_string()),
                                    attributes: TreeNodeAttributes::default(),
                                    children: Vec::new(),
                                },
                                TreeNodeSnapshot {
                                    kind: "element".to_string(),
                                    tag_name: Some("link".to_string()),
                                    text_content: None,
                                    attributes: TreeNodeAttributes {
                                        href: Some("https://example.com".to_string()),
                                        target: Some("_blank".to_string()),
                                        ..TreeNodeAttributes::default()
                                    },
                                    children: vec![TreeNodeSnapshot {
                                        kind: "text".to_string(),
                                        tag_name: None,
                                        text_content: Some("a link".to_string()),
                                        attributes: TreeNodeAttributes::default(),
                                        children: Vec::new(),
                                    }],
                                },
                            ],
                        },
                    ],
                },
            }),
            blocks: Vec::new(),
            rdf_triple_count: 0,
            document_kind: None,
        }
    }

    /// Extract the leading `<iri>` token from a serialized triple line.
    fn leading_iri(s: &str) -> (String, &str) {
        let s = s.trim_start();
        let s = s.strip_prefix('<').expect("triple token starts with <");
        let close = s.find('>').expect("triple token has closing >");
        (s[..close].to_string(), &s[close + 1..])
    }

    /// The set of predicates `document_tree_triples` emits ON the bare doc
    /// `<subject>` (i.e. with subject exactly `subject`, no `#`-fragment),
    /// expressed as MNEMO_NS-local names.
    fn emitted_bare_subject_predicate_locals(document: &DocumentRecord) -> BTreeSet<String> {
        let subject = &document.rdf_subject;
        let mut locals = BTreeSet::new();
        for triple in document_tree_triples(document) {
            let line = format_rdf_triple(&triple);
            let (s, rest) = leading_iri(&line);
            if &s != subject {
                continue; // a `{subject}#…` fragment subject, not the bare one
            }
            let (p, _) = leading_iri(rest);
            let local = p.strip_prefix(MNEMO_NS).unwrap_or(&p).to_string();
            locals.insert(local);
        }
        locals
    }

    /// DRIFT-PROOF: the projection provably conforms to its DECLARED bare-subject
    /// vocabulary. Whatever `document_tree_triples` emits on the bare `<subject>`
    /// must be a SUBSET of `DOCUMENT_LEVEL_PREDICATES` — so the declared set (used
    /// by both the old materializer's DELETE allowlist and the MO's owned-span)
    /// can never silently drift from what the projection actually emits. Checked
    /// across representative docs: populated/rich, edited-text, and empty (None).
    /// PARTITION INVARIANT: the FS-sourced (7) / content (2) / projection-meta (1)
    /// constants split `DOCUMENT_LEVEL_PREDICATES` (10) DISJOINTLY and COMPLETELY.
    /// This is what lets the storage-metadata Face own exactly the 7 FS-sourced
    /// bare predicates without double-claiming any the tree/content/meta paths own.
    #[test]
    fn partition_is_disjoint_cover_of_document_level_predicates() {
        let union: BTreeSet<&str> = DOCUMENT_LEVEL_PREDICATES.iter().copied().collect();
        let fs: BTreeSet<&str> = FS_SOURCED_PREDICATES.iter().copied().collect();
        let content: BTreeSet<&str> = CONTENT_PREDICATES.iter().copied().collect();
        let meta: BTreeSet<&str> = PROJECTION_META_PREDICATES.iter().copied().collect();

        // No duplicates within any partition.
        assert_eq!(fs.len(), FS_SOURCED_PREDICATES.len());
        assert_eq!(content.len(), CONTENT_PREDICATES.len());
        assert_eq!(meta.len(), PROJECTION_META_PREDICATES.len());

        // Pairwise disjoint.
        assert!(fs.is_disjoint(&content));
        assert!(fs.is_disjoint(&meta));
        assert!(content.is_disjoint(&meta));

        // Their union is exactly the 10-union.
        let mut cover: BTreeSet<&str> = BTreeSet::new();
        cover.extend(&fs);
        cover.extend(&content);
        cover.extend(&meta);
        assert_eq!(
            cover, union,
            "the three partitions must cover DOCUMENT_LEVEL_PREDICATES exactly"
        );
    }

    #[test]
    fn document_tree_triples_emit_only_declared_bare_predicates() {
        let declared: BTreeSet<String> = DOCUMENT_LEVEL_PREDICATES
            .iter()
            .map(|p| p.to_string())
            .collect();

        for record in [
            rich_record("first version of the paragraph"),
            rich_record("a SECOND, edited version of the paragraph"),
            {
                let mut empty = rich_record("ignored");
                empty.tree = None;
                empty
            },
        ] {
            let emitted = emitted_bare_subject_predicate_locals(&record);
            assert!(
                emitted.is_subset(&declared),
                "document_tree_triples emitted bare-subject predicates {emitted:?} that are \
                 NOT in the declared DOCUMENT_LEVEL_PREDICATES {declared:?} — the projection \
                 has drifted from its declared bare-subject vocabulary"
            );
        }
    }
}
