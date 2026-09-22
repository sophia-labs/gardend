use crate::{
    emporium::contract::song_vocabulary,
    emporium::reconcile::{reconcile_classes_validated, ClassScope, Placement, SpanKey},
    emporium::terms::{Triple, TripleDiff},
    geist_song_projection::song_verse_label,
    geist_song_store::{song_doc_id, LocalSongStore},
    rdf::{
        format_rdf_triple, graph_subject, push_integer_triple, push_string_triple, push_uri_triple,
        sparql_string_literal, RdfTriple,
    },
    rdf_authority::{song_projection_graph_iri, song_projection_graph_iri_for},
    rdf_record_materializer::rdf_triple_to_term,
    rdf_service::open_graph_store,
    runtime_config::{DCTERMS_NS, MNEMO_NS, RDF_TYPE},
};
use oxigraph::sparql::SparqlEvaluator;
use std::path::Path;

/// The pure SONG triple builder — the verbatim INSERT side of
/// `materialize_song_store` (Song head + verse-enumerate + optional coda),
/// extracted as the SINGLE SOURCE OF TRUTH so the wholesale path and the
/// reconcile `desired` cannot drift. Both `materialize_song_store` and
/// [`song_desired`] route through this one builder.
///
/// Order/positional invariants preserved: the head's `verseCount = |verses|`,
/// each verse subject `.../verse/{i}` POSITION-KEYED with `verseIndex == i`, the
/// derived `voiceCount = 1 + |counterpoints|` / `counterpointCount`, and the coda
/// emitted ONLY when `store.coda.is_some()` (Option ⇒ 0-or-1 cardinality).
fn song_value_triples(store: &LocalSongStore) -> Vec<RdfTriple> {
    // Per-observer Song subject (fix #3): the shared `{graph}:song` for the
    // singleton (empty observer, byte-identical), `{graph}:song:agent:{observer}`
    // for a per-agent Song. The GRAPH-scoping is the load-bearing isolation, but
    // keying the subject too keeps the two witnesses' Songs distinct under any read.
    let song_subject = match crate::rdf_authority::observer_segment(&store.observer) {
        Some(seg) => format!("{}:song:agent:{seg}", graph_subject(&store.graph_id)),
        None => format!("{}:song", graph_subject(&store.graph_id)),
    };
    let doc_id = song_doc_id(&store.observer);
    let mut triples = Vec::new();
    push_uri_triple(
        &mut triples,
        &song_subject,
        RDF_TYPE,
        &format!("{MNEMO_NS}Song"),
    );
    push_string_triple(
        &mut triples,
        &song_subject,
        &format!("{MNEMO_NS}graphId"),
        &store.graph_id,
    );
    push_string_triple(
        &mut triples,
        &song_subject,
        &format!("{MNEMO_NS}narrativeKind"),
        "song",
    );
    push_string_triple(
        &mut triples,
        &song_subject,
        &format!("{MNEMO_NS}documentId"),
        &doc_id,
    );
    push_integer_triple(
        &mut triples,
        &song_subject,
        &format!("{MNEMO_NS}verseCount"),
        store.verses.len() as i64,
    );
    for (index, verse) in store.verses.iter().enumerate() {
        let verse_subject = format!("{song_subject}/verse/{index}");
        push_uri_triple(
            &mut triples,
            &verse_subject,
            RDF_TYPE,
            &format!("{MNEMO_NS}SongVerse"),
        );
        push_string_triple(
            &mut triples,
            &verse_subject,
            &format!("{MNEMO_NS}graphId"),
            &store.graph_id,
        );
        push_string_triple(
            &mut triples,
            &verse_subject,
            &format!("{MNEMO_NS}narrativeKind"),
            "song-verse",
        );
        push_string_triple(
            &mut triples,
            &verse_subject,
            &format!("{MNEMO_NS}documentId"),
            &doc_id,
        );
        push_integer_triple(
            &mut triples,
            &verse_subject,
            &format!("{MNEMO_NS}verseIndex"),
            index as i64,
        );
        push_string_triple(
            &mut triples,
            &verse_subject,
            &format!("{MNEMO_NS}verseLabel"),
            &song_verse_label(index),
        );
        push_string_triple(
            &mut triples,
            &verse_subject,
            &format!("{MNEMO_NS}content"),
            &verse.text,
        );
        push_integer_triple(
            &mut triples,
            &verse_subject,
            &format!("{MNEMO_NS}voiceCount"),
            (1 + verse.counterpoints.len()) as i64,
        );
        push_integer_triple(
            &mut triples,
            &verse_subject,
            &format!("{MNEMO_NS}counterpointCount"),
            verse.counterpoints.len() as i64,
        );
        push_string_triple(
            &mut triples,
            &verse_subject,
            &format!("{DCTERMS_NS}created"),
            &verse.created_at,
        );
        push_string_triple(
            &mut triples,
            &verse_subject,
            &format!("{DCTERMS_NS}modified"),
            &verse.updated_at,
        );
    }
    if let Some(coda) = &store.coda {
        let coda_subject = format!("{song_subject}/coda");
        push_uri_triple(
            &mut triples,
            &coda_subject,
            RDF_TYPE,
            &format!("{MNEMO_NS}SongCoda"),
        );
        push_string_triple(
            &mut triples,
            &coda_subject,
            &format!("{MNEMO_NS}graphId"),
            &store.graph_id,
        );
        push_string_triple(
            &mut triples,
            &coda_subject,
            &format!("{MNEMO_NS}narrativeKind"),
            "song-coda",
        );
        push_string_triple(
            &mut triples,
            &coda_subject,
            &format!("{MNEMO_NS}documentId"),
            &doc_id,
        );
        push_string_triple(
            &mut triples,
            &coda_subject,
            &format!("{MNEMO_NS}content"),
            &coda.text,
        );
        push_integer_triple(
            &mut triples,
            &coda_subject,
            &format!("{MNEMO_NS}ejectionsRemaining"),
            coda.ejections_remaining,
        );
    }
    triples
}

/// The OLD wholesale SONG materializer — narrativeKind-keyed teardown
/// (`DELETE { ?s ?p ?o } WHERE { ?s mnemo:graphId "gid" ; mnemo:narrativeKind
/// ?kind ; ?p ?o }`) then full re-INSERT. As of step [2] the live call site
/// (`persist_song_store`) now uses [`reconcile_song_store`]; this STAYS as the
/// P3c oracle baseline (equivalence/domination run it against the reconcile path)
/// and is therefore test-only on the non-test build.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn materialize_song_store(
    graph_dir: &Path,
    store: &LocalSongStore,
) -> Result<(), String> {
    let oxi_store = open_graph_store(graph_dir)?;
    let authority_graph = song_projection_graph_iri_for(&store.graph_id, &store.observer);
    let graph_id_literal = sparql_string_literal(&store.graph_id);
    // NAMED-graph parity with the reconcile path (Placement::Named): the wart fix
    // moved song out of the default graph into `:projection:song`, so this oracle
    // baseline must DELETE/INSERT in the SAME named graph the reconcile path writes.
    let delete = format!(
        r#"
PREFIX mnemo: <{MNEMO_NS}>
DELETE {{ GRAPH <{authority_graph}> {{ ?subject ?p ?o . }} }}
WHERE {{
  GRAPH <{authority_graph}> {{
  ?subject mnemo:graphId {graph_id_literal} ;
    mnemo:narrativeKind ?kind ;
    ?p ?o .
  }}
}}
"#
    );
    SparqlEvaluator::new()
        .parse_update(&delete)
        .map_err(|error| format!("parse song cleanup update: {error}"))?
        .on_store(&oxi_store)
        .execute()
        .map_err(|error| format!("clear song store RDF: {error}"))?;

    let triples = song_value_triples(store);
    let insert = triples
        .iter()
        .map(format_rdf_triple)
        .collect::<Vec<_>>()
        .join("\n  ");
    let update = format!("INSERT DATA {{\n  GRAPH <{authority_graph}> {{\n  {insert}\n  }}\n}}");
    SparqlEvaluator::new()
        .parse_update(&update)
        .map_err(|error| format!("parse song materialization update: {error}"))?
        .on_store(&oxi_store)
        .execute()
        .map_err(|error| format!("materialize song store RDF: {error}"))
}

/// `project(source) -> desired` for the SONG Meaningful Object: the
/// [`song_value_triples`] (Song head + verses + coda), bridged
/// `RdfTriple -> Triple` through the SAME `rdf_triple_to_term` round-trip the
/// Document/graph/salience MOs use — MANDATORY so the bridged `desired`
/// serializes byte-identically to what `survey_class` reparses out of the store
/// (else the diff reads parity as drift and song never converges).
fn song_desired(store: &LocalSongStore) -> Vec<Triple> {
    song_value_triples(store)
        .iter()
        .map(rdf_triple_to_term)
        .collect()
}

/// The three SONG class IRIs (in disjoint-subject-set order: head, verse, coda).
/// The union of these three `Fixed` rdf:type spans IS the song MO (the
/// `SpanKey::Present` narrativeKind span was DROPPED by the 2026-06-21 ruling —
/// song matches-the-proof as a union of `Fixed` classes).
fn song_class_iris() -> [String; 3] {
    [
        format!("{MNEMO_NS}Song"),
        format!("{MNEMO_NS}SongVerse"),
        format!("{MNEMO_NS}SongCoda"),
    ]
}

