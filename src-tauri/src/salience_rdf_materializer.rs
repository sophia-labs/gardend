use crate::{
    emporium::contract::salience_vocabulary,
    emporium::reconcile::{reconcile_class_validated, ClassScope, Placement, SpanKey},
    emporium::terms::{Triple, TripleDiff},
    ids::url_component,
    rdf::{
        document_subject, format_rdf_triple, graph_subject, push_float_triple, push_integer_triple,
        push_string_triple, push_uri_triple, sparql_string_literal, RdfTriple,
    },
    rdf_authority::{salience_projection_graph_iri, salience_projection_graph_iri_for},
    rdf_record_materializer::rdf_triple_to_term,
    rdf_service::open_graph_store,
    runtime_config::{MDOC_NS, MNEMO_NS, RDF_TYPE},
    salience_value_store::{record_has_value_score, LocalValueStore},
};
use oxigraph::sparql::SparqlEvaluator;
use std::path::Path;

fn value_subject(graph_id: &str, document_id: &str, block_id: &str) -> String {
    format!(
        "{}value/{}/{}",
        graph_subject(graph_id),
        url_component(document_id),
        url_component(block_id)
    )
}

/// The OLD wholesale teardown-and-rebuild path. The 3 live salience call sites
/// now route through [`reconcile_value_store`]; this STAYS as the P3b oracle
/// baseline (the equivalence/domination tests run it against `reconcile_class` on
/// twin stores). Only the `#[cfg(test)]` oracle references it now, so it is
/// `dead_code` in a non-test build — annotated, not deleted: it is the ground
/// truth the reconcile path is proven against.
#[cfg_attr(not(test), allow(dead_code))]
pub(super) fn materialize_value_store(
    graph_dir: &Path,
    store: &LocalValueStore,
) -> Result<(), String> {
    let oxi_store = open_graph_store(graph_dir)?;
    let authority_graph = salience_projection_graph_iri_for(&store.graph_id, &store.observer);
    let graph_id_literal = sparql_string_literal(&store.graph_id);
    // NAMED-graph parity with the reconcile path (Placement::Named): the wart fix
    // moved salience out of the default graph into `:projection:salience`, so this
    // oracle baseline must DELETE/INSERT in the SAME named graph the reconcile
    // path writes — else the EQUIVALENCE oracle compares two different graphs.
    let delete = format!(
        r#"
PREFIX mnemo: <{MNEMO_NS}>
DELETE {{ GRAPH <{authority_graph}> {{ ?value ?p ?o . }} }}
WHERE {{
  GRAPH <{authority_graph}> {{
  ?value a mnemo:BlockValuation ;
    mnemo:graphId {graph_id_literal} ;
    ?p ?o .
  }}
}}
"#
    );
    SparqlEvaluator::new()
        .parse_update(&delete)
        .map_err(|error| format!("parse value cleanup update: {error}"))?
        .on_store(&oxi_store)
        .execute()
        .map_err(|error| format!("clear value store RDF: {error}"))?;

    let triples = salience_value_triples(store);

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
        .map_err(|error| format!("parse value materialization update: {error}"))?
        .on_store(&oxi_store)
        .execute()
        .map_err(|error| format!("materialize value store RDF: {error}"))
}

/// The pure per-block `BlockValuation` triple builder — the INSERT side of
/// `materialize_value_store`, extracted verbatim so the wholesale path and the
/// reconcile `desired` stay in lockstep (one source of truth for the projection).
///
/// SPARSE-skip preserved: an unvalued+untagged block (`!record_has_value_score &&
/// tags.is_empty()`) emits ZERO triples (the `continue`). Builds the dual-namespace
/// (`mnemo:`/`mdoc:`) fan-out + the optional `lastValuatedAt` / `userImportance` /
/// `userValence` / `tag` guards, exactly as the old loop did.
fn salience_value_triples(store: &LocalValueStore) -> Vec<RdfTriple> {
    let mut triples = Vec::new();
    for record in store.blocks.values() {
        if !record_has_value_score(record) && record.tags.is_empty() {
            continue;
        }
        let subject = value_subject(&store.graph_id, &record.document_id, &record.block_id);
        let block_uri = format!(
            "{}#block-{}",
            document_subject(&record.document_id),
            record.block_id
        );
        push_uri_triple(
            &mut triples,
            &subject,
            RDF_TYPE,
            &format!("{MNEMO_NS}BlockValuation"),
        );
        push_string_triple(
            &mut triples,
            &subject,
            &format!("{MNEMO_NS}graphId"),
            &store.graph_id,
        );
        push_string_triple(
            &mut triples,
            &subject,
            &format!("{MNEMO_NS}documentId"),
            &record.document_id,
        );
        push_string_triple(
            &mut triples,
            &subject,
            &format!("{MNEMO_NS}blockId"),
            &record.block_id,
        );
        push_uri_triple(
            &mut triples,
            &subject,
            &format!("{MNEMO_NS}targetsBlock"),
            &block_uri,
        );
        push_uri_triple(
            &mut triples,
            &subject,
            &format!("{MDOC_NS}blockRef"),
            &block_uri,
        );
        push_float_triple(
            &mut triples,
            &subject,
            &format!("{MDOC_NS}rawImportanceSum"),
            record.raw_importance_sum,
        );
        push_integer_triple(
            &mut triples,
            &subject,
            &format!("{MDOC_NS}importanceCount"),
            record.importance_count as i64,
        );
        push_float_triple(
            &mut triples,
            &subject,
            &format!("{MNEMO_NS}cumulativeImportance"),
            record.cumulative_importance,
        );
        push_float_triple(
            &mut triples,
            &subject,
            &format!("{MDOC_NS}cumulativeImportance"),
            record.cumulative_importance,
        );
        push_float_triple(
            &mut triples,
            &subject,
            &format!("{MDOC_NS}rawValenceSum"),
            record.raw_valence_sum,
        );
        push_integer_triple(
            &mut triples,
            &subject,
            &format!("{MDOC_NS}valenceCount"),
            record.valence_count as i64,
        );
        push_float_triple(
            &mut triples,
            &subject,
            &format!("{MNEMO_NS}cumulativeValence"),
            record.cumulative_valence,
        );
        push_float_triple(
            &mut triples,
            &subject,
            &format!("{MDOC_NS}cumulativeValence"),
            record.cumulative_valence,
        );
        push_integer_triple(
            &mut triples,
            &subject,
            &format!("{MNEMO_NS}valuationCount"),
            record.valuation_count as i64,
        );
        if !record.last_valuated_at.is_empty() {
            push_string_triple(
                &mut triples,
                &subject,
                &format!("{MNEMO_NS}lastValuatedAt"),
                &record.last_valuated_at,
            );
            push_string_triple(
                &mut triples,
                &subject,
                &format!("{MDOC_NS}lastValuatedAt"),
                &record.last_valuated_at,
            );
        }
        if let Some(user_importance) = record.user_importance {
            push_float_triple(
                &mut triples,
                &subject,
                &format!("{MDOC_NS}userImportance"),
                user_importance,
            );
        }
        if let Some(user_valence) = record.user_valence {
            push_float_triple(
                &mut triples,
                &subject,
                &format!("{MDOC_NS}userValence"),
                user_valence,
            );
        }
        for tag in &record.tags {
            push_string_triple(&mut triples, &subject, &format!("{MNEMO_NS}tag"), tag);
            push_string_triple(&mut triples, &subject, &format!("{MDOC_NS}tag"), tag);
        }
    }
    triples
}

/// `project(source) -> desired` for the salience Meaningful Object: the per-block
/// `BlockValuation` triples [`salience_value_triples`] builds, bridged
/// `RdfTriple -> Triple` through the SAME `rdf_triple_to_term` round-trip the
/// Document/graph MOs use — MANDATORY so the bridged `desired` serializes
/// byte-identically to what [`survey_class`] reparses out of the store (else the
/// value-diff reads parity as drift and salience never converges). Sparse-skip is
/// inherited from the builder.
///
/// [`survey_class`]: crate::emporium::reconcile::survey_class
fn salience_desired(store: &LocalValueStore) -> Vec<Triple> {
    salience_value_triples(store)
        .iter()
        .map(rdf_triple_to_term)
        .collect()
}

/// The salience [`ClassScope`]: the `:projection:salience` NAMED graph
/// ([`Placement::Named`] — the default-graph WART is FIXED, salience now wraps in
/// `GRAPH <…:projection:salience> { … }` like every other projection kind),
/// keyed on the single `Fixed` `mnemo:BlockValuation` class, with the
/// single-cell-constancy `mnemo:graphId "gid"` conjunct so the in-store
/// survey/reclaim only touches THIS cell's valuations. The Rust image of the
/// wholesale DELETE's WHERE pattern (now also named-graph-scoped).
fn salience_scope(store: &LocalValueStore) -> ClassScope {
    ClassScope {
        placement: Placement::Named(salience_projection_graph_iri_for(
            &store.graph_id,
            &store.observer,
        )),
        key: SpanKey::Fixed {
            rdf_type: format!("{MNEMO_NS}BlockValuation"),
        },
        graph_id_conjunct: Some((format!("{MNEMO_NS}graphId"), store.graph_id.clone())),
        subjects: None,
    }
}

