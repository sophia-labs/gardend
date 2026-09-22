use crate::app_runtime::AppHandle;
use crate::{
    paths::existing_graph_dir,
    rdf::{
        format_rdf_triple, graph_subject, push_boolean_triple, push_string_triple,
        push_typed_literal_triple, push_uri_triple,
    },
    rdf_authority::lme_labeled_memory_projection_graph_iri,
    runtime_config::{RDF_TYPE, XSD_NS},
    semantic_embedder::embed_texts,
    semantic_index::{
        dot_product, normalize_vector, read_semantic_index, SemanticBlockEmbedding,
        SemanticIndexFile,
    },
    semantic_scaffold::{read_semantic_scaffold, SemanticNeighborEdge, SemanticScaffoldFile},
};
use oxigraph::{
    io::{RdfFormat, RdfParser},
    model::Term as OxTerm,
    sparql::{QueryResults, SparqlEvaluator},
    store::Store,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

const LME_NS: &str = "http://mnemosyne.dev/longmemeval#";
const PROV_NS: &str = "http://www.w3.org/ns/prov#";
#[allow(dead_code)]
const DEFAULT_RECALL_K: usize = 10;
const MAX_RECALL_K: usize = 50;

#[allow(dead_code)]
#[derive(Debug, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LmeRecallBenchmarkInput {
    #[serde(default, alias = "graph_id")]
    pub(crate) graph_id: String,
    pub(crate) k: Option<usize>,
    #[serde(default)]
    pub(crate) capture: bool,
    #[serde(default, alias = "run_id")]
    pub(crate) run_id: Option<String>,
}

#[derive(Debug, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LmeRecallBenchmarkResult {
    pub(crate) graph_id: String,
    pub(crate) run_id: String,
    pub(crate) generated_at: String,
    pub(crate) k: usize,
    pub(crate) provider_id: String,
    pub(crate) model_id: String,
    pub(crate) dimensions: usize,
    pub(crate) indexed_at: String,
    pub(crate) case_count: usize,
    pub(crate) baseline: LmeRecallStrategyResult,
    pub(crate) scaffold: LmeRecallStrategyResult,
    pub(crate) captured_check_count: usize,
}

#[derive(Debug, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LmeRecallStrategyResult {
    pub(crate) strategy: String,
    pub(crate) recall_at_k: f32,
    pub(crate) passed_case_count: usize,
    pub(crate) case_count: usize,
    pub(crate) cases: Vec<LmeRecallCaseResult>,
}

#[derive(Debug, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LmeRecallCaseResult {
    pub(crate) case_subject_iri: String,
    pub(crate) case_id: String,
    pub(crate) question_text: String,
    pub(crate) expected_session_ids: Vec<String>,
    pub(crate) observed_session_ids: Vec<String>,
    pub(crate) matched_session_ids: Vec<String>,
    pub(crate) recall_at_k: f32,
    pub(crate) passed: bool,
    pub(crate) hits: Vec<LmeRecallHit>,
}

#[derive(Debug, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LmeRecallHit {
    pub(crate) iri: String,
    pub(crate) document_id: String,
    pub(crate) block_id: String,
    pub(crate) score: f32,
    pub(crate) source: String,
}

#[derive(Debug, Clone)]
struct LmeQuestionCase {
    subject_iri: String,
    case_id: String,
    question_text: String,
    answer_session_ids: Vec<String>,
}

#[derive(Debug, Clone)]
struct ScoredBlock {
    iri: String,
    document_id: String,
    block_id: String,
    score: f32,
    source: &'static str,
}

