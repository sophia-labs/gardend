//! `memory_semantic_recall` — semantic recall over the `:projection:memory`
//! commons graph. Reads the memory heads via SPARQL — live (CURRENT,
//! non-superseded, active) heads by default, or heads reconstructed AS OF a
//! past `cutoff` from `createdAt` + `supersedes` alone when one is given, so
//! a record superseded only AFTER `cutoff` still surfaces as the as-of head
//! (see `candidate_query`) — the query text NEVER touches the query string
//! (it exists only on the embedding side) — then embeds the query +
//! candidate contents with the same in-cell embedder machinery
//! `lme_recall_benchmark.rs` uses (`embed_texts` + `normalize_vector` +
//! `dot_product`) and ranks by cosine similarity.
//!
//! Split for testability the same way `lme_recall_benchmark.rs` is: the
//! candidate SELECT (`read_candidate_records`) and the ranking math
//! (`rank_candidates`) are pure, `&Store`/vector-only functions with no
//! `AppHandle` dependency, so they run under the default (non-headless) test
//! suite with fixture vectors — no embedder or ONNX runtime required. Only
//! the top-level [`memory_semantic_recall`] touches `AppHandle` (graph
//! resolution + the real embedder).

use crate::app_runtime::AppHandle;
use crate::{
    paths::existing_graph_dir,
    rdf_authority::memory_projection_graph_iri,
    rdf_service::open_graph_store,
    runtime_config::XSD_NS,
    semantic_embedder::embed_texts,
    semantic_index::{dot_product, normalize_vector},
    semantic_mcp_inputs::memory_semantic_recall_input_from_mcp_args,
    semantic_models::{read_semantic_model_config, semantic_model_spec_by_id},
};
use oxigraph::{
    model::Term,
    sparql::{QueryResults, SparqlEvaluator},
    store::Store,
};
use serde::{Deserialize, Serialize};

