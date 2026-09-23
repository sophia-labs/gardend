//! Graph search entry points. Every consumer — the MCP tools and all three
//! loopback routes the frontend calls — funnels through these two functions,
//! so ranking behavior lands everywhere at once.
//!
//! Hybrid ranking is Reciprocal Rank Fusion: BM25 scores are unbounded and
//! cosine similarity's range shifts per embedding model, so the two lists
//! fuse by RANK, not by score. A block found by both paths collects both
//! contributions and naturally rises (`match_source: "both"`). Raw
//! per-source scores stay on each hit (`lexical_score`, `semantic_score`).

use std::collections::HashMap;

use crate::app_runtime::AppHandle;
use crate::{
    document_service::read_graph_documents_cold,
    json_utils::json_number,
    mcp_utils::{
        mcp_arg_bool, mcp_arg_string, mcp_arg_usize, mcp_query_terms, mcp_required_graph_id,
    },
    paths::existing_graph_dir,
    search_lexical_projection::{ranked_lexical_block_hits, LexicalBlockHit},
    semantic_search_projection::SemanticSearchHit,
    semantic_service::{semantic_search, SemanticSearchInput},
};

const RRF_K: f64 = 60.0;
/// Diversity cap: at most this many hits per document when no doc_filter is
/// set (a doc-scoped search must be allowed to fill from one document).
const MAX_BLOCK_RESULTS_PER_DOCUMENT: usize = 3;
/// Semantic floor in hybrid mode: keeps low-similarity noise from padding
/// thin result sets. Model-relative (BGE and MiniLM have different score
/// climates), so it is overridable via the `minScore` argument.
const HYBRID_SEMANTIC_MIN_SCORE: f64 = 0.15;
/// Fusion depth: both lists are fetched at least this deep so RRF has real
/// rankings to fuse even for small `limit` values.
const HYBRID_FETCH_FLOOR: usize = 20;

pub(super) fn mcp_local_search_documents(
    app: AppHandle,
    arguments: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let graph_id = mcp_required_graph_id(arguments)?;
    let graph_dir = existing_graph_dir(&app, &graph_id)?;
    let queries = mcp_query_terms(arguments)?;
    let mode = mcp_arg_string(arguments, &["mode"]).unwrap_or_else(|| "auto".to_string());
    let limit = mcp_arg_usize(arguments, &["limit"], 20).clamp(1, 100);
    let documents = read_graph_documents_cold(&graph_dir)?;
    let mut scored = Vec::new();

    for document in &documents {
        let mut best: Option<(f64, &String)> = None;
        for query in &queries {
            let score =
                document_match_score(&document.title, &document.document_id, query, &mode)?;
            if let Some(score) = score {
                if best.map(|(existing, _)| score > existing).unwrap_or(true) {
                    best = Some((score, query));
                }
            }
        }
        if let Some((score, query)) = best {
            scored.push((score, query.clone(), document));
        }
    }
    // Stable sort: equal scores keep recency order (documents are sorted
    // most-recently-updated first by read_graph_documents_cold).
    scored.sort_by(|left, right| right.0.total_cmp(&left.0));

    let results = scored
        .iter()
        .take(limit)
        .map(|(score, query, document)| {
            serde_json::json!({
                "document_id": document.document_id,
                "documentId": document.document_id,
                "title": document.title,
                "updated_at": document.updated_at,
                "updatedAt": document.updated_at,
                "block_count": document.blocks.len(),
                "blockCount": document.blocks.len(),
                "rdfTripleCount": document.rdf_triple_count,
                "score": score,
                "match": query,
                "type": "document",
            })
        })
        .collect::<Vec<_>>();

    Ok(serde_json::json!({
        "graph_id": graph_id,
        "query": queries.first().cloned().unwrap_or_default(),
        "queries": queries,
        "mode": mode,
        "results": results,
        "total": results.len(),
    }))
}

