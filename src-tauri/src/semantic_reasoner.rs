use crate::app_runtime::AppHandle;
use crate::{
    paths::existing_graph_dir,
    rdf_authority::salience_projection_graph_iri,
    runtime_config::{MNEMO_NS, WIRE_NS},
    semantic_index::{
        dot_product, normalize_vector, read_semantic_index, SemanticBlockEmbedding,
        SemanticIndexFile,
    },
    semantic_mcp_inputs::semantic_reason_input_from_mcp_args,
    semantic_relation::{
        collect_wire_relations, read_semantic_relations, SemanticRelationFile, WireRelation,
    },
    semantic_scaffold::{read_semantic_scaffold, SemanticScaffoldFile},
};
use oxigraph::{
    model::Term as OxTerm,
    sparql::{QueryResults, SparqlEvaluator},
    store::Store,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

const MAX_REASON_LIMIT: usize = 50;
const DEFAULT_REASON_LIMIT: usize = 10;
const PREDICTED_WEIGHT: f32 = 0.45;
const CLUSTER_WEIGHT: f32 = 0.20;
const STRUCTURAL_WEIGHT: f32 = 0.20;
const RELATION_SIMILAR_WEIGHT: f32 = 0.10;
const SALIENCE_WEIGHT: f32 = 0.05;

#[derive(Debug, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub(super) struct SemanticReasonInput {
    #[serde(default, alias = "graph_id")]
    pub(super) graph_id: String,
    #[serde(default, alias = "head", alias = "h", alias = "head_iri")]
    pub(super) head_iri: Option<String>,
    #[serde(
        default,
        alias = "relation",
        alias = "r",
        alias = "predicate",
        alias = "predicateIri",
        alias = "relation_iri"
    )]
    pub(super) relation_iri: String,
    #[serde(default, alias = "tail", alias = "t", alias = "tail_iri")]
    pub(super) tail_iri: Option<String>,
    pub(super) limit: Option<usize>,
}

#[derive(Debug, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub(super) struct SemanticReasonResult {
    pub(super) graph_id: String,
    pub(super) query: SemanticReasonQuery,
    pub(super) provider_id: String,
    pub(super) model_id: String,
    pub(super) dimensions: usize,
    pub(super) indexed_at: Option<String>,
    pub(super) candidate_count: usize,
    pub(super) candidates: Vec<SemanticReasonCandidate>,
}

#[derive(Debug, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub(super) struct SemanticReasonQuery {
    pub(super) head_iri: Option<String>,
    pub(super) relation_iri: String,
    pub(super) tail_iri: Option<String>,
    pub(super) direction: String,
}

#[derive(Debug, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub(super) struct SemanticReasonCandidate {
    pub(super) iri: String,
    pub(super) document_id: String,
    pub(super) block_id: String,
    pub(super) document_title: String,
    pub(super) block_type: String,
    pub(super) content: String,
    pub(super) score: f32,
    pub(super) score_parts: SemanticReasonScoreParts,
    pub(super) evidence: SemanticReasonEvidence,
}

#[derive(Debug, Serialize, Clone, Default)]
#[serde(rename_all = "camelCase")]
pub(super) struct SemanticReasonScoreParts {
    pub(super) predicted_coordinate: f32,
    pub(super) cluster_prior: f32,
    pub(super) structural_adjacent: f32,
    pub(super) relation_similar: f32,
    pub(super) salience: f32,
    pub(super) delta_variance_gate: f32,
}

#[derive(Debug, Serialize, Clone, Default)]
#[serde(rename_all = "camelCase")]
pub(super) struct SemanticReasonEvidence {
    pub(super) cluster_id: Option<String>,
    pub(super) direct_wire_count: usize,
    pub(super) similar_wire_count: usize,
    pub(super) salience_present: bool,
    pub(super) delta_observations: usize,
    pub(super) delta_variance: Option<f32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReasonDirection {
    HeadToTail,
    TailToHead,
}

impl ReasonDirection {
    fn as_str(self) -> &'static str {
        match self {
            Self::HeadToTail => "headToTail",
            Self::TailToHead => "tailToHead",
        }
    }
}

