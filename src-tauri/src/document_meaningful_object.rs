//! Document-as-Meaningful-Object — the ADDITIVE, declaration-shaped projection.
//!
//! This module re-expresses the document tree→RDF projection (the existing
//! [`crate::rdf_document_tree::document_tree_triples`]) as a **Meaningful
//! Object**: a DECLARATION value that names its dimension values, plus a
//! `project(source) -> desired` front-end that WRAPS the existing pure
//! projection, plus a `reconcile` that surveys the live per-doc projection
//! graph, value-canonical-diffs `desired` vs `current` (reusing the REAL
//! emporium [`terms`] engine), and applies only the delta DIRECT-ON-STORE into
//! `document_projection_graph_iri`.
//!
//! It is a PARALLEL, tested alternative to the production materializer
//! ([`crate::rdf_record_materializer::materialize_document_record`]) — which
//! tears down the whole `{subject}#` subtree + a bare-subject allowlist and
//! re-INSERTs wholesale. The MO declares the SAME desired graph but reaches it
//! by value-diff, so a converged save emits zero ops and an edit emits only the
//! delta. `save_document` is NOT touched; this path is exercised only by the
//! parity tests below (and is the drop-in a future switch would use).
//!
//! The DELETE span the MO must reproduce is DERIVED, not transcribed: the
//! `TreeToRdf` face declares an [`OwnedSpan`] ([`Face::owned_span`]) computed
//! from the identity subject + the projection's declared bare-subject vocabulary
//! ([`crate::rdf_document_tree::DOCUMENT_LEVEL_PREDICATES`] — the SAME const the
//! old materializer's allowlist now references), and the survey SPARQL is
//! GENERATED from that span. So the parity contract lives in ONE place.
//!
//! Reconciliation reuses the memory materializer's proven shape
//! ([`crate::emporium::memory_applier`]): survey → [`terms::diff_triples`] →
//! [`terms::render_updates`] → graph-wrap + `SparqlEvaluator` on the `&Store`.
//! Like the existing materializer and the memory sink, it BYPASSES
//! `validate_sparql_update_authority` (which reserves every `:projection:`
//! graph and would refuse it).

use oxigraph::sparql::{QueryResults, SparqlEvaluator};
use oxigraph::store::Store;

use crate::document_service::DocumentRecord;
use crate::emporium::survey::parse_term;
use crate::emporium::terms::{diff_triples, render_updates, Term, Triple, TripleDiff};
use crate::rdf::{format_rdf_triple, push_integer_triple, push_string_triple, RdfTriple};
use crate::rdf_authority::document_projection_graph_iri;
use crate::rdf_document_tree::{
    document_tree_triples, CONTENT_PREDICATES, FS_SOURCED_PREDICATES, PROJECTION_META_PREDICATES,
};
use crate::runtime_config::MNEMO_NS;

// ───────────────────────── the owned span (DERIVED) ─────────────────────────

/// The portion of the per-doc projection graph a face OWNS — i.e. the exact set
/// of `(subject, predicate)` slots its reconcile is allowed to retract. This is
/// the survey/DELETE scope expressed as a VALUE, DERIVED from the declaration
/// (the identity subject + the face's declared vocabulary), never transcribed.
///
/// The Document face's span has two parts with different ownership:
/// - **Owned outright** — the fragment subtree (`{subject}#…`): every subject
///   under the doc subject's `#` namespace. Derived purely from `identity`; no
///   list. Captured by [`OwnedSpan::fragment_subject`].
/// - **Co-managed** — the bare doc subject `<{subject}>` is SHARED with other
///   projections (salience / semantic / wires), so the Document face may retract
///   only ITS OWN predicates there: the declared
///   [`DOCUMENT_LEVEL_PREDICATES`]. Captured by `(bare_subject, bare_predicates)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OwnedSpan {
    /// The doc subject whose `#`-namespace fragment subtree is owned OUTRIGHT, or
    /// `None` if this face owns nothing under `{subject}#` (e.g. the
    /// storage-metadata face, which lives entirely on the bare subject).
    pub(crate) fragment_subject: Option<String>,
    /// The bare doc subject shared with other projections.
    pub(crate) bare_subject: String,
    /// The fully-qualified bare-subject predicate IRIs this face co-manages. May
    /// be empty (e.g. `TreeToRdf`, which emits no bare-subject triple).
    pub(crate) bare_predicates: Vec<String>,
}

impl OwnedSpan {
    /// Render the span as the `SELECT`/`DELETE` FILTER body that scopes the
    /// survey: the fragment subtree (`STRSTARTS`, only when owned) OR the bare
    /// subject restricted to the co-managed predicates. GENERATED from the span —
    /// the survey SPARQL is no longer hand-written. At least one of the two
    /// disjuncts is always present (a face owns the fragment subtree, the bare
    /// predicates, or both); an all-empty span would be a programming error.
    fn filter_clause(&self) -> String {
        let mut disjuncts: Vec<String> = Vec::new();
        if let Some(frag) = &self.fragment_subject {
            disjuncts.push(format!("STRSTARTS(STR(?s), \"{frag}#\")"));
        }
        if !self.bare_predicates.is_empty() {
            let bare_clause = self
                .bare_predicates
                .iter()
                .map(|p| format!("?p = <{p}>"))
                .collect::<Vec<_>>()
                .join(" || ");
            disjuncts.push(format!(
                "( ?s = <{bare}> && ( {bare_clause} ) )",
                bare = self.bare_subject,
            ));
        }
        debug_assert!(
            !disjuncts.is_empty(),
            "an OwnedSpan must own SOMETHING (fragment subtree and/or bare predicates)"
        );
        disjuncts.join("\n      || ")
    }
}

// ───────────────────────── the declaration value ─────────────────────────

/// What kind of source the tree is read from. The Document MO's tree comes from
/// the yrs Y.Doc snapshot (`DocumentRecord.tree`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SourceKind {
    /// Tree comes from the yrs Y.Doc snapshot (`document.tree`).
    Ydoc,
    /// The bare-subject storage-metadata comes from the DURABLE STORE: the
    /// on-disk per-graph profile tree (JSON manifests + yrs binaries + Oxigraph),
    /// EFS-backed in the cell. The authority is the on-disk `DocumentRecord`
    /// (hydrated from the Y.Doc snapshot on boot); the projection is the
    /// disposable Oxigraph named graph. A THIRD durable-store source distinct
    /// from [`SourceKind::Ydoc`] (CRDT-sourced) and born-RDF. See
    /// `cell_durability` (hydrate-on-boot / flush-on-save, single-writer-per-cell).
    DurableStore,
}

/// How the target graph is reached from the desired set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Recon {
    /// Reach the SAME desired graph the old wholesale rebuild produced, but by
    /// value-diff against a survey of the live per-doc graph: a converged save
    /// emits zero ops; an edit emits only the delta.
    RebuildEquivViaDiff,
}

/// The faces this MO ships. `faces` is a LIST so blocks / markdown / curl faces
/// can slot in later without changing this one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum Face {
    /// The tree→RDF projection (the existing `document_tree_triples`). Owns the
    /// `{subject}#…` fragment subtree OUTRIGHT and emits NO bare-subject triples.
    TreeToRdf,
    /// The filesystem/durable-store storage-metadata projection: the 7 FS-sourced
    /// bare-`<subject>` predicates ([`FS_SOURCED_PREDICATES`]) read from the real
    /// `DocumentRecord` record/path fields. The FIRST path to actually emit the
    /// predicates the old allowlist NAMES but no production write path emits.
    StorageMetadataToRdf,
    /// The Y.Doc CONTENT projection: the 2 bare-`<subject>` content-serialization
    /// predicates ([`CONTENT_PREDICATES`] = `body`, `tiptapXml`) — the canonical
    /// plaintext + TipTap/ProseMirror XML serializations of the document tree. Both
    /// are authored by the Y.Doc (`SourceKind::Ydoc`); the MO reads them from the
    /// `DocumentRecord`'s cached `.body` / `.tiptap_xml` fields, which the real CRDT
    /// write path fills with `projection::materialize_ydoc(doc).body` /
    /// `projection::ydoc_to_tiptap_xml(doc)` and `save_document` persists verbatim —
    /// so on a real record they ARE byte-equal to Garden's canonical projection.
    ContentToRdf,
    /// The projection-meta emitter: the 1 self-referential `rdfTripleCount`
    /// ([`PROJECTION_META_PREDICATES`]) — the size of footprint (1)'s tree
    /// projection, `|document_tree_triples(document)|`. Tree-only by construction
    /// (it re-derives the tree count), so it counts NEITHER the bare metadata NOR
    /// itself, and has no real ordering coupling on the other bare faces.
    ProjectionMetaToRdf,
}

impl Face {
    /// The SINGLE SOURCE OF TRUTH for which faces this MO ships, in face-list
    /// order. EVERY enumeration of the faces — `dims().faces`, the parallel
    /// `dims().source_kinds`, [`full_reclaim_filter`], and the
    /// [`reconcile_document_record`] survey/desired loop — derives from THIS const,
    /// so a new face slots in by (1) adding the variant, (2) adding its arm to the
    /// (compile-time-exhaustive) [`Face::owned_span`] / [`Face::source_kind`] /
    /// [`face_desired_triples`] matches, and (3) listing it HERE — there is no
    /// separate array literal that can silently drop it. The
    /// `all_faces_registered_in_face_all` test guard re-derives this list from an
    /// exhaustive match, so a variant added without registering it here fails to
    /// compile.
    pub(crate) const ALL: &'static [Face] = &[
        Face::TreeToRdf,
        Face::StorageMetadataToRdf,
        Face::ContentToRdf,
        Face::ProjectionMetaToRdf,
    ];

    /// The [`SourceKind`] backing this face — the authority its `project()` reads.
    /// A compile-time-exhaustive match (a new face MUST declare its source here),
    /// so the `dims().source_kinds` vector is DERIVED from [`Face::ALL`] rather than
    /// being a parallel literal that can drift out of sync with `faces`.
    pub(crate) fn source_kind(self) -> SourceKind {
        match self {
            // The tree face reads the Y.Doc snapshot.
            Face::TreeToRdf => SourceKind::Ydoc,
            // The storage-metadata face reads the durable-store record/path fields.
            Face::StorageMetadataToRdf => SourceKind::DurableStore,
            // The content face reads the Y.Doc-authored cached serializations.
            Face::ContentToRdf => SourceKind::Ydoc,
            // The projection-meta face re-derives the tree-projection size (Y.Doc).
            Face::ProjectionMetaToRdf => SourceKind::Ydoc,
        }
    }

    /// The face's OWNED SPAN over the per-doc graph, DERIVED from the identity
    /// `subject` plus the face's declared vocabulary. A face declares BOTH what
    /// it projects (`project()`) AND the slots it owns, so reconcile's
    /// survey/DELETE scope is part of the declaration, not hand-written. The two
    /// faces target the SAME per-doc graph but DISJOINT spans:
    ///
    /// - [`Face::TreeToRdf`] owns the `{subject}#…` fragment subtree OUTRIGHT and
    ///   declares an EMPTY bare-predicate set — it emits NO bare-subject triples
    ///   (pinned by `document_tree_triples_emit_only_declared_bare_predicates`),
    ///   so it has nothing to co-manage there. (This also TIGHTENS the old
    ///   over-claim where `TreeToRdf` named all 10 bare predicates.)
    /// - [`Face::StorageMetadataToRdf`] owns NOTHING under `{subject}#` and
    ///   co-manages exactly the 7 [`FS_SOURCED_PREDICATES`] on the bare subject.
    ///
    /// Their union reproduces the old materializer's full reclaim span, but
    /// partitioned so neither face clobbers the other's slots.
    pub(crate) fn owned_span(self, subject: &str) -> OwnedSpan {
        match self {
            Face::TreeToRdf => OwnedSpan {
                fragment_subject: Some(subject.to_string()),
                bare_subject: subject.to_string(),
                bare_predicates: Vec::new(),
            },
            Face::StorageMetadataToRdf => OwnedSpan {
                // The storage-metadata face owns NOTHING under `{subject}#`; it
                // lives entirely on the bare subject's 7 FS-sourced predicates.
                fragment_subject: None,
                bare_subject: subject.to_string(),
                bare_predicates: FS_SOURCED_PREDICATES
                    .iter()
                    .map(|local| format!("{MNEMO_NS}{local}"))
                    .collect(),
            },
            Face::ContentToRdf => OwnedSpan {
                // The content face owns NOTHING under `{subject}#`; it co-manages
                // exactly the 2 CONTENT predicates (`body`, `tiptapXml`) on the bare
                // subject — disjoint from the 7 FS-sourced and the 1 meta predicate.
                fragment_subject: None,
                bare_subject: subject.to_string(),
                bare_predicates: CONTENT_PREDICATES
                    .iter()
                    .map(|local| format!("{MNEMO_NS}{local}"))
                    .collect(),
            },
            Face::ProjectionMetaToRdf => OwnedSpan {
                // The projection-meta face owns NOTHING under `{subject}#`; it
                // co-manages exactly the 1 PROJECTION_META predicate
                // (`rdfTripleCount`) on the bare subject.
                fragment_subject: None,
                bare_subject: subject.to_string(),
                bare_predicates: PROJECTION_META_PREDICATES
                    .iter()
                    .map(|local| format!("{MNEMO_NS}{local}"))
                    .collect(),
            },
        }
    }
}

/// The concrete dimension VALUES of the Document Meaningful Object, computed
/// from a `source` [`DocumentRecord`]. This is the declaration — it NAMES its
/// dimension values rather than describing them in prose. `target` / `identity`
/// / `source_kind` are the load-bearing values; `faces` is a list so additional
/// faces extend without disturbing this one.
#[derive(Debug, Clone)]
pub(crate) struct DocDims {
    /// The source kind backing each face, parallel to `faces`: the tree face is
    /// [`SourceKind::Ydoc`], the storage-metadata face is
    /// [`SourceKind::DurableStore`].
    pub(crate) source_kinds: Vec<SourceKind>,
    /// `document.rdf_subject` = `urn:mnemosyne:local:document:{id}`.
    pub(crate) identity: String,
    /// `document_projection_graph_iri(graph_id, document_id)` — the per-doc graph.
    pub(crate) target: String,
    /// rebuild-equiv-via-diff (old path did wholesale rebuild).
    pub(crate) reconciliation: Recon,
    /// The faces this MO ships: the tree→RDF projection and the (additive)
    /// storage-metadata bare-subject projection.
    pub(crate) faces: Vec<Face>,
}

/// Document-as-Meaningful-Object = a DECLARATION value (mirroring the
/// prototype's ReconcilePlan) that names its dimension values. Borrows the
/// `project()` input.
#[derive(Debug, Clone)]
pub(crate) struct DocumentDeclaration<'a> {
    /// The `project()` input.
    pub(crate) source: &'a DocumentRecord,
}

impl<'a> DocumentDeclaration<'a> {
    /// Build the declaration over a `source` record.
    pub(crate) fn new(source: &'a DocumentRecord) -> Self {
        Self { source }
    }

    /// Compute the concrete dimension values from `source`.
    pub(crate) fn dims(&self) -> DocDims {
        DocDims {
            // Both `faces` and the parallel `source_kinds` are DERIVED from the
            // single source of truth [`Face::ALL`] — `source_kinds[i]` is
            // `Face::ALL[i].source_kind()` — so the two vectors cannot drift apart
            // and a new face cannot be silently dropped from either.
            source_kinds: Face::ALL.iter().map(|f| f.source_kind()).collect(),
            identity: self.source.rdf_subject.clone(),
            target: document_projection_graph_iri(&self.source.graph_id, &self.source.document_id),
            reconciliation: Recon::RebuildEquivViaDiff,
            faces: Face::ALL.to_vec(),
        }
    }
}