/// ADDITIVE entry point: reconcile the salience projection by VALUE-DIFF instead
/// of the wholesale teardown-and-rebuild [`materialize_value_store`] does. Opens
/// the store (the thin `&Path` wrapper — minimal call-site change; the `&Store`
/// primitive `reconcile_class` is what the oracle clones into) and runs
/// `reconcile_class(&store, &salience_scope, &salience_desired)`.
///
/// PURE PARITY: reaches the SAME `:projection:salience` named-graph projection as
/// `materialize_value_store` (same `desired`, same span), by MINIMAL delta — a
/// converged save emits 0 ops; a single re-valuation emits only the changed
/// slots. The default-graph WART is now FIXED (salience moved to a named graph,
/// mirroring `:projection:document`/`:projection:workspace`). Returns the
/// structured [`TripleDiff`] it applied.
///
/// WIRED to the 3 live salience call sites (salience_service.rs:98,
/// salience_route_service.rs:54, salience_mcp_valuation.rs:130) — the LIVE save
/// path. The wholesale [`materialize_value_store`] is retained only as the oracle
/// baseline.
pub(super) fn reconcile_value_store(
    graph_dir: &Path,
    store: &LocalValueStore,
) -> Result<TripleDiff, String> {
    let oxi_store = open_graph_store(graph_dir)?;
    let scope = salience_scope(store);
    let desired = salience_desired(store);
    // S6: contract metadata made load-bearing — the `emporium-salience` retrofit
    // shapes now GATE this write (was `contract: None`, gating nothing). This is an
    // OBSERVE seam, not a trust boundary: salience is a deterministic re-projection
    // from a trusted `LocalValueStore`, so a violation here signals materializer↔vocab
    // DRIFT (the `shacl_oracle_salience_conforms` oracle proves the real projection
    // conforms today), not hostile input. A loud HALT on drift is the point.
    reconcile_class_validated(&oxi_store, &scope, &desired, Some(salience_vocabulary()))
}

// ============================================================================
// P3b ORACLE — ties the Lean salience model (Vocab/Salience.lean) to the REAL
// `materialize_value_store`. No mocks: we build a real `LocalValueStore`, run the
// REAL materializer against a real on-disk Oxigraph store, and read the DEFAULT
// graph back with SPARQL (there is NO `:projection:salience` NAMED graph — the
// materializer emits no `GRAPH <…>` clause, so the projection lives in the default
// graph; verified salience_rdf_materializer.rs:29-39/192 + RECON-RUST).
//
// Four corpora, one per structural claim the Lean model makes:
//   (1) CONTENT      — the projected set EQUALS a model built INDEPENDENTLY from
//                      the record fields + namespace constants (never re-running the
//                      materializer; the model fn calls NO materializer helper).
//                      Anti-tautology guard: a bent model must NOT match.
//   (2) SPARSE       — Target B. An UNVALUED block (no score, empty tags) emits ZERO
//                      triples (the rs:49 `continue`), while a VALUED block in the
//                      SAME store DOES emit — proving selectivity, not a dead store.
//   (3) STALE-CLASS  — Target A. A stale `mnemo:BlockValuation` subject pre-seeded
//                      into the store but ABSENT from the new value store is reclaimed
//                      by its IN-STORE rdf:type (the type-keyed DELETE…WHERE reads the
//                      store, not the snapshot).
//   (4) BLOCK-EXISTS — D1 code-read catch. A valuation referencing a NON-existent
//                      block still emits its targetsBlock/blockRef edge (referential
//                      integrity UNENFORCED) — pinned POSITIVELY (present, not absent).
// ============================================================================
#[cfg(test)]
mod p3b_oracle {
    use super::*;
    use crate::rdf_query_service::execute_sparql_query;
    use crate::runtime_config::XSD_NS;
    use crate::salience_value_config::default_value_config;
    use crate::salience_value_store::{LocalBlockValueRecord, LocalValueStore};
    use std::collections::BTreeSet;
    use uuid::Uuid;

    const GID: &str = "graph-a";

    /// A fresh, unique on-disk graph dir (the global store cache in
    /// `rdf_store_service` keys by `graph_dir/store.oxigraph`, so a unique dir per
    /// test is REQUIRED to avoid cross-test bleed). RAII-cleaned by `TempGraph`.
    struct TempGraph {
        dir: std::path::PathBuf,
    }
    impl TempGraph {
        fn new() -> Self {
            let dir =
                std::env::temp_dir().join(format!("sophia-salience-oracle-{}", Uuid::new_v4()));
            std::fs::create_dir_all(&dir).expect("create temp graph dir");
            TempGraph { dir }
        }
        fn path(&self) -> &std::path::Path {
            &self.dir
        }
    }
    impl Drop for TempGraph {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    /// Build a real `LocalValueStore` over the given records (config = the real
    /// default; this is the actual runtime struct, not a mock).
    fn store_with(records: Vec<LocalBlockValueRecord>) -> LocalValueStore {
        let mut blocks = std::collections::BTreeMap::new();
        for r in records {
            blocks.insert(format!("{}:{}", r.document_id, r.block_id), r);
        }
        LocalValueStore {
            schema_version: 1,
            graph_id: GID.to_string(),
            observer: String::new(),
            config: default_value_config(),
            config_history: Vec::new(),
            blocks,
        }
    }

    /// Read EVERY triple of the `:projection:salience` NAMED graph back as a
    /// normalized `S P O` set (oxigraph term `to_string()` rendering). The wart fix
    /// moved salience off the default graph, so the read-back must scope the named
    /// graph. This is the REAL store read-back.
    fn read_default_set(graph_dir: &std::path::Path) -> BTreeSet<String> {
        let store = open_graph_store(graph_dir).expect("open store");
        let g = salience_projection_graph_iri(GID);
        let result = execute_sparql_query(
            &store,
            &format!("SELECT ?s ?p ?o WHERE {{ GRAPH <{g}> {{ ?s ?p ?o }} }}"),
        )
        .expect("query salience graph");
        result
            .rows
            .iter()
            .map(|row| format!("{} {} {}", row["s"], row["p"], row["o"]))
            .collect()
    }

    /// All `P O` pairs for one subject in the `:projection:salience` named graph.
    fn subject_pairs(graph_dir: &std::path::Path, subject: &str) -> BTreeSet<String> {
        let store = open_graph_store(graph_dir).expect("open store");
        let g = salience_projection_graph_iri(GID);
        let result = execute_sparql_query(
            &store,
            &format!("SELECT ?p ?o WHERE {{ GRAPH <{g}> {{ <{subject}> ?p ?o }} }}"),
        )
        .expect("query subject");
        result
            .rows
            .iter()
            .map(|row| format!("{} {}", row["p"], row["o"]))
            .collect()
    }

    /// Seed a raw triple into the `:projection:salience` named graph WITHOUT going
    /// through the materializer (so the stale-class corpus is genuinely
    /// independent) — but in the SAME graph the named-graph survey reaches.
    fn seed_raw(graph_dir: &std::path::Path, s: &str, p: &str, o_term: &str) {
        let store = open_graph_store(graph_dir).expect("open store");
        let g = salience_projection_graph_iri(GID);
        let update = format!("INSERT DATA {{ GRAPH <{g}> {{ <{s}> <{p}> {o_term} }} }}");
        SparqlEvaluator::new()
            .parse_update(&update)
            .expect("parse seed")
            .on_store(&store)
            .execute()
            .expect("seed raw triple");
    }

    // --- term renderers matching oxigraph `Term::to_string()` ----------------
    // ANTI-TAUTOLOGY: these spell out the EXPECTED rendering from namespace
    // constants + literal urn prefixes. NONE of the model builders below call
    // `value_subject`, `document_subject`, `graph_subject`, `url_component`, or any
    // `push_*` helper — a bug in any of those makes real ≠ model, not both drift.
    fn uri(s: &str) -> String {
        format!("<{s}>")
    }
    fn lit(s: &str) -> String {
        format!("\"{s}\"")
    }
    fn float_lit(v: f64) -> String {
        // mirror rdf.rs:144 `float_literal` + oxigraph round-trip (verified by the
        // P3a workspace oracle: xsd:float "0" round-trips to "0"^^<xsd:float>).
        format!("\"{v}\"^^<{XSD_NS}float>")
    }
    fn int_lit(v: i64) -> String {
        format!("\"{v}\"^^<{XSD_NS}integer>")
    }

    /// INDEPENDENT minter mirror of `value_subject` (NOT calling it):
    /// `urn:mnemosyne:local:graph:{g}value/{doc}/{block}` — note `value/` glues
    /// DIRECTLY onto `{g}` (graph_subject has NO trailing separator, rdf.rs:122-124),
    /// the exact off-by-a-separator shape the Lean `valueSubject` pins (Salience.lean:251).
    /// The record ids used in these tests are url-safe, so `url_component` is identity.
    fn value_subject_model(doc: &str, block: &str) -> String {
        format!("urn:mnemosyne:local:graph:{GID}value/{doc}/{block}")
    }
    /// INDEPENDENT mirror of `document_subject(doc)#block-{block}` (rdf.rs:126-128).
    fn block_uri_model(doc: &str, block: &str) -> String {
        format!("urn:mnemosyne:local:document:{doc}#block-{block}")
    }