#[allow(dead_code)]
pub(crate) fn score_lme_answer_session_recall(
    app: AppHandle,
    input: LmeRecallBenchmarkInput,
) -> Result<LmeRecallBenchmarkResult, String> {
    let k = input.k.unwrap_or(DEFAULT_RECALL_K).clamp(1, MAX_RECALL_K);
    let graph_dir = existing_graph_dir(&app, &input.graph_id)?;
    let index = read_semantic_index(&graph_dir)?;
    let scaffold = read_semantic_scaffold(&graph_dir)?;
    let store = crate::rdf_service::open_graph_store(&graph_dir)?;
    let cases = read_lme_question_cases(&store, &input.graph_id)?;
    if cases.is_empty() {
        return Err("no LongMemEval question cases found in projection:lme-labeled-memory".into());
    }
    let question_texts = cases
        .iter()
        .map(|case| case.question_text.clone())
        .collect::<Vec<_>>();
    let query_vectors = embed_texts(&app, &question_texts, "search_query")?;
    let mut result = score_lme_answer_session_recall_from_parts(
        &input.graph_id,
        &index,
        &scaffold,
        &cases,
        &query_vectors,
        k,
        input.run_id.as_deref(),
    )?;
    if input.capture {
        result.captured_check_count =
            capture_lme_backprojection_checks(&store, &input.graph_id, &result)?;
    }
    Ok(result)
}

fn score_lme_answer_session_recall_from_parts(
    graph_id: &str,
    index: &SemanticIndexFile,
    scaffold: &SemanticScaffoldFile,
    cases: &[LmeQuestionCase],
    query_vectors: &[Vec<f32>],
    k: usize,
    run_id: Option<&str>,
) -> Result<LmeRecallBenchmarkResult, String> {
    if index.blocks.is_empty() {
        return Err("cannot score LongMemEval recall with an empty semantic index".to_string());
    }
    if cases.len() != query_vectors.len() {
        return Err(format!(
            "LongMemEval case/vector length mismatch: {} cases, {} vectors",
            cases.len(),
            query_vectors.len()
        ));
    }
    validate_scaffold_manifest(index, scaffold)?;
    let k = k.clamp(1, MAX_RECALL_K);
    let run_id = run_id
        .map(str::to_string)
        .unwrap_or_else(|| default_lme_recall_run_id(graph_id, index, k));
    let generated_at = index.manifest.indexed_at.clone();
    let mut baseline_cases = Vec::new();
    let mut scaffold_cases = Vec::new();
    for (case, query_vector) in cases.iter().zip(query_vectors.iter()) {
        let ranked = rank_semantic_blocks(index, query_vector)?;
        baseline_cases.push(score_case(case, top_hits(&ranked, k)));
        let scaffold_ranked = rank_scaffold_neighbor_blocks(index, scaffold, &ranked, k);
        scaffold_cases.push(score_case(case, top_hits(&scaffold_ranked, k)));
    }
    let baseline = strategy_result("semantic_baseline", baseline_cases);
    let scaffold = strategy_result("semantic_scaffold", scaffold_cases);
    Ok(LmeRecallBenchmarkResult {
        graph_id: graph_id.to_string(),
        run_id,
        generated_at,
        k,
        provider_id: index.manifest.provider_id.clone(),
        model_id: index.manifest.model_id.clone(),
        dimensions: index.manifest.dimensions,
        indexed_at: index.manifest.indexed_at.clone(),
        case_count: cases.len(),
        baseline,
        scaffold,
        captured_check_count: 0,
    })
}

fn validate_scaffold_manifest(
    index: &SemanticIndexFile,
    scaffold: &SemanticScaffoldFile,
) -> Result<(), String> {
    if scaffold.manifest.dimensions != index.manifest.dimensions {
        return Err(format!(
            "semantic scaffold dimensions {} do not match index dimensions {}",
            scaffold.manifest.dimensions, index.manifest.dimensions
        ));
    }
    Ok(())
}

fn rank_semantic_blocks(
    index: &SemanticIndexFile,
    query_vector: &[f32],
) -> Result<Vec<ScoredBlock>, String> {
    if query_vector.len() != index.manifest.dimensions {
        return Err(format!(
            "query vector has {} dimensions; index requires {}",
            query_vector.len(),
            index.manifest.dimensions
        ));
    }
    let query_vector = normalize_vector(query_vector.to_vec());
    let mut scored = Vec::new();
    for block in &index.blocks {
        if block.vector.len() != index.manifest.dimensions {
            return Err(format!(
                "block {} has {} dimensions; index requires {}",
                block.iri,
                block.vector.len(),
                index.manifest.dimensions
            ));
        }
        scored.push(scored_block(
            block,
            dot_product(&block.vector, &query_vector),
            "semantic",
        ));
    }
    sort_scored_blocks(&mut scored);
    Ok(scored)
}