// ───────────────────── project(source) -> desired (the face) ─────────────────────

/// project(source)->desired: the EXISTING tree projection, adapted to engine
/// triples. The projection LOGIC is reused verbatim (`document_tree_triples`);
/// this is pure adaptation.
pub(crate) fn document_desired_triples(document: &DocumentRecord) -> Vec<Triple> {
    document_tree_triples(document)
        .iter()
        .map(rdf_triple_to_term)
        .collect()
}

/// project(source)->desired for [`Face::StorageMetadataToRdf`]: the 7 FS-sourced
/// bare-`<subject>` storage-metadata triples, READ from the REAL durable-store
/// `DocumentRecord` record/path fields — NOT transcribed constants, NOT
/// re-derived paths. Each value is the field `save_document` already computed and
/// persisted to `document.json` (record fields) / derived as a path
/// (`localPath` = the `documents/{id}` fs path, `ydocStatePath` =
/// `ydocs/documents/{id}/update-v1.bin`). The face READS these; it does not
/// re-derive them.
///
/// These land on the BARE doc subject `<{document.rdf_subject}>` — the subject
/// the tree face never touches. `body`/`tiptapXml` (content, Y.Doc-authored) and
/// `rdfTripleCount` (projection-meta) are deliberately EXCLUDED here (separate
/// faces, OUT of scope per the spec). The `schemaVersion` integer is emitted as
/// an `xsd:integer` literal, matching how the record field is typed.
///
/// Built through the SAME [`RdfTriple`] → engine-`Term` adapter the tree face
/// uses ([`rdf_triple_to_term`]) so term serialization is byte-identical across
/// both faces and round-trips through the store the same way.
pub(crate) fn document_storage_metadata_triples(document: &DocumentRecord) -> Vec<Triple> {
    let subject = &document.rdf_subject;
    let mut raw: Vec<RdfTriple> = Vec::with_capacity(FS_SOURCED_PREDICATES.len());

    push_string_triple(
        &mut raw,
        subject,
        &format!("{MNEMO_NS}graphId"),
        &document.graph_id,
    );
    push_string_triple(
        &mut raw,
        subject,
        &format!("{MNEMO_NS}origin"),
        &document.origin,
    );
    push_string_triple(
        &mut raw,
        subject,
        &format!("{MNEMO_NS}providerId"),
        &document.provider_id,
    );
    push_string_triple(
        &mut raw,
        subject,
        &format!("{MNEMO_NS}localPath"),
        &document.local_path,
    );
    push_string_triple(
        &mut raw,
        subject,
        &format!("{MNEMO_NS}documentId"),
        &document.document_id,
    );
    push_integer_triple(
        &mut raw,
        subject,
        &format!("{MNEMO_NS}schemaVersion"),
        document.schema_version as i64,
    );
    push_string_triple(
        &mut raw,
        subject,
        &format!("{MNEMO_NS}ydocStatePath"),
        &document.ydoc_state_path,
    );

    raw.iter().map(rdf_triple_to_term).collect()
}

/// project(source)->desired for [`Face::ContentToRdf`]: the 2 Y.Doc-authored
/// CONTENT serializations on the bare doc subject `<{document.rdf_subject}>`:
///
/// - `mnemo:body` (`xsd:string`) = the document's canonical PLAINTEXT projection
///   of the tree. SOURCE = `document.body`, the cached field the real CRDT write
///   path fills with `projection::materialize_ydoc(doc).body`
///   (= `document_tree_plain_text(&tree)`, the cell's canonical plaintext
///   renderer) and `save_document` persists verbatim (`document.body = input.body`).
///   On a real record it IS byte-equal to that projection (spec §16). Empty
///   document → `""` (the renderer over zero children yields the empty string).
///
/// - `mnemo:tiptapXml` (`xsd:string`) = the document's canonical TipTap/ProseMirror
///   XML serialization of the tree. SOURCE = `document.tiptap_xml`, the cached field
///   the real CRDT write path fills with `projection::ydoc_to_tiptap_xml(doc)` and
///   `save_document` persists verbatim. **Availability:** the canonical serializer
///   EXISTS cell-side in Rust (`crdt_engine::projection::ydoc_to_tiptap_xml`) — it is
///   NOT frontend-only — BUT it requires a LIVE yrs `&Doc`. The MO holds a
///   `DocumentRecord` (no live Doc), and there is NO serializer from the record's
///   `tree` snapshot nor from `tiptap_json` (the only adjacent fn,
///   `content_parse::tiptap_xml_to_tiptap_json`, is the REVERSE/parse direction). So
///   the MO can only EMIT the cached field, not recompute it. This is faithful for
///   records written through the real CRDT path (which always supplies it); the
///   **documented gap** (spec §24) is that a record whose write path supplied no
///   `tiptapXml` (`save_document` does `input.tiptap_xml.unwrap_or_default()`, e.g.
///   the hand-built test records) carries an empty `tiptap_xml`, which the MO cannot
///   re-derive — it faithfully emits `""` rather than fabricating a serialization.
///
/// Both land on the BARE subject the tree face never touches, over a span DISJOINT
/// from the 7 FS-sourced and the 1 projection-meta predicate. Built through the SAME
/// [`RdfTriple`] → engine-`Term` adapter ([`rdf_triple_to_term`]) so serialization is
/// byte-identical across all bare-subject faces.
pub(crate) fn document_content_triples(document: &DocumentRecord) -> Vec<Triple> {
    let subject = &document.rdf_subject;
    let mut raw: Vec<RdfTriple> = Vec::with_capacity(CONTENT_PREDICATES.len());

    // mnemo:body — the canonical plaintext projection (cached `.body`, byte-equal
    // to `document_tree_plain_text` on a real record). Empty doc → "".
    push_string_triple(
        &mut raw,
        subject,
        &format!("{MNEMO_NS}body"),
        &document.body,
    );
    // mnemo:tiptapXml — the canonical TipTap/ProseMirror XML serialization (cached
    // `.tiptap_xml`; emitted as-is — see the availability gap above).
    push_string_triple(
        &mut raw,
        subject,
        &format!("{MNEMO_NS}tiptapXml"),
        &document.tiptap_xml,
    );

    raw.iter().map(rdf_triple_to_term).collect()
}

/// project(source)->desired for [`Face::ProjectionMetaToRdf`]: the 1
/// projection-meta triple `mnemo:rdfTripleCount` (`xsd:integer`) on the bare doc
/// subject = `|document_tree_triples(document)|`, the size of footprint (1)'s TREE
/// projection ONLY (the `{subject}#…` fragment subtree).
///
/// It re-derives the count from the tree, so it counts NEITHER the bare-subject
/// metadata (the 9 other bare predicates) NOR itself — eliminating self-reference
/// and making the value order-stable (it depends only on the tree face, never on
/// the metadata faces). This is identical to what production computes
/// (`document_persistence_service`: `rdf_triple_count = document_tree_triples(&document).len()`),
/// so on a real record it equals the cached `document.rdf_triple_count`. Empty
/// document (`tree = None`) → `document_tree_triples` returns `∅` → `0`.
///
/// Although the spec stipulates it is "computed after the tree projection," there
/// is no actual ordering coupling: re-deriving `|document_tree_triples|` is
/// self-contained, so the face is safe at any position in the face list.
pub(crate) fn document_projection_meta_triples(document: &DocumentRecord) -> Vec<Triple> {
    let subject = &document.rdf_subject;
    let mut raw: Vec<RdfTriple> = Vec::with_capacity(PROJECTION_META_PREDICATES.len());

    let tree_triple_count = document_tree_triples(document).len();
    push_integer_triple(
        &mut raw,
        subject,
        &format!("{MNEMO_NS}rdfTripleCount"),
        tree_triple_count as i64,
    );

    raw.iter().map(rdf_triple_to_term).collect()
}

/// project(source)->desired for ONE face. Dispatches to the face's projection:
/// [`Face::TreeToRdf`] → the tree projection; [`Face::StorageMetadataToRdf`] →
/// the FS-sourced storage-metadata triples; [`Face::ContentToRdf`] → the 2 Y.Doc
/// content serializations; [`Face::ProjectionMetaToRdf`] → the tree-projection
/// `rdfTripleCount`.
pub(crate) fn face_desired_triples(face: Face, document: &DocumentRecord) -> Vec<Triple> {
    match face {
        Face::TreeToRdf => document_desired_triples(document),
        Face::StorageMetadataToRdf => document_storage_metadata_triples(document),
        Face::ContentToRdf => document_content_triples(document),
        Face::ProjectionMetaToRdf => document_projection_meta_triples(document),
    }
}

/// Adapt one [`RdfTriple`] → `(s, p, Term)` WITHOUT touching private fields.
/// `format_rdf_triple(&t)` emits `<s> <p> OBJ .`; we split off `s`, `p` and feed
/// `OBJ` to [`parse_term`], which already consumes `<uri>` / `"lit"` /
/// `"lit"^^<dt>` exactly as `format_rdf_triple` emits them. No new vocab, no
/// field access.
fn rdf_triple_to_term(t: &RdfTriple) -> Triple {
    let line = format_rdf_triple(t);
    // Shape: `<s> <p> OBJ .`  — s and p are always angle-bracketed URIs.
    let rest = line.trim();
    let (subject, rest) = split_angle_iri(rest)
        .unwrap_or_else(|| panic!("malformed serialized triple subject: {line}"));
    let (predicate, rest) = split_angle_iri(rest.trim_start())
        .unwrap_or_else(|| panic!("malformed serialized triple predicate: {line}"));
    // The remainder is `OBJ .` — strip the trailing ` .` and parse the object.
    let object_nt = rest.trim().strip_suffix('.').unwrap_or(rest).trim();
    let object: Term = parse_term(object_nt);
    (subject, predicate, object)
}

/// Split a leading `<iri>` token, returning `(iri, remainder)`.
fn split_angle_iri(s: &str) -> Option<(String, &str)> {
    let s = s.strip_prefix('<')?;
    let close = s.find('>')?;
    Some((s[..close].to_string(), &s[close + 1..]))
}

// ───────────────────────── survey current (the read) ─────────────────────────

/// Survey the live per-doc projection graph for the triples covered by the FULL
/// document reclaim span — the UNION of every face's [`OwnedSpan`] (the fragment
/// subtree `STRSTARTS "{subject}#"` from `TreeToRdf` PLUS all 10 bare-subject
/// `DOCUMENT_LEVEL_PREDICATES`: the 7 FS-sourced from `StorageMetadataToRdf`, the 2
/// content from `ContentToRdf`, and the 1 projection-meta from
/// `ProjectionMetaToRdf`). This is exactly the span the OLD wholesale materializer's
/// DELETE covers, so it remains the right measure of "what the old path would have
/// re-deleted." Per-face reconcile uses [`survey_face_span`]; this union view is the
/// wholesale-equivalent oracle.
/// Returns `current: Vec<Triple>` via the proven oxigraph-`term.to_string()` →
/// [`parse_term`] round-trip.
pub(crate) fn survey_document_projection(
    store: &Store,
    graph_id: &str,
    document_id: &str,
    subject: &str,
) -> Result<Vec<Triple>, String> {
    let filter = full_reclaim_filter(subject);
    survey_with_filter(store, graph_id, document_id, &filter)
}

/// Survey the live per-doc projection graph scoped to ONE face's [`OwnedSpan`].
/// Used by the per-face reconcile loop.
fn survey_face_span(
    store: &Store,
    graph_id: &str,
    document_id: &str,
    face: Face,
    subject: &str,
) -> Result<Vec<Triple>, String> {
    let filter = face.owned_span(subject).filter_clause();
    survey_with_filter(store, graph_id, document_id, &filter)
}

/// Survey the live per-doc projection graph scoped to an arbitrary
/// [`OwnedSpan`] — the REAL span-SQL (`filter_clause` → `survey_with_filter`),
/// the SAME code path [`survey_face_span`] and [`reconcile_document_record`]
/// drive, but parameterized by an externally-supplied span rather than a
/// production [`Face`]. This exists so the differential oracle can exercise the
/// production `STRSTARTS`-subtree / bare-predicate-restricted survey over the
/// shared L5 corpus (whose face scopes are NOT the 4 production faces) against a
/// seeded Oxigraph [`Store`] — driving the production filter, not a mirror.
pub(crate) fn survey_owned_span(
    store: &Store,
    graph_id: &str,
    document_id: &str,
    span: &OwnedSpan,
) -> Result<Vec<Triple>, String> {
    let filter = span.filter_clause();
    survey_with_filter(store, graph_id, document_id, &filter)
}

/// The UNION of every face's owned-span filter, OR-joined — the full document
/// reclaim span (DERIVED from the faces, not hand-written).
fn full_reclaim_filter(subject: &str) -> String {
    Face::ALL
        .iter()
        .map(|face| format!("( {} )", face.owned_span(subject).filter_clause()))
        .collect::<Vec<_>>()
        .join("\n      || ")
}

/// Run one survey SELECT scoped by a pre-rendered FILTER body.
fn survey_with_filter(
    store: &Store,
    graph_id: &str,
    document_id: &str,
    filter: &str,
) -> Result<Vec<Triple>, String> {
    let graph = document_projection_graph_iri(graph_id, document_id);
    let query = format!(
        r#"SELECT ?s ?p ?o WHERE {{
  GRAPH <{graph}> {{
    ?s ?p ?o .
    FILTER(
      {filter}
    )
  }}
}}"#
    );

    let solutions = match SparqlEvaluator::new()
        .parse_query(&query)
        .map_err(|e| format!("parse document survey: {e}"))?
        .on_store(store)
        .execute()
        .map_err(|e| format!("execute document survey: {e}"))?
    {
        QueryResults::Solutions(s) => s,
        _ => return Err("document survey expected SELECT solutions".to_string()),
    };

    let mut out = Vec::new();
    for sol in solutions {
        let sol = sol.map_err(|e| format!("document survey row: {e}"))?;
        let s = iri_string(sol.get("s").ok_or("survey row missing ?s")?)?;
        let p = iri_string(sol.get("p").ok_or("survey row missing ?p")?)?;
        let o = parse_term(&sol.get("o").ok_or("survey row missing ?o")?.to_string());
        out.push((s, p, o));
    }
    Ok(out)
}

/// Bare IRI string for a subject/predicate term (the survey only ever binds
/// NamedNodes to `?s`/`?p`).
fn iri_string(term: &oxigraph::model::Term) -> Result<String, String> {
    match term {
        oxigraph::model::Term::NamedNode(n) => Ok(n.as_str().to_string()),
        other => Err(format!(
            "expected a NamedNode subject/predicate, got {other}"
        )),
    }
}

// ───────────────────────── apply (the write sink) ─────────────────────────

/// Run one already-rendered `INSERT DATA` / `DELETE DATA` body against the
/// per-doc projection graph, GRAPH-wrapping it into
/// `document_projection_graph_iri` first. Port of
/// `memory_applier::run_memory_update` + `graph_wrap_memory`, retargeted to the
/// document graph. DIRECT-ON-STORE; bypasses the authority gate (as the
/// existing materializer and memory sink do). LOUD-halt on the first error.
pub(crate) fn run_document_update(
    store: &Store,
    graph_id: &str,
    document_id: &str,
    body: &str,
) -> Result<(), String> {
    let graph = document_projection_graph_iri(graph_id, document_id);
    let wrapped = graph_wrap_document(body, &graph)?;
    SparqlEvaluator::new()
        .parse_update(&wrapped)
        .map_err(|e| format!("parse document update: {e}"))?
        .on_store(store)
        .execute()
        .map_err(|e| format!("execute document update: {e}"))
}