    /// Hand-built EXPECTED `P O` set for ONE valued record. Mirrors the Lean
    /// `valuationTriples` emitter (Salience.lean:275-290) line-for-line for the
    /// ALWAYS-ON predicates + the OPTIONAL guards (lastValuatedAt / userImportance /
    /// userValence / tags), with the dual-namespace tag/cumulative fan-out. Built
    /// purely from `r`'s fields + the namespace constants — no materializer helper.
    fn expected_pairs(r: &LocalBlockValueRecord) -> BTreeSet<String> {
        let mut out = BTreeSet::new();
        let bu = block_uri_model(&r.document_id, &r.block_id);
        out.insert(format!(
            "{} {}",
            uri(RDF_TYPE),
            uri(&format!("{MNEMO_NS}BlockValuation"))
        ));
        out.insert(format!(
            "{} {}",
            uri(&format!("{MNEMO_NS}graphId")),
            lit(GID)
        ));
        out.insert(format!(
            "{} {}",
            uri(&format!("{MNEMO_NS}documentId")),
            lit(&r.document_id)
        ));
        out.insert(format!(
            "{} {}",
            uri(&format!("{MNEMO_NS}blockId")),
            lit(&r.block_id)
        ));
        out.insert(format!(
            "{} {}",
            uri(&format!("{MNEMO_NS}targetsBlock")),
            uri(&bu)
        ));
        out.insert(format!(
            "{} {}",
            uri(&format!("{MDOC_NS}blockRef")),
            uri(&bu)
        ));
        out.insert(format!(
            "{} {}",
            uri(&format!("{MDOC_NS}rawImportanceSum")),
            float_lit(r.raw_importance_sum)
        ));
        out.insert(format!(
            "{} {}",
            uri(&format!("{MDOC_NS}importanceCount")),
            int_lit(r.importance_count as i64)
        ));
        out.insert(format!(
            "{} {}",
            uri(&format!("{MNEMO_NS}cumulativeImportance")),
            float_lit(r.cumulative_importance)
        ));
        out.insert(format!(
            "{} {}",
            uri(&format!("{MDOC_NS}cumulativeImportance")),
            float_lit(r.cumulative_importance)
        ));
        out.insert(format!(
            "{} {}",
            uri(&format!("{MDOC_NS}rawValenceSum")),
            float_lit(r.raw_valence_sum)
        ));
        out.insert(format!(
            "{} {}",
            uri(&format!("{MDOC_NS}valenceCount")),
            int_lit(r.valence_count as i64)
        ));
        out.insert(format!(
            "{} {}",
            uri(&format!("{MNEMO_NS}cumulativeValence")),
            float_lit(r.cumulative_valence)
        ));
        out.insert(format!(
            "{} {}",
            uri(&format!("{MDOC_NS}cumulativeValence")),
            float_lit(r.cumulative_valence)
        ));
        out.insert(format!(
            "{} {}",
            uri(&format!("{MNEMO_NS}valuationCount")),
            int_lit(r.valuation_count as i64)
        ));
        if !r.last_valuated_at.is_empty() {
            out.insert(format!(
                "{} {}",
                uri(&format!("{MNEMO_NS}lastValuatedAt")),
                lit(&r.last_valuated_at)
            ));
            out.insert(format!(
                "{} {}",
                uri(&format!("{MDOC_NS}lastValuatedAt")),
                lit(&r.last_valuated_at)
            ));
        }
        if let Some(ui) = r.user_importance {
            out.insert(format!(
                "{} {}",
                uri(&format!("{MDOC_NS}userImportance")),
                float_lit(ui)
            ));
        }
        if let Some(uv) = r.user_valence {
            out.insert(format!(
                "{} {}",
                uri(&format!("{MDOC_NS}userValence")),
                float_lit(uv)
            ));
        }
        for tag in &r.tags {
            out.insert(format!("{} {}", uri(&format!("{MNEMO_NS}tag")), lit(tag)));
            out.insert(format!("{} {}", uri(&format!("{MDOC_NS}tag")), lit(tag)));
        }
        out
    }

    // ------------------------------------------------------------------------
    // (1) CONTENT: real projection == independently-built model.
    // ------------------------------------------------------------------------
    /// CONTENT ORACLE — the REAL `materialize_value_store` output for one fully
    /// populated valued block EQUALS the independently-built model set. Ties the Lean
    /// `valuationTriples` emitter to the runtime. Anti-tautology: `expected_pairs`
    /// never calls a materializer helper, and a bent model is rejected below.
    #[test]
    fn oracle_content_real_equals_independent_model() {
        let tg = TempGraph::new();
        let record = LocalBlockValueRecord {
            document_id: "doc-a".to_string(),
            block_id: "block-a".to_string(),
            raw_importance_sum: 3.5,
            raw_valence_sum: -1.0,
            cumulative_importance: 2.0,
            cumulative_valence: -1.0,
            importance_count: 4,
            valence_count: 2,
            valuation_count: 6,
            tags: vec!["decision".to_string(), "praxis".to_string()],
            last_valuated_at: "2026-06-21T00:00:00Z".to_string(),
            user_importance: Some(0.75),
            user_valence: None,
        };
        let store = store_with(vec![record.clone()]);

        materialize_value_store(tg.path(), &store).expect("materialize");

        let subject = value_subject_model("doc-a", "block-a");
        let real = subject_pairs(tg.path(), &subject);
        let model = expected_pairs(&record);

        let missing: Vec<_> = model.difference(&real).cloned().collect();
        let extra: Vec<_> = real.difference(&model).cloned().collect();
        eprintln!("REAL valuation face ({}):", real.len());
        for t in &real {
            eprintln!("  {t}");
        }
        eprintln!("MODEL-only (missing from real): {missing:?}");
        eprintln!("REAL-only (extra vs model):     {extra:?}");
        assert_eq!(
            real, model,
            "real materializer output must EQUAL the independent model set"
        );

        // ANTI-TAUTOLOGY GUARD: a deliberately-wrong model must NOT match — so the
        // equality above has teeth (it isn't comparing two empty/degenerate sets).
        let mut wrong = model.clone();
        wrong.insert(format!(
            "{} {}",
            uri(&format!("{MNEMO_NS}graphId")),
            lit("WRONG-GRAPH")
        ));
        assert_ne!(real, wrong, "anti-tautology: bent model rejected");
        assert!(
            !real.is_empty(),
            "the valued block actually projected something"
        );
    }

    // ------------------------------------------------------------------------
    // (2) SPARSE (Target B): unvalued emits nothing; valued is selective.
    // ------------------------------------------------------------------------
    /// SPARSE TEETH-CHECK — an UNVALUED/UNTAGGED block emits ZERO triples (the
    /// rs:49-51 `continue`), while a VALUED block in the SAME store DOES emit. This
    /// is the faithful image of `unvalued_emits_empty` (B1) + `valued_can_emit`
    /// (B1-witness): the [] is DRIVEN by the guard, proven by selectivity in one
    /// store, not by a dead/empty store.
    #[test]
    fn oracle_sparse_unvalued_emits_nothing_valued_does() {
        let tg = TempGraph::new();

        // UNVALUED: no score (all zero/none counts+sums), empty tags ⇒ skip.
        let unvalued = LocalBlockValueRecord {
            document_id: "doc-u".to_string(),
            block_id: "block-u".to_string(),
            ..Default::default()
        };
        // Confirm our seed truly hits the skip predicate (the REAL fn, no mock).
        assert!(
            !record_has_value_score(&unvalued) && unvalued.tags.is_empty(),
            "precondition: the unvalued record really is unvalued+untagged (hits rs:49 continue)"
        );

        // VALUED: a non-zero importance_count ⇒ has value score ⇒ emits.
        let valued = LocalBlockValueRecord {
            document_id: "doc-v".to_string(),
            block_id: "block-v".to_string(),
            importance_count: 1,
            cumulative_importance: 1.0,
            valuation_count: 1,
            ..Default::default()
        };
        assert!(
            record_has_value_score(&valued),
            "precondition: the valued record really has a value score"
        );

        let store = store_with(vec![unvalued, valued]);
        materialize_value_store(tg.path(), &store).expect("materialize");

        let unvalued_subject = value_subject_model("doc-u", "block-u");
        let valued_subject = value_subject_model("doc-v", "block-v");

        assert!(
            subject_pairs(tg.path(), &unvalued_subject).is_empty(),
            "SPARSE: unvalued/untagged block emits ZERO triples (rs:49 continue)"
        );
        assert!(
            !subject_pairs(tg.path(), &valued_subject).is_empty(),
            "SELECTIVITY: a valued block in the SAME store DOES emit (the [] is guard-driven)"
        );
        // And the valued block's type head is present (ties to the class span).
        let head = format!(
            "{} {}",
            uri(RDF_TYPE),
            uri(&format!("{MNEMO_NS}BlockValuation"))
        );
        assert!(
            subject_pairs(tg.path(), &valued_subject).contains(&head),
            "valued block emits its rdf:type head (the inScopeClass key)"
        );
    }

