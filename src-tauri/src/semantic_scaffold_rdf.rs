use crate::{
    emporium::{
        contract::semantic_vocabulary,
        reconcile::{reconcile_classes_validated, ClassScope, Placement, SpanKey},
        terms::{Term, Triple, TripleDiff},
    },
    rdf::{
        graph_subject, push_boolean_triple, push_float_triple, push_integer_triple,
        push_string_triple, push_uri_triple,
    },
    rdf_authority::semantic_projection_graph_iri,
    rdf_record_materializer::rdf_triple_to_term,
    runtime_config::{MNEMO_NS, RDF_TYPE},
    semantic_relation::SemanticRelationFile,
    semantic_scaffold::SemanticScaffoldFile,
};
use oxigraph::store::Store;

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn reconcile_semantic_scaffold(
    store: &Store,
    graph_id: &str,
    scaffold: &SemanticScaffoldFile,
) -> Result<TripleDiff, String> {
    let desired = semantic_scaffold_desired(graph_id, scaffold);
    reconcile_semantic_desired_for_classes(store, graph_id, &desired, semantic_scaffold_classes())
}

pub(crate) fn reconcile_semantic_projection(
    store: &Store,
    graph_id: &str,
    scaffold: &SemanticScaffoldFile,
    relations: Option<&SemanticRelationFile>,
) -> Result<TripleDiff, String> {
    let mut desired = semantic_scaffold_desired(graph_id, scaffold);
    if let Some(relations) = relations {
        desired.extend(semantic_relation_desired(graph_id, relations));
    }
    reconcile_semantic_desired_for_classes(store, graph_id, &desired, semantic_projection_classes())
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn reconcile_semantic_desired(
    store: &Store,
    graph_id: &str,
    desired: &[Triple],
) -> Result<TripleDiff, String> {
    reconcile_semantic_desired_for_classes(store, graph_id, desired, semantic_scaffold_classes())
}

fn reconcile_semantic_desired_for_classes(
    store: &Store,
    graph_id: &str,
    desired: &[Triple],
    classes: &[&str],
) -> Result<TripleDiff, String> {
    let scopes = semantic_scopes(graph_id, classes)
        .into_iter()
        .map(|scope| {
            let SpanKey::Fixed { rdf_type } = &scope.key;
            let subset = desired
                .iter()
                .filter(|(_, predicate, object)| {
                    predicate == RDF_TYPE
                        && matches!(object, Term::Uri(node) if node.as_str() == rdf_type)
                })
                .map(|(subject, _, _)| subject.clone())
                .collect::<std::collections::BTreeSet<_>>();
            let desired_subset = desired
                .iter()
                .filter(|(subject, _, _)| subset.contains(subject))
                .cloned()
                .collect::<Vec<_>>();
            (scope, desired_subset)
        })
        .collect::<Vec<_>>();
    reconcile_classes_validated(store, &scopes, Some(semantic_vocabulary()))
}

pub(crate) fn semantic_scaffold_desired(
    graph_id: &str,
    scaffold: &SemanticScaffoldFile,
) -> Vec<Triple> {
    let mut raw = Vec::new();
    for entity in &scaffold.entities {
        let subject = semantic_entity_subject(&entity.iri);
        push_uri_triple(
            &mut raw,
            &subject,
            RDF_TYPE,
            &format!("{MNEMO_NS}SemanticEntity"),
        );
        push_string_triple(&mut raw, &subject, &format!("{MNEMO_NS}graphId"), graph_id);
        push_uri_triple(
            &mut raw,
            &subject,
            &format!("{MNEMO_NS}entityIri"),
            &entity.iri,
        );
        push_string_triple(
            &mut raw,
            &subject,
            &format!("{MNEMO_NS}semanticKind"),
            &entity.kind,
        );
        push_string_triple(
            &mut raw,
            &subject,
            &format!("{MNEMO_NS}vectorFile"),
            &entity.vector_ref.file,
        );
        push_integer_triple(
            &mut raw,
            &subject,
            &format!("{MNEMO_NS}vectorIndex"),
            entity.vector_ref.index as i64,
        );
        push_integer_triple(
            &mut raw,
            &subject,
            &format!("{MNEMO_NS}dimensions"),
            entity.vector_ref.dimensions as i64,
        );
        push_string_triple(
            &mut raw,
            &subject,
            &format!("{MNEMO_NS}contentHash"),
            &entity.content_hash,
        );
    }
    for edge in &scaffold.neighbor_edges {
        let subject = semantic_edge_subject(graph_id, &edge.id);
        push_uri_triple(
            &mut raw,
            &subject,
            RDF_TYPE,
            &format!("{MNEMO_NS}SemanticNeighborEdge"),
        );
        push_string_triple(&mut raw, &subject, &format!("{MNEMO_NS}graphId"), graph_id);
        push_uri_triple(
            &mut raw,
            &subject,
            &format!("{MNEMO_NS}sourceEntity"),
            &edge.source_iri,
        );
        push_uri_triple(
            &mut raw,
            &subject,
            &format!("{MNEMO_NS}targetEntity"),
            &edge.target_iri,
        );
        push_float_triple(
            &mut raw,
            &subject,
            &format!("{MNEMO_NS}score"),
            edge.score as f64,
        );
        push_integer_triple(
            &mut raw,
            &subject,
            &format!("{MNEMO_NS}rank"),
            edge.rank as i64,
        );
        push_boolean_triple(
            &mut raw,
            &subject,
            &format!("{MNEMO_NS}mutual"),
            edge.mutual,
        );
    }
    for cluster in &scaffold.clusters {
        let subject = semantic_cluster_subject(graph_id, &cluster.id);
        push_uri_triple(
            &mut raw,
            &subject,
            RDF_TYPE,
            &format!("{MNEMO_NS}SemanticCluster"),
        );
        push_string_triple(&mut raw, &subject, &format!("{MNEMO_NS}graphId"), graph_id);
        push_string_triple(
            &mut raw,
            &subject,
            &format!("{MNEMO_NS}clusterId"),
            &cluster.id,
        );
        push_integer_triple(
            &mut raw,
            &subject,
            &format!("{MNEMO_NS}memberCount"),
            cluster.member_count as i64,
        );
    }
    for membership in &scaffold.memberships {
        let subject = semantic_membership_subject(graph_id, &membership.id);
        push_uri_triple(
            &mut raw,
            &subject,
            RDF_TYPE,
            &format!("{MNEMO_NS}SemanticClusterMembership"),
        );
        push_string_triple(&mut raw, &subject, &format!("{MNEMO_NS}graphId"), graph_id);
        push_uri_triple(
            &mut raw,
            &subject,
            &format!("{MNEMO_NS}cluster"),
            &semantic_cluster_subject(graph_id, &membership.cluster_id),
        );
        push_uri_triple(
            &mut raw,
            &subject,
            &format!("{MNEMO_NS}memberEntity"),
            &membership.iri,
        );
        push_integer_triple(
            &mut raw,
            &subject,
            &format!("{MNEMO_NS}rank"),
            membership.rank as i64,
        );
    }
    raw.iter().map(rdf_triple_to_term).collect()
}

pub(crate) fn semantic_relation_desired(
    graph_id: &str,
    relations: &SemanticRelationFile,
) -> Vec<Triple> {
    let mut raw = Vec::new();
    let profile_subjects = relations
        .profiles
        .iter()
        .map(|profile| {
            (
                profile.predicate_iri.as_str(),
                semantic_relation_subject(graph_id, &profile.id),
            )
        })
        .collect::<std::collections::BTreeMap<_, _>>();
    for profile in &relations.profiles {
        let subject = semantic_relation_subject(graph_id, &profile.id);
        push_uri_triple(
            &mut raw,
            &subject,
            RDF_TYPE,
            &format!("{MNEMO_NS}SemanticRelationProfile"),
        );
        push_string_triple(&mut raw, &subject, &format!("{MNEMO_NS}graphId"), graph_id);
        push_uri_triple(
            &mut raw,
            &subject,
            &format!("{MNEMO_NS}predicate"),
            &profile.predicate_iri,
        );
        push_string_triple(
            &mut raw,
            &subject,
            &format!("{MNEMO_NS}label"),
            &profile.label,
        );
        if let Some(category) = profile.category.as_deref() {
            push_string_triple(&mut raw, &subject, &format!("{MNEMO_NS}category"), category);
        }
        push_integer_triple(
            &mut raw,
            &subject,
            &format!("{MNEMO_NS}obsCount"),
            profile.obs_count as i64,
        );
        if let Some(variance) = profile.variance {
            push_float_triple(
                &mut raw,
                &subject,
                &format!("{MNEMO_NS}variance"),
                variance as f64,
            );
        }
    }
    for edge in &relations.interaction_edges {
        let subject = semantic_relation_interaction_subject(graph_id, &edge.id);
        push_uri_triple(
            &mut raw,
            &subject,
            RDF_TYPE,
            &format!("{MNEMO_NS}RelationInteractionEdge"),
        );
        push_string_triple(&mut raw, &subject, &format!("{MNEMO_NS}graphId"), graph_id);
        push_uri_triple(
            &mut raw,
            &subject,
            &format!("{MNEMO_NS}sourceRelation"),
            profile_subjects
                .get(edge.source_relation.as_str())
                .map(String::as_str)
                .unwrap_or(edge.source_relation.as_str()),
        );
        push_uri_triple(
            &mut raw,
            &subject,
            &format!("{MNEMO_NS}targetRelation"),
            profile_subjects
                .get(edge.target_relation.as_str())
                .map(String::as_str)
                .unwrap_or(edge.target_relation.as_str()),
        );
        push_string_triple(
            &mut raw,
            &subject,
            &format!("{MNEMO_NS}interactionKind"),
            &edge.interaction_kind,
        );
        push_integer_triple(
            &mut raw,
            &subject,
            &format!("{MNEMO_NS}count"),
            edge.count as i64,
        );
        push_float_triple(
            &mut raw,
            &subject,
            &format!("{MNEMO_NS}weight"),
            edge.weight as f64,
        );
    }
    for edge in &relations.relation_neighbor_edges {
        let subject = semantic_relation_neighbor_subject(graph_id, &edge.id);
        push_uri_triple(
            &mut raw,
            &subject,
            RDF_TYPE,
            &format!("{MNEMO_NS}RelationSemanticNeighborEdge"),
        );
        push_string_triple(&mut raw, &subject, &format!("{MNEMO_NS}graphId"), graph_id);
        push_uri_triple(
            &mut raw,
            &subject,
            &format!("{MNEMO_NS}sourceRelation"),
            profile_subjects
                .get(edge.source_relation.as_str())
                .map(String::as_str)
                .unwrap_or(edge.source_relation.as_str()),
        );
        push_uri_triple(
            &mut raw,
            &subject,
            &format!("{MNEMO_NS}targetRelation"),
            profile_subjects
                .get(edge.target_relation.as_str())
                .map(String::as_str)
                .unwrap_or(edge.target_relation.as_str()),
        );
        push_float_triple(
            &mut raw,
            &subject,
            &format!("{MNEMO_NS}score"),
            edge.score as f64,
        );
        push_integer_triple(
            &mut raw,
            &subject,
            &format!("{MNEMO_NS}rank"),
            edge.rank as i64,
        );
        push_boolean_triple(
            &mut raw,
            &subject,
            &format!("{MNEMO_NS}mutual"),
            edge.mutual,
        );
    }
    raw.iter().map(rdf_triple_to_term).collect()
}

#[cfg_attr(not(test), allow(dead_code))]
fn semantic_scaffold_classes() -> &'static [&'static str] {
    &[
        "SemanticEntity",
        "SemanticNeighborEdge",
        "SemanticCluster",
        "SemanticClusterMembership",
    ]
}

