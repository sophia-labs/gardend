use crate::{
    emporium::contract::workspace_vocabulary,
    emporium::reconcile::{reconcile_classes_validated, ClassScope, Placement, SpanKey},
    emporium::terms::{Triple, TripleDiff},
    rdf::format_rdf_triple,
    rdf_authority::workspace_projection_graph_iri,
    rdf_record_materializer::rdf_triple_to_term,
    rdf_workspace_materializer::{workspace_entity_triples, workspace_snapshot_triples},
    runtime_config::{DCTERMS_NS, MDOC_NS, NFO_NS, NIE_NS, RDF_TYPE, WIRE_NS},
};
use oxigraph::{sparql::SparqlEvaluator, store::Store};
use std::collections::HashMap;

/// The OLD wholesale teardown-and-rebuild path (type-keyed DELETE of every
/// Folder/Artifact/Wire/TipTapDocument span, then full re-INSERT of
/// `workspace_snapshot_triples` incl. the ontology block). As of the Layer-1 flip
/// the live save sites route through [`reconcile_workspace_snapshot`]; this STAYS
/// as the P3a oracle baseline (the equivalence/domination tests run it against the
/// reconcile path on twin stores), so it is `dead_code` in a non-test build —
/// annotated, not deleted: it is the ground truth the reconcile path is proven against.
#[cfg_attr(not(test), allow(dead_code))]
pub(super) fn materialize_workspace_snapshot(
    store: &Store,
    graph_id: &str,
    snapshot: &serde_json::Value,
) -> Result<(), String> {
    let authority_graph = workspace_projection_graph_iri(graph_id);
    let triples = workspace_snapshot_triples(graph_id, snapshot);
    let insert = triples
        .iter()
        .map(format_rdf_triple)
        .collect::<Vec<_>>()
        .join("\n  ");
    let insert_block = if insert.is_empty() {
        String::new()
    } else {
        format!(
            r#";
INSERT DATA {{
  GRAPH <{authority_graph}> {{
  {insert}
  }}
}}"#
        )
    };
    let update = format!(
        r#"
PREFIX rdf: <{RDF_TYPE_PREFIX}>
PREFIX doc: <{MDOC_NS}>
PREFIX dcterms: <{DCTERMS_NS}>
PREFIX nfo: <{NFO_NS}>
PREFIX nie: <{NIE_NS}>
PREFIX mnemo: <{WIRE_NS}>

DELETE {{
  GRAPH <{authority_graph}> {{
  ?folder ?folder_p ?folder_o .
  }}
}}
WHERE {{
  GRAPH <{authority_graph}> {{
  ?folder a doc:Folder .
  ?folder ?folder_p ?folder_o .
  }}
}};
DELETE {{
  GRAPH <{authority_graph}> {{
  ?artifact ?artifact_p ?artifact_o .
  }}
}}
WHERE {{
  GRAPH <{authority_graph}> {{
  ?artifact a doc:Artifact .
  ?artifact ?artifact_p ?artifact_o .
  }}
}};
DELETE {{
  GRAPH <{authority_graph}> {{
  ?wire ?wire_p ?wire_o .
  }}
}}
WHERE {{
  GRAPH <{authority_graph}> {{
  ?wire a mnemo:Wire .
  ?wire ?wire_p ?wire_o .
  }}
}};
DELETE {{
  GRAPH <{authority_graph}> {{
  ?doc rdf:type doc:TipTapDocument .
  ?doc ?doc_p ?doc_o .
  }}
}}
WHERE {{
  GRAPH <{authority_graph}> {{
  OPTIONAL {{ ?doc rdf:type doc:TipTapDocument . }}
  OPTIONAL {{
    ?doc ?doc_p ?doc_o .
    VALUES ?doc_p {{
      dcterms:title
      dcterms:description
      nfo:belongsToContainer
      doc:order
      doc:section
      doc:createdAt
      doc:updatedAt
      doc:lastAccessedAt
      doc:describedAt
      doc:readOnly
      doc:sourceStorageKey
      doc:sourceOriginalFilename
      doc:sourceMimeType
      doc:sourceContentSize
      doc:sourceFileType
    }}
  }}
  }}
}}{insert_block}
"#,
        RDF_TYPE_PREFIX = "http://www.w3.org/1999/02/22-rdf-syntax-ns#",
    );
    SparqlEvaluator::new()
        .parse_update(&update)
        .map_err(|error| format!("parse workspace materialization update: {error}"))?
        .on_store(store)
        .execute()
        .map_err(|error| format!("materialize workspace metadata: {error}"))
}

// ============================================================================
// WORKSPACE RECONCILE — the multi-class instantiation of `reconcile_classes`
// (mirrors the SONG template: union of `Fixed` rdf:type spans, survey-all-then-
// apply-once). Replaces the wholesale teardown-and-rebuild
// `materialize_workspace_snapshot` at the LIVE save sites; the wholesale path is
// retained as the P3 oracle baseline.
//
// The class set (10 spans) — each carries an in-store `rdf:type` so the survey
// collects it cleanly (prereq-B match-the-proof, reconcile-class-design §Forks):
//   • 4 ENTITY classes:  mdoc:Folder, mdoc:Artifact, mdoc:TipTapDocument, mnemo:Wire
//   • 6 SCENE classes:   mdoc:SceneProjection, mdoc:SceneAnchor, mdoc:SceneFrame,
//                        mdoc:SceneText, mdoc:SceneWireCandidate, mdoc:SceneDiagnostic
//
// PREREQ-B (the off-class subgraph) is resolved two ways, per the design ruling:
//   (1) SCENE family — each `#scene*` subject carries its OWN `mdoc:Scene*` type,
//       so the six scene classes are FIRST-CLASS surveyed spans. Without them the
//       artifact span (keyed `mdoc:Artifact`) would never collect the scene
//       children, `workspace_entity_triples` would re-emit them every reconcile,
//       and CONVERGENCE would fail forever (perpetual ADDs). Surveying them gives
//       full convergence parity (a mutated scene reconciles by minimal delta).
//   (2) ONTOLOGY block — the `rdfs:subClassOf` class-level triples (subjects are
//       class IRIs with NO `rdf:type`) match NO span; routed to a once-at-seed
//       idempotent INSERT (NEVER reclaimed), matching the wholesale path (which
//       re-INSERTs the same graph-invariant set each save). NOT a survey span.
// ============================================================================

/// The 4 ENTITY + 6 SCENE class IRIs whose UNION is the reconciled workspace MO,
/// in a fixed order (the partition + scopes zip on this order).
fn workspace_class_iris() -> [String; 10] {
    [
        format!("{MDOC_NS}Folder"),
        format!("{MDOC_NS}Artifact"),
        format!("{MDOC_NS}TipTapDocument"),
        format!("{WIRE_NS}Wire"),
        format!("{MDOC_NS}SceneProjection"),
        format!("{MDOC_NS}SceneAnchor"),
        format!("{MDOC_NS}SceneFrame"),
        format!("{MDOC_NS}SceneText"),
        format!("{MDOC_NS}SceneWireCandidate"),
        format!("{MDOC_NS}SceneDiagnostic"),
    ]
}

/// The 10 workspace [`ClassScope`]s — one per `Fixed` rdf:type class, in
/// [`workspace_class_iris`] order. ALL share the ONE `:projection:workspace`
/// named placement (required by [`reconcile_classes`]'s apply-once). NO
/// `graph_id_conjunct`: the named-graph IRI is already per-cell graph-scoped (one
/// gardend cell owns one graph = one `:projection:workspace`), and workspace
/// entities carry no `mnemo:graphId` triple — byte-for-byte the wholesale DELETE
/// spans, which are also free `?p` over each type within the named graph.
fn workspace_scopes(graph_id: &str) -> [ClassScope; 10] {
    let authority_graph = workspace_projection_graph_iri(graph_id);
    workspace_class_iris().map(|rdf_type| ClassScope {
        placement: Placement::Named(authority_graph.clone()),
        key: SpanKey::Fixed { rdf_type },
        graph_id_conjunct: None,
        subjects: None,
    })
}

