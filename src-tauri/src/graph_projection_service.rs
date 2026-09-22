use crate::app_runtime::AppHandle;
use crate::{
    document_service::list_documents,
    graph_rdf_terms::{
        rdf_count_literal, rdf_friendly_label, rdf_node_kind, rdf_term_lexical, sparql_count,
    },
    graph_service::read_graph_record,
    json_utils::json_string,
    rdf_service::{run_sparql_query, SparqlInput},
};
use std::collections::{BTreeMap, BTreeSet};

pub(super) use crate::graph_catalog_projection::{
    hosted_graph_entries_service, hosted_graph_entry_from_record,
};

pub(super) fn hosted_workspace_summary(
    app: AppHandle,
    graph_id: &str,
) -> Result<serde_json::Value, String> {
    let (_, graph) = read_graph_record(&app, graph_id)?;
    let summary = run_sparql_query(
        app.clone(),
        SparqlInput {
            graph_id: graph_id.to_string(),
            query: r#"
SELECT (COUNT(DISTINCT ?node) AS ?node_count)
WHERE {
  { ?node ?p ?o . }
  UNION
  { ?s ?p ?node . FILTER(isIRI(?node) || isBlank(?node)) }
}
"#
            .to_string(),
        },
    )?;
    let edge_count = summary.quad_count;
    let node_count = sparql_count(summary, "node_count");
    let documents = list_documents(app, graph_id.to_string())?;
    let last_updated_at = documents
        .iter()
        .map(|document| document.updated_at.clone())
        .max()
        .unwrap_or(graph.updated_at);
    Ok(serde_json::json!({
        "graph_id": graph_id,
        "node_count": node_count,
        "edge_count": edge_count,
        "document_count": documents.len(),
        "last_updated_at": last_updated_at,
    }))
}

pub(super) fn hosted_workspace_properties(
    app: AppHandle,
    graph_id: &str,
) -> Result<serde_json::Value, String> {
    let result = run_sparql_query(
        app,
        SparqlInput {
            graph_id: graph_id.to_string(),
            query: r#"
SELECT ?p (COUNT(*) AS ?count) (COUNT(DISTINCT ?s) AS ?distinct_subjects) (COUNT(DISTINCT ?o) AS ?distinct_objects)
WHERE { ?s ?p ?o . }
GROUP BY ?p
ORDER BY DESC(?count)
"#
            .to_string(),
        },
    )?;
    let properties = result
        .rows
        .iter()
        .filter_map(|row| {
            let iri = rdf_term_lexical(row.get("p")?)?;
            let label = rdf_friendly_label(&iri);
            Some(serde_json::json!({
                "iri": iri,
                "label": label,
                "count": rdf_count_literal(row.get("count")).unwrap_or(0),
                "distinct_subjects": rdf_count_literal(row.get("distinct_subjects")).unwrap_or(0),
                "distinct_objects": rdf_count_literal(row.get("distinct_objects")).unwrap_or(0),
            }))
        })
        .collect::<Vec<_>>();
    Ok(serde_json::json!({
        "graph_id": graph_id,
        "properties": properties,
    }))
}

pub(super) fn hosted_workspace_viz(
    app: AppHandle,
    graph_id: &str,
    limit_nodes: Option<usize>,
    edge_limit: usize,
) -> Result<serde_json::Value, String> {
    let result = run_sparql_query(
        app,
        SparqlInput {
            graph_id: graph_id.to_string(),
            query: format!(
                r#"
SELECT ?s ?p ?o
WHERE {{
  ?s ?p ?o .
  FILTER(isIRI(?s))
  FILTER(isIRI(?o))
}}
LIMIT {edge_limit}
"#
            ),
        },
    )?;
    let mut node_degrees: BTreeMap<String, usize> = BTreeMap::new();
    let mut predicate_iris: BTreeSet<String> = BTreeSet::new();
    let mut edges = Vec::new();

    for row in &result.rows {
        let Some(source) = row.get("s").and_then(rdf_term_lexical) else {
            continue;
        };
        let Some(target) = row.get("o").and_then(rdf_term_lexical) else {
            continue;
        };
        let Some(predicate) = row.get("p").and_then(rdf_term_lexical) else {
            continue;
        };
        predicate_iris.insert(predicate.clone());
        *node_degrees.entry(source.clone()).or_insert(0) += 1;
        *node_degrees.entry(target.clone()).or_insert(0) += 1;
        let predicate_label = rdf_friendly_label(&predicate);
        edges.push(serde_json::json!({
            "id": format!("edge:{}", edges.len()),
            "source": source,
            "target": target,
            "predicate": predicate,
            "label": predicate_label,
            "weight": null,
        }));
    }

    let max_degree = node_degrees.values().copied().max().unwrap_or(0);
    let mut nodes = node_degrees
        .iter()
        .map(|(iri, degree)| {
            let importance = if max_degree > 0 {
                Some((*degree as f64 / max_degree as f64 * 1000.0).round() / 1000.0)
            } else {
                None
            };
            serde_json::json!({
                "id": iri,
                "iri": iri,
                "types": [],
                "label": rdf_friendly_label(iri),
                "kind": rdf_node_kind(iri),
                "degree": degree,
                "importance": importance,
            })
        })
        .collect::<Vec<_>>();

    if let Some(limit_nodes) = limit_nodes.filter(|value| *value > 0) {
        nodes.truncate(limit_nodes);
        let allowed = nodes
            .iter()
            .filter_map(|node| json_string(node.get("id")))
            .collect::<BTreeSet<_>>();
        edges.retain(|edge| {
            json_string(edge.get("source"))
                .map(|source| allowed.contains(&source))
                .unwrap_or(false)
                && json_string(edge.get("target"))
                    .map(|target| allowed.contains(&target))
                    .unwrap_or(false)
        });
    }

    Ok(serde_json::json!({
        "graph_id": graph_id,
        "nodes": nodes,
        "edges": edges,
        "stats": {
            "node_count": nodes.len(),
            "edge_count": edges.len(),
            "distinct_predicates": predicate_iris.len(),
        },
    }))
}