fn rank_scaffold_neighbor_blocks(
    index: &SemanticIndexFile,
    scaffold: &SemanticScaffoldFile,
    semantic_ranked: &[ScoredBlock],
    k: usize,
) -> Vec<ScoredBlock> {
    let block_by_iri = index
        .blocks
        .iter()
        .map(|block| (block.iri.as_str(), block))
        .collect::<BTreeMap<_, _>>();
    let mut candidates: BTreeMap<String, ScoredBlock> = BTreeMap::new();
    for seed in semantic_ranked.iter().take(k) {
        insert_best(&mut candidates, seed.clone());
        for (neighbor_iri, edge) in neighbor_edges_for(&scaffold.neighbor_edges, &seed.iri) {
            let Some(block) = block_by_iri.get(neighbor_iri.as_str()) else {
                continue;
            };
            let neighbor_score = ((seed.score + cosine_01(edge.score)) * 0.5).clamp(0.0, 1.0);
            insert_best(
                &mut candidates,
                scored_block(block, neighbor_score, "semanticNeighbor"),
            );
        }
    }
    if candidates.is_empty() {
        return semantic_ranked.to_vec();
    }
    let mut out = candidates.into_values().collect::<Vec<_>>();
    sort_scored_blocks(&mut out);
    out
}

fn neighbor_edges_for(
    edges: &[SemanticNeighborEdge],
    iri: &str,
) -> Vec<(String, SemanticNeighborEdge)> {
    let mut out = Vec::new();
    for edge in edges {
        if edge.source_iri == iri {
            out.push((edge.target_iri.clone(), edge.clone()));
        } else if edge.target_iri == iri {
            out.push((edge.source_iri.clone(), edge.clone()));
        }
    }
    out
}

fn insert_best(candidates: &mut BTreeMap<String, ScoredBlock>, candidate: ScoredBlock) {
    candidates
        .entry(candidate.iri.clone())
        .and_modify(|current| {
            if candidate.score > current.score {
                *current = candidate.clone();
            }
        })
        .or_insert(candidate);
}

fn scored_block(block: &SemanticBlockEmbedding, score: f32, source: &'static str) -> ScoredBlock {
    ScoredBlock {
        iri: block.iri.clone(),
        document_id: block.document_id.clone(),
        block_id: block.block_id.clone(),
        score,
        source,
    }
}

fn sort_scored_blocks(scored: &mut [ScoredBlock]) {
    scored.sort_by(|left, right| {
        right
            .score
            .total_cmp(&left.score)
            .then_with(|| left.iri.cmp(&right.iri))
    });
}

fn top_hits(scored: &[ScoredBlock], k: usize) -> Vec<LmeRecallHit> {
    scored
        .iter()
        .take(k)
        .map(|hit| LmeRecallHit {
            iri: hit.iri.clone(),
            document_id: hit.document_id.clone(),
            block_id: hit.block_id.clone(),
            score: hit.score,
            source: hit.source.to_string(),
        })
        .collect()
}

fn score_case(case: &LmeQuestionCase, hits: Vec<LmeRecallHit>) -> LmeRecallCaseResult {
    let expected = case
        .answer_session_ids
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>();
    let mut observed = BTreeSet::new();
    let mut observed_ordered = Vec::new();
    for hit in &hits {
        if observed.insert(hit.document_id.clone()) {
            observed_ordered.push(hit.document_id.clone());
        }
    }
    let matched = expected
        .intersection(&observed)
        .cloned()
        .collect::<Vec<_>>();
    let recall_at_k = if expected.is_empty() {
        0.0
    } else {
        matched.len() as f32 / expected.len() as f32
    };
    LmeRecallCaseResult {
        case_subject_iri: case.subject_iri.clone(),
        case_id: case.case_id.clone(),
        question_text: case.question_text.clone(),
        expected_session_ids: case.answer_session_ids.clone(),
        observed_session_ids: observed_ordered,
        matched_session_ids: matched,
        recall_at_k,
        passed: recall_at_k > 0.0,
        hits,
    }
}