/// `project(source) -> desired` for the WORKSPACE MO: the per-snapshot instance
/// triples ([`workspace_entity_triples`] — folders/docs/artifacts+scene/wires,
/// EXCLUDING the once-at-seed ontology block), bridged `RdfTriple -> Triple`
/// through the SAME `rdf_triple_to_term` round-trip the other MOs use (MANDATORY
/// so the bridged `desired` serializes byte-identically to what `survey_class`
/// reparses out of the store — else convergence breaks on a serialization
/// mismatch).
fn workspace_desired(graph_id: &str, snapshot: &serde_json::Value) -> Vec<Triple> {
    workspace_entity_triples(graph_id, snapshot)
        .iter()
        .map(rdf_triple_to_term)
        .collect()
}

/// Partition `desired` into the 10 class subsets by each subject's
/// rdf:type-AS-DECLARED-IN-DESIRED, in [`workspace_class_iris`] order.
///
/// CRITICAL (the spurious-ADD trap, from the SONG instantiation): each class span
/// surveys ONLY its own subjects, so each `class_diff` MUST be fed ONLY its
/// class's triples — else a sister class's triples read as ADDs against this
/// class's (correctly disjoint) survey. We build a subject -> class-index map from
/// the `rdf:type` triples in `desired` (the bridged type object renders `<iri>`),
/// then route every triple by its subject's class.
///
/// ARTIFACT DUAL-TYPE NOTE: an artifact carries BOTH `rdf:type mdoc:Artifact` AND
/// its kind class (`rdf:type mdoc:Image`/…). Only `mdoc:Artifact` is in the iris
/// list, so the kind-class rdf:type never re-routes the subject — the artifact
/// subject maps to the Artifact span, and its kind-class triple rides along
/// (subject-keyed), exactly as the wholesale `?artifact a doc:Artifact ; ?p ?o`
/// span reclaims it.
///
/// OFF-CLASS DROP: a subject whose rdf:type is NOT in the list (the ontology
/// class subjects — they have no rdf:type at all) routes to NO subset and is
/// dropped here. That is CORRECT: the ontology is seeded once, never reconciled.
fn partition_workspace_desired(desired: &[Triple]) -> [Vec<Triple>; 10] {
    let iris = workspace_class_iris();
    let mut subject_class: HashMap<String, usize> = HashMap::new();
    for (s, p, o) in desired {
        if p == RDF_TYPE {
            let object_nt = o.as_nt();
            let type_iri = object_nt
                .strip_prefix('<')
                .and_then(|rest| rest.strip_suffix('>'))
                .unwrap_or(object_nt.as_str());
            if let Some(idx) = iris.iter().position(|iri| iri == type_iri) {
                subject_class.insert(s.clone(), idx);
            }
        }
    }

    let mut subsets: [Vec<Triple>; 10] = Default::default();
    for triple in desired {
        if let Some(&idx) = subject_class.get(&triple.0) {
            subsets[idx].push(triple.clone());
        }
    }
    subsets
}

/// Seed the artifact-kind ONTOLOGY block (the `rdfs:subClassOf` + capability
/// class-level triples) into `:projection:workspace` with an idempotent INSERT
/// DATA — NEVER a DELETE (prereq-B ruling §2: the ontology is graph-invariant and
/// stays OUTSIDE the survey). Oxigraph set-merges duplicate quads, so repeated
/// seeds are a no-op; this matches the wholesale path, which re-INSERTs the same
/// set each save. Bypasses the authority gate (a `:projection:` reserved graph).
fn seed_ontology_block(store: &Store, graph_id: &str) -> Result<(), String> {
    let authority_graph = workspace_projection_graph_iri(graph_id);
    let triples = crate::artifact_kinds::ontology_triples();
    if triples.is_empty() {
        return Ok(());
    }
    let insert = triples
        .iter()
        .map(format_rdf_triple)
        .collect::<Vec<_>>()
        .join("\n  ");
    let update = format!("INSERT DATA {{\n  GRAPH <{authority_graph}> {{\n  {insert}\n  }}\n}}");
    SparqlEvaluator::new()
        .parse_update(&update)
        .map_err(|error| format!("parse ontology seed update: {error}"))?
        .on_store(store)
        .execute()
        .map_err(|error| format!("seed artifact-kind ontology: {error}"))
}

/// ADDITIVE entry point: reconcile the workspace projection by MULTI-CLASS
/// VALUE-DIFF (the union of the 4 entity + 6 scene `Fixed` rdf:type spans)
/// instead of the wholesale teardown-and-rebuild [`materialize_workspace_snapshot`]
/// does. Seeds the once-at-seed ontology block (idempotent), partitions
/// [`workspace_desired`] into the 10 class subsets, builds the 10
/// [`workspace_scopes`], and runs [`reconcile_classes`] (survey-all-then-apply-
/// once) over the 10 `(scope, subset)` pairs.
///
/// PURE PARITY: reaches the SAME `:projection:workspace` projection as the
/// wholesale path (same `desired` instance triples + the same ontology block), by
/// MINIMAL delta — a converged save emits 0 ops; an edit to one wire/folder/scene
/// touches ONLY that span's changed slots. Returns the merged [`TripleDiff`] the
/// reconcile applied (the ontology seed is idempotent and not counted in it).
///
/// WIRED to the live workspace save path (`save_workspace` + seed + duplicate),
/// replacing the wholesale call.
pub(super) fn reconcile_workspace_snapshot(
    store: &Store,
    graph_id: &str,
    snapshot: &serde_json::Value,
) -> Result<TripleDiff, String> {
    // (1) Ontology block — once-at-seed idempotent INSERT (never reconciled).
    seed_ontology_block(store, graph_id)?;

    // (2) The 10-class instance reconcile (survey-all-then-apply-once).
    let desired = workspace_desired(graph_id, snapshot);
    let subsets = partition_workspace_desired(&desired);
    let scopes = workspace_scopes(graph_id);
    let scopes_desireds: Vec<(ClassScope, Vec<Triple>)> = scopes.into_iter().zip(subsets).collect();
    // THE FOURTH GATE (S6 → S7 → here): workspace is now SHACL-validated like its
    // three siblings. The road ran through two REAL emitter↔golden drifts, both
    // found by attempting the flip and both fixed on the honest side: (1) the S6
    // datatype drift — entity timestamps stamped xsd:dateTime over epoch-ms values;
    // the emitter now emits xsd:string per the golden, and the conformance oracle
    // seeds every timestamp field so that class of drift can't ship green again;
    // (2) the S7 golden gap — the emitter is the SOLE capturer of a document's
    // `description` (dcterms:description), which the golden's closed TipTapDocument
    // shape didn't declare; the golden now declares it (1.1.0, re-pinned). The
    // once-blocker oracle (`workspace_described_document_conforms`) now proves the
    // described-document projection CONFORMS — the observe-seam is honest.
    reconcile_classes_validated(store, &scopes_desireds, Some(workspace_vocabulary()))
}