const MEM_NS: &str = "http://mnemosyne.dev/memory#";
/// Candidate cap on the SPARQL side (distinct from `k`, the ranked-result cap).
const CANDIDATE_LIMIT: usize = 500;
const DEFAULT_K: usize = 10;
const MAX_K: usize = 50;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct MemorySemanticRecallInput {
    pub(super) graph_id: String,
    pub(super) query: String,
    pub(super) k: Option<usize>,
    pub(super) cutoff: Option<String>,
    pub(super) include_episodes: Option<bool>,
    pub(super) min_score: Option<f32>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct MemorySemanticRecallHit {
    pub(super) subject: String,
    pub(super) content: String,
    pub(super) kind: Option<String>,
    pub(super) scope: Option<String>,
    pub(super) content_orientation: Option<String>,
    pub(super) created_at: Option<String>,
    pub(super) score: f32,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct MemorySemanticRecallResult {
    pub(super) graph_id: String,
    pub(super) k: usize,
    pub(super) provider_id: String,
    pub(super) model_id: String,
    pub(super) dimensions: usize,
    pub(super) candidate_count: usize,
    pub(super) hits: Vec<MemorySemanticRecallHit>,
}

pub(super) fn mcp_local_memory_semantic_recall(
    app: AppHandle,
    arguments: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    serde_json::to_value(memory_semantic_recall(
        app,
        memory_semantic_recall_input_from_mcp_args(arguments),
    )?)
    .map_err(|error| error.to_string())
}

/// The AppHandle-driven entry point: resolves the graph, validates `cutoff`,
/// reads the current-head candidates, and — only if there ARE candidates —
/// embeds the query + contents through the real embedder and ranks them.
///
/// An empty candidate set (a genuinely empty projection, or every record
/// excluded by `cutoff`/episode filtering) short-circuits BEFORE the embedder
/// is touched: that is a legitimate `{ hits: [], candidateCount: 0 }` success,
/// never conflated with "the embedder is unavailable". When candidates DO
/// exist, any embedder failure (unprepared model, missing ONNX runtime, …)
/// propagates loudly — never silently-empty hits.
pub(super) fn memory_semantic_recall(
    app: AppHandle,
    input: MemorySemanticRecallInput,
) -> Result<MemorySemanticRecallResult, String> {
    let graph_dir = existing_graph_dir(&app, &input.graph_id)?;
    let query = input.query.trim();
    if query.is_empty() {
        return Err("memory_semantic_recall query cannot be empty".to_string());
    }
    let k = input.k.unwrap_or(DEFAULT_K).clamp(1, MAX_K);
    let min_score = input.min_score.unwrap_or(0.0);
    let include_episodes = input.include_episodes.unwrap_or(false);
    let cutoff_literal = match input.cutoff.as_deref().map(str::trim) {
        None => None,
        Some(raw) if raw.is_empty() => {
            return Err("memory_semantic_recall cutoff cannot be empty".to_string())
        }
        Some(raw) => Some(validate_cutoff(raw)?),
    };

    let store = open_graph_store(&graph_dir)?;
    let mem_graph = memory_projection_graph_iri(&input.graph_id);
    let candidates = read_candidate_records(
        &store,
        &mem_graph,
        include_episodes,
        cutoff_literal.as_deref(),
    )?;
    let candidate_count = candidates.len();

    let config = read_semantic_model_config(&app)?;
    let spec = semantic_model_spec_by_id(&config.selected_model_id)?;

    if candidates.is_empty() {
        return Ok(MemorySemanticRecallResult {
            graph_id: input.graph_id,
            k,
            provider_id: spec.provider_id.to_string(),
            model_id: spec.model_id.to_string(),
            dimensions: spec.dimensions,
            candidate_count,
            hits: Vec::new(),
        });
    }

    let mut query_vectors = embed_texts(&app, &[query.to_string()], "search_query")?;
    let Some(query_vector) = query_vectors.pop() else {
        return Err("fastembed returned no query vector".to_string());
    };
    let query_vector = normalize_vector(query_vector);

    let contents: Vec<String> = candidates.iter().map(|c| c.content.clone()).collect();
    let content_vectors = embed_texts(&app, &contents, "search_document")?;

    let hits = rank_candidates(candidates, &query_vector, &content_vectors, k, min_score)?;

    Ok(MemorySemanticRecallResult {
        graph_id: input.graph_id,
        k,
        provider_id: spec.provider_id.to_string(),
        model_id: spec.model_id.to_string(),
        dimensions: spec.dimensions,
        candidate_count,
        hits,
    })
}

/// Strictly parse+validate `cutoff` as an RFC3339 / xsd:dateTime instant
/// (requires an explicit offset or `Z` — never trust a bare, timezone-less
/// string), then re-serialize it so only a value we minted ourselves is ever
/// placed in the SPARQL FILTER literal.
fn validate_cutoff(raw: &str) -> Result<String, String> {
    chrono::DateTime::parse_from_rfc3339(raw)
        .map(|parsed| parsed.to_rfc3339())
        .map_err(|error| {
            format!(
                "memory_semantic_recall cutoff must be a valid ISO 8601 / xsd:dateTime instant \
                 (e.g. \"2026-07-01T00:00:00Z\"): {raw:?} ({error})"
            )
        })
}

#[derive(Debug, Clone)]
struct MemoryCandidate {
    subject: String,
    content: String,
    kind: Option<String>,
    scope: Option<String>,
    content_orientation: Option<String>,
    created_at: Option<String>,
}

/// Build the candidate SELECT. `content` is a mandatory bind (a row without
/// it cannot be a real MemoryRecord under the golden contract);
/// `kind`/`scope`/`contentOrientation`/`createdAt` are OPTIONAL.
/// `cutoff_literal` (already validated + re-serialized xsd:dateTime text) is
/// compared as a TYPED literal, never as an integer or a bare string — the
/// cell materializes `createdAt` as `xsd:dateTime`.
///
/// Two distinct read modes, NOT one query with a bolted-on filter:
/// - **Live** (`cutoff_literal` is `None`): the CURRENT head — live
///   `mem:status = "active"` and no record currently supersedes `?m`.
/// - **Cutoff** (as-of a past instant): the head that was current AS OF
///   `cutoff`, reconstructed from `createdAt` + `supersedes` alone. Live
///   `mem:status` is a present-tense field — a record superseded AFTER
///   `cutoff` still carries `status = "superseded"` today even though it
///   WAS the head at `cutoff` — so cutoff mode does not require it. A
///   superseder only disqualifies `?m` as the as-of head if the superseder
///   itself existed by `cutoff` (its own `createdAt <= cutoff`); an
///   untimestamped supersession filter would smuggle a FUTURE supersession
///   into a past read. Mirrors the `heads_as_of` reconstruction in
///   `emporium/sweep.rs`.
fn candidate_query(
    mem_graph: &str,
    include_episodes: bool,
    cutoff_literal: Option<&str>,
) -> String {
    let episode_filter = if include_episodes {
        String::new()
    } else {
        "    FILTER (!BOUND(?kind) || ?kind != \"EpisodeMemory\")\n".to_string()
    };
    let cutoff_filter = match cutoff_literal {
        Some(literal) => format!(
            "    FILTER (BOUND(?created) && ?created <= \"{}\"^^<{XSD_NS}dateTime>)\n",
            escape_sparql_string(literal)
        ),
        None => String::new(),
    };
    let supersedes_filter = match cutoff_literal {
        Some(literal) => format!(
            "FILTER NOT EXISTS {{ \
             ?newer <{MEM_NS}supersedes> ?m ; <{MEM_NS}createdAt> ?newerCreated . \
             FILTER(?newerCreated <= \"{}\"^^<{XSD_NS}dateTime>) \
             }}",
            escape_sparql_string(literal)
        ),
        None => format!("FILTER NOT EXISTS {{ ?newer <{MEM_NS}supersedes> ?m }}"),
    };

    let mut lines: Vec<String> = vec![
        "SELECT ?m ?content ?kind ?scope ?co ?created WHERE {".to_string(),
        format!("  GRAPH <{mem_graph}> {{"),
        format!("    ?m a <{MEM_NS}MemoryRecord> ;"),
    ];
    if cutoff_literal.is_none() {
        lines.push(format!("       <{MEM_NS}status> \"active\" ;"));
    }
    lines.push(format!("       <{MEM_NS}content> ?content ."));
    lines.push(format!("    OPTIONAL {{ ?m <{MEM_NS}kind> ?kind }}"));
    lines.push(format!("    OPTIONAL {{ ?m <{MEM_NS}scope> ?scope }}"));
    lines.push(format!(
        "    OPTIONAL {{ ?m <{MEM_NS}contentOrientation> ?co }}"
    ));
    lines.push(format!(
        "    OPTIONAL {{ ?m <{MEM_NS}createdAt> ?created }}"
    ));
    lines.push(format!(
        "{episode_filter}{cutoff_filter}    {supersedes_filter}"
    ));
    lines.push(format!("  }}\n}} LIMIT {CANDIDATE_LIMIT}"));
    lines.join("\n")
}

/// Defensive escaping for the cutoff literal (belt-and-suspenders: the raw
/// text only ever reaches here after `validate_cutoff` re-serialized it via
/// chrono, so it can never contain `"`/`\`, but this file NEVER trusts a
/// string into a query unescaped).
fn escape_sparql_string(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Pure store-level candidate read — no `AppHandle`, so it is directly
/// testable against a plain `Store::new()` fixture.
fn read_candidate_records(
    store: &Store,
    mem_graph: &str,
    include_episodes: bool,
    cutoff_literal: Option<&str>,
) -> Result<Vec<MemoryCandidate>, String> {
    let query = candidate_query(mem_graph, include_episodes, cutoff_literal);
    let prepared = SparqlEvaluator::new()
        .parse_query(&query)
        .map_err(|error| format!("parse memory_semantic_recall candidate query: {error}"))?;
    let solutions = match prepared
        .on_store(store)
        .execute()
        .map_err(|error| format!("execute memory_semantic_recall candidate query: {error}"))?
    {
        QueryResults::Solutions(solutions) => solutions,
        _ => {
            return Err(
                "memory_semantic_recall candidate query did not return solutions".to_string(),
            )
        }
    };

    let mut candidates = Vec::new();
    for solution in solutions.flatten() {
        let Some(Term::NamedNode(subject)) = solution.get("m") else {
            continue;
        };
        let Some(Term::Literal(content)) = solution.get("content") else {
            continue;
        };
        candidates.push(MemoryCandidate {
            subject: subject.as_str().to_string(),
            content: content.value().to_string(),
            kind: literal_value(solution.get("kind")),
            scope: literal_value(solution.get("scope")),
            content_orientation: literal_value(solution.get("co")),
            created_at: literal_value(solution.get("created")),
        });
    }
    Ok(candidates)
}

fn literal_value(term: Option<&Term>) -> Option<String> {
    match term {
        Some(Term::Literal(literal)) => Some(literal.value().to_string()),
        _ => None,
    }
}

/// Pure ranking core — no `AppHandle`, no embedder call. Takes precomputed,
/// UNNORMALIZED content vectors (normalizes them here) and an already-
/// normalized `query_vector`; scores by dot product (cosine similarity on
/// unit vectors), sorts descending, drops anything below `min_score`, then
/// truncates to `k`. Testable with fixture vectors the same way
/// `lme_recall_benchmark.rs::score_lme_answer_session_recall_from_parts` is.
fn rank_candidates(
    candidates: Vec<MemoryCandidate>,
    query_vector: &[f32],
    content_vectors: &[Vec<f32>],
    k: usize,
    min_score: f32,
) -> Result<Vec<MemorySemanticRecallHit>, String> {
    if candidates.len() != content_vectors.len() {
        return Err(format!(
            "fastembed returned {} vectors for {} candidate contents",
            content_vectors.len(),
            candidates.len()
        ));
    }
    let mut scored: Vec<MemorySemanticRecallHit> = candidates
        .into_iter()
        .zip(content_vectors.iter())
        .map(|(candidate, vector)| {
            let normalized = normalize_vector(vector.clone());
            let score = dot_product(query_vector, &normalized);
            MemorySemanticRecallHit {
                subject: candidate.subject,
                content: candidate.content,
                kind: candidate.kind,
                scope: candidate.scope,
                content_orientation: candidate.content_orientation,
                created_at: candidate.created_at,
                score,
            }
        })
        .collect();
    scored.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    scored.retain(|hit| hit.score >= min_score);
    scored.truncate(k);
    Ok(scored)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rdf::{
        format_rdf_triple, push_string_triple, push_typed_literal_triple, push_uri_triple,
        RdfTriple,
    };
    use crate::runtime_config::RDF_TYPE;
    use oxigraph::io::{RdfFormat, RdfParser};

    const MEM_GRAPH: &str = "urn:mnemosyne:local:graph:lab:projection:memory";

    fn push_memory_record(
        triples: &mut Vec<RdfTriple>,
        subject: &str,
        content: &str,
        kind: Option<&str>,
        created_at_iso: &str,
        status: &str,
    ) {
        push_uri_triple(triples, subject, RDF_TYPE, &format!("{MEM_NS}MemoryRecord"));
        push_string_triple(triples, subject, &format!("{MEM_NS}content"), content);
        push_string_triple(triples, subject, &format!("{MEM_NS}status"), status);
        if let Some(kind) = kind {
            push_string_triple(triples, subject, &format!("{MEM_NS}kind"), kind);
        }
        push_typed_literal_triple(
            triples,
            subject,
            &format!("{MEM_NS}createdAt"),
            created_at_iso,
            &format!("{XSD_NS}dateTime"),
        );
    }

    fn push_supersedes(triples: &mut Vec<RdfTriple>, newer_subject: &str, older_subject: &str) {
        push_uri_triple(
            triples,
            newer_subject,
            &format!("{MEM_NS}supersedes"),
            older_subject,
        );
    }

    fn load_into(store: &Store, triples: &[RdfTriple]) {
        let trig = triples
            .iter()
            .map(format_rdf_triple)
            .map(|triple| format!("<{MEM_GRAPH}> {{ {triple} }}"))
            .collect::<Vec<_>>()
            .join("\n");
        store
            .load_from_slice(RdfParser::from_format(RdfFormat::TriG), trig.as_bytes())
            .expect("load memory fixture trig");
    }

    fn subjects(candidates: &[MemoryCandidate]) -> Vec<&str> {
        candidates.iter().map(|c| c.subject.as_str()).collect()
    }

    #[test]
    fn empty_projection_yields_zero_candidates() {
        let store = Store::new().expect("in-memory store");
        let candidates = read_candidate_records(&store, MEM_GRAPH, false, None)
            .expect("query an empty projection");
        assert!(candidates.is_empty());
    }

    #[test]
    fn superseded_records_are_excluded() {
        let store = Store::new().expect("in-memory store");
        let mut triples = Vec::new();
        push_memory_record(
            &mut triples,
            "urn:test:mem:h",
            "vera prefers the fish shell",
            Some("ClaimMemory"),
            "2026-06-01T00:00:00+00:00",
            "active",
        );
        push_memory_record(
            &mut triples,
            "urn:test:mem:v2",
            "vera prefers zsh",
            Some("ClaimMemory"),
            "2026-06-02T00:00:00+00:00",
            "active",
        );
        push_supersedes(&mut triples, "urn:test:mem:v2", "urn:test:mem:h");
        load_into(&store, &triples);

        let candidates =
            read_candidate_records(&store, MEM_GRAPH, false, None).expect("query candidates");
        assert_eq!(subjects(&candidates), vec!["urn:test:mem:v2"]);
    }

    #[test]
    fn inactive_status_is_excluded() {
        let store = Store::new().expect("in-memory store");
        let mut triples = Vec::new();
        push_memory_record(
            &mut triples,
            "urn:test:mem:retracted",
            "no longer asserted",
            Some("ClaimMemory"),
            "2026-06-01T00:00:00+00:00",
            "retracted",
        );
        load_into(&store, &triples);

        let candidates =
            read_candidate_records(&store, MEM_GRAPH, false, None).expect("query candidates");
        assert!(candidates.is_empty());
    }

    #[test]
    fn cutoff_excludes_records_created_after_it() {
        let store = Store::new().expect("in-memory store");
        let mut triples = Vec::new();
        push_memory_record(
            &mut triples,
            "urn:test:mem:early",
            "filed before the cutoff",
            Some("ClaimMemory"),
            "2026-06-01T00:00:00+00:00",
            "active",
        );
        push_memory_record(
            &mut triples,
            "urn:test:mem:late",
            "filed after the cutoff",
            Some("ClaimMemory"),
            "2026-06-10T00:00:00+00:00",
            "active",
        );
        load_into(&store, &triples);

        let cutoff = validate_cutoff("2026-06-05T00:00:00Z").expect("valid cutoff");
        let candidates = read_candidate_records(&store, MEM_GRAPH, false, Some(&cutoff))
            .expect("query candidates as-of cutoff");
        assert_eq!(subjects(&candidates), vec!["urn:test:mem:early"]);

        // A cutoff at/after both createdAt stamps includes both.
        let cutoff_late = validate_cutoff("2026-06-11T00:00:00Z").expect("valid cutoff");
        let mut both = read_candidate_records(&store, MEM_GRAPH, false, Some(&cutoff_late))
            .expect("query candidates as-of later cutoff");
        both.sort_by(|a, b| a.subject.cmp(&b.subject));
        assert_eq!(
            subjects(&both),
            vec!["urn:test:mem:early", "urn:test:mem:late"]
        );
    }

    /// Regression for the `cutoff` as-of reconstruction: a record created
    /// BEFORE the cutoff but superseded AFTER it must still surface as the
    /// as-of head, even though its LIVE status is now "superseded" and a
    /// newer record DOES currently supersede it. A cutoff at/after the
    /// supersession must instead surface the newer record and exclude the
    /// old one.
    #[test]
    fn cutoff_reconstructs_the_head_that_was_current_even_if_later_superseded() {
        let store = Store::new().expect("in-memory store");
        let mut triples = Vec::new();
        push_memory_record(
            &mut triples,
            "urn:test:mem:h",
            "vera prefers the fish shell",
            Some("ClaimMemory"),
            "2026-06-01T00:00:00+00:00",
            "superseded", // live status: this head has since been superseded
        );
        push_memory_record(
            &mut triples,
            "urn:test:mem:v2",
            "vera prefers zsh",
            Some("ClaimMemory"),
            "2026-06-10T00:00:00+00:00", // supersession happens AFTER the cutoff below
            "active",
        );
        push_supersedes(&mut triples, "urn:test:mem:v2", "urn:test:mem:h");
        load_into(&store, &triples);

        // Cutoff BEFORE the supersession: h was the head at that point —
        // must be returned even though its live status is "superseded" and
        // v2 currently supersedes it (v2 didn't exist yet as of cutoff).
        let cutoff_before = validate_cutoff("2026-06-05T00:00:00Z").expect("valid cutoff");
        let candidates = read_candidate_records(&store, MEM_GRAPH, false, Some(&cutoff_before))
            .expect("query candidates as-of cutoff before supersession");
        assert_eq!(
            subjects(&candidates),
            vec!["urn:test:mem:h"],
            "the as-of head must be reconstructed from createdAt+supersedes, \
             not filtered out by present-tense status/supersession"
        );

        // Cutoff AFTER the supersession: v2 is now the as-of head, h is not.
        let cutoff_after = validate_cutoff("2026-06-15T00:00:00Z").expect("valid cutoff");
        let candidates_after =
            read_candidate_records(&store, MEM_GRAPH, false, Some(&cutoff_after))
                .expect("query candidates as-of cutoff after supersession");
        assert_eq!(subjects(&candidates_after), vec!["urn:test:mem:v2"]);
    }

    #[test]
    fn episode_memory_excluded_by_default_and_included_when_flagged() {
        let store = Store::new().expect("in-memory store");
        let mut triples = Vec::new();
        push_memory_record(
            &mut triples,
            "urn:test:mem:claim",
            "a durable claim",
            Some("ClaimMemory"),
            "2026-06-01T00:00:00+00:00",
            "active",
        );
        push_memory_record(
            &mut triples,
            "urn:test:mem:episode",
            "a one-off episodic note",
            Some("EpisodeMemory"),
            "2026-06-01T00:00:00+00:00",
            "active",
        );
        load_into(&store, &triples);

        let default_candidates =
            read_candidate_records(&store, MEM_GRAPH, false, None).expect("query default");
        assert_eq!(subjects(&default_candidates), vec!["urn:test:mem:claim"]);

        let mut with_episodes =
            read_candidate_records(&store, MEM_GRAPH, true, None).expect("query with episodes");
        with_episodes.sort_by(|a, b| a.subject.cmp(&b.subject));
        assert_eq!(
            subjects(&with_episodes),
            vec!["urn:test:mem:claim", "urn:test:mem:episode"]
        );
    }

    #[test]
    fn validate_cutoff_rejects_malformed_input() {
        let error = validate_cutoff("not-a-datetime").expect_err("malformed cutoff must error");
        assert!(
            error.contains("cutoff must be a valid ISO 8601"),
            "error should name the problem clearly: {error}"
        );
    }

    #[test]
    fn validate_cutoff_accepts_rfc3339_with_or_without_fractional_seconds() {
        assert!(validate_cutoff("2026-07-01T00:00:00Z").is_ok());
        assert!(validate_cutoff("2026-07-01T00:00:00.123+00:00").is_ok());
        assert!(
            validate_cutoff("2026-07-01T00:00:00").is_err(),
            "no offset must be rejected"
        );
    }

    #[test]
    fn rank_candidates_orders_by_score_applies_min_score_and_k() {
        let candidates = vec![
            MemoryCandidate {
                subject: "urn:test:mem:a".to_string(),
                content: "vera prefers the fish shell".to_string(),
                kind: None,
                scope: None,
                content_orientation: None,
                created_at: None,
            },
            MemoryCandidate {
                subject: "urn:test:mem:b".to_string(),
                content: "vera dislikes bash".to_string(),
                kind: None,
                scope: None,
                content_orientation: None,
                created_at: None,
            },
            MemoryCandidate {
                subject: "urn:test:mem:c".to_string(),
                content: "unrelated content about weather".to_string(),
                kind: None,
                scope: None,
                content_orientation: None,
                created_at: None,
            },
        ];
        // Fixture vectors (already unit-ish; rank_candidates re-normalizes):
        // query is closest to `a`, next closest to `b`, orthogonal to `c`.
        let query_vector = normalize_vector(vec![1.0, 0.0]);
        let content_vectors = vec![
            vec![1.0, 0.0], // a: cos = 1.0
            vec![0.7, 0.3], // b: cos < 1.0 but > 0
            vec![0.0, 1.0], // c: cos = 0.0
        ];

        let ranked = rank_candidates(candidates.clone(), &query_vector, &content_vectors, 2, 0.0)
            .expect("rank candidates");
        assert_eq!(ranked.len(), 2, "k=2 truncates to two hits");
        assert_eq!(ranked[0].subject, "urn:test:mem:a");
        assert_eq!(ranked[1].subject, "urn:test:mem:b");
        assert!(ranked[0].score >= ranked[1].score, "descending by score");

        let filtered = rank_candidates(candidates, &query_vector, &content_vectors, 10, 0.5)
            .expect("rank candidates with min_score");
        assert!(
            filtered.iter().all(|hit| hit.score >= 0.5),
            "min_score threshold must hold: {filtered:?}"
        );
        assert!(
            !filtered.iter().any(|hit| hit.subject == "urn:test:mem:c"),
            "orthogonal candidate must be filtered by min_score"
        );
    }

    #[test]
    fn rank_candidates_rejects_vector_count_mismatch() {
        let candidates = vec![MemoryCandidate {
            subject: "urn:test:mem:a".to_string(),
            content: "content".to_string(),
            kind: None,
            scope: None,
            content_orientation: None,
            created_at: None,
        }];
        let query_vector = normalize_vector(vec![1.0, 0.0]);
        let error = rank_candidates(candidates, &query_vector, &[], 10, 0.0)
            .expect_err("vector count mismatch must error");
        assert!(error.contains("fastembed returned"));
    }
}
