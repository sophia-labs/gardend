use crate::{
    graph_service::GraphRecord,
    profile_service::ProfileManifest,
    rdf::{graph_subject, sparql_string_literal},
    runtime_config::{DCTERMS_NS, MNEMO_NS},
};
use oxigraph::{sparql::SparqlEvaluator, store::Store};

pub(crate) fn profile_metadata_graph_iri(profile_id: &str) -> String {
    format!("urn:mnemosyne:local:profile:{profile_id}:meta")
}

pub(crate) fn profile_graph_catalog_subject(profile_id: &str, graph_id: &str) -> String {
    format!("urn:mnemosyne:local:profile:{profile_id}:graph:{graph_id}")
}

pub(crate) fn materialize_profile_metadata_graph(
    store: &Store,
    profile: &ProfileManifest,
    graphs: &[GraphRecord],
) -> Result<(), String> {
    let metadata_graph = profile_metadata_graph_iri(&profile.profile_id);
    let mut entries = Vec::with_capacity(graphs.len() + 1);
    entries.push(profile_metadata_entry(profile, &metadata_graph));
    entries.extend(
        graphs
            .iter()
            .map(|graph| graph_catalog_entry(profile, graph)),
    );

    let insert = entries.join("\n  ");
    let update = format!(
        r#"
PREFIX mnemo: <{MNEMO_NS}>
PREFIX dcterms: <{DCTERMS_NS}>

DELETE {{
  GRAPH <{metadata_graph}> {{
    ?s ?p ?o .
  }}
}}
WHERE {{
  GRAPH <{metadata_graph}> {{
    ?s ?p ?o .
  }}
}};
INSERT DATA {{
  GRAPH <{metadata_graph}> {{
  {insert}
  }}
}}
"#
    );

    SparqlEvaluator::new()
        .parse_update(&update)
        .map_err(|error| format!("parse profile metadata graph update: {error}"))?
        .on_store(store)
        .execute()
        .map_err(|error| format!("materialize profile metadata graph: {error}"))
}

pub(crate) fn materialize_profile_graph_catalog_entry(
    store: &Store,
    profile: &ProfileManifest,
    graph: &GraphRecord,
) -> Result<(), String> {
    let metadata_graph = profile_metadata_graph_iri(&profile.profile_id);
    let graph_subject = profile_graph_catalog_subject(&profile.profile_id, &graph.graph_id);
    let profile_entry = profile_metadata_entry(profile, &metadata_graph);
    let graph_entry = graph_catalog_entry(profile, graph);
    let update = format!(
        r#"
PREFIX mnemo: <{MNEMO_NS}>
PREFIX dcterms: <{DCTERMS_NS}>

DELETE WHERE {{
  GRAPH <{metadata_graph}> {{
    <{metadata_graph}> ?profile_p ?profile_o .
  }}
}};
DELETE WHERE {{
  GRAPH <{metadata_graph}> {{
    <{graph_subject}> ?graph_p ?graph_o .
  }}
}};
INSERT DATA {{
  GRAPH <{metadata_graph}> {{
  {profile_entry}
  {graph_entry}
  }}
}}
"#
    );

    SparqlEvaluator::new()
        .parse_update(&update)
        .map_err(|error| format!("parse profile metadata graph entry update: {error}"))?
        .on_store(store)
        .execute()
        .map_err(|error| format!("materialize profile metadata graph entry: {error}"))
}

pub(crate) fn delete_profile_graph_catalog_entry(
    store: &Store,
    profile: &ProfileManifest,
    graph_id: &str,
) -> Result<(), String> {
    let metadata_graph = profile_metadata_graph_iri(&profile.profile_id);
    let graph_subject = profile_graph_catalog_subject(&profile.profile_id, graph_id);
    let profile_entry = profile_metadata_entry(profile, &metadata_graph);
    let update = format!(
        r#"
PREFIX mnemo: <{MNEMO_NS}>
PREFIX dcterms: <{DCTERMS_NS}>

DELETE WHERE {{
  GRAPH <{metadata_graph}> {{
    <{metadata_graph}> ?profile_p ?profile_o .
  }}
}};
DELETE WHERE {{
  GRAPH <{metadata_graph}> {{
    <{graph_subject}> ?graph_p ?graph_o .
  }}
}};
INSERT DATA {{
  GRAPH <{metadata_graph}> {{
  {profile_entry}
  }}
}}
"#
    );

    SparqlEvaluator::new()
        .parse_update(&update)
        .map_err(|error| format!("parse profile graph catalog delete: {error}"))?
        .on_store(store)
        .execute()
        .map_err(|error| format!("delete profile graph catalog entry: {error}"))
}