/// `INSERT/DELETE DATA { body }` → same verb wrapped in
/// `GRAPH <{doc_graph}> { body }`. Port of `memory_applier::graph_wrap_memory`,
/// scoped to the document graph: match the leading verb, take the body between
/// the FIRST `{` and the LAST `}`, re-emit wrapped. The `render_updates` output
/// satisfies this shape.
fn graph_wrap_document(update: &str, doc_graph: &str) -> Result<String, String> {
    let trimmed = update.trim_start();
    let (verb_word, after) = if let Some(rest) = trimmed.strip_prefix("INSERT DATA") {
        ("INSERT DATA", rest)
    } else if let Some(rest) = trimmed.strip_prefix("DELETE DATA") {
        ("DELETE DATA", rest)
    } else {
        return Err(format!(
            "unexpected document update shape: {}",
            &update.chars().take(80).collect::<String>()
        ));
    };
    let open = after
        .find('{')
        .ok_or_else(|| "document update missing opening brace".to_string())?;
    let close = update
        .rfind('}')
        .ok_or_else(|| "document update missing closing brace".to_string())?;
    let body_start = update.len() - after.len() + open + 1;
    if body_start > close {
        return Err("document update has empty/invalid body span".to_string());
    }
    let body = &update[body_start..close];
    Ok(format!(
        "{verb_word} {{ GRAPH <{doc_graph}> {{\n{body}\n}} }}"
    ))
}

// ───────────────────────── reconcile (the MO entry point) ─────────────────────────

/// ADDITIVE entry point: reconcile the per-doc projection graph to the document's
/// FULL declared footprint by VALUE-DIFF, living BESIDE (not replacing)
/// `materialize_document_record`. Iterates the MO's FACES
/// ([`Face::TreeToRdf`] ⊕ [`Face::StorageMetadataToRdf`] ⊕ [`Face::ContentToRdf`]
/// ⊕ [`Face::ProjectionMetaToRdf`]): for each face it surveys `current` over the
/// face's DISJOINT [`OwnedSpan`] and computes that face's `desired`
/// ([`face_desired_triples`]). It then UNIONS the per-face surveys and the
/// per-face desired sets and runs ONE value-canonical [`diff_triples`] over the
/// union (the faces own disjoint spans, so the union diff is exactly the per-face
/// diffs concatenated), rendering DELETE/INSERT bodies applied direct-on-store via
/// [`run_document_update`]. Returns the structured [`TripleDiff`] it applied — a
/// converged save returns an empty diff; an edit returns only the delta. The op
/// count (`removes + adds`, via [`TripleDiff::op_count`]) is one face of it,
/// projected at the consumer.
///
/// With all four faces, the combined `desired` covers the COMPLETE per-doc
/// footprint: the tree (`{subject}#…`) PLUS all 10 bare-subject
/// `DOCUMENT_LEVEL_PREDICATES` — 7 FS-sourced (storage metadata) + 2 content
/// (`body`/`tiptapXml`) + 1 projection-meta (`rdfTripleCount`) — over DISJOINT
/// spans. This is the FIRST path to emit any of those 10 bare predicates (the old
/// allowlist NAMES them but no production write path emits them), while still
/// reproducing the tree footprint and honoring the reclaim invariant (a stale bare
/// predicate — or any with a changed value — diffs to convergence). `tree:None`
/// records flow through unchanged: the tree face's `desired = ∅` ⇒ its survey rows
/// all remove, and the projection-meta face emits `rdfTripleCount = 0`; the
/// storage-metadata + content faces still emit their bare triples (a record always
/// carries them; an empty body is `""`).
pub(crate) fn reconcile_document_record(
    store: &Store,
    document: &DocumentRecord,
) -> Result<TripleDiff, String> {
    let subject = document.rdf_subject.clone();

    // Per-face survey (disjoint spans) ∪ per-face desired (disjoint subjects/
    // predicates). Because the spans are disjoint, surveying each and unioning is
    // equivalent to one union-span survey, and lets each face declare its own
    // scope — the composition is purely additive.
    let mut current: Vec<Triple> = Vec::new();
    let mut desired: Vec<Triple> = Vec::new();
    for &face in Face::ALL {
        current.extend(survey_face_span(
            store,
            &document.graph_id,
            &document.document_id,
            face,
            &subject,
        )?);
        desired.extend(face_desired_triples(face, document));
    }

    let diff = diff_triples(&current, &desired);

    // DELETE first, then INSERT (the materializer's order; graph-agnostic bodies).
    for body in render_updates("DELETE DATA", &diff.removes, 60) {
        run_document_update(store, &document.graph_id, &document.document_id, &body)?;
    }
    for body in render_updates("INSERT DATA", &diff.adds, 60) {
        run_document_update(store, &document.graph_id, &document.document_id, &body)?;
    }

    Ok(diff)
}