pub(super) fn mcp_local_semantic_reason(
    app: AppHandle,
    arguments: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    serde_json::to_value(semantic_reason(
        app,
        semantic_reason_input_from_mcp_args(arguments),
    )?)
    .map_err(|error| error.to_string())
}

#[cfg_attr(feature = "desktop", tauri::command)]
pub(super) fn semantic_reason(
    app: AppHandle,
    input: SemanticReasonInput,
) -> Result<SemanticReasonResult, String> {
    let graph_dir = existing_graph_dir(&app, &input.graph_id)?;
    let index = read_semantic_index(&graph_dir)?;
    let scaffold = read_semantic_scaffold(&graph_dir)?;
    let relations = read_semantic_relations(&graph_dir)?;
    let store = crate::rdf_service::open_graph_store(&graph_dir)?;
    let wires = collect_wire_relations(&store)?;
    let salience = read_salience_scores(&store, &input.graph_id)?;
    semantic_reason_from_parts(input, &index, &scaffold, &relations, &wires, &salience)
}

fn semantic_reason_from_parts(
    input: SemanticReasonInput,
    index: &SemanticIndexFile,
    scaffold: &SemanticScaffoldFile,
    relations: &SemanticRelationFile,
    wires: &[WireRelation],
    salience: &BTreeMap<String, f32>,
) -> Result<SemanticReasonResult, String> {
    validate_manifests(index, scaffold, relations)?;
    let relation_iri = normalize_relation_iri(&input.relation_iri)?;
    let (direction, anchor_iri) = reason_direction(&input)?;
    let limit = input
        .limit
        .unwrap_or(DEFAULT_REASON_LIMIT)
        .clamp(1, MAX_REASON_LIMIT);
    let blocks_by_iri = index
        .blocks
        .iter()
        .filter(|block| !block.iri.trim().is_empty())
        .map(|block| (block.iri.as_str(), block))
        .collect::<BTreeMap<_, _>>();
    let Some(anchor) = blocks_by_iri.get(anchor_iri.as_str()) else {
        return Err(format!(
            "semantic reason anchor {anchor_iri} is not indexed"
        ));
    };
    if anchor.vector.len() != index.manifest.dimensions {
        return Err(format!(
            "semantic reason anchor {} has {} dimensions; index requires {}",
            anchor.iri,
            anchor.vector.len(),
            index.manifest.dimensions
        ));
    }
    let cluster_by_iri = cluster_memberships(scaffold);
    let anchor_cluster = cluster_by_iri.get(anchor_iri.as_str()).cloned();
    let delta = relations
        .delta_operators
        .iter()
        .find(|delta| delta.predicate_iri == relation_iri);
    let predicted = predicted_coordinate(anchor, delta, direction, index.manifest.dimensions)?;
    let relation_similarity = relation_similarity_scores(&relation_iri, relations);
    let mut candidates = Vec::new();
    for block in blocks_by_iri.values() {
        if block.iri == anchor_iri {
            continue;
        }
        if block.vector.len() != index.manifest.dimensions {
            return Err(format!(
                "semantic reason candidate {} has {} dimensions; index requires {}",
                block.iri,
                block.vector.len(),
                index.manifest.dimensions
            ));
        }
        let candidate_cluster = cluster_by_iri.get(block.iri.as_str()).cloned();
        let cluster_prior =
            same_cluster_score(anchor_cluster.as_deref(), candidate_cluster.as_deref());
        let (structural_adjacent, direct_wire_count) =
            structural_score(wires, direction, &anchor_iri, &relation_iri, &block.iri);
        let (relation_similar, similar_wire_count) = relation_similar_score(
            wires,
            direction,
            &anchor_iri,
            &relation_iri,
            &block.iri,
            &relation_similarity,
        );
        let salience_value = salience.get(block.iri.as_str()).copied();
        let salience_score = salience_value.map(salience_score).unwrap_or_default();
        let predicted_score = predicted
            .as_ref()
            .map(|prediction| {
                cosine_01(dot_product(&block.vector, &prediction.vector)) * prediction.gate
            })
            .unwrap_or_default();
        let score_parts = SemanticReasonScoreParts {
            predicted_coordinate: predicted_score,
            cluster_prior,
            structural_adjacent,
            relation_similar,
            salience: salience_score,
            delta_variance_gate: predicted
                .as_ref()
                .map(|prediction| prediction.gate)
                .unwrap_or_default(),
        };
        let score = fused_score(&score_parts);
        candidates.push(SemanticReasonCandidate {
            iri: block.iri.clone(),
            document_id: block.document_id.clone(),
            block_id: block.block_id.clone(),
            document_title: block.document_title.clone(),
            block_type: block.block_type.clone(),
            content: block.content.clone(),
            score,
            score_parts,
            evidence: SemanticReasonEvidence {
                cluster_id: candidate_cluster,
                direct_wire_count,
                similar_wire_count,
                salience_present: salience_value.is_some(),
                delta_observations: delta.map(|delta| delta.obs_count).unwrap_or_default(),
                delta_variance: delta.map(|delta| delta.variance),
            },
        });
    }
    candidates.sort_by(|left, right| {
        right
            .score
            .total_cmp(&left.score)
            .then_with(|| left.iri.cmp(&right.iri))
    });
    let candidate_count = candidates.len();
    candidates.truncate(limit);
    Ok(SemanticReasonResult {
        graph_id: input.graph_id,
        query: SemanticReasonQuery {
            head_iri: input.head_iri.and_then(non_empty_string),
            relation_iri,
            tail_iri: input.tail_iri.and_then(non_empty_string),
            direction: direction.as_str().to_string(),
        },
        provider_id: index.manifest.provider_id.clone(),
        model_id: index.manifest.model_id.clone(),
        dimensions: index.manifest.dimensions,
        indexed_at: Some(index.manifest.indexed_at.clone()),
        candidate_count,
        candidates,
    })
}