/// Title/ID match ladder: exact > title prefix > title substring > id
/// substring > fuzzy token overlap. Returns None for no match. The fuzzy
/// tier tokenizes on whitespace AND hyphens/underscores with collapsed
/// forms kept ("Anti-Oedipus" matches "antioedipus"), ported from the
/// cloud-1 MCP search_documents ladder.
fn document_match_score(
    title: &str,
    document_id: &str,
    query: &str,
    mode: &str,
) -> Result<Option<f64>, String> {
    let title_lower = title.to_lowercase();
    let id_lower = document_id.to_lowercase();
    let query_lower = query.to_lowercase();

    let exact = title_lower == query_lower || id_lower == query_lower;
    match mode {
        "exact" => return Ok(exact.then_some(1.0)),
        "substring" | "auto" => {}
        other => return Err(format!("unsupported search_documents mode: {other}")),
    }
    if exact {
        return Ok(Some(1.0));
    }
    if title_lower.starts_with(&query_lower) {
        return Ok(Some(0.9));
    }
    if title_lower.contains(&query_lower) {
        return Ok(Some(0.75));
    }
    if id_lower.contains(&query_lower) {
        return Ok(Some(0.7));
    }
    if mode == "auto" {
        let overlap = fuzzy_token_overlap(
            &fuzzy_tokens(&query_lower),
            &fuzzy_tokens(&title_lower),
        );
        if overlap >= 0.5 {
            return Ok(Some(0.6 * overlap));
        }
    }
    Ok(None)
}

fn fuzzy_tokens(text: &str) -> Vec<String> {
    let mut tokens: Vec<String> = Vec::new();
    for word in text.split_whitespace() {
        tokens.push(word.to_string());
        let parts: Vec<&str> = word
            .split(['-', '_'])
            .filter(|part| !part.is_empty())
            .collect();
        if parts.len() > 1 {
            tokens.extend(parts.iter().map(|part| part.to_string()));
            tokens.push(parts.concat());
        }
    }
    tokens.sort();
    tokens.dedup();
    tokens
}

/// Exact token matches count 1.0, partial (one contains the other) 0.5,
/// normalized by QUERY coverage — this tier only fires after the substring
/// tiers missed, so a short query fully covered by a long title is a strong
/// signal, not a weak one.
fn fuzzy_token_overlap(query_tokens: &[String], text_tokens: &[String]) -> f64 {
    if query_tokens.is_empty() || text_tokens.is_empty() {
        return 0.0;
    }
    let mut score = 0.0f64;
    for query_token in query_tokens {
        if text_tokens.contains(query_token) {
            score += 1.0;
        } else if text_tokens.iter().any(|text_token| {
            text_token.contains(query_token.as_str()) || query_token.contains(text_token.as_str())
        }) {
            score += 0.5;
        }
    }
    score / query_tokens.len() as f64
}

#[derive(Debug)]
struct FusedHit {
    document_id: String,
    document_title: String,
    block_id: String,
    block_type: String,
    content: String,
    order: f64,
    query: String,
    fused_score: f64,
    lexical_score: Option<f64>,
    semantic_score: Option<f64>,
}

impl FusedHit {
    fn match_source(&self) -> &'static str {
        match (self.lexical_score.is_some(), self.semantic_score.is_some()) {
            (true, true) => "both",
            (true, false) => "lexical",
            (false, true) => "semantic",
            (false, false) => "semantic",
        }
    }
}