// ============================================================================
// P3a ORACLE — ties the Lean workspace model (Vocab/Workspace.lean) to the REAL
// `materialize_workspace_snapshot`. No mocks: we seed a real Oxigraph `Store`,
// run the REAL materializer, and read the authority graph back with SPARQL.
//
// Three corpora, one per Lean structural claim:
//   (1) CONTENT      — the projected set EQUALS a model built independently from
//                      the snapshot fields + constants (never re-running the
//                      materializer). Anti-tautology: model is hand-built.
//   (2) STALE-CLASS  — Target A context-dependence. A stale entity present in the
//                      store but ABSENT from the new snapshot is reclaimed by its
//                      IN-STORE rdf:type (the type-keyed DELETE…WHERE), proving
//                      span-membership reads `cur`, not the snapshot.
//   (3) CYCLE        — Target B. A cyclic parentId chain is materialized VERBATIM
//                      (acyclicity is PERMITTED-not-prevented), and the REAL
//                      `folder_path` reader TERMINATES on it (the cycle-tolerant
//                      visited-set stop the Lean termination theorem models).
// ============================================================================
#[cfg(test)]
mod p3a_oracle {
    use super::*;
    use crate::rdf_query_service::execute_sparql_query;
    // DCTERMS_NS, MDOC_NS, NFO_NS, NIE_NS, WIRE_NS arrive via `use super::*`
    // (the parent module imports them); only RDF_TYPE + XSD_NS are net-new here.
    use crate::runtime_config::{RDF_TYPE, XSD_NS};
    use crate::workspace_entity_projection::folder_path;
    use std::collections::BTreeSet;

    const GID: &str = "graph-a";

    fn authority() -> String {
        workspace_projection_graph_iri(GID)
    }

    /// Read every triple of the workspace authority graph back as a normalized
    /// `S P O` set (oxigraph term `to_string()` rendering, matching what the
    /// query service returns). This is the REAL store read-back.
    fn read_authority_set(store: &Store) -> BTreeSet<String> {
        let g = authority();
        let result = execute_sparql_query(
            store,
            &format!("SELECT ?s ?p ?o WHERE {{ GRAPH <{g}> {{ ?s ?p ?o }} }}"),
        )
        .expect("query authority graph");
        result
            .rows
            .iter()
            .map(|row| format!("{} {} {}", row["s"], row["p"], row["o"]))
            .collect()
    }

    /// All triples for one subject in the authority graph, as `P O` pairs.
    fn subject_pairs(store: &Store, subject: &str) -> BTreeSet<String> {
        let g = authority();
        let result = execute_sparql_query(
            store,
            &format!("SELECT ?p ?o WHERE {{ GRAPH <{g}> {{ <{subject}> ?p ?o }} }}"),
        )
        .expect("query subject");
        result
            .rows
            .iter()
            .map(|row| format!("{} {}", row["p"], row["o"]))
            .collect()
    }

    // --- term renderers matching oxigraph `Term::to_string()` ----------------
    fn uri(s: &str) -> String {
        format!("<{s}>")
    }
    fn lit(s: &str) -> String {
        format!("\"{s}\"")
    }
    fn typed(s: &str, dt: &str) -> String {
        format!("\"{s}\"^^<{dt}>")
    }
    fn float0() -> String {
        // push_float_triple renders 0.0_f64 as "0" with xsd:float (rdf.rs:144).
        typed("0", &format!("{XSD_NS}float"))
    }

    fn folder_subject(id: &str) -> String {
        format!("urn:mnemosyne:local:graph:{GID}:folder:{id}")
    }
    fn doc_subject(id: &str) -> String {
        format!("urn:mnemosyne:local:document:{id}")
    }
    fn artifact_subject(id: &str) -> String {
        format!("urn:mnemosyne:local:graph:{GID}:artifact:{id}")
    }
    fn wire_subject(id: &str) -> String {
        format!("urn:mnemosyne:local:graph:{GID}:wire:{id}")
    }

    /// Seed a raw triple into the authority graph WITHOUT going through the
    /// materializer (so the stale-class corpus is genuinely independent).
    fn seed_raw(store: &Store, s: &str, p: &str, o_term: &str) {
        let g = authority();
        let update = format!("INSERT DATA {{ GRAPH <{g}> {{ <{s}> <{p}> {o_term} }} }}");
        SparqlEvaluator::new()
            .parse_update(&update)
            .expect("parse seed")
            .on_store(store)
            .execute()
            .expect("seed raw triple");
    }

    // ------------------------------------------------------------------------
    // (1) CONTENT: real projection == independently-built model.
    // One folder, one document child, one artifact, one wire. The model below
    // is built BY HAND from snapshot fields + the namespace constants — it does
    // NOT call the materializer or any projection helper.
    // ------------------------------------------------------------------------
    #[test]
    fn oracle_content_real_equals_independent_model() {
        let store = Store::new().expect("store");
        let snapshot = serde_json::json!({
            "folders": [
                { "id": "fold-1", "name": "Reports", "order": 0, "parentId": "root-fold" }
            ],
            "documents": [
                { "id": "doc-1", "title": "Q3", "order": 0, "parentId": "fold-1" }
            ],
            "artifacts": [
                { "id": "art-1", "name": "chart.png", "mimeType": "image/png", "order": 0, "parentId": "fold-1" }
            ],
            "wires": [
                { "id": "wire-1", "sourceDocumentId": "doc-1", "targetDocumentId": "doc-x", "predicate": "supports" }
            ]
        });

        materialize_workspace_snapshot(&store, GID, &snapshot).expect("materialize");

        // --- INDEPENDENT MODEL (hand-built; never re-runs the materializer) ---
        let fold = folder_subject("fold-1");
        let doc = doc_subject("doc-1");
        let art = artifact_subject("art-1");
        let wire = wire_subject("wire-1");

        // Folder face.
        let mut model_fold = BTreeSet::new();
        model_fold.insert(format!(
            "{} {}",
            uri(RDF_TYPE),
            uri(&format!("{MDOC_NS}Folder"))
        ));
        model_fold.insert(format!(
            "{} {}",
            uri(&format!("{NFO_NS}fileName")),
            lit("Reports")
        ));
        model_fold.insert(format!(
            "{} {}",
            uri(&format!("{NFO_NS}belongsToContainer")),
            uri(&folder_subject("root-fold"))
        ));
        model_fold.insert(format!("{} {}", uri(&format!("{MDOC_NS}order")), float0()));

        // Document face.
        let mut model_doc = BTreeSet::new();
        model_doc.insert(format!(
            "{} {}",
            uri(RDF_TYPE),
            uri(&format!("{MDOC_NS}TipTapDocument"))
        ));
        model_doc.insert(format!(
            "{} {}",
            uri(&format!("{DCTERMS_NS}title")),
            lit("Q3")
        ));
        model_doc.insert(format!(
            "{} {}",
            uri(&format!("{NFO_NS}belongsToContainer")),
            uri(&fold)
        ));
        model_doc.insert(format!("{} {}", uri(&format!("{MDOC_NS}order")), float0()));

        // Artifact face (image/png ⇒ doc:Image kind class + doc:kind "image").
        let mut model_art = BTreeSet::new();
        model_art.insert(format!(
            "{} {}",
            uri(RDF_TYPE),
            uri(&format!("{MDOC_NS}Artifact"))
        ));
        model_art.insert(format!(
            "{} {}",
            uri(RDF_TYPE),
            uri(&format!("{MDOC_NS}Image"))
        ));
        model_art.insert(format!(
            "{} {}",
            uri(&format!("{MDOC_NS}kind")),
            lit("image")
        ));
        model_art.insert(format!(
            "{} {}",
            uri(&format!("{NFO_NS}fileName")),
            lit("chart.png")
        ));
        model_art.insert(format!(
            "{} {}",
            uri(&format!("{NFO_NS}belongsToContainer")),
            uri(&fold)
        ));
        model_art.insert(format!("{} {}", uri(&format!("{MDOC_NS}order")), float0()));
        model_art.insert(format!(
            "{} {}",
            uri(&format!("{NIE_NS}mimeType")),
            lit("image/png")
        ));

        // Wire face: type + the wire-materializer's endpoint/predicate triples.
        let mut model_wire = BTreeSet::new();
        model_wire.insert(format!(
            "{} {}",
            uri(RDF_TYPE),
            uri(&format!("{WIRE_NS}Wire"))
        ));
        model_wire.insert(format!(
            "{} {}",
            uri(&format!("{WIRE_NS}sourceDocument")),
            uri(&doc_subject("doc-1"))
        ));
        model_wire.insert(format!(
            "{} {}",
            uri(&format!("{WIRE_NS}targetDocument")),
            uri(&doc_subject("doc-x"))
        ));
        model_wire.insert(format!(
            "{} {}",
            uri(&format!("{WIRE_NS}predicate")),
            uri(&format!("{WIRE_NS}supports"))
        ));
        // `targetGraph` is ALWAYS emitted by the wire materializer
        // (rdf_wire_materializer.rs:111-114, "defaults to graph_id when
        // targetGraphId absent"). The oracle surfaced this — the hand-model must
        // include it, mirroring the real projection rather than re-running it.
        model_wire.insert(format!(
            "{} {}",
            uri(&format!("{WIRE_NS}targetGraph")),
            lit(GID)
        ));

        // --- assert each face set-equals its independent model -----------------
        assert_eq!(subject_pairs(&store, &fold), model_fold, "folder face");
        assert_eq!(subject_pairs(&store, &doc), model_doc, "document face");
        assert_eq!(subject_pairs(&store, &art), model_art, "artifact face");
        assert_eq!(subject_pairs(&store, &wire), model_wire, "wire face");

        // ANTI-TAUTOLOGY GUARD: a deliberately-wrong model must NOT match, so the
        // equalities above have teeth (they aren't comparing two empty sets etc.).
        let mut wrong = model_doc.clone();
        wrong.insert(format!(
            "{} {}",
            uri(&format!("{DCTERMS_NS}title")),
            lit("WRONG")
        ));
        assert_ne!(
            subject_pairs(&store, &doc),
            wrong,
            "anti-tautology: bent model rejected"
        );
        assert!(
            !subject_pairs(&store, &fold).is_empty(),
            "folder actually projected something"
        );
    }