fn profile_metadata_entry(profile: &ProfileManifest, metadata_graph: &str) -> String {
    format!(
        r#"<{metadata_graph}> a mnemo:MetadataGraph ;
    mnemo:profileId {profile_id} ;
    mnemo:runtimeProfile {runtime_profile} ;
    dcterms:title {title} ;
    dcterms:created {created_at} ;
    dcterms:modified {updated_at} ."#,
        profile_id = sparql_string_literal(&profile.profile_id),
        runtime_profile = sparql_string_literal(&profile.runtime_profile),
        title = sparql_string_literal(&profile.display_name),
        created_at = sparql_string_literal(&profile.created_at),
        updated_at = sparql_string_literal(&profile.updated_at),
    )
}

fn graph_catalog_entry(profile: &ProfileManifest, graph: &GraphRecord) -> String {
    let subject = profile_graph_catalog_subject(&profile.profile_id, &graph.graph_id);
    let mut predicates = vec![
        "a mnemo:Graph".to_string(),
        format!("mnemo:graphId {}", sparql_string_literal(&graph.graph_id)),
        format!("dcterms:title {}", sparql_string_literal(&graph.title)),
        format!(
            "dcterms:created {}",
            sparql_string_literal(&graph.created_at)
        ),
        format!(
            "dcterms:modified {}",
            sparql_string_literal(&graph.updated_at)
        ),
        format!("mnemo:status {}", sparql_string_literal(&graph.status)),
        format!("mnemo:origin {}", sparql_string_literal(&graph.origin)),
        format!(
            "mnemo:providerId {}",
            sparql_string_literal(&graph.provider_id)
        ),
        format!(
            "mnemo:localPath {}",
            sparql_string_literal(&graph.local_path)
        ),
        format!("mnemo:contentGraph <{}>", graph_subject(&graph.graph_id)),
    ];
    if let Some(description) = &graph.description {
        predicates.push(format!(
            "dcterms:description {}",
            sparql_string_literal(description)
        ));
    }
    format!("<{subject}> {} .", predicates.join(" ;\n    "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        rdf_query_service::execute_sparql_query,
        runtime_config::{GRAPH_STATUS_ACTIVE, GRAPH_STATUS_DELETED},
    };

    fn profile() -> ProfileManifest {
        ProfileManifest {
            profile_id: "default".to_string(),
            display_name: "Default Local Profile".to_string(),
            runtime_profile: "local_only".to_string(),
            created_at: "1".to_string(),
            updated_at: "2".to_string(),
        }
    }

    fn graph_record(graph_id: &str, status: &str, description: Option<&str>) -> GraphRecord {
        GraphRecord {
            graph_id: graph_id.to_string(),
            title: format!("Graph {graph_id}"),
            description: description.map(str::to_string),
            status: status.to_string(),
            origin: "local".to_string(),
            provider_id: "local-profile".to_string(),
            local_path: format!("/tmp/{graph_id}"),
            created_at: "10".to_string(),
            incarnation_id: None,
            updated_at: "20".to_string(),
            capabilities: Vec::new(),
            created_by_operation_id: None,
            validation_policy: crate::runtime_config::ValidationPolicy::default(),
            content_revision: None,
        }
    }

    #[test]
    fn metadata_graph_catalog_is_profile_scoped_and_keeps_deleted_records() {
        let store = Store::new().expect("store");
        let profile = profile();
        let metadata_graph = profile_metadata_graph_iri(&profile.profile_id);

        materialize_profile_metadata_graph(
            &store,
            &profile,
            &[
                graph_record("graph-a", GRAPH_STATUS_ACTIVE, Some("A")),
                graph_record("graph-b", GRAPH_STATUS_DELETED, None),
            ],
        )
        .expect("materialize metadata graph");

        let result = execute_sparql_query(
            &store,
            &format!(
                r#"
PREFIX mnemo: <{MNEMO_NS}>
SELECT ?graph ?graphId ?status ?contentGraph WHERE {{
  GRAPH <{metadata_graph}> {{
    ?graph a mnemo:Graph ;
      mnemo:graphId ?graphId ;
      mnemo:status ?status ;
      mnemo:contentGraph ?contentGraph .
  }}
}}
ORDER BY ?graphId
"#
            ),
        )
        .expect("query metadata graph");

        assert_eq!(result.rows.len(), 2);
        assert_eq!(result.rows[0]["graphId"], "\"graph-a\"");
        assert_eq!(result.rows[0]["status"], "\"active\"");
        assert_eq!(
            result.rows[0]["contentGraph"],
            "<urn:mnemosyne:local:graph:graph-a>"
        );
        assert_eq!(result.rows[1]["graphId"], "\"graph-b\"");
        assert_eq!(result.rows[1]["status"], "\"deleted\"");
    }

    #[test]
    fn metadata_graph_materialization_replaces_stale_catalog_values() {
        let store = Store::new().expect("store");
        let profile = profile();
        let metadata_graph = profile_metadata_graph_iri(&profile.profile_id);

        materialize_profile_metadata_graph(
            &store,
            &profile,
            &[graph_record("graph-a", GRAPH_STATUS_ACTIVE, Some("Old"))],
        )
        .expect("initial materialize");
        materialize_profile_metadata_graph(
            &store,
            &profile,
            &[graph_record("graph-a", GRAPH_STATUS_ACTIVE, None)],
        )
        .expect("replacement materialize");

        let result = execute_sparql_query(
            &store,
            &format!(
                r#"
PREFIX dcterms: <{DCTERMS_NS}>
SELECT ?description WHERE {{
  GRAPH <{metadata_graph}> {{
    ?graph dcterms:description ?description .
  }}
}}
"#
            ),
        )
        .expect("query descriptions");

        assert!(result.rows.is_empty());
    }

    #[test]
    fn metadata_graph_entry_materialization_updates_one_graph() {
        let store = Store::new().expect("store");
        let profile = profile();
        let metadata_graph = profile_metadata_graph_iri(&profile.profile_id);

        materialize_profile_metadata_graph(
            &store,
            &profile,
            &[
                graph_record("graph-a", GRAPH_STATUS_ACTIVE, Some("Old")),
                graph_record("graph-b", GRAPH_STATUS_ACTIVE, None),
            ],
        )
        .expect("initial materialize");
        let mut updated = graph_record("graph-a", GRAPH_STATUS_DELETED, None);
        updated.title = "Graph A Updated".to_string();
        materialize_profile_graph_catalog_entry(&store, &profile, &updated)
            .expect("incremental materialize");

        let result = execute_sparql_query(
            &store,
            &format!(
                r#"
PREFIX dcterms: <{DCTERMS_NS}>
PREFIX mnemo: <{MNEMO_NS}>
SELECT ?graphId ?title ?status WHERE {{
  GRAPH <{metadata_graph}> {{
    ?graph a mnemo:Graph ;
      mnemo:graphId ?graphId ;
      dcterms:title ?title ;
      mnemo:status ?status .
  }}
}}
ORDER BY ?graphId
"#
            ),
        )
        .expect("query graph entries");

        assert_eq!(result.rows.len(), 2);
        assert_eq!(result.rows[0]["graphId"], "\"graph-a\"");
        assert_eq!(result.rows[0]["title"], "\"Graph A Updated\"");
        assert_eq!(result.rows[0]["status"], "\"deleted\"");
        assert_eq!(result.rows[1]["graphId"], "\"graph-b\"");
    }
}