    /// SPARSE companion — a store where EVERY block is unvalued projects an EMPTY
    /// default graph (the rs:184-186 whole-store early-return: the INSERT is skipped
    /// entirely). The pure `unvalued → []` invariant at the store level.
    #[test]
    fn oracle_all_unvalued_projects_empty() {
        let tg = TempGraph::new();
        let a = LocalBlockValueRecord {
            document_id: "d1".to_string(),
            block_id: "b1".to_string(),
            ..Default::default()
        };
        let b = LocalBlockValueRecord {
            document_id: "d2".to_string(),
            block_id: "b2".to_string(),
            ..Default::default()
        };
        let store = store_with(vec![a, b]);
        materialize_value_store(tg.path(), &store).expect("materialize");
        assert!(
            read_default_set(tg.path()).is_empty(),
            "all-unvalued ⇒ the whole default graph is empty (rs:184 early-return)"
        );
    }

    // ------------------------------------------------------------------------
    // (3) STALE-CLASS (Target A): type-keyed DELETE reclaims context-dependently.
    // ------------------------------------------------------------------------
    /// STALE-CLASS RECLAIM — a stale `mnemo:BlockValuation` subject pre-seeded into
    /// the store (typed + carrying the cell's graphId + payload) but ABSENT from the
    /// new value store is RECLAIMED by the type-keyed DELETE…WHERE (which reads the
    /// IN-STORE rdf:type + graphId, not the value store). The runtime image of
    /// `mem_equiv_class` / the context-dependent class span. A fresh valued block
    /// IS present (the INSERT half ran).
    #[test]
    fn oracle_stale_blockvaluation_reclaimed() {
        let tg = TempGraph::new();
        // A stale valuation subject for a block no longer in the value store.
        let stale = value_subject_model("doc-ghost", "block-ghost");
        seed_raw(
            tg.path(),
            &stale,
            RDF_TYPE,
            &uri(&format!("{MNEMO_NS}BlockValuation")),
        );
        // It MUST carry the cell graphId to be in the type-keyed WHERE's scope.
        seed_raw(tg.path(), &stale, &format!("{MNEMO_NS}graphId"), &lit(GID));
        seed_raw(
            tg.path(),
            &stale,
            &format!("{MNEMO_NS}valuationCount"),
            &int_lit(9),
        );

        assert!(
            !subject_pairs(tg.path(), &stale).is_empty(),
            "precondition: stale valuation seeded"
        );

        // New value store: a DIFFERENT, valued block; the ghost is absent.
        let fresh = LocalBlockValueRecord {
            document_id: "doc-fresh".to_string(),
            block_id: "block-fresh".to_string(),
            importance_count: 2,
            cumulative_importance: 1.5,
            valuation_count: 2,
            ..Default::default()
        };
        let store = store_with(vec![fresh]);
        materialize_value_store(tg.path(), &store).expect("materialize");

        // RECLAIM: the stale valuation's triples are GONE — deleted by the type-keyed
        // span keyed on its IN-STORE rdf:type + graphId, not the value store.
        assert!(
            subject_pairs(tg.path(), &stale).is_empty(),
            "Target A: stale BlockValuation reclaimed by in-store type-keyed DELETE (context-dependent)"
        );
        // The fresh valuation IS present.
        assert!(
            !subject_pairs(tg.path(), &value_subject_model("doc-fresh", "block-fresh")).is_empty(),
            "fresh valuation projected (INSERT half ran)"
        );
    }

    /// STALE-CLASS TEETH companion — a stale subject NOT typed `mnemo:BlockValuation`
    /// SURVIVES the type-keyed DELETE (it is not `a mnemo:BlockValuation`), confirming
    /// the DELETE is genuinely type-keyed, NOT a blanket graph clear. (If it cleared
    /// everything, this asserts-out and the context-dependence claim is REFUTED.)
    #[test]
    fn oracle_untyped_stale_survives_type_keyed_delete() {
        let tg = TempGraph::new();
        let orphan = format!("urn:mnemosyne:local:graph:{GID}:orphan:x");
        seed_raw(
            tg.path(),
            &orphan,
            &format!("{MDOC_NS}note"),
            &lit("survivor"),
        );

        let valued = LocalBlockValueRecord {
            document_id: "doc-x".to_string(),
            block_id: "block-x".to_string(),
            importance_count: 1,
            cumulative_importance: 1.0,
            valuation_count: 1,
            ..Default::default()
        };
        materialize_value_store(tg.path(), &store_with(vec![valued])).expect("materialize");

        assert!(
            !subject_pairs(tg.path(), &orphan).is_empty(),
            "untyped orphan is NOT reclaimed: the DELETE is type-keyed, not a blanket clear"
        );
    }

    // ------------------------------------------------------------------------
    // (4) BLOCK-EXISTS (D1 code-read catch): referential integrity UNENFORCED.
    // ------------------------------------------------------------------------
    /// BLOCK-EXISTS PASSTHROUGH (D1) — a valuation whose (document_id, block_id) name
    /// a block that does NOT exist anywhere still emits its `mnemo:targetsBlock` and
    /// `doc:blockRef` edges, built STRAIGHT FROM the record's own fields with NO
    /// existence check (salience_rdf_materializer.rs:53-93). Pins the finding
    /// POSITIVELY: the dangling edge IS present. (If it were suppressed this asserts-
    /// out and D1 would be REFUTED — here we expect it PRESENT, matching the Lean
    /// `block_ref_passthrough`.)
    #[test]
    fn oracle_block_ref_unenforced_dangling_still_emitted() {
        let tg = TempGraph::new();
        // doc-ghost/block-ghost are never declared as a real block anywhere — a pure
        // reference invented from the valuation record's own fields.
        let ghost = LocalBlockValueRecord {
            document_id: "doc-ghost".to_string(),
            block_id: "block-ghost".to_string(),
            importance_count: 1,
            cumulative_importance: 1.0,
            valuation_count: 1,
            ..Default::default()
        };
        materialize_value_store(tg.path(), &store_with(vec![ghost])).expect("materialize");

        let subject = value_subject_model("doc-ghost", "block-ghost");
        let bu = block_uri_model("doc-ghost", "block-ghost");
        let pairs = subject_pairs(tg.path(), &subject);

        let targets = format!("{} {}", uri(&format!("{MNEMO_NS}targetsBlock")), uri(&bu));
        let block_ref = format!("{} {}", uri(&format!("{MDOC_NS}blockRef")), uri(&bu));
        eprintln!("dangling valuation face:");
        for t in &pairs {
            eprintln!("  {t}");
        }
        assert!(
            pairs.contains(&targets),
            "D1: dangling mnemo:targetsBlock MUST still be emitted (no existence check)"
        );
        assert!(
            pairs.contains(&block_ref),
            "D1: dangling doc:blockRef MUST still be emitted (no existence check)"
        );
    }
}

// ============================================================================
// P3b RECONCILE ORACLE — ties the GENERAL `reconcile_class` primitive (via the
// `reconcile_value_store` salience instantiation) to the wholesale
// `materialize_value_store` BASELINE, mirroring the Lean class algebra
// (`Workspace.lean`: `mem_equiv_class`, `class_ops_dominate`,
// `class_converged_zero_ops`). No mocks: real `LocalValueStore`, real on-disk
// Oxigraph, real SPARQL read-back. Four claims, one corpus shape:
//
//   (1) EQUIVALENCE — wholesale on store A and reconcile on store B, from the
//       SAME seed (identical `LocalValueStore` + thus identical `desired`),
//       reach the SAME value-canonical default-graph set. Plus an INDEPENDENT
//       expected set (built from record fields + namespace constants, never the
//       materializer) the reconcile output must equal. Anti-tautology: a bent
//       expected set is rejected.
//   (2) DOMINATION — `reconcile op_count <= wholesale op_count` always; on a
//       CONVERGED re-run, reconcile is STRICTLY less (0 vs the wholesale's
//       always-`|span| + |desired|` teardown-rebuild). This IS `class_ops_dominate`.
//   (3) CONVERGENCE — a second reconcile against the SAME desired emits 0 ops
//       (`class_converged_zero_ops`). THIS is where the step-[0] xsd:float canon
//       fix lands: without it the surveyed floats never key-equal the desired
//       floats and salience churns forever. A NONZERO here = the canon is still
//       broken (reported as a real finding, not papered over).
//   (4) SPARSE-RECLAIM — a block goes valued -> unvalued: its `BlockValuation`
//       subject is REMOVED by the diff (op_count>0, removes name that subject),
//       then a re-reconcile is 0. The reconcile image of the wholesale teardown's
//       sparse-skip.
// ============================================================================
#[cfg(test)]
mod p3b_reconcile_oracle {
    use super::*;
    use crate::rdf_query_service::execute_sparql_query;
    use crate::runtime_config::XSD_NS;
    use crate::salience_value_config::default_value_config;
    use crate::salience_value_store::{
        record_has_value_score, LocalBlockValueRecord, LocalValueStore,
    };
    use std::collections::BTreeSet;
    use uuid::Uuid;