/// Collapses per-query semantic lists into ONE ranking (dedup by block,
/// best similarity wins, sorted by score). Batch queries are alternatives
/// on the lexical side (max-over-queries, one list) — this makes the
/// semantic side match. Without it, a block present in N per-query lists
/// collects N RRF contributions and structurally outvotes every lexical
/// hit N-fold (angel finding, 2026-08-28).
fn merged_semantic_ranking(
    semantic_lists: Vec<(String, Vec<SemanticSearchHit>)>,
) -> Vec<(String, SemanticSearchHit)> {
    let mut best: HashMap<(String, String), (String, SemanticSearchHit)> = HashMap::new();
    for (query, hits) in semantic_lists {
        for hit in hits {
            let key = (hit.document_id.clone(), hit.block_id.clone());
            let replace = best
                .get(&key)
                .map(|(_, existing)| hit.score > existing.score)
                .unwrap_or(true);
            if replace {
                best.insert(key, (query.clone(), hit));
            }
        }
    }
    let mut merged: Vec<(String, SemanticSearchHit)> = best.into_values().collect();
    merged.sort_by(|left, right| right.1.score.total_cmp(&left.1.score));
    merged
}

/// Reciprocal Rank Fusion over one lexical ranking and one semantic
/// ranking (each already collapsed across batch queries). Each list
/// contributes `1/(RRF_K + rank)` to a block's fused score; a block found
/// by both lists sums both contributions.
fn rrf_fuse(
    lexical: &[LexicalBlockHit],
    semantic: &[(String, SemanticSearchHit)],
) -> Vec<FusedHit> {
    let mut fused: HashMap<(String, String), FusedHit> = HashMap::new();

    for (rank, hit) in lexical.iter().enumerate() {
        let contribution = 1.0 / (RRF_K + rank as f64 + 1.0);
        let key = (hit.document_id.clone(), hit.block_id.clone());
        let entry = fused.entry(key).or_insert_with(|| FusedHit {
            document_id: hit.document_id.clone(),
            document_title: hit.document_title.clone(),
            block_id: hit.block_id.clone(),
            block_type: hit.block_type.clone(),
            content: hit.content.clone(),
            order: hit.order,
            query: hit.query.clone(),
            fused_score: 0.0,
            lexical_score: None,
            semantic_score: None,
        });
        entry.fused_score += contribution;
        entry.lexical_score = Some(hit.score);
    }

    for (rank, (query, hit)) in semantic.iter().enumerate() {
        let contribution = 1.0 / (RRF_K + rank as f64 + 1.0);
        let key = (hit.document_id.clone(), hit.block_id.clone());
        let entry = fused.entry(key).or_insert_with(|| FusedHit {
            document_id: hit.document_id.clone(),
            document_title: hit.document_title.clone(),
            block_id: hit.block_id.clone(),
            block_type: hit.block_type.clone(),
            content: hit.content.clone(),
            order: hit.order,
            query: query.clone(),
            fused_score: 0.0,
            lexical_score: None,
            semantic_score: None,
        });
        entry.fused_score += contribution;
        entry.semantic_score = Some(hit.score as f64);
    }

    let mut hits: Vec<FusedHit> = fused.into_values().collect();
    hits.sort_by(|left, right| {
        right
            .fused_score
            .total_cmp(&left.fused_score)
            .then_with(|| left.document_id.cmp(&right.document_id))
            .then_with(|| left.block_id.cmp(&right.block_id))
    });
    hits
}

fn cap_hits_per_document<T>(
    hits: Vec<T>,
    document_id: impl Fn(&T) -> &str,
    per_document_limit: Option<usize>,
    total_limit: usize,
) -> Vec<T> {
    let mut per_document: HashMap<String, usize> = HashMap::new();
    let mut capped = Vec::new();
    for hit in hits {
        if capped.len() >= total_limit {
            break;
        }
        if let Some(per_document_limit) = per_document_limit {
            let count = per_document.entry(document_id(&hit).to_string()).or_insert(0);
            if *count >= per_document_limit {
                continue;
            }
            *count += 1;
        }
        capped.push(hit);
    }
    capped
}

