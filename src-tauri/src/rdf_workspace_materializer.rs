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

    for kind in WORKSPACE_ENTITY_KINDS {
        for entity in snapshot_array(snapshot, kind.snapshot_key()) {
            let mut entity_triples = kind.triples(graph_id, entity);
            let total = entity_triples.len();
            let mut first_left_out: Option<String> = None;
            entity_triples.retain(|triple| match triple.invalid_iri() {
                Some(iri) => {
                    first_left_out.get_or_insert_with(|| iri.to_string());
                    false
                }
                None => true,
            });
            if let Some(iri) = first_left_out {
                log::warn!(
                    "[workspace projection] graph {graph_id}: {} {:?}: {} of {total} triple(s) left out, {iri:?} is not an IRI",
                    kind.name(),
                    entity.get("id").and_then(serde_json::Value::as_str).unwrap_or_default(),
                    total - entity_triples.len(),
                );
            }
            triples.extend(entity_triples);
        }
    }

    triples
}

/// The workspace entities the projection makes triples of. A client may write
/// any key or value into the workspace Y.Doc, and an id with a space or a `<`
/// makes no IRI: one such triple failed the whole workspace projection, so
/// every later workspace write of the graph failed after its commit and was
/// retried forever (2026-10-14). [`workspace_entity_triples`] leaves such
/// triples out with a warning (all of an entity's, when its own id makes no
/// IRI), and an op that creates a folder, an artifact or a wire refuses one
/// before it writes ([`workspace_entity_unprojectable_iri`]).
#[derive(Clone, Copy)]
pub(crate) enum WorkspaceEntityKind {
    Folder,
    Document,
    Artifact,
    Wire,
}

const WORKSPACE_ENTITY_KINDS: [WorkspaceEntityKind; 4] = [
    WorkspaceEntityKind::Folder,
    WorkspaceEntityKind::Document,
    WorkspaceEntityKind::Artifact,
    WorkspaceEntityKind::Wire,
];

impl WorkspaceEntityKind {
    fn snapshot_key(self) -> &'static str {
        match self {
            Self::Folder => "folders",
            Self::Document => "documents",
            Self::Artifact => "artifacts",
            Self::Wire => "wires",
        }
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Folder => "folder",
            Self::Document => "document",
            Self::Artifact => "artifact",
            Self::Wire => "wire",
        }
    }

    fn triples(self, graph_id: &str, entity: &serde_json::Value) -> Vec<RdfTriple> {
        let mut triples = Vec::new();
        match self {
            Self::Folder => push_folder_snapshot_triples(&mut triples, graph_id, entity),
            Self::Document => push_document_snapshot_triples(&mut triples, graph_id, entity),
            Self::Artifact => push_artifact_snapshot_triples(&mut triples, graph_id, entity),
            Self::Wire => push_wire_snapshot_triples(&mut triples, graph_id, entity),
        }
        triples
    }
}

/// The first IRI this entity (in the snapshot's shape) would project that is
/// not one, if any.
pub(crate) fn workspace_entity_unprojectable_iri(
    graph_id: &str,
    kind: WorkspaceEntityKind,
    entity: &serde_json::Value,
) -> Option<String> {
    kind.triples(graph_id, entity)
        .iter()
        .find_map(|triple| triple.invalid_iri().map(str::to_string))
}