    const GID: &str = "graph-b";

    /// Fresh unique on-disk graph dir (the global store cache keys by
    /// `graph_dir/store.oxigraph`, so a unique dir per store is REQUIRED — and is
    /// exactly what lets EQUIVALENCE hold two genuinely separate A/B stores).
    struct TempGraph {
        dir: std::path::PathBuf,
    }
    impl TempGraph {
        fn new() -> Self {
            let dir =
                std::env::temp_dir().join(format!("sophia-salience-recon-{}", Uuid::new_v4()));
            std::fs::create_dir_all(&dir).expect("create temp graph dir");
            TempGraph { dir }
        }
        fn path(&self) -> &std::path::Path {
            &self.dir
        }
    }
    impl Drop for TempGraph {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    /// A real `LocalValueStore` over the records (real runtime struct, real config).
    fn store_with(records: Vec<LocalBlockValueRecord>) -> LocalValueStore {
        let mut blocks = std::collections::BTreeMap::new();
        for r in records {
            blocks.insert(format!("{}:{}", r.document_id, r.block_id), r);
        }
        LocalValueStore {
            schema_version: 1,
            graph_id: GID.to_string(),
            observer: String::new(),
            config: default_value_config(),
            config_history: Vec::new(),
            blocks,
        }
    }

    /// Read the whole `:projection:salience` named graph back as a normalized
    /// `S P O` set (oxigraph term `to_string()`). The REAL store read-back — both
    /// wholesale and reconcile now write into the SAME named graph (the wart fix),
    /// so this is the common comparison face.
    fn read_default_set(graph_dir: &std::path::Path) -> BTreeSet<String> {
        let store = open_graph_store(graph_dir).expect("open store");
        let g = salience_projection_graph_iri(GID);
        let result = execute_sparql_query(
            &store,
            &format!("SELECT ?s ?p ?o WHERE {{ GRAPH <{g}> {{ ?s ?p ?o }} }}"),
        )
        .expect("query salience graph");
        result
            .rows
            .iter()
            .map(|row| format!("{} {} {}", row["s"], row["p"], row["o"]))
            .collect()
    }

    /// All `P O` pairs whose subject is `subject`, in the `:projection:salience`
    /// named graph.
    fn subject_triples(graph_dir: &std::path::Path, subject: &str) -> BTreeSet<String> {
        let store = open_graph_store(graph_dir).expect("open store");
        let g = salience_projection_graph_iri(GID);
        let result = execute_sparql_query(
            &store,
            &format!("SELECT ?p ?o WHERE {{ GRAPH <{g}> {{ <{subject}> ?p ?o }} }}"),
        )
        .expect("query subject");
        result
            .rows
            .iter()
            .map(|row| format!("{} {}", row["p"], row["o"]))
            .collect()
    }

    // --- INDEPENDENT term renderers + subject minter (anti-tautology) ---------
    // NONE of these call value_subject / document_subject / graph_subject /
    // url_component / any push_* helper. A bug in those makes real != model.
    fn uri(s: &str) -> String {
        format!("<{s}>")
    }
    fn lit(s: &str) -> String {
        format!("\"{s}\"")
    }
    fn float_lit(v: f64) -> String {
        format!("\"{v}\"^^<{XSD_NS}float>")
    }
    fn int_lit(v: i64) -> String {
        format!("\"{v}\"^^<{XSD_NS}integer>")
    }
    fn value_subject_model(doc: &str, block: &str) -> String {
        format!("urn:mnemosyne:local:graph:{GID}value/{doc}/{block}")
    }
    fn block_uri_model(doc: &str, block: &str) -> String {
        format!("urn:mnemosyne:local:document:{doc}#block-{block}")
    }

    /// Hand-built EXPECTED `P O` set for ONE valued record — purely from `r`'s
    /// fields + namespace constants, no materializer helper. Mirrors the always-on
    /// predicates + optional guards (lastValuatedAt / userImportance / userValence
    /// / tags) with the dual mnemo:/mdoc: fan-out.
    fn expected_pairs(r: &LocalBlockValueRecord) -> BTreeSet<String> {
        let mut out = BTreeSet::new();
        let bu = block_uri_model(&r.document_id, &r.block_id);
        out.insert(format!(
            "{} {}",
            uri(RDF_TYPE),
            uri(&format!("{MNEMO_NS}BlockValuation"))
        ));
        out.insert(format!(
            "{} {}",
            uri(&format!("{MNEMO_NS}graphId")),
            lit(GID)
        ));
        out.insert(format!(
            "{} {}",
            uri(&format!("{MNEMO_NS}documentId")),
            lit(&r.document_id)
        ));
        out.insert(format!(
            "{} {}",
            uri(&format!("{MNEMO_NS}blockId")),
            lit(&r.block_id)
        ));
        out.insert(format!(
            "{} {}",
            uri(&format!("{MNEMO_NS}targetsBlock")),
            uri(&bu)
        ));
        out.insert(format!(
            "{} {}",
            uri(&format!("{MDOC_NS}blockRef")),
            uri(&bu)
        ));
        out.insert(format!(
            "{} {}",
            uri(&format!("{MDOC_NS}rawImportanceSum")),
            float_lit(r.raw_importance_sum)
        ));
        out.insert(format!(
            "{} {}",
            uri(&format!("{MDOC_NS}importanceCount")),
            int_lit(r.importance_count as i64)
        ));
        out.insert(format!(
            "{} {}",
            uri(&format!("{MNEMO_NS}cumulativeImportance")),
            float_lit(r.cumulative_importance)
        ));
        out.insert(format!(
            "{} {}",
            uri(&format!("{MDOC_NS}cumulativeImportance")),
            float_lit(r.cumulative_importance)
        ));
        out.insert(format!(
            "{} {}",
            uri(&format!("{MDOC_NS}rawValenceSum")),
            float_lit(r.raw_valence_sum)
        ));
        out.insert(format!(
            "{} {}",
            uri(&format!("{MDOC_NS}valenceCount")),
            int_lit(r.valence_count as i64)
        ));
        out.insert(format!(
            "{} {}",
            uri(&format!("{MNEMO_NS}cumulativeValence")),
            float_lit(r.cumulative_valence)
        ));
        out.insert(format!(
            "{} {}",
            uri(&format!("{MDOC_NS}cumulativeValence")),
            float_lit(r.cumulative_valence)
        ));
        out.insert(format!(
            "{} {}",
            uri(&format!("{MNEMO_NS}valuationCount")),
            int_lit(r.valuation_count as i64)
        ));
        if !r.last_valuated_at.is_empty() {
            out.insert(format!(
                "{} {}",
                uri(&format!("{MNEMO_NS}lastValuatedAt")),
                lit(&r.last_valuated_at)
            ));
            out.insert(format!(
                "{} {}",
                uri(&format!("{MDOC_NS}lastValuatedAt")),
                lit(&r.last_valuated_at)
            ));
        }
        if let Some(ui) = r.user_importance {
            out.insert(format!(
                "{} {}",
                uri(&format!("{MDOC_NS}userImportance")),
                float_lit(ui)
            ));
        }
        if let Some(uv) = r.user_valence {
            out.insert(format!(
                "{} {}",
                uri(&format!("{MDOC_NS}userValence")),
                float_lit(uv)
            ));
        }
        for tag in &r.tags {
            out.insert(format!("{} {}", uri(&format!("{MNEMO_NS}tag")), lit(tag)));
            out.insert(format!("{} {}", uri(&format!("{MDOC_NS}tag")), lit(tag)));
        }
        out
    }

    /// A fully-populated valued record (every optional guard ON, a real float in
    /// importance so the xsd:float canon is on the critical path of CONVERGENCE).
    fn full_record() -> LocalBlockValueRecord {
        LocalBlockValueRecord {
            document_id: "doc-a".to_string(),
            block_id: "block-a".to_string(),
            raw_importance_sum: 3.5,
            raw_valence_sum: -1.25,
            cumulative_importance: 2.0,
            cumulative_valence: -0.5,
            importance_count: 4,
            valence_count: 2,
            valuation_count: 6,
            tags: vec!["decision".to_string(), "praxis".to_string()],
            last_valuated_at: "2026-06-21T00:00:00Z".to_string(),
            user_importance: Some(0.75),
            user_valence: None,
        }
    }