fn strategy_result(strategy: &str, cases: Vec<LmeRecallCaseResult>) -> LmeRecallStrategyResult {
    let case_count = cases.len();
    let passed_case_count = cases.iter().filter(|case| case.passed).count();
    let recall_at_k = if case_count == 0 {
        0.0
    } else {
        cases.iter().map(|case| case.recall_at_k).sum::<f32>() / case_count as f32
    };
    LmeRecallStrategyResult {
        strategy: strategy.to_string(),
        recall_at_k,
        passed_case_count,
        case_count,
        cases,
    }
}

fn cosine_01(value: f32) -> f32 {
    ((value + 1.0) * 0.5).clamp(0.0, 1.0)
}

fn read_lme_question_cases(store: &Store, graph_id: &str) -> Result<Vec<LmeQuestionCase>, String> {
    let graph = lme_labeled_memory_projection_graph_iri(graph_id);
    let query = format!(
        "SELECT ?case ?caseId ?questionText ?answerSessionId WHERE {{
  GRAPH <{graph}> {{
    ?case <{RDF_TYPE}> <{LME_NS}QuestionCase> ;
          <{LME_NS}caseId> ?caseId ;
          <{LME_NS}questionText> ?questionText ;
          <{LME_NS}answerSessionId> ?answerSessionId .
  }}
}}
ORDER BY ?caseId ?answerSessionId"
    );
    let solutions = match SparqlEvaluator::new()
        .parse_query(&query)
        .map_err(|error| format!("parse LongMemEval recall case query: {error}"))?
        .on_store(store)
        .execute()
        .map_err(|error| format!("execute LongMemEval recall case query: {error}"))?
    {
        QueryResults::Solutions(solutions) => solutions,
        _ => return Err("LongMemEval recall case query expected SELECT solutions".to_string()),
    };
    let mut grouped = BTreeMap::<String, LmeQuestionCase>::new();
    for solution in solutions {
        let solution =
            solution.map_err(|error| format!("read LongMemEval recall case row: {error}"))?;
        let Some(OxTerm::NamedNode(case_node)) = solution.get("case") else {
            continue;
        };
        let Some(case_id) = literal_string(solution.get("caseId")) else {
            continue;
        };
        let Some(question_text) = literal_string(solution.get("questionText")) else {
            continue;
        };
        let Some(answer_session_id) = literal_string(solution.get("answerSessionId")) else {
            continue;
        };
        let entry = grouped
            .entry(case_node.as_str().to_string())
            .or_insert_with(|| LmeQuestionCase {
                subject_iri: case_node.as_str().to_string(),
                case_id,
                question_text,
                answer_session_ids: Vec::new(),
            });
        if !entry.answer_session_ids.contains(&answer_session_id) {
            entry.answer_session_ids.push(answer_session_id);
        }
    }
    Ok(grouped.into_values().collect())
}

fn literal_string(term: Option<&OxTerm>) -> Option<String> {
    let Some(OxTerm::Literal(literal)) = term else {
        return None;
    };
    let value = literal.value().trim();
    (!value.is_empty()).then(|| value.to_string())
}

