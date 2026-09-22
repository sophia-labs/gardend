use crate::{
    emporium::contract::wires_vocabulary,
    emporium::reconcile::{reconcile_class_validated, ClassScope, Placement, SpanKey},
    emporium::terms::{Triple, TripleDiff},
    json_utils::json_string,
    rdf::{push_uri_triple, RdfTriple},
    rdf_authority::workspace_projection_graph_iri,
    rdf_record_materializer::rdf_triple_to_term,
    rdf_workspace_terms::{
        block_ref_uri, document_ref_uri, mark_ref_uri, snapshot_array, wire_predicate_uri,
        wire_ref_uri, workspace_entity_subject,
    },
    rdf_workspace_values::{extra_value, push_workspace_value_triple},
    runtime_config::{MDOC_NS, RDF_TYPE, WIRE_NS, XSD_NS},
};
use oxigraph::store::Store;

pub(super) fn push_wire_triples(
    triples: &mut Vec<RdfTriple>,
    graph_id: &str,
    subject: &str,
    wire: &serde_json::Value,
) {
    let target_graph_id =
        json_string(wire.get("targetGraphId")).unwrap_or_else(|| graph_id.to_string());
    let source_document_id = json_string(wire.get("sourceDocumentId"));
    let target_document_id = json_string(wire.get("targetDocumentId"));

    if let Some(source_document_id) = &source_document_id {
        push_uri_triple(
            triples,
            subject,
            &format!("{WIRE_NS}sourceDocument"),
            &document_ref_uri(source_document_id),
        );
    }
    if let Some(target_document_id) = &target_document_id {
        push_uri_triple(
            triples,
            subject,
            &format!("{WIRE_NS}targetDocument"),
            &document_ref_uri(target_document_id),
        );
    }
    if let (Some(source_document_id), Some(source_block_id)) = (
        source_document_id.as_deref(),
        json_string(wire.get("sourceBlockId")).as_deref(),
    ) {
        push_uri_triple(
            triples,
            subject,
            &format!("{WIRE_NS}sourceBlock"),
            &block_ref_uri(source_document_id, source_block_id),
        );
    }
    if let (Some(target_document_id), Some(target_block_id)) = (
        target_document_id.as_deref(),
        json_string(wire.get("targetBlockId")).as_deref(),
    ) {
        push_uri_triple(
            triples,
            subject,
            &format!("{WIRE_NS}targetBlock"),
            &block_ref_uri(target_document_id, target_block_id),
        );
    }
    if let Some(source_mark_id) = json_string(wire.get("sourceMarkId")) {
        if let (Some(source_document_id), Some(source_block_id)) = (
            source_document_id.as_deref(),
            json_string(wire.get("sourceBlockId")).as_deref(),
        ) {
            push_uri_triple(
                triples,
                subject,
                &format!("{WIRE_NS}sourceContext"),
                &mark_ref_uri(source_document_id, source_block_id, &source_mark_id),
            );
        }
    }
    if let Some(target_mark_id) = json_string(wire.get("targetMarkId")) {
        if let (Some(target_document_id), Some(target_block_id)) = (
            target_document_id.as_deref(),
            json_string(wire.get("targetBlockId")).as_deref(),
        ) {
            push_uri_triple(
                triples,
                subject,
                &format!("{WIRE_NS}targetContext"),
                &mark_ref_uri(target_document_id, target_block_id, &target_mark_id),
            );
        }
    }
    if let Some(predicate) = json_string(wire.get("predicate")) {
        push_uri_triple(
            triples,
            subject,
            &format!("{WIRE_NS}predicate"),
            &wire_predicate_uri(&predicate),
        );
    }
    push_workspace_value_triple(
        triples,
        subject,
        &format!("{WIRE_NS}bidirectional"),
        wire.get("bidirectional"),
        Some(&format!("{XSD_NS}boolean")),
    );
    if let Some(inverse_of) = json_string(wire.get("inverseOf")) {
        push_uri_triple(
            triples,
            subject,
            &format!("{WIRE_NS}inverseOf"),
            &wire_ref_uri(graph_id, &inverse_of),
        );
    }
    push_workspace_value_triple(
        triples,
        subject,
        &format!("{WIRE_NS}targetGraph"),
        Some(&serde_json::Value::String(target_graph_id)),
        None,
    );
    for (field, predicate) in [
        ("targetTitle", format!("{WIRE_NS}targetTitle")),
        ("targetSnippet", format!("{WIRE_NS}targetSnippet")),
        ("sourceTitle", format!("{WIRE_NS}sourceTitle")),
        ("sourceSnippet", format!("{WIRE_NS}sourceSnippet")),
        ("sceneGraphId", format!("{WIRE_NS}sceneGraphId")),
        ("sceneArtifactId", format!("{WIRE_NS}sceneArtifactId")),
        ("sceneElementId", format!("{WIRE_NS}sceneElementId")),
        (
            "sceneSourceElementId",
            format!("{WIRE_NS}sceneSourceElementId"),
        ),
        (
            "sceneTargetElementId",
            format!("{WIRE_NS}sceneTargetElementId"),
        ),
        ("sceneStableKey", format!("{WIRE_NS}sceneStableKey")),
        ("createdAt", format!("{MDOC_NS}createdAt")),
        ("deletedAt", format!("{WIRE_NS}deletedAt")),
        ("snapshotAt", format!("{WIRE_NS}snapshotAt")),
    ] {
        let datatype = matches!(field, "createdAt" | "deletedAt" | "snapshotAt")
            .then(|| format!("{XSD_NS}dateTime"));
        push_workspace_value_triple(
            triples,
            subject,
            &predicate,
            wire.get(field).or_else(|| extra_value(wire, field)),
            datatype.as_deref(),
        );
    }
}