    // ------------------------------------------------------------------------
    // (1) EQUIVALENCE — wholesale(A) == reconcile(B) == independent model.
    // ------------------------------------------------------------------------
    /// `mem_equiv_class`: the reconcile path reaches the EXACT same default-graph
    /// projection the wholesale path does, from an identical seed. Twin stores A
    /// (wholesale) and B (reconcile) over the SAME `LocalValueStore`; their
    /// full default-graph sets must be set-equal. Anti-tautology: the reconcile
    /// subject face also equals an INDEPENDENTLY-built expected set, and a bent
    /// expected set is rejected.
    #[test]
    fn oracle_reconcile_equivalent_to_wholesale() {
        let tg_a = TempGraph::new();
        let tg_b = TempGraph::new();
        let record = full_record();
        let store = store_with(vec![record.clone()]);

        // A: the wholesale baseline. B: the reconcile path. SAME seed.
        materialize_value_store(tg_a.path(), &store).expect("wholesale A");
        let diff = reconcile_value_store(tg_b.path(), &store).expect("reconcile B");

        let set_a = read_default_set(tg_a.path());
        let set_b = read_default_set(tg_b.path());
        let missing: Vec<_> = set_a.difference(&set_b).cloned().collect();
        let extra: Vec<_> = set_b.difference(&set_a).cloned().collect();
        eprintln!("wholesale-only (missing from reconcile): {missing:?}");
        eprintln!("reconcile-only (extra vs wholesale):     {extra:?}");
        assert_eq!(
            set_a, set_b,
            "EQUIVALENCE: reconcile default-graph set must EQUAL the wholesale set"
        );
        assert!(
            !set_b.is_empty(),
            "the valued block actually projected something"
        );
        // From empty, reconcile is pure INSERT of the whole desired (no removes).
        assert!(
            diff.removes.is_empty(),
            "from-empty reconcile removes nothing"
        );
        assert!(
            !diff.adds.is_empty(),
            "from-empty reconcile adds the projection"
        );

        // ANTI-TAUTOLOGY: reconcile's subject face equals the INDEPENDENT model.
        let subject = value_subject_model("doc-a", "block-a");
        let real = subject_triples(tg_b.path(), &subject);
        let model = expected_pairs(&record);
        assert_eq!(
            real, model,
            "reconcile subject face must EQUAL the independently-built model"
        );
        let mut wrong = model.clone();
        wrong.insert(format!(
            "{} {}",
            uri(&format!("{MNEMO_NS}graphId")),
            lit("WRONG")
        ));
        assert_ne!(real, wrong, "anti-tautology: bent model rejected");
    }

    // ------------------------------------------------------------------------
    // (2) DOMINATION — reconcile op_count <= wholesale; strictly < when converged.
    // ------------------------------------------------------------------------
    /// `class_ops_dominate`: from EMPTY both pay `|desired|` ops (reconcile = all
    /// adds; wholesale = `|span(empty)| + |desired|` = `0 + |desired|`), so
    /// reconcile <= wholesale. After CONVERGENCE, reconcile pays 0 while the
    /// wholesale STILL tears down and rebuilds (`|span| + |desired| > 0`), so
    /// reconcile is STRICTLY less — the whole point of the value-diff.
    #[test]
    fn oracle_reconcile_op_count_dominates_wholesale() {
        let tg = TempGraph::new();
        let store = store_with(vec![full_record()]);
        let desired_len = salience_desired(&store).len();
        assert!(desired_len > 0, "the fixture must actually project triples");

        // FIRST reconcile (from empty): op_count == |desired| (all adds).
        let first = reconcile_value_store(tg.path(), &store).expect("first reconcile");
        assert_eq!(
            first.op_count(),
            desired_len,
            "from-empty reconcile op_count == |desired| (pure inserts)"
        );

        // The wholesale op_count for the SAME transition, measured INDEPENDENTLY:
        // wholesale ALWAYS deletes everything in span then inserts all |desired|.
        // First transition: span was empty -> 0 deletes + |desired| inserts.
        let wholesale_first = 0 + desired_len;
        assert!(
            first.op_count() <= wholesale_first,
            "DOMINATION: reconcile <= wholesale on the first transition ({} <= {})",
            first.op_count(),
            wholesale_first
        );

        // SECOND reconcile, SAME desired: converged -> 0 ops.
        let second = reconcile_value_store(tg.path(), &store).expect("second reconcile");
        // Wholesale on the SAME no-op transition would STILL churn: it deletes the
        // |desired| triples now in span and re-inserts them = 2 * |desired| ops.
        let wholesale_second = desired_len + desired_len;
        assert_eq!(second.op_count(), 0, "converged reconcile pays 0 ops");
        assert!(
            second.op_count() < wholesale_second,
            "DOMINATION (strict): converged reconcile {} < wholesale teardown-rebuild {}",
            second.op_count(),
            wholesale_second
        );
    }

    // ------------------------------------------------------------------------
    // (3) CONVERGENCE — second reconcile == 0 ops (the xsd:float canon gate).
    // ------------------------------------------------------------------------
    /// `class_converged_zero_ops`: reconcile, then reconcile the SAME desired ->
    /// the second diff is EMPTY. The surveyed floats (xsd:float literals read back
    /// out of the store) must key-equal the desired floats; that is the step-[0]
    /// `canon_value` xsd:float collapse. If this is NONZERO the canon is broken —
    /// the test reports the residual diff verbatim and FAILS (no papering over).
    #[test]
    fn oracle_reconcile_converges_to_zero_ops() {
        let tg = TempGraph::new();
        let store = store_with(vec![full_record()]);

        let first = reconcile_value_store(tg.path(), &store).expect("first reconcile");
        assert!(
            first.op_count() > 0,
            "the first reconcile must DO something"
        );

        let second = reconcile_value_store(tg.path(), &store).expect("second reconcile");
        if second.op_count() != 0 {
            eprintln!("CONVERGENCE FAILURE — residual diff after a no-op re-reconcile:");
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
            "CONVERGENCE: a second reconcile of the same desired must be 0 ops \
             (nonzero => the xsd:float canon collapse is broken)"
        );
    }

    // ------------------------------------------------------------------------
    // (4) SPARSE-RECLAIM — valued -> unvalued removes the valuation subject.
    // ------------------------------------------------------------------------
    /// A block that becomes unvalued+untagged drops out of `salience_desired`
    /// (the sparse-skip), so the reconcile DIFF reclaims its `BlockValuation`
    /// subject: op_count>0, the removes name that subject, the subject is GONE
    /// from the store, and a follow-up reconcile is 0. The reconcile image of the
    /// wholesale teardown's sparse reclaim.
    #[test]
    fn oracle_reconcile_sparse_reclaim_on_unvalue() {
        let tg = TempGraph::new();
        let valued = full_record();
        let subject = value_subject_model(&valued.document_id, &valued.block_id);

        // 1. Value the block + reconcile -> the valuation subject exists.
        let store_valued = store_with(vec![valued.clone()]);
        let r1 = reconcile_value_store(tg.path(), &store_valued).expect("reconcile valued");
        assert!(r1.op_count() > 0, "valuing a fresh block does ops");
        assert!(
            !subject_triples(tg.path(), &subject).is_empty(),
            "precondition: the valuation subject is present after valuing"
        );

        // 2. Unvalue the SAME block (no score, no tags) -> it leaves `desired`.
        let unvalued = LocalBlockValueRecord {
            document_id: valued.document_id.clone(),
            block_id: valued.block_id.clone(),
            ..Default::default()
        };
        assert!(
            !record_has_value_score(&unvalued) && unvalued.tags.is_empty(),
            "the unvalued record really hits the sparse-skip"
        );
        let store_unvalued = store_with(vec![unvalued]);
        // sanity: the desired projection is now empty (nothing valued/tagged).
        assert!(
            salience_desired(&store_unvalued).is_empty(),
            "unvalued store projects an EMPTY desired"
        );

        let r2 = reconcile_value_store(tg.path(), &store_unvalued).expect("reconcile unvalued");
        // RECLAIM: op_count>0, and EVERY op is a removal naming the valuation subject.
        assert!(
            r2.op_count() > 0,
            "SPARSE-RECLAIM: unvaluing reclaims (op_count>0)"
        );
        assert!(r2.adds.is_empty(), "reclaim is pure removal, no adds");
        assert!(
            r2.removes.iter().all(|(s, _, _)| s == &subject),
            "every removed triple belongs to the reclaimed valuation subject"
        );
        assert!(
            subject_triples(tg.path(), &subject).is_empty(),
            "SPARSE-RECLAIM: the valuation subject is GONE from the store"
        );

        // 3. Re-reconcile the unvalued store -> already converged, 0 ops.
        let r3 = reconcile_value_store(tg.path(), &store_unvalued).expect("re-reconcile unvalued");
        assert_eq!(r3.op_count(), 0, "post-reclaim store is converged (0 ops)");
    }