#[cfg(all(test, feature = "headless"))]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    use oxigraph::sparql::QueryResults;

    use crate::document_service::{DocumentTreeSnapshot, TreeNodeAttributes, TreeNodeSnapshot};
    use crate::emporium::terms::{canon_value, CanonValue};
    use crate::rdf::document_subject;
    use crate::rdf_authority::{document_projection_graph_iri, user_rdf_graph_iri};
    use crate::rdf_record_materializer::materialize_document_record_with_triples;
    use crate::runtime_config::{
        DOCUMENT_SCHEMA_VERSION, LOCAL_GRAPH_ORIGIN, LOCAL_PROVIDER_ID, MNEMO_NS,
    };

    const GRAPH: &str = "graph-a";
    const DOC: &str = "doc-a";

    /// Build a real DocumentRecord with a populated tree. `paragraph_text` keys the
    /// editable content so v1/v2 transitions can change one block's text.
    fn document_record_with_text(paragraph_text: &str) -> DocumentRecord {
        DocumentRecord {
            document_id: DOC.to_string(),
            graph_id: GRAPH.to_string(),
            title: "Document A".to_string(),
            revision: 1,
            body: String::new(),
            origin: LOCAL_GRAPH_ORIGIN.to_string(),
            provider_id: LOCAL_PROVIDER_ID.to_string(),
            local_path: format!("/tmp/{DOC}"),
            rdf_subject: document_subject(DOC),
            created_at: "1".to_string(),
            updated_at: "2".to_string(),
            capabilities: Vec::new(),
            schema_version: DOCUMENT_SCHEMA_VERSION,
            tiptap_xml: String::new(),
            tiptap_json: None,
            ydoc_update_base64: String::new(),
            ydoc_state_path: String::new(),
            tree: Some(DocumentTreeSnapshot {
                doc_id: DOC.to_string(),
                root: TreeNodeSnapshot {
                    kind: "element".to_string(),
                    tag_name: Some("doc".to_string()),
                    text_content: None,
                    attributes: TreeNodeAttributes::default(),
                    children: vec![TreeNodeSnapshot {
                        kind: "element".to_string(),
                        tag_name: Some("paragraph".to_string()),
                        text_content: None,
                        attributes: TreeNodeAttributes {
                            block_id: Some("block-a".to_string()),
                            ..TreeNodeAttributes::default()
                        },
                        children: vec![TreeNodeSnapshot {
                            kind: "text".to_string(),
                            tag_name: None,
                            text_content: Some(paragraph_text.to_string()),
                            attributes: TreeNodeAttributes::default(),
                            children: Vec::new(),
                        }],
                    }],
                },
            }),
            blocks: Vec::new(),
            rdf_triple_count: 0,
        }
    }

    fn document_record_empty_tree() -> DocumentRecord {
        let mut rec = document_record_with_text("ignored");
        rec.tree = None;
        rec
    }

    /// Canon-set of the WHOLE per-doc projection graph (NOT scoped to the survey
    /// span) — the value-canonical equality used for net-state parity. Mirrors the
    /// planner-parity `canon_set`: survey ?s ?p ?o, map each through `parse_term` +
    /// `canon_value` into a `BTreeSet<(String,String,CanonValue)>`.
    fn canon_set_of_graph(store: &Store, graph: &str) -> BTreeSet<(String, String, CanonValue)> {
        let query = format!("SELECT ?s ?p ?o WHERE {{ GRAPH <{graph}> {{ ?s ?p ?o }} }}");
        let solutions = match SparqlEvaluator::new()
            .parse_query(&query)
            .expect("parse canon query")
            .on_store(store)
            .execute()
            .expect("execute canon query")
        {
            QueryResults::Solutions(s) => s,
            _ => panic!("expected solutions"),
        };
        let mut set = BTreeSet::new();
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
            let o = parse_term(&sol.get("o").expect("?o").to_string());
            set.insert((s, p, canon_value(&o)));
        }
        set
    }

    fn count(store: &Store, query: &str) -> usize {
        match SparqlEvaluator::new()
            .parse_query(query)
            .expect("parse count query")
            .on_store(store)
            .execute()
            .expect("execute count query")
        {
            QueryResults::Solutions(solutions) => solutions.count(),
            _ => panic!("expected SELECT solutions"),
        }
    }

    /// Stringified `?o` bindings of a single-var SELECT (N-Triples form).
    fn run_sparql_rows(store: &Store, query: &str) -> Vec<String> {
        match SparqlEvaluator::new()
            .parse_query(query)
            .expect("parse rows query")
            .on_store(store)
            .execute()
            .expect("execute rows query")
        {
            QueryResults::Solutions(solutions) => solutions
                .map(|sol| sol.expect("row").get("o").expect("?o").to_string())
                .collect(),
            _ => panic!("expected SELECT solutions"),
        }
    }

    // ── declaration value ──────────────────────────────────────────────────

    #[test]
    fn declaration_names_its_dimension_values() {
        let rec = document_record_with_text("Hello");
        let dims = DocumentDeclaration::new(&rec).dims();
        assert_eq!(
            dims.source_kinds,
            vec![
                SourceKind::Ydoc,
                SourceKind::DurableStore,
                SourceKind::Ydoc,
                SourceKind::Ydoc,
            ]
        );
        assert_eq!(dims.identity, document_subject(DOC));
        assert_eq!(dims.target, document_projection_graph_iri(GRAPH, DOC));
        assert_eq!(dims.reconciliation, Recon::RebuildEquivViaDiff);
        assert_eq!(
            dims.faces,
            vec![
                Face::TreeToRdf,
                Face::StorageMetadataToRdf,
                Face::ContentToRdf,
                Face::ProjectionMetaToRdf,
            ]
        );
    }

    /// FACE-REGISTRATION GUARD: every [`Face`] variant must be listed in the single
    /// source of truth [`Face::ALL`]. The `match` is exhaustive, so adding a new
    /// `Face` variant WITHOUT an arm here fails to COMPILE; each arm then asserts the
    /// variant is present in `Face::ALL`, so a variant that compiles but was never
    /// registered fails the TEST. Together these close the silent-drop trap: a new
    /// face cannot be reached by `dims().faces` / `source_kinds` / the reclaim filter
    /// / the reconcile loop (all derived from `Face::ALL`) unless it is registered
    /// here AND in `Face::ALL`.
    #[test]
    fn all_faces_registered_in_face_all() {
        // Exhaustive over Face — a NEW variant breaks compilation until it gets an
        // arm, which forces the author to confirm registration in `Face::ALL`.
        fn assert_registered(face: Face) {
            assert!(
                Face::ALL.contains(&face),
                "Face::{face:?} is not listed in Face::ALL — it would be silently \
                 dropped from dims()/full_reclaim_filter/reconcile_document_record"
            );
        }
        for variant in [
            Face::TreeToRdf,
            Face::StorageMetadataToRdf,
            Face::ContentToRdf,
            Face::ProjectionMetaToRdf,
        ] {
            // The compile-time exhaustiveness lives in this match: a new variant
            // forces a new arm, and every arm routes through the registration check.
            match variant {
                Face::TreeToRdf
                | Face::StorageMetadataToRdf
                | Face::ContentToRdf
                | Face::ProjectionMetaToRdf => assert_registered(variant),
            }
        }
        // No DUPLICATES and no EXTRAS: Face::ALL is exactly the registered set.
        let unique: BTreeSet<Face> = Face::ALL.iter().copied().collect();
        assert_eq!(
            unique.len(),
            Face::ALL.len(),
            "Face::ALL must not contain duplicate faces"
        );
        // Every entry in Face::ALL declares a source_kind (no panic / unreachable).
        for &face in Face::ALL {
            let _ = face.source_kind();
        }
    }

    /// The expected canon-set of the 7 FS-sourced bare-`<subject>` storage-metadata
    /// triples for a record — built directly from the record fields, INDEPENDENT of
    /// `document_storage_metadata_triples` so it is a real oracle (not a tautology).
    fn expected_storage_metadata_canon(
        rec: &DocumentRecord,
    ) -> BTreeSet<(String, String, CanonValue)> {
        let s = rec.rdf_subject.clone();
        let lit =
            |v: &str| canon_value(&Term::Lit(oxigraph::model::Literal::new_simple_literal(v)));
        let int = |v: i64| {
            canon_value(&Term::Lit(oxigraph::model::Literal::new_typed_literal(
                v.to_string(),
                oxigraph::model::vocab::xsd::INTEGER,
            )))
        };
        let mut set = BTreeSet::new();
        set.insert((s.clone(), format!("{MNEMO_NS}graphId"), lit(&rec.graph_id)));
        set.insert((s.clone(), format!("{MNEMO_NS}origin"), lit(&rec.origin)));
        set.insert((
            s.clone(),
            format!("{MNEMO_NS}providerId"),
            lit(&rec.provider_id),
        ));
        set.insert((
            s.clone(),
            format!("{MNEMO_NS}localPath"),
            lit(&rec.local_path),
        ));
        set.insert((
            s.clone(),
            format!("{MNEMO_NS}documentId"),
            lit(&rec.document_id),
        ));
        set.insert((
            s.clone(),
            format!("{MNEMO_NS}schemaVersion"),
            int(rec.schema_version as i64),
        ));
        set.insert((
            s.clone(),
            format!("{MNEMO_NS}ydocStatePath"),
            lit(&rec.ydoc_state_path),
        ));
        set
    }

    /// The expected canon-set of the 2 CONTENT bare-`<subject>` triples
    /// (`body`/`tiptapXml`) for a record — built directly from the record fields,
    /// INDEPENDENT of `document_content_triples`. Both are `xsd:string` literals.
    fn expected_content_canon(rec: &DocumentRecord) -> BTreeSet<(String, String, CanonValue)> {
        let s = rec.rdf_subject.clone();
        let lit =
            |v: &str| canon_value(&Term::Lit(oxigraph::model::Literal::new_simple_literal(v)));
        let mut set = BTreeSet::new();
        set.insert((s.clone(), format!("{MNEMO_NS}body"), lit(&rec.body)));
        set.insert((s, format!("{MNEMO_NS}tiptapXml"), lit(&rec.tiptap_xml)));
        set
    }

    /// The expected canon-set of the 1 projection-meta bare-`<subject>` triple
    /// (`rdfTripleCount`) — built directly from `|document_tree_triples|` (an
    /// independent re-derivation, NOT the emitter), as an `xsd:integer`.
    fn expected_projection_meta_canon(
        rec: &DocumentRecord,
    ) -> BTreeSet<(String, String, CanonValue)> {
        let s = rec.rdf_subject.clone();
        let int = |v: i64| {
            canon_value(&Term::Lit(oxigraph::model::Literal::new_typed_literal(
                v.to_string(),
                oxigraph::model::vocab::xsd::INTEGER,
            )))
        };
        let mut set = BTreeSet::new();
        set.insert((
            s,
            format!("{MNEMO_NS}rdfTripleCount"),
            int(document_tree_triples(rec).len() as i64),
        ));
        set
    }

    /// The full bare-subject metadata footprint the COMBINED MO now adds beyond the
    /// tree: the 7 FS-sourced ∪ the 2 content ∪ the 1 projection-meta = all 10 bare
    /// `DOCUMENT_LEVEL_PREDICATES`. Independent oracle (built from record fields +
    /// tree-triple count), used by the net-state parity tests.
    fn expected_bare_metadata_canon(
        rec: &DocumentRecord,
    ) -> BTreeSet<(String, String, CanonValue)> {
        let mut set = expected_storage_metadata_canon(rec);
        set.extend(expected_content_canon(rec));
        set.extend(expected_projection_meta_canon(rec));
        set
    }

    /// The content face emits EXACTLY the 2 CONTENT bare-subject triples
    /// (`body`/`tiptapXml`), sourced from the real record fields (derived, not
    /// transcribed), all on the bare doc subject.
    #[test]
    fn content_face_projects_the_two_content_triples() {
        let rec = document_record_with_text("Hello");
        let triples = document_content_triples(&rec);
        assert_eq!(triples.len(), 2, "exactly the 2 CONTENT predicates");
        let got: BTreeSet<(String, String, CanonValue)> = triples
            .iter()
            .map(|(s, p, o)| (s.clone(), p.clone(), canon_value(o)))
            .collect();
        assert_eq!(
            got,
            expected_content_canon(&rec),
            "the content face emits body/tiptapXml onto the bare subject"
        );
        assert!(
            triples.iter().all(|(s, _, _)| s == &rec.rdf_subject),
            "content triples land on the bare doc subject"
        );
    }

    /// The projection-meta face emits EXACTLY the 1 `rdfTripleCount` triple, equal
    /// to `|document_tree_triples|` (tree-only) as an `xsd:integer`.
    #[test]
    fn projection_meta_face_emits_tree_triple_count() {
        let rec = document_record_with_text("Hello");
        let triples = document_projection_meta_triples(&rec);
        assert_eq!(triples.len(), 1, "exactly the 1 PROJECTION_META predicate");
        let got: BTreeSet<(String, String, CanonValue)> = triples
            .iter()
            .map(|(s, p, o)| (s.clone(), p.clone(), canon_value(o)))
            .collect();
        assert_eq!(
            got,
            expected_projection_meta_canon(&rec),
            "rdfTripleCount = |document_tree_triples| (tree-only, xsd:integer)"
        );
        // It counts the TREE ONLY — NOT the bare metadata, NOT itself.
        assert_eq!(
            triples[0].0, rec.rdf_subject,
            "rdfTripleCount lands on the bare doc subject"
        );
    }

    /// EMPTY DOCUMENT (`tree = None`, body/tiptapXml empty): `body=""`,
    /// `tiptapXml=""` (the cached fields, faithfully emitted — see the documented
    /// availability gap), and `rdfTripleCount=0` (tree-only over the empty tree).
    #[test]
    fn empty_document_content_and_meta_are_faithful() {
        let rec = document_record_empty_tree();
        assert!(rec.body.is_empty() && rec.tiptap_xml.is_empty());

        let content = document_content_triples(&rec);
        let content_set: BTreeSet<(String, String, CanonValue)> = content
            .iter()
            .map(|(s, p, o)| (s.clone(), p.clone(), canon_value(o)))
            .collect();
        assert_eq!(
            content_set,
            expected_content_canon(&rec),
            "empty doc: body=\"\" and tiptapXml=\"\" are EMITTED (not omitted)"
        );

        let meta = document_projection_meta_triples(&rec);
        assert_eq!(meta.len(), 1);
        // tree=None ⇒ document_tree_triples = ∅ ⇒ rdfTripleCount = 0.
        assert_eq!(
            document_tree_triples(&rec).len(),
            0,
            "empty tree projects zero triples"
        );
        let meta_set: BTreeSet<(String, String, CanonValue)> = meta
            .iter()
            .map(|(s, p, o)| (s.clone(), p.clone(), canon_value(o)))
            .collect();
        assert_eq!(
            meta_set,
            expected_projection_meta_canon(&rec),
            "empty doc: rdfTripleCount = 0"
        );
        // DETERMINISM: re-deriving yields the identical canon-set.
        let meta_set_again: BTreeSet<(String, String, CanonValue)> =
            document_projection_meta_triples(&rec)
                .iter()
                .map(|(s, p, o)| (s.clone(), p.clone(), canon_value(o)))
                .collect();
        assert_eq!(meta_set, meta_set_again, "deterministic");
    }

    /// The project() of the storage-metadata face emits EXACTLY the 7 FS-sourced
    /// bare-subject triples, sourced from the real record fields (derived, not
    /// transcribed).
    #[test]
    fn storage_metadata_face_projects_the_seven_fs_sourced_triples() {
        let rec = document_record_with_text("Hello");
        let triples = document_storage_metadata_triples(&rec);
        assert_eq!(triples.len(), 7, "exactly the 7 FS-sourced predicates");
        let got: BTreeSet<(String, String, CanonValue)> = triples
            .iter()
            .map(|(s, p, o)| (s.clone(), p.clone(), canon_value(o)))
            .collect();
        assert_eq!(
            got,
            expected_storage_metadata_canon(&rec),
            "the face emits the 7 record/path values onto the bare subject"
        );
        // ALL on the bare subject — nothing under `{subject}#`.
        assert!(
            triples.iter().all(|(s, _, _)| s == &rec.rdf_subject),
            "storage-metadata triples land on the bare doc subject"
        );
    }

    /// The project front-end REUSES the existing projection verbatim: the adapted
    /// desired set has exactly as many triples as `document_tree_triples`, with the
    /// same subjects/predicates (adaptation, not reimplementation).
    #[test]
    fn project_wraps_existing_tree_projection() {
        let rec = document_record_with_text("Hello");
        let raw = document_tree_triples(&rec);
        let desired = document_desired_triples(&rec);
        assert_eq!(raw.len(), desired.len());
        assert!(!desired.is_empty(), "a populated tree projects triples");
    }

    // ── FRESH parity ───────────────────────────────────────────────────────

    #[test]
    fn fresh_parity_old_vs_new_net_state_identical() {
        let rec = document_record_with_text("Hello world");
        let graph = document_projection_graph_iri(GRAPH, DOC);

        let store_old = Store::new().expect("store A");
        materialize_document_record_with_triples(&store_old, &rec, &document_tree_triples(&rec))
            .expect("old wholesale materialize");

        let store_new = Store::new().expect("store B");
        let ops = reconcile_document_record(&store_new, &rec)
            .expect("new reconcile")
            .op_count();
        assert!(ops > 0, "a fresh reconcile on a clean store writes triples");

        // FULL footprint: the NEW combined MO = the OLD tree footprint ∪ ALL 10
        // bare-subject `DOCUMENT_LEVEL_PREDICATES` (7 FS-sourced storage-metadata +
        // 2 content + 1 projection-meta) — none of which the OLD path ever wrote
        // (the allowlist never fired). The tree subset must be byte-identical to the
        // OLD materializer output.
        let old_set = canon_set_of_graph(&store_old, &graph);
        let new_set = canon_set_of_graph(&store_new, &graph);
        let bare = expected_bare_metadata_canon(&rec);

        assert!(
            old_set.is_subset(&new_set),
            "FRESH: the OLD tree footprint is reproduced byte-for-byte by the NEW MO"
        );
        assert!(
            bare.is_subset(&new_set),
            "FRESH: the NEW MO is the FIRST path to emit the 10 bare-subject triples"
        );
        assert_eq!(
            bare.len(),
            10,
            "the 10 bare DOCUMENT_LEVEL_PREDICATES (7 fs + 2 content + 1 meta)"
        );
        let mut expected = old_set.clone();
        expected.extend(bare.iter().cloned());
        assert_eq!(
            new_set, expected,
            "FRESH: per-doc graph = (OLD tree footprint) ∪ (10 bare-subject triples)"
        );
        // The OLD path wrote ZERO bare-subject metadata, so all 10 are genuinely new.
        assert!(
            bare.is_disjoint(&old_set),
            "the 10 bare-subject triples are absent from the OLD footprint"
        );
    }

    // ── UPDATE parity + improvement + convergence ───────────────────────────

    #[test]
    fn update_parity_improvement_and_convergence() {
        let graph = document_projection_graph_iri(GRAPH, DOC);
        let rec_v1 = document_record_with_text("first version of the paragraph");
        let rec_v2 = document_record_with_text("second version of the paragraph");

        // Seed the OLD store to v1 with the OLD materializer (tree only).
        let store_old = Store::new().expect("store A");
        materialize_document_record_with_triples(
            &store_old,
            &rec_v1,
            &document_tree_triples(&rec_v1),
        )
        .expect("seed A v1");
        // Seed the NEW store to the FULL v1 footprint via the NEW reconcile (tree +
        // all 10 bare), so the v1→v2 transition's bare values are UNCHANGED (same
        // tree shape ⇒ same rdfTripleCount; body/tiptapXml empty in these synthetic
        // records ⇒ stable) — isolating the tree delta as the improvement we measure.
        let store_new = Store::new().expect("store B");
        reconcile_document_record(&store_new, &rec_v1).expect("seed B v1 via reconcile");

        // OLD wholesale op count for the v1→v2 transition: the old DELETE span over
        // the live graph + a full re-INSERT of the v2 tree.
        let old_removes = survey_document_projection(&store_old, GRAPH, DOC, &rec_v1.rdf_subject)
            .expect("survey old delete span")
            .len();
        let old_adds = document_tree_triples(&rec_v2).len();
        let ops_old = old_removes + old_adds;

        // Apply v2: OLD wholesale on A, NEW diffed on B.
        materialize_document_record_with_triples(
            &store_old,
            &rec_v2,
            &document_tree_triples(&rec_v2),
        )
        .expect("old v2");
        let ops_new = reconcile_document_record(&store_new, &rec_v2)
            .expect("new v2")
            .op_count();

        // NET STATE: the NEW full footprint = the OLD tree footprint ∪ the 10
        // bare-subject triples (bare values are v1==v2 here, stable).
        let old_set = canon_set_of_graph(&store_old, &graph);
        let new_set = canon_set_of_graph(&store_new, &graph);
        let bare = expected_bare_metadata_canon(&rec_v2);
        let mut expected = old_set.clone();
        expected.extend(bare.iter().cloned());
        assert_eq!(
            new_set, expected,
            "UPDATE: per-doc graph = (OLD tree footprint) ∪ (10 bare-subject triples)"
        );
        assert!(
            old_set.is_subset(&new_set),
            "UPDATE: the OLD tree footprint is reproduced byte-for-byte"
        );

        // IMPROVEMENT: the diffed path emits FEWER ops than the wholesale rebuild,
        // and only a small delta (one block's text changed → its textContent +
        // the text node's content triple; the 10 bare triples are unchanged → 0 ops).
        assert!(
            ops_new < ops_old,
            "diffed update emits fewer ops ({ops_new}) than wholesale rebuild ({ops_old})"
        );
        assert!(
            ops_new <= 6,
            "a single-block edit (bare metadata stable) touches only a small delta, got {ops_new}"
        );

        // CONVERGENCE: a second reconcile to the SAME v2 emits zero ops and the
        // graph is byte-stable.
        let before = canon_set_of_graph(&store_new, &graph);
        let d_converge = reconcile_document_record(&store_new, &rec_v2).expect("converge");
        assert_eq!(
            d_converge.op_count(),
            0,
            "a converged reconcile emits zero ops"
        );
        assert_eq!(
            before,
            canon_set_of_graph(&store_new, &graph),
            "the graph is byte-stable across an identical re-reconcile"
        );
    }

    // ── edge case: tree:None (empty tree) ───────────────────────────────────

    #[test]
    fn empty_tree_old_delete_only_equals_new_removes_only() {
        let graph = document_projection_graph_iri(GRAPH, DOC);
        let rec_v1 = document_record_with_text("seeded content");
        let rec_none = document_record_empty_tree();

        let store_old = Store::new().expect("store A");
        let store_new = Store::new().expect("store B");
        // Seed both to a populated v1.
        materialize_document_record_with_triples(
            &store_old,
            &rec_v1,
            &document_tree_triples(&rec_v1),
        )
        .expect("seed A");
        materialize_document_record_with_triples(
            &store_new,
            &rec_v1,
            &document_tree_triples(&rec_v1),
        )
        .expect("seed B");

        // OLD: empty-tree branch = DELETE-only (no bare written). NEW: the tree
        // face's desired=∅ ⇒ it removes the seeded tree; the bare faces STILL emit
        // their 10 triples (a record always carries them — body/tiptapXml="" and
        // rdfTripleCount=0 for an empty doc).
        materialize_document_record_with_triples(
            &store_old,
            &rec_none,
            &document_tree_triples(&rec_none),
        )
        .expect("old empty");
        let ops = reconcile_document_record(&store_new, &rec_none)
            .expect("new empty")
            .op_count();
        assert!(
            ops > 0,
            "clearing the tree removes the seeded triples (and adds metadata)"
        );

        let old_set = canon_set_of_graph(&store_old, &graph);
        let new_set = canon_set_of_graph(&store_new, &graph);
        assert!(
            old_set.is_empty(),
            "OLD empty-tree path leaves an empty graph"
        );
        let bare = expected_bare_metadata_canon(&rec_none);
        assert_eq!(
            new_set, bare,
            "empty-tree: NEW graph = the OLD cleaned (empty) tree ∪ the 10 bare-subject triples"
        );
        // Faithful empty-doc values: body="", tiptapXml="", rdfTripleCount=0.
        let subject = &rec_none.rdf_subject;
        for (pred, expect_empty) in [("body", true), ("tiptapXml", true)] {
            let rows = run_sparql_rows(
                &store_new,
                &format!(
                    "SELECT ?o WHERE {{ GRAPH <{graph}> {{ <{subject}> <{MNEMO_NS}{pred}> ?o }} }}"
                ),
            );
            assert_eq!(rows.len(), 1, "{pred} is emitted for the empty doc");
            if expect_empty {
                assert!(
                    rows[0].contains("\"\""),
                    "empty-doc {pred} is the empty string literal, got {:?}",
                    rows[0]
                );
            }
        }
        let count_rows = run_sparql_rows(
            &store_new,
            &format!(
                "SELECT ?o WHERE {{ GRAPH <{graph}> {{ <{subject}> <{MNEMO_NS}rdfTripleCount> ?o }} }}"
            ),
        );
        assert_eq!(count_rows.len(), 1);
        assert!(
            count_rows[0].contains("\"0\""),
            "empty-doc rdfTripleCount = 0, got {:?}",
            count_rows[0]
        );
    }

    // ── reclaim invariant: a stale FS-sourced bare predicate converges ───────

    /// The storage-metadata face OWNS the 7 FS-sourced bare predicates, so a stale
    /// value (or a stale extra triple) on one of them gets diffed to convergence:
    /// the wrong value is REMOVED and the correct record value is the only one left.
    #[test]
    fn stale_fs_sourced_bare_predicate_converges() {
        let graph = document_projection_graph_iri(GRAPH, DOC);
        let subject = document_subject(DOC);
        let rec = document_record_with_text("content");

        let store = Store::new().expect("store");
        // Reconcile the full footprint (tree + the 7 correct bare metadata triples).
        reconcile_document_record(&store, &rec).expect("reconcile");

        // Seed a STALE value for an FS-sourced predicate the face owns.
        run_document_update(
            &store,
            GRAPH,
            DOC,
            &format!("INSERT DATA {{ <{subject}> <{MNEMO_NS}localPath> \"/wrong/stale/path\" }}"),
        )
        .expect("seed stale fs-sourced predicate");
        assert_eq!(
            count(
                &store,
                &format!(
                    "SELECT ?o WHERE {{ GRAPH <{graph}> {{ <{subject}> <{MNEMO_NS}localPath> ?o }} }}"
                ),
            ),
            2,
            "the stale value coexists with the correct one before reconcile"
        );

        // Reconciling must reclaim the stale value (the face owns localPath) and
        // leave ONLY the correct record value.
        reconcile_document_record(&store, &rec).expect("reconcile converges");
        let rows = run_sparql_rows(
            &store,
            &format!(
                "SELECT ?o WHERE {{ GRAPH <{graph}> {{ <{subject}> <{MNEMO_NS}localPath> ?o }} }}"
            ),
        );
        assert_eq!(rows.len(), 1, "exactly one localPath after convergence");
        assert!(
            rows[0].contains(&rec.local_path),
            "the surviving localPath is the correct record value, got {:?}",
            rows[0]
        );

        // The CONTENT predicate `body` IS now owned by this MO (the content face):
        // seeding a stale value gets reclaimed and converges to the face's desired
        // (here the synthetic record's body is "", so the stale extra is removed).
        run_document_update(
            &store,
            GRAPH,
            DOC,
            &format!("INSERT DATA {{ <{subject}> <{MNEMO_NS}body> \"content-face-territory\" }}"),
        )
        .expect("seed stale content predicate");
        reconcile_document_record(&store, &rec).expect("reconcile reclaims stale content");
        let body_rows = run_sparql_rows(
            &store,
            &format!("SELECT ?o WHERE {{ GRAPH <{graph}> {{ <{subject}> <{MNEMO_NS}body> ?o }} }}"),
        );
        assert_eq!(
            body_rows.len(),
            1,
            "the content `body` predicate IS owned by this MO — exactly the desired value remains"
        );
        assert!(
            !body_rows[0].contains("content-face-territory"),
            "the stale body value is reclaimed by the content face, got {:?}",
            body_rows[0]
        );

        // A predicate OUTSIDE the 10 DOCUMENT_LEVEL_PREDICATES (e.g. a salience-face
        // predicate co-attached to the bare subject) is NOT owned by this MO and
        // survives reconcile untouched — the faces co-manage only their own slots.
        run_document_update(
            &store,
            GRAPH,
            DOC,
            &format!("INSERT DATA {{ <{subject}> <{MNEMO_NS}salienceScore> \"0.42\" }}"),
        )
        .expect("seed foreign-face predicate");
        reconcile_document_record(&store, &rec).expect("reconcile leaves foreign slots alone");
        assert_eq!(
            count(
                &store,
                &format!(
                    "SELECT ?o WHERE {{ GRAPH <{graph}> {{ <{subject}> <{MNEMO_NS}salienceScore> ?o }} }}"
                ),
            ),
            1,
            "a predicate outside DOCUMENT_LEVEL_PREDICATES is NOT owned and survives reconcile"
        );
    }

    // ── named-graph isolation ───────────────────────────────────────────────

    #[test]
    fn reconcile_writes_only_in_the_per_doc_projection_graph() {
        let graph = document_projection_graph_iri(GRAPH, DOC);
        let user_rdf = user_rdf_graph_iri(GRAPH);
        let rec = document_record_with_text("isolation under test");

        let store = Store::new().expect("store");
        reconcile_document_record(&store, &rec).expect("reconcile");

        // Triples ARE in the per-doc projection graph.
        let in_doc = count(
            &store,
            &format!("SELECT ?s WHERE {{ GRAPH <{graph}> {{ ?s ?p ?o }} }}"),
        );
        assert!(
            in_doc > 0,
            "projection triples must be in the per-doc graph"
        );

        // NOT in the default graph.
        let in_default = count(&store, "SELECT ?s WHERE { ?s ?p ?o }");
        assert_eq!(in_default, 0, "nothing in the default graph");

        // NOT in the user:rdf authority graph.
        let in_user_rdf = count(
            &store,
            &format!("SELECT ?s WHERE {{ GRAPH <{user_rdf}> {{ ?s ?p ?o }} }}"),
        );
        assert_eq!(in_user_rdf, 0, "nothing in the :user:rdf graph");

        // Belt-and-suspenders: every quad is in the per-doc graph.
        let elsewhere = count(
            &store,
            &format!("SELECT ?s WHERE {{ GRAPH ?g {{ ?s ?p ?o }} FILTER(?g != <{graph}>) }}"),
        );
        assert_eq!(
            elsewhere, 0,
            "the document sink wrote outside its per-doc graph"
        );
    }
}