fn capture_lme_backprojection_checks(
    store: &Store,
    graph_id: &str,
    result: &LmeRecallBenchmarkResult,
) -> Result<usize, String> {
    let graph = lme_labeled_memory_projection_graph_iri(graph_id);
    let mut triples = Vec::new();
    for strategy in [&result.baseline, &result.scaffold] {
        for case in &strategy.cases {
            let check_subject = backprojection_check_subject(graph_id, case, strategy, result.k);
            push_uri_triple(
                &mut triples,
                &case.case_subject_iri,
                &format!("{LME_NS}hasBackprojectionCheck"),
                &check_subject,
            );
            push_uri_triple(
                &mut triples,
                &check_subject,
                RDF_TYPE,
                &format!("{LME_NS}BackprojectionCheck"),
            );
            push_uri_triple(
                &mut triples,
                &check_subject,
                RDF_TYPE,
                &format!("{PROV_NS}Entity"),
            );
            push_string_triple(
                &mut triples,
                &check_subject,
                &format!("{LME_NS}caseId"),
                &case.case_id,
            );
            push_string_triple(
                &mut triples,
                &check_subject,
                &format!("{LME_NS}checkId"),
                &format!("{}:{}@{}", result.run_id, strategy.strategy, result.k),
            );
            push_string_triple(
                &mut triples,
                &check_subject,
                &format!("{LME_NS}runId"),
                &result.run_id,
            );
            for session_id in &case.expected_session_ids {
                push_string_triple(
                    &mut triples,
                    &check_subject,
                    &format!("{LME_NS}expectedAnswerSessionId"),
                    session_id,
                );
            }
            for session_id in observed_or_none(&case.observed_session_ids) {
                push_string_triple(
                    &mut triples,
                    &check_subject,
                    &format!("{LME_NS}observedSessionId"),
                    &session_id,
                );
            }
            push_boolean_triple(
                &mut triples,
                &check_subject,
                &format!("{LME_NS}passed"),
                case.passed,
            );
            push_uri_triple(
                &mut triples,
                &check_subject,
                &format!("{LME_NS}target"),
                &case.case_subject_iri,
            );
            push_typed_literal_triple(
                &mut triples,
                &check_subject,
                &format!("{PROV_NS}generatedAtTime"),
                &result.generated_at,
                &format!("{XSD_NS}dateTime"),
            );
        }
    }
    if triples.is_empty() {
        return Ok(0);
    }
    let trig = triples
        .iter()
        .map(format_rdf_triple)
        .map(|triple| format!("<{graph}> {{ {triple} }}"))
        .collect::<Vec<_>>()
        .join("\n");
    store
        .load_from_slice(RdfParser::from_format(RdfFormat::TriG), trig.as_bytes())
        .map_err(|error| format!("capture LongMemEval recall checks: {error}"))?;
    Ok(result.baseline.cases.len() + result.scaffold.cases.len())
}

fn observed_or_none(observed: &[String]) -> Vec<String> {
    if observed.is_empty() {
        vec!["__none__".to_string()]
    } else {
        observed.to_vec()
    }
}

fn backprojection_check_subject(
    graph_id: &str,
    case: &LmeRecallCaseResult,
    strategy: &LmeRecallStrategyResult,
    k: usize,
) -> String {
    format!(
        "{}:projection:lme-labeled-memory:case:{}:backprojection:{}-k{}",
        graph_subject(graph_id),
        iri_safe_token(&case.case_id),
        iri_safe_token(&strategy.strategy),
        k
    )
}

fn iri_safe_token(value: &str) -> String {
    let token = value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                ch
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches('-')
        .to_string();
    if token.is_empty() {
        "id".to_string()
    } else {
        token
    }
}