    // ------------------------------------------------------------------------
    // (2) STALE-CLASS RECLAIM (Target A — context-dependence).
    // Pre-seed a stale folder (typed doc:Folder, with payload triples) NOT in the
    // new snapshot. The type-keyed DELETE…WHERE { ?folder a doc:Folder } reclaims
    // it BY ITS IN-STORE TYPE — proving span-membership reads the STORE, not the
    // snapshot. This is the disjoint-CLASS / context-dependent span made concrete.
    // ------------------------------------------------------------------------
    #[test]
    fn oracle_stale_class_reclaimed_context_dependent() {
        let store = Store::new().expect("store");
        let stale = folder_subject("ghost-fold");

        // Seed a stale folder directly into the store (independent of materializer).
        seed_raw(&store, &stale, RDF_TYPE, &uri(&format!("{MDOC_NS}Folder")));
        seed_raw(
            &store,
            &stale,
            &format!("{NFO_NS}fileName"),
            &lit("Ghost Folder"),
        );
        // Also seed a stale WIRE (the purest type-keyed span: free ?wire AND ?wire_p).
        let stale_wire = wire_subject("ghost-wire");
        seed_raw(
            &store,
            &stale_wire,
            RDF_TYPE,
            &uri(&format!("{WIRE_NS}Wire")),
        );
        seed_raw(
            &store,
            &stale_wire,
            &format!("{WIRE_NS}predicate"),
            &uri(&format!("{WIRE_NS}supports")),
        );

        // Sanity: the stale triples ARE present before materialize.
        assert!(
            !subject_pairs(&store, &stale).is_empty(),
            "precondition: stale folder seeded"
        );
        assert!(
            !subject_pairs(&store, &stale_wire).is_empty(),
            "precondition: stale wire seeded"
        );

        // New snapshot contains a DIFFERENT folder; the ghost is absent from it.
        let snapshot = serde_json::json!({
            "folders": [ { "id": "fresh-fold", "name": "Fresh", "order": 0 } ],
            "wires": [ { "id": "fresh-wire", "sourceDocumentId": "doc-1", "predicate": "supports" } ]
        });
        materialize_workspace_snapshot(&store, GID, &snapshot).expect("materialize");

        // RECLAIM: the stale folder's and stale wire's triples are GONE — deleted
        // by the type-keyed span keyed on their IN-STORE rdf:type, not the snapshot.
        assert!(
            subject_pairs(&store, &stale).is_empty(),
            "Target A: stale folder reclaimed by in-store doc:Folder type (context-dependent DELETE)"
        );
        assert!(
            subject_pairs(&store, &stale_wire).is_empty(),
            "Target A: stale wire reclaimed by in-store mnemo:Wire type (purest type-keyed span)"
        );

        // And the FRESH entities are present (the INSERT half ran).
        assert!(
            !subject_pairs(&store, &folder_subject("fresh-fold")).is_empty(),
            "fresh folder projected"
        );
        assert!(
            !subject_pairs(&store, &wire_subject("fresh-wire")).is_empty(),
            "fresh wire projected"
        );
    }

    /// TEETH companion — a stale subject WITHOUT the class type is NOT reclaimed
    /// by the type-keyed span (it is not `a doc:Folder`), confirming the DELETE is
    /// genuinely type-keyed and not a blanket graph clear. (If everything were
    /// cleared, this would assert-out and the context-dependence claim REFUTED.)
    #[test]
    fn oracle_untyped_stale_survives_type_keyed_delete() {
        let store = Store::new().expect("store");
        // An orphan subject in the authority graph with NO rdf:type doc:Folder etc.
        let orphan = format!("urn:mnemosyne:local:graph:{GID}:orphan:x");
        seed_raw(&store, &orphan, &format!("{MDOC_NS}note"), &lit("survivor"));

        let snapshot = serde_json::json!({
            "folders": [ { "id": "f", "name": "F", "order": 0 } ]
        });
        materialize_workspace_snapshot(&store, GID, &snapshot).expect("materialize");

        assert!(
            !subject_pairs(&store, &orphan).is_empty(),
            "untyped orphan is NOT reclaimed: the DELETE is type-keyed, not a blanket clear"
        );
    }

    // ------------------------------------------------------------------------
    // (3) CYCLE: acyclicity UNENFORCED + reader terminates (Target B).
    // Snapshot with folder-a parent folder-b and folder-b parent folder-a (a
    // 2-cycle), plus the self-loop the runtime test pins. The cyclic parentId
    // edges are emitted VERBATIM (acyclicity not enforced), and the REAL
    // folder_path reader TERMINATES on the cycle (matching the Lean termination
    // + cycle-stop theorems).
    // ------------------------------------------------------------------------
    #[test]
    fn oracle_cycle_emitted_and_reader_terminates() {
        let store = Store::new().expect("store");
        let snapshot = serde_json::json!({
            "folders": [
                { "id": "fold-a", "name": "A", "order": 0, "parentId": "fold-b" },
                { "id": "fold-b", "name": "B", "order": 0, "parentId": "fold-a" },
                { "id": "selfloop", "name": "Loop", "order": 0, "parentId": "selfloop" }
            ]
        });
        materialize_workspace_snapshot(&store, GID, &snapshot).expect("materialize");

        // The CYCLIC parentId edges ARE emitted verbatim — acyclicity NOT enforced.
        let pred = format!("{NFO_NS}belongsToContainer");
        let a_to_b = format!("{} {}", uri(&pred), uri(&folder_subject("fold-b")));
        let b_to_a = format!("{} {}", uri(&pred), uri(&folder_subject("fold-a")));
        let loop_to_self = format!("{} {}", uri(&pred), uri(&folder_subject("selfloop")));
        assert!(
            subject_pairs(&store, &folder_subject("fold-a")).contains(&a_to_b),
            "Target B: cyclic edge a->b emitted verbatim (acyclicity unenforced)"
        );
        assert!(
            subject_pairs(&store, &folder_subject("fold-b")).contains(&b_to_a),
            "Target B: cyclic edge b->a emitted verbatim (acyclicity unenforced)"
        );
        assert!(
            subject_pairs(&store, &folder_subject("selfloop")).contains(&loop_to_self),
            "Target B: self-loop edge emitted verbatim"
        );

        // The REAL reader TERMINATES on the cycle (no hang) and returns a valid
        // finite ancestor prefix — matching the Lean termination theorems
        // (walk_len_le_fuel) and the cycle-stop (walk_self_loop_stops).
        let folders = vec![
            serde_json::json!({ "id": "fold-a", "title": "A", "parentId": "fold-b" }),
            serde_json::json!({ "id": "fold-b", "title": "B", "parentId": "fold-a" }),
            serde_json::json!({ "id": "selfloop", "title": "Loop", "parentId": "selfloop" }),
        ];
        // 2-cycle: walking from fold-a visits {a, b} then re-sees a => stops.
        let path_a = folder_path(&folders, Some("fold-a".to_string()));
        assert!(
            path_a.is_some(),
            "reader returns a finite prefix on the 2-cycle (terminates)"
        );
        let p = path_a.unwrap();
        // Exactly the two distinct nodes, no infinite repetition.
        assert_eq!(
            p.matches('/').count(),
            1,
            "2-cycle yields exactly 2 path segments (bounded)"
        );
        assert!(
            p.contains('A') && p.contains('B'),
            "both cycle members appear once"
        );

        // Self-loop: stops after the single node (pins the runtime test's "Loop").
        assert_eq!(
            folder_path(&folders, Some("selfloop".to_string())),
            Some("Loop".to_string()),
            "self-loop ⇒ single-node prefix (walk_self_loop_stops analogue)"
        );

        // Dangling parent: reader stops (find?=none), no fabrication.
        assert_eq!(
            folder_path(&folders, Some("nonexistent".to_string())),
            None,
            "dangling parent ⇒ None (find?=none break)"
        );
    }