/// REAL-HARNESS parity oracle.
///
/// Unlike the `tests` module above (which hand-builds a `DocumentRecord` + tree),
/// this drives the FULL gardend cell through the documented test harness:
/// `build_mock_app_for_tests(true)` → `create_graph_service` (a real seeded graph
/// on a temp profile) → `enqueue_crdt_operation("document.write", <markdown>)` (the
/// real CRDT engine mints the Y.Doc, `save_document` runs the OLD materializer into
/// the real on-disk per-doc projection graph) → `read_document_record` (a REAL
/// record with a REAL populated tree).
///
/// It then proves NET-STATE parity between the OLD wholesale materializer (read
/// back from the real graph via `run_sparql_query_service`) and the NEW diffed
/// `reconcile_document_record`, for FRESH and UPDATE, plus the improvement
/// (NEW emits strictly fewer DELETE/INSERT ops than the old wholesale rebuild) and
/// zero-ops convergence. NO MOCKS — every layer is the real cell.
#[cfg(all(test, feature = "headless"))]
mod harness_tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use serde_json::json;

    use crate::app_runtime::AppHandle;
    use crate::crdt_operation_types::EnqueueCrdtOperationInput;
    use crate::crdt_queue::enqueue_crdt_operation;
    use crate::document_service::DocumentRecord;
    use crate::emporium::terms::{canon_value, CanonValue};
    use crate::graph_service::{create_graph_service, CreateGraphInput};
    use crate::rdf_record_materializer::materialize_document_record_with_triples;
    use crate::rdf_service::{run_sparql_query_service, SparqlInput};

    /// `GARDEN_PROFILE_DIR` is process-global; serialize the headless harness tests
    /// (across ALL modules) so they cannot stomp each other's profile/env.
    fn env_serial() -> &'static std::sync::Mutex<()> {
        crate::tauri_runtime::profile_env_serial()
    }

    fn temp_profile(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        std::env::temp_dir().join(format!("garden-document-mo-{name}-{nanos}"))
    }

    /// Build a MockRuntime app handle with the CRDT queue + room registry managed,
    /// so `enqueue_crdt_operation` drains through the in-process headless executor.
    fn mock_app() -> AppHandle {
        crate::tauri_runtime::build_mock_app_for_tests(true)
    }

    fn seed_graph(app: &AppHandle, graph_id: &str) {
        create_graph_service(
            app,
            CreateGraphInput {
                graph_id: Some(graph_id.to_string()),
                title: "Lab".to_string(),
                description: None,
                operation_id: None,
            },
        )
        .expect("create graph");
    }

    /// Write `markdown` into `doc_id` through the REAL CRDT engine (the same
    /// `document.write` op the spine/applier enqueue). The headless executor mints
    /// the Y.Doc, materializes the tree, and `save_document` persists the record +
    /// runs the OLD wholesale materializer into the real per-doc projection graph.
    fn write_doc(app: &AppHandle, graph_id: &str, doc_id: &str, markdown: &str) {
        crate::app_runtime::async_runtime::block_on(enqueue_crdt_operation(
            app.clone(),
            EnqueueCrdtOperationInput {
                kind: "document.write".to_string(),
                graph_id: graph_id.to_string(),
                document_id: Some(doc_id.to_string()),
                payload: json!({
                    "documentId": doc_id,
                    "content": markdown,
                    "format": "markdown",
                    "title": "Doc",
                }),
            },
        ))
        .expect("document.write drains through the real CRDT engine");
    }

    /// Read the REAL persisted record (with its REAL populated tree) back through
    /// the same path the spine uses.
    fn read_record(app: &AppHandle, graph_id: &str, doc_id: &str) -> DocumentRecord {
        let graph_dir = crate::paths::existing_graph_dir(app, graph_id).expect("graph dir");
        let dir = crate::paths::document_dir(&graph_dir, doc_id).expect("doc dir");
        let manifest = dir.join("document.json");
        crate::document_record_store::read_document_record(&graph_dir, &manifest).expect("record")
    }

    /// Canon-set of the per-doc projection graph as it lives in the REAL graph
    /// store, read back through the harness `run_sparql_query_service`. Each row's
    /// object is the N-Triples string `run_sparql_query_service` returns (`<uri>` /
    /// `"lit"` / `"lit"^^<dt>`), parsed via `parse_term` and canonicalized via
    /// `canon_value` — the SAME value-canonical equality the planner-parity test
    /// uses, robust to store round-trip normalization.
    fn canon_set_from_harness(
        app: &AppHandle,
        graph_id: &str,
        doc_id: &str,
    ) -> BTreeSet<(String, String, CanonValue)> {
        let graph = document_projection_graph_iri(graph_id, doc_id);
        let query = format!("SELECT ?s ?p ?o WHERE {{ GRAPH <{graph}> {{ ?s ?p ?o }} }}");
        let result = run_sparql_query_service(
            app.clone(),
            SparqlInput {
                graph_id: graph_id.to_string(),
                query,
            },
        )
        .expect("harness sparql query");
        result
            .rows
            .iter()
            .map(|row| {
                // `run_sparql_query_service` serializes ?s/?p as full N-Triples
                // terms (`<uri>`); the in-store reader yields BARE IRIs. Normalize
                // to bare so the two read paths compare as the SAME RDF state — the
                // angle brackets are a serialization artifact of the read path, not
                // a difference in the graph.
                let s = strip_angle(row.get("s").expect("?s"));
                let p = strip_angle(row.get("p").expect("?p"));
                let o = parse_term(row.get("o").expect("?o"));
                (s, p, canon_value(&o))
            })
            .collect()
    }

    /// Strip a leading `<` … trailing `>` from a serialized IRI term; pass other
    /// shapes through unchanged.
    fn strip_angle(s: &str) -> String {
        s.strip_prefix('<')
            .and_then(|rest| rest.strip_suffix('>'))
            .unwrap_or(s)
            .to_string()
    }

    /// Expected canon-set of the 7 FS-sourced bare-`<subject>` storage-metadata
    /// triples, built from the REAL persisted record fields — an oracle INDEPENDENT
    /// of `document_storage_metadata_triples`.
    fn expected_storage_metadata_canon(
        rec: &DocumentRecord,
    ) -> BTreeSet<(String, String, CanonValue)> {
        let s = rec.rdf_subject.clone();
        let lit =
            |v: &str| canon_value(&Term::Lit(oxigraph::model::Literal::new_simple_literal(v)));
        let int = |v: i64| {
            canon_value(&Term::Lit(oxigraph::model::Literal::new_typed_literal(
                v.to_string(),
                oxigraph::model::vocab::xsd::INTEGER,
            )))
        };
        let mut set = BTreeSet::new();
        set.insert((s.clone(), format!("{MNEMO_NS}graphId"), lit(&rec.graph_id)));
        set.insert((s.clone(), format!("{MNEMO_NS}origin"), lit(&rec.origin)));
        set.insert((
            s.clone(),
            format!("{MNEMO_NS}providerId"),
            lit(&rec.provider_id),
        ));
        set.insert((
            s.clone(),
            format!("{MNEMO_NS}localPath"),
            lit(&rec.local_path),
        ));
        set.insert((
            s.clone(),
            format!("{MNEMO_NS}documentId"),
            lit(&rec.document_id),
        ));
        set.insert((
            s.clone(),
            format!("{MNEMO_NS}schemaVersion"),
            int(rec.schema_version as i64),
        ));
        set.insert((
            s,
            format!("{MNEMO_NS}ydocStatePath"),
            lit(&rec.ydoc_state_path),
        ));
        set
    }

    /// Expected canon-set of the 2 CONTENT bare-`<subject>` triples
    /// (`body`/`tiptapXml`), built from the REAL persisted record fields — the
    /// values the canonical CRDT projection produced (`document.body` =
    /// `materialize_ydoc(doc).body`, `document.tiptap_xml` = `ydoc_to_tiptap_xml(doc)`).
    fn expected_content_canon(rec: &DocumentRecord) -> BTreeSet<(String, String, CanonValue)> {
        let s = rec.rdf_subject.clone();
        let lit =
            |v: &str| canon_value(&Term::Lit(oxigraph::model::Literal::new_simple_literal(v)));
        let mut set = BTreeSet::new();
        set.insert((s.clone(), format!("{MNEMO_NS}body"), lit(&rec.body)));
        set.insert((s, format!("{MNEMO_NS}tiptapXml"), lit(&rec.tiptap_xml)));
        set
    }

    /// Expected canon-set of the 1 projection-meta bare-`<subject>` triple
    /// (`rdfTripleCount`), built from `|document_tree_triples|` over the REAL tree —
    /// an oracle INDEPENDENT of the emitter.
    fn expected_projection_meta_canon(
        rec: &DocumentRecord,
    ) -> BTreeSet<(String, String, CanonValue)> {
        let s = rec.rdf_subject.clone();
        let int = |v: i64| {
            canon_value(&Term::Lit(oxigraph::model::Literal::new_typed_literal(
                v.to_string(),
                oxigraph::model::vocab::xsd::INTEGER,
            )))
        };
        let mut set = BTreeSet::new();
        set.insert((
            s,
            format!("{MNEMO_NS}rdfTripleCount"),
            int(document_tree_triples(rec).len() as i64),
        ));
        set
    }

    /// All 10 bare-subject triples the COMBINED MO adds beyond the tree (7 fs + 2
    /// content + 1 meta), built from the REAL persisted record.
    fn expected_bare_metadata_canon(
        rec: &DocumentRecord,
    ) -> BTreeSet<(String, String, CanonValue)> {
        let mut set = expected_storage_metadata_canon(rec);
        set.extend(expected_content_canon(rec));
        set.extend(expected_projection_meta_canon(rec));
        set
    }

    /// Canon-set of the per-doc projection graph in a standalone in-memory store
    /// (the clean target the NEW reconcile path writes into).
    fn canon_set_from_store(
        store: &Store,
        graph_id: &str,
        doc_id: &str,
    ) -> BTreeSet<(String, String, CanonValue)> {
        let graph = document_projection_graph_iri(graph_id, doc_id);
        let query = format!("SELECT ?s ?p ?o WHERE {{ GRAPH <{graph}> {{ ?s ?p ?o }} }}");
        let solutions = match SparqlEvaluator::new()
            .parse_query(&query)
            .expect("parse canon query")
            .on_store(store)
            .execute()
            .expect("execute canon query")
        {
            QueryResults::Solutions(s) => s,
            _ => panic!("expected solutions"),
        };
        let mut set = BTreeSet::new();
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
            let o = parse_term(&sol.get("o").expect("?o").to_string());
            set.insert((s, p, canon_value(&o)));
        }
        set
    }

    /// The OLD wholesale op count for a v1→v2 transition: the old DELETE span over
    /// the live (v1) per-doc graph + a full re-INSERT of the v2 tree.
    fn old_wholesale_ops(store: &Store, rec_v1: &DocumentRecord, rec_v2: &DocumentRecord) -> usize {
        let removes = survey_document_projection(
            store,
            &rec_v1.graph_id,
            &rec_v1.document_id,
            &rec_v1.rdf_subject,
        )
        .expect("survey old delete span")
        .len();
        let adds = document_tree_triples(rec_v2).len();
        removes + adds
    }

    fn with_profile(name: &str, body: impl FnOnce() + std::panic::UnwindSafe) {
        let _serial = env_serial().lock().unwrap_or_else(|p| p.into_inner());
        let profile = temp_profile(name);
        std::env::set_var("GARDEN_PROFILE_DIR", &profile);
        let result = std::panic::catch_unwind(body);
        std::env::remove_var("GARDEN_PROFILE_DIR");
        let _ = std::fs::remove_dir_all(&profile);
        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
    }

    // ── 1. FRESH parity: real OLD graph vs NEW reconcile (clean store) ──────────

    #[test]
    fn harness_fresh_parity_old_vs_new_net_state_identical() {
        with_profile("fresh", || {
            let app = mock_app();
            let graph_id = "lab";
            let doc_id = "doc-fresh";
            seed_graph(&app, graph_id);

            // REAL document.write → real tree → save_document runs the OLD
            // wholesale materializer into the real per-doc projection graph.
            write_doc(
                &app,
                graph_id,
                doc_id,
                "# Title\n\nA real paragraph of body text.\n\nA second paragraph.",
            );
            let rec = read_record(&app, graph_id, doc_id);
            assert!(rec.tree.is_some(), "the real CRDT write produced a tree");
            assert!(
                !document_tree_triples(&rec).is_empty(),
                "the real tree projects triples"
            );
            // GUARD: the 7 FS-sourced fields are genuinely POPULATED in the real
            // persisted record — so the "present and correct" parity below compares
            // against real derived path/manifest values, NOT empty strings (which
            // would make the storage-metadata parity vacuous). graph_id/document_id/
            // origin/provider_id/local_path/ydoc_state_path are non-empty; the
            // integer schema_version is positive.
            for (name, value) in [
                ("graph_id", rec.graph_id.as_str()),
                ("origin", rec.origin.as_str()),
                ("provider_id", rec.provider_id.as_str()),
                ("local_path", rec.local_path.as_str()),
                ("document_id", rec.document_id.as_str()),
                ("ydoc_state_path", rec.ydoc_state_path.as_str()),
            ] {
                assert!(
                    !value.is_empty(),
                    "real persisted record field `{name}` is empty — storage-metadata parity \
                     would be vacuous; the real document.write path must populate it"
                );
            }
            assert!(
                rec.schema_version > 0,
                "real persisted record schema_version must be a positive integer"
            );
            assert!(
                rec.local_path.contains(doc_id) && rec.ydoc_state_path.contains(doc_id),
                "the derived paths must reference the document id (real fs derivation, not a stub)"
            );

            // OLD net-state: read the real on-disk graph back through the harness.
            let old_set = canon_set_from_harness(&app, graph_id, doc_id);
            assert!(
                !old_set.is_empty(),
                "the OLD materializer wrote the projection"
            );

            // NEW net-state: reconcile the SAME real record into a clean store.
            let store_new = Store::new().expect("clean store");
            let ops = reconcile_document_record(&store_new, &rec)
                .expect("new reconcile")
                .op_count();
            assert!(ops > 0, "a fresh reconcile on a clean store writes triples");
            let new_set = canon_set_from_store(&store_new, graph_id, doc_id);

            // GUARD: the content fields are genuinely POPULATED by the REAL CRDT
            // write path (not empty strings) — so the body/tiptapXml faithfulness
            // below is meaningful. The canonical plaintext of a multi-paragraph doc
            // is non-empty, and the real serializer always emits a tiptapXml.
            assert!(
                !rec.body.is_empty(),
                "real persisted record .body (canonical plaintext) is non-empty for a real doc"
            );
            assert!(
                !rec.tiptap_xml.is_empty(),
                "real persisted record .tiptap_xml (canonical XML serialization) is non-empty for a real doc"
            );
            assert!(
                rec.rdf_triple_count > 0,
                "real persisted record .rdf_triple_count is positive for a populated tree"
            );
            // FAITHFULNESS: the cached fields the content face emits ARE the canonical
            // projection — `.rdf_triple_count` (cached at save) equals
            // `|document_tree_triples|` (the value the meta face re-derives).
            assert_eq!(
                rec.rdf_triple_count,
                document_tree_triples(&rec).len(),
                "cached rdf_triple_count == |document_tree_triples| (meta face faithfulness)"
            );

            // FULL footprint: the NEW combined MO reproduces the OLD tree footprint
            // byte-for-byte AND adds ALL 10 bare-subject `DOCUMENT_LEVEL_PREDICATES`
            // (7 FS-sourced + 2 content + 1 projection-meta) the OLD path never wrote
            // (the allowlist never fired). Sourced from the REAL persisted record.
            let bare = expected_bare_metadata_canon(&rec);
            assert_eq!(bare.len(), 10, "the 10 bare DOCUMENT_LEVEL_PREDICATES");
            assert!(
                old_set.is_subset(&new_set),
                "FRESH: the real OLD tree footprint is reproduced byte-for-byte by the NEW MO"
            );
            assert!(
                bare.is_subset(&new_set),
                "FRESH: the NEW MO is the FIRST path to emit the 10 bare-subject triples"
            );
            assert!(
                bare.is_disjoint(&old_set),
                "the 10 bare-subject triples are absent from the real OLD footprint"
            );
            let mut expected = old_set.clone();
            expected.extend(bare.iter().cloned());
            assert_eq!(
                new_set, expected,
                "FRESH: per-doc graph = (real OLD tree footprint) ∪ (10 bare-subject triples)"
            );
        });
    }

    // ── 2. UPDATE parity + improvement + 3. convergence ─────────────────────────

    #[test]
    fn harness_update_parity_improvement_and_convergence() {
        with_profile("update", || {
            let app = mock_app();
            let graph_id = "lab";
            let doc_id = "doc-update";
            seed_graph(&app, graph_id);

            // v1: real write; capture the real v1 record.
            write_doc(
                &app,
                graph_id,
                doc_id,
                "# Heading\n\nThe first version of the paragraph stands here.\n\nShared tail paragraph.",
            );
            let rec_v1 = read_record(&app, graph_id, doc_id);

            // Seed the NEW path's clean store to the FULL v1 footprint via the NEW
            // reconcile (tree + 10 bare). NOTE: unlike the synthetic test, this is a
            // REAL content edit, so `body` and `tiptapXml` DO change v1→v2 (their
            // serializations differ); `rdfTripleCount` is stable (same tree shape).
            // The improvement assertion below tolerates that small content delta.
            let store_new = Store::new().expect("clean store");
            reconcile_document_record(&store_new, &rec_v1).expect("seed NEW store to v1");
            // GUARD: the real edit DID change the content serializations (so the
            // content face's update path is genuinely exercised, not vacuous).
            assert_ne!(rec_v1.body, "", "v1 body populated by real CRDT path");

            // v2: EDIT one paragraph through the REAL CRDT engine. save_document
            // runs the OLD wholesale rebuild into the real on-disk graph.
            write_doc(
                &app,
                graph_id,
                doc_id,
                "# Heading\n\nThe SECOND version of the paragraph stands here.\n\nShared tail paragraph.",
            );
            let rec_v2 = read_record(&app, graph_id, doc_id);

            // OLD wholesale op count for the v1→v2 transition (delete span + full
            // re-insert), measured against the NEW store's v1 state.
            let ops_old = old_wholesale_ops(&store_new, &rec_v1, &rec_v2);

            // NEW: diffed reconcile v1→v2 against the populated store.
            let ops_new = reconcile_document_record(&store_new, &rec_v2)
                .expect("new v2")
                .op_count();

            // UPDATE FAITHFULNESS: the real edit changed the content serializations
            // (body/tiptapXml differ v1→v2) while the tree shape (and thus
            // rdfTripleCount) is stable. The content face faithfully tracks the edit.
            assert_ne!(
                rec_v1.body, rec_v2.body,
                "UPDATE: the edit changed the canonical plaintext body"
            );
            assert_ne!(
                rec_v1.tiptap_xml, rec_v2.tiptap_xml,
                "UPDATE: the edit changed the canonical tiptapXml serialization"
            );

            // NET STATE: the real OLD graph (after its v2 wholesale rebuild, tree
            // only) ∪ the 10 bare-subject triples == the NEW diffed store. The tree
            // subset is byte-identical to the real OLD output, and the bare values
            // track the v2 record.
            let old_set = canon_set_from_harness(&app, graph_id, doc_id);
            let new_set = canon_set_from_store(&store_new, graph_id, doc_id);
            let bare = expected_bare_metadata_canon(&rec_v2);
            assert!(
                old_set.is_subset(&new_set),
                "UPDATE: the real OLD tree footprint is reproduced byte-for-byte"
            );
            let mut expected = old_set.clone();
            expected.extend(bare.iter().cloned());
            assert_eq!(
                new_set, expected,
                "UPDATE: per-doc graph = (real OLD tree footprint) ∪ (10 bare-subject triples)"
            );

            // IMPROVEMENT: the diffed path emits STRICTLY FEWER ops than the
            // wholesale rebuild — proving it diffs, not rebuilds. (The tree delta +
            // the 2 changed content predicates (body/tiptapXml) is still far smaller
            // than re-inserting the whole tree; the 7 fs + rdfTripleCount are stable.)
            assert!(
                ops_new < ops_old,
                "diffed update emits fewer ops ({ops_new}) than wholesale rebuild ({ops_old})"
            );
            assert!(
                ops_new > 0,
                "the v1→v2 edit DID change the tree + content (non-zero delta)"
            );

            // CONVERGENCE: a second reconcile to the SAME v2 emits zero ops and the
            // graph is byte-stable.
            let before = canon_set_from_store(&store_new, graph_id, doc_id);
            let d_converge = reconcile_document_record(&store_new, &rec_v2).expect("converge");
            assert_eq!(
                d_converge.op_count(),
                0,
                "a converged reconcile emits zero ops"
            );
            assert_eq!(
                before,
                canon_set_from_store(&store_new, graph_id, doc_id),
                "the graph is byte-stable across an identical re-reconcile"
            );
        });
    }
}