    // ════════════════════════════════════════════════════════════════════════
    //  EA-2b SHACL CONFORMANCE ORACLE — salience BlockValuation kind.
    //
    //  The retrofit consistency check for the dual-namespace (mnemo:/mdoc:) +
    //  sparse salience projection: the REAL projection (`reconcile_value_store`
    //  into a real on-disk store) CONFORMS to the SHACL shapes DERIVED from the
    //  `emporium-salience` vocab contract (`vocab_to_shacl`). A non-conformance =
    //  a materializer↔vocab DRIFT BUG.
    //
    //  This is the AUTHORED-CONTRACT path (salience has no served golden — the
    //  span is code-defined in `salience_value_triples`): the contract is the
    //  minimal mirror of that span, and the closed shape + per-predicate datatype/
    //  cardinality constraints are DERIVED from it (never hand-written). The float
    //  predicates exercise the EA-2b `Datatype::float`→`xsd:float` emitter
    //  extension (the validator probe proved xsd:float does NOT satisfy
    //  xsd:double, so the extension is load-bearing).
    //
    //  NO MOCKS: real reconcile, real store, real rudof. Triples are READ BACK out
    //  of the persisted named graph (covering the store round-trip), then fed to
    //  the same `validate_desired` the live appliers use.
    //
    //  TEETH: a bent projection (required predicate DROPPED; a float predicate
    //  RETYPED to an integer literal) is REJECTED.
    // ════════════════════════════════════════════════════════════════════════

    use crate::emporium::contract::salience_vocabulary;
    use crate::emporium::shacl_validator::validate_desired;
    use crate::emporium::survey::parse_term as oracle_parse_term;
    use crate::emporium::terms::{Term as EngineTerm, Triple as EngineTriple};

    /// Read the REAL persisted valuation projection for one subject back out of the
    /// `:projection:salience` named graph as engine `Triple`s — the input shape
    /// `validate_desired` consumes. Bridges each `?o` to an engine `Term` via the
    /// proven `parse_term` round-trip (the same one the survey uses), so the
    /// validated graph is EXACTLY what the store holds (float datatype + all).
    fn read_back_valuation(graph_dir: &std::path::Path, subject: &str) -> Vec<EngineTriple> {
        let store = open_graph_store(graph_dir).expect("open store");
        let g = salience_projection_graph_iri(GID);
        let result = execute_sparql_query(
            &store,
            &format!("SELECT ?p ?o WHERE {{ GRAPH <{g}> {{ <{subject}> ?p ?o }} }}"),
        )
        .expect("query subject");
        result
            .rows
            .iter()
            .map(|row| {
                let p = row["p"]
                    .strip_prefix('<')
                    .and_then(|s| s.strip_suffix('>'))
                    .map(str::to_string)
                    .unwrap_or_else(|| row["p"].clone());
                let o = oracle_parse_term(&row["o"]);
                (subject.to_string(), p, o)
            })
            .collect()
    }

    /// CONFORMANCE: the REAL salience projection conforms to the vocab-derived
    /// shapes (a fully-populated valuation — all always-on predicates + the
    /// optional lastValuatedAt/userImportance/tags guards exercised).
    #[test]
    fn shacl_oracle_salience_conforms() {
        let tg = TempGraph::new();
        let record = full_record();
        let store = store_with(vec![record.clone()]);

        // REAL live projection path.
        reconcile_value_store(tg.path(), &store).expect("reconcile salience");

        let subject = value_subject_model(&record.document_id, &record.block_id);
        let projection = read_back_valuation(tg.path(), &subject);
        assert!(
            !projection.is_empty(),
            "PRECONDITION: the valued block must project a non-empty face"
        );

        let result = validate_desired(&projection, salience_vocabulary());
        assert!(
            result.is_ok(),
            "the REAL salience projection must conform to the vocab-derived shapes \
             (a violation = materializer↔vocab drift): {result:?}"
        );
    }

    /// TEETH #1: a projection MISSING a required predicate (mnemo:valuationCount
    /// dropped) is REJECTED — the derived shape carries `sh:minCount 1` on it.
    #[test]
    fn shacl_oracle_salience_teeth_missing_required_predicate() {
        let tg = TempGraph::new();
        let record = full_record();
        let store = store_with(vec![record.clone()]);
        reconcile_value_store(tg.path(), &store).expect("reconcile salience");

        let subject = value_subject_model(&record.document_id, &record.block_id);
        let mut bent = read_back_valuation(tg.path(), &subject);
        let dropped = format!("{MNEMO_NS}valuationCount");
        let before = bent.len();
        bent.retain(|(_, p, _)| p != &dropped);
        assert_eq!(
            bent.len(),
            before - 1,
            "exactly the valuationCount triple dropped"
        );

        let result = validate_desired(&bent, salience_vocabulary());
        assert!(
            result.is_err(),
            "a projection missing the required mnemo:valuationCount must be rejected"
        );
        assert!(
            result.unwrap_err().starts_with("SHACL:"),
            "loud-halt prefix"
        );
    }

    /// TEETH #2: a DATATYPE violation — a float predicate (mdoc:rawImportanceSum)
    /// RETYPED to an xsd:integer literal is REJECTED. This specifically exercises
    /// the `xsd:float` shape constraint derived from `Datatype::float`: had the
    /// emitter mapped it to xsd:double (or string), this teeth-check would not
    /// catch the wrong datatype.
    #[test]
    fn shacl_oracle_salience_teeth_wrong_float_datatype() {
        let tg = TempGraph::new();
        let record = full_record();
        let store = store_with(vec![record.clone()]);
        reconcile_value_store(tg.path(), &store).expect("reconcile salience");

        let subject = value_subject_model(&record.document_id, &record.block_id);
        let mut bent = read_back_valuation(tg.path(), &subject);
        let float_pred = format!("{MDOC_NS}rawImportanceSum");
        // Replace the xsd:float object with an xsd:integer object (wrong datatype).
        for (_, p, o) in bent.iter_mut() {
            if p == &float_pred {
                *o = EngineTerm::Lit(oxigraph::model::Literal::new_typed_literal(
                    "3",
                    oxigraph::model::NamedNode::new(format!("{XSD_NS}integer")).unwrap(),
                ));
            }
        }

        let result = validate_desired(&bent, salience_vocabulary());
        assert!(
            result.is_err(),
            "an xsd:integer where the contract declares xsd:float must be rejected"
        );
        assert!(result.unwrap_err().starts_with("SHACL:"));
    }
}

// ============================================================================
// PER-OBSERVER SALIENCE ORACLE — the smallest mirror of the Song/Memory observer
// seam, applied to the ONE faculty (Valuation) that was NOT witness-scoped.
//
// Before: every observer's valuations collapsed into the single
// `:projection:salience` store + named graph and SUMMED into one global
// `LocalBlockValueRecord` ("a view from nowhere" the observer-relative ontology
// forbids). After: a non-empty observer routes the value store
// (`values/{observer}/block-values.json`) AND the named graph
// (`…:projection:salience:agent:{observer}`) — so two witnesses' valuations of the
// SAME block are INDEPENDENT and do not sum.
//
// NO MOCKS: the test drives the EXACT functions the live `mcp_local_value` handler
// calls — `read_value_store_for` (per-observer file), the same record-sum mutation
// the handler does, `write_value_store`, `reconcile_value_store` (real on-disk
// Oxigraph projection) — then reads the per-observer named graph back with real
// SPARQL. Distinct dirs are NOT used: ONE graph dir holds all witnesses, exactly as
// production (the witnesses are separated by the observer segment, not by dir).
//
// TEETH: the empty-observer commons case still lands in the un-segmented
// `:projection:salience` graph + `values/block-values.json`, byte-for-byte today's
// behavior; and the two observer graphs are DISJOINT (neither contains the other's
// subject), proving the sum was actually broken (not merely re-labeled).
// ============================================================================
#[cfg(test)]
mod per_observer_oracle {
    use super::*;
    use crate::rdf_query_service::execute_sparql_query;
    use crate::salience_value_store::{
        block_value_key, cumulative_importance, read_value_store, read_value_store_for,
        write_value_store, LocalBlockValueRecord,
    };
    use std::collections::BTreeSet;
    use uuid::Uuid;

    const GID: &str = "graph-obs";

    struct TempGraph {
        dir: std::path::PathBuf,
    }
    impl TempGraph {
        fn new() -> Self {
            let dir =
                std::env::temp_dir().join(format!("sophia-salience-observer-{}", Uuid::new_v4()));
            std::fs::create_dir_all(&dir).expect("create temp graph dir");
            TempGraph { dir }
        }
        fn path(&self) -> &std::path::Path {
            &self.dir
        }
    }
    impl Drop for TempGraph {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn value_subject_model(doc: &str, block: &str) -> String {
        format!("urn:mnemosyne:local:graph:{GID}value/{doc}/{block}")
    }

    /// All `P O` pairs for one subject in a SPECIFIC named graph — the real store
    /// read-back, scoped to the witness's graph IRI.
    fn subject_pairs_in(
        graph_dir: &std::path::Path,
        graph_iri: &str,
        subject: &str,
    ) -> BTreeSet<String> {
        let store = open_graph_store(graph_dir).expect("open store");
        let result = execute_sparql_query(
            &store,
            &format!("SELECT ?p ?o WHERE {{ GRAPH <{graph_iri}> {{ <{subject}> ?p ?o }} }}"),
        )
        .expect("query subject");
        result
            .rows
            .iter()
            .map(|row| format!("{} {}", row["p"], row["o"]))
            .collect()
    }