    // A full authority read-back is non-empty after a real materialize — guards
    // that read_authority_set itself isn't silently returning nothing (so the
    // emptiness assertions in the stale test are meaningful).
    #[test]
    fn oracle_authority_readback_nonempty_after_materialize() {
        let store = Store::new().expect("store");
        let snapshot = serde_json::json!({
            "folders": [ { "id": "f", "name": "F", "order": 0 } ]
        });
        materialize_workspace_snapshot(&store, GID, &snapshot).expect("materialize");
        assert!(
            !read_authority_set(&store).is_empty(),
            "authority graph has triples after materialize (read-back is live)"
        );
    }
}

// ============================================================================
// P3 WORKSPACE RECONCILE ORACLE — ties the multi-class `reconcile_workspace_
// snapshot` (the 4 entity + 6 scene `Fixed` spans composed by `reconcile_classes`
// + the once-at-seed ontology block) to the wholesale `materialize_workspace_
// snapshot` BASELINE. NO MOCKS: real in-memory Oxigraph `Store`, real
// materializers, real SPARQL read-back of the `:projection:workspace` graph.
//
//   (1) EQUIVALENCE — wholesale(A) FULL workspace-graph set == reconcile(B) full
//       set, from the SAME snapshot (folder + doc + ARTIFACT-WITH-SCENE + wires).
//       The reconcile seeds the ontology + reconciles entity+scene, so its total
//       projection equals the wholesale's (entity + scene + ontology). The
//       scene-bearing artifact is LOAD-BEARING: it exercises the 6 scene spans.
//   (2) DOMINATION — from empty, reconcile op_count <= the wholesale fresh cost;
//       on a CONVERGED re-run, reconcile is 0 (strictly < the wholesale rebuild).
//   (3) CONVERGENCE — THE PREREQ-B PROOF. A second reconcile of the SAME
//       scene-bearing snapshot emits 0 ops. If the scene children were off-class
//       (no surveyed span) OR the ontology were reconciled (not seeded), this
//       would show perpetual ADDs and FAIL. 0 ops = prereq-B is actually solved.
//   (4) ONTOLOGY-IDEMPOTENT — the ontology block is present after reconcile and
//       is NEVER in the reconcile diff (seeded once, never reclaimed/re-added).
//   (5) SCENE-EDIT-MINIMAL — editing one scene anchor touches ONLY that anchor's
//       span (minimal delta through the scene class spans), and re-converges to 0.
//   (6) STALE-RECLAIM — a stale wire/folder absent from the new snapshot is
//       reclaimed by its in-store type (the type-keyed survey reads the store).
// ============================================================================
#[cfg(test)]
mod p3_workspace_reconcile_oracle {
    use super::*;
    use crate::rdf_query_service::execute_sparql_query;
    use std::collections::BTreeSet;

    const GID: &str = "graph-ws-recon";

    fn ws() -> String {
        workspace_projection_graph_iri(GID)
    }

    /// EVERY triple of the `:projection:workspace` named graph as `S P O` strings.
    fn full_set(store: &Store) -> BTreeSet<String> {
        let g = ws();
        execute_sparql_query(
            store,
            &format!("SELECT ?s ?p ?o WHERE {{ GRAPH <{g}> {{ ?s ?p ?o }} }}"),
        )
        .expect("query workspace graph")
        .rows
        .iter()
        .map(|row| format!("{} {} {}", row["s"], row["p"], row["o"]))
        .collect()
    }

    /// All `P O` pairs for one subject in the workspace graph.
    fn subject_pairs(store: &Store, subject: &str) -> BTreeSet<String> {
        let g = ws();
        execute_sparql_query(
            store,
            &format!("SELECT ?p ?o WHERE {{ GRAPH <{g}> {{ <{subject}> ?p ?o }} }}"),
        )
        .expect("query subject")
        .rows
        .iter()
        .map(|row| format!("{} {}", row["p"], row["o"]))
        .collect()
    }

    /// A snapshot with one folder, one document child, one ARTIFACT-WITH-SCENE
    /// (exercising ALL six scene spans: projection + anchor + frame + text +
    /// wire-candidate + diagnostic), and two wires. The scene-bearing artifact is
    /// what makes the convergence test a real prereq-B proof.
    fn scene_snapshot() -> serde_json::Value {
        serde_json::json!({
            "folders": [ { "id": "fold-1", "name": "Reports", "order": 0.0 } ],
            "documents": [ { "id": "doc-1", "title": "Q3", "order": 0.0, "parentId": "fold-1" } ],
            "artifacts": [{
                "id": "art-1",
                "name": "Map.excalidraw",
                "mimeType": "application/vnd.excalidraw+json",
                "status": "ready",
                "order": 1.0,
                "parentId": "fold-1",
                "sceneProjection": {
                    "projectedAt": "2026-06-04T12:00:00.000Z",
                    "searchText": "Anchor: Source",
                    "anchors": [{
                        "sceneElementId": "source-el",
                        "kind": "document",
                        "graphId": GID,
                        "id": "doc-1",
                        "title": "Source",
                        "targetExists": true,
                        "frameId": "frame-1"
                    }],
                    "frames": [{ "sceneElementId": "frame-1", "title": "Cluster" }],
                    "text": [{ "sceneElementId": "note-1", "text": "Important note", "frameId": "frame-1" }],
                    "wireCandidates": [{
                        "sceneElementId": "arrow-1",
                        "sourceKind": "document", "sourceId": "doc-1", "sourceDocumentId": "doc-1",
                        "targetGraphId": GID, "targetKind": "document", "targetId": "doc-b",
                        "targetDocumentId": "doc-b", "predicate": "supports", "label": "supports"
                    }],
                    "diagnostics": [{
                        "sceneElementId": "bad-arrow",
                        "code": "arrow-unlinked-endpoint",
                        "message": "Arrow endpoint is not linked."
                    }]
                }
            }],
            "wires": [
                { "id": "w1", "sourceDocumentId": "doc-1", "targetDocumentId": "doc-b", "predicate": "supports" }
            ]
        })
    }