fn lexical_hit_json(hit: &LexicalBlockHit, score: f64, match_source: &str) -> serde_json::Value {
    serde_json::json!({
        "document_id": hit.document_id,
        "documentId": hit.document_id,
        "document_title": hit.document_title,
        "documentTitle": hit.document_title,
        "block_id": hit.block_id,
        "blockId": hit.block_id,
        "block_type": hit.block_type,
        "blockType": hit.block_type,
        "content": hit.content,
        "order": hit.order,
        "score": score,
        "lexical_score": hit.score,
        "lexicalScore": hit.score,
        "match_source": match_source,
        "matchSource": match_source,
        "query": hit.query,
    })
}

fn fused_hit_json(hit: &FusedHit) -> serde_json::Value {
    let mut value = serde_json::json!({
        "document_id": hit.document_id,
        "documentId": hit.document_id,
        "document_title": hit.document_title,
        "documentTitle": hit.document_title,
        "block_id": hit.block_id,
        "blockId": hit.block_id,
        "block_type": hit.block_type,
        "blockType": hit.block_type,
        "content": hit.content,
        "order": hit.order,
        "score": hit.fused_score,
        "match_source": hit.match_source(),
        "matchSource": hit.match_source(),
        "query": hit.query,
    });
    let object = value.as_object_mut().expect("fused hit json object");
    if let Some(lexical_score) = hit.lexical_score {
        object.insert("lexical_score".to_string(), serde_json::json!(lexical_score));
        object.insert("lexicalScore".to_string(), serde_json::json!(lexical_score));
    }
    if let Some(semantic_score) = hit.semantic_score {
        object.insert("semantic_score".to_string(), serde_json::json!(semantic_score));
        object.insert("semanticScore".to_string(), serde_json::json!(semantic_score));
    }
    value
}

