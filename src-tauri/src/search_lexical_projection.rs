//! Lexical block search: BM25-ranked term matching over the projected block
//! store, with an exact-phrase bonus and a substring fallback tier.
//!
//! Scoring model:
//! - Blocks are the BM25 "documents": per-term tf over the block's tokens,
//!   idf over the scanned block population, standard length normalization.
//! - A multi-word query found verbatim in the content multiplies the score
//!   (phrase precision on top of bag-of-terms recall).
//! - A query with NO token matches that still appears as a raw substring
//!   (partial words, ID fragments — "settl", "block-7f") keeps a tiny flat
//!   score so nothing that matched under the old substring scan stops
//!   matching; ties rank by the caller's document order (recency).
//! - Headings get a small boost.
//!
//! Corpus statistics are computed per query over the in-memory scan — the
//! block population a search reads is already the cost of the search, and
//! this keeps ranking correct over EVERYTHING (no candidate-pool truncation:
//! cloud-1's SPARQL fetch cap ranked over an arbitrary first-N subset).

use std::collections::HashMap;

use crate::{
    document_projection_service::document_blocks_for_read, document_service::DocumentRecord,
};

const BM25_K1: f64 = 1.2;
const BM25_B: f64 = 0.75;
const PHRASE_BONUS: f64 = 1.5;
const HEADING_BOOST: f64 = 1.25;
const SUBSTRING_FALLBACK_SCORE: f64 = 0.01;

#[derive(Debug, Clone)]
pub(super) struct LexicalBlockHit {
    pub(super) document_id: String,
    pub(super) document_title: String,
    pub(super) block_id: String,
    pub(super) block_type: String,
    pub(super) content: String,
    pub(super) order: f64,
    pub(super) score: f64,
    pub(super) query: String,
}

pub(super) fn lexical_tokens(text: &str, case_sensitive: bool) -> Vec<String> {
    let normalized = if case_sensitive {
        text.to_string()
    } else {
        text.to_lowercase()
    };
    normalized
        .split(|c: char| !c.is_alphanumeric())
        .filter(|token| !token.is_empty())
        .map(str::to_string)
        .collect()
}

fn content_contains(content: &str, query: &str, case_sensitive: bool) -> bool {
    if case_sensitive {
        content.contains(query)
    } else {
        content.to_lowercase().contains(&query.to_lowercase())
    }
}

struct ScannedBlock<'a> {
    document: &'a DocumentRecord,
    block_id: String,
    block_type: String,
    content: String,
    order: f64,
    token_count: usize,
    /// tf for every distinct query term that appears in this block.
    term_frequencies: HashMap<String, u32>,
    /// Per query index: does the raw query string appear verbatim?
    contains_query: Vec<bool>,
}