    // (1) EQUIVALENCE + (4) ONTOLOGY presence.
    #[test]
    fn oracle_workspace_reconcile_equivalent_to_wholesale() {
        let snap = scene_snapshot();

        let store_a = Store::new().expect("store A");
        materialize_workspace_snapshot(&store_a, GID, &snap).expect("wholesale A");

        let store_b = Store::new().expect("store B");
        let diff = reconcile_workspace_snapshot(&store_b, GID, &snap).expect("reconcile B");

        let set_a = full_set(&store_a);
        let set_b = full_set(&store_b);
        let missing: Vec<_> = set_a.difference(&set_b).cloned().collect();
        let extra: Vec<_> = set_b.difference(&set_a).cloned().collect();
        eprintln!("wholesale-only (missing from reconcile): {missing:?}");
        eprintln!("reconcile-only (extra vs wholesale):     {extra:?}");
        assert_eq!(
            set_a, set_b,
            "EQUIVALENCE: reconcile full workspace set == wholesale full set (incl. scene + ontology)"
        );
        assert!(
            !set_b.is_empty(),
            "the snapshot actually projected something"
        );
        // from empty: pure inserts (the entity+scene desired; the ontology is seeded
        // OUTSIDE the diff, so the diff has no removes).
        assert!(
            diff.removes.is_empty(),
            "from-empty reconcile removes nothing"
        );
        assert!(
            !diff.adds.is_empty(),
            "from-empty reconcile adds the projection"
        );

        // ONTOLOGY present: the artifact-kind class hierarchy is in the graph.
        let image_class = format!("{MDOC_NS}Image");
        assert!(
            !subject_pairs(&store_b, &image_class).is_empty(),
            "ontology block (mdoc:Image rdfs:subClassOf …) is seeded into the graph"
        );

        // SCENE present: the scene root + its six child types projected.
        let scene_root = format!("urn:mnemosyne:local:graph:{GID}:artifact:art-1#scene");
        assert!(
            subject_pairs(&store_b, &scene_root)
                .contains(&format!("<{RDF_TYPE}> <{MDOC_NS}SceneProjection>")),
            "the scene root carries mdoc:SceneProjection"
        );

        // ANTI-TAUTOLOGY: a bent full set must not match.
        let mut wrong = set_b.clone();
        wrong.insert("<urn:bogus> <urn:bogus> <urn:bogus>".to_string());
        assert_ne!(set_a, wrong, "anti-tautology: bent set rejected");
    }

    // (2) DOMINATION + (3) CONVERGENCE — THE PREREQ-B PROOF.
    #[test]
    fn oracle_workspace_reconcile_dominates_and_converges() {
        let snap = scene_snapshot();
        let store = Store::new().expect("store");

        let first = reconcile_workspace_snapshot(&store, GID, &snap).expect("first reconcile");
        assert!(first.op_count() > 0, "from-empty reconcile does ops");
        assert_eq!(first.removes.len(), 0, "from-empty = pure inserts");
        let desired_len = first.op_count();
        assert!(
            first.op_count() <= desired_len,
            "DOMINATION fresh: reconcile <= wholesale fresh"
        );

        // THE PREREQ-B PROOF: a second reconcile of the SAME scene-bearing snapshot
        // emits ZERO ops. Off-class scene children OR a reconciled ontology would
        // show perpetual ADDs here. Report any residual verbatim before failing.
        let second = reconcile_workspace_snapshot(&store, GID, &snap).expect("second reconcile");
        if second.op_count() != 0 {
            eprintln!("CONVERGENCE FAILURE — residual diff (prereq-B leak):");
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
            "CONVERGENCE (prereq-B): re-reconcile of the scene-bearing snapshot is 0 ops \
             (nonzero ⇒ an off-class scene/ontology subgraph leaks as perpetual ADDs)"
        );
        assert!(
            second.op_count() < 2 * desired_len,
            "DOMINATION strict: converged reconcile < wholesale teardown-rebuild"
        );
    }

    // (4) ONTOLOGY-IDEMPOTENT — the ontology is never in the reconcile diff.
    #[test]
    fn oracle_workspace_ontology_idempotent_outside_diff() {
        let snap = scene_snapshot();
        let store = Store::new().expect("store");
        // First reconcile: the entity+scene desired are added; the ontology is
        // SEEDED (not in the diff). The diff must contain NO rdfs:subClassOf triple.
        let first = reconcile_workspace_snapshot(&store, GID, &snap).expect("first reconcile");
        let subclass = "http://www.w3.org/2000/01/rdf-schema#subClassOf";
        assert!(
            !first.adds.iter().any(|(_, p, _)| p == subclass),
            "the ontology rdfs:subClassOf triples are NOT in the reconcile diff (seeded, not reconciled)"
        );
        // But they ARE in the graph (seeded).
        let g = ws();
        let n = execute_sparql_query(
            &store,
            &format!("SELECT (COUNT(*) AS ?n) WHERE {{ GRAPH <{g}> {{ ?s <{subclass}> ?o }} }}"),
        )
        .expect("count subclass")
        .rows
        .first()
        .map(|r| r["n"].clone())
        .and_then(|raw| raw.split('"').nth(1).and_then(|d| d.parse::<i64>().ok()))
        .unwrap_or(-1);
        assert!(
            n > 0,
            "the ontology subClassOf triples ARE in the graph (seeded once)"
        );
    }

    // (5) SCENE-EDIT-MINIMAL — editing one anchor title touches ONLY that anchor.
    #[test]
    fn oracle_workspace_scene_edit_is_minimal() {
        let store = Store::new().expect("store");
        reconcile_workspace_snapshot(&store, GID, &scene_snapshot()).expect("seed");

        // Edit: change the anchor's title from "Source" to "Edited Source".
        let mut edited = scene_snapshot();
        edited["artifacts"][0]["sceneProjection"]["anchors"][0]["title"] =
            serde_json::json!("Edited Source");
        let diff = reconcile_workspace_snapshot(&store, GID, &edited).expect("edit reconcile");

        assert!(diff.op_count() > 0, "the edit does ops");
        let anchor =
            format!("urn:mnemosyne:local:graph:{GID}:artifact:art-1#scene-anchor-source-el");
        // EVERY changed triple belongs to the edited anchor subject (minimal delta).
        assert!(
            diff.adds
                .iter()
                .chain(diff.removes.iter())
                .all(|(s, _, _)| s == &anchor),
            "SCENE-EDIT-MINIMAL: only the edited anchor's span changed"
        );
        // The new title is present, the old gone.
        let pairs = subject_pairs(&store, &anchor);
        assert!(
            pairs.contains("<http://purl.org/dc/terms/title> \"Edited Source\""),
            "the edited title is projected"
        );
        assert!(
            !pairs.contains("<http://purl.org/dc/terms/title> \"Source\""),
            "the old title is gone"
        );
        // Re-converges to 0.
        let reconv = reconcile_workspace_snapshot(&store, GID, &edited).expect("re-reconcile");
        assert_eq!(
            reconv.op_count(),
            0,
            "post-edit store re-converges to 0 ops"
        );
    }