fn validate_manifests(
    index: &SemanticIndexFile,
    scaffold: &SemanticScaffoldFile,
    relations: &SemanticRelationFile,
) -> Result<(), String> {
    if scaffold.manifest.dimensions != index.manifest.dimensions {
        return Err(format!(
            "semantic scaffold dimensions {} do not match index dimensions {}",
            scaffold.manifest.dimensions, index.manifest.dimensions
        ));
    }
    if relations.manifest.dimensions != index.manifest.dimensions {
        return Err(format!(
            "semantic relation dimensions {} do not match index dimensions {}",
            relations.manifest.dimensions, index.manifest.dimensions
        ));
    }
    Ok(())
}

fn reason_direction(input: &SemanticReasonInput) -> Result<(ReasonDirection, String), String> {
    let head = input.head_iri.as_deref().and_then(non_empty_str);
    let tail = input.tail_iri.as_deref().and_then(non_empty_str);
    match (head, tail) {
        (Some(head), None) => Ok((ReasonDirection::HeadToTail, head.to_string())),
        (None, Some(tail)) => Ok((ReasonDirection::TailToHead, tail.to_string())),
        (Some(_), Some(_)) => {
            Err("semantic_reason expects exactly one of headIri or tailIri".to_string())
        }
        (None, None) => Err("semantic_reason requires one of headIri or tailIri".to_string()),
    }
}

fn normalize_relation_iri(value: &str) -> Result<String, String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err("semantic_reason relationIri cannot be empty".to_string());
    }
    if trimmed.starts_with("http://")
        || trimmed.starts_with("https://")
        || trimmed.starts_with("urn:")
    {
        Ok(trimmed.to_string())
    } else {
        Ok(format!("{WIRE_NS}{trimmed}"))
    }
}

fn non_empty_string(value: String) -> Option<String> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

fn non_empty_str(value: &str) -> Option<&str> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then_some(trimmed)
}

struct PredictedCoordinate {
    vector: Vec<f32>,
    gate: f32,
}