/// FAITHFULNESS oracle (spec §39).
///
/// The 3 content/projection-meta predicates (`body`, `tiptapXml`,
/// `rdfTripleCount`) are **net-new** in RDF — there is no production write path
/// emitting them, so the prior `harness_tests` module can only prove the NEW MO
/// reproduces the OLD *tree* footprint plus these 3 as additions. That is NOT a
/// faithfulness oracle for the 3 themselves: those tests build the "expected"
/// `body`/`tiptapXml` from `rec.body`/`rec.tiptap_xml` (the SAME cached fields the
/// content face reads) — a tautology.
///
/// This module is the real oracle. For a document built through the REAL CRDT
/// `document.write` path, it computes the canonical projection **independently**
/// of both the MO face AND the record's cached `.body`/`.tiptap_xml` fields:
/// it reconstructs a LIVE yrs `&Doc` from the persisted CRDT authority file
/// (`update-v1.bin`, surfaced as `rec.ydoc_update_base64` by `read_document_record`),
/// then calls Garden's OWN canonical cell-side renderers
/// [`crate::crdt_engine::projection::materialize_ydoc`] (→ `body`) and
/// [`crate::crdt_engine::projection::ydoc_to_tiptap_xml`] (→ `tiptapXml`). It then
/// reconciles the MO into a clean store and asserts the literal the MO actually
/// WROTE is **byte-equal** to that independently-computed canonical value.
///
/// The independent recompute touches NEITHER the face NOR the cached record
/// fields, so a regression where the cached field drifts from the canonical
/// projection (or the face fabricates a value) would FAIL here. NO MOCKS —
/// every layer is the real cell, and the oracle is Garden's own renderer.
#[cfg(all(test, feature = "headless"))]
mod faithfulness_oracle_tests {
    use super::*;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _};
    use serde_json::json;
    use yrs::updates::decoder::Decode;
    use yrs::{Doc, ReadTxn, Transact, Update};

    use crate::app_runtime::AppHandle;
    use crate::crdt_engine::projection::{materialize_ydoc, ydoc_to_tiptap_xml};
    use crate::crdt_operation_types::EnqueueCrdtOperationInput;
    use crate::crdt_queue::enqueue_crdt_operation;
    use crate::document_service::DocumentRecord;

    fn env_serial() -> &'static std::sync::Mutex<()> {
        crate::tauri_runtime::profile_env_serial()
    }

    fn temp_profile(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        std::env::temp_dir().join(format!("garden-document-mo-faith-{name}-{nanos}"))
    }

    fn mock_app() -> AppHandle {
        crate::tauri_runtime::build_mock_app_for_tests(true)
    }

    fn seed_graph(app: &AppHandle, graph_id: &str) {
        crate::graph_service::create_graph_service(
            app,
            crate::graph_service::CreateGraphInput {
                graph_id: Some(graph_id.to_string()),
                title: "Lab".to_string(),
                description: None,
                operation_id: None,
            },
        )
        .expect("create graph");
    }

    fn write_doc(app: &AppHandle, graph_id: &str, doc_id: &str, markdown: &str) {
        crate::app_runtime::async_runtime::block_on(enqueue_crdt_operation(
            app.clone(),
            EnqueueCrdtOperationInput {
                kind: "document.write".to_string(),
                graph_id: graph_id.to_string(),
                document_id: Some(doc_id.to_string()),
                payload: json!({
                    "documentId": doc_id,
                    "content": markdown,
                    "format": "markdown",
                    "title": "Doc",
                }),
            },
        ))
        .expect("document.write drains through the real CRDT engine");
    }

    fn read_record(app: &AppHandle, graph_id: &str, doc_id: &str) -> DocumentRecord {
        let graph_dir = crate::paths::existing_graph_dir(app, graph_id).expect("graph dir");
        let dir = crate::paths::document_dir(&graph_dir, doc_id).expect("doc dir");
        let manifest = dir.join("document.json");
        crate::document_record_store::read_document_record(&graph_dir, &manifest).expect("record")
    }

    /// Reconstruct a LIVE yrs `Doc` from the persisted CRDT authority bytes,
    /// INDEPENDENTLY of the record's cached `.body`/`.tiptap_xml` (it reads only
    /// `ydoc_update_base64`, which `read_document_record` filled from the on-disk
    /// `update-v1.bin`). This is the same hydration `RoomRegistry::get_or_create`
    /// performs, isolated here so the canonical renderers run on a Doc the test
    /// rebuilt — not on any value the MO or the manifest cached.
    fn live_doc_from_record(rec: &DocumentRecord) -> Doc {
        let bytes = BASE64_STANDARD
            .decode(rec.ydoc_update_base64.as_bytes())
            .expect("decode persisted ydoc update base64");
        let doc = Doc::new();
        if !bytes.is_empty() {
            let update = Update::decode_v1(&bytes).expect("decode_v1 persisted ydoc update");
            let mut txn = doc.transact_mut();
            txn.apply_update(update)
                .expect("apply persisted ydoc update");
        }
        doc
    }

    /// Garden's OWN canonical plaintext for the document, computed from the live
    /// Doc via `materialize_ydoc(...).body` — NOT from `rec.body`, NOT from the
    /// face. This is the spec's "compute the expected plaintext independently via
    /// the canonical cell-side renderer."
    fn canonical_body(rec: &DocumentRecord) -> String {
        materialize_ydoc(&live_doc_from_record(rec), &rec.document_id).body
    }

    /// Garden's OWN canonical TipTap/ProseMirror XML for the document, computed
    /// from the live Doc via `ydoc_to_tiptap_xml` — NOT from `rec.tiptap_xml`,
    /// NOT from the face.
    fn canonical_tiptap_xml(rec: &DocumentRecord) -> String {
        ydoc_to_tiptap_xml(&live_doc_from_record(rec))
    }

    /// The single literal value the MO actually WROTE for `<subject> mnemo:{pred}`
    /// in the per-doc projection graph, read back as a plain Rust string (the
    /// literal's lexical value, brackets/quotes/datatype stripped). Asserts exactly
    /// one binding. Read directly from the standalone `store` the MO wrote into.
    fn emitted_literal(store: &Store, graph_id: &str, doc_id: &str, pred: &str) -> String {
        let graph = document_projection_graph_iri(graph_id, doc_id);
        let subject = crate::rdf::document_subject(doc_id);
        let query = format!(
            "SELECT ?o WHERE {{ GRAPH <{graph}> {{ <{subject}> <{MNEMO_NS}{pred}> ?o }} }}"
        );
        let solutions = match SparqlEvaluator::new()
            .parse_query(&query)
            .expect("parse emitted-literal query")
            .on_store(store)
            .execute()
            .expect("execute emitted-literal query")
        {
            QueryResults::Solutions(s) => s,
            _ => panic!("expected solutions"),
        };
        let mut values: Vec<String> = Vec::new();
        for sol in solutions {
            let sol = sol.expect("row");
            match sol.get("o").expect("?o") {
                oxigraph::model::Term::Literal(l) => values.push(l.value().to_string()),
                other => panic!("expected a literal object for {pred}, got {other}"),
            }
        }
        assert_eq!(
            values.len(),
            1,
            "exactly one {pred} literal in the per-doc graph"
        );
        values.pop().unwrap()
    }

    fn with_profile(name: &str, body: impl FnOnce() + std::panic::UnwindSafe) {
        let _serial = env_serial().lock().unwrap_or_else(|p| p.into_inner());
        let profile = temp_profile(name);
        std::env::set_var("GARDEN_PROFILE_DIR", &profile);
        let result = std::panic::catch_unwind(body);
        std::env::remove_var("GARDEN_PROFILE_DIR");
        let _ = std::fs::remove_dir_all(&profile);
        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
    }

    // ── 1+2+3: body, tiptapXml, rdfTripleCount faithful to the canonical
    //          projection of a REAL document (byte-equal, independent oracle) ──

    #[test]
    fn body_tiptap_xml_and_triple_count_byte_equal_canonical_projection() {
        with_profile("populated", || {
            let app = mock_app();
            let graph_id = "lab";
            let doc_id = "doc-faith";
            seed_graph(&app, graph_id);

            // REAL document.write → real Y.Doc → real on-disk per-doc projection.
            write_doc(
                &app,
                graph_id,
                doc_id,
                "# Title\n\nFirst paragraph with **bold** and *italic* text.\n\nA second paragraph for body length.",
            );
            let rec = read_record(&app, graph_id, doc_id);
            assert!(rec.tree.is_some(), "the real CRDT write produced a tree");
            assert!(
                !rec.ydoc_update_base64.is_empty(),
                "the persisted CRDT authority bytes are present (independent-oracle input)"
            );

            // Reconcile the MO into a CLEAN store: this is the ONLY place the 3
            // predicates get written; we read back what the MO actually emitted.
            let store = Store::new().expect("clean store");
            let ops = reconcile_document_record(&store, &rec)
                .expect("reconcile")
                .op_count();
            assert!(ops > 0, "a fresh reconcile writes triples");

            // ── INDEPENDENT ORACLE: recompute the canonical projection from the
            //    live Doc rebuilt from the persisted authority bytes. Touches
            //    neither the MO face nor rec.body/rec.tiptap_xml. ──
            let expected_body = canonical_body(&rec);
            let expected_xml = canonical_tiptap_xml(&rec);
            // Sanity: a real multi-paragraph doc has non-empty canonical forms, so
            // the byte-equality below is a MEANINGFUL check (not "" == "").
            assert!(
                !expected_body.is_empty(),
                "canonical plaintext of a real doc is non-empty"
            );
            assert!(
                !expected_xml.is_empty(),
                "canonical tiptapXml of a real doc is non-empty"
            );
            assert!(
                expected_body.contains("First paragraph")
                    && expected_body.contains("second paragraph"),
                "canonical plaintext carries the real body text, got {expected_body:?}"
            );

            // (1) body == Garden's canonical plaintext (byte-equal).
            let emitted_body = emitted_literal(&store, graph_id, doc_id, "body");
            assert_eq!(
                emitted_body, expected_body,
                "FAITHFUL: emitted mnemo:body is byte-equal to materialize_ydoc(doc).body \
                 (the canonical cell-side plaintext renderer), independent of the cached field"
            );

            // (2) tiptapXml == Garden's canonical XML serialization, BYTE-EQUAL.
            // The serializer EXISTS cell-side (ydoc_to_tiptap_xml) and runs here on
            // the rebuilt live Doc — so for a REAL CRDT-written doc this is a true
            // faithfulness oracle, NOT the documented "" gap (that gap is only for
            // hand-built records whose write path supplied no Doc/tiptapXml).
            //
            // RESOLVED — yrs element-attribute-order nondeterminism. `yrs`
            // `XmlFragment::get_string()` emits XML-element attributes from an
            // unordered map (`Branch.map: HashMap<…>`, per-process RandomState seed),
            // so raw start-tag attribute order depended on the Doc's construction
            // history (incrementally-mutated write Doc vs hydrated-from-update Doc)
            // AND on the process. `ydoc_to_tiptap_xml` now CANONICALIZES attribute
            // order (lexicographic by key) as a post-serialization pass — so it is
            // byte-stable across Doc reconstructions and across processes, and the
            // frontend serializer applies the identical canonical order. tiptapXml is
            // a convergence/diff key, so this byte-equality is the correct oracle.
            let emitted_xml = emitted_literal(&store, graph_id, doc_id, "tiptapXml");
            assert_eq!(
                emitted_xml, expected_xml,
                "FAITHFUL (byte-equal): emitted mnemo:tiptapXml is the canonical \
                 ydoc_to_tiptap_xml(doc) serialization — byte-identical across Doc \
                 reconstructions thanks to canonical attribute order, independent of \
                 the cached field"
            );

            // (3) rdfTripleCount (xsd:integer) == |document_tree_triples| EXACTLY —
            //     tree-only. Cross-check it counts NEITHER the bare metadata NOR
            //     itself: the per-doc graph holds (tree + 10 bare) triples, strictly
            //     MORE than the count value, and the count equals just the tree size.
            let tree_count = document_tree_triples(&rec).len();
            let emitted_count = emitted_literal(&store, graph_id, doc_id, "rdfTripleCount");
            assert_eq!(
                emitted_count,
                tree_count.to_string(),
                "FAITHFUL: emitted mnemo:rdfTripleCount == |document_tree_triples| (tree-only)"
            );
            // It is an xsd:integer literal. Read the typed term back from the SAME
            // standalone `store` the MO wrote into (the N-Triples form carries the
            // `^^<…#integer>` datatype tag).
            let dt_rows = {
                let graph = document_projection_graph_iri(graph_id, doc_id);
                let subject = crate::rdf::document_subject(doc_id);
                let query = format!(
                    "SELECT ?o WHERE {{ GRAPH <{graph}> {{ <{subject}> <{MNEMO_NS}rdfTripleCount> ?o }} }}"
                );
                match SparqlEvaluator::new()
                    .parse_query(&query)
                    .expect("parse datatype query")
                    .on_store(&store)
                    .execute()
                    .expect("exec datatype query")
                {
                    QueryResults::Solutions(s) => s
                        .map(|sol| sol.expect("row").get("o").expect("?o").to_string())
                        .collect::<Vec<_>>(),
                    _ => panic!("solutions"),
                }
            };
            assert_eq!(dt_rows.len(), 1);
            assert!(
                dt_rows[0].contains("integer"),
                "rdfTripleCount is typed xsd:integer, got {:?}",
                dt_rows[0]
            );
            // It does NOT count the bare metadata or itself: total graph size =
            // tree + 10 bare > the count, and the count is exactly the tree slice.
            let total_in_graph = {
                let graph = document_projection_graph_iri(graph_id, doc_id);
                let query = format!("SELECT ?s WHERE {{ GRAPH <{graph}> {{ ?s ?p ?o }} }}");
                match SparqlEvaluator::new()
                    .parse_query(&query)
                    .expect("parse total")
                    .on_store(&store)
                    .execute()
                    .expect("exec total")
                {
                    QueryResults::Solutions(s) => s.count(),
                    _ => panic!("solutions"),
                }
            };
            assert_eq!(
                total_in_graph,
                tree_count + 10,
                "the per-doc graph holds the tree + exactly 10 bare predicates"
            );
            assert!(
                tree_count < total_in_graph,
                "rdfTripleCount ({tree_count}) counts ONLY the tree, NOT the 10 bare nor itself \
                 (total graph = {total_in_graph})"
            );
        });
    }

    // ── 4: FULL FOOTPRINT — disjoint spans, zero-op convergence, edit tracks ──

    #[test]
    fn full_footprint_disjoint_convergent_and_edit_tracks_content() {
        with_profile("footprint", || {
            let app = mock_app();
            let graph_id = "lab";
            let doc_id = "doc-fp";
            seed_graph(&app, graph_id);

            write_doc(
                &app,
                graph_id,
                doc_id,
                "# Heading\n\nVersion ONE of the paragraph.\n\nShared tail.",
            );
            let rec_v1 = read_record(&app, graph_id, doc_id);

            let store = Store::new().expect("clean store");
            reconcile_document_record(&store, &rec_v1).expect("reconcile v1");

            // The combined MO emits the tree + all 10 bare predicates, over DISJOINT
            // spans: every one of the 10 bare DOCUMENT_LEVEL_PREDICATES is present on
            // the bare subject exactly once, and the tree lives under {subject}#.
            let graph = document_projection_graph_iri(graph_id, doc_id);
            let subject = crate::rdf::document_subject(doc_id);
            for pred in crate::rdf_document_tree::DOCUMENT_LEVEL_PREDICATES {
                let n = {
                    let query = format!(
                        "SELECT ?o WHERE {{ GRAPH <{graph}> {{ <{subject}> <{MNEMO_NS}{pred}> ?o }} }}"
                    );
                    match SparqlEvaluator::new()
                        .parse_query(&query)
                        .expect("parse bare-pred")
                        .on_store(&store)
                        .execute()
                        .expect("exec bare-pred")
                    {
                        QueryResults::Solutions(s) => s.count(),
                        _ => panic!("solutions"),
                    }
                };
                assert_eq!(
                    n, 1,
                    "bare predicate `{pred}` present exactly once (disjoint cover)"
                );
            }
            // No bare-subject triple carries a predicate OUTSIDE the 10 (the MO emits
            // ONLY its declared bare vocabulary on the bare subject).
            let stray_bare = {
                let in_set = crate::rdf_document_tree::DOCUMENT_LEVEL_PREDICATES
                    .iter()
                    .map(|p| format!("?p = <{MNEMO_NS}{p}>"))
                    .collect::<Vec<_>>()
                    .join(" || ");
                let query = format!(
                    "SELECT ?p WHERE {{ GRAPH <{graph}> {{ <{subject}> ?p ?o }} FILTER( !( {in_set} ) ) }}"
                );
                match SparqlEvaluator::new()
                    .parse_query(&query)
                    .expect("parse stray")
                    .on_store(&store)
                    .execute()
                    .expect("exec stray")
                {
                    QueryResults::Solutions(s) => s.count(),
                    _ => panic!("solutions"),
                }
            };
            assert_eq!(
                stray_bare, 0,
                "the bare subject carries ONLY the 10 declared predicates"
            );

            // CONVERGENCE: re-reconcile the UNCHANGED doc → ZERO ops.
            let d_converge = reconcile_document_record(&store, &rec_v1).expect("converge");
            assert_eq!(
                d_converge.op_count(),
                0,
                "re-reconcile of the unchanged doc emits zero ops"
            );

            // EDIT: a real content edit changes body + tiptapXml. The tree shape is
            // preserved (same paragraph count), so rdfTripleCount is STABLE — proving
            // it tracks tree SHAPE, not content.
            let body_v1 = emitted_literal(&store, graph_id, doc_id, "body");
            let xml_v1 = emitted_literal(&store, graph_id, doc_id, "tiptapXml");
            let count_v1 = emitted_literal(&store, graph_id, doc_id, "rdfTripleCount");

            write_doc(
                &app,
                graph_id,
                doc_id,
                "# Heading\n\nVersion TWO of the paragraph, now longer.\n\nShared tail.",
            );
            let rec_v2 = read_record(&app, graph_id, doc_id);
            let ops_edit = reconcile_document_record(&store, &rec_v2)
                .expect("reconcile v2")
                .op_count();
            assert!(ops_edit > 0, "the edit is a non-empty delta");

            // Re-verify the edited content is STILL byte-equal to the canonical
            // projection of the edited Doc (independent oracle, post-edit).
            let body_v2 = emitted_literal(&store, graph_id, doc_id, "body");
            let xml_v2 = emitted_literal(&store, graph_id, doc_id, "tiptapXml");
            let count_v2 = emitted_literal(&store, graph_id, doc_id, "rdfTripleCount");
            assert_eq!(
                body_v2,
                canonical_body(&rec_v2),
                "post-edit body is byte-equal to the canonical plaintext of the edited Doc"
            );
            assert_eq!(
                xml_v2,
                canonical_tiptap_xml(&rec_v2),
                "post-edit tiptapXml is byte-equal to the canonical XML of the edited Doc \
                 (canonical attribute order)"
            );
            assert_eq!(
                count_v2,
                document_tree_triples(&rec_v2).len().to_string(),
                "post-edit rdfTripleCount == |document_tree_triples| of the edited tree"
            );

            // The edit DID change body + tiptapXml…
            assert_ne!(
                body_v1, body_v2,
                "the edit changed the canonical plaintext body"
            );
            assert_ne!(xml_v1, xml_v2, "the edit changed the canonical tiptapXml");
            // …and the tree shape (paragraph count) is preserved → rdfTripleCount STABLE.
            assert_eq!(
                count_v1, count_v2,
                "same tree shape ⇒ rdfTripleCount stable (it tracks tree SHAPE, not content)"
            );

            // CONVERGENCE after the edit too.
            let d_converge2 = reconcile_document_record(&store, &rec_v2).expect("converge v2");
            assert_eq!(
                d_converge2.op_count(),
                0,
                "re-reconcile of the edited doc emits zero ops"
            );
        });
    }

    // ── 5: EMPTY doc — body="", tiptapXml = the REAL empty-doc serialization,
    //       and rdfTripleCount faithful to the REAL empty write (a CRITICAL
    //       spec-vs-reality finding on the count) ──

    #[test]
    fn empty_document_body_count_and_real_serialized_empty_tiptap_xml() {
        with_profile("empty", || {
            let app = mock_app();
            let graph_id = "lab";
            let doc_id = "doc-empty";
            seed_graph(&app, graph_id);

            // A REAL write of empty markdown through the CRDT engine — so the Doc is
            // a real (empty-content) Y.Doc, and tiptapXml is whatever the REAL
            // serializer emits for it (not a fabricated/omitted value).
            write_doc(&app, graph_id, doc_id, "");
            let rec = read_record(&app, graph_id, doc_id);

            let store = Store::new().expect("clean store");
            reconcile_document_record(&store, &rec).expect("reconcile empty");

            // INDEPENDENT oracle on the rebuilt live Doc.
            let expected_body = canonical_body(&rec);
            let expected_xml = canonical_tiptap_xml(&rec);
            let tree_count = document_tree_triples(&rec).len();

            // body == "" : the canonical plaintext of an empty-content doc is the
            // empty string (no non-empty paragraph lines to join).
            assert_eq!(
                expected_body, "",
                "canonical plaintext of an empty-content doc is \"\""
            );
            let emitted_body = emitted_literal(&store, graph_id, doc_id, "body");
            assert_eq!(emitted_body, "", "FAITHFUL: empty-doc body emitted as \"\"");
            assert_eq!(
                emitted_body, expected_body,
                "byte-equal to the canonical empty plaintext"
            );

            // CRITICAL SPEC-vs-REALITY FINDING — empty doc is NOT tree=None.
            // The spec (§19, §34, §47) stipulates "empty document (tree = None) →
            // rdfTripleCount = 0". But the REAL `document.write("")` path does NOT
            // yield `tree = None`: the CRDT engine mints a Y.Doc with a DEFAULT empty
            // paragraph block, so `rec.tree` is Some(...) and `document_tree_triples`
            // projects a NON-zero tree (observed: 7 triples — the paragraph element +
            // its block-id/ordering/structure triples). The `tree = None` / count = 0
            // case is reachable ONLY for a SYNTHETIC hand-built record, never via the
            // real write path. The MO is FAITHFUL either way: it emits exactly
            // `|document_tree_triples|`. We assert that real-faithfulness here, and
            // verify the spec's literal tree=None→0 stipulation on a synthetic record
            // below.
            assert!(
                tree_count > 0,
                "FINDING: a REAL empty-content write yields a default-paragraph tree \
                 ({tree_count} triples), NOT tree=None/0 as the spec's empty case assumes"
            );
            let emitted_count = emitted_literal(&store, graph_id, doc_id, "rdfTripleCount");
            assert_eq!(
                emitted_count,
                tree_count.to_string(),
                "FAITHFUL: empty-content rdfTripleCount == |document_tree_triples| (= {tree_count}, \
                 the real default-paragraph tree), tracking the REAL write, not the spec's tree=None"
            );

            // The spec's literal stipulation (tree=None → 0) holds for a SYNTHETIC
            // record whose tree IS None — the only way to actually reach it.
            let mut none_rec = rec.clone();
            none_rec.tree = None;
            assert_eq!(
                document_tree_triples(&none_rec).len(),
                0,
                "the spec's tree=None case: zero tree triples"
            );
            let none_meta = document_projection_meta_triples(&none_rec);
            assert_eq!(none_meta.len(), 1);
            if let (_, _, Term::Lit(l)) = &none_meta[0] {
                assert_eq!(
                    l.value(),
                    "0",
                    "tree=None ⇒ rdfTripleCount = 0 (spec's literal case)"
                );
            } else {
                panic!("rdfTripleCount must be a literal");
            }

            // tiptapXml == the REAL empty-doc serialization. ydoc_to_tiptap_xml of the
            // real empty-content Doc is some concrete XML (e.g. an empty `<paragraph>`),
            // emitted faithfully — NOT the hand-built-record "" gap. Byte-equal thanks
            // to canonical attribute order (the yrs nondeterminism is resolved).
            let emitted_xml = emitted_literal(&store, graph_id, doc_id, "tiptapXml");
            assert_eq!(
                emitted_xml, expected_xml,
                "FAITHFUL (byte-equal): empty-doc tiptapXml is the canonical \
                 ydoc_to_tiptap_xml of the real empty-content Doc (actual output: {expected_xml:?})"
            );
            println!("[INFO] real empty-content tiptapXml = {emitted_xml:?}");
        });
    }
}