    // (6) STALE-RECLAIM — a wire/folder dropped from the snapshot is reclaimed.
    #[test]
    fn oracle_workspace_stale_reclaimed() {
        let store = Store::new().expect("store");
        // Seed two folders + a wire.
        let snap_two = serde_json::json!({
            "folders": [
                { "id": "keep", "name": "Keep", "order": 0.0 },
                { "id": "ghost", "name": "Ghost", "order": 1.0 }
            ],
            "documents": [], "artifacts": [],
            "wires": [
                { "id": "wkeep", "sourceDocumentId": "doc-a", "predicate": "supports" },
                { "id": "wghost", "sourceDocumentId": "doc-b", "predicate": "supports" }
            ]
        });
        reconcile_workspace_snapshot(&store, GID, &snap_two).expect("seed two");
        let ghost_fold = format!("urn:mnemosyne:local:graph:{GID}:folder:ghost");
        let ghost_wire = format!("urn:mnemosyne:local:graph:{GID}:wire:wghost");
        assert!(
            !subject_pairs(&store, &ghost_fold).is_empty(),
            "ghost folder seeded"
        );
        assert!(
            !subject_pairs(&store, &ghost_wire).is_empty(),
            "ghost wire seeded"
        );

        // New snapshot drops the ghosts.
        let snap_one = serde_json::json!({
            "folders": [ { "id": "keep", "name": "Keep", "order": 0.0 } ],
            "documents": [], "artifacts": [],
            "wires": [ { "id": "wkeep", "sourceDocumentId": "doc-a", "predicate": "supports" } ]
        });
        let diff = reconcile_workspace_snapshot(&store, GID, &snap_one).expect("reconcile one");
        assert!(diff.op_count() > 0, "dropping entities does ops");
        assert!(diff.adds.is_empty(), "the drop is pure removal");
        assert!(
            subject_pairs(&store, &ghost_fold).is_empty(),
            "STALE-RECLAIM: ghost folder reclaimed by its in-store mdoc:Folder type"
        );
        assert!(
            subject_pairs(&store, &ghost_wire).is_empty(),
            "STALE-RECLAIM: ghost wire reclaimed by its in-store mnemo:Wire type"
        );
        assert!(
            !subject_pairs(
                &store,
                &format!("urn:mnemosyne:local:graph:{GID}:folder:keep")
            )
            .is_empty(),
            "the kept folder survives"
        );
        let reconv = reconcile_workspace_snapshot(&store, GID, &snap_one).expect("re-reconcile");
        assert_eq!(
            reconv.op_count(),
            0,
            "post-reclaim store re-converges to 0 ops"
        );
    }
}

// ════════════════════════════════════════════════════════════════════════════
//  EA-2b+ SHACL CONFORMANCE ORACLE — WORKSPACE kind (the structural fork: a
//  MULTI-CLASS union + a once-at-seed rdfs:subClassOf ontology block).
//
//  The REAL workspace projection (`reconcile_workspace_snapshot` into a real
//  Oxigraph store) CONFORMS to the SHACL shapes DERIVED from the `emporium-workspace`
//  vocab contract (`vocab_to_shacl` → one flat closed NodeShape per entity class).
//  NO MOCKS: real reconcile, real store, real rudof engine. The validated triples are
//  READ BACK out of the persisted `:projection:workspace` graph (the entity-class
//  union), so the oracle covers the WHOLE materializer→store→shapes path.
//
//  DERIVED vs COMPLEMENT vs SHACL-INEXPRESSIBLE:
//   • DERIVED: the per-entity-class field shapes (Folder/TipTapDocument/Artifact —
//     rdf:type, datatypes incl. xsd:float order, the attribute predicates, sh:closed).
//     The multi-class union is just several NodeShapes; vocab_to_shacl handles it.
//     (The Artifact's dual kind-class rdf:type, e.g. mdoc:Image, is permitted by the
//     closed shape since rdf:type is sh:ignoredProperties.)
//   • SCOPE: the 4th entity class WIRE (wire:Wire, WIRE_NS not MDOC_NS) is covered by
//     the dedicated WIRES oracle; the 6 SCENE classes are out of this contract (their
//     many optional predicates would bloat a closed shape) — documented scope choices.
//   • SHACL-INEXPRESSIBLE: the ONTOLOGY BLOCK (mdoc:Image rdfs:subClassOf mdoc:Artifact
//     + capability triples; subjects are CLASS IRIs with NO rdf:type) — no instance
//     shape targets a class-level subject, so it is NOT expressible in this model. It
//     is a once-at-seed idempotent INSERT (seed_ontology_block) and is asserted
//     present + OUTSIDE the reconcile diff by the complement test (NOT silently skipped).
//
//  TEETH: (a) a folder MISSING nothing required still conforms; a folder carrying a
//             rogue predicate is REJECTED (sh:closed); (b) a wrong-datatype order
//             (string where xsd:float) is REJECTED; (c) the ontology subclass triples
//             are present + idempotent (the SHACL-inexpressible seed complement).
// ════════════════════════════════════════════════════════════════════════════
#[cfg(test)]
mod shacl_workspace_conformance_oracle {
    use super::*;
    use crate::emporium::contract::workspace_vocabulary;
    use crate::emporium::shacl_validator::{validate_desired, validate_desired_structured};
    use crate::emporium::survey::parse_term as oracle_parse_term;
    use crate::emporium::terms::{Term, Triple as EngineTriple};
    use oxigraph::sparql::{QueryResults, SparqlEvaluator};

    const GID: &str = "graph-ws-shacl";

    /// The 3 mdoc-namespaced entity classes this contract targets (Wire is its own).
    fn entity_class_iris() -> [String; 3] {
        [
            format!("{MDOC_NS}Folder"),
            format!("{MDOC_NS}TipTapDocument"),
            format!("{MDOC_NS}Artifact"),
        ]
    }