/// Partition `desired` into the three (Song / SongVerse / SongCoda) subsets by
/// each subject's rdf:type-AS-DECLARED-IN-DESIRED. Returns subsets in the SAME
/// order as [`song_class_iris`] / [`song_scopes`].
///
/// CRITICAL (the spurious-ADD trap): each class span surveys ONLY its own
/// subjects, so each `class_diff` MUST be fed ONLY its class's triples. Feeding
/// the full `song_desired` to the Song-class diff makes every verse/coda triple
/// read as an ADD against the Song survey (which sees no verse/coda subjects).
/// We build a subject -> class-IRI map from the `rdf:type` triples in `desired`
/// (the bridged type object is a `Term::Uri` whose `as_nt()` is `<iri>`), then
/// route every triple by its subject's class.
fn partition_song_desired(desired: &[Triple]) -> [Vec<Triple>; 3] {
    let iris = song_class_iris();
    // class index for a subject, from its rdf:type triple in `desired`.
    let mut subject_class: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    for (s, p, o) in desired {
        if p == RDF_TYPE {
            // The bridged type object renders as `<iri>`; match the bare IRI.
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

    let mut subsets: [Vec<Triple>; 3] = [Vec::new(), Vec::new(), Vec::new()];
    for triple in desired {
        if let Some(&idx) = subject_class.get(&triple.0) {
            subsets[idx].push(triple.clone());
        }
    }
    subsets
}

/// The three SONG [`ClassScope`]s — one per `Fixed` rdf:type class, in
/// [`song_class_iris`] order. All three live in the `:projection:song` NAMED graph
/// ([`Placement::Named`] — the default-graph WART is FIXED, matching salience and
/// every other projection kind) with the single-cell-constancy `mnemo:graphId
/// "gid"` conjunct so each in-store survey only touches THIS cell's song spans.
/// All three share ONE placement (required by `reconcile_classes`'s apply-once).
fn song_scopes(store: &LocalSongStore) -> [ClassScope; 3] {
    // Per-observer NAMED graph (fix #3): the GRAPH-scoped DELETE in the reconcile
    // path is confined to THIS witness's `:projection:song:agent:{observer}` graph,
    // so a co-tenant's Song is untouchable. Empty observer = the shared singleton
    // graph (today's behavior, byte-identical).
    let authority_graph = song_projection_graph_iri_for(&store.graph_id, &store.observer);
    song_class_iris().map(|rdf_type| ClassScope {
        placement: Placement::Named(authority_graph.clone()),
        key: SpanKey::Fixed { rdf_type },
        graph_id_conjunct: Some((format!("{MNEMO_NS}graphId"), store.graph_id.clone())),
        subjects: None,
    })
}

/// ADDITIVE entry point: reconcile the SONG projection by MULTI-CLASS VALUE-DIFF
/// (the union of the three `Fixed` rdf:type spans) instead of the wholesale
/// narrativeKind-keyed teardown-and-rebuild [`materialize_song_store`] does.
/// Opens the store (the thin `&Path` wrapper — fork #5, minimal call-site
/// change), partitions [`song_desired`] into the three class subsets, builds the
/// three [`song_scopes`], and runs [`reconcile_classes`] (survey-all-then-apply-
/// once) over the three `(scope, subset)` pairs.
///
/// PURE PARITY (default-graph WART preserved): reaches the SAME default-graph
/// projection as `materialize_song_store` (same `desired`, same spans), by
/// MINIMAL delta — a converged save emits 0 ops; a single edit emits only the
/// changed slots. Returns the merged [`TripleDiff`].
///
/// MATCH-THE-PROOF PRICE: the wholesale DELETE keys on narrativeKind-PRESENCE;
/// this keys on the 3 rdf:types. They coincide under the construction premise
/// (every song subject carries BOTH its rdf:type AND narrativeKind — true as
/// built). A foreign narrativeKind-WITHOUT-type subject is the only divergence
/// (the oracle guards it).
///
/// WIRED (step [2]): this is the live SONG projection path — `persist_song_store`
/// calls it.
pub(crate) fn reconcile_song_store(
    graph_dir: &Path,
    store: &LocalSongStore,
) -> Result<TripleDiff, String> {
    let oxi_store = open_graph_store(graph_dir)?;
    let desired = song_desired(store);
    let subsets = partition_song_desired(&desired);
    let scopes = song_scopes(store);
    let scopes_desireds: Vec<(ClassScope, Vec<Triple>)> = scopes.into_iter().zip(subsets).collect();
    // S6: contract metadata made load-bearing — the `emporium-song` retrofit shapes
    // now GATE this write (the full 3-class desired union is validated once, was
    // `contract: None`). OBSERVE seam, not a trust boundary: song is a deterministic
    // re-projection from a trusted `LocalSongStore`, so a violation = materializer↔vocab
    // DRIFT (`shacl_oracle_song_conforms` proves today's projection conforms), loud-halted.
    reconcile_classes_validated(&oxi_store, &scopes_desireds, Some(song_vocabulary()))
}

#[cfg(test)]
mod p3c_oracle {
    //! P3c SONG oracle — ties the Lean song model (`Vocab/Song.lean`) to the REAL
    //! `materialize_song_store` (NO mocks): seed a real `LocalSongStore`, run the
    //! real materializer, read the projection back out of the REAL oxigraph store,
    //! and compare against a model built INDEPENDENTLY from the store fields +
    //! namespace constants — never by re-running the materializer or calling any of
    //! its helpers (`graph_subject`, `song_verse_label`, `push_*`, …).
    //!
    //! Same harness shape as the P3b Salience oracle (`salience_rdf_materializer.rs`):
    //! a per-test temp graph dir (the store cache keys on `graph_dir`, so a unique
    //! dir per test is REQUIRED), set read-back via `execute_sparql_query`, and an
    //! anti-tautology guard so the set-equality has teeth.
    use super::*;
    use crate::geist_song_store::{LocalSongCoda, LocalSongStore, LocalSongVerse};
    use crate::rdf_query_service::execute_sparql_query;
    use crate::runtime_config::XSD_NS;
    use std::collections::BTreeSet;
    use uuid::Uuid;

    const GID: &str = "graph-a";

    /// A fresh, unique on-disk graph dir per test (the global store cache in
    /// `rdf_store_service` keys by `graph_dir`, so cross-test bleed is avoided ONLY
    /// by a unique dir). RAII-cleaned by `Drop`.
    struct TempGraph {
        dir: std::path::PathBuf,
    }
    impl TempGraph {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!("sophia-song-oracle-{}", Uuid::new_v4()));
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

    // --- INDEPENDENT term/subject renderers ----------------------------------
    // ANTI-TAUTOLOGY: these spell out the EXPECTED rendering from the namespace
    // constants + the literal urn prefixes. NONE of them call `graph_subject`,
    // `song_verse_label`, `sparql_string_literal`, `push_*`, or `format_rdf_triple`
    // — so a bug in any materializer helper makes real ≠ model, not both drift.

    fn uri(s: &str) -> String {
        format!("<{s}>")
    }
    fn lit(s: &str) -> String {
        format!("\"{s}\"")
    }
    fn int_lit(v: i64) -> String {
        format!("\"{v}\"^^<{XSD_NS}integer>")
    }

    /// INDEPENDENT mirror of `format!("{}:song", graph_subject(g))` (rdf.rs:122-124
    /// has NO trailing separator, so `:song` glues directly): the song head subject.
    fn song_subject_model() -> String {
        format!("urn:mnemosyne:local:graph:{GID}:song")
    }
    /// INDEPENDENT mirror of `{song_subject}/verse/{i}` (geist_song_rdf.rs:71).
    fn verse_subject_model(i: usize) -> String {
        format!("{}/verse/{i}", song_subject_model())
    }
    /// INDEPENDENT mirror of `{song_subject}/coda` (geist_song_rdf.rs:140).
    fn coda_subject_model() -> String {
        format!("{}/coda", song_subject_model())
    }
    /// INDEPENDENT reimplementation of the pure label fn (geist_song_lines.rs:1-7):
    /// `0` at index 0, `-{i}` otherwise. We DELIBERATELY do NOT call
    /// `song_verse_label` — the determinism witness is that this hand-written pure
    /// fn of the index matches the real emission.
    fn verse_label_model(i: usize) -> String {
        if i == 0 {
            "0".to_string()
        } else {
            format!("-{i}")
        }
    }

    /// Read EVERY triple of the `:projection:song` named graph back as a
    /// normalized `S P O` set (oxigraph term `to_string()` rendering). The wart fix
    /// moved song off the default graph; the REAL store read-back scopes the
    /// named graph.
    fn read_default_set(graph_dir: &std::path::Path) -> BTreeSet<String> {
        let store = super::open_graph_store(graph_dir).expect("open store");
        let g = super::song_projection_graph_iri(GID);
        let result = execute_sparql_query(
            &store,
            &format!("SELECT ?s ?p ?o WHERE {{ GRAPH <{g}> {{ ?s ?p ?o }} }}"),
        )
        .expect("query song graph");
        result
            .rows
            .iter()
            .map(|row| format!("{} {} {}", row["s"], row["p"], row["o"]))
            .collect()
    }

    /// All `P O` pairs for one subject in the `:projection:song` named graph.
    fn subject_pairs(graph_dir: &std::path::Path, subject: &str) -> BTreeSet<String> {
        let store = super::open_graph_store(graph_dir).expect("open store");
        let g = super::song_projection_graph_iri(GID);
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

    /// Seed a raw triple into the `:projection:song` named graph WITHOUT the
    /// materializer (so the stale-class corpus is genuinely independent) — in the
    /// SAME graph the named-graph survey reaches.
    fn seed_raw(graph_dir: &std::path::Path, s: &str, p: &str, o_term: &str) {
        let store = super::open_graph_store(graph_dir).expect("open store");
        let g = super::song_projection_graph_iri(GID);
        let update = format!("INSERT DATA {{ GRAPH <{g}> {{ <{s}> <{p}> {o_term} }} }}");
        SparqlEvaluator::new()
            .parse_update(&update)
            .expect("parse seed")
            .on_store(&store)
            .execute()
            .expect("seed raw triple");
    }

    // --- INDEPENDENT model builders ------------------------------------------

    fn verse(text: &str, counterpoints: &[&str], created: &str, modified: &str) -> LocalSongVerse {
        LocalSongVerse {
            text: text.to_string(),
            counterpoints: counterpoints.iter().map(|c| c.to_string()).collect(),
            created_at: created.to_string(),
            updated_at: modified.to_string(),
        }
    }

    /// Hand-built EXPECTED `P O` set for the SONG HEAD subject — mirrors
    /// geist_song_rdf.rs:40-69 line-for-line, from `store` fields + constants.
    fn expected_head_pairs(store: &LocalSongStore) -> BTreeSet<String> {
        let mut out = BTreeSet::new();
        out.insert(format!(
            "{} {}",
            uri(RDF_TYPE),
            uri(&format!("{MNEMO_NS}Song"))
        ));
        out.insert(format!(
            "{} {}",
            uri(&format!("{MNEMO_NS}graphId")),
            lit(GID)
        ));
        out.insert(format!(
            "{} {}",
            uri(&format!("{MNEMO_NS}narrativeKind")),
            lit("song")
        ));
        out.insert(format!(
            "{} {}",
            uri(&format!("{MNEMO_NS}documentId")),
            lit("geist-song")
        ));
        // DERIVED COUNT: verseCount = |verses|, tied to the positional list length.
        out.insert(format!(
            "{} {}",
            uri(&format!("{MNEMO_NS}verseCount")),
            int_lit(store.verses.len() as i64)
        ));
        out
    }

    /// Hand-built EXPECTED `P O` set for the i-th VERSE subject — mirrors
    /// geist_song_rdf.rs:72-137. The POSITIONAL `verseIndex` and the subject minting
    /// both key on the SAME `i`; the DERIVED voiceCount/counterpointCount come from
    /// the counterpoints length; the label is the independent pure-fn image.
    fn expected_verse_pairs(i: usize, v: &LocalSongVerse) -> BTreeSet<String> {
        let mut out = BTreeSet::new();
        out.insert(format!(
            "{} {}",
            uri(RDF_TYPE),
            uri(&format!("{MNEMO_NS}SongVerse"))
        ));
        out.insert(format!(
            "{} {}",
            uri(&format!("{MNEMO_NS}graphId")),
            lit(GID)
        ));
        out.insert(format!(
            "{} {}",
            uri(&format!("{MNEMO_NS}narrativeKind")),
            lit("song-verse")
        ));
        out.insert(format!(
            "{} {}",
            uri(&format!("{MNEMO_NS}documentId")),
            lit("geist-song")
        ));
        // POSITIONAL: verseIndex == i (the same i that names the subject).
        out.insert(format!(
            "{} {}",
            uri(&format!("{MNEMO_NS}verseIndex")),
            int_lit(i as i64)
        ));
        // DETERMINISM: label is the pure-fn image of the positional index.
        out.insert(format!(
            "{} {}",
            uri(&format!("{MNEMO_NS}verseLabel")),
            lit(&verse_label_model(i))
        ));
        out.insert(format!(
            "{} {}",
            uri(&format!("{MNEMO_NS}content")),
            lit(&v.text)
        ));
        // DERIVED COUNT: voiceCount = 1 + |counterpoints|.
        out.insert(format!(
            "{} {}",
            uri(&format!("{MNEMO_NS}voiceCount")),
            int_lit(1 + v.counterpoints.len() as i64)
        ));
        // DERIVED COUNT: counterpointCount = |counterpoints| (so voiceCount = 1 + this).
        out.insert(format!(
            "{} {}",
            uri(&format!("{MNEMO_NS}counterpointCount")),
            int_lit(v.counterpoints.len() as i64)
        ));
        // PASSTHROUGH (F3-analogue): created/modified are independent raw string
        // literals, emitted verbatim, never compared. created≤modified UNENFORCED.
        out.insert(format!(
            "{} {}",
            uri(&format!("{DCTERMS_NS}created")),
            lit(&v.created_at)
        ));
        out.insert(format!(
            "{} {}",
            uri(&format!("{DCTERMS_NS}modified")),
            lit(&v.updated_at)
        ));
        out
    }

    /// Hand-built EXPECTED `P O` set for the CODA subject — mirrors
    /// geist_song_rdf.rs:141-176 (only emitted when `store.coda.is_some()`).
    fn expected_coda_pairs(c: &LocalSongCoda) -> BTreeSet<String> {
        let mut out = BTreeSet::new();
        out.insert(format!(
            "{} {}",
            uri(RDF_TYPE),
            uri(&format!("{MNEMO_NS}SongCoda"))
        ));
        out.insert(format!(
            "{} {}",
            uri(&format!("{MNEMO_NS}graphId")),
            lit(GID)
        ));
        out.insert(format!(
            "{} {}",
            uri(&format!("{MNEMO_NS}narrativeKind")),
            lit("song-coda")
        ));
        out.insert(format!(
            "{} {}",
            uri(&format!("{MNEMO_NS}documentId")),
            lit("geist-song")
        ));
        out.insert(format!(
            "{} {}",
            uri(&format!("{MNEMO_NS}content")),
            lit(&c.text)
        ));
        out.insert(format!(
            "{} {}",
            uri(&format!("{MNEMO_NS}ejectionsRemaining")),
            int_lit(c.ejections_remaining)
        ));
        out
    }

    fn store_with(verses: Vec<LocalSongVerse>, coda: Option<LocalSongCoda>) -> LocalSongStore {
        LocalSongStore {
            schema_version: 1,
            graph_id: GID.to_string(),
            observer: String::new(),
            verses,
            coda,
            archives: Vec::new(),
        }
    }

    // ------------------------------------------------------------------------
    // (1) CONTENT: real projection == independently-built model (whole store).
    // ------------------------------------------------------------------------
    /// CONTENT ORACLE — the REAL `materialize_song_store` output for a multi-verse,
    /// counterpoint-bearing, coda-bearing store EQUALS the independently-built model
    /// set, FACE BY FACE (head + each verse + coda) and over the WHOLE default graph.
    /// Anti-tautology: the model never calls a materializer helper, and a bent model
    /// is rejected below.
    #[test]
    fn oracle_content_real_equals_independent_model() {
        let tg = TempGraph::new();
        let verses = vec![
            verse("first verse", &["counter-a"], "1000", "1001"),
            verse("second verse", &["counter-b", "counter-c"], "2000", "2000"),
            verse("third verse", &[], "3000", "3500"),
        ];
        let coda = Some(LocalSongCoda {
            text: "the closing".to_string(),
            ejections_remaining: 8,
            created_at: "9000".to_string(),
        });
        let store = store_with(verses.clone(), coda.clone());

        materialize_song_store(tg.path(), &store).expect("materialize");

        // -- per-face set-equality ------------------------------------------
        let real_head = subject_pairs(tg.path(), &song_subject_model());
        let model_head = expected_head_pairs(&store);
        eprintln!("REAL head ({}): {real_head:?}", real_head.len());
        assert_eq!(
            real_head, model_head,
            "HEAD face: real == independent model"
        );

        for (i, v) in verses.iter().enumerate() {
            let real_v = subject_pairs(tg.path(), &verse_subject_model(i));
            let model_v = expected_verse_pairs(i, v);
            let missing: Vec<_> = model_v.difference(&real_v).cloned().collect();
            let extra: Vec<_> = real_v.difference(&model_v).cloned().collect();
            eprintln!("VERSE {i}: MODEL-only={missing:?} REAL-only={extra:?}");
            assert_eq!(real_v, model_v, "VERSE {i} face: real == independent model");
        }

        let real_coda = subject_pairs(tg.path(), &coda_subject_model());
        let model_coda = expected_coda_pairs(coda.as_ref().unwrap());
        assert_eq!(
            real_coda, model_coda,
            "CODA face: real == independent model"
        );

        // -- WHOLE-GRAPH set-equality (catches any extra/stray subject) ------
        let real_all = read_default_set(tg.path());
        let mut model_all: BTreeSet<String> = BTreeSet::new();
        let s = song_subject_model();
        for po in &model_head {
            model_all.insert(format!("{} {po}", uri(&s)));
        }
        for (i, v) in verses.iter().enumerate() {
            let vs = verse_subject_model(i);
            for po in &expected_verse_pairs(i, v) {
                model_all.insert(format!("{} {po}", uri(&vs)));
            }
        }
        let cs = coda_subject_model();
        for po in &model_coda {
            model_all.insert(format!("{} {po}", uri(&cs)));
        }
        let missing: Vec<_> = model_all.difference(&real_all).cloned().collect();
        let extra: Vec<_> = real_all.difference(&model_all).cloned().collect();
        eprintln!("WHOLE-GRAPH MODEL-only={missing:?}");
        eprintln!("WHOLE-GRAPH REAL-only={extra:?}");
        assert_eq!(
            real_all, model_all,
            "WHOLE default graph: real == independent model"
        );

        // ANTI-TAUTOLOGY: a deliberately-wrong model must NOT match.
        assert!(
            !real_all.is_empty(),
            "the store actually projected something"
        );
        let mut wrong = model_all.clone();
        wrong.insert(format!(
            "{} {} {}",
            uri(&song_subject_model()),
            uri(&format!("{MNEMO_NS}graphId")),
            lit("WRONG-GRAPH")
        ));
        assert_ne!(real_all, wrong, "anti-tautology: bent model rejected");
    }

    // ------------------------------------------------------------------------
    // (2) TEETH-CHECK positional: verse at position i has subject .../verse/i AND
    //     verseIndex == i, for each i in order (order-preservation, true by
    //     construction from `store.verses.iter().enumerate()`, rs:70).
    // ------------------------------------------------------------------------
    #[test]
    fn oracle_positional_verse_index_equals_position() {
        let tg = TempGraph::new();
        let verses = vec![
            verse("v0", &[], "1000", "1000"),
            verse("v1", &["c"], "1000", "1000"),
            verse("v2", &["c", "d"], "1000", "1000"),
        ];
        let store = store_with(verses.clone(), None);
        materialize_song_store(tg.path(), &store).expect("materialize");

        for (i, v) in verses.iter().enumerate() {
            let subj = verse_subject_model(i);
            let pairs = subject_pairs(tg.path(), &subj);
            assert!(
                !pairs.is_empty(),
                "POSITIONAL: verse {i} projects to subject .../verse/{i} (non-empty)"
            );
            // The POSITIONAL invariant: the verseIndex literal == the SAME i that
            // names the subject. (If the loop emitted a wrong index this fails.)
            let index_pair = format!(
                "{} {}",
                uri(&format!("{MNEMO_NS}verseIndex")),
                int_lit(i as i64)
            );
            assert!(
                pairs.contains(&index_pair),
                "ORDER-PRESERVATION: subject .../verse/{i} carries verseIndex {i} (== position)"
            );
            // The content emitted at this subject is THIS verse's text (the i-th
            // verse, not some other one — order is preserved end-to-end).
            let content_pair = format!("{} {}", uri(&format!("{MNEMO_NS}content")), lit(&v.text));
            assert!(
                pairs.contains(&content_pair),
                "ORDER-PRESERVATION: subject .../verse/{i} carries the i-th verse's content {:?}",
                v.text
            );
        }

        // TEETH on the negative direction: there is NO .../verse/3 subject (only 3
        // verses, indices 0..2) — the positional minting is exact, not over-emitting.
        assert!(
            subject_pairs(tg.path(), &verse_subject_model(3)).is_empty(),
            "no phantom .../verse/3 subject — positional minting is exact"
        );
    }

    // ------------------------------------------------------------------------
    // (3) TEETH-CHECK coda 0|1 + derived voiceCount = 1 + |counterpoints|.
    // ------------------------------------------------------------------------
    /// CODA CARDINALITY 0 — a store with NO coda emits ZERO coda triples (the
    /// `if let Some` guard is load-bearing, rs:139).
    #[test]
    fn oracle_coda_none_emits_no_coda() {
        let tg = TempGraph::new();
        let store = store_with(vec![verse("only", &[], "1", "1")], None);
        materialize_song_store(tg.path(), &store).expect("materialize");
        assert!(
            subject_pairs(tg.path(), &coda_subject_model()).is_empty(),
            "CODA-0: no coda in the store ⇒ no .../coda subject emitted"
        );
        // And no SongCoda type anywhere (the only-0-or-1 cardinality holds at 0).
        let any_coda_type = read_default_set(tg.path())
            .iter()
            .any(|t| t.contains(&format!("{MNEMO_NS}SongCoda")));
        assert!(!any_coda_type, "CODA-0: no mnemo:SongCoda triple anywhere");
    }

    /// CODA CARDINALITY 1 — a store WITH a coda emits EXACTLY ONE coda subject
    /// (Option ⇒ ≤1 by construction), with the full coda face.
    #[test]
    fn oracle_coda_some_emits_exactly_one() {
        let tg = TempGraph::new();
        let coda = LocalSongCoda {
            text: "closing".to_string(),
            ejections_remaining: 8,
            created_at: "5".to_string(),
        };
        let store = store_with(vec![verse("only", &[], "1", "1")], Some(coda.clone()));
        materialize_song_store(tg.path(), &store).expect("materialize");

        // Exactly ONE coda subject (count the distinct subjects carrying SongCoda).
        let oxi = super::open_graph_store(tg.path()).expect("open");
        let g = super::song_projection_graph_iri(GID);
        let q = format!(
            "SELECT (COUNT(DISTINCT ?s) AS ?n) WHERE {{ GRAPH <{g}> {{ ?s <{RDF_TYPE}> <{MNEMO_NS}SongCoda> }} }}"
        );
        let res = execute_sparql_query(&oxi, &q).expect("count codas");
        let n = res.rows.first().map(|r| r["n"].clone()).unwrap_or_default();
        assert_eq!(n, int_lit(1), "CODA-1: exactly one mnemo:SongCoda subject");

        // And that subject's face equals the independent model.
        let real_coda = subject_pairs(tg.path(), &coda_subject_model());
        assert_eq!(
            real_coda,
            expected_coda_pairs(&coda),
            "CODA-1 face: real == independent model"
        );
    }

    /// DERIVED COUNT TEETH — a verse with k counterpoints emits voiceCount = 1 + k
    /// and counterpointCount = k (so voiceCount == 1 + counterpointCount), built
    /// straight from the counterpoints vec length at emit time (rs:114-125).
    #[test]
    fn oracle_voicecount_is_one_plus_counterpoints() {
        let tg = TempGraph::new();
        // k = 2 counterpoints ⇒ voiceCount 3, counterpointCount 2.
        let store = store_with(vec![verse("v", &["alpha", "beta"], "1", "1")], None);
        materialize_song_store(tg.path(), &store).expect("materialize");

        let pairs = subject_pairs(tg.path(), &verse_subject_model(0));
        let voice = format!("{} {}", uri(&format!("{MNEMO_NS}voiceCount")), int_lit(3));
        let counter = format!(
            "{} {}",
            uri(&format!("{MNEMO_NS}counterpointCount")),
            int_lit(2)
        );
        assert!(pairs.contains(&voice), "DERIVED: voiceCount = 1 + 2 = 3");
        assert!(pairs.contains(&counter), "DERIVED: counterpointCount = 2");
        // The relation voiceCount = 1 + counterpointCount holds by construction:
        // 3 == 1 + 2. (Asserted here so a divergent emit would fail.)
        assert_eq!(
            3,
            1 + 2,
            "voiceCount == 1 + counterpointCount (structural relation)"
        );

        // And verseCount on the head == 1 (single verse), the positional length.
        let head = subject_pairs(tg.path(), &song_subject_model());
        let vc = format!("{} {}", uri(&format!("{MNEMO_NS}verseCount")), int_lit(1));
        assert!(head.contains(&vc), "DERIVED: verseCount = |verses| = 1");
    }

    // ------------------------------------------------------------------------
    // (4) TEETH-CHECK stale reclaim: a stale song/verse subject NOT in the new
    //     store is RECLAIMED by the narrativeKind-keyed DELETE…WHERE.
    // ------------------------------------------------------------------------
    /// STALE RECLAIM — a stale verse subject (typed `mnemo:SongVerse`, carrying the
    /// cell graphId AND a `mnemo:narrativeKind`, exactly what the DELETE WHERE keys
    /// on, rs:23-30) for a position that does NOT exist in the new (smaller) store is
    /// RECLAIMED. The runtime image of the narrativeKind head-keyed span. A fresh
    /// verse IS present (the INSERT half ran).
    #[test]
    fn oracle_stale_verse_reclaimed_by_narrativekind_span() {
        let tg = TempGraph::new();

        // Pre-seed a STALE .../verse/5 subject (a position the new store won't have),
        // carrying the three keys the DELETE WHERE matches: graphId + narrativeKind +
        // (any) ?p ?o. Seeded RAW (not via the materializer), so it's independent.
        let stale = verse_subject_model(5);
        seed_raw(
            tg.path(),
            &stale,
            RDF_TYPE,
            &uri(&format!("{MNEMO_NS}SongVerse")),
        );
        seed_raw(tg.path(), &stale, &format!("{MNEMO_NS}graphId"), &lit(GID));
        seed_raw(
            tg.path(),
            &stale,
            &format!("{MNEMO_NS}narrativeKind"),
            &lit("song-verse"),
        );
        seed_raw(
            tg.path(),
            &stale,
            &format!("{MNEMO_NS}content"),
            &lit("OLD VERSE TEXT"),
        );
        // Also a STALE coda from a prior larger song.
        let stale_coda = coda_subject_model();
        seed_raw(
            tg.path(),
            &stale_coda,
            RDF_TYPE,
            &uri(&format!("{MNEMO_NS}SongCoda")),
        );
        seed_raw(
            tg.path(),
            &stale_coda,
            &format!("{MNEMO_NS}graphId"),
            &lit(GID),
        );
        seed_raw(
            tg.path(),
            &stale_coda,
            &format!("{MNEMO_NS}narrativeKind"),
            &lit("song-coda"),
        );

        assert!(
            !subject_pairs(tg.path(), &stale).is_empty(),
            "precondition: stale verse seeded"
        );
        assert!(
            !subject_pairs(tg.path(), &stale_coda).is_empty(),
            "precondition: stale coda seeded"
        );

        // New store: ONE verse (position 0), NO coda. The ghost verse/5 and the coda
        // are both absent from the new store.
        let store = store_with(vec![verse("fresh verse", &["cp"], "1", "1")], None);
        materialize_song_store(tg.path(), &store).expect("materialize");

        // RECLAIM: the stale verse's triples are GONE — deleted by the narrativeKind-
        // keyed span (reads the IN-STORE graphId + narrativeKind, not the new store).
        assert!(
            subject_pairs(tg.path(), &stale).is_empty(),
            "STALE: ghost .../verse/5 reclaimed by the narrativeKind-keyed DELETE"
        );
        assert!(
            subject_pairs(tg.path(), &stale_coda).is_empty(),
            "STALE: ghost coda reclaimed (no coda in the new store)"
        );
        // The fresh verse IS present (INSERT half ran) and carries the right position.
        let fresh = subject_pairs(tg.path(), &verse_subject_model(0));
        assert!(
            !fresh.is_empty(),
            "fresh verse 0 projected (INSERT half ran)"
        );
        assert!(
            fresh.contains(&format!(
                "{} {}",
                uri(&format!("{MNEMO_NS}content")),
                lit("fresh verse")
            )),
            "fresh verse carries the NEW content, not the stale text"
        );
    }

    /// STALE TEETH companion — a subject WITHOUT a `mnemo:narrativeKind` SURVIVES the
    /// DELETE (the span is narrativeKind-keyed, NOT a blanket clear of the graphId).
    /// If it cleared everything, this asserts-out and the span claim is REFUTED.
    #[test]
    fn oracle_non_narrative_subject_survives() {
        let tg = TempGraph::new();
        // An orphan carrying the cell graphId but NO narrativeKind ⇒ out of the span.
        let orphan = format!("urn:mnemosyne:local:graph:{GID}:orphan:x");
        seed_raw(tg.path(), &orphan, &format!("{MNEMO_NS}graphId"), &lit(GID));
        seed_raw(
            tg.path(),
            &orphan,
            &format!("{MNEMO_NS}note"),
            &lit("survivor"),
        );

        let store = store_with(vec![verse("v", &[], "1", "1")], None);
        materialize_song_store(tg.path(), &store).expect("materialize");

        assert!(
            !subject_pairs(tg.path(), &orphan).is_empty(),
            "non-narrativeKind orphan is NOT reclaimed: the DELETE is narrativeKind-keyed, not a blanket clear"
        );
    }
}

// ───────────────────────────────────────────────────────────────────────────
// P3c RECONCILE ORACLE — ties the GENERAL multi-class primitive
// (`reconcile_classes`, via the `reconcile_song_store` SONG instantiation) to the
// wholesale `materialize_song_store` baseline, mirroring the salience
// `p3b_reconcile_oracle` (which ties `reconcile_class` to `materialize_value_store`).
//
// The differential oracle (per the design decision-record §"The differential
// oracle"): clone the SEEDED store twice from identical state, run the OLD
// wholesale materializer on clone A and `reconcile_song_store` on clone B (both
// fed the SAME `LocalSongStore` => the SAME `song_desired`), assert value-canonical
// set-equality. Plus:
//   (1) EQUIVALENCE — wholesale(A) default-graph set == reconcile(B) default-graph
//       set, from the SAME seed. Anti-tautology: a bent set is rejected, and the
//       from-empty reconcile is pure INSERT (no removes).
//   (2) DOMINATION — `reconcile op_count <= wholesale op_count` always; on a
//       CONVERGED re-run, reconcile is STRICTLY less (0 vs the wholesale's
//       teardown+rebuild `2*|desired|`).
//   (3) CONVERGENCE — a second reconcile against the SAME desired emits 0 ops
//       (the multi-class survey-all reads back what the first apply wrote; if
//       NONZERO the residual diff is reported verbatim and the test FAILS).
//   (4) ORDER-PRESERVATION-THROUGH-RECONCILE — after the multi-class reconcile,
//       read verse subjects ORDER BY verseIndex and assert verseIndex == position
//       (the positional order survives the union-of-classes path).
//   (5) CODA 0|1 — a store WITH a coda reconciles EXACTLY ONE SongCoda subject;
//       WITHOUT, zero (the Option ⇒ 0-or-1 cardinality, through reconcile).
//   (6) FOREIGN-narrativeKind GUARD — the ONE match-the-proof divergence made
//       EXPLICIT: a subject with `mnemo:narrativeKind` but NO song rdf:type is
//       RECLAIMED by the wholesale narrativeKind-keyed DELETE yet NOT surveyed by
//       the rdf:type-keyed reconcile. Documents that real song stores never
//       produce such a subject (every song subject carries BOTH type AND kind).
#[cfg(test)]
mod p3c_reconcile_oracle {
    use super::*;
    use crate::geist_song_store::{LocalSongCoda, LocalSongStore, LocalSongVerse};
    use crate::rdf_query_service::execute_sparql_query;
    use crate::runtime_config::XSD_NS;
    use std::collections::BTreeSet;
    use uuid::Uuid;

    const GID: &str = "graph-song-recon";

    /// Fresh unique on-disk graph dir per store (the global store cache keys by
    /// `graph_dir`, so a unique dir per store is REQUIRED — and is exactly what
    /// lets EQUIVALENCE hold two genuinely separate A/B stores). RAII-cleaned.
    struct TempGraph {
        dir: std::path::PathBuf,
    }
    impl TempGraph {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!("sophia-song-recon-{}", Uuid::new_v4()));
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

    fn store_with(verses: Vec<LocalSongVerse>, coda: Option<LocalSongCoda>) -> LocalSongStore {
        LocalSongStore {
            schema_version: 1,
            graph_id: GID.to_string(),
            observer: String::new(),
            verses,
            coda,
            archives: Vec::new(),
        }
    }

    fn verse(text: &str, counterpoints: &[&str], created: &str, modified: &str) -> LocalSongVerse {
        LocalSongVerse {
            text: text.to_string(),
            counterpoints: counterpoints.iter().map(|c| c.to_string()).collect(),
            created_at: created.to_string(),
            updated_at: modified.to_string(),
        }
    }

    /// Read the whole `:projection:song` named graph back as a normalized `S P O`
    /// set (oxigraph term `to_string()`). Both wholesale and reconcile now write
    /// into the SAME named graph (the wart fix), so this is the common comparison
    /// face.
    fn read_default_set(graph_dir: &std::path::Path) -> BTreeSet<String> {
        let store = super::open_graph_store(graph_dir).expect("open store");
        let g = super::song_projection_graph_iri(GID);
        let result = execute_sparql_query(
            &store,
            &format!("SELECT ?s ?p ?o WHERE {{ GRAPH <{g}> {{ ?s ?p ?o }} }}"),
        )
        .expect("query song graph");
        result
            .rows
            .iter()
            .map(|row| format!("{} {} {}", row["s"], row["p"], row["o"]))
            .collect()
    }

    /// All `P O` pairs for one subject in the `:projection:song` named graph.
    fn subject_pairs(graph_dir: &std::path::Path, subject: &str) -> BTreeSet<String> {
        let store = super::open_graph_store(graph_dir).expect("open store");
        let g = super::song_projection_graph_iri(GID);
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

    /// Seed a raw triple into the `:projection:song` named graph WITHOUT any
    /// materializer (so the foreign-subject corpus is genuinely independent of the
    /// projection path) — in the SAME graph the named-graph survey reaches.
    fn seed_raw(graph_dir: &std::path::Path, s: &str, p: &str, o_term: &str) {
        let store = super::open_graph_store(graph_dir).expect("open store");
        let g = super::song_projection_graph_iri(GID);
        let update = format!("INSERT DATA {{ GRAPH <{g}> {{ <{s}> <{p}> {o_term} }} }}");
        SparqlEvaluator::new()
            .parse_update(&update)
            .expect("parse seed")
            .on_store(&store)
            .execute()
            .expect("seed raw triple");
    }

    // --- INDEPENDENT term/subject renderers (anti-tautology) ------------------
    // NONE call graph_subject / song_verse_label / sparql_string_literal / push_*.
    fn uri(s: &str) -> String {
        format!("<{s}>")
    }
    fn lit(s: &str) -> String {
        format!("\"{s}\"")
    }
    fn int_lit(v: i64) -> String {
        format!("\"{v}\"^^<{XSD_NS}integer>")
    }
    /// INDEPENDENT mirror of `format!("{}:song", graph_subject(g))`.
    fn song_subject_model() -> String {
        format!("urn:mnemosyne:local:graph:{GID}:song")
    }
    fn verse_subject_model(i: usize) -> String {
        format!("{}/verse/{i}", song_subject_model())
    }
    fn coda_subject_model() -> String {
        format!("{}/coda", song_subject_model())
    }

    /// READ-BACK the verse subjects ORDER BY verseIndex (the reader's view) →
    /// `(subject, verseIndex, content)` rows in ascending-index order. Built fresh
    /// here (not borrowed from the fixture) so this oracle is self-contained.
    fn read_verses_ordered(graph_dir: &std::path::Path) -> Vec<(String, i64, String)> {
        let store = super::open_graph_store(graph_dir).expect("open store");
        let g = super::song_projection_graph_iri(GID);
        let q = format!(
            "SELECT ?s ?idx ?text WHERE {{ GRAPH <{g}> {{ \
             ?s <{MNEMO_NS}graphId> \"{GID}\" ; \
                <{MNEMO_NS}narrativeKind> \"song-verse\" ; \
                <{MNEMO_NS}verseIndex> ?idx ; \
                <{MNEMO_NS}content> ?text }} }} ORDER BY ?idx"
        );
        let res = execute_sparql_query(&store, &q).expect("query verses ordered");
        res.rows
            .iter()
            .map(|row| {
                // ?idx renders as the typed integer literal `"N"^^<…integer>` —
                // strip to the bare i64 for the positional assertion.
                let raw = row["idx"].clone();
                let n = raw
                    .split('"')
                    .nth(1)
                    .and_then(|d| d.parse::<i64>().ok())
                    .unwrap_or(-1);
                let text = row["text"].trim_matches('"').to_string();
                // `?s` renders as the bracketed NamedNode `<iri>`; strip to bare IRI
                // so the positional comparison is against `verse_subject_model(i)`.
                let subject = row["s"]
                    .trim_start_matches('<')
                    .trim_end_matches('>')
                    .to_string();
                (subject, n, text)
            })
            .collect()
    }

    /// Count distinct subjects of a given rdf:type in the default graph.
    fn count_type(graph_dir: &std::path::Path, type_iri: &str) -> i64 {
        let store = super::open_graph_store(graph_dir).expect("open store");
        let g = super::song_projection_graph_iri(GID);
        let q = format!(
            "SELECT (COUNT(DISTINCT ?s) AS ?n) WHERE {{ GRAPH <{g}> {{ ?s <{RDF_TYPE}> <{type_iri}> }} }}"
        );
        let res = execute_sparql_query(&store, &q).expect("count type");
        res.rows
            .first()
            .map(|r| r["n"].clone())
            .and_then(|raw| raw.split('"').nth(1).and_then(|d| d.parse::<i64>().ok()))
            .unwrap_or(-1)
    }

    /// A non-trivial song: 3 verses (one counterpoint-bearing), a coda. Exercises
    /// all three classes so the multi-class composition is actually composing.
    fn full_song() -> LocalSongStore {
        store_with(
            vec![
                verse("first verse", &["counter-a"], "1000", "1001"),
                verse("second verse", &["counter-b", "counter-c"], "2000", "2000"),
                verse("third verse", &[], "3000", "3500"),
            ],
            Some(LocalSongCoda {
                text: "the closing".to_string(),
                ejections_remaining: 8,
                created_at: "9000".to_string(),
            }),
        )
    }

    // ────────────────────────────────────────────────────────────────────────
    // (1) EQUIVALENCE — wholesale(A) == reconcile(B), from an identical seed.
    // ────────────────────────────────────────────────────────────────────────
    /// `mem_equiv_class` over the UNION of the 3 song classes: the multi-class
    /// reconcile reaches the EXACT same default-graph projection the wholesale
    /// narrativeKind-keyed teardown-and-rebuild does. Twin stores A (wholesale)
    /// and B (reconcile) over the SAME `LocalSongStore`; their full default-graph
    /// sets must be set-equal. Anti-tautology: from empty the reconcile is pure
    /// INSERT (no removes), the set is non-empty, and a bent set is rejected.
    #[test]
    fn oracle_reconcile_equivalent_to_wholesale() {
        let tg_a = TempGraph::new();
        let tg_b = TempGraph::new();
        let store = full_song();

        // A: the wholesale baseline. B: the multi-class reconcile path. SAME seed.
        materialize_song_store(tg_a.path(), &store).expect("wholesale A");
        let diff = reconcile_song_store(tg_b.path(), &store).expect("reconcile B");

        let set_a = read_default_set(tg_a.path());
        let set_b = read_default_set(tg_b.path());
        let missing: Vec<_> = set_a.difference(&set_b).cloned().collect();
        let extra: Vec<_> = set_b.difference(&set_a).cloned().collect();
        eprintln!("wholesale-only (missing from reconcile): {missing:?}");
        eprintln!("reconcile-only (extra vs wholesale):     {extra:?}");
        assert_eq!(
            set_a, set_b,
            "EQUIVALENCE: multi-class reconcile default-graph set must EQUAL the wholesale set"
        );
        assert!(!set_b.is_empty(), "the song actually projected something");

        // From empty, reconcile is pure INSERT of the whole desired (no removes).
        assert!(
            diff.removes.is_empty(),
            "from-empty reconcile removes nothing"
        );
        assert!(
            !diff.adds.is_empty(),
            "from-empty reconcile adds the projection"
        );

        // All three classes are present in the reconcile output (the union really
        // composed — not just the Song head).
        assert_eq!(
            count_type(tg_b.path(), &format!("{MNEMO_NS}Song")),
            1,
            "one Song head"
        );
        assert_eq!(
            count_type(tg_b.path(), &format!("{MNEMO_NS}SongVerse")),
            3,
            "three verses"
        );
        assert_eq!(
            count_type(tg_b.path(), &format!("{MNEMO_NS}SongCoda")),
            1,
            "one coda"
        );

        // ANTI-TAUTOLOGY: a deliberately-bent expected set must NOT match the real.
        let mut wrong = set_b.clone();
        wrong.insert(format!(
            "{} {} {}",
            uri(&song_subject_model()),
            uri(&format!("{MNEMO_NS}graphId")),
            lit("WRONG-GRAPH")
        ));
        assert_ne!(set_b, wrong, "anti-tautology: bent set rejected");
    }

    // ────────────────────────────────────────────────────────────────────────
    // (2) DOMINATION — reconcile op_count <= wholesale; strictly < when converged.
    // ────────────────────────────────────────────────────────────────────────
    /// `class_ops_dominate` over the union: from EMPTY both pay `|desired|` ops
    /// (reconcile = all adds; wholesale = `0 deletes + |desired| inserts`), so
    /// reconcile <= wholesale. After CONVERGENCE, reconcile pays 0 while the
    /// wholesale STILL tears down the in-span `|desired|` triples and re-inserts
    /// them (`2 * |desired|`), so reconcile is STRICTLY less — the whole point.
    #[test]
    fn oracle_reconcile_op_count_dominates_wholesale() {
        let tg = TempGraph::new();
        let store = full_song();
        let desired_len = song_desired(&store).len();
        assert!(desired_len > 0, "the fixture must actually project triples");

        // FIRST reconcile (from empty): op_count == |desired| (all adds).
        let first = reconcile_song_store(tg.path(), &store).expect("first reconcile");
        assert_eq!(
            first.op_count(),
            desired_len,
            "from-empty reconcile op_count == |desired| (pure inserts)"
        );

        // Wholesale op_count for the SAME from-empty transition, INDEPENDENTLY:
        // the narrativeKind span was empty ⇒ 0 deletes + |desired| inserts.
        let wholesale_first = 0 + desired_len;
        assert!(
            first.op_count() <= wholesale_first,
            "DOMINATION: reconcile <= wholesale on the first transition ({} <= {})",
            first.op_count(),
            wholesale_first
        );

        // SECOND reconcile, SAME desired: converged ⇒ 0 ops.
        let second = reconcile_song_store(tg.path(), &store).expect("second reconcile");
        assert_eq!(second.op_count(), 0, "converged reconcile pays 0 ops");
        // Wholesale on the SAME no-op transition would STILL churn: it deletes the
        // |desired| triples now in span and re-inserts them = 2 * |desired| ops.
        let wholesale_second = desired_len + desired_len;
        assert!(
            second.op_count() < wholesale_second,
            "DOMINATION (strict): converged reconcile {} < wholesale teardown-rebuild {}",
            second.op_count(),
            wholesale_second
        );
    }

    // ────────────────────────────────────────────────────────────────────────
    // (3) CONVERGENCE — a second reconcile of the same desired == 0 ops.
    // ────────────────────────────────────────────────────────────────────────
    /// `class_converged_zero_ops` across all three classes: reconcile, then
    /// reconcile the SAME desired → the second merged diff is EMPTY. The surveyed
    /// triples (read back out of the store across the 3 spans) must key-equal the
    /// bridged desired; this is where the `rdf_triple_to_term` round-trip and the
    /// canon collapse have to hold over EVERY class. If NONZERO, the residual diff
    /// is reported verbatim and the test FAILS (no papering over — a real finding).
    #[test]
    fn oracle_reconcile_converges_to_zero_ops() {
        let tg = TempGraph::new();
        let store = full_song();

        let first = reconcile_song_store(tg.path(), &store).expect("first reconcile");
        assert!(
            first.op_count() > 0,
            "the first reconcile must DO something"
        );

        let second = reconcile_song_store(tg.path(), &store).expect("second reconcile");
        if second.op_count() != 0 {
            eprintln!("CONVERGENCE FAILURE — residual diff after a no-op re-reconcile:");
            for (s, p, o) in &second.adds {
                eprintln!("  ADD    {s} {p} {o:?}");
            }
            for (s, p, o) in &second.removes {
                eprintln!("  REMOVE {s} {p} {o:?}");
            }
        }
        assert_eq!(
            second.op_count(),
            0,
            "CONVERGENCE: a second multi-class reconcile of the same desired must be 0 ops"
        );
    }

    // ────────────────────────────────────────────────────────────────────────
    // (4) ORDER-PRESERVATION-THROUGH-RECONCILE — verseIndex == position survives.
    // ────────────────────────────────────────────────────────────────────────
    /// After the MULTI-CLASS reconcile, the SongVerse span read back ORDER BY
    /// verseIndex has, for every i, subject `.../verse/{i}`, verseIndex == i, and
    /// content == verses[i].text. The positional order survives the union-of-
    /// classes path (the verse subjects are not reordered/renumbered by composing
    /// three spans). Anti-tautology: the verse count matches and a phantom verse
    /// position is absent.
    #[test]
    fn oracle_order_preserved_through_reconcile() {
        let tg = TempGraph::new();
        let store = store_with(
            vec![
                verse("alpha", &[], "1", "1"),
                verse("bravo", &["c"], "1", "1"),
                verse("charlie", &["c", "d"], "1", "1"),
                verse("delta", &[], "1", "1"),
            ],
            None,
        );
        let n = store.verses.len();

        reconcile_song_store(tg.path(), &store).expect("reconcile");

        let rows = read_verses_ordered(tg.path());
        assert_eq!(
            rows.len(),
            n,
            "every source verse projects exactly one verse subject through reconcile"
        );
        for (i, (subject, index, content)) in rows.iter().enumerate() {
            assert_eq!(
                *index, i as i64,
                "verseIndex == position for verse {i} (through reconcile)"
            );
            assert_eq!(
                subject,
                &verse_subject_model(i),
                "verse subject is position-keyed (through reconcile)"
            );
            assert_eq!(
                content, &store.verses[i].text,
                "verse {i} content matches source order (through reconcile)"
            );
        }
        // TEETH (negative): no phantom .../verse/{n} subject — the positional
        // minting through reconcile is exact, not over-emitting.
        assert!(
            subject_pairs(tg.path(), &verse_subject_model(n)).is_empty(),
            "no phantom .../verse/{n} subject through reconcile"
        );
    }

    // ────────────────────────────────────────────────────────────────────────
    // (5) CODA 0|1 — Option ⇒ 0-or-1 SongCoda subject, through reconcile.
    // ────────────────────────────────────────────────────────────────────────
    /// A store WITH a coda reconciles EXACTLY ONE SongCoda subject; WITHOUT a coda,
    /// zero. The Option cardinality holds through the multi-class path (the
    /// SongCoda class diff is empty when the coda subset is empty).
    #[test]
    fn oracle_coda_zero_or_one_through_reconcile() {
        // WITH coda ⇒ exactly one SongCoda subject, with its face.
        let tg_some = TempGraph::new();
        let coda = LocalSongCoda {
            text: "closing".to_string(),
            ejections_remaining: 8,
            created_at: "5".to_string(),
        };
        let store_some = store_with(vec![verse("only", &[], "1", "1")], Some(coda));
        reconcile_song_store(tg_some.path(), &store_some).expect("reconcile with coda");
        assert_eq!(
            count_type(tg_some.path(), &format!("{MNEMO_NS}SongCoda")),
            1,
            "CODA-1: a store with a coda reconciles EXACTLY one SongCoda subject"
        );
        let coda_face = subject_pairs(tg_some.path(), &coda_subject_model());
        assert!(
            coda_face.contains(&format!(
                "{} {}",
                uri(&format!("{MNEMO_NS}content")),
                lit("closing")
            )),
            "CODA-1: the coda subject carries its content"
        );
        assert!(
            coda_face.contains(&format!(
                "{} {}",
                uri(&format!("{MNEMO_NS}ejectionsRemaining")),
                int_lit(8)
            )),
            "CODA-1: the coda subject carries ejectionsRemaining"
        );

        // WITHOUT coda ⇒ zero SongCoda subjects.
        let tg_none = TempGraph::new();
        let store_none = store_with(vec![verse("only", &[], "1", "1")], None);
        reconcile_song_store(tg_none.path(), &store_none).expect("reconcile without coda");
        assert_eq!(
            count_type(tg_none.path(), &format!("{MNEMO_NS}SongCoda")),
            0,
            "CODA-0: a store with no coda reconciles ZERO SongCoda subjects"
        );
        assert!(
            subject_pairs(tg_none.path(), &coda_subject_model()).is_empty(),
            "CODA-0: no .../coda subject through reconcile"
        );
    }

    // ────────────────────────────────────────────────────────────────────────
    // (6) FOREIGN-narrativeKind GUARD — the ONE match-the-proof divergence, EXPLICIT.
    // ────────────────────────────────────────────────────────────────────────
    /// THE MATCH-THE-PROOF PRICE, made an EXPLICIT assertion (not a silent gap):
    /// a foreign subject carrying `mnemo:narrativeKind` + the cell `mnemo:graphId`
    /// but NO `mnemo:Song`/`SongVerse`/`SongCoda` rdf:type is the SINGLE point where
    /// the two paths diverge —
    ///   • the WHOLESALE `materialize_song_store` DELETE keys on narrativeKind-
    ///     PRESENCE (`?s mnemo:graphId "gid" ; mnemo:narrativeKind ?kind ; ?p ?o`),
    ///     so it RECLAIMS this subject;
    ///   • the reconcile keys on the 3 rdf:types, so its survey NEVER sees this
    ///     subject and it SURVIVES.
    /// Both are asserted on TWIN stores from an identical foreign seed, so the
    /// divergence is demonstrated, not assumed. The accompanying note: real song
    /// stores NEVER produce such a subject — every song subject minted by
    /// `song_value_triples` carries BOTH its rdf:type AND its narrativeKind — so
    /// under the construction premise the two paths coincide on all real input.
    #[test]
    fn oracle_foreign_narrativekind_divergence_is_explicit() {
        let tg_wholesale = TempGraph::new();
        let tg_reconcile = TempGraph::new();

        // The foreign subject: narrativeKind + cell graphId, but NO song rdf:type.
        // Seeded RAW (no materializer) into BOTH twins identically.
        let foreign = format!("urn:mnemosyne:local:graph:{GID}:foreign:rogue");
        for tg in [&tg_wholesale, &tg_reconcile] {
            seed_raw(
                tg.path(),
                &foreign,
                &format!("{MNEMO_NS}graphId"),
                &lit(GID),
            );
            seed_raw(
                tg.path(),
                &foreign,
                &format!("{MNEMO_NS}narrativeKind"),
                &lit("song-verse"),
            );
            seed_raw(
                tg.path(),
                &foreign,
                &format!("{MNEMO_NS}content"),
                &lit("a rogue verse"),
            );
            assert!(
                !subject_pairs(tg.path(), &foreign).is_empty(),
                "precondition: foreign narrativeKind subject seeded"
            );
        }

        // A real (well-formed) song over the SAME twins.
        let store = store_with(vec![verse("real verse", &["cp"], "1", "1")], None);
        materialize_song_store(tg_wholesale.path(), &store).expect("wholesale");
        reconcile_song_store(tg_reconcile.path(), &store).expect("reconcile");

        // WHOLESALE: the narrativeKind-keyed DELETE RECLAIMED the foreign subject.
        assert!(
            subject_pairs(tg_wholesale.path(), &foreign).is_empty(),
            "WHOLESALE reclaims the foreign narrativeKind subject (narrativeKind-presence DELETE)"
        );
        // RECONCILE: the rdf:type-keyed survey NEVER saw it ⇒ it SURVIVES.
        assert!(
            !subject_pairs(tg_reconcile.path(), &foreign).is_empty(),
            "RECONCILE does NOT survey the foreign subject (no song rdf:type) ⇒ it survives — \
             the ONE match-the-proof divergence, EXPLICIT"
        );

        // The real song projected identically on BOTH twins, set-equal AFTER
        // removing the foreign subject's triples from the wholesale side (it has
        // none) and from the reconcile side (the surviving divergence). I.e. on the
        // real-song subjects the two paths agree byte-for-byte.
        let real_subjects = [song_subject_model(), verse_subject_model(0)];
        for s in &real_subjects {
            assert_eq!(
                subject_pairs(tg_wholesale.path(), s),
                subject_pairs(tg_reconcile.path(), s),
                "real song subject {s} projects identically under both paths"
            );
        }

        // CONSTRUCTION-PREMISE WITNESS: every subject `song_value_triples` mints
        // carries BOTH its rdf:type AND a narrativeKind — so no real input ever
        // produces a foreign-shaped subject. Asserted over the actual builder
        // output (not assumed): every subject that has a narrativeKind also has a
        // song rdf:type.
        let desired = song_desired(&store);
        let song_types: BTreeSet<&str> = ["Song", "SongVerse", "SongCoda"].into_iter().collect();
        let mut kind_subjects: BTreeSet<String> = BTreeSet::new();
        let mut typed_subjects: BTreeSet<String> = BTreeSet::new();
        for (s, p, o) in &desired {
            if p == &format!("{MNEMO_NS}narrativeKind") {
                kind_subjects.insert(s.clone());
            }
            if p == RDF_TYPE {
                // object renders as `<iri>`; take the local name after the ns.
                let nt = o.as_nt();
                let iri = nt.trim_start_matches('<').trim_end_matches('>');
                if let Some(local) = iri.strip_prefix(MNEMO_NS) {
                    if song_types.contains(local) {
                        typed_subjects.insert(s.clone());
                    }
                }
            }
        }
        assert!(
            !kind_subjects.is_empty(),
            "real song mints narrativeKind-bearing subjects"
        );
        assert!(
            kind_subjects.is_subset(&typed_subjects),
            "CONSTRUCTION PREMISE: every narrativeKind subject also carries a song rdf:type \
             (so no real input is foreign-shaped — the divergence is unreachable on real stores)"
        );
    }
}

// ════════════════════════════════════════════════════════════════════════════
//  EA-2b+ SHACL CONFORMANCE ORACLE — SONG kind (the structural fork: a MULTI-CLASS
//  union Song / SongVerse / SongCoda).
//
//  The REAL song projection (`reconcile_song_store` into a real on-disk Oxigraph
//  store) CONFORMS to the SHACL shapes DERIVED from the `emporium-song` vocab
//  contract (`vocab_to_shacl` → THREE flat closed NodeShapes, one per class). NO
//  MOCKS: real reconcile, real store, real rudof engine. The validated triples are
//  READ BACK out of the persisted `:projection:song` graph (all three classes), so
//  the oracle covers the WHOLE materializer→store→shapes path.
//
//  DERIVED vs COMPLEMENT vs SHACL-INEXPRESSIBLE:
//   • DERIVED: the per-class field shapes (Song/SongVerse/SongCoda — rdf:type,
//     datatypes, required predicates, sh:closed). The multi-class union is just
//     three NodeShapes; vocab_to_shacl already handles it (no emitter change).
//   • SHACL-INEXPRESSIBLE in rudof 0.2.12 (no sh:sparql), stays Lean-proven +
//     oracle-checked HERE, NOT silently skipped:
//       - verseIndex == position (cross-class, spans subject-minting + literal);
//       - verseCount == |verses| (graph-level count on the head);
//       - voiceCount == 1 + counterpointCount (derived count);
//       - coda 0-or-1 cardinality (graph-level type count).
//     The `oracle_*` tests below assert each via a SPARQL read-back of the persisted
//     store (the honest hand-check that complements the SHACL field-conformance).
//
//  TEETH: (a) a verse MISSING its required mnemo:content is REJECTED (sh:minCount);
//         (b) a verse's mnemo:verseIndex carrying a STRING (declared xsd:integer) is
//             REJECTED (sh:datatype); (c) the verseIndex==position cross-class
//             invariant is asserted via SPARQL (the SHACL-inexpressible part).
// ════════════════════════════════════════════════════════════════════════════
#[cfg(test)]
mod shacl_song_conformance_oracle {
    use super::*;
    use crate::emporium::contract::song_vocabulary;
    use crate::emporium::shacl_validator::validate_desired;
    use crate::emporium::survey::parse_term as oracle_parse_term;
    use crate::emporium::terms::{Term, Triple as EngineTriple};
    use crate::geist_song_store::{LocalSongCoda, LocalSongStore, LocalSongVerse};
    use crate::rdf_query_service::execute_sparql_query;
    use oxigraph::sparql::{QueryResults, SparqlEvaluator};
    use uuid::Uuid;

    const GID: &str = "graph-song-shacl";

    struct TempGraph {
        dir: std::path::PathBuf,
    }
    impl TempGraph {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!("sophia-song-shacl-{}", Uuid::new_v4()));
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

    fn verse(text: &str, counterpoints: &[&str], created: &str, modified: &str) -> LocalSongVerse {
        LocalSongVerse {
            text: text.to_string(),
            counterpoints: counterpoints.iter().map(|c| c.to_string()).collect(),
            created_at: created.to_string(),
            updated_at: modified.to_string(),
        }
    }

    fn full_song() -> LocalSongStore {
        LocalSongStore {
            schema_version: 1,
            graph_id: GID.to_string(),
            observer: String::new(),
            verses: vec![
                verse("first verse", &["counter-a"], "1000", "1001"),
                verse("second verse", &["counter-b", "counter-c"], "2000", "2000"),
                verse("third verse", &[], "3000", "3500"),
            ],
            coda: Some(LocalSongCoda {
                text: "the closing".to_string(),
                ejections_remaining: 8,
                created_at: "9000".to_string(),
            }),
            archives: Vec::new(),
        }
    }

    /// Read the REAL persisted song projection back out of `:projection:song` as
    /// engine `Triple`s — ALL THREE classes (Song/SongVerse/SongCoda). Bridges each
    /// `?o` via `parse_term` (the survey round-trip).
    fn read_back_song_projection(graph_dir: &std::path::Path) -> Vec<EngineTriple> {
        let store = open_graph_store(graph_dir).expect("open store");
        let g = song_projection_graph_iri(GID);
        let query = format!("SELECT ?s ?p ?o WHERE {{ GRAPH <{g}> {{ ?s ?p ?o }} }}");
        let solutions = match SparqlEvaluator::new()
            .parse_query(&query)
            .expect("parse song readback")
            .on_store(&store)
            .execute()
            .expect("execute song readback")
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

    /// CONFORMANCE: the REAL multi-class song projection conforms to the three
    /// vocab-derived shapes (Song head + 3 verses + 1 coda all validate at once).
    #[test]
    fn shacl_oracle_song_conforms() {
        let tg = TempGraph::new();
        reconcile_song_store(tg.path(), &full_song()).expect("reconcile song");

        let projection = read_back_song_projection(tg.path());
        // 1 head (5) + 3 verses (11 each) + 1 coda (6) = 44 triples.
        assert_eq!(
            projection.len(),
            44,
            "song projection span size: {projection:#?}"
        );

        let result = validate_desired(&projection, song_vocabulary());
        assert!(
            result.is_ok(),
            "the REAL multi-class song projection must conform to the three vocab-derived \
             shapes (a violation = materializer↔vocab drift): {result:?}"
        );
    }

    /// TEETH #1: a verse MISSING its required mnemo:content is REJECTED (the
    /// SongVerse shape carries sh:minCount 1 on content).
    #[test]
    fn shacl_oracle_song_teeth_missing_required() {
        let tg = TempGraph::new();
        reconcile_song_store(tg.path(), &full_song()).expect("reconcile song");

        let mut bent = read_back_song_projection(tg.path());
        let content_p = format!("{MNEMO_NS}content");
        let verse0 = format!("urn:mnemosyne:local:graph:{GID}:song/verse/0");
        let before = bent.len();
        bent.retain(|(s, p, _)| !(s == &verse0 && p == &content_p));
        assert_eq!(bent.len(), before - 1, "dropped verse-0 content");

        let result = validate_desired(&bent, song_vocabulary());
        assert!(
            result.is_err(),
            "a SongVerse missing its required mnemo:content must be rejected"
        );
        assert!(result.unwrap_err().starts_with("SHACL:"));
    }

    /// TEETH #2: a verse's mnemo:verseIndex carrying a STRING (declared xsd:integer)
    /// is REJECTED (sh:datatype).
    #[test]
    fn shacl_oracle_song_teeth_wrong_datatype() {
        let tg = TempGraph::new();
        reconcile_song_store(tg.path(), &full_song()).expect("reconcile song");

        let mut bent = read_back_song_projection(tg.path());
        let idx_p = format!("{MNEMO_NS}verseIndex");
        let verse0 = format!("urn:mnemosyne:local:graph:{GID}:song/verse/0");
        // Replace the integer verseIndex with a string literal on verse 0.
        bent.retain(|(s, p, _)| !(s == &verse0 && p == &idx_p));
        bent.push((
            verse0,
            idx_p,
            Term::Lit(oxigraph::model::Literal::new_simple_literal("zero")),
        ));
        let result = validate_desired(&bent, song_vocabulary());
        assert!(
            result.is_err(),
            "mnemo:verseIndex carrying a string (declared xsd:integer) must be rejected"
        );
        assert!(result.unwrap_err().starts_with("SHACL:"));
    }

    /// SHACL-INEXPRESSIBLE COMPLEMENT (hand-checked, NOT silently skipped): the
    /// cross-class invariant verseIndex == position. rudof 0.2.12 has no sh:sparql,
    /// so this is asserted directly via a SPARQL read-back of the PERSISTED store:
    /// every verse subject `.../verse/{i}` carries verseIndex == i, in order, with
    /// no gaps, and the head's verseCount equals the verse count.
    #[test]
    fn shacl_complement_song_verseindex_equals_position() {
        let tg = TempGraph::new();
        reconcile_song_store(tg.path(), &full_song()).expect("reconcile song");
        let store = open_graph_store(tg.path()).expect("open store");
        let g = song_projection_graph_iri(GID);

        // Read every (subject, verseIndex) ORDER BY verseIndex from the real store.
        let q = format!(
            "SELECT ?s ?idx WHERE {{ GRAPH <{g}> {{ \
             ?s <{RDF_TYPE}> <{MNEMO_NS}SongVerse> ; <{MNEMO_NS}verseIndex> ?idx }} }} ORDER BY ?idx"
        );
        let res = execute_sparql_query(&store, &q).expect("query verses ordered");
        assert_eq!(res.rows.len(), 3, "three verses persisted");
        for (expected_i, row) in res.rows.iter().enumerate() {
            // ?idx renders as `"N"^^<…integer>`; strip to the bare i64.
            let idx: i64 = row["idx"]
                .split('"')
                .nth(1)
                .and_then(|d| d.parse().ok())
                .unwrap_or(-1);
            assert_eq!(
                idx, expected_i as i64,
                "INVARIANT (SHACL-inexpressible): the {expected_i}-th verse carries verseIndex {expected_i}"
            );
            // The subject URI's /verse/{i} fragment == the verseIndex literal.
            let s = row["s"].trim_start_matches('<').trim_end_matches('>');
            assert!(
                s.ends_with(&format!("/verse/{expected_i}")),
                "INVARIANT: subject {s} is position-keyed at {expected_i} (== verseIndex)"
            );
        }

        // verseCount on the head == |verses| (the graph-level count, also SHACL-inexpressible).
        let qc = format!(
            "SELECT ?n WHERE {{ GRAPH <{g}> {{ \
             ?s <{RDF_TYPE}> <{MNEMO_NS}Song> ; <{MNEMO_NS}verseCount> ?n }} }}"
        );
        let resc = execute_sparql_query(&store, &qc).expect("query verseCount");
        let n: i64 = resc.rows[0]["n"]
            .split('"')
            .nth(1)
            .and_then(|d| d.parse().ok())
            .unwrap_or(-1);
        assert_eq!(n, 3, "INVARIANT: head verseCount == |verses| == 3");

        // Coda 0-or-1: exactly one SongCoda subject (this store has a coda).
        let qd = format!(
            "SELECT (COUNT(DISTINCT ?s) AS ?n) WHERE {{ GRAPH <{g}> {{ \
             ?s <{RDF_TYPE}> <{MNEMO_NS}SongCoda> }} }}"
        );
        let resd = execute_sparql_query(&store, &qd).expect("count codas");
        let nd: i64 = resd.rows[0]["n"]
            .split('"')
            .nth(1)
            .and_then(|d| d.parse().ok())
            .unwrap_or(-1);
        assert_eq!(nd, 1, "INVARIANT: coda cardinality is 0-or-1 (here 1)");
    }
}

/// ── PER-AGENT SONG isolation — the acceptance organism's AC4 (fix #3) ─────────
///
/// Two distinct observers each sing into the SAME graph dir; after REPEATED
/// `reconcile_song_store` persists (the live song projection path — its GRAPH-
/// scoped `narrativeKind`-presence DELETE), BOTH Songs coexist intact. Pre-fix,
/// the singleton's per-`graphId` DELETE wiped the co-tenant on every persist. NO
/// mocks — real `LocalSongStore`, real oxigraph, real reconcile path.
#[cfg(test)]
mod per_agent_song_isolation {
    use super::*;
    use crate::geist_song_store::{LocalSongStore, LocalSongVerse};
    use crate::rdf_authority::song_projection_graph_iri_for;
    use crate::rdf_query_service::execute_sparql_query;
    use std::collections::BTreeSet;
    use uuid::Uuid;

    const GID: &str = "graph-song-iso";

    struct TempGraph {
        dir: std::path::PathBuf,
    }
    impl TempGraph {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!("sophia-song-iso-{}", Uuid::new_v4()));
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

    fn song_for(observer: &str, verse_text: &str) -> LocalSongStore {
        LocalSongStore {
            schema_version: 1,
            graph_id: GID.to_string(),
            observer: observer.to_string(),
            verses: vec![LocalSongVerse {
                text: verse_text.to_string(),
                counterpoints: Vec::new(),
                created_at: "1000".to_string(),
                updated_at: "1000".to_string(),
            }],
            coda: None,
            archives: Vec::new(),
        }
    }

    /// Count the Song heads (mnemo:Song subjects) in a specific observer's graph.
    fn song_heads(graph_dir: &std::path::Path, observer: &str) -> usize {
        let store = open_graph_store(graph_dir).expect("open store");
        let g = song_projection_graph_iri_for(GID, observer);
        let q = format!("SELECT ?s WHERE {{ GRAPH <{g}> {{ ?s a <{MNEMO_NS}Song> }} }}");
        execute_sparql_query(&store, &q).expect("query").rows.len()
    }

    /// The verse text in a specific observer's graph (proves the CONTENT survived,
    /// not just the head).
    fn verse_texts(graph_dir: &std::path::Path, observer: &str) -> BTreeSet<String> {
        let store = open_graph_store(graph_dir).expect("open store");
        let g = song_projection_graph_iri_for(GID, observer);
        let q = format!(
            "SELECT ?c WHERE {{ GRAPH <{g}> {{ ?s a <{MNEMO_NS}SongVerse> ; <{MNEMO_NS}content> ?c }} }}"
        );
        execute_sparql_query(&store, &q)
            .expect("query")
            .rows
            .iter()
            .map(|row| row["c"].trim_matches('"').to_string())
            .collect()
    }

    #[test]
    fn two_observers_songs_coexist_through_repeated_persist() {
        let tg = TempGraph::new();
        let a = song_for("agent-aaaa", "A's first line about the sea");
        let b = song_for("agent-bbbb", "B's first line about the sky");

        // Each observer persists its Song (the live reconcile path), interleaved
        // and REPEATED — exactly the co-tenant respawn/churn scenario.
        for _ in 0..3 {
            reconcile_song_store(tg.path(), &a).expect("persist A");
            reconcile_song_store(tg.path(), &b).expect("persist B");
            // After B persists, A's Song MUST still be intact (the headline fix:
            // pre-fix, B's GRAPH-scoped delete in the SHARED graph wiped A).
            assert_eq!(
                song_heads(tg.path(), "agent-aaaa"),
                1,
                "A's Song survives B's persist"
            );
            assert_eq!(
                song_heads(tg.path(), "agent-bbbb"),
                1,
                "B's Song is present"
            );
        }

        // Both Songs coexist with their OWN content.
        let a_verses = verse_texts(tg.path(), "agent-aaaa");
        let b_verses = verse_texts(tg.path(), "agent-bbbb");
        assert!(
            a_verses.contains("A's first line about the sea"),
            "A's verse content intact: {a_verses:?}"
        );
        assert!(
            b_verses.contains("B's first line about the sky"),
            "B's verse content intact: {b_verses:?}"
        );
        // ISOLATION: A's graph never holds B's verse and vice-versa.
        assert!(a_verses.is_disjoint(&b_verses), "AC4: A∩B song content = ∅");

        // The two Songs are in DISTINCT named graphs (the GRAPH-scoped DELETE can't
        // cross), and the shared singleton graph is EMPTY (no leakage).
        assert_eq!(
            song_heads(tg.path(), ""),
            0,
            "nothing leaks into the shared singleton graph"
        );
    }

    #[test]
    fn empty_observer_song_is_the_unchanged_singleton() {
        // The commons path (empty observer) still writes the singleton graph,
        // byte-identical to today — co-existing with per-agent Songs in the SAME
        // dir without collision.
        let tg = TempGraph::new();
        let commons = song_for("", "the shared default verse");
        let agent = song_for("agent-aaaa", "an agent's private verse");
        reconcile_song_store(tg.path(), &commons).expect("persist commons");
        reconcile_song_store(tg.path(), &agent).expect("persist agent");
        // Commons re-persist must NOT touch the agent's Song.
        reconcile_song_store(tg.path(), &commons).expect("re-persist commons");

        assert_eq!(
            song_heads(tg.path(), ""),
            1,
            "the singleton Song is in the shared graph"
        );
        assert_eq!(
            song_heads(tg.path(), "agent-aaaa"),
            1,
            "the agent Song survives commons churn"
        );
    }
}