fn semantic_projection_classes() -> &'static [&'static str] {
    &[
        "SemanticEntity",
        "SemanticNeighborEdge",
        "SemanticCluster",
        "SemanticClusterMembership",
        "SemanticRelationProfile",
        "RelationInteractionEdge",
        "RelationSemanticNeighborEdge",
    ]
}

fn semantic_scopes(graph_id: &str, classes: &[&str]) -> Vec<ClassScope> {
    classes
        .into_iter()
        .map(|class| ClassScope {
            placement: Placement::Named(semantic_projection_graph_iri(graph_id)),
            key: SpanKey::Fixed {
                rdf_type: format!("{MNEMO_NS}{class}"),
            },
            graph_id_conjunct: None,
            subjects: None,
        })
        .collect()
}

fn semantic_entity_subject(entity_iri: &str) -> String {
    format!("{entity_iri}:semantic")
}

fn semantic_edge_subject(graph_id: &str, edge_id: &str) -> String {
    format!("{}:semantic:edge:{edge_id}", graph_subject(graph_id))
}

fn semantic_cluster_subject(graph_id: &str, cluster_id: &str) -> String {
    format!("{}:semantic:cluster:{cluster_id}", graph_subject(graph_id))
}

fn semantic_membership_subject(graph_id: &str, membership_id: &str) -> String {
    format!(
        "{}:semantic:membership:{membership_id}",
        graph_subject(graph_id)
    )
}

