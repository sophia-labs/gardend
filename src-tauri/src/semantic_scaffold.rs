use crate::{
    semantic_index::{
        dot_product, normalize_vector, semantic_entities_from_blocks, SemanticBlockEmbedding,
        SemanticEntity, SemanticIndexFile,
    },
    semantic_index_paths::{semantic_index_dir, semantic_scaffold_path},
    storage::{create_dir_all, read_json, write_json},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    path::Path,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SemanticScaffoldConfig {
    pub(crate) materialized_neighbor_limit: usize,
}

impl Default for SemanticScaffoldConfig {
    fn default() -> Self {
        Self {
            materialized_neighbor_limit: 8,
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SemanticScaffoldFile {
    pub(crate) manifest: SemanticScaffoldManifest,
    pub(crate) entities: Vec<SemanticEntity>,
    pub(crate) neighbor_edges: Vec<SemanticNeighborEdge>,
    pub(crate) clusters: Vec<SemanticCluster>,
    pub(crate) memberships: Vec<SemanticClusterMembership>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SemanticScaffoldManifest {
    pub(crate) schema_version: u32,
    pub(crate) graph_id: String,
    pub(crate) provider_id: String,
    pub(crate) model_id: String,
    pub(crate) dimensions: usize,
    pub(crate) source_indexed_at: String,
    pub(crate) source_index_path: String,
    pub(crate) scaffold_path: String,
    pub(crate) materialized_neighbor_limit: usize,
    #[serde(default)]
    pub(crate) source_entity_count: usize,
    #[serde(default)]
    pub(crate) entity_selection_mode: String,
    #[serde(default)]
    pub(crate) max_entity_cap: Option<usize>,
    pub(crate) entity_count: usize,
    pub(crate) neighbor_edge_count: usize,
    pub(crate) cluster_count: usize,
    pub(crate) membership_count: usize,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SemanticNeighborEdge {
    pub(crate) id: String,
    pub(crate) source_iri: String,
    pub(crate) target_iri: String,
    pub(crate) score: f32,
    pub(crate) rank: usize,
    pub(crate) mutual: bool,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SemanticCluster {
    pub(crate) id: String,
    pub(crate) member_count: usize,
    pub(crate) centroid: Vec<f32>,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SemanticClusterMembership {
    pub(crate) id: String,
    pub(crate) cluster_id: String,
    pub(crate) iri: String,
    pub(crate) rank: usize,
}

#[derive(Clone)]
struct IndexedEntity {
    entity: SemanticEntity,
    vector: Vec<f32>,
}

struct EntitySelection {
    indexed: Vec<IndexedEntity>,
    source_entity_count: usize,
    selection_mode: String,
    max_entity_cap: Option<usize>,
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn read_semantic_scaffold(graph_dir: &Path) -> Result<SemanticScaffoldFile, String> {
    let path = semantic_scaffold_path(graph_dir);
    if !path.is_file() {
        return Err("semantic scaffold not found; run refresh_semantic_index first".to_string());
    }
    read_json::<SemanticScaffoldFile>(&path).map_err(Into::into)
}

pub(crate) fn write_semantic_scaffold(
    graph_dir: &Path,
    scaffold: &SemanticScaffoldFile,
) -> Result<(), String> {
    let _flush_guard = crate::cell_durability::write_guard();
    create_dir_all(&semantic_index_dir(graph_dir))?;
    write_json(&semantic_scaffold_path(graph_dir), scaffold).map_err(Into::into)
}

pub(crate) fn build_semantic_scaffold(
    index: &SemanticIndexFile,
    cfg: SemanticScaffoldConfig,
    scaffold_path: String,
) -> Result<SemanticScaffoldFile, String> {
    build_semantic_scaffold_with_cap(index, cfg, scaffold_path, semantic_scaffold_max_entities())
}

fn build_semantic_scaffold_with_cap(
    index: &SemanticIndexFile,
    cfg: SemanticScaffoldConfig,
    scaffold_path: String,
    cap: Option<usize>,
) -> Result<SemanticScaffoldFile, String> {
    let selection = select_indexed_entities_with_cap(indexed_entities(index)?, cap);
    let indexed = selection.indexed;
    let neighbor_edges = build_neighbor_edges(&indexed, cfg.materialized_neighbor_limit);
    let (clusters, memberships) =
        build_clusters(&indexed, &neighbor_edges, index.manifest.dimensions);
    let entities = indexed
        .iter()
        .map(|entry| entry.entity.clone())
        .collect::<Vec<_>>();
    Ok(SemanticScaffoldFile {
        manifest: SemanticScaffoldManifest {
            schema_version: index.manifest.schema_version,
            graph_id: index.manifest.graph_id.clone(),
            provider_id: index.manifest.provider_id.clone(),
            model_id: index.manifest.model_id.clone(),
            dimensions: index.manifest.dimensions,
            source_indexed_at: index.manifest.indexed_at.clone(),
            source_index_path: index.manifest.index_path.clone(),
            scaffold_path,
            materialized_neighbor_limit: cfg.materialized_neighbor_limit,
            source_entity_count: selection.source_entity_count,
            entity_selection_mode: selection.selection_mode,
            max_entity_cap: selection.max_entity_cap,
            entity_count: entities.len(),
            neighbor_edge_count: neighbor_edges.len(),
            cluster_count: clusters.len(),
            membership_count: memberships.len(),
        },
        entities,
        neighbor_edges,
        clusters,
        memberships,
    })
}

fn select_indexed_entities_with_cap(
    indexed: Vec<IndexedEntity>,
    cap: Option<usize>,
) -> EntitySelection {
    let source_entity_count = indexed.len();
    let Some(max_entities) = cap else {
        return EntitySelection {
            indexed,
            source_entity_count,
            selection_mode: "full".to_string(),
            max_entity_cap: None,
        };
    };
    if indexed.len() <= max_entities {
        return EntitySelection {
            indexed,
            source_entity_count,
            selection_mode: "full".to_string(),
            max_entity_cap: Some(max_entities),
        };
    }

    let mut ranked = indexed
        .into_iter()
        .map(|entry| (stable_sample_key(&entry.entity.iri), entry))
        .collect::<Vec<_>>();
    ranked.sort_by(|left, right| {
        left.0
            .cmp(&right.0)
            .then_with(|| left.1.entity.iri.cmp(&right.1.entity.iri))
    });
    let mut limited = ranked
        .into_iter()
        .take(max_entities)
        .map(|(_, entry)| entry)
        .collect::<Vec<_>>();
    limited.sort_by(|left, right| left.entity.iri.cmp(&right.entity.iri));
    log::warn!(
        "semantic scaffold capped from {source_entity_count} to {} entities via GARDEN_SEMANTIC_SCAFFOLD_MAX_ENTITIES={max_entities}",
        limited.len()
    );
    EntitySelection {
        indexed: limited,
        source_entity_count,
        selection_mode: "stable-hash-sample".to_string(),
        max_entity_cap: Some(max_entities),
    }
}

pub(crate) fn semantic_scaffold_max_entities() -> Option<usize> {
    std::env::var("GARDEN_SEMANTIC_SCAFFOLD_MAX_ENTITIES")
        .ok()
        .and_then(|value| value.trim().parse::<usize>().ok())
        .filter(|value| *value > 0)
}

fn stable_sample_key(value: &str) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(value.as_bytes());
    let digest = hasher.finalize();
    let mut key = [0; 32];
    key.copy_from_slice(&digest);
    key
}

fn indexed_entities(index: &SemanticIndexFile) -> Result<Vec<IndexedEntity>, String> {
    let mut seen = BTreeSet::new();
    let mut entries = Vec::new();
    let fallback_entities = semantic_entities_from_blocks(&index.blocks, index.manifest.dimensions);
    let entities_by_iri = index
        .entities
        .iter()
        .chain(fallback_entities.iter())
        .map(|entity| (entity.iri.as_str(), entity))
        .collect::<BTreeMap<_, _>>();
    for (block_index, block) in index.blocks.iter().enumerate() {
        if block.iri.trim().is_empty() {
            continue;
        }
        if block.vector.len() != index.manifest.dimensions {
            return Err(format!(
                "semantic block {} has {} dimensions; scaffold requires {}",
                block.iri,
                block.vector.len(),
                index.manifest.dimensions
            ));
        }
        if !seen.insert(block.iri.clone()) {
            return Err(format!("duplicate semantic entity IRI {}", block.iri));
        }
        let entity = entities_by_iri
            .get(block.iri.as_str())
            .map(|entity| (**entity).clone())
            .unwrap_or_else(|| {
                semantic_entities_from_blocks(
                    &[SemanticBlockEmbedding {
                        vector_ref: block.vector_ref.clone(),
                        ..block.clone()
                    }],
                    index.manifest.dimensions,
                )
                .remove(0)
            });
        let mut entity = entity.clone();
        entity.vector_ref.index = block_index;
        entity.vector_ref.dimensions = index.manifest.dimensions;
        entries.push(IndexedEntity {
            entity,
            vector: block.vector.clone(),
        });
    }
    entries.sort_by(|left, right| left.entity.iri.cmp(&right.entity.iri));
    Ok(entries)
}

fn build_neighbor_edges(
    indexed: &[IndexedEntity],
    materialized_neighbor_limit: usize,
) -> Vec<SemanticNeighborEdge> {
    if materialized_neighbor_limit == 0 {
        return Vec::new();
    }
    let mut top_by_source = BTreeMap::<String, Vec<(String, f32, usize)>>::new();
    for source in indexed {
        let mut scored = indexed
            .iter()
            .filter(|target| target.entity.iri != source.entity.iri)
            .map(|target| {
                (
                    target.entity.iri.clone(),
                    dot_product(&source.vector, &target.vector),
                )
            })
            .collect::<Vec<_>>();
        scored.sort_by(|left, right| {
            right
                .1
                .total_cmp(&left.1)
                .then_with(|| left.0.cmp(&right.0))
        });
        top_by_source.insert(
            source.entity.iri.clone(),
            scored
                .into_iter()
                .take(materialized_neighbor_limit)
                .enumerate()
                .map(|(rank, (iri, score))| (iri, score, rank + 1))
                .collect(),
        );
    }

    let mut edges = Vec::new();
    for (source, neighbors) in &top_by_source {
        for (target, score, rank) in neighbors {
            let mutual = top_by_source
                .get(target)
                .is_some_and(|reverse| reverse.iter().any(|(iri, _, _)| iri == source));
            edges.push(SemanticNeighborEdge {
                id: stable_id("semedge", &[source, target]),
                source_iri: source.clone(),
                target_iri: target.clone(),
                score: *score,
                rank: *rank,
                mutual,
            });
        }
    }
    edges.sort_by(|left, right| {
        left.source_iri
            .cmp(&right.source_iri)
            .then_with(|| left.rank.cmp(&right.rank))
            .then_with(|| left.target_iri.cmp(&right.target_iri))
    });
    edges
}

fn build_clusters(
    indexed: &[IndexedEntity],
    edges: &[SemanticNeighborEdge],
    dimensions: usize,
) -> (Vec<SemanticCluster>, Vec<SemanticClusterMembership>) {
    let vectors = indexed
        .iter()
        .map(|entry| (entry.entity.iri.as_str(), entry.vector.as_slice()))
        .collect::<BTreeMap<_, _>>();
    let mut adjacency = indexed
        .iter()
        .map(|entry| (entry.entity.iri.clone(), BTreeSet::<String>::new()))
        .collect::<BTreeMap<_, _>>();
    for edge in edges.iter().filter(|edge| edge.mutual) {
        adjacency
            .entry(edge.source_iri.clone())
            .or_default()
            .insert(edge.target_iri.clone());
        adjacency
            .entry(edge.target_iri.clone())
            .or_default()
            .insert(edge.source_iri.clone());
    }

    let mut visited = BTreeSet::new();
    let mut clusters = Vec::new();
    let mut memberships = Vec::new();
    for start in adjacency.keys() {
        if visited.contains(start) {
            continue;
        }
        let mut queue = VecDeque::from([start.clone()]);
        let mut members = BTreeSet::new();
        visited.insert(start.clone());
        while let Some(iri) = queue.pop_front() {
            members.insert(iri.clone());
            if let Some(neighbors) = adjacency.get(&iri) {
                for neighbor in neighbors {
                    if visited.insert(neighbor.clone()) {
                        queue.push_back(neighbor.clone());
                    }
                }
            }
        }
        let member_list = members.into_iter().collect::<Vec<_>>();
        let cluster_id = stable_id(
            "semcluster",
            &member_list.iter().map(String::as_str).collect::<Vec<_>>(),
        );
        let centroid = centroid_for_members(&member_list, &vectors, dimensions);
        clusters.push(SemanticCluster {
            id: cluster_id.clone(),
            member_count: member_list.len(),
            centroid,
        });
        for (rank, iri) in member_list.iter().enumerate() {
            memberships.push(SemanticClusterMembership {
                id: stable_id("semmember", &[cluster_id.as_str(), iri.as_str()]),
                cluster_id: cluster_id.clone(),
                iri: iri.clone(),
                rank: rank + 1,
            });
        }
    }
    clusters.sort_by(|left, right| left.id.cmp(&right.id));
    memberships.sort_by(|left, right| {
        left.cluster_id
            .cmp(&right.cluster_id)
            .then_with(|| left.iri.cmp(&right.iri))
    });
    (clusters, memberships)
}

fn centroid_for_members(
    members: &[String],
    vectors: &BTreeMap<&str, &[f32]>,
    dimensions: usize,
) -> Vec<f32> {
    let mut centroid = vec![0.0f32; dimensions];
    if members.is_empty() {
        return centroid;
    }
    for member in members {
        if let Some(vector) = vectors.get(member.as_str()) {
            for (index, value) in vector.iter().enumerate().take(dimensions) {
                centroid[index] += *value;
            }
        }
    }
    let denom = members.len() as f32;
    for value in &mut centroid {
        *value /= denom;
    }
    normalize_vector(centroid)
}

fn stable_id(prefix: &str, parts: &[&str]) -> String {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update(part.as_bytes());
        hasher.update([0]);
    }
    let digest = hasher.finalize();
    format!("{prefix}-{}", hex_prefix(&digest, 16))
}

fn hex_prefix(bytes: &[u8], chars: usize) -> String {
    bytes
        .iter()
        .flat_map(|byte| [byte >> 4, byte & 0x0f])
        .take(chars)
        .map(|nibble| match nibble {
            0..=9 => (b'0' + nibble) as char,
            _ => (b'a' + (nibble - 10)) as char,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::semantic_index::{
        semantic_content_hash, semantic_vector_ref, SemanticIndexManifest,
    };

    fn block(iri: &str, vector: Vec<f32>) -> SemanticBlockEmbedding {
        SemanticBlockEmbedding {
            iri: iri.to_string(),
            kind: "block".to_string(),
            graph_id: "graph-a".to_string(),
            document_id: "doc".to_string(),
            document_title: "Doc".to_string(),
            block_id: iri.rsplit('-').next().unwrap_or("x").to_string(),
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
            block("urn:e:b", vec![0.95, 0.05]),
            block("urn:e:c", vec![0.0, 1.0]),
            block("urn:e:d", vec![0.05, 0.95]),
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

    #[test]
    fn scaffold_knn_has_no_self_edges_bounded_out_degree_and_mutual_flags() {
        let scaffold = build_semantic_scaffold(
            &fixture_index(),
            SemanticScaffoldConfig {
                materialized_neighbor_limit: 1,
            },
            "indexes/semantic/scaffold.json".to_string(),
        )
        .expect("build scaffold");

        assert_eq!(scaffold.neighbor_edges.len(), 4);
        for edge in &scaffold.neighbor_edges {
            assert_ne!(edge.source_iri, edge.target_iri);
            assert_eq!(edge.rank, 1);
        }
        for iri in ["urn:e:a", "urn:e:b", "urn:e:c", "urn:e:d"] {
            assert!(
                scaffold
                    .neighbor_edges
                    .iter()
                    .filter(|edge| edge.source_iri == iri)
                    .count()
                    <= 1
            );
        }
        assert!(scaffold
            .neighbor_edges
            .iter()
            .any(|edge| edge.source_iri == "urn:e:a"
                && edge.target_iri == "urn:e:b"
                && edge.mutual));
    }

    #[test]
    fn scaffold_clusters_are_content_addressed_and_byte_deterministic() {
        let config = SemanticScaffoldConfig {
            materialized_neighbor_limit: 1,
        };
        let left = build_semantic_scaffold(
            &fixture_index(),
            config,
            "indexes/semantic/scaffold.json".to_string(),
        )
        .expect("build left scaffold");
        let right = build_semantic_scaffold(
            &fixture_index(),
            config,
            "indexes/semantic/scaffold.json".to_string(),
        )
        .expect("build right scaffold");

        assert_eq!(left.clusters.len(), 2);
        assert_eq!(left.memberships.len(), 4);
        assert!(left
            .clusters
            .iter()
            .all(|cluster| cluster.id.starts_with("semcluster-")));
        assert_eq!(
            serde_json::to_string_pretty(&left).unwrap(),
            serde_json::to_string_pretty(&right).unwrap()
        );
    }

    #[test]
    fn scaffold_manifest_records_entity_selection_cap() {
        let selected = select_indexed_entities_with_cap(
            indexed_entities(&fixture_index()).expect("fixture index is valid"),
            Some(2),
        );

        assert_eq!(selected.source_entity_count, 4);
        assert_eq!(selected.indexed.len(), 2);
        assert_eq!(selected.selection_mode, "stable-hash-sample");
        assert_eq!(selected.max_entity_cap, Some(2));

        let scaffold = build_semantic_scaffold_with_cap(
            &fixture_index(),
            SemanticScaffoldConfig {
                materialized_neighbor_limit: 1,
            },
            "indexes/semantic/scaffold.json".to_string(),
            None,
        )
        .expect("build scaffold");
        assert_eq!(scaffold.manifest.source_entity_count, 4);
        assert_eq!(scaffold.manifest.entity_selection_mode, "full");
        assert_eq!(scaffold.manifest.max_entity_cap, None);

        let capped = build_semantic_scaffold_with_cap(
            &fixture_index(),
            SemanticScaffoldConfig {
                materialized_neighbor_limit: 1,
            },
            "indexes/semantic/scaffold.json".to_string(),
            Some(2),
        )
        .expect("build capped scaffold");
        assert_eq!(capped.manifest.source_entity_count, 4);
        assert_eq!(capped.manifest.entity_count, 2);
        assert_eq!(capped.manifest.entity_selection_mode, "stable-hash-sample");
        assert_eq!(capped.manifest.max_entity_cap, Some(2));
    }

    #[test]
    fn scaffold_manifest_counts_match_arrays_and_memberships_reference_clusters() {
        let scaffold = build_semantic_scaffold(
            &fixture_index(),
            SemanticScaffoldConfig {
                materialized_neighbor_limit: 2,
            },
            "indexes/semantic/scaffold.json".to_string(),
        )
        .expect("build scaffold");
        let cluster_ids = scaffold
            .clusters
            .iter()
            .map(|cluster| cluster.id.as_str())
            .collect::<BTreeSet<_>>();

        assert_eq!(scaffold.manifest.entity_count, scaffold.entities.len());
        assert_eq!(
            scaffold.manifest.neighbor_edge_count,
            scaffold.neighbor_edges.len()
        );
        assert_eq!(scaffold.manifest.cluster_count, scaffold.clusters.len());
        assert_eq!(
            scaffold.manifest.membership_count,
            scaffold.memberships.len()
        );
        assert!(scaffold
            .memberships
            .iter()
            .all(|membership| cluster_ids.contains(membership.cluster_id.as_str())));
    }
}