pub(super) fn mcp_local_search_blocks(
    app: AppHandle,
    arguments: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let graph_id = mcp_required_graph_id(arguments)?;
    let graph_dir = existing_graph_dir(&app, &graph_id)?;
    let queries = mcp_query_terms(arguments)?;
    let mode = mcp_arg_string(arguments, &["mode"]).unwrap_or_else(|| "hybrid".to_string());
    let limit = mcp_arg_usize(arguments, &["limit"], 30).clamp(1, 100);
    let doc_filter = mcp_arg_string(arguments, &["docFilter", "doc_filter"]);
    // Case-INSENSITIVE by default: search is a recall surface, and no
    // caller (agents included — the tool schema never mentioned the flag)
    // was passing this consciously. "Settlement" should find "settlement".
    // Pass caseSensitive=true explicitly for exact-case lookups.
    let case_sensitive = mcp_arg_bool(arguments, &["caseSensitive", "case_sensitive"], false);
    let min_score_arg = arguments
        .get("minScore")
        .or_else(|| arguments.get("min_score"))
        .and_then(|value| json_number(Some(value)));
    let per_document_cap = doc_filter
        .is_none()
        .then_some(MAX_BLOCK_RESULTS_PER_DOCUMENT);
    let mut semantic_error = None;

    let lexical_hits = if mode == "lexical" || mode == "hybrid" {
        let documents = read_graph_documents_cold(&graph_dir)?;
        ranked_lexical_block_hits(&documents, &queries, doc_filter.as_deref(), case_sensitive)
    } else {
        Vec::new()
    };

    let semantic_lists: Vec<(String, Vec<SemanticSearchHit>)> = if mode == "semantic"
        || mode == "hybrid"
    {
        let fetch_limit = if mode == "hybrid" {
            limit.max(HYBRID_FETCH_FLOOR)
        } else {
            limit
        };
        let min_score = min_score_arg.unwrap_or(if mode == "hybrid" {
            HYBRID_SEMANTIC_MIN_SCORE
        } else {
            0.0
        });
        let mut lists = Vec::new();
        for query in &queries {
            match semantic_search(
                app.clone(),
                SemanticSearchInput {
                    graph_id: graph_id.clone(),
                    query: query.clone(),
                    limit: Some(fetch_limit),
                },
            ) {
                Ok(search_result) => {
                    let hits = search_result
                        .hits
                        .into_iter()
                        .filter(|hit| hit.score as f64 >= min_score)
                        .filter(|hit| {
                            doc_filter
                                .as_deref()
                                .map(|document_id| hit.document_id == document_id)
                                .unwrap_or(true)
                        })
                        .collect::<Vec<_>>();
                    lists.push((query.clone(), hits));
                }
                Err(error) if mode == "semantic" => return Err(error),
                Err(error) => semantic_error = Some(error),
            }
        }
        lists
    } else {
        Vec::new()
    };

    let results: Vec<serde_json::Value> = match mode.as_str() {
        "lexical" => cap_hits_per_document(
            lexical_hits,
            |hit| hit.document_id.as_str(),
            per_document_cap,
            limit,
        )
        .iter()
        .map(|hit| lexical_hit_json(hit, hit.score, "lexical"))
        .collect(),
        "semantic" => {
            // Dedup across batch queries keeping the best similarity; no
            // per-doc cap — pure semantic ranking is left untouched.
            merged_semantic_ranking(semantic_lists)
                .into_iter()
                .take(limit)
                .map(|(query, hit)| {
                    serde_json::json!({
                        "document_id": hit.document_id,
                        "documentId": hit.document_id,
                        "document_title": hit.document_title,
                        "documentTitle": hit.document_title,
                        "block_id": hit.block_id,
                        "blockId": hit.block_id,
                        "block_type": hit.block_type,
                        "blockType": hit.block_type,
                        "content": hit.content,
                        "order": hit.order,
                        "score": hit.score,
                        "semantic_score": hit.score,
                        "semanticScore": hit.score,
                        "match_source": "semantic",
                        "matchSource": "semantic",
                        "query": query,
                    })
                })
                .collect()
        }
        "hybrid" => cap_hits_per_document(
            rrf_fuse(&lexical_hits, &merged_semantic_ranking(semantic_lists)),
            |hit| hit.document_id.as_str(),
            per_document_cap,
            limit,
        )
        .iter()
        .map(fused_hit_json)
        .collect(),
        other => return Err(format!("unsupported search_blocks mode: {other}")),
    };

    Ok(serde_json::json!({
        "graph_id": graph_id,
        "query": queries.first().cloned().unwrap_or_default(),
        "queries": queries,
        "mode": mode,
        "doc_filter": doc_filter,
        "results": results,
        "total": results.len(),
        "semantic_error": semantic_error,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crdt_queue::{enqueue_crdt_operation, EnqueueCrdtOperationInput};
    use crate::graph_service::{create_graph_service, CreateGraphInput};
    use serde_json::json;

    fn lexical_hit(document_id: &str, block_id: &str, score: f64) -> LexicalBlockHit {
        LexicalBlockHit {
            document_id: document_id.to_string(),
            document_title: format!("Title {document_id}"),
            block_id: block_id.to_string(),
            block_type: "paragraph".to_string(),
            content: format!("content {block_id}"),
            order: 0.0,
            score,
            query: "q".to_string(),
        }
    }

    fn semantic_hit(document_id: &str, block_id: &str, score: f32) -> SemanticSearchHit {
        SemanticSearchHit {
            document_id: document_id.to_string(),
            document_title: format!("Title {document_id}"),
            block_id: block_id.to_string(),
            block_type: "paragraph".to_string(),
            content: format!("content {block_id}"),
            score,
            order: 0.0,
        }
    }

    #[test]
    fn rrf_ranks_dual_source_hits_above_single_source() {
        let lexical = vec![
            lexical_hit("doc-a", "block-shared", 4.0),
            lexical_hit("doc-b", "block-lex-only", 9.0),
        ];
        let semantic = vec![
            ("q".to_string(), semantic_hit("doc-c", "block-sem-only", 0.92)),
            ("q".to_string(), semantic_hit("doc-a", "block-shared", 0.66)),
        ];

        let fused = rrf_fuse(&lexical, &semantic);

        assert_eq!(fused[0].block_id, "block-shared");
        assert_eq!(fused[0].match_source(), "both");
        assert_eq!(fused[0].lexical_score, Some(4.0));
        assert!((fused[0].semantic_score.unwrap() - 0.66).abs() < 1e-6);
        // Both-hit sums rank-1 and rank-2 contributions; every single-source
        // hit has at most a rank-1 contribution.
        assert!(fused[0].fused_score > fused[1].fused_score);
        assert_eq!(fused.len(), 3);
    }

    /// Regression for the angel-found batch asymmetry: a block ranked top
    /// in EVERY per-query semantic list must collect exactly ONE rank-1
    /// contribution after merging — the same weight a top lexical hit gets
    /// — not one contribution per query.
    #[test]
    fn batch_semantic_lists_merge_to_a_single_ranking_before_fusion() {
        let lists = vec![
            (
                "q1".to_string(),
                vec![
                    semantic_hit("doc-a", "block-star", 0.91),
                    semantic_hit("doc-b", "block-other", 0.55),
                ],
            ),
            (
                "q2".to_string(),
                vec![
                    semantic_hit("doc-a", "block-star", 0.88),
                    semantic_hit("doc-c", "block-third", 0.61),
                ],
            ),
        ];

        let merged = merged_semantic_ranking(lists);
        assert_eq!(merged.len(), 3, "dedup across lists");
        assert_eq!(merged[0].1.block_id, "block-star");
        assert_eq!(merged[0].0, "q1", "winning query is the best-scoring one");

        let fused = rrf_fuse(&[], &merged);
        let star = fused
            .iter()
            .find(|hit| hit.block_id == "block-star")
            .expect("star hit");
        let single_rank_one = 1.0 / (RRF_K + 1.0);
        assert!(
            (star.fused_score - single_rank_one).abs() < 1e-9,
            "one contribution, not one per query: {}",
            star.fused_score
        );
    }

    #[test]
    fn per_document_cap_applies_only_without_doc_filter() {
        let hits = vec![
            lexical_hit("doc-a", "b1", 5.0),
            lexical_hit("doc-a", "b2", 4.0),
            lexical_hit("doc-a", "b3", 3.0),
            lexical_hit("doc-a", "b4", 2.0),
            lexical_hit("doc-b", "b5", 1.0),
        ];

        let capped = cap_hits_per_document(
            hits.clone(),
            |hit| hit.document_id.as_str(),
            Some(MAX_BLOCK_RESULTS_PER_DOCUMENT),
            10,
        );
        assert_eq!(capped.len(), 4);
        assert!(capped.iter().filter(|hit| hit.document_id == "doc-a").count() == 3);

        let uncapped = cap_hits_per_document(hits, |hit| hit.document_id.as_str(), None, 10);
        assert_eq!(uncapped.len(), 5);
    }

    #[test]
    fn document_match_ladder_orders_exact_prefix_substring_fuzzy() {
        let exact = document_match_score("Vibe Restoration", "doc-1", "vibe restoration", "auto")
            .unwrap()
            .unwrap();
        let prefix = document_match_score("Vibe Restoration", "doc-1", "vibe", "auto")
            .unwrap()
            .unwrap();
        let substring = document_match_score("Vibe Restoration", "doc-1", "restor", "auto")
            .unwrap()
            .unwrap();
        let fuzzy = document_match_score("Anti-Oedipus Notes", "doc-2", "antioedipus", "auto")
            .unwrap()
            .unwrap();
        assert!(exact > prefix && prefix > substring && substring > fuzzy);

        assert!(
            document_match_score("Anti-Oedipus Notes", "doc-2", "antioedipus", "substring")
                .unwrap()
                .is_none(),
            "fuzzy tier is auto-mode only"
        );
        assert!(document_match_score("Some Title", "doc-3", "unrelated", "auto")
            .unwrap()
            .is_none());
        assert!(document_match_score("Some Title", "doc-3", "query", "bogus-mode").is_err());
    }

    /// Regression for the all-documents hydration walks reachable from
    /// ordinary MCP traffic: `search_documents` and `get_workspace` now list
    /// documents COLD. The observable is the hydrating read's own side
    /// effect — for a legacy record whose manifest carries the inline Y.Doc
    /// update but whose sidecar file is missing, `read_document_record`
    /// BACKFILLS (writes) the sidecar as part of a mere read. A cold listing
    /// must find the document and leave the filesystem untouched. Real
    /// engine, real `document.write` records, real MCP handlers — no mocks.
    #[test]
    fn search_and_get_workspace_list_documents_cold_without_sidecar_backfill() {
        let _serial = crate::tauri_runtime::profile_env_serial()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let profile = std::env::temp_dir().join(format!("garden-search-cold-{nanos}"));
        std::env::set_var("GARDEN_PROFILE_DIR", &profile);

        let result = std::panic::catch_unwind(|| {
            let app = crate::tauri_runtime::build_mock_app_for_tests(true);
            let graph_id = "search-cold-walks";
            create_graph_service(
                &app,
                CreateGraphInput {
                    title: "Search Cold Walks".to_string(),
                    graph_id: Some(graph_id.to_string()),
                    description: None,
                    operation_id: None,
                },
            )
            .expect("create graph");
            let graph_dir = existing_graph_dir(&app, graph_id).expect("graph dir");

            for (doc_id, title) in [
                ("search-doc-plain", "Plain Document"),
                ("search-doc-legacy", "Legacy Needle"),
            ] {
                crate::app_runtime::async_runtime::block_on(enqueue_crdt_operation(
                    app.clone(),
                    EnqueueCrdtOperationInput {
                        kind: "document.write".to_string(),
                        graph_id: graph_id.to_string(),
                        document_id: Some(doc_id.to_string()),
                        payload: json!({
                            "documentId": doc_id,
                            "content": format!("Body of {title} with enough real text."),
                            "format": "markdown",
                            "title": title,
                        }),
                    },
                ))
                .expect("document.write drains through the real engine");
            }

            // The legacy shape: inline update kept in document.json, sidecar
            // file removed. The OLD hydrating listing would re-create this
            // file as a side effect of searching.
            let legacy_sidecar =
                crate::ydoc_paths::document_ydoc_state_path(&graph_dir, "search-doc-legacy");
            assert!(legacy_sidecar.is_file());
            std::fs::remove_file(&legacy_sidecar).expect("simulate pre-sidecar legacy record");

            let hits = mcp_local_search_documents(
                app.clone(),
                &json!({ "graphId": graph_id, "query": "needle" }),
            )
            .expect("search documents");
            assert_eq!(hits["total"], 1, "{hits}");
            assert_eq!(hits["results"][0]["document_id"], "search-doc-legacy");
            assert!(
                !legacy_sidecar.is_file(),
                "a cold search must not backfill the Y.Doc sidecar as a side effect"
            );

            let workspace = crate::app_runtime::async_runtime::block_on(
                crate::workspace_projection_service::mcp_local_get_workspace(
                    app.clone(),
                    &json!({ "graphId": graph_id }),
                ),
            )
            .expect("get workspace");
            assert!(
                workspace["counts"]["documents"].as_u64().unwrap_or(0) >= 2,
                "{workspace}"
            );
            assert!(
                !legacy_sidecar.is_file(),
                "a cold workspace projection must not backfill the Y.Doc sidecar either"
            );
        });

        std::env::remove_var("GARDEN_PROFILE_DIR");
        let _ = std::fs::remove_dir_all(&profile);
        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
    }
}
