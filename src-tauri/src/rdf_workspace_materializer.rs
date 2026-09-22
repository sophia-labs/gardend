use crate::{
    rdf::RdfTriple,
    rdf_workspace_entity_triples::{
        push_artifact_snapshot_triples, push_document_snapshot_triples,
        push_folder_snapshot_triples, push_wire_snapshot_triples,
    },
    rdf_workspace_terms::snapshot_array,
};

pub(super) fn workspace_snapshot_triples(
    graph_id: &str,
    snapshot: &serde_json::Value,
) -> Vec<RdfTriple> {
    let mut triples = workspace_entity_triples(graph_id, snapshot);

    // Artifact-kind ontology (classes + subClassOf + capabilities). Graph-native
    // and SPARQL-queryable. Materialized into the workspace projection graph for
    // now (no shared/system ontology graph exists yet — see plan Risks).
    triples.extend(crate::artifact_kinds::ontology_triples());

    triples
}

/// The per-snapshot INSTANCE-LEVEL triples: every folder / document / artifact
/// (incl. its nested scene subgraph) / wire. This is the portion the RECONCILE
/// path surveys + diffs as type-keyed class spans (each entity carries its
/// `rdf:type`). It DELIBERATELY EXCLUDES the artifact-kind ontology block (the
/// `rdfs:subClassOf` class-level triples whose subjects are class IRIs carrying
/// NO `rdf:type` — they match no class span), which the reconcile path routes to
/// a once-at-seed idempotent INSERT (prereq-B ruling, reconcile-class-design §2):
/// the ontology is graph-invariant, so re-emitting it every reconcile would be
/// pure churn (perpetual ADDs the wholesale path leaks identically). The wholesale
/// `workspace_snapshot_triples` keeps the ontology appended (the wholesale path
/// re-INSERTs it, idempotent set-merge); the reconcile path seeds it separately.
pub(super) fn workspace_entity_triples(
    graph_id: &str,
    snapshot: &serde_json::Value,
) -> Vec<RdfTriple> {
    let mut triples = Vec::new();

    for folder in snapshot_array(snapshot, "folders") {
        push_folder_snapshot_triples(&mut triples, graph_id, folder);
    }

    for document in snapshot_array(snapshot, "documents") {
        push_document_snapshot_triples(&mut triples, graph_id, document);
    }

    for artifact in snapshot_array(snapshot, "artifacts") {
        push_artifact_snapshot_triples(&mut triples, graph_id, artifact);
    }

    for wire in snapshot_array(snapshot, "wires") {
        push_wire_snapshot_triples(&mut triples, graph_id, wire);
    }

    triples
}