fn predicted_coordinate(
    anchor: &SemanticBlockEmbedding,
    delta: Option<&crate::semantic_relation::SemanticDeltaOperator>,
    direction: ReasonDirection,
    dimensions: usize,
) -> Result<Option<PredictedCoordinate>, String> {
    let Some(delta) = delta else {
        return Ok(None);
    };
    if delta.vector.len() != dimensions {
        return Err(format!(
            "semantic delta {} has {} dimensions; reasoner requires {}",
            delta.predicate_iri,
            delta.vector.len(),
            dimensions
        ));
    }
    if delta.obs_count == 0 {
        return Ok(None);
    }
    let vector = anchor
        .vector
        .iter()
        .zip(delta.vector.iter())
        .map(|(anchor, delta)| match direction {
            ReasonDirection::HeadToTail => anchor + delta,
            ReasonDirection::TailToHead => anchor - delta,
        })
        .collect::<Vec<_>>();
    Ok(Some(PredictedCoordinate {
        vector: normalize_vector(vector),
        gate: delta_variance_gate(delta.variance),
    }))
}

fn delta_variance_gate(variance: f32) -> f32 {
    (1.0 / (1.0 + variance.max(0.0))).clamp(0.0, 1.0)
}

fn cluster_memberships(scaffold: &SemanticScaffoldFile) -> BTreeMap<&str, String> {
    scaffold
        .memberships
        .iter()
        .map(|membership| (membership.iri.as_str(), membership.cluster_id.clone()))
        .collect()
}

fn same_cluster_score(anchor_cluster: Option<&str>, candidate_cluster: Option<&str>) -> f32 {
    match (anchor_cluster, candidate_cluster) {
        (Some(left), Some(right)) if left == right => 1.0,
        _ => 0.0,
    }
}

fn relation_similarity_scores(
    relation_iri: &str,
    relations: &SemanticRelationFile,
) -> BTreeMap<String, f32> {
    let mut scores = BTreeMap::<String, f32>::from([(relation_iri.to_string(), 1.0)]);
    for edge in &relations.relation_neighbor_edges {
        if edge.source_relation == relation_iri {
            scores
                .entry(edge.target_relation.clone())
                .and_modify(|score| *score = (*score).max(cosine_01(edge.score)))
                .or_insert_with(|| cosine_01(edge.score));
        }
        if edge.target_relation == relation_iri {
            scores
                .entry(edge.source_relation.clone())
                .and_modify(|score| *score = (*score).max(cosine_01(edge.score)))
                .or_insert_with(|| cosine_01(edge.score));
        }
    }
    scores
}

fn structural_score(
    wires: &[WireRelation],
    direction: ReasonDirection,
    anchor_iri: &str,
    relation_iri: &str,
    candidate_iri: &str,
) -> (f32, usize) {
    let count = wires
        .iter()
        .filter(|wire| {
            wire.predicate_iri == relation_iri
                && wire_matches_candidate(wire, direction, anchor_iri, candidate_iri)
        })
        .count();
    ((count > 0) as u8 as f32, count)
}

fn relation_similar_score(
    wires: &[WireRelation],
    direction: ReasonDirection,
    anchor_iri: &str,
    relation_iri: &str,
    candidate_iri: &str,
    relation_similarity: &BTreeMap<String, f32>,
) -> (f32, usize) {
    let mut score = 0.0f32;
    let mut count = 0usize;
    for wire in wires {
        if wire.predicate_iri == relation_iri {
            continue;
        }
        let Some(similarity) = relation_similarity.get(&wire.predicate_iri) else {
            continue;
        };
        if wire_matches_candidate(wire, direction, anchor_iri, candidate_iri) {
            score = score.max(*similarity);
            count += 1;
        }
    }
    (score, count)
}

fn wire_matches_candidate(
    wire: &WireRelation,
    direction: ReasonDirection,
    anchor_iri: &str,
    candidate_iri: &str,
) -> bool {
    match direction {
        ReasonDirection::HeadToTail => {
            wire.source_iri == anchor_iri && wire.target_iri == candidate_iri
        }
        ReasonDirection::TailToHead => {
            wire.target_iri == anchor_iri && wire.source_iri == candidate_iri
        }
    }
}

fn salience_score(value: f32) -> f32 {
    (value / 5.0).clamp(0.0, 1.0)
}