fn semantic_relation_subject(graph_id: &str, relation_id: &str) -> String {
    format!(
        "{}:semantic:relation:{relation_id}",
        graph_subject(graph_id)
    )
}

fn semantic_relation_interaction_subject(graph_id: &str, edge_id: &str) -> String {
    format!(
        "{}:semantic:relation-interaction:{edge_id}",
        graph_subject(graph_id)
    )
}

fn semantic_relation_neighbor_subject(graph_id: &str, edge_id: &str) -> String {
    format!(
        "{}:semantic:relation-neighbor:{edge_id}",
        graph_subject(graph_id)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        semantic_index::{
            normalize_vector, semantic_content_hash, semantic_entities_from_blocks,
            semantic_vector_ref, SemanticBlockEmbedding, SemanticIndexFile, SemanticIndexManifest,
        },
        semantic_relation::{
            RelationInteractionEdge, RelationSemanticNeighborEdge, SemanticDeltaOperator,
            SemanticRelationFile, SemanticRelationManifest, SemanticRelationProfile,
        },
        semantic_scaffold::{build_semantic_scaffold, SemanticScaffoldConfig},
    };
    use oxigraph::sparql::{QueryResults, SparqlEvaluator};

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

    fn fixture_scaffold() -> SemanticScaffoldFile {
        let blocks = vec![
            block("urn:mnemosyne:local:document:doc#block-a", vec![1.0, 0.0]),
            block("urn:mnemosyne:local:document:doc#block-b", vec![0.95, 0.05]),
            block("urn:mnemosyne:local:document:doc#block-c", vec![0.0, 1.0]),
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
        build_semantic_scaffold(
            &index,
            SemanticScaffoldConfig {
                materialized_neighbor_limit: 1,
            },
            "indexes/semantic/scaffold.json".to_string(),
        )
        .expect("build scaffold")
    }

    fn count_class(store: &Store, graph_id: &str, class: &str) -> usize {
        let graph = semantic_projection_graph_iri(graph_id);
        let query = format!(
            "SELECT ?s WHERE {{ GRAPH <{graph}> {{ ?s <{RDF_TYPE}> <{MNEMO_NS}{class}> }} }}"
        );
        let solutions = match SparqlEvaluator::new()
            .parse_query(&query)
            .unwrap()
            .on_store(store)
            .execute()
            .unwrap()
        {
            QueryResults::Solutions(solutions) => solutions,
            _ => panic!("expected solutions"),
        };
        solutions.count()
    }

    fn fixture_relations() -> SemanticRelationFile {
        let supports = "http://mnemosyne.ai/vocab#supports".to_string();
        let requires = "http://mnemosyne.ai/vocab#requires".to_string();
        SemanticRelationFile {
            manifest: SemanticRelationManifest {
                schema_version: 1,
                graph_id: "graph-a".to_string(),
                dimensions: 2,
                relation_profile_count: 2,
                interaction_edge_count: 1,
                relation_neighbor_edge_count: 1,
                delta_operator_count: 0,
                relation_profiles_path: "indexes/semantic/relation-profiles.json".to_string(),
            },
            profiles: vec![
                SemanticRelationProfile {
                    id: "semrel-supports".to_string(),
                    predicate_iri: supports.clone(),
                    label: "supports".to_string(),
                    category: Some("argumentation".to_string()),
                    verbalization: "relation supports".to_string(),
                    vector_ref: semantic_vector_ref(0, 2),
                    obs_count: 2,
                    variance: Some(0.125),
                    vector: Vec::new(),
                },
                SemanticRelationProfile {
                    id: "semrel-requires".to_string(),
                    predicate_iri: requires.clone(),
                    label: "requires".to_string(),
                    category: Some("dependency".to_string()),
                    verbalization: "relation requires".to_string(),
                    vector_ref: semantic_vector_ref(1, 2),
                    obs_count: 1,
                    variance: Some(0.0),
                    vector: Vec::new(),
                },
            ],
            interaction_edges: vec![RelationInteractionEdge {
                id: "rel-int-1".to_string(),
                source_relation: supports.clone(),
                target_relation: requires.clone(),
                interaction_kind: "HH".to_string(),
                count: 1,
                weight: 1.25,
            }],
            relation_neighbor_edges: vec![RelationSemanticNeighborEdge {
                id: "rel-neighbor-1".to_string(),
                source_relation: supports,
                target_relation: requires,
                score: 0.75,
                rank: 1,
                mutual: true,
            }],
            delta_operators: Vec::<SemanticDeltaOperator>::new(),
        }
    }

    fn count_relation_profile_links(store: &Store, graph_id: &str) -> usize {
        let graph = semantic_projection_graph_iri(graph_id);
        let query = format!(
            "SELECT ?edge WHERE {{
  GRAPH <{graph}> {{
    ?edge <{RDF_TYPE}> <{MNEMO_NS}RelationInteractionEdge> ;
          <{MNEMO_NS}sourceRelation> ?relation .
    ?relation <{RDF_TYPE}> <{MNEMO_NS}SemanticRelationProfile> .
  }}
}}"
        );
        let solutions = match SparqlEvaluator::new()
            .parse_query(&query)
            .unwrap()
            .on_store(store)
            .execute()
            .unwrap()
        {
            QueryResults::Solutions(solutions) => solutions,
            _ => panic!("expected solutions"),
        };
        solutions.count()
    }

    #[test]
    fn semantic_scaffold_reconciles_counts_and_is_idempotent() {
        let store = Store::new().expect("store");
        let scaffold = fixture_scaffold();

        let first =
            reconcile_semantic_scaffold(&store, "graph-a", &scaffold).expect("first reconcile");
        let second =
            reconcile_semantic_scaffold(&store, "graph-a", &scaffold).expect("second reconcile");

        assert!(first.op_count() > 0);
        assert_eq!(second.op_count(), 0);
        assert_eq!(
            count_class(&store, "graph-a", "SemanticNeighborEdge"),
            scaffold.neighbor_edges.len()
        );
        assert_eq!(
            count_class(&store, "graph-a", "SemanticCluster"),
            scaffold.clusters.len()
        );
    }

    #[test]
    fn semantic_projection_reconciles_relation_counts_and_is_idempotent() {
        let store = Store::new().expect("store");
        let scaffold = fixture_scaffold();
        let relations = fixture_relations();

        let first = reconcile_semantic_projection(&store, "graph-a", &scaffold, Some(&relations))
            .expect("first projection reconcile");
        let second = reconcile_semantic_projection(&store, "graph-a", &scaffold, Some(&relations))
            .expect("second projection reconcile");

        assert!(first.op_count() > 0);
        assert_eq!(second.op_count(), 0);
        assert_eq!(
            count_class(&store, "graph-a", "SemanticRelationProfile"),
            relations.profiles.len()
        );
        assert_eq!(
            count_class(&store, "graph-a", "RelationInteractionEdge"),
            relations.interaction_edges.len()
        );
        assert_eq!(
            count_class(&store, "graph-a", "RelationSemanticNeighborEdge"),
            relations.relation_neighbor_edges.len()
        );
        assert_eq!(
            count_class(&store, "graph-a", "SemanticNeighborEdge"),
            scaffold.neighbor_edges.len()
        );
        assert_eq!(count_relation_profile_links(&store, "graph-a"), 1);
    }

    #[test]
    fn semantic_scaffold_shacl_halt_leaves_store_unchanged() {
        let store = Store::new().expect("store");
        let scaffold = fixture_scaffold();
        reconcile_semantic_scaffold(&store, "graph-a", &scaffold).expect("seed scaffold");
        let before = count_class(&store, "graph-a", "SemanticEntity");
        let malformed = semantic_scaffold_desired("graph-a", &scaffold)
            .into_iter()
            .filter(|(_, predicate, _)| predicate != &format!("{MNEMO_NS}dimensions"))
            .collect::<Vec<_>>();

        let error = reconcile_semantic_desired(&store, "graph-a", &malformed).unwrap_err();

        assert!(error.contains("SHACL"), "{error}");
        assert_eq!(count_class(&store, "graph-a", "SemanticEntity"), before);
    }
}