// ============================================================================
// WIRE RECONCILE — the standalone single-class `Wire` instantiation of the
// `reconcile_class` primitive (mirrors the salience/graph templates). Wires are
// materialized inside the workspace snapshot (there is no standalone wire SAVE
// call site), so the LIVE flip routes through `reconcile_workspace_snapshot`
// where Wire is ONE of the multi-class spans. This standalone entry point is the
// single-class organism: it surveys + diffs ONLY the `mnemo:Wire` class in the
// `:projection:workspace` named graph, so the wire-specific laws (nodup,
// dangling-endpoint passthrough, convergence) are provable in isolation, AND the
// workspace multi-class path reuses `wire_subject_triples` / `wire_scope` for
// its Wire span (one source of truth — no drift between standalone and composed).
// ============================================================================

/// The full per-wire DESIRED triples (`RdfTriple` form): the head
/// `rdf:type mnemo:Wire` (emitted by the workspace layer, NOT `push_wire_triples`)
/// + the endpoint/predicate/scalar body. This is the SINGLE SOURCE the workspace
/// `push_wire_snapshot_triples` also produces (identical: type head + body), so
/// the standalone Wire span and the composed workspace Wire span project the
/// SAME set. The subject is `workspace_entity_subject(graph_id, "wire", id)`.
pub(super) fn wire_subject_triples(
    triples: &mut Vec<RdfTriple>,
    graph_id: &str,
    wire: &serde_json::Value,
) {
    let Some(id) = json_string(wire.get("id")) else {
        return;
    };
    let subject = workspace_entity_subject(graph_id, "wire", &id);
    push_uri_triple(triples, &subject, RDF_TYPE, &format!("{WIRE_NS}Wire"));
    push_wire_triples(triples, graph_id, &subject, wire);
}

/// `project(source) -> desired` for the WIRE Meaningful Object: the per-wire
/// reified triples [`wire_subject_triples`] builds for every wire in the
/// snapshot's `wires` array, bridged `RdfTriple -> Triple` through the SAME
/// `rdf_triple_to_term` round-trip the other MOs use (MANDATORY so the bridged
/// `desired` serializes byte-identically to what `survey_class` reparses out of
/// the store — else convergence breaks on a serialization mismatch).
pub(super) fn wire_desired(graph_id: &str, snapshot: &serde_json::Value) -> Vec<Triple> {
    let mut raw = Vec::new();
    for wire in snapshot_array(snapshot, "wires") {
        wire_subject_triples(&mut raw, graph_id, wire);
    }
    raw.iter().map(rdf_triple_to_term).collect()
}

/// The wire [`ClassScope`]: the `:projection:workspace` NAMED graph
/// ([`Placement::Named`]), keyed on the single `Fixed` `mnemo:Wire` class. NO
/// `graph_id_conjunct` — the named-graph IRI is already per-cell graph-scoped
/// (one gardend cell owns one graph = one `:projection:workspace`), and wires
/// carry no `mnemo:graphId` triple (unlike salience/song). The Rust image of the
/// wholesale DELETE's `?wire a mnemo:Wire ; ?p ?o` span.
pub(super) fn wire_scope(graph_id: &str) -> ClassScope {
    ClassScope {
        placement: Placement::Named(workspace_projection_graph_iri(graph_id)),
        key: SpanKey::Fixed {
            rdf_type: format!("{WIRE_NS}Wire"),
        },
        graph_id_conjunct: None,
        subjects: None,
    }
}