fn cosine_01(value: f32) -> f32 {
    ((value + 1.0) * 0.5).clamp(0.0, 1.0)
}

fn fused_score(parts: &SemanticReasonScoreParts) -> f32 {
    parts.predicted_coordinate * PREDICTED_WEIGHT
        + parts.cluster_prior * CLUSTER_WEIGHT
        + parts.structural_adjacent * STRUCTURAL_WEIGHT
        + parts.relation_similar * RELATION_SIMILAR_WEIGHT
        + parts.salience * SALIENCE_WEIGHT
}

fn read_salience_scores(store: &Store, graph_id: &str) -> Result<BTreeMap<String, f32>, String> {
    let graph = salience_projection_graph_iri(graph_id);
    let query = format!(
        "SELECT ?block ?importance WHERE {{
  GRAPH <{graph}> {{
    ?valuation <{MNEMO_NS}targetsBlock> ?block ;
               <{MNEMO_NS}cumulativeImportance> ?importance .
  }}
}}"
    );
    let solutions = match SparqlEvaluator::new()
        .parse_query(&query)
        .map_err(|error| format!("parse semantic reason salience query: {error}"))?
        .on_store(store)
        .execute()
        .map_err(|error| format!("execute semantic reason salience query: {error}"))?
    {
        QueryResults::Solutions(solutions) => solutions,
        _ => return Err("semantic reason salience query expected SELECT solutions".to_string()),
    };
    let mut out = BTreeMap::new();
    for solution in solutions {
        let solution =
            solution.map_err(|error| format!("read semantic reason salience row: {error}"))?;
        let Some(OxTerm::NamedNode(block)) = solution.get("block") else {
            continue;
        };
        let Some(OxTerm::Literal(importance)) = solution.get("importance") else {
            continue;
        };
        if let Ok(value) = importance.value().parse::<f32>() {
            out.insert(block.as_str().to_string(), value);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        rdf::{format_rdf_triple, push_float_triple, push_uri_triple},
        runtime_config::RDF_TYPE,
        semantic_index::{
            semantic_content_hash, semantic_entities_from_blocks, semantic_vector_ref,
            SemanticIndexManifest, SemanticVectorRef,
        },
        semantic_relation::{
            RelationSemanticNeighborEdge, SemanticDeltaOperator, SemanticRelationManifest,
            SemanticRelationProfile,
        },
        semantic_scaffold::{build_semantic_scaffold, SemanticScaffoldConfig},
    };
    use oxigraph::io::{RdfFormat, RdfParser};

    fn block(iri: &str, vector: Vec<f32>) -> SemanticBlockEmbedding {
        SemanticBlockEmbedding {
            iri: iri.to_string(),
            kind: "block".to_string(),
            graph_id: "graph-a".to_string(),
            document_id: "doc".to_string(),
            document_title: "Doc".to_string(),
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
            block("urn:e:a", vec![1.0, 0.0]),
            block("urn:e:b", vec![0.0, 1.0]),
            block("urn:e:c", vec![0.8, 0.2]),
            block("urn:e:d", vec![-1.0, 0.0]),
        ];
        SemanticIndexFile {
            manifest: SemanticIndexManifest {
                schema_version: 1,
                graph_id: "graph-a".to_string(),
                provider_id: "fastembed".to_string(),
                model_id: "model-a".to_string(),
                dimensions: 2,
                block_count: blocks.len(),
                document_count: 1,
                indexed_at: "2026-06-25T00:00:00Z".to_string(),
                index_path: "indexes/semantic/blocks.json".to_string(),
            },
            entities: semantic_entities_from_blocks(&blocks, 2),
            blocks,
        }
    }

    fn fixture_scaffold(index: &SemanticIndexFile) -> SemanticScaffoldFile {
        build_semantic_scaffold(
            index,
            SemanticScaffoldConfig {
                materialized_neighbor_limit: 2,
            },
            "indexes/semantic/scaffold.json".to_string(),
        )
        .expect("build scaffold")
    }

    fn fixture_relations(include_delta: bool) -> SemanticRelationFile {
        let supports = format!("{WIRE_NS}supports");
        let requires = format!("{WIRE_NS}requires");
        SemanticRelationFile {
            manifest: SemanticRelationManifest {
                schema_version: 1,
                graph_id: "graph-a".to_string(),
                dimensions: 2,
                relation_profile_count: 2,
                interaction_edge_count: 0,
                relation_neighbor_edge_count: 1,
                delta_operator_count: usize::from(include_delta),
                relation_profiles_path: "indexes/semantic/relation-profiles.json".to_string(),
            },
            profiles: vec![
                SemanticRelationProfile {
                    id: "semrel-supports".to_string(),
                    predicate_iri: supports.clone(),
                    label: "supports".to_string(),
                    category: None,
                    verbalization: "relation supports".to_string(),
                    vector_ref: SemanticVectorRef {
                        file: "vectors/relation-supports.f32".to_string(),
                        index: 0,
                        dimensions: 2,
                    },
                    obs_count: 1,
                    variance: Some(0.0),
                    vector: vec![1.0, 0.0],
                },
                SemanticRelationProfile {
                    id: "semrel-requires".to_string(),
                    predicate_iri: requires.clone(),
                    label: "requires".to_string(),
                    category: None,
                    verbalization: "relation requires".to_string(),
                    vector_ref: SemanticVectorRef {
                        file: "vectors/relation-requires.f32".to_string(),
                        index: 0,
                        dimensions: 2,
                    },
                    obs_count: 1,
                    variance: Some(0.0),
                    vector: vec![0.0, 1.0],
                },
            ],
            interaction_edges: Vec::new(),
            relation_neighbor_edges: vec![RelationSemanticNeighborEdge {
                id: "rel-neighbor".to_string(),
                source_relation: supports.clone(),
                target_relation: requires,
                score: 0.8,
                rank: 1,
                mutual: true,
            }],
            delta_operators: include_delta
                .then(|| SemanticDeltaOperator {
                    id: "delta-supports".to_string(),
                    predicate_iri: supports,
                    vector_ref: SemanticVectorRef {
                        file: "vectors/delta-supports.f32".to_string(),
                        index: 0,
                        dimensions: 2,
                    },
                    obs_count: 2,
                    variance: 0.25,
                    vector: vec![-1.0, 1.0],
                })
                .into_iter()
                .collect(),
        }
    }

    fn fixture_wires() -> Vec<WireRelation> {
        vec![
            WireRelation {
                wire_iri: "urn:w:1".to_string(),
                source_iri: "urn:e:a".to_string(),
                predicate_iri: format!("{WIRE_NS}supports"),
                target_iri: "urn:e:b".to_string(),
            },
            WireRelation {
                wire_iri: "urn:w:2".to_string(),
                source_iri: "urn:e:a".to_string(),
                predicate_iri: format!("{WIRE_NS}requires"),
                target_iri: "urn:e:c".to_string(),
            },
        ]
    }

    #[test]
    fn semantic_reason_head_and_tail_queries_rank_candidates_with_score_parts() {
        let index = fixture_index();
        let scaffold = fixture_scaffold(&index);
        let relations = fixture_relations(true);
        let mut salience = BTreeMap::new();
        salience.insert("urn:e:b".to_string(), 5.0);
        let forward = semantic_reason_from_parts(
            SemanticReasonInput {
                graph_id: "graph-a".to_string(),
                head_iri: Some("urn:e:a".to_string()),
                relation_iri: "supports".to_string(),
                tail_iri: None,
                limit: Some(3),
            },
            &index,
            &scaffold,
            &relations,
            &fixture_wires(),
            &salience,
        )
        .expect("forward reason");

        assert_eq!(forward.query.direction, "headToTail");
        assert_eq!(forward.candidates[0].iri, "urn:e:b");
        assert!(forward.candidates[0].score_parts.predicted_coordinate > 0.0);
        assert_eq!(forward.candidates[0].score_parts.structural_adjacent, 1.0);
        assert_eq!(forward.candidates[0].score_parts.salience, 1.0);
        assert!(forward.candidates[0].evidence.salience_present);

        let reverse = semantic_reason_from_parts(
            SemanticReasonInput {
                graph_id: "graph-a".to_string(),
                head_iri: None,
                relation_iri: format!("{WIRE_NS}supports"),
                tail_iri: Some("urn:e:b".to_string()),
                limit: Some(3),
            },
            &index,
            &scaffold,
            &relations,
            &fixture_wires(),
            &BTreeMap::new(),
        )
        .expect("reverse reason");

        assert_eq!(reverse.query.direction, "tailToHead");
        assert_eq!(reverse.candidates[0].iri, "urn:e:a");
        assert_eq!(reverse.candidates[0].score_parts.structural_adjacent, 1.0);
    }

    #[test]
    fn semantic_reason_missing_delta_degrades_to_cluster_and_structural_features() {
        let index = fixture_index();
        let scaffold = fixture_scaffold(&index);
        let relations = fixture_relations(false);
        let result = semantic_reason_from_parts(
            SemanticReasonInput {
                graph_id: "graph-a".to_string(),
                head_iri: Some("urn:e:a".to_string()),
                relation_iri: "supports".to_string(),
                tail_iri: None,
                limit: Some(3),
            },
            &index,
            &scaffold,
            &relations,
            &fixture_wires(),
            &BTreeMap::new(),
        )
        .expect("reason without delta");

        assert_eq!(result.candidates[0].iri, "urn:e:b");
        assert_eq!(result.candidates[0].score_parts.predicted_coordinate, 0.0);
        assert_eq!(result.candidates[0].score_parts.delta_variance_gate, 0.0);
        assert_eq!(result.candidates[0].score_parts.structural_adjacent, 1.0);
    }

    #[test]
    fn semantic_reason_reads_salience_sparse_and_does_not_write_projection() {
        let store = Store::new().expect("store");
        let graph = salience_projection_graph_iri("graph-a");
        let mut triples = Vec::new();
        push_uri_triple(
            &mut triples,
            "urn:valuation:1",
            RDF_TYPE,
            &format!("{MNEMO_NS}BlockValuation"),
        );
        push_uri_triple(
            &mut triples,
            "urn:valuation:1",
            &format!("{MNEMO_NS}targetsBlock"),
            "urn:e:b",
        );
        push_float_triple(
            &mut triples,
            "urn:valuation:1",
            &format!("{MNEMO_NS}cumulativeImportance"),
            5.0,
        );
        let ttl = triples
            .iter()
            .map(format_rdf_triple)
            .map(|triple| format!("<{graph}> {{ {triple} }}"))
            .collect::<Vec<_>>()
            .join("\n");
        store
            .load_from_slice(RdfParser::from_format(RdfFormat::TriG), ttl.as_bytes())
            .expect("load salience trig");
        let before = count_store_triples(&store);
        let salience = read_salience_scores(&store, "graph-a").expect("read salience");
        assert_eq!(salience.get("urn:e:b"), Some(&5.0));

        let index = fixture_index();
        let scaffold = fixture_scaffold(&index);
        let relations = fixture_relations(true);
        let _ = semantic_reason_from_parts(
            SemanticReasonInput {
                graph_id: "graph-a".to_string(),
                head_iri: Some("urn:e:a".to_string()),
                relation_iri: "supports".to_string(),
                tail_iri: None,
                limit: Some(2),
            },
            &index,
            &scaffold,
            &relations,
            &fixture_wires(),
            &salience,
        )
        .expect("reason");

        assert_eq!(count_store_triples(&store), before);
    }

    fn count_store_triples(store: &Store) -> usize {
        let query = "SELECT ?s ?p ?o ?g WHERE { { ?s ?p ?o } UNION { GRAPH ?g { ?s ?p ?o } } }";
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

    #[test]
    fn semantic_reason_rejects_ambiguous_query_shape() {
        let input = SemanticReasonInput {
            graph_id: "graph-a".to_string(),
            head_iri: Some("urn:e:a".to_string()),
            relation_iri: "supports".to_string(),
            tail_iri: Some("urn:e:b".to_string()),
            limit: None,
        };
        assert!(reason_direction(&input)
            .unwrap_err()
            .contains("exactly one"));
    }
}