    /// The cumulativeImportance literal for a subject in a named graph (the value
    /// whose SUM is what we are proving stays independent). `None` if absent.
    fn cumulative_importance_in(
        graph_dir: &std::path::Path,
        graph_iri: &str,
        subject: &str,
    ) -> Option<String> {
        let store = open_graph_store(graph_dir).expect("open store");
        let pred = format!("{MNEMO_NS}cumulativeImportance");
        let result = execute_sparql_query(
            &store,
            &format!("SELECT ?o WHERE {{ GRAPH <{graph_iri}> {{ <{subject}> <{pred}> ?o }} }}"),
        )
        .expect("query cumulativeImportance");
        result.rows.first().map(|row| row["o"].clone())
    }

    /// Value a block AS A WITNESS — the EXACT sequence `mcp_local_value` runs for one
    /// importance entry: read THIS observer's store, add to the record's raw sum +
    /// count, recompute cumulative, persist, reconcile. Real functions, no mock.
    fn value_block_as(
        graph_dir: &std::path::Path,
        observer: &str,
        doc: &str,
        block: &str,
        importance: f64,
    ) {
        let mut store = read_value_store_for(graph_dir, GID, observer).expect("read per-observer");
        let key = block_value_key(doc, block);
        let record = store
            .blocks
            .entry(key)
            .or_insert_with(|| LocalBlockValueRecord {
                document_id: doc.to_string(),
                block_id: block.to_string(),
                ..LocalBlockValueRecord::default()
            });
        record.document_id = doc.to_string();
        record.block_id = block.to_string();
        record.raw_importance_sum += importance;
        record.importance_count += 1;
        record.cumulative_importance = cumulative_importance(record.raw_importance_sum);
        record.valuation_count = record.importance_count + record.valence_count;
        record.last_valuated_at = "2026-06-23T00:00:00Z".to_string();
        write_value_store(graph_dir, &store).expect("write per-observer store");
        let _diff = reconcile_value_store(graph_dir, &store).expect("reconcile per-observer");
    }

    /// DONE-CRITERION: two distinct observers value the SAME block; their valuations
    /// land in DISTINCT `:projection:salience:agent:{id}` named graphs and DO NOT sum.
    #[test]
    fn two_observers_same_block_do_not_sum() {
        let tg = TempGraph::new();
        let (doc, block) = ("doc-shared", "block-shared");
        let subject = value_subject_model(doc, block);

        // Observer ALPHA values with importance 5; BETA values the SAME block with 3.
        // In the OLD global store these would land in ONE record and SUM (raw 8).
        value_block_as(tg.path(), "agent-alpha", doc, block, 5.0);
        value_block_as(tg.path(), "agent-beta", doc, block, 3.0);

        let alpha_g = salience_projection_graph_iri_for(GID, "agent-alpha");
        let beta_g = salience_projection_graph_iri_for(GID, "agent-beta");
        assert_ne!(alpha_g, beta_g, "the two witness graphs are distinct IRIs");

        // Each witness's valuation lives in ITS OWN named graph.
        let alpha_pairs = subject_pairs_in(tg.path(), &alpha_g, &subject);
        let beta_pairs = subject_pairs_in(tg.path(), &beta_g, &subject);
        assert!(
            !alpha_pairs.is_empty(),
            "alpha's valuation projected into alpha's graph"
        );
        assert!(
            !beta_pairs.is_empty(),
            "beta's valuation projected into beta's graph"
        );

        // INDEPENDENCE (the sum is broken): alpha's graph carries NO beta valuation and
        // vice-versa — each cumulativeImportance reflects only that witness's own
        // importance, never the global sum.
        let alpha_ci = cumulative_importance_in(tg.path(), &alpha_g, &subject)
            .expect("alpha has a cumulativeImportance");
        let beta_ci = cumulative_importance_in(tg.path(), &beta_g, &subject)
            .expect("beta has a cumulativeImportance");
        // The two witnesses valued the SAME block with DIFFERENT importances; their
        // projected cumulativeImportance MUST differ — if they had summed into one
        // record (or shared a graph) both reads would return the same summed value.
        assert_ne!(
            alpha_ci, beta_ci,
            "the witnesses' cumulativeImportance differ (no sum): alpha={alpha_ci} beta={beta_ci}"
        );
        // Each equals that witness's OWN importance projected — neither equals the
        // would-be global sum (raw 8.0). The store persists xsd:float, so compare
        // through the SAME f32 canon the projection lands at (not full f64 display).
        let canon = |v: f64| format!("\"{}\"", cumulative_importance(v) as f32);
        let alpha_lit = canon(5.0);
        let beta_lit = canon(3.0);
        let summed_lit = canon(8.0);
        assert!(
            alpha_ci.starts_with(&alpha_lit),
            "alpha cumulativeImportance reflects ONLY alpha's 5.0 (got {alpha_ci}, want prefix {alpha_lit})"
        );
        assert!(
            beta_ci.starts_with(&beta_lit),
            "beta cumulativeImportance reflects ONLY beta's 3.0 (got {beta_ci}, want prefix {beta_lit})"
        );
        assert!(
            !alpha_ci.starts_with(&summed_lit) && !beta_ci.starts_with(&summed_lit),
            "neither witness reflects the would-be global sum (8.0 → {summed_lit})"
        );

        // TEETH — DISJOINTNESS: cross-read each witness's subject in the OTHER's graph
        // is EMPTY (neither graph contains the other's valuation). If the sum were
        // merely re-labeled into one graph this would assert-out.
        let beta_subject_in_alpha = subject_pairs_in(tg.path(), &alpha_g, &subject);
        let alpha_subject_in_beta = subject_pairs_in(tg.path(), &beta_g, &subject);
        // (same subject IRI, different graphs) — already confirmed non-empty in each
        // OWN graph; now confirm the COMMONS graph is empty (no leak to the un-
        // segmented graph the sum used to live in).
        let commons_g = salience_projection_graph_iri(GID);
        assert!(
            subject_pairs_in(tg.path(), &commons_g, &subject).is_empty(),
            "TEETH: observer valuations DO NOT leak into the un-segmented commons graph"
        );
        // And the per-observer stores are physically separate files.
        assert!(
            tg.path()
                .join("values")
                .join("agent-alpha")
                .join("block-values.json")
                .is_file(),
            "alpha's value store is a distinct per-observer file"
        );
        assert!(
            tg.path()
                .join("values")
                .join("agent-beta")
                .join("block-values.json")
                .is_file(),
            "beta's value store is a distinct per-observer file"
        );
        // The witness raw sums are independent at the store layer too.
        let alpha_store = read_value_store_for(tg.path(), GID, "agent-alpha").unwrap();
        let beta_store = read_value_store_for(tg.path(), GID, "agent-beta").unwrap();
        let key = block_value_key(doc, block);
        assert_eq!(
            alpha_store.blocks.get(&key).unwrap().raw_importance_sum,
            5.0,
            "alpha's store holds ONLY alpha's importance (no beta sum)"
        );
        assert_eq!(
            beta_store.blocks.get(&key).unwrap().raw_importance_sum,
            3.0,
            "beta's store holds ONLY beta's importance (no alpha sum)"
        );

        // Sanity: the cross-reads above ARE the witness's own (non-empty), proving the
        // read-backs are live, not vacuously empty.
        assert!(!beta_subject_in_alpha.is_empty() && !alpha_subject_in_beta.is_empty());
    }

    /// BACK-COMPAT: the empty-observer commons case lands in the UN-SEGMENTED
    /// `:projection:salience` graph + `values/block-values.json`, byte-for-byte
    /// today's behavior — the per-observer machinery is invisible to single-agent use.
    #[test]
    fn empty_observer_lands_in_commons_unchanged() {
        let tg = TempGraph::new();
        let (doc, block) = ("doc-c", "block-c");
        let subject = value_subject_model(doc, block);

        // Value with NO observer (the commons / today's single-agent path).
        value_block_as(tg.path(), "", doc, block, 4.0);

        // It lands in the un-segmented commons graph + the shared store file.
        let commons_g = salience_projection_graph_iri(GID);
        assert!(
            !subject_pairs_in(tg.path(), &commons_g, &subject).is_empty(),
            "commons valuation projects into the un-segmented :projection:salience graph"
        );
        assert!(
            tg.path().join("values").join("block-values.json").is_file(),
            "commons value store is the shared values/block-values.json (no agent subdir)"
        );
        // No per-observer subdir was created.
        assert!(
            !tg.path().join("values").join("agent-alpha").exists(),
            "no stray per-observer dir for the commons case"
        );

        // read_value_store (the commons wrapper) sees it; the segmented read does NOT
        // (proving the commons is genuinely the empty-observer store, not a witness).
        let key = block_value_key(doc, block);
        let commons_store = read_value_store(tg.path(), GID).unwrap();
        assert_eq!(
            commons_store.blocks.get(&key).unwrap().raw_importance_sum,
            4.0,
            "the commons store holds the un-observed valuation"
        );
        let alpha_store = read_value_store_for(tg.path(), GID, "agent-alpha").unwrap();
        assert!(
            alpha_store.blocks.get(&key).is_none(),
            "an observer's store does NOT see the commons valuation"
        );
    }
}