// ════════════════════════════════════════════════════════════════════════════
//  EA-2b+ SHACL CONFORMANCE ORACLE — DOCUMENT kind (the structural fork: a
//  RECURSIVE node TREE via mdoc:childNode).
//
//  The REAL document-tree projection (`reconcile_document_record` into a real
//  Oxigraph store) CONFORMS to the SHACL shapes DERIVED from the `emporium-document`
//  vocab contract (`vocab_to_shacl` → one flat closed NodeShape per node-type class).
//  NO MOCKS: real reconcile, real store, real rudof engine. The validated triples are
//  READ BACK out of the persisted `:projection:document` graph (the tree-node face),
//  so the oracle covers the WHOLE materializer→store→shapes path.
//
//  DERIVED vs COMPLEMENT vs SHACL-INEXPRESSIBLE:
//   • DERIVED: the per-node-type field shapes (XmlFragment/Paragraph/TextNode —
//     rdf:type, mnemo:documentId required, the attribute predicates, the childNode
//     edge as sh:nodeKind sh:IRI, sh:closed). The multi-node-class set is just
//     several NodeShapes; vocab_to_shacl handles it (no emitter change).
//   • SHACL-INEXPRESSIBLE in rudof 0.2.12 (no sh:node recursion), stays
//     reconcile-guaranteed + oracle-checked HERE, NOT silently skipped:
//       - the RECURSIVE tree: every mdoc:childNode target must EXIST as a subject in
//         the projection (referential integrity WITHIN the tree — UNLIKE wires). The
//         complement test below asserts this via a SPARQL read-back (no orphan edges).
//   • OUT OF SCOPE (documented): the BARE doc subject <{subject}> (no rdf:type) is
//     written by OTHER faces, not document_tree_triples — no node-shape targets it.
//
//  TEETH: (a) a node MISSING its required mnemo:documentId is REJECTED (sh:minCount);
//         (b) a rogue predicate on a tree node is REJECTED (sh:closed);
//         (c) every childNode target exists (the SHACL-inexpressible tree complement).
// ════════════════════════════════════════════════════════════════════════════
#[cfg(all(test, feature = "headless"))]
mod shacl_document_conformance_oracle {
    use super::*;
    use oxigraph::sparql::{QueryResults, SparqlEvaluator};

