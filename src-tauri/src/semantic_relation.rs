use crate::{
    runtime_config::{RDF_TYPE, WIRE_NS},
    semantic_index::{
        dot_product, normalize_vector, semantic_vector_ref, SemanticIndexFile, SemanticVectorRef,
    },
    semantic_index_paths::{
        semantic_index_dir, semantic_relation_profiles_path, semantic_vectors_dir,
    },
    semantic_scaffold::SemanticScaffoldFile,
    storage::{create_dir_all, read_bytes, read_json, write_bytes, write_json},
    verbalize::{verbalize, VerbalizeCfg},
    wire_predicates::{builtin_wire_predicates, predicate_short_name},
};
use oxigraph::{
    model::Term as OxTerm,
    sparql::{QueryResults, SparqlEvaluator},
    store::Store,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, path::Path};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SemanticRelationConfig {
    pub(crate) relation_neighbor_limit: usize,
    pub(crate) sample_label_limit: usize,
}

impl Default for SemanticRelationConfig {
    fn default() -> Self {
        Self {
            relation_neighbor_limit: 4,
            sample_label_limit: 2,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WireRelation {
    pub(crate) wire_iri: String,
    pub(crate) source_iri: String,
    pub(crate) predicate_iri: String,
    pub(crate) target_iri: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RelationProfileSpec {
    pub(crate) id: String,
    pub(crate) predicate_iri: String,
    pub(crate) label: String,
    pub(crate) category: Option<String>,
    pub(crate) verbalization: String,
    pub(crate) obs_count: usize,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SemanticRelationFile {
    pub(crate) manifest: SemanticRelationManifest,
    pub(crate) profiles: Vec<SemanticRelationProfile>,
    pub(crate) interaction_edges: Vec<RelationInteractionEdge>,
    pub(crate) relation_neighbor_edges: Vec<RelationSemanticNeighborEdge>,
    pub(crate) delta_operators: Vec<SemanticDeltaOperator>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SemanticRelationManifest {
    pub(crate) schema_version: u32,
    pub(crate) graph_id: String,
    pub(crate) dimensions: usize,
    pub(crate) relation_profile_count: usize,
    pub(crate) interaction_edge_count: usize,
    pub(crate) relation_neighbor_edge_count: usize,
    pub(crate) delta_operator_count: usize,
    pub(crate) relation_profiles_path: String,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SemanticRelationProfile {
    pub(crate) id: String,
    pub(crate) predicate_iri: String,
    pub(crate) label: String,
    pub(crate) category: Option<String>,
    pub(crate) verbalization: String,
    pub(crate) vector_ref: SemanticVectorRef,
    pub(crate) obs_count: usize,
    pub(crate) variance: Option<f32>,
    #[serde(skip)]
    pub(crate) vector: Vec<f32>,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RelationInteractionEdge {
    pub(crate) id: String,
    pub(crate) source_relation: String,
    pub(crate) target_relation: String,
    pub(crate) interaction_kind: String,
    pub(crate) count: usize,
    pub(crate) weight: f32,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RelationSemanticNeighborEdge {
    pub(crate) id: String,
    pub(crate) source_relation: String,
    pub(crate) target_relation: String,
    pub(crate) score: f32,
    pub(crate) rank: usize,
    pub(crate) mutual: bool,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SemanticDeltaOperator {
    pub(crate) id: String,
    pub(crate) predicate_iri: String,
    pub(crate) vector_ref: SemanticVectorRef,
    pub(crate) obs_count: usize,
    pub(crate) variance: f32,
    #[serde(skip)]
    pub(crate) vector: Vec<f32>,
}

#[allow(dead_code)]
pub(crate) fn read_semantic_relations(graph_dir: &Path) -> Result<SemanticRelationFile, String> {
    let path = semantic_relation_profiles_path(graph_dir);
    if !path.is_file() {
        return Err(
            "semantic relation profiles not found; run refresh_semantic_index first".to_string(),
        );
    }
    let mut relations = read_json::<SemanticRelationFile>(&path).map_err(String::from)?;
    for profile in &mut relations.profiles {
        profile.vector = read_vector_ref(graph_dir, &profile.vector_ref)?;
    }
    for delta in &mut relations.delta_operators {
        delta.vector = read_vector_ref(graph_dir, &delta.vector_ref)?;
    }
    Ok(relations)
}

pub(crate) fn write_semantic_relations(
    graph_dir: &Path,
    relations: &SemanticRelationFile,
) -> Result<(), String> {
    let _flush_guard = crate::cell_durability::write_guard();
    create_dir_all(&semantic_vectors_dir(graph_dir))?;
    for profile in &relations.profiles {
        write_vector_ref(graph_dir, &profile.vector_ref, &profile.vector)?;
    }
    for delta in &relations.delta_operators {
        write_vector_ref(graph_dir, &delta.vector_ref, &delta.vector)?;
    }
    write_json(&semantic_relation_profiles_path(graph_dir), relations).map_err(Into::into)
}

fn write_vector_ref(
    graph_dir: &Path,
    vector_ref: &SemanticVectorRef,
    vector: &[f32],
) -> Result<(), String> {
    let path = semantic_index_dir(graph_dir).join(&vector_ref.file);
    if let Some(parent) = path.parent() {
        create_dir_all(parent)?;
    }
    let mut bytes = Vec::with_capacity(vector.len() * 4);
    for value in vector {
        bytes.extend(value.to_le_bytes());
    }
    write_bytes(&path, &bytes).map_err(Into::into)
}

fn read_vector_ref(graph_dir: &Path, vector_ref: &SemanticVectorRef) -> Result<Vec<f32>, String> {
    let path = semantic_index_dir(graph_dir).join(&vector_ref.file);
    let bytes = read_bytes(&path).map_err(String::from)?;
    let byte_offset = vector_ref
        .index
        .checked_mul(vector_ref.dimensions)
        .and_then(|value| value.checked_mul(4))
        .ok_or_else(|| format!("semantic vector offset overflow for {}", vector_ref.file))?;
    let byte_len = vector_ref
        .dimensions
        .checked_mul(4)
        .ok_or_else(|| format!("semantic vector length overflow for {}", vector_ref.file))?;
    let end = byte_offset
        .checked_add(byte_len)
        .ok_or_else(|| format!("semantic vector end overflow for {}", vector_ref.file))?;
    if bytes.len() < end {
        return Err(format!(
            "semantic vector {} has {} bytes; need at least {} for index {} dimensions {}",
            vector_ref.file,
            bytes.len(),
            end,
            vector_ref.index,
            vector_ref.dimensions
        ));
    }
    let mut vector = Vec::with_capacity(vector_ref.dimensions);
    for chunk in bytes[byte_offset..end].chunks_exact(4) {
        vector.push(f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
    }
    Ok(vector)
}

pub(crate) fn collect_wire_relations(store: &Store) -> Result<Vec<WireRelation>, String> {
    let query = format!(
        "SELECT DISTINCT ?wire ?source ?predicate ?target WHERE {{
  {{
    ?wire <{RDF_TYPE}> <{WIRE_NS}Wire> ;
          <{WIRE_NS}sourceBlock> ?source ;
          <{WIRE_NS}predicate> ?predicate ;
          <{WIRE_NS}targetBlock> ?target .
  }} UNION {{
    GRAPH ?g {{
    ?wire <{RDF_TYPE}> <{WIRE_NS}Wire> ;
          <{WIRE_NS}sourceBlock> ?source ;
          <{WIRE_NS}predicate> ?predicate ;
          <{WIRE_NS}targetBlock> ?target .
    }}
  }}
}}"
    );
    let mut wires = Vec::new();
    for row in select_solutions(store, &query)? {
        wires.push(WireRelation {
            wire_iri: named_node(row.get("wire"), "wire")?,
            source_iri: named_node(row.get("source"), "source")?,
            predicate_iri: named_node(row.get("predicate"), "predicate")?,
            target_iri: named_node(row.get("target"), "target")?,
        });
    }
    wires.sort_by(|left, right| left.wire_iri.cmp(&right.wire_iri));
    Ok(wires)
}

pub(crate) fn relation_profile_specs(
    store: &Store,
    wires: &[WireRelation],
    cfg: SemanticRelationConfig,
) -> Vec<RelationProfileSpec> {
    let mut by_predicate = BTreeMap::<String, Vec<&WireRelation>>::new();
    for wire in wires {
        by_predicate
            .entry(wire.predicate_iri.clone())
            .or_default()
            .push(wire);
    }
    by_predicate
        .into_iter()
        .map(|(predicate_iri, mut predicate_wires)| {
            predicate_wires.sort_by(|left, right| left.wire_iri.cmp(&right.wire_iri));
            let short = predicate_short_name(&predicate_iri);
            let label = relation_label(&predicate_iri);
            let category = relation_category(&short).map(str::to_string);
            let verbalization = relation_profile_verbalization(
                store,
                &label,
                category.as_deref(),
                &predicate_wires,
                cfg.sample_label_limit,
            );
            RelationProfileSpec {
                id: stable_id("semrel", &[predicate_iri.as_str()]),
                predicate_iri,
                label,
                category,
                verbalization,
                obs_count: predicate_wires.len(),
            }
        })
        .collect()
}

pub(crate) fn build_semantic_relations(
    graph_id: &str,
    dimensions: usize,
    index: &SemanticIndexFile,
    _scaffold: &SemanticScaffoldFile,
    wires: &[WireRelation],
    specs: &[RelationProfileSpec],
    profile_vectors: Vec<Vec<f32>>,
    relation_profiles_path: String,
    cfg: SemanticRelationConfig,
) -> Result<SemanticRelationFile, String> {
    if profile_vectors.len() != specs.len() {
        return Err(format!(
            "relation embedder returned {} vectors for {} relation profiles",
            profile_vectors.len(),
            specs.len()
        ));
    }
    let entity_vectors = entity_vector_map(index, dimensions)?;
    let mut delta_by_predicate =
        build_delta_operators(graph_id, dimensions, wires, &entity_vectors);
    let mut profiles = Vec::new();
    for (spec, vector) in specs.iter().zip(profile_vectors) {
        if vector.len() != dimensions {
            return Err(format!(
                "relation vector for {} has {} dimensions; omphalos requires {}",
                spec.predicate_iri,
                vector.len(),
                dimensions
            ));
        }
        let normalized = normalize_vector(vector);
        let variance = delta_by_predicate
            .get(&spec.predicate_iri)
            .map(|delta| delta.variance);
        profiles.push(SemanticRelationProfile {
            id: spec.id.clone(),
            predicate_iri: spec.predicate_iri.clone(),
            label: spec.label.clone(),
            category: spec.category.clone(),
            verbalization: spec.verbalization.clone(),
            vector_ref: SemanticVectorRef {
                file: format!("vectors/relation-{}.f32", spec.id),
                index: 0,
                dimensions,
            },
            obs_count: spec.obs_count,
            variance,
            vector: normalized,
        });
    }
    profiles.sort_by(|left, right| left.predicate_iri.cmp(&right.predicate_iri));
    let interaction_edges = build_interaction_edges(wires);
    let relation_neighbor_edges =
        build_relation_neighbor_edges(&profiles, cfg.relation_neighbor_limit);
    let mut delta_operators = delta_by_predicate
        .values_mut()
        .map(|delta| {
            delta.vector_ref = SemanticVectorRef {
                file: format!("vectors/delta-{}.f32", delta.id),
                index: 0,
                dimensions,
            };
            delta.clone()
        })
        .collect::<Vec<_>>();
    delta_operators.sort_by(|left, right| left.predicate_iri.cmp(&right.predicate_iri));
    Ok(SemanticRelationFile {
        manifest: SemanticRelationManifest {
            schema_version: index.manifest.schema_version,
            graph_id: graph_id.to_string(),
            dimensions,
            relation_profile_count: profiles.len(),
            interaction_edge_count: interaction_edges.len(),
            relation_neighbor_edge_count: relation_neighbor_edges.len(),
            delta_operator_count: delta_operators.len(),
            relation_profiles_path,
        },
        profiles,
        interaction_edges,
        relation_neighbor_edges,
        delta_operators,
    })
}

fn relation_profile_verbalization(
    store: &Store,
    label: &str,
    category: Option<&str>,
    wires: &[&WireRelation],
    sample_label_limit: usize,
) -> String {
    let mut parts = vec![format!("relation {label}")];
    if let Some(category) = category.filter(|value| !value.is_empty()) {
        parts.push(format!("category {category}"));
    }
    let verbalize_cfg = VerbalizeCfg::default();
    let mut heads = Vec::new();
    let mut tails = Vec::new();
    for wire in wires {
        if heads.len() < sample_label_limit {
            if let Some(text) = verbalize(&wire.source_iri, store, &verbalize_cfg) {
                heads.push(text);
            }
        }
        if tails.len() < sample_label_limit {
            if let Some(text) = verbalize(&wire.target_iri, store, &verbalize_cfg) {
                tails.push(text);
            }
        }
        if heads.len() >= sample_label_limit && tails.len() >= sample_label_limit {
            break;
        }
    }
    if !heads.is_empty() {
        parts.push(format!("heads {}", heads.join(" | ")));
    }
    if !tails.is_empty() {
        parts.push(format!("tails {}", tails.join(" | ")));
    }
    parts.join("; ")
}

fn relation_label(predicate_iri: &str) -> String {
    let short = predicate_short_name(predicate_iri);
    builtin_wire_predicates()
        .into_iter()
        .find(|(name, _, _)| *name == short)
        .map(|(_, label, _)| label.to_string())
        .unwrap_or_else(|| humanize_local_name(&short))
}

fn humanize_local_name(short: &str) -> String {
    let mut out = String::new();
    let mut previous_lower = false;
    for ch in short.chars() {
        if ch == '_' || ch == '-' {
            if !out.ends_with(' ') {
                out.push(' ');
            }
            previous_lower = false;
            continue;
        }
        if ch.is_ascii_uppercase() && previous_lower {
            out.push(' ');
        }
        out.push(ch.to_ascii_lowercase());
        previous_lower = ch.is_ascii_lowercase() || ch.is_ascii_digit();
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn relation_category(short_name: &str) -> Option<&'static str> {
    builtin_wire_predicates()
        .into_iter()
        .find(|(name, _, _)| *name == short_name)
        .and_then(|(_, _, category)| (!category.is_empty()).then_some(category))
}

fn entity_vector_map(
    index: &SemanticIndexFile,
    dimensions: usize,
) -> Result<BTreeMap<String, Vec<f32>>, String> {
    let mut out = BTreeMap::new();
    for block in &index.blocks {
        if block.iri.trim().is_empty() {
            continue;
        }
        if block.vector.len() != dimensions {
            return Err(format!(
                "entity vector {} has {} dimensions; relation layer requires {}",
                block.iri,
                block.vector.len(),
                dimensions
            ));
        }
        out.insert(block.iri.clone(), block.vector.clone());
    }
    Ok(out)
}

fn build_delta_operators(
    graph_id: &str,
    dimensions: usize,
    wires: &[WireRelation],
    entity_vectors: &BTreeMap<String, Vec<f32>>,
) -> BTreeMap<String, SemanticDeltaOperator> {
    let mut deltas = BTreeMap::<String, Vec<Vec<f32>>>::new();
    for wire in wires {
        let (Some(source), Some(target)) = (
            entity_vectors.get(&wire.source_iri),
            entity_vectors.get(&wire.target_iri),
        ) else {
            continue;
        };
        if source.len() != dimensions || target.len() != dimensions {
            continue;
        }
        let delta = target
            .iter()
            .zip(source.iter())
            .map(|(target, source)| target - source)
            .collect::<Vec<_>>();
        deltas
            .entry(wire.predicate_iri.clone())
            .or_default()
            .push(delta);
    }
    deltas
        .into_iter()
        .map(|(predicate_iri, observations)| {
            let vector = mean_vector(&observations, dimensions);
            let variance = mean_squared_residual(&observations, &vector);
            let id = stable_id("semdelta", &[graph_id, predicate_iri.as_str()]);
            (
                predicate_iri.clone(),
                SemanticDeltaOperator {
                    id,
                    predicate_iri,
                    vector_ref: semantic_vector_ref(0, dimensions),
                    obs_count: observations.len(),
                    variance,
                    vector,
                },
            )
        })
        .collect()
}

fn mean_vector(observations: &[Vec<f32>], dimensions: usize) -> Vec<f32> {
    let mut mean = vec![0.0; dimensions];
    if observations.is_empty() {
        return mean;
    }
    for observation in observations {
        for (index, value) in observation.iter().enumerate().take(dimensions) {
            mean[index] += *value;
        }
    }
    let denom = observations.len() as f32;
    for value in &mut mean {
        *value /= denom;
    }
    mean
}

fn mean_squared_residual(observations: &[Vec<f32>], mean: &[f32]) -> f32 {
    if observations.is_empty() {
        return 0.0;
    }
    let sum = observations
        .iter()
        .map(|observation| {
            observation
                .iter()
                .zip(mean.iter())
                .map(|(value, mean)| {
                    let residual = value - mean;
                    residual * residual
                })
                .sum::<f32>()
        })
        .sum::<f32>();
    sum / observations.len() as f32
}

fn build_interaction_edges(wires: &[WireRelation]) -> Vec<RelationInteractionEdge> {
    let relation_counts = wires
        .iter()
        .fold(BTreeMap::<String, usize>::new(), |mut acc, wire| {
            *acc.entry(wire.predicate_iri.clone()).or_default() += 1;
            acc
        });
    let mut counts = BTreeMap::<(String, String, String), usize>::new();
    for left in wires {
        for right in wires {
            if left.wire_iri == right.wire_iri {
                continue;
            }
            for kind in interaction_kinds(left, right) {
                *counts
                    .entry((
                        left.predicate_iri.clone(),
                        right.predicate_iri.clone(),
                        kind.to_string(),
                    ))
                    .or_default() += 1;
            }
        }
    }
    let total_pairs = wires
        .len()
        .saturating_mul(wires.len().saturating_sub(1))
        .max(1) as f32;
    counts
        .into_iter()
        .map(
            |((source_relation, target_relation, interaction_kind), count)| {
                let source_count = *relation_counts.get(&source_relation).unwrap_or(&1) as f32;
                let target_count = *relation_counts.get(&target_relation).unwrap_or(&1) as f32;
                let weight = (((count as f32) * total_pairs + 1.0)
                    / (source_count * target_count + 1.0))
                    .ln();
                RelationInteractionEdge {
                    id: stable_id(
                        "semrelint",
                        &[
                            source_relation.as_str(),
                            target_relation.as_str(),
                            interaction_kind.as_str(),
                        ],
                    ),
                    source_relation,
                    target_relation,
                    interaction_kind,
                    count,
                    weight,
                }
            },
        )
        .collect()
}

fn interaction_kinds(left: &WireRelation, right: &WireRelation) -> Vec<&'static str> {
    let mut kinds = Vec::new();
    if left.source_iri == right.source_iri {
        kinds.push("HH");
    }
    if left.source_iri == right.target_iri {
        kinds.push("HT");
    }
    if left.target_iri == right.source_iri {
        kinds.push("TH");
    }
    if left.target_iri == right.target_iri {
        kinds.push("TT");
    }
    kinds
}

fn build_relation_neighbor_edges(
    profiles: &[SemanticRelationProfile],
    limit: usize,
) -> Vec<RelationSemanticNeighborEdge> {
    if limit == 0 {
        return Vec::new();
    }
    let mut top_by_source = BTreeMap::<String, Vec<(String, f32, usize)>>::new();
    for source in profiles {
        let mut scored = profiles
            .iter()
            .filter(|target| target.predicate_iri != source.predicate_iri)
            .map(|target| {
                (
                    target.predicate_iri.clone(),
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
            source.predicate_iri.clone(),
            scored
                .into_iter()
                .take(limit)
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
            edges.push(RelationSemanticNeighborEdge {
                id: stable_id("semrelnei", &[source, target]),
                source_relation: source.clone(),
                target_relation: target.clone(),
                score: *score,
                rank: *rank,
                mutual,
            });
        }
    }
    edges.sort_by(|left, right| {
        left.source_relation
            .cmp(&right.source_relation)
            .then_with(|| left.rank.cmp(&right.rank))
            .then_with(|| left.target_relation.cmp(&right.target_relation))
    });
    edges
}

fn named_node(term: Option<&OxTerm>, field: &str) -> Result<String, String> {
    match term {
        Some(OxTerm::NamedNode(node)) => Ok(node.as_str().to_string()),
        Some(other) => Err(format!(
            "wire relation field ?{field} expected IRI, got {other}"
        )),
        None => Err(format!("wire relation row missing ?{field}")),
    }
}

fn select_solutions(store: &Store, query: &str) -> Result<Vec<BTreeMap<String, OxTerm>>, String> {
    let solutions = match SparqlEvaluator::new()
        .parse_query(query)
        .map_err(|error| format!("parse semantic relation query: {error}"))?
        .on_store(store)
        .execute()
        .map_err(|error| format!("execute semantic relation query: {error}"))?
    {
        QueryResults::Solutions(solutions) => solutions,
        _ => return Err("semantic relation query expected SELECT solutions".to_string()),
    };
    let mut rows = Vec::new();
    for solution in solutions {
        let solution = solution.map_err(|error| format!("read semantic relation row: {error}"))?;
        let mut row = BTreeMap::new();
        for (name, term) in solution.iter() {
            row.insert(
                name.to_string().trim_start_matches('?').to_string(),
                term.clone(),
            );
        }
        rows.push(row);
    }
    Ok(rows)
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
    use crate::{
        semantic_index::{
            normalize_vector, semantic_content_hash, semantic_entities_from_blocks,
            SemanticBlockEmbedding, SemanticIndexManifest,
        },
        semantic_scaffold::{build_semantic_scaffold, SemanticScaffoldConfig},
    };
    use oxigraph::io::{RdfFormat, RdfParser};

    fn store_from_turtle(ttl: &str) -> Store {
        let store = Store::new().expect("store");
        store
            .load_from_slice(RdfParser::from_format(RdfFormat::Turtle), ttl.as_bytes())
            .expect("load turtle");
        store
    }

    fn wire_fixture_store() -> Store {
        store_from_turtle(&format!(
            r#"@prefix wire: <{WIRE_NS}> .
@prefix mdoc: <http://mnemosyne.dev/doc#> .

<urn:wire:1> a wire:Wire ; wire:predicate wire:supports ; wire:sourceBlock <urn:e:a> ; wire:targetBlock <urn:e:b> .
<urn:wire:2> a wire:Wire ; wire:predicate wire:requires ; wire:sourceBlock <urn:e:a> ; wire:targetBlock <urn:e:c> .
<urn:wire:3> a wire:Wire ; wire:predicate wire:supports ; wire:sourceBlock <urn:e:b> ; wire:targetBlock <urn:e:c> .

<urn:e:a> mdoc:textContent "alpha beta gamma" .
<urn:e:b> mdoc:textContent "bravo charlie delta" .
<urn:e:c> mdoc:textContent "charlie delta echo" .
"#
        ))
    }

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

    fn fixture_index_and_scaffold() -> (SemanticIndexFile, SemanticScaffoldFile) {
        let blocks = vec![
            block("urn:e:a", vec![1.0, 0.0]),
            block("urn:e:b", vec![0.0, 1.0]),
            block("urn:e:c", vec![1.0, 1.0]),
        ];
        let index = SemanticIndexFile {
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
        };
        let scaffold = build_semantic_scaffold(
            &index,
            SemanticScaffoldConfig {
                materialized_neighbor_limit: 2,
            },
            "indexes/semantic/scaffold.json".to_string(),
        )
        .expect("build scaffold");
        (index, scaffold)
    }

    fn relation_count(
        relations: &SemanticRelationFile,
        source: &str,
        target: &str,
        kind: &str,
    ) -> usize {
        relations
            .interaction_edges
            .iter()
            .find(|edge| {
                edge.source_relation.ends_with(source)
                    && edge.target_relation.ends_with(target)
                    && edge.interaction_kind == kind
            })
            .map(|edge| edge.count)
            .unwrap_or_default()
    }

    #[test]
    fn interaction_graph_counts_hh_ht_th_tt_from_wire_fixture() {
        let store = wire_fixture_store();
        let wires = collect_wire_relations(&store).expect("collect wires");
        let specs = relation_profile_specs(&store, &wires, SemanticRelationConfig::default());
        let (index, scaffold) = fixture_index_and_scaffold();
        let relations = build_semantic_relations(
            "graph-a",
            2,
            &index,
            &scaffold,
            &wires,
            &specs,
            vec![vec![1.0, 0.0], vec![0.0, 1.0]],
            "indexes/semantic/relation-profiles.json".to_string(),
            SemanticRelationConfig::default(),
        )
        .expect("build relations");

        assert_eq!(relation_count(&relations, "supports", "requires", "HH"), 1);
        assert_eq!(relation_count(&relations, "supports", "supports", "TH"), 1);
        assert_eq!(relation_count(&relations, "supports", "supports", "HT"), 1);
        assert_eq!(relation_count(&relations, "requires", "supports", "TT"), 1);
    }

    #[test]
    fn unknown_relation_labels_fallback_to_humanized_local_name() {
        let store = store_from_turtle(&format!(
            r#"@prefix wire: <{WIRE_NS}> .
<urn:wire:x> a wire:Wire ; wire:predicate wire:customRelationThing ; wire:sourceBlock <urn:e:a> ; wire:targetBlock <urn:e:b> .
"#
        ));
        let wires = collect_wire_relations(&store).expect("collect wires");
        let specs = relation_profile_specs(&store, &wires, SemanticRelationConfig::default());

        assert_eq!(specs[0].label, "custom relation thing");
        assert!(!specs[0].verbalization.contains("customRelationThing"));
    }

    #[test]
    fn relation_vectors_dimensions_and_delta_variance_are_recorded() {
        let store = wire_fixture_store();
        let wires = collect_wire_relations(&store).expect("collect wires");
        let specs = relation_profile_specs(&store, &wires, SemanticRelationConfig::default());
        let (index, scaffold) = fixture_index_and_scaffold();
        let relations = build_semantic_relations(
            "graph-a",
            2,
            &index,
            &scaffold,
            &wires,
            &specs,
            vec![vec![1.0, 0.0], vec![0.0, 1.0]],
            "indexes/semantic/relation-profiles.json".to_string(),
            SemanticRelationConfig::default(),
        )
        .expect("build relations");

        assert!(relations
            .profiles
            .iter()
            .all(|profile| profile.vector_ref.dimensions == 2));
        let supports = relations
            .delta_operators
            .iter()
            .find(|delta| delta.predicate_iri.ends_with("supports"))
            .expect("supports delta");
        assert_eq!(supports.obs_count, 2);
        assert!(supports.variance >= 0.0);
        assert_eq!(supports.vector_ref.dimensions, 2);

        let wrong = build_semantic_relations(
            "graph-a",
            2,
            &index,
            &scaffold,
            &wires,
            &specs,
            vec![vec![1.0], vec![0.0, 1.0]],
            "indexes/semantic/relation-profiles.json".to_string(),
            SemanticRelationConfig::default(),
        );
        assert!(wrong.unwrap_err().contains("omphalos requires 2"));
    }
}