/// Scores every block in `documents` against `queries` and returns hits
/// ranked best-first. A block's score is its best score over the queries
/// (batch queries are alternatives, not conjunction); `query` on the hit is
/// the winning query. Zero-score blocks are omitted.
pub(super) fn ranked_lexical_block_hits(
    documents: &[DocumentRecord],
    queries: &[String],
    doc_filter: Option<&str>,
    case_sensitive: bool,
) -> Vec<LexicalBlockHit> {
    let query_tokens: Vec<Vec<String>> = queries
        .iter()
        .map(|query| lexical_tokens(query, case_sensitive))
        .collect();
    let all_terms: Vec<&String> = {
        let mut terms: Vec<&String> = query_tokens.iter().flatten().collect();
        terms.sort();
        terms.dedup();
        terms
    };

    let mut scanned: Vec<ScannedBlock> = Vec::new();
    for document in documents {
        if let Some(doc_filter) = doc_filter {
            if document.document_id != doc_filter {
                continue;
            }
        }
        for block in document_blocks_for_read(document) {
            let tokens = lexical_tokens(&block.content, case_sensitive);
            let mut term_frequencies: HashMap<String, u32> = HashMap::new();
            for token in &tokens {
                if all_terms.iter().any(|term| *term == token) {
                    *term_frequencies.entry(token.clone()).or_insert(0) += 1;
                }
            }
            let contains_query = queries
                .iter()
                .map(|query| content_contains(&block.content, query, case_sensitive))
                .collect::<Vec<_>>();
            if term_frequencies.is_empty() && !contains_query.iter().any(|hit| *hit) {
                continue;
            }
            // Empty block ids get the same synthetic id the semantic index
            // uses (semantic_block_sources) — RRF fusion keys on
            // (document_id, block_id), so an unnormalized "" here would
            // collide distinct blocks into one entry AND never pair with
            // the semantic side's "block-{order}" for the same block.
            let block_id = if block.id.is_empty() {
                format!("block-{}", block.order)
            } else {
                block.id.clone()
            };
            scanned.push(ScannedBlock {
                document,
                block_id,
                block_type: block.block_type.clone(),
                content: block.content.clone(),
                order: block.order,
                token_count: tokens.len(),
                term_frequencies,
                contains_query,
            });
        }
    }
    if scanned.is_empty() {
        return Vec::new();
    }

    // idf over the MATCH population would overweight ubiquitous terms'
    // blocks; the honest N is every scanned block, matched or not. Count
    // total blocks and per-term document frequency in one cheap pass.
    let mut total_blocks = 0usize;
    for document in documents {
        if let Some(doc_filter) = doc_filter {
            if document.document_id != doc_filter {
                continue;
            }
        }
        total_blocks += document_blocks_for_read(document).len();
    }
    let mut document_frequency: HashMap<&str, usize> = HashMap::new();
    for block in &scanned {
        for term in block.term_frequencies.keys() {
            *document_frequency.entry(term.as_str()).or_insert(0) += 1;
        }
    }
    let average_length = {
        let matched_tokens: usize = scanned.iter().map(|block| block.token_count).sum();
        // Blocks outside `scanned` contribute unknown lengths; approximating
        // the corpus average by the scanned average keeps this a single pass
        // and only shifts length normalization, not ordering between blocks
        // of different lengths.
        (matched_tokens as f64 / scanned.len() as f64).max(1.0)
    };
    let idf = |term: &str| -> f64 {
        let df = document_frequency.get(term).copied().unwrap_or(0) as f64;
        let n = total_blocks as f64;
        (1.0 + (n - df + 0.5) / (df + 0.5)).ln()
    };

    let mut hits: Vec<LexicalBlockHit> = Vec::new();
    for block in &scanned {
        let mut best_score = 0.0f64;
        let mut best_query = queries.first().cloned().unwrap_or_default();
        for (query_index, query) in queries.iter().enumerate() {
            let terms = &query_tokens[query_index];
            let mut score = 0.0f64;
            for term in terms {
                let tf = block.term_frequencies.get(term).copied().unwrap_or(0) as f64;
                if tf == 0.0 {
                    continue;
                }
                let length_norm =
                    1.0 - BM25_B + BM25_B * (block.token_count as f64 / average_length);
                score += idf(term) * (tf * (BM25_K1 + 1.0)) / (tf + BM25_K1 * length_norm);
            }
            if score > 0.0 && terms.len() > 1 && block.contains_query[query_index] {
                score *= PHRASE_BONUS;
            }
            if score == 0.0 && block.contains_query[query_index] {
                score = SUBSTRING_FALLBACK_SCORE;
            }
            if score > best_score {
                best_score = score;
                best_query = query.clone();
            }
        }
        if best_score == 0.0 {
            continue;
        }
        if block.block_type == "heading" {
            best_score *= HEADING_BOOST;
        }
        hits.push(LexicalBlockHit {
            document_id: block.document.document_id.clone(),
            document_title: block.document.title.clone(),
            block_id: block.block_id.clone(),
            block_type: block.block_type.clone(),
            content: block.content.clone(),
            order: block.order,
            score: best_score,
            query: best_query,
        });
    }
    // Stable sort: equal scores keep the caller's document order (recency).
    hits.sort_by(|left, right| right.score.total_cmp(&left.score));
    hits
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document_service::BlockSnapshot;
    use crate::rdf::document_subject;
    use crate::runtime_config::{DOCUMENT_SCHEMA_VERSION, LOCAL_GRAPH_ORIGIN, LOCAL_PROVIDER_ID};

    fn test_document(document_id: &str, title: &str, blocks: Vec<BlockSnapshot>) -> DocumentRecord {
        DocumentRecord {
            document_id: document_id.to_string(),
            graph_id: "graph-a".to_string(),
            title: title.to_string(),
            revision: 1,
            body: blocks
                .iter()
                .map(|block| block.content.as_str())
                .collect::<Vec<_>>()
                .join("\n"),
            origin: LOCAL_GRAPH_ORIGIN.to_string(),
            provider_id: LOCAL_PROVIDER_ID.to_string(),
            local_path: format!("/tmp/{document_id}.json"),
            rdf_subject: document_subject(document_id),
            created_at: "1000".to_string(),
            updated_at: "2000".to_string(),
            capabilities: Vec::new(),
            schema_version: DOCUMENT_SCHEMA_VERSION,
            tiptap_xml: String::new(),
            tiptap_json: None,
            ydoc_update_base64: String::new(),
            ydoc_state_path: String::new(),
            tree: None,
            blocks,
            rdf_triple_count: 0,
            document_kind: None,
        }
    }

    fn test_block(id: &str, content: &str, order: f64) -> BlockSnapshot {
        BlockSnapshot {
            id: id.to_string(),
            block_type: "paragraph".to_string(),
            content: content.to_string(),
            parent_id: None,
            order,
            level: None,
            checked: None,
            language: None,
            marks: Vec::new(),
        }
    }

    #[test]
    fn lexical_hits_respect_doc_filter_and_case() {
        let documents = vec![
            test_document(
                "doc-a",
                "Doc A",
                vec![
                    test_block("block-a", "Alpha beta", 0.0),
                    test_block("block-b", "alpha gamma", 1.0),
                ],
            ),
            test_document(
                "doc-b",
                "Doc B",
                vec![test_block("block-c", "Alpha beta", 0.0)],
            ),
        ];
        let queries = vec!["alpha".to_string()];

        let hits = ranked_lexical_block_hits(&documents, &queries, Some("doc-a"), false);
        assert_eq!(hits.len(), 2);
        assert!(hits.iter().all(|hit| hit.document_id == "doc-a"));

        let case_sensitive = ranked_lexical_block_hits(&documents, &queries, Some("doc-a"), true);
        assert_eq!(case_sensitive.len(), 1);
        assert_eq!(case_sensitive[0].block_id, "block-b");
    }

    #[test]
    fn bm25_prefers_rare_terms_and_higher_term_frequency() {
        let documents = vec![test_document(
            "doc-a",
            "Doc A",
            vec![
                test_block("common-1", "settlement notes on the garden", 0.0),
                test_block("common-2", "the garden holds the settlement", 1.0),
                test_block("common-3", "a garden and another garden here", 2.0),
                test_block("rare", "the omphalos constitution and the garden", 3.0),
            ],
        )];

        // "omphalos" appears in 1/4 blocks, "garden" in 4/4: the rare-term
        // block must outrank every common-term-only block.
        let hits = ranked_lexical_block_hits(
            &documents,
            &["omphalos garden".to_string()],
            None,
            false,
        );
        assert_eq!(hits[0].block_id, "rare");

        // Same term, doubled tf, same-ish length: more occurrences win.
        let tf_hits =
            ranked_lexical_block_hits(&documents, &["garden".to_string()], None, false);
        assert_eq!(tf_hits[0].block_id, "common-3");
    }

    #[test]
    fn phrase_bonus_outranks_scattered_terms() {
        let documents = vec![test_document(
            "doc-a",
            "Doc A",
            vec![
                test_block("scattered", "quiet garden time and settlement", 0.0),
                test_block("verbatim", "the settlement quiet time arrives", 1.0),
            ],
        )];

        let hits = ranked_lexical_block_hits(
            &documents,
            &["settlement quiet time".to_string()],
            None,
            false,
        );
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].block_id, "verbatim");
        assert!(hits[0].score > hits[1].score);
    }

    #[test]
    fn substring_fallback_preserves_partial_word_recall() {
        let documents = vec![test_document(
            "doc-a",
            "Doc A",
            vec![
                test_block("whole", "the settlement service ticks", 0.0),
                test_block("unrelated", "nothing relevant here", 1.0),
            ],
        )];

        // "settl" is not a token anywhere; the old substring scan matched it
        // and the fallback tier must keep matching it — below token matches.
        let hits = ranked_lexical_block_hits(&documents, &["settl".to_string()], None, false);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].block_id, "whole");
        assert!(hits[0].score <= SUBSTRING_FALLBACK_SCORE * HEADING_BOOST);

        let token_hits =
            ranked_lexical_block_hits(&documents, &["settlement".to_string()], None, false);
        assert!(token_hits[0].score > SUBSTRING_FALLBACK_SCORE);
    }

    /// Regression for the angel-found fusion-key collision: two distinct
    /// blocks with empty ids must surface as two hits with the same
    /// synthetic ids the semantic index would assign ("block-{order}"),
    /// never merge under a shared "" key downstream.
    #[test]
    fn empty_block_ids_get_semantic_aligned_synthetic_ids() {
        let mut block_one = test_block("", "settlement quiet ticks", 3.0);
        block_one.id = String::new();
        let block_two = test_block("", "settlement noisy ticks", 7.0);
        let documents = vec![test_document("doc-a", "Doc A", vec![block_one, block_two])];

        let hits =
            ranked_lexical_block_hits(&documents, &["settlement".to_string()], None, false);
        assert_eq!(hits.len(), 2);
        let mut ids: Vec<&str> = hits.iter().map(|hit| hit.block_id.as_str()).collect();
        ids.sort();
        assert_eq!(ids, vec!["block-3", "block-7"]);
    }

    #[test]
    fn batch_queries_score_as_alternatives_and_report_winning_query() {
        let documents = vec![test_document(
            "doc-a",
            "Doc A",
            vec![test_block("block-a", "candle embeddings parity", 0.0)],
        )];

        let hits = ranked_lexical_block_hits(
            &documents,
            &["nothing".to_string(), "embeddings".to_string()],
            None,
            false,
        );
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].query, "embeddings");
    }
}