    use crate::document_service::{
        DocumentRecord, DocumentTreeSnapshot, TreeNodeAttributes, TreeNodeSnapshot,
    };
    use crate::emporium::contract::document_vocabulary;
    use crate::emporium::shacl_validator::validate_desired;
    use crate::emporium::survey::parse_term as oracle_parse_term;
    use crate::emporium::terms::{Term, Triple as EngineTriple};
    use crate::rdf::document_subject;
    use crate::rdf_authority::document_projection_graph_iri;
    use crate::runtime_config::{
        DOCUMENT_SCHEMA_VERSION, LOCAL_GRAPH_ORIGIN, LOCAL_PROVIDER_ID, MDOC_NS,
    };

    const GRAPH: &str = "graph-doc-shacl";
    const DOC: &str = "doc-shacl";

    /// A real DocumentRecord with a populated, nested tree: a heading block, a
    /// paragraph block (with a text leaf child), exercising XmlFragment + Paragraph
    /// + TextNode node classes AND the recursive mdoc:childNode tree.
    fn document_record() -> DocumentRecord {
        let text_leaf = TreeNodeSnapshot {
            kind: "text".to_string(),
            tag_name: None,
            text_content: Some("hello tree".to_string()),
            attributes: TreeNodeAttributes::default(),
            children: Vec::new(),
        };
        let paragraph = TreeNodeSnapshot {
            kind: "element".to_string(),
            tag_name: Some("paragraph".to_string()),
            text_content: None,
            attributes: TreeNodeAttributes {
                block_id: Some("block-p".to_string()),
                ..TreeNodeAttributes::default()
            },
            children: vec![text_leaf],
        };
        let heading = TreeNodeSnapshot {
            kind: "element".to_string(),
            tag_name: Some("heading".to_string()),
            text_content: None,
            attributes: TreeNodeAttributes {
                block_id: Some("block-h".to_string()),
                level: Some(2),
                ..TreeNodeAttributes::default()
            },
            children: Vec::new(),
        };
        DocumentRecord {
            document_id: DOC.to_string(),
            graph_id: GRAPH.to_string(),
            title: "Doc SHACL".to_string(),
            revision: 1,
            body: String::new(),
            origin: LOCAL_GRAPH_ORIGIN.to_string(),
            provider_id: LOCAL_PROVIDER_ID.to_string(),
            local_path: format!("/tmp/{DOC}"),
            rdf_subject: document_subject(DOC),
            created_at: "1".to_string(),
            updated_at: "2".to_string(),
            capabilities: Vec::new(),
            schema_version: DOCUMENT_SCHEMA_VERSION,
            tiptap_xml: String::new(),
            tiptap_json: None,
            ydoc_update_base64: String::new(),
            ydoc_state_path: String::new(),
            tree: Some(DocumentTreeSnapshot {
                doc_id: DOC.to_string(),
                root: TreeNodeSnapshot {
                    kind: "element".to_string(),
                    tag_name: Some("doc".to_string()),
                    text_content: None,
                    attributes: TreeNodeAttributes::default(),
                    children: vec![heading, paragraph],
                },
            }),
            blocks: Vec::new(),
            rdf_triple_count: 0,
        }
    }

    /// Read the REAL persisted TREE-NODE projection back from `:projection:document`
    /// as engine `Triple`s — scoped to subjects that carry a `mdoc:` node rdf:type
    /// (the tree face; the bare-subject storage-metadata triples have NO rdf:type and
    /// are out of the contract's scope). Bridges each `?o` via `parse_term`.
    fn read_back_tree_projection(store: &Store) -> Vec<EngineTriple> {
        let g = document_projection_graph_iri(GRAPH, DOC);
        // Only subjects whose rdf:type is a mdoc:-namespaced node type (the tree face).
        let query = format!(
            "SELECT ?s ?p ?o WHERE {{ GRAPH <{g}> {{ \
             ?s <{RDF_TYPE}> ?t . FILTER(STRSTARTS(STR(?t), \"{MDOC_NS}\")) . ?s ?p ?o }} }}",
            RDF_TYPE = crate::runtime_config::RDF_TYPE,
        );
        let solutions = match SparqlEvaluator::new()
            .parse_query(&query)
            .expect("parse tree readback")
            .on_store(store)
            .execute()
            .expect("execute tree readback")
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

    /// CONFORMANCE: the REAL document-tree projection conforms to the vocab-derived
    /// per-node-type shapes (fragment + heading/paragraph blocks + text leaf).
    #[test]
    fn shacl_oracle_document_tree_conforms() {
        let store = Store::new().expect("store");
        reconcile_document_record(&store, &document_record()).expect("reconcile document");

        let projection = read_back_tree_projection(&store);
        assert!(!projection.is_empty(), "the tree projected typed nodes");
        // The fragment, both block nodes, and the text leaf are all present and typed.
        let typed_subjects: std::collections::BTreeSet<&str> = projection
            .iter()
            .filter(|(_, p, _)| p == &crate::runtime_config::RDF_TYPE)
            .map(|(s, _, _)| s.as_str())
            .collect();
        assert!(
            typed_subjects.len() >= 4,
            "fragment + heading + paragraph + text leaf are all typed: {typed_subjects:?}"
        );

        let result = validate_desired(&projection, document_vocabulary());
        assert!(
            result.is_ok(),
            "the REAL document-tree projection must conform to the vocab-derived \
             per-node-type shapes (a violation = materializer↔vocab drift): {result:?}"
        );
    }

    /// TEETH #1: a tree node MISSING its required mnemo:documentId is REJECTED
    /// (every node-type shape carries sh:minCount 1 on documentId).
    #[test]
    fn shacl_oracle_document_teeth_missing_required() {
        let store = Store::new().expect("store");
        reconcile_document_record(&store, &document_record()).expect("reconcile document");

        let mut bent = read_back_tree_projection(&store);
        let doc_id_p = format!("{}documentId", crate::runtime_config::MNEMO_NS);
        // Drop documentId from the fragment subject.
        let frag = format!("{}#frag", document_subject(DOC));
        let before = bent.len();
        bent.retain(|(s, p, _)| !(s == &frag && p == &doc_id_p));
        assert!(bent.len() < before, "dropped the fragment's documentId");

        let result = validate_desired(&bent, document_vocabulary());
        assert!(
            result.is_err(),
            "a tree node missing its required mnemo:documentId must be rejected"
        );
        assert!(result.unwrap_err().starts_with("SHACL:"));
    }

    /// TEETH #2: a CLOSED-shape violation — a rogue predicate outside the document
    /// contract on a tree node is rejected (sh:closed true).
    #[test]
    fn shacl_oracle_document_teeth_rogue_predicate() {
        let store = Store::new().expect("store");
        reconcile_document_record(&store, &document_record()).expect("reconcile document");

        let mut bent = read_back_tree_projection(&store);
        let frag = format!("{}#frag", document_subject(DOC));
        bent.push((
            frag,
            "http://example.org/not-in-the-document-contract".to_string(),
            Term::Lit(oxigraph::model::Literal::new_simple_literal("rogue")),
        ));
        let result = validate_desired(&bent, document_vocabulary());
        assert!(
            result.is_err(),
            "a predicate outside the closed document contract must be rejected"
        );
        assert!(result.unwrap_err().starts_with("SHACL:"));
    }

    /// SHACL-INEXPRESSIBLE COMPLEMENT (hand-checked, NOT silently skipped): the
    /// RECURSIVE tree integrity. rudof 0.2.12 has no sh:node recursion, so this is
    /// asserted directly via a SPARQL read-back of the PERSISTED store: every
    /// mdoc:childNode target IS a subject in the projection (no orphan edges) — the
    /// tree's referential integrity (UNLIKE wires, where dangling is allowed).
    #[test]
    fn shacl_complement_document_childnode_targets_exist() {
        let store = Store::new().expect("store");
        reconcile_document_record(&store, &document_record()).expect("reconcile document");
        let g = document_projection_graph_iri(GRAPH, DOC);

        // A childNode edge whose target is NOT a subject anywhere in the graph would
        // be an orphan. ASK for one; it must NOT exist.
        let q = format!(
            "ASK {{ GRAPH <{g}> {{ ?parent <{MDOC_NS}childNode> ?child . \
             FILTER NOT EXISTS {{ GRAPH <{g}> {{ ?child ?cp ?co }} }} }} }}"
        );
        let has_orphan = match SparqlEvaluator::new()
            .parse_query(&q)
            .expect("parse orphan ask")
            .on_store(&store)
            .execute()
            .expect("execute orphan ask")
        {
            QueryResults::Boolean(b) => b,
            _ => panic!("expected ASK boolean"),
        };
        assert!(
            !has_orphan,
            "INVARIANT (SHACL-inexpressible): every mdoc:childNode target exists as a \
             subject — the tree has NO orphan edges (referential integrity WITHIN the tree)"
        );

        // And the tree is actually exercised: at least 2 childNode edges (frag→2 blocks
        // + paragraph→text leaf).
        let qc = format!(
            "SELECT (COUNT(*) AS ?n) WHERE {{ GRAPH <{g}> {{ ?p <{MDOC_NS}childNode> ?c }} }}"
        );
        let edges = match SparqlEvaluator::new()
            .parse_query(&qc)
            .expect("parse edge count")
            .on_store(&store)
            .execute()
            .expect("execute edge count")
        {
            QueryResults::Solutions(mut s) => {
                let row = s.next().expect("row").expect("sol");
                row.get("n").expect("?n").to_string()
            }
            _ => panic!("expected solutions"),
        };
        let n: i64 = edges
            .split('"')
            .nth(1)
            .and_then(|d| d.parse().ok())
            .unwrap_or(-1);
        assert!(
            n >= 3,
            "the recursive tree has >=3 childNode edges (exercised): {n}"
        );
    }
}