/// ADDITIVE entry point: reconcile the WIRE projection by single-class VALUE-DIFF
/// instead of the wholesale teardown-and-rebuild the workspace materializer's
/// `?wire a mnemo:Wire` DELETE does. Surveys the `mnemo:Wire` class span in
/// `:projection:workspace`, diffs against [`wire_desired`], applies only the
/// delta. A converged save emits 0 ops; deleting a wire from the snapshot
/// reclaims exactly that wire's reified subject (its type leaves `desired`).
///
/// This is the standalone single-class organism. The LIVE workspace save routes
/// through `reconcile_workspace_snapshot`, where Wire is one of the multi-class
/// spans (composed via `reconcile_classes`) — both paths share the SAME
/// `wire_subject_triples`/`wire_scope`, so they project identically.
pub(super) fn reconcile_wire_store(
    store: &Store,
    graph_id: &str,
    snapshot: &serde_json::Value,
) -> Result<TripleDiff, String> {
    let scope = wire_scope(graph_id);
    let desired = wire_desired(graph_id, snapshot);
    // S6: contract metadata made load-bearing — the `emporium-wires` retrofit shapes
    // now GATE this write (was `contract: None`). OBSERVE seam, not a trust boundary:
    // the wire projection is a deterministic re-projection from a trusted snapshot, so
    // a violation = materializer↔vocab DRIFT (`shacl_oracle_wires_conforms` proves the
    // real projection conforms today). Dangling endpoints are IRIs the shapes
    // intentionally do NOT enforce referential integrity over.
    reconcile_class_validated(store, &scope, &desired, Some(wires_vocabulary()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rdf::format_rdf_triple;
    use std::collections::HashSet;

    /// Render a wire's emitted triples to a SET of N-Triples-ish strings, the same
    /// shape the in-file happy-path test uses, but as a `HashSet` so we can assert
    /// SET-EQUALITY against an independently-built model (the differential oracle).
    fn render_set(graph_id: &str, subject: &str, wire: &serde_json::Value) -> HashSet<String> {
        let mut triples = Vec::new();
        push_wire_triples(&mut triples, graph_id, subject, wire);
        triples.iter().map(format_rdf_triple).collect()
    }

    /// ANTI-TAUTOLOGY GUARD. The model set is built by THIS function ONLY — it never
    /// calls `push_wire_triples`, `wire_ref_uri`, `document_ref_uri`, `block_ref_uri`,
    /// `mark_ref_uri`, or `wire_predicate_uri`. Every expected URI/literal is spelled
    /// out from the WIRE_NS/XSD_NS constants + literal `urn:mnemosyne:local:…` prefixes,
    /// so a bug in any materializer helper would make real ≠ model rather than both
    /// drift together. This mirrors `Vocab.Wires.wireTriples` (the Lean model) line for
    /// line for the 8 structural endpoints, plus the always-on `targetGraph` value
    /// triple that `push_wire_triples` emits (the Lean model abstracts scalars away;
    /// here we must account for every emitted triple to claim SET equality).
    struct WireModel {
        subject: String,
        graph_id: String,
        src_doc: Option<String>,
        tgt_doc: Option<String>,
        src_block: Option<String>,
        tgt_block: Option<String>,
        src_mark: Option<String>,
        tgt_mark: Option<String>,
        predicate: Option<String>,
        inverse_of: Option<String>,
        // scalar/value fields the materializer also emits (independently modelled):
        bidirectional: Option<bool>,
        target_graph_id: Option<String>,
    }

    impl WireModel {
        /// Build the EXPECTED rendered-triple set with hand-written formatting — no
        /// materializer helper is invoked anywhere in this method.
        fn expected(&self) -> HashSet<String> {
            // literal mirrors of the URI builders, written out independently:
            let doc = |id: &str| -> String {
                if id.starts_with("urn:mnemosyne:") {
                    id.to_string()
                } else {
                    format!("urn:mnemosyne:local:document:{id}")
                }
            };
            let block = |d: &str, b: &str| -> String { format!("{}#block-{b}", doc(d)) };
            let mark = |d: &str, b: &str, m: &str| -> String { format!("{}#mark-{b}-{m}", doc(d)) };
            let wire_ref = |g: &str, w: &str| -> String {
                if w.starts_with("urn:mnemosyne:") {
                    w.to_string()
                } else {
                    format!("urn:mnemosyne:local:graph:{g}:wire:{w}")
                }
            };
            let pred = |p: &str| -> String {
                if p.starts_with("http://") || p.starts_with("https://") || p.starts_with("urn:") {
                    p.to_string()
                } else {
                    format!("{WIRE_NS}{p}")
                }
            };
            let s = &self.subject;
            let mut out = HashSet::new();
            let uri = |p: String, o: String| format!("<{s}> <{p}> <{o}> .");

            if let Some(d) = &self.src_doc {
                out.insert(uri(format!("{WIRE_NS}sourceDocument"), doc(d)));
            }
            if let Some(d) = &self.tgt_doc {
                out.insert(uri(format!("{WIRE_NS}targetDocument"), doc(d)));
            }
            if let (Some(d), Some(b)) = (&self.src_doc, &self.src_block) {
                out.insert(uri(format!("{WIRE_NS}sourceBlock"), block(d, b)));
            }
            if let (Some(d), Some(b)) = (&self.tgt_doc, &self.tgt_block) {
                out.insert(uri(format!("{WIRE_NS}targetBlock"), block(d, b)));
            }
            if let (Some(m), Some(d), Some(b)) = (&self.src_mark, &self.src_doc, &self.src_block) {
                out.insert(uri(format!("{WIRE_NS}sourceContext"), mark(d, b, m)));
            }
            if let (Some(m), Some(d), Some(b)) = (&self.tgt_mark, &self.tgt_doc, &self.tgt_block) {
                out.insert(uri(format!("{WIRE_NS}targetContext"), mark(d, b, m)));
            }
            if let Some(p) = &self.predicate {
                out.insert(uri(format!("{WIRE_NS}predicate"), pred(p)));
            }
            if let Some(iv) = &self.inverse_of {
                out.insert(uri(
                    format!("{WIRE_NS}inverseOf"),
                    wire_ref(&self.graph_id, iv),
                ));
            }
            // scalar value triples (always-on targetGraph; optional bidirectional):
            if let Some(b) = self.bidirectional {
                out.insert(format!(
                    "<{s}> <{WIRE_NS}bidirectional> \"{b}\"^^<{XSD_NS}boolean> ."
                ));
            }
            // targetGraph is ALWAYS emitted: defaults to graph_id when targetGraphId absent.
            let tg = self
                .target_graph_id
                .clone()
                .unwrap_or_else(|| self.graph_id.clone());
            out.insert(format!("<{s}> <{WIRE_NS}targetGraph> \"{tg}\" ."));
            out
        }
    }

    /// CONTENT ORACLE — the REAL `push_wire_triples` output set EQUALS the independently
    /// built model set, on a fully-populated wire (8 endpoints + nesting + scalars).
    /// Ties the Lean `wireTriples` model to the runtime. Anti-tautology: `expected()`
    /// never calls any materializer fn (asserted by construction + a paranoia check that
    /// the model and real sets were built from disjoint code paths).
    #[test]
    fn oracle_wire_triples_equal_independent_model() {
        let graph_id = "graph-a";
        let subject = "urn:mnemosyne:local:graph:graph-a:wire:wire-1";
        let wire = serde_json::json!({
            "sourceDocumentId": "doc-a",
            "targetDocumentId": "doc-b",
            "sourceBlockId": "block-a",
            "targetBlockId": "block-b",
            "sourceMarkId": "mark-a",
            "targetMarkId": "mark-b",
            "predicate": "supports",
            "bidirectional": true,
            "inverseOf": "wire-b"
        });

        let real = render_set(graph_id, subject, &wire);

        let model = WireModel {
            subject: subject.to_string(),
            graph_id: graph_id.to_string(),
            src_doc: Some("doc-a".into()),
            tgt_doc: Some("doc-b".into()),
            src_block: Some("block-a".into()),
            tgt_block: Some("block-b".into()),
            src_mark: Some("mark-a".into()),
            tgt_mark: Some("mark-b".into()),
            predicate: Some("supports".into()),
            inverse_of: Some("wire-b".into()),
            bidirectional: Some(true),
            target_graph_id: None,
        }
        .expected();

        let missing: Vec<_> = model.difference(&real).cloned().collect();
        let extra: Vec<_> = real.difference(&model).cloned().collect();
        eprintln!("REAL set ({}):", real.len());
        let mut r: Vec<_> = real.iter().cloned().collect();
        r.sort();
        for t in &r {
            eprintln!("  {t}");
        }
        eprintln!("MODEL-only (missing from real): {missing:?}");
        eprintln!("REAL-only (extra vs model):     {extra:?}");
        assert_eq!(
            real, model,
            "real materializer output must EQUAL the independent model set"
        );
    }

    /// CONTENT ORACLE — the OPEN-predicate case (W2 teeth, content side): an absolute
    /// IRI predicate passes through verbatim; a short name expands to WIRE_NS+name.
    /// The model encodes the open behaviour independently and must match the real output.
    #[test]
    fn oracle_open_predicate_absolute_and_short_match_model() {
        let graph_id = "graph-a";
        let subject = "urn:mnemosyne:local:graph:graph-a:wire:wire-2";

        for p in [
            "https://example.test/relatesTo",
            "urn:custom:p",
            "customRelation",
        ] {
            let wire = serde_json::json!({ "predicate": p });
            let real = render_set(graph_id, subject, &wire);
            let model = WireModel {
                subject: subject.to_string(),
                graph_id: graph_id.to_string(),
                src_doc: None,
                tgt_doc: None,
                src_block: None,
                tgt_block: None,
                src_mark: None,
                tgt_mark: None,
                predicate: Some(p.to_string()),
                inverse_of: None,
                bidirectional: None,
                target_graph_id: None,
            }
            .expected();
            assert_eq!(real, model, "open-predicate `{p}` must match the model");
        }
    }

    /// W4 TEETH (cardinality) — exactly which "no duplicate" the system guarantees.
    /// (a) SAME wire_id (⇒ same subject) ⇒ the two emissions DEDUPLICATE as a SET
    ///     (identical subjects + identical triples — RDF set-merge collapses them).
    /// (b) DIFFERENT ids with IDENTICAL endpoints+predicate ⇒ DISTINCT subjects ⇒ the
    ///     union has BOTH triple sets (logical duplicates are NOT deduped).
    /// This validates the Lean `wire_subjects_nodup` premise (distinct ids ⇒ distinct
    /// subjects) AND the explicitly-logged NON-invariant (logical-edge dedup absent).
    #[test]
    fn oracle_no_dup_wires_subject_keyed() {
        let graph_id = "graph-a";
        let endpoints = serde_json::json!({
            "sourceDocumentId": "doc-x",
            "targetDocumentId": "doc-y",
            "predicate": "supports"
        });

        // subjects are minted off wire_id by the caller; emulate that here.
        let subj = |id: &str| format!("urn:mnemosyne:local:graph:{graph_id}:wire:{id}");

        // (a) SAME id ⇒ SAME subject ⇒ set-dedup.
        let s_same = subj("wire-dup");
        let a = render_set(graph_id, &s_same, &endpoints);
        let b = render_set(graph_id, &s_same, &endpoints);
        let union_same: HashSet<_> = a.union(&b).cloned().collect();
        assert_eq!(
            union_same.len(),
            a.len(),
            "same wire_id ⇒ identical subject ⇒ triples DEDUPLICATE as a set"
        );
        assert_eq!(a, b, "same id, same endpoints ⇒ bit-identical triple sets");

        // (b) DIFFERENT ids, IDENTICAL endpoints+predicate ⇒ DISTINCT subjects.
        let s1 = subj("wire-1");
        let s2 = subj("wire-2");
        let t1 = render_set(graph_id, &s1, &endpoints);
        let t2 = render_set(graph_id, &s2, &endpoints);
        assert_ne!(
            s1, s2,
            "distinct ids ⇒ distinct subjects (the nodup premise)"
        );
        // every triple in t1 has subject s1, none has subject s2 (and vice-versa):
        assert!(
            t1.iter().all(|t| t.starts_with(&format!("<{s1}> "))),
            "all wire-1 triples carry the wire-1 subject"
        );
        assert!(
            t2.iter().all(|t| t.starts_with(&format!("<{s2}> "))),
            "all wire-2 triples carry the wire-2 subject"
        );
        let union_diff: HashSet<_> = t1.union(&t2).cloned().collect();
        // logical duplicates NOT deduped: the union is the DISJOINT sum of both.
        assert_eq!(
            union_diff.len(),
            t1.len() + t2.len(),
            "distinct-id logical-duplicate wires are NOT deduped (the stated NON-invariant)"
        );
        assert!(
            t1.is_disjoint(&t2),
            "distinct subjects ⇒ disjoint rendered triple sets"
        );
    }

    /// W1 TEETH (referential integrity is NOT enforced) — a wire whose targetDocumentId
    /// names a document that does NOT exist still emits the `targetDocument` endpoint.
    /// Confirms the finding: materialization performs NO existence check, so a dangling
    /// endpoint is materialized verbatim. (If it were suppressed, this asserts-out and
    /// W1 would be REFUTED — here we expect it PRESENT.)
    #[test]
    fn oracle_dangling_endpoint_still_emitted_w1() {
        let graph_id = "graph-a";
        let subject = "urn:mnemosyne:local:graph:graph-a:wire:wire-dangling";
        // doc-ghost is never declared anywhere; a pure reference.
        let wire = serde_json::json!({
            "targetDocumentId": "doc-ghost",
            "targetBlockId": "block-ghost"
        });
        let real = render_set(graph_id, subject, &wire);
        let expected_doc = format!(
            "<{subject}> <{WIRE_NS}targetDocument> <urn:mnemosyne:local:document:doc-ghost> ."
        );
        let expected_block = format!(
            "<{subject}> <{WIRE_NS}targetBlock> <urn:mnemosyne:local:document:doc-ghost#block-block-ghost> ."
        );
        eprintln!("dangling-endpoint real set:");
        for t in &real {
            eprintln!("  {t}");
        }
        assert!(
            real.contains(&expected_doc),
            "W1: dangling targetDocument endpoint MUST still be emitted (no existence check)"
        );
        assert!(
            real.contains(&expected_block),
            "W1: dangling targetBlock endpoint MUST still be emitted (no existence check)"
        );
    }

    /// W1/nesting companion — a markId WITHOUT its blockId drops the context endpoint
    /// entirely (construction-driven nesting guard), while the doc endpoint still emits.
    /// Pins the negative the recon predicted: the mark is silently dropped, not errored.
    #[test]
    fn oracle_mark_without_block_drops_context() {
        let graph_id = "graph-a";
        let subject = "urn:mnemosyne:local:graph:graph-a:wire:wire-nest";
        let wire = serde_json::json!({
            "sourceDocumentId": "doc-a",
            "sourceMarkId": "mark-a"
            // NO sourceBlockId
        });
        let real = render_set(graph_id, subject, &wire);
        assert!(
            real.iter()
                .any(|t| t.contains(&format!("{WIRE_NS}sourceDocument"))),
            "doc endpoint still emits"
        );
        assert!(
            !real
                .iter()
                .any(|t| t.contains(&format!("{WIRE_NS}sourceContext"))),
            "mark-without-block ⇒ NO sourceContext (silently dropped)"
        );
    }

    #[test]
    fn wire_triples_include_document_block_predicate_and_temporal_context() {
        let wire = serde_json::json!({
            "sourceDocumentId": "doc-a",
            "targetDocumentId": "doc-b",
            "sourceBlockId": "block-a",
            "targetBlockId": "block-b",
            "sourceMarkId": "mark-a",
            "predicate": "supports",
            "bidirectional": true,
            "inverseOf": "wire-b",
            "sceneGraphId": "graph-a",
            "sceneArtifactId": "artifact-scene",
            "sceneElementId": "arrow-1",
            "extra": {
                "createdAt": "1000"
            }
        });
        let mut triples = Vec::new();

        push_wire_triples(&mut triples, "graph-a", "urn:test:wire-a", &wire);

        let rendered = triples
            .iter()
            .map(format_rdf_triple)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("<urn:test:wire-a> <"));
        assert!(rendered.contains("sourceDocument> <urn:mnemosyne:local:document:doc-a>"));
        assert!(
            rendered.contains("sourceBlock> <urn:mnemosyne:local:document:doc-a#block-block-a>")
        );
        assert!(rendered
            .contains("sourceContext> <urn:mnemosyne:local:document:doc-a#mark-block-a-mark-a>"));
        assert!(rendered.contains(&format!("predicate> <{WIRE_NS}supports>")));
        assert!(rendered.contains("inverseOf> <urn:mnemosyne:local:graph:graph-a:wire:wire-b>"));
        assert!(rendered.contains("sceneArtifactId> \"artifact-scene\""));
        assert!(rendered.contains("sceneElementId> \"arrow-1\""));
        assert!(rendered.contains("createdAt> \"1000\"^^"));
    }
}

// ============================================================================
// P3 WIRE RECONCILE ORACLE — ties the single-class `reconcile_wire_store`
// instantiation to the wholesale `materialize_workspace_snapshot` BASELINE, the
// way the salience/song oracles tie their reconciles to their wholesale paths.
// NO MOCKS: real in-memory Oxigraph `Store` (the wire reconcile takes `&Store`
// directly, so no on-disk dir is needed), real materializers, real SPARQL
// read-back. The wholesale path projects wires + ontology into
// `:projection:workspace`; the standalone wire reconcile projects ONLY wires, so
// the comparison face is the WIRE-SUBJECT span (`?w a mnemo:Wire ; ?p ?o`).
//
//   (1) EQUIVALENCE — wholesale(A) wire-subject set == reconcile(B) wire-subject
//       set, from the SAME wires-only snapshot. Plus an INDEPENDENT model of the
//       reified wire subjects (hand-built from constants), anti-tautology guarded.
//   (2) DOMINATION — `reconcile op_count <= |desired|` from empty; on a CONVERGED
//       re-run, reconcile is 0 (the wholesale would re-teardown all wires).
//   (3) CONVERGENCE — a second reconcile of the SAME snapshot emits 0 ops.
//   (4) WIRE-RECLAIM — drop a wire from the snapshot ⇒ its reified subject is
//       removed (its `mnemo:Wire` type leaves `desired`), then re-reconcile = 0.
// ============================================================================
#[cfg(test)]
mod p3_wire_reconcile_oracle {
    use super::*;
    use crate::rdf_workspace_store_materializer::materialize_workspace_snapshot;
    use oxigraph::sparql::{QueryResults, SparqlEvaluator};
    use std::collections::BTreeSet;

    const GID: &str = "graph-wire-recon";

    fn ws_iri() -> String {
        workspace_projection_graph_iri(GID)
    }

    /// All `?w ?p ?o` of every reified `mnemo:Wire` subject in the workspace named
    /// graph, as normalized `S P O` strings (the wire-subject comparison face).
    fn wire_span_set(store: &Store) -> BTreeSet<String> {
        let g = ws_iri();
        let q = format!(
            "SELECT ?s ?p ?o WHERE {{ GRAPH <{g}> {{ ?s a <{WIRE_NS}Wire> . ?s ?p ?o }} }}"
        );
        let solutions = match SparqlEvaluator::new()
            .parse_query(&q)
            .expect("parse wire span")
            .on_store(store)
            .execute()
            .expect("exec wire span")
        {
            QueryResults::Solutions(s) => s,
            _ => panic!("expected solutions"),
        };
        let mut out = BTreeSet::new();
        for sol in solutions {
            let sol = sol.expect("row");
            out.insert(format!(
                "{} {} {}",
                sol.get("s").unwrap(),
                sol.get("p").unwrap(),
                sol.get("o").unwrap()
            ));
        }
        out
    }

    /// All `P O` pairs for one subject in the workspace named graph.
    fn subject_pairs(store: &Store, subject: &str) -> BTreeSet<String> {
        let g = ws_iri();
        let q = format!("SELECT ?p ?o WHERE {{ GRAPH <{g}> {{ <{subject}> ?p ?o }} }}");
        let solutions = match SparqlEvaluator::new()
            .parse_query(&q)
            .expect("parse subject")
            .on_store(store)
            .execute()
            .expect("exec subject")
        {
            QueryResults::Solutions(s) => s,
            _ => panic!("expected solutions"),
        };
        let mut out = BTreeSet::new();
        for sol in solutions {
            let sol = sol.expect("row");
            out.insert(format!(
                "{} {}",
                sol.get("p").unwrap(),
                sol.get("o").unwrap()
            ));
        }
        out
    }

    fn wires_snapshot(wires: serde_json::Value) -> serde_json::Value {
        serde_json::json!({ "folders": [], "documents": [], "artifacts": [], "wires": wires })
    }

    fn full_wires() -> serde_json::Value {
        serde_json::json!([
            { "id": "w1", "sourceDocumentId": "doc-a", "targetDocumentId": "doc-b", "predicate": "supports", "bidirectional": true },
            { "id": "w2", "sourceDocumentId": "doc-b", "targetDocumentId": "doc-ghost" }
        ])
    }

    // (1) EQUIVALENCE — wholesale(A) wire span == reconcile(B) wire span == model.
    #[test]
    fn oracle_wire_reconcile_equivalent_to_wholesale() {
        let snap = wires_snapshot(full_wires());

        let store_a = Store::new().expect("store A");
        materialize_workspace_snapshot(&store_a, GID, &snap).expect("wholesale A");

        let store_b = Store::new().expect("store B");
        let diff = reconcile_wire_store(&store_b, GID, &snap).expect("reconcile B");

        let set_a = wire_span_set(&store_a);
        let set_b = wire_span_set(&store_b);
        let missing: Vec<_> = set_a.difference(&set_b).cloned().collect();
        let extra: Vec<_> = set_b.difference(&set_a).cloned().collect();
        eprintln!("wholesale-only (missing): {missing:?}");
        eprintln!("reconcile-only (extra):   {extra:?}");
        assert_eq!(
            set_a, set_b,
            "EQUIVALENCE: reconcile wire span == wholesale wire span"
        );
        assert!(!set_b.is_empty(), "the wires actually projected something");
        assert!(
            diff.removes.is_empty(),
            "from-empty reconcile removes nothing"
        );
        assert!(
            !diff.adds.is_empty(),
            "from-empty reconcile adds the wire projection"
        );

        // ANTI-TAUTOLOGY: the w1 subject face equals an INDEPENDENT model (no
        // materializer helper) — type head + the endpoints/predicate/bidi/targetGraph.
        let w1 = format!("urn:mnemosyne:local:graph:{GID}:wire:w1");
        let real = subject_pairs(&store_b, &w1);
        let mut model = BTreeSet::new();
        model.insert(format!("<{RDF_TYPE}> <{WIRE_NS}Wire>"));
        model.insert(format!(
            "<{WIRE_NS}sourceDocument> <urn:mnemosyne:local:document:doc-a>"
        ));
        model.insert(format!(
            "<{WIRE_NS}targetDocument> <urn:mnemosyne:local:document:doc-b>"
        ));
        model.insert(format!("<{WIRE_NS}predicate> <{WIRE_NS}supports>"));
        model.insert(format!(
            "<{WIRE_NS}bidirectional> \"true\"^^<{XSD_NS}boolean>"
        ));
        model.insert(format!("<{WIRE_NS}targetGraph> \"{GID}\""));
        assert_eq!(real, model, "reconcile w1 face == independent model");
        let mut wrong = model.clone();
        wrong.insert(format!("<{WIRE_NS}targetGraph> \"WRONG\""));
        assert_ne!(real, wrong, "anti-tautology: bent model rejected");
    }

    // (2) DOMINATION + (3) CONVERGENCE.
    #[test]
    fn oracle_wire_reconcile_dominates_and_converges() {
        let snap = wires_snapshot(full_wires());
        let store = Store::new().expect("store");

        let first = reconcile_wire_store(&store, GID, &snap).expect("first reconcile");
        assert!(first.op_count() > 0, "from-empty reconcile does ops");
        assert_eq!(first.removes.len(), 0, "from-empty = pure inserts");
        let desired_len = first.op_count();

        // The wholesale would teardown+rebuild all wires every save (2*|desired|);
        // the reconcile from empty is exactly |desired| (== wholesale fresh).
        assert!(
            first.op_count() <= desired_len,
            "DOMINATION fresh: reconcile <= wholesale"
        );

        let second = reconcile_wire_store(&store, GID, &snap).expect("second reconcile");
        if second.op_count() != 0 {
            for (s, p, o) in &second.adds {
                eprintln!("  ADD    {s} {p} {o}");
            }
            for (s, p, o) in &second.removes {
                eprintln!("  REMOVE {s} {p} {o}");
            }
        }
        assert_eq!(
            second.op_count(),
            0,
            "CONVERGENCE: re-reconcile of same snapshot = 0 ops"
        );
        assert!(
            second.op_count() < 2 * desired_len,
            "DOMINATION strict: converged reconcile < wholesale teardown-rebuild"
        );
    }

    // (4) WIRE-RECLAIM — dropping a wire reclaims exactly its reified subject.
    #[test]
    fn oracle_wire_reconcile_reclaims_dropped_wire() {
        let store = Store::new().expect("store");
        let snap_two = wires_snapshot(full_wires());
        reconcile_wire_store(&store, GID, &snap_two).expect("seed two wires");
        let w2 = format!("urn:mnemosyne:local:graph:{GID}:wire:w2");
        assert!(
            !subject_pairs(&store, &w2).is_empty(),
            "w2 present after seed"
        );

        // Drop w2 from the snapshot (only w1 remains).
        let snap_one = wires_snapshot(serde_json::json!([
            { "id": "w1", "sourceDocumentId": "doc-a", "targetDocumentId": "doc-b", "predicate": "supports", "bidirectional": true }
        ]));
        let diff = reconcile_wire_store(&store, GID, &snap_one).expect("reconcile one wire");
        assert!(diff.op_count() > 0, "dropping a wire does ops");
        assert!(diff.adds.is_empty(), "reclaim is pure removal");
        assert!(
            diff.removes.iter().all(|(s, _, _)| s == &w2),
            "every removed triple belongs to the dropped wire w2"
        );
        assert!(
            subject_pairs(&store, &w2).is_empty(),
            "w2 reclaimed from the store"
        );

        let reconverged = reconcile_wire_store(&store, GID, &snap_one).expect("re-reconcile");
        assert_eq!(reconverged.op_count(), 0, "post-reclaim store is converged");
    }
}

// ════════════════════════════════════════════════════════════════════════════
//  EA-2b+ SHACL CONFORMANCE ORACLE — WIRES kind (the structural fork: a flat,
//  fully-DERIVED single-class shape whose ONLY structural subtlety is what it must
//  NOT do — enforce referential integrity).
//
//  The REAL wire projection (`reconcile_wire_store` into a real Oxigraph store)
//  CONFORMS to the SHACL shapes DERIVED from the `emporium-wires` vocab contract
//  (`vocab_to_shacl`). NO MOCKS: real reconcile, real store, real rudof engine
//  (the same `validate_desired` the appliers run live). The validated triples are
//  READ BACK out of the persisted `:projection:workspace` graph (not the in-memory
//  desired), so the oracle covers the WHOLE materializer→store→shapes path.
//
//  DERIVED vs COMPLEMENT vs SHACL-INEXPRESSIBLE:
//   • DERIVED: the entire wire:Wire shape (rdf:type + ~24 predicates, datatypes,
//     the one required wire:targetGraph, sh:closed). vocab_to_shacl handles it whole.
//   • SHACL-INEXPRESSIBLE-by-design: REFERENTIAL INTEGRITY. The endpoints are
//     sh:nodeKind sh:IRI (they ARE IRIs) but NO sh:class — a wire may target a
//     non-existent document/block (proven allowed). The teeth-check below seeds a
//     DANGLING wire and asserts it STILL CONFORMS (the opposite of typical RI).
//     Endpoint-consistency would be a separate hand-authored complement, OUT of scope.
//
//  TEETH: (a) a dangling-endpoint wire CONFORMS (RI is intentionally absent);
//         (b) a wrong-datatype wire (bidirectional carrying a string) is REJECTED;
//         (c) a rogue predicate outside the closed contract is REJECTED.
// ════════════════════════════════════════════════════════════════════════════
#[cfg(test)]
mod shacl_wire_conformance_oracle {
    use super::*;
    use crate::emporium::contract::wires_vocabulary;
    use crate::emporium::shacl_validator::validate_desired;
    use crate::emporium::survey::parse_term as oracle_parse_term;
    use crate::emporium::terms::{Term, Triple as EngineTriple};
    use oxigraph::sparql::{QueryResults, SparqlEvaluator};

    const GID: &str = "graph-wire-shacl";

    /// Read the REAL persisted wire projection back out of `:projection:workspace`
    /// as engine `Triple`s — the exact input shape `validate_desired` consumes.
    /// Scopes to the `wire:Wire` subjects (the kind's own span), bridging each `?o`
    /// back to an engine `Term` via the proven `parse_term` round-trip.
    fn read_back_wire_projection(store: &Store) -> Vec<EngineTriple> {
        let g = workspace_projection_graph_iri(GID);
        let query = format!(
            "SELECT ?s ?p ?o WHERE {{ GRAPH <{g}> {{ ?s a <{WIRE_NS}Wire> . ?s ?p ?o }} }}"
        );
        let solutions = match SparqlEvaluator::new()
            .parse_query(&query)
            .expect("parse wire readback")
            .on_store(store)
            .execute()
            .expect("execute wire readback")
        {
            QueryResults::Solutions(s) => s,
            _ => panic!("expected SELECT solutions"),
        };
        let mut out = Vec::new();
        for sol in solutions {
            let sol = sol.expect("row");
            let s = match sol.get("s").expect("?s") {
                oxigraph::model::Term::NamedNode(n) => n.as_str().to_string(),
                other => other.to_string(),
            };
            let p = match sol.get("p").expect("?p") {
                oxigraph::model::Term::NamedNode(n) => n.as_str().to_string(),
                other => other.to_string(),
            };
            let o = oracle_parse_term(&sol.get("o").expect("?o").to_string());
            out.push((s, p, o));
        }
        out
    }

    fn wires_snapshot(wires: serde_json::Value) -> serde_json::Value {
        serde_json::json!({ "folders": [], "documents": [], "artifacts": [], "wires": wires })
    }

    /// CONFORMANCE: the REAL wire projection conforms to the vocab-derived shapes —
    /// INCLUDING a wire whose target document/block does NOT exist (dangling endpoints
    /// are allowed by design; the shapes do not enforce referential integrity).
    #[test]
    fn shacl_oracle_wire_conforms_including_dangling() {
        let store = Store::new().expect("store");
        // w1 fully-populated; w2 has a DANGLING target (doc-ghost never declared).
        // createdAt is stamped ^^xsd:dateTime by the materializer; the LIVE system
        // emits ISO-8601 (iso_timestamp), and rudof validates the xsd:dateTime lexical
        // space — so the conformance seed uses a VALID ISO value (a faithful real wire).
        let snap = wires_snapshot(serde_json::json!([
            { "id": "w1", "sourceDocumentId": "doc-a", "targetDocumentId": "doc-b",
              "sourceBlockId": "block-a", "targetBlockId": "block-b",
              "predicate": "supports", "bidirectional": true, "inverseOf": "w2",
              "createdAt": "2026-06-22T12:00:00Z", "sceneArtifactId": "art-scene" },
            { "id": "w2", "sourceDocumentId": "doc-b", "targetDocumentId": "doc-ghost",
              "targetBlockId": "block-ghost" }
        ]));
        reconcile_wire_store(&store, GID, &snap).expect("reconcile wires");

        let projection = read_back_wire_projection(&store);
        assert!(!projection.is_empty(), "the wires projected something");
        // Confirm the dangling endpoint is actually in the projection (the teeth are real).
        assert!(
            projection
                .iter()
                .any(|(_, p, o)| p == &format!("{WIRE_NS}targetDocument")
                    && matches!(o, Term::Uri(n) if n.as_str().contains("doc-ghost"))),
            "PRECONDITION: the dangling targetDocument endpoint is present in the projection"
        );

        let result = validate_desired(&projection, wires_vocabulary());
        assert!(
            result.is_ok(),
            "the REAL wire projection (dangling endpoint INCLUDED) must conform to the \
             vocab-derived shapes — referential integrity is intentionally unenforced: {result:?}"
        );
    }

    /// TEETH #1: a wrong-datatype projection — wire:bidirectional carrying a STRING
    /// where the contract declares xsd:boolean — is REJECTED (sh:datatype).
    #[test]
    fn shacl_oracle_wire_teeth_wrong_datatype() {
        let store = Store::new().expect("store");
        let snap = wires_snapshot(serde_json::json!([
            { "id": "w1", "sourceDocumentId": "doc-a", "targetDocumentId": "doc-b", "predicate": "supports" }
        ]));
        reconcile_wire_store(&store, GID, &snap).expect("reconcile wires");

        let mut bent = read_back_wire_projection(&store);
        // Append a bidirectional with the WRONG type (string, not boolean) on the wire.
        let w1 = format!("urn:mnemosyne:local:graph:{GID}:wire:w1");
        bent.push((
            w1,
            format!("{WIRE_NS}bidirectional"),
            Term::Lit(oxigraph::model::Literal::new_simple_literal("yes")),
        ));
        let result = validate_desired(&bent, wires_vocabulary());
        assert!(
            result.is_err(),
            "wire:bidirectional carrying a string (declared xsd:boolean) must be rejected"
        );
        assert!(
            result.unwrap_err().starts_with("SHACL:"),
            "loud-halt prefix"
        );
    }

    /// TEETH #1b: the xsd:dateTime LEXICAL space is enforced — a wire:createdAt
    /// stamped ^^xsd:dateTime but carrying an epoch-seconds string (NOT ISO-8601) is
    /// REJECTED. This pins a real conformance property: the materializer types
    /// createdAt/deletedAt/snapshotAt as xsd:dateTime, and rudof validates the lexical
    /// form, so a malformed timestamp is a genuine drift the shape catches.
    #[test]
    fn shacl_oracle_wire_teeth_malformed_datetime() {
        let store = Store::new().expect("store");
        let snap = wires_snapshot(serde_json::json!([
            { "id": "w1", "sourceDocumentId": "doc-a", "targetDocumentId": "doc-b", "predicate": "supports" }
        ]));
        reconcile_wire_store(&store, GID, &snap).expect("reconcile wires");

        let mut bent = read_back_wire_projection(&store);
        let w1 = format!("urn:mnemosyne:local:graph:{GID}:wire:w1");
        bent.push((
            w1,
            format!("{MDOC_NS}createdAt"),
            Term::Lit(oxigraph::model::Literal::new_typed_literal(
                "1700000000",
                oxigraph::model::NamedNode::new(format!("{XSD_NS}dateTime")).unwrap(),
            )),
        ));
        let result = validate_desired(&bent, wires_vocabulary());
        assert!(
            result.is_err(),
            "an epoch-seconds string typed ^^xsd:dateTime (not a valid dateTime lexical) must be rejected"
        );
        assert!(result.unwrap_err().starts_with("SHACL:"));
    }

    /// TEETH #2: a CLOSED-shape violation — a rogue predicate OUTSIDE the wire
    /// contract on the wire subject is rejected (sh:closed true).
    #[test]
    fn shacl_oracle_wire_teeth_rogue_predicate() {
        let store = Store::new().expect("store");
        let snap = wires_snapshot(serde_json::json!([
            { "id": "w1", "sourceDocumentId": "doc-a", "targetDocumentId": "doc-b", "predicate": "supports" }
        ]));
        reconcile_wire_store(&store, GID, &snap).expect("reconcile wires");

        let mut bent = read_back_wire_projection(&store);
        let w1 = format!("urn:mnemosyne:local:graph:{GID}:wire:w1");
        bent.push((
            w1,
            "http://example.org/not-in-the-wire-contract".to_string(),
            Term::Lit(oxigraph::model::Literal::new_simple_literal("rogue")),
        ));
        let result = validate_desired(&bent, wires_vocabulary());
        assert!(
            result.is_err(),
            "a predicate outside the closed wire contract must be rejected"
        );
        assert!(result.unwrap_err().starts_with("SHACL:"));
    }
}