    /// Read the REAL persisted entity-class projection back from `:projection:workspace`
    /// as engine `Triple`s — scoped to subjects whose rdf:type is one of the 3 entity
    /// classes (so the ontology class-IRI subjects, which have no rdf:type, and the
    /// wire/scene subjects are excluded). Bridges each `?o` via `parse_term`.
    fn read_back_entity_projection(store: &Store) -> Vec<EngineTriple> {
        let g = workspace_projection_graph_iri(GID);
        let iris = entity_class_iris();
        let values = iris
            .iter()
            .map(|i| format!("<{i}>"))
            .collect::<Vec<_>>()
            .join(" ");
        let query = format!(
            "SELECT ?s ?p ?o WHERE {{ GRAPH <{g}> {{ \
             ?s <{RDF_TYPE}> ?t . VALUES ?t {{ {values} }} . ?s ?p ?o }} }}"
        );
        let solutions = match SparqlEvaluator::new()
            .parse_query(&query)
            .expect("parse entity readback")
            .on_store(store)
            .execute()
            .expect("execute entity readback")
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

    fn workspace_snapshot() -> serde_json::Value {
        // Every entity carries its TIMESTAMP fields (createdAt/updatedAt +
        // describedAt/lastAccessedAt on the document + sceneProjectedAt on the
        // artifact). These were OMITTED from the oracle seed before S7 — the exact
        // blind spot that let the emitter's `^^xsd:dateTime` datatype drift ship
        // green (the shapes type them xsd:string; epoch-ms values like
        // "1700000000000" are NOT valid xsd:dateTime lexical forms, and even a
        // genuine ISO-8601 sceneProjectedAt is contract-typed xsd:string). With them
        // seeded, the conformance oracle can never again miss this class of drift.
        serde_json::json!({
            "folders": [
                { "id": "fold-1", "name": "Reports", "order": 0, "parentId": "root-fold",
                  "createdAt": "1700000000000", "updatedAt": "1700000001000" }
            ],
            "documents": [
                { "id": "doc-1", "title": "Q3", "order": 1, "parentId": "fold-1",
                  "createdAt": "1700000000000", "updatedAt": "1700000001000",
                  "describedAt": "1700000002000", "lastAccessedAt": "1700000003000" }
            ],
            "artifacts": [
                { "id": "art-1", "name": "chart.png", "mimeType": "image/png", "order": 2, "parentId": "fold-1",
                  "fileType": "image", "status": "ready", "size": 123,
                  "storageKey": "local://artifacts/art-1/original/chart.png",
                  "originalFilename": "chart.png", "errorMessage": "",
                  "createdAt": "1700000000000", "updatedAt": "1700000001000",
                  "sceneProjectedAt": "2026-06-04T12:00:00.000Z" }
            ],
            "wires": []
        })
    }

    /// THE ONCE-BLOCKER, NOW THE PROOF: S7 pinned this as the reason the workspace
    /// flip stayed deferred — a described document projected `dcterms:description`,
    /// undeclared by the golden's closed `TipTapDocument` shape, so gating would have
    /// loud-halted every real save of a described document. The golden now declares
    /// `dcterms:description` (xsd:string, 1.1.0), the reconcile IS gated
    /// (`Some(workspace_vocabulary())`), and this oracle holds the door open: a
    /// described document's projection must CONFORM — and the gated reconcile itself
    /// must accept it end-to-end.
    #[test]
    fn workspace_described_document_conforms() {
        let store = Store::new().expect("store");
        let snap = serde_json::json!({
            "folders": [],
            "documents": [
                { "id": "doc-desc", "title": "T", "order": 0,
                  "description": "About things",
                  "createdAt": "1700000000000", "updatedAt": "1700000001000" }
            ],
            "artifacts": [],
            "wires": []
        });
        // The reconcile is now GATED — a described document landing at all proves the
        // gate accepts it; the explicit validation below keeps the oracle independent.
        reconcile_workspace_snapshot(&store, GID, &snap)
            .expect("the GATED reconcile accepts a described document");
        let projection = read_back_entity_projection(&store);
        assert!(
            projection
                .iter()
                .any(|(_, p, _)| p == "http://purl.org/dc/terms/description"),
            "the description is captured (the emitter is its sole projection)"
        );
        let result = validate_desired(&projection, workspace_vocabulary());
        assert!(
            result.is_ok(),
            "a described document CONFORMS to the completed workspace contract: {result:?}"
        );
    }

    /// CONFORMANCE: the REAL multi-class workspace entity projection conforms to the
    /// three vocab-derived shapes (Folder + TipTapDocument + Artifact validate at once).
    #[test]
    fn shacl_oracle_workspace_conforms() {
        let store = Store::new().expect("store");
        reconcile_workspace_snapshot(&store, GID, &workspace_snapshot())
            .expect("reconcile workspace");

        let projection = read_back_entity_projection(&store);
        assert!(
            !projection.is_empty(),
            "the workspace projected entity triples"
        );
        // All three entity classes present.
        let types: std::collections::BTreeSet<String> = projection
            .iter()
            .filter(|(_, p, _)| p == &RDF_TYPE)
            .filter_map(|(_, _, o)| match o {
                Term::Uri(n) => Some(n.as_str().to_string()),
                _ => None,
            })
            .collect();
        for iri in entity_class_iris() {
            assert!(types.contains(&iri), "class {iri} present in projection");
        }

        let result = validate_desired_structured(&projection, workspace_vocabulary());
        assert!(
            result.is_ok(),
            "the REAL workspace entity projection must conform to the vocab-derived \
             multi-class shapes (a violation = materializer↔vocab drift): {result:#?}"
        );
    }

    /// TEETH #1: a CLOSED-shape violation — a rogue predicate on a folder is rejected.
    #[test]
    fn shacl_oracle_workspace_teeth_rogue_predicate() {
        let store = Store::new().expect("store");
        reconcile_workspace_snapshot(&store, GID, &workspace_snapshot())
            .expect("reconcile workspace");

        let mut bent = read_back_entity_projection(&store);
        let fold = format!("urn:mnemosyne:local:graph:{GID}:folder:fold-1");
        bent.push((
            fold,
            "http://example.org/not-in-the-workspace-contract".to_string(),
            Term::Lit(oxigraph::model::Literal::new_simple_literal("rogue")),
        ));
        let result = validate_desired(&bent, workspace_vocabulary());
        assert!(
            result.is_err(),
            "a predicate outside the closed workspace contract must be rejected"
        );
        assert!(result.unwrap_err().starts_with("SHACL:"));
    }

    /// TEETH #2: a wrong-datatype projection — mdoc:order carrying a STRING where the
    /// contract declares xsd:float — is REJECTED (sh:datatype).
    #[test]
    fn shacl_oracle_workspace_teeth_wrong_datatype() {
        let store = Store::new().expect("store");
        reconcile_workspace_snapshot(&store, GID, &workspace_snapshot())
            .expect("reconcile workspace");

        let mut bent = read_back_entity_projection(&store);
        let order_p = format!("{MDOC_NS}order");
        let fold = format!("urn:mnemosyne:local:graph:{GID}:folder:fold-1");
        bent.retain(|(s, p, _)| !(s == &fold && p == &order_p));
        bent.push((
            fold,
            order_p,
            Term::Lit(oxigraph::model::Literal::new_simple_literal("not-a-float")),
        ));
        let result = validate_desired(&bent, workspace_vocabulary());
        assert!(
            result.is_err(),
            "mdoc:order carrying a string (declared xsd:float) must be rejected"
        );
        assert!(result.unwrap_err().starts_with("SHACL:"));
    }

    /// SHACL-INEXPRESSIBLE COMPLEMENT (hand-checked, NOT silently skipped): the
    /// once-at-seed ONTOLOGY block. The class-level rdfs:subClassOf triples (subjects
    /// are CLASS IRIs with no rdf:type) match NO instance shape, so they are validated
    /// here by direct SPARQL read-back of the PERSISTED store, AND shown idempotent
    /// (a second reconcile emits 0 instance ops, the ontology is not re-counted).
    #[test]
    fn shacl_complement_workspace_ontology_seeded_and_idempotent() {
        let store = Store::new().expect("store");
        let first = reconcile_workspace_snapshot(&store, GID, &workspace_snapshot())
            .expect("reconcile workspace");
        assert!(first.op_count() > 0, "first reconcile does instance ops");

        let g = workspace_projection_graph_iri(GID);
        // mdoc:Image rdfs:subClassOf mdoc:Artifact is in the graph (the artifact-kind
        // ontology). It is a CLASS-LEVEL triple — the subject mdoc:Image has no rdf:type.
        let subclass = "http://www.w3.org/2000/01/rdf-schema#subClassOf";
        let q = format!(
            "ASK {{ GRAPH <{g}> {{ <{MDOC_NS}Image> <{subclass}> <{MDOC_NS}Artifact> }} }}"
        );
        let has_subclass = match SparqlEvaluator::new()
            .parse_query(&q)
            .expect("parse subclass ask")
            .on_store(&store)
            .execute()
            .expect("execute subclass ask")
        {
            QueryResults::Boolean(b) => b,
            _ => panic!("expected ASK boolean"),
        };
        assert!(
            has_subclass,
            "INVARIANT (SHACL-inexpressible): the ontology block (mdoc:Image rdfs:subClassOf \
             mdoc:Artifact) is seeded into the workspace graph"
        );

        // The class-IRI subject has NO rdf:type (it IS a class, not an instance) —
        // confirming why no instance shape can target it.
        let qt = format!("ASK {{ GRAPH <{g}> {{ <{MDOC_NS}Image> <{RDF_TYPE}> ?t }} }}");
        let class_has_type = match SparqlEvaluator::new()
            .parse_query(&qt)
            .expect("parse class-type ask")
            .on_store(&store)
            .execute()
            .expect("execute class-type ask")
        {
            QueryResults::Boolean(b) => b,
            _ => panic!("expected ASK boolean"),
        };
        assert!(
            !class_has_type,
            "the ontology class subject has NO rdf:type — SHACL-inexpressible by the instance-shape model"
        );

        // IDEMPOTENT: a second reconcile of the same snapshot emits 0 ops (the ontology
        // seed is set-merged, not re-counted; instances converge).
        let second = reconcile_workspace_snapshot(&store, GID, &workspace_snapshot())
            .expect("re-reconcile workspace");
        assert_eq!(
            second.op_count(),
            0,
            "ONTOLOGY IDEMPOTENT + instances converged: re-reconcile emits 0 ops"
        );

        // And the entity projection still conforms after the convergence round.
        let projection = read_back_entity_projection(&store);
        assert!(
            validate_desired(&projection, workspace_vocabulary()).is_ok(),
            "the entity projection still conforms after the idempotent re-seed"
        );
    }
}