fn default_lme_recall_run_id(graph_id: &str, index: &SemanticIndexFile, k: usize) -> String {
    let mut hasher = Sha256::new();
    for part in [
        graph_id,
        &index.manifest.provider_id,
        &index.manifest.model_id,
        &index.manifest.indexed_at,
        &index.manifest.block_count.to_string(),
        &k.to_string(),
    ] {
        hasher.update(part.as_bytes());
        hasher.update([0]);
    }
    let hash = format!("{:x}", hasher.finalize());
    format!("semma-foundation-{}", &hash[..12])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        rdf::push_uri_triple,
        runtime_config::RDF_TYPE,
        semantic_index::{
            semantic_content_hash, semantic_entities_from_blocks, semantic_vector_ref,
            SemanticIndexManifest,
        },
        semantic_scaffold::{SemanticCluster, SemanticClusterMembership, SemanticScaffoldManifest},
    };

    fn block(iri: &str, document_id: &str, vector: Vec<f32>) -> SemanticBlockEmbedding {
        SemanticBlockEmbedding {
            iri: iri.to_string(),
            kind: "block".to_string(),
            graph_id: "graph-a".to_string(),
            document_id: document_id.to_string(),
            document_title: document_id.to_string(),
            block_id: iri.rsplit(':').next().unwrap_or("x").to_string(),
            block_type: "paragraph".to_string(),
            content: format!("content for {iri}"),
            content_hash: semantic_content_hash(iri),
            order: 1.0,
            vector_ref: Some(semantic_vector_ref(0, vector.len())),
            vector: normalize_vector(vector),
        }
    }

    fn fixture_index() -> SemanticIndexFile {
        let blocks = vec![
            block("urn:e:a-answer", "session-gold", vec![0.0, 1.0]),
            block("urn:e:z-seed", "session-decoy", vec![1.0, 0.0]),
        ];
        SemanticIndexFile {
            manifest: SemanticIndexManifest {
                schema_version: 1,
                graph_id: "graph-a".to_string(),
                provider_id: "fastembed".to_string(),
                model_id: "model-a".to_string(),
                dimensions: 2,
                block_count: blocks.len(),
                document_count: 2,
                indexed_at: "2026-06-25T00:00:00Z".to_string(),
                index_path: "indexes/semantic/blocks.json".to_string(),
            },
            entities: semantic_entities_from_blocks(&blocks, 2),
            blocks,
        }
    }

    fn fixture_scaffold(index: &SemanticIndexFile) -> SemanticScaffoldFile {
        SemanticScaffoldFile {
            manifest: SemanticScaffoldManifest {
                schema_version: 1,
                graph_id: "graph-a".to_string(),
                provider_id: "fastembed".to_string(),
                model_id: "model-a".to_string(),
                dimensions: 2,
                source_indexed_at: "2026-06-25T00:00:00Z".to_string(),
                source_index_path: "indexes/semantic/blocks.json".to_string(),
                scaffold_path: "indexes/semantic/scaffold.json".to_string(),
                materialized_neighbor_limit: 1,
                source_entity_count: index.entities.len(),
                entity_selection_mode: "full".to_string(),
                max_entity_cap: None,
                entity_count: index.entities.len(),
                neighbor_edge_count: 1,
                cluster_count: 1,
                membership_count: index.entities.len(),
            },
            entities: index.entities.clone(),
            neighbor_edges: vec![SemanticNeighborEdge {
                id: "edge-seed-answer".to_string(),
                source_iri: "urn:e:z-seed".to_string(),
                target_iri: "urn:e:a-answer".to_string(),
                score: 1.0,
                rank: 1,
                mutual: true,
            }],
            clusters: vec![SemanticCluster {
                id: "cluster-a".to_string(),
                member_count: index.entities.len(),
                centroid: vec![0.70710677, 0.70710677],
            }],
            memberships: index
                .entities
                .iter()
                .enumerate()
                .map(|(rank, entity)| SemanticClusterMembership {
                    id: format!("member-{rank}"),
                    cluster_id: "cluster-a".to_string(),
                    iri: entity.iri.clone(),
                    rank: rank + 1,
                })
                .collect(),
        }
    }

    fn fixture_case() -> LmeQuestionCase {
        LmeQuestionCase {
            subject_iri: format!(
                "{}:projection:lme-labeled-memory:case:case-a",
                graph_subject("graph-a")
            ),
            case_id: "case-a".to_string(),
            question_text: "Which session contains the answer?".to_string(),
            answer_session_ids: vec!["session-gold".to_string()],
        }
    }

    #[test]
    fn answer_session_recall_scores_baseline_vs_scaffold() {
        let index = fixture_index();
        let scaffold = fixture_scaffold(&index);
        let result = score_lme_answer_session_recall_from_parts(
            "graph-a",
            &index,
            &scaffold,
            &[fixture_case()],
            &[vec![1.0, 0.0]],
            1,
            Some("smoke-run"),
        )
        .expect("score recall");

        assert_eq!(result.k, 1);
        assert_eq!(result.baseline.recall_at_k, 0.0);
        assert_eq!(result.scaffold.recall_at_k, 1.0);
        assert_eq!(
            result.baseline.cases[0].observed_session_ids,
            vec!["session-decoy"]
        );
        assert_eq!(
            result.scaffold.cases[0].observed_session_ids,
            vec!["session-gold"]
        );
        assert_eq!(result.scaffold.cases[0].hits[0].source, "semanticNeighbor");
    }

    #[test]
    fn reads_lme_cases_and_captures_backprojection_checks() {
        let store = Store::new().expect("store");
        seed_lme_case(&store);
        let cases = read_lme_question_cases(&store, "graph-a").expect("read cases");
        assert_eq!(cases.len(), 1);
        assert_eq!(cases[0].answer_session_ids, vec!["session-gold"]);

        let index = fixture_index();
        let scaffold = fixture_scaffold(&index);
        let mut result = score_lme_answer_session_recall_from_parts(
            "graph-a",
            &index,
            &scaffold,
            &cases,
            &[vec![1.0, 0.0]],
            1,
            Some("smoke-run"),
        )
        .expect("score recall");
        result.captured_check_count =
            capture_lme_backprojection_checks(&store, "graph-a", &result).expect("capture");

        assert_eq!(result.captured_check_count, 2);
        assert_eq!(count_backprojection_checks(&store, "graph-a"), 2);
        assert_eq!(count_passed_checks(&store, "graph-a"), 1);
    }

    fn seed_lme_case(store: &Store) {
        let graph = lme_labeled_memory_projection_graph_iri("graph-a");
        let case = format!(
            "{}:projection:lme-labeled-memory:case:case-a",
            graph_subject("graph-a")
        );
        let mut triples = Vec::new();
        push_uri_triple(
            &mut triples,
            &case,
            RDF_TYPE,
            &format!("{LME_NS}QuestionCase"),
        );
        push_uri_triple(&mut triples, &case, RDF_TYPE, &format!("{PROV_NS}Entity"));
        push_string_triple(&mut triples, &case, &format!("{LME_NS}caseId"), "case-a");
        push_string_triple(
            &mut triples,
            &case,
            &format!("{LME_NS}questionText"),
            "Which session contains the answer?",
        );
        push_string_triple(
            &mut triples,
            &case,
            &format!("{LME_NS}answerSessionId"),
            "session-gold",
        );
        push_string_triple(&mut triples, &case, &format!("{LME_NS}runId"), "fixture");
        push_string_triple(
            &mut triples,
            &case,
            &format!("{LME_NS}questionType"),
            "single-session-recall",
        );
        push_typed_literal_triple(
            &mut triples,
            &case,
            &format!("{LME_NS}questionDate"),
            "2026-06-25T00:00:00Z",
            &format!("{XSD_NS}dateTime"),
        );
        let trig = triples
            .iter()
            .map(format_rdf_triple)
            .map(|triple| format!("<{graph}> {{ {triple} }}"))
            .collect::<Vec<_>>()
            .join("\n");
        store
            .load_from_slice(RdfParser::from_format(RdfFormat::TriG), trig.as_bytes())
            .expect("load lme trig");
    }

    fn count_backprojection_checks(store: &Store, graph_id: &str) -> usize {
        count_query(
            store,
            &format!(
                "SELECT ?s WHERE {{ GRAPH <{}> {{ ?s <{RDF_TYPE}> <{LME_NS}BackprojectionCheck> }} }}",
                lme_labeled_memory_projection_graph_iri(graph_id)
            ),
        )
    }

    fn count_passed_checks(store: &Store, graph_id: &str) -> usize {
        count_query(
            store,
            &format!(
                "SELECT ?s WHERE {{ GRAPH <{}> {{ ?s <{RDF_TYPE}> <{LME_NS}BackprojectionCheck> ; <{LME_NS}passed> true }} }}",
                lme_labeled_memory_projection_graph_iri(graph_id)
            ),
        )
    }

    fn count_query(store: &Store, query: &str) -> usize {
        match SparqlEvaluator::new()
            .parse_query(query)
            .unwrap()
            .on_store(store)
            .execute()
            .unwrap()
        {
            QueryResults::Solutions(solutions) => solutions.count(),
            _ => panic!("expected solutions"),
        }
    }
}
